"""Opt-in real Claude Code + Docker + host MCP, with a scripted Messages peer.

Requires the integrated MCP broker and built stdio bridge. This is transport and
tool execution evidence, not real model inference or third-party SaaS validation.
"""

import base64
import hashlib
import json
import os
from pathlib import Path
import queue
import shlex
import subprocess
import sys
import tempfile
import threading
import unittest


class ClaudeMcpModel:
    def __init__(self):
        self.calls = 0
        self.failure = None

    def __call__(self, body):
        try:
            return self.respond(body)
        except Exception as error:
            self.failure = str(error)
            raise

    def respond(self, body):
        self.calls += 1
        assert body["model"] == "circus-fixture" and body["stream"] is True
        names = {tool.get("name") for tool in body["tools"]}
        outputs = [block for message in body["messages"] if message["role"] == "user"
                   for block in message["content"] if isinstance(block, dict) and block.get("type") == "tool_result"]
        if self.calls == 1:
            candidates = [name for name in names if name.endswith("c0__read_issue")]
            assert len(candidates) == 1, f"approved MCP tool missing: {sorted(names)}"
            name, arguments = candidates[0], {"number": 42}
        elif self.calls == 2:
            assert any("Issue 42: fix addition" in str(block.get("content")) for block in outputs), "MCP result absent"
            assert "host-mcp-password" not in json.dumps(body), "MCP credential reached model"
            code = """import hashlib, os
from pathlib import Path
assert 'CIRCUS_MCP_CONFIG' not in os.environ
assert 'CIRCUS_MODEL_API_KEY' not in os.environ
for value in os.environ.values():
    assert hashlib.sha256(value.encode()).hexdigest() not in CREDENTIAL_DIGESTS
Path('result.txt').write_text('MCP issue 42 read through host policy; secret absent\\n')
print('MCP credential isolation passed')
""".replace("CREDENTIAL_DIGESTS", repr([
                hashlib.sha256(value).hexdigest() for value in [b"host-mcp-password", b"host-model-test-secret"]]))
            name, arguments = "Bash", {"command": "python3 -c " + shlex.quote(code), "timeout": 10000,
                                       "description": "Verify credentials remain outside guest"}
        elif self.calls == 3:
            assert any("MCP credential isolation passed" in str(block.get("content")) and not block.get("is_error")
                       for block in outputs), "guest checks failed"
        else:
            raise AssertionError("unexpected Messages request")
        if self.calls < 3:
            block = {"type": "tool_use", "id": f"toolu_mcp_{self.calls}", "name": name, "input": {}}
            delta = {"type": "input_json_delta", "partial_json": json.dumps(arguments)}
            stop = "tool_use"
        else:
            block, delta, stop = {"type": "text", "text": ""}, {"type": "text_delta", "text": "MCP task completed."}, "end_turn"
        events = [
            {"type": "message_start", "message": {"id": f"msg_mcp_{self.calls}", "type": "message", "role": "assistant",
                "model": "circus-fixture", "content": [], "stop_reason": None, "stop_sequence": None,
                "usage": {"input_tokens": 10, "output_tokens": 1}}},
            {"type": "content_block_start", "index": 0, "content_block": block},
            {"type": "content_block_delta", "index": 0, "delta": delta},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None}, "usage": {"output_tokens": 20}},
            {"type": "message_stop"},
        ]
        return b"".join((f"event: {event['type']}\ndata: " + json.dumps(event) + "\n\n").encode() for event in events)


@unittest.skipUnless(os.environ.get("CIRCUS_CLAUDE_MCP_DOCKER") == "1", "requires integrated broker, real Docker and Claude image")
class ClaudeMcpDockerTests(unittest.TestCase):
    def test_claude_calls_shared_host_mcp_without_upstream_credentials(self):
        from test_mcp import McpService
        service = McpService()
        self.addCleanup(service.close)
        service.model = ClaudeMcpModel()
        with tempfile.TemporaryDirectory(prefix="circus-claude-mcp-") as directory:
            root = Path(directory)
            key, headers = root / "model-key", root / "mcp-headers.json"
            key.write_text("host-model-test-secret")
            headers.write_text(json.dumps({"Authorization": "Bearer host-mcp-password"}))
            key.chmod(0o600)
            headers.chmod(0o600)
            connection = service.config()
            del connection["headers"]
            connection["headersFile"] = str(headers)
            config = {"version": 1, "agent": "claude-code", "dockerEndpoint": os.environ["CIRCUS_DOCKER_ENDPOINT"],
                      "agentImage": os.environ["CIRCUS_CLAUDE_IMAGE"], "python": sys.executable,
                      "resources": {"memoryBytes": 536870912, "cpuMillis": 1000, "maxProcesses": 128,
                                    "workspaceBytes": 134217728, "maxFileBytes": 2097152},
                      "model": {"baseUrl": service.base + "/v1", "model": "circus-fixture", "apiKeyFile": str(key),
                                "caFile": service.cert}, "mcpConnections": [connection]}
            config_path = root / "config.json"
            config_path.write_text(json.dumps(config))
            process = subprocess.Popen([os.environ["CIRCUS_BRIDGE_BINARY"], "--config", str(config_path)],
                                       stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)

            def settle():
                if process.poll() is None:
                    if not process.stdin.closed:
                        process.stdin.close()
                    try:
                        process.wait(timeout=45)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
                for pipe in (process.stdin, process.stdout, process.stderr):
                    if not pipe.closed:
                        pipe.close()

            self.addCleanup(settle)
            frames = queue.Queue()

            def receive():
                for line in process.stdout:
                    frames.put(json.loads(line))
                frames.put(None)

            threading.Thread(target=receive, daemon=True).start()
            self.assertEqual(frames.get(timeout=20)["type"], "ready")
            request = {"v": 1, "type": "start", "requestId": "claude-mcp", "request": {
                "prompt": "Read approved issue 42 through MCP, verify credential isolation, and write result.txt.",
                "inputFiles": [], "outputPaths": ["result.txt"], "timeoutMs": 180000,
                "mcpGrants": [{"connectionId": "issues", "tools": ["read_issue"]}]}}
            process.stdin.write(json.dumps(request).encode() + b"\n")
            process.stdin.flush()
            collected, instance = [], None
            while True:
                frame = frames.get(timeout=210)
                self.assertIsNotNone(frame, "bridge exited unexpectedly")
                collected.append(frame)
                if frame.get("event", {}).get("kind") == "sandbox_created":
                    instance = frame["event"]["instanceId"]
                if frame["type"] == "result":
                    result = frame["result"]
                    break
            process.stdin.close()
            self.assertEqual(process.wait(timeout=15), 0, process.stderr.read().decode())
            self.assertIsNone(service.model.failure, {"failure": service.model.failure, "events": collected})
            self.assertEqual(result["status"], "succeeded", {"events": collected, "mcp": [rpc["method"] for _, _, rpc in service.requests], "models": service.model.calls})
            self.assertEqual(result["cleanup"], "confirmed")
            artifacts = {item["path"]: base64.b64decode(item["data"]) for item in result["artifacts"]}
            self.assertEqual(artifacts["result.txt"], b"MCP issue 42 read through host policy; secret absent\n")
            self.assertTrue(any(frame.get("event", {}).get("tool", "").endswith("c0__read_issue") for frame in collected))
            self.assertNotIn("host-mcp-password", json.dumps(collected))
            self.assertNotIn("host-model-test-secret", json.dumps(collected))
            self.assertEqual(service.model.calls, 3)
            calls = [rpc for _, _, rpc in service.requests if rpc["method"] == "tools/call"]
            self.assertEqual(len(calls), 1)
            self.assertEqual(calls[0]["params"]["arguments"], {"repo": "approved/repo", "number": 42})
            self.assertIsNotNone(instance)
            inspect = subprocess.run(["docker", "--host", os.environ["CIRCUS_DOCKER_ENDPOINT"], "inspect", instance],
                                     capture_output=True, timeout=15)
            self.assertNotEqual(inspect.returncode, 0)
            self.assertIn("no such", inspect.stderr.decode().lower())


if __name__ == "__main__":
    unittest.main()

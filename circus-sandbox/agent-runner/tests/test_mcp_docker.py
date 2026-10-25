"""Opt-in real Docker/Codex + real HTTPS MCP with a scripted Responses peer.

The default uses no real inference, SaaS credentials, or external MCP product.
CIRCUS_MCP_REAL_MODEL explicitly opts into real model inference, while MCP remains
the local fixture. The model key comes from CIRCUS_MODEL_API_KEY (or the local
FENNO_API_KEY), never command arguments or test output.
Run after building circus-stdio-bridge with CIRCUS_MCP_DOCKER=1 and the documented
CIRCUS_DOCKER_ENDPOINT / CIRCUS_AGENT_IMAGE / CIRCUS_BRIDGE_BINARY variables.
"""

import base64
import hashlib
import json
import os
import queue
import shlex
import subprocess
import sys
import tempfile
import threading
import unittest
from pathlib import Path

from test_mcp import McpService


class ScriptedModel:
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
        names = {tool.get("name") for tool in body["tools"]}
        namespace = None
        if self.calls == 1:
            candidates = [name for name in names if isinstance(name, str) and name.endswith("c0__read_issue")]
            if not candidates:
                for tool in body["tools"]:
                    if tool.get("type") == "namespace" and tool.get("name") == "mcp__circus":
                        candidates = [entry["name"] for entry in tool["tools"] if entry.get("name") == "c0__read_issue"]
                        namespace = tool["name"]
            assert len(candidates) == 1, "approved MCP tool not found"
            name, arguments = candidates[0], {"number": 42}
        elif self.calls == 2:
            outputs = [item for item in body["input"] if item.get("type") == "function_call_output"]
            assert any("Issue 42: fix addition" in str(item.get("output")) for item in outputs), f"MCP result absent: {outputs}"
            assert not any("host-mcp-password" in str(item.get("output")) for item in outputs), "MCP credential leaked"
            digest = hashlib.sha256(b"host-mcp-password").hexdigest()
            code = """import os
import hashlib
import http.client
import json
import tomllib
from pathlib import Path
assert 'CIRCUS_MCP_CONFIG' not in os.environ
assert 'CIRCUS_MODEL_API_KEY' not in os.environ
config=Path('/workspace/.circus/home/.codex/config.toml').read_text()
assert 'headersFile' not in config and 'headersKeychain' not in config
for value in [*os.environ.values(), config]:
    for offset in range(len(value) - 16):
        assert hashlib.sha256(value[offset:offset+17].encode()).hexdigest() != 'SECRET_DIGEST'
assert 'mcp_servers.circus' in config
port = int(tomllib.loads(config)['mcp_servers']['circus']['args'][-1])
for index, (method, params) in enumerate([
    ('tools/call', {'name':'c0__read_issue','arguments':{'number':42,'repo':'forbidden/repo'}}),
    ('tools/call', {'name':'c0__delete_issue','arguments':{}}),
    ('resources/read', {'uri':'file:///etc/passwd'})
]):
    client = http.client.HTTPConnection('127.0.0.1', port, timeout=10)
    request = {'jsonrpc':'2.0','id':900+index,'method':method,'params':params}
    client.request('POST','/mcp',body=json.dumps(request),headers={
        'Authorization':'Bearer '+os.environ['CIRCUS_TASK_TOKEN'],'Content-Type':'application/json'})
    response = client.getresponse()
    assert response.status == 200
    denial = json.loads(response.read())
    assert denial['error']['code'] == -32000 and 'result' not in denial
    client.close()
Path('result.txt').write_text('MCP issue 42 read through host policy; secret absent\\n')
print('MCP credential isolation passed')
""".replace("SECRET_DIGEST", digest)
            command = "python3 -c " + shlex.quote(code)
            if "exec_command" in names:
                name, arguments = "exec_command", {"cmd": command, "yield_time_ms": 1000, "max_output_tokens": 1024}
            elif "shell_command" in names:
                name, arguments = "shell_command", {"command": command, "timeout_ms": 10000}
            else:
                name, arguments = "shell", {"command": ["sh", "-c", command], "timeout_ms": 10000}
        elif self.calls == 3:
            outputs = [item for item in body["input"] if item.get("type") == "function_call_output"]
            assert any("MCP credential isolation passed" in str(item.get("output")) for item in outputs), "guest checks failed"
            item = {"id": "msg_done", "type": "message", "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": "MCP task completed.", "annotations": []}]}
        else:
            raise AssertionError("unexpected model request count")
        if self.calls < 3:
            item = {"id": f"fc_{self.calls}", "type": "function_call", "call_id": f"call_{self.calls}",
                    "name": name, "arguments": json.dumps(arguments)}
            if namespace:
                item["namespace"] = namespace
        response = {"id": f"resp_{self.calls}", "object": "response", "created_at": 1,
                    "status": "completed", "output": [item],
                    "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}
        events = [
            {"type": "response.created", "response": {**response, "status": "in_progress", "output": []}},
            {"type": "response.output_item.added", "output_index": 0, "item": item},
            {"type": "response.output_item.done", "output_index": 0, "item": item},
            {"type": "response.completed", "response": response}]
        return b"".join(("event: " + event["type"] + "\ndata: " + json.dumps(event) + "\n\n").encode() for event in events)


@unittest.skipUnless(os.environ.get("CIRCUS_MCP_DOCKER") == "1", "requires explicit real Docker and Codex image")
class DockerMcpTests(unittest.TestCase):
    def test_codex_calls_host_mcp_and_never_receives_credentials(self):
        service = McpService()
        self.addCleanup(service.close)
        real_model = os.environ.get("CIRCUS_MCP_REAL_MODEL")
        service.model = None if real_model else ScriptedModel()
        with tempfile.TemporaryDirectory(prefix="circus-mcp-docker-") as directory:
            root = Path(directory)
            key, headers = root / "model-key", root / "mcp-headers.json"
            key.write_text((os.environ.get("CIRCUS_MODEL_API_KEY") or os.environ["FENNO_API_KEY"]) if real_model else "host-model-test-secret")
            headers.write_text(json.dumps({"Authorization": "Bearer host-mcp-password"}))
            key.chmod(0o600)
            headers.chmod(0o600)
            connection = service.config()
            del connection["headers"]
            connection["headersFile"] = str(headers)
            config = {"version": 1, "dockerEndpoint": os.environ["CIRCUS_DOCKER_ENDPOINT"],
                      "agentImage": os.environ["CIRCUS_AGENT_IMAGE"], "python": sys.executable,
                      "resources": {"memoryBytes": 536870912, "cpuMillis": 1000, "maxProcesses": 128,
                                    "workspaceBytes": 134217728, "maxFileBytes": 2097152},
                      "model": {"baseUrl": os.environ["CIRCUS_MCP_MODEL_BASE_URL"] if real_model else service.base + "/v1",
                                "model": real_model or "circus-fixture", "apiKeyFile": str(key),
                                **(({"caFile": os.environ["CIRCUS_MCP_MODEL_CA_FILE"]} if os.environ.get("CIRCUS_MCP_MODEL_CA_FILE") else {}) if real_model else {"caFile": service.cert})}, "mcpConnections": [connection]}
            config_path = root / "config.json"
            config_path.write_text(json.dumps(config))
            process = subprocess.Popen([os.environ["CIRCUS_BRIDGE_BINARY"], "--config", str(config_path)],
                                       stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            def settle():
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=40)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
                for pipe in (process.stdin, process.stdout, process.stderr):
                    if pipe and not pipe.closed:
                        pipe.close()
            self.addCleanup(settle)
            frames = queue.Queue()
            def receive():
                for line in process.stdout:
                    frames.put(json.loads(line))
                frames.put(None)
            thread = threading.Thread(target=receive, daemon=True)
            thread.start()
            self.assertEqual(frames.get(timeout=20)["type"], "ready")
            request = {"v": 1, "type": "start", "requestId": "mcp-docker", "request": {
                "prompt": ("Use the Circus MCP tool to read approved issue number 42. Then run Python assertions that "
                           "CIRCUS_MCP_CONFIG and CIRCUS_MODEL_API_KEY are absent from os.environ, "
                           "the config /workspace/.circus/home/.codex/config.toml has no headersFile or headersKeychain "
                           "and does contain mcp_servers.circus. "
                           "Only after the MCP tool returns Issue 42 and all assertions pass, write exactly "
                           "MCP issue 42 read through host policy; secret absent followed by a newline to result.txt."),
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
            if service.model:
                self.assertIsNone(service.model.failure, f"{service.model.failure}; MCP methods: {[rpc['method'] for _, _, rpc in service.requests]}; events: {collected}")
            self.assertEqual(result["status"], "succeeded", {
                "result": result, "mcpMethods": [rpc["method"] for _, _, rpc in service.requests],
                "text": "".join(frame.get("event", {}).get("text", "") for frame in collected)})
            self.assertEqual(result["cleanup"], "confirmed")
            artifacts = {item["path"]: base64.b64decode(item["data"]) for item in result["artifacts"]}
            self.assertEqual(artifacts["result.txt"], b"MCP issue 42 read through host policy; secret absent\n")
            self.assertTrue(any(frame.get("event", {}).get("tool") == "mcpToolCall" for frame in collected), collected)
            self.assertNotIn("host-mcp-password", json.dumps(collected))
            self.assertIsNotNone(instance)
            inspect = subprocess.run(["docker", "--host", os.environ["CIRCUS_DOCKER_ENDPOINT"], "inspect", instance],
                                     capture_output=True, timeout=15)
            self.assertNotEqual(inspect.returncode, 0)
            self.assertIn("no such", inspect.stderr.decode().lower())
            calls = [rpc for _, _, rpc in service.requests if rpc["method"] == "tools/call"]
            self.assertGreaterEqual(len(calls), 1)
            if not real_model:
                self.assertEqual(len(calls), 1, "a denied guest request reached the MCP service")
            self.assertEqual(calls[0]["params"]["arguments"], {"repo": "approved/repo", "number": 42})


if __name__ == "__main__":
    unittest.main()

"""A scripted Responses peer, NOT a model. Used only by the Docker integration test.

It asks the real Codex binary to execute a tool, then returns a final answer after
checking the tool output. This validates transport/tool execution without inference.
"""


class ScriptedResponses:
    def __init__(self):
        self.calls = 0

    def forward(self, raw, emit):
        body = decode_json(raw)
        assert body["store"] is False
        assert body["model"] == "circus-fixture"
        self.calls += 1
        if self.calls == 1:
            import shlex
            names = {tool.get("name") for tool in body["tools"]}
            code = """from pathlib import Path
import errno
import os
import socket
assert 'CIRCUS_MODEL_API_KEY' not in os.environ
assert not any('host-only-test-sentinel' in value for value in os.environ.values())
interfaces = {name for _, name in socket.if_nameindex()}
# Docker Desktop's Linux kernel may expose inactive tunnel devices in a fresh
# network namespace. None may be UP or have a route out of this sandbox.
for name in interfaces - {'lo'}:
    flags = int(Path(f'/sys/class/net/{name}/flags').read_text().strip(), 16)
    assert not flags & 1, f'active external interface: {name}'
assert all(row.split()[0] == 'lo' for row in Path('/proc/net/route').read_text().splitlines()[1:])
assert all(row.split()[-1] == 'lo' for row in Path('/proc/net/ipv6_route').read_text().splitlines())
with socket.socket() as probe:
    probe.settimeout(0.2)
    assert probe.connect_ex(('192.0.2.1', 443)) == errno.ENETUNREACH
assert Path('answer.py').read_text() == 'def add(a, b): return a - b\\n'
Path('answer.py').write_text('def add(a, b): return a + b\\n')
exec(Path('answer.py').read_text())
assert add(20, 22) == 42
persisted = Path('persisted.txt')
if persisted.exists():
    assert persisted.read_text() == 'durable\\n', 'durable workspace was not restored'
else:
    persisted.write_text('durable\\n')
Path('result.txt').write_text('42\\n')
print('tests passed')
"""
            command = "python3 -c " + shlex.quote(code)
            if "exec_command" in names:
                name, args = "exec_command", {"cmd": command, "yield_time_ms": 1000, "max_output_tokens": 1024}
            elif "shell_command" in names:
                name, args = "shell_command", {"command": command, "timeout_ms": 10000}
            elif "shell" in names:
                name, args = "shell", {"command": ["sh", "-c", command], "timeout_ms": 10000}
            else:
                raise AssertionError("fixture could not find a supported shell tool")
            item = {"id": "fc_fixture", "type": "function_call", "call_id": "call_fixture",
                    "name": name, "arguments": json.dumps(args)}
        elif self.calls == 2:
            outputs = [item for item in body["input"] if item.get("type") == "function_call_output"]
            assert any("tests passed" in str(item.get("output")) for item in outputs), "tool did not pass"
            item = {"id": "msg_fixture", "type": "message", "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": "Created answer.py, tests passed, result.txt is ready.", "annotations": []}]}
        else:
            raise AssertionError("unexpected extra model request")
        response = {"id": f"resp_fixture_{self.calls}", "object": "response", "created_at": 1,
                    "status": "completed", "output": [item],
                    "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}
        events = [
            {"type": "response.created", "response": {**response, "status": "in_progress", "output": []}},
            {"type": "response.output_item.added", "output_index": 0, "item": item},
            {"type": "response.output_item.done", "output_index": 0, "item": item},
            {"type": "response.completed", "response": response},
        ]
        emit("http_start", None)
        for event in events:
            data = ("event: " + event["type"] + "\ndata: " + json.dumps(event) + "\n\n").encode()
            # Split even UTF-8/JSON/event boundaries, like a real streaming response.
            for offset in range(0, len(data), 37):
                emit("http_chunk", data[offset:offset + 37])
        emit("http_end", None)


# This peer contains only public fixture data. Surface its assertion and protocol
# diagnostics to the test collector; production still emits only safe categories.
_report_failure = failure


def failure(error):
    if isinstance(error, (AssertionError, BridgeError)):
        event("text", text=f"fixture failure: {error}")
    _report_failure(error)


main_host(ScriptedResponses())

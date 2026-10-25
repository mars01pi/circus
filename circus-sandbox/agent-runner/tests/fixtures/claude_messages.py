"""Scripted Anthropic Messages peer. Executes the real CLI's Bash tool, NOT inference."""

import hashlib
import json
import shlex


class ScriptedMessages:
    def __init__(self):
        self.calls = 0

    def reply(self, body):
        assert body["model"] == "circus-fixture"
        assert body["stream"] is True
        self.calls += 1
        if self.calls == 1:
            assert "Bash" in {tool["name"] for tool in body["tools"]}
            code = """from pathlib import Path
import errno, hashlib, os, socket
assert 'CIRCUS_MODEL_API_KEY' not in os.environ
assert all(hashlib.sha256(value.encode()).hexdigest() != 'HOST_CREDENTIAL_DIGEST' for value in os.environ.values())
assert not Path('/var/run/docker.sock').exists()
with socket.socket() as probe:
    probe.settimeout(0.2)
    assert probe.connect_ex(('192.0.2.1', 443)) == errno.ENETUNREACH
assert Path('answer.py').read_text() == 'def add(a, b): return a - b\\n'
Path('answer.py').write_text('def add(a, b): return a + b\\n')
exec(Path('answer.py').read_text())
assert add(20, 22) == 42 and add(-2, 2) == 0
Path('result.txt').write_text('42\\n')
print('tests passed; no upstream credential in guest')
"""
            code = code.replace("HOST_CREDENTIAL_DIGEST", hashlib.sha256(b"host-only-claude-sentinel").hexdigest())
            block = {"type": "tool_use", "id": "toolu_circus", "name": "Bash", "input": {}}
            delta = {"type": "input_json_delta", "partial_json": json.dumps({
                "command": "python3 -c " + shlex.quote(code), "timeout": 10000,
                "description": "Repair fixture and verify isolation"})}
            stop = "tool_use"
        elif self.calls == 2:
            outputs = [block for item in body["messages"] if item["role"] == "user"
                       for block in item["content"] if isinstance(block, dict) and block.get("type") == "tool_result"]
            assert any("tests passed" in str(block.get("content")) and not block.get("is_error") for block in outputs), outputs
            block = {"type": "text", "text": ""}
            delta = {"type": "text_delta", "text": "Repaired answer.py; tests passed; result.txt is ready."}
            stop = "end_turn"
        else:
            raise AssertionError("unexpected extra Messages request")
        events = [
            {"type": "message_start", "message": {
                "id": f"msg_circus_{self.calls}", "type": "message", "role": "assistant",
                "model": "circus-fixture", "content": [], "stop_reason": None,
                "stop_sequence": None, "usage": {"input_tokens": 10, "output_tokens": 1}}},
            {"type": "content_block_start", "index": 0, "content_block": block},
            {"type": "content_block_delta", "index": 0, "delta": delta},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None}, "usage": {"output_tokens": 20}},
            {"type": "message_stop"},
        ]
        return b"".join((f"event: {event['type']}\ndata: " + json.dumps(event) + "\n\n").encode() for event in events)

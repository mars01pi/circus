"""Messages policy/event tests. No model calls or sandbox isolation claims."""

import base64
import json
from pathlib import Path
import types
import unittest
from unittest.mock import patch

SOURCE = Path(__file__).resolve().parents[1] / "src"
bridge = types.ModuleType("circus_claude_test")
exec((SOURCE / "codex_bridge.py").read_text(), bridge.__dict__)
exec((SOURCE / "claude_bridge.py").read_text(), bridge.__dict__)


class ClaudeTests(unittest.TestCase):
    def setUp(self):
        self.provider = bridge.ClaudeModelEndpoint("https://example.invalid/approved/v1", "host-secret")
        self.controller = bridge.ClaudeHostController(self.provider, "explicit-model", "task-token", "test")

    def request(self, **changes):
        raw = json.dumps({"model": "explicit-model", "stream": True, "messages": []}).encode()
        return {"id": 1, "method": "POST", "path": "/v1/messages?beta=true", "token": "task-token",
                "headers": {"anthropic-version": "2023-06-01", "anthropic-beta": "context-1m-2025-08-07"},
                "body": base64.b64encode(raw).decode(), **changes}

    def test_only_host_destination_and_real_key_are_used(self):
        self.controller.validate_request(self.request())
        self.assertEqual(self.provider.path, "/approved/v1/messages?beta=true")
        headers = self.provider.request_headers()
        self.assertEqual(headers["X-Api-Key"], "host-secret")
        self.assertNotIn("Authorization", headers)
        self.assertEqual(headers["anthropic-beta"], "context-1m-2025-08-07")

    def test_rejects_wrong_route_credentials_model_headers_and_replay(self):
        for changes in [
            {"path": "/v1/responses"}, {"path": "https://evil.invalid/v1/messages"},
            {"token": "wrong"}, {"headers": {"Authorization": "Bearer attack"}},
            {"headers": {"anthropic-version": "2023-06-01", "anthropic-beta": "x\r\nHost: evil"}},
            {"body": base64.b64encode(b'{"model":"other","stream":true}').decode()},
            {"body": base64.b64encode(b'{"model":"explicit-model","stream":false}').decode()},
        ]:
            with self.subTest(changes=changes), self.assertRaises(bridge.BridgeError):
                self.controller.validate_request(self.request(**changes))
        self.controller.validate_request(self.request())
        with self.assertRaises(bridge.BridgeError):
            self.controller.validate_request(self.request())

    def test_text_tools_and_success_are_normalized_without_fake_exit_code(self):
        events = []
        with patch.object(bridge, "event", side_effect=lambda kind, **data: events.append((kind, data))), \
                patch.object(bridge, "send"):
            self.controller.on_stream({"type": "system", "subtype": "init", "model": "explicit-model", "session_id": "one"})
            self.controller.on_stream({"type": "assistant", "message": {"content": [
                {"type": "text", "text": "Working"}, {"type": "tool_use", "id": "t1", "name": "Bash"}]}})
            self.controller.on_stream({"type": "user", "message": {"content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "passed"}]}})
            self.controller.on_stream({"type": "result", "subtype": "success", "is_error": False, "session_id": "one"})
        self.assertTrue(self.controller.finished)
        self.assertTrue(self.controller.revoked.is_set())
        self.assertIn(("tool", {"id": "t1", "tool": "Bash", "phase": "completed", "exit_code": None}), events)
        self.assertEqual(events[-1], ("turn_completed", {}))

    def test_failed_denied_and_mismatched_sessions_are_not_success(self):
        for result in [
            {"type": "result", "subtype": "error_during_execution", "is_error": True},
            {"type": "result", "subtype": "success", "is_error": False, "permission_denials": ["Bash"]},
            {"type": "result", "subtype": "success", "is_error": False, "session_id": "two"},
        ]:
            self.controller.session_id = "one"
            with self.subTest(result=result), self.assertRaises(bridge.BridgeError):
                self.controller.on_stream(result)
            self.assertFalse(self.controller.finished)

    def test_configured_mcp_must_be_connected_before_running(self):
        self.controller.mcp = object()
        for servers in [[], [{"name": "circus", "status": "failed"}], [{"name": "other", "status": "connected"}]]:
            with self.subTest(servers=servers), self.assertRaises(bridge.BridgeError):
                self.controller.on_stream({"type": "system", "subtype": "init", "model": "explicit-model",
                                           "mcp_servers": servers})
        with patch.object(bridge, "event") as emit:
            self.controller.on_stream({"type": "system", "subtype": "init", "model": "explicit-model",
                                       "mcp_servers": [{"name": "circus", "status": "connected"}]})
        emit.assert_called_once_with("running")

    def test_resume_request_fails_closed_before_cli_start(self):
        failures = []
        with patch.dict(bridge.os.environ, {"CIRCUS_RESUME_THREAD": "thread-1"}), \
                patch.object(bridge, "failure", failures.append), \
                patch.object(bridge.subprocess, "Popen") as popen:
            with self.assertRaises(SystemExit):
                bridge.main_claude_host()
        self.assertEqual(len(failures), 1)
        self.assertIsInstance(failures[0], bridge.BridgeError)
        popen.assert_not_called()


if __name__ == "__main__":
    unittest.main()

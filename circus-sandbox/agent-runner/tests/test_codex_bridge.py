"""Host/guest protocol policy tests; no Docker, model traffic or real credentials."""

import base64
import importlib.util
import io
import json
import ssl
import tomllib
import types
from pathlib import Path
import unittest
from unittest.mock import patch

SOURCE = Path(__file__).resolve().parents[1] / "src/codex_bridge.py"
SPEC = importlib.util.spec_from_file_location("circus_codex_bridge", SOURCE)
bridge = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bridge)


class ModelPolicyTests(unittest.TestCase):
    def setUp(self):
        self.controller = bridge.HostController(None, "allowed-model", "task-token", "task")

    def request(self, body=None, **overrides):
        raw = body if body is not None else b'{"model":"allowed-model","stream":true,"store":true}'
        return {"id": 1, "method": "POST", "path": "/v1/responses", "token": "task-token",
                "body": base64.b64encode(raw).decode(), **overrides}

    def test_guest_cannot_select_destination_or_forward_authorization(self):
        for override in [{"path": "https://other.invalid/responses"}, {"path": "/v1/models"},
                         {"path": "/v1/responses?redirect=1"}, {"method": "CONNECT"},
                         {"token": "wrong"}]:
            with self.subTest(override=override), self.assertRaises(bridge.BridgeError):
                self.controller.validate_request(self.request(**override))

    def test_model_and_background_are_fixed_and_storage_is_disabled(self):
        for raw in [b'{"model":"other","stream":true}',
                    b'{"model":"allowed-model","stream":false}',
                    b'{"model":"allowed-model","stream":true,"background":true}',
                    b'{"model":"other","model":"allowed-model","stream":true}']:
            with self.subTest(raw=raw), self.assertRaises(bridge.BridgeError):
                self.controller.validate_request(self.request(raw))
        _, body = self.controller.validate_request(self.request())
        self.assertIs(json.loads(body)["store"], False)

    def test_no_replay_and_bounded_requests(self):
        self.controller.validate_request(self.request())
        with self.assertRaises(bridge.BridgeError):
            self.controller.validate_request(self.request())
        self.controller.seen = set(range(bridge.MAX_REQUESTS))
        with self.assertRaises(bridge.BridgeError):
            self.controller.validate_request(self.request(id=100))

    def test_bounded_untrusted_json_and_encodings(self):
        for data in [b'{"a":NaN}', b'{"a":1,"a":2}', b'\xff', b'[' * 2000]:
            with self.subTest(data=data[:20]), self.assertRaises(bridge.BridgeError):
                bridge.decode_json(data)
        for encoded in ["!", "x" * 100, None]:
            with self.assertRaises(bridge.BridgeError):
                bridge.unpack(encoded, 16)
        with self.assertRaises(bridge.BridgeError):
            list(bridge.frames(io.BytesIO(b'{"channel":"rpc"}')))
        with patch.object(bridge, "MAX_FRAME", 10), self.assertRaises(bridge.BridgeError):
            list(bridge.frames(io.BytesIO(b'{"channel":"rpc"}\n')))

    def test_endpoint_requires_https_and_has_no_guest_url(self):
        for url in ["http://example.com/v1", "https://user:pass@example.com/v1",
                    "https://example.com/v1?api_key=x", "https://example.com/v1#x",
                    "https://example.com/../v1", "https://example.com:bad/v1"]:
            with self.subTest(url=url), self.assertRaises(bridge.BridgeError):
                bridge.ModelEndpoint(url, "host-key")
        endpoint = bridge.ModelEndpoint("https://gateway.example/api/v1/", "host-key")
        self.assertEqual((endpoint.host, endpoint.port, endpoint.path),
                         ("gateway.example", 443, "/api/v1/responses"))

    def test_upstream_errors_do_not_echo_credentials(self):
        class FailingProvider:
            def forward(self, *_args):
                raise RuntimeError("upstream echoed host-key and task prompt")
        self.controller.provider = FailingProvider()
        sent = []
        with patch.object(bridge, "send", sent.append), patch.object(bridge, "event") as events:
            self.controller.model_request(self.request())
            self.controller.worker.join(timeout=2)
        self.assertEqual(sent, [{"channel": "http_error", "id": 1}])
        events.assert_called_once_with("failure", code="controller", status=None)

    def test_guest_config_keeps_dynamic_port_as_data(self):
        config = tomllib.loads(bridge.guest_config(12345))
        self.assertEqual(config["model_provider"], "circus")
        provider = config["model_providers"]["circus"]
        self.assertEqual(provider["base_url"], "http://127.0.0.1:12345/v1")
        self.assertEqual(provider["env_key"], "CIRCUS_TASK_TOKEN")
        self.assertFalse(provider["supports_websockets"])
        self.assertEqual(provider["request_max_retries"], 0)
        for invalid in [0, 65536, True, "12345", '1234";sh']:
            with self.subTest(port=invalid), self.assertRaises(bridge.BridgeError):
                bridge.guest_config(invalid)

    def test_revocation_prevents_new_model_requests(self):
        self.controller.revoked.set()
        with self.assertRaises(bridge.BridgeError):
            self.controller.model_request(self.request())

    def test_thread_start_reports_agent_session_and_resume_uses_native_thread(self):
        cases = [
            (None, "thread/start",
             {"model": "allowed-model", "modelProvider": "circus",
              "cwd": "/workspace/project", "approvalPolicy": "never"},
             {"thread": {"id": "thread-new"}}, "thread-new"),
            ("thread-persisted_1", "thread/resume",
             {"threadId": "thread-persisted_1"}, {}, "thread-persisted_1"),
            ("thread-persisted_2", "thread/resume",
             {"threadId": "thread-persisted_2"},
             {"thread": {"id": "thread-persisted_2"}}, "thread-persisted_2"),
        ]
        for resume, method, expected, response, session in cases:
            with self.subTest(resume=resume):
                controller = bridge.HostController(
                    None, "allowed-model", "task-token", "task", resume)
                sent = []
                with patch.object(bridge, "send", sent.append), \
                        patch.object(bridge, "event") as emitted:
                    controller.on_rpc({"id": 1, "result": {"userAgent": "codex"}})
                    controller.on_rpc({"id": 2, "result": response})
                requests = [message["message"] for message in sent]
                assert requests[0] == {"method": "initialized", "params": {}}
                assert requests[1] == {"method": method, "params": expected, "id": 2}
                assert requests[2]["method"] == "turn/start"
                assert requests[2]["params"]["threadId"] == session
                assert emitted.call_args.args == ("agent_session",)
                assert emitted.call_args.kwargs == {"id": session}
        controller = bridge.HostController(
            None, "allowed-model", "task-token", "task", "thread-persisted_3")
        sent = []
        with patch.object(bridge, "send", sent.append), patch.object(bridge, "event"):
            controller.on_rpc({"id": 1, "result": {"userAgent": "codex"}})
            sent.clear()
            with self.assertRaises(bridge.BridgeError):
                controller.on_rpc({"id": 2, "result": {"thread": {"id": "other"}}})
        assert all(message["message"].get("method") != "turn/start" for message in sent)

    def test_agent_thread_ids_are_bounded_and_path_free(self):
        with self.assertRaises(bridge.BridgeError):
            bridge.HostController(None, "allowed-model", "task-token", "task", "")
        for invalid in ["/tmp/thread", "a b", "a" * 129, "thread\0", "thread/1"]:
            with self.subTest(invalid=invalid), self.assertRaises(bridge.BridgeError):
                bridge.thread_id(invalid)
        assert bridge.thread_id("0199-abcd_EF") == "0199-abcd_EF"

    def test_completions_are_correlated_and_failures_are_not_success(self):
        self.controller.thread_id, self.controller.turn_id = "thread", "turn"
        for thread, turn, status in [("other", "turn", "completed"),
                                     ("thread", "other", "completed"),
                                     ("thread", "turn", "failed")]:
            with self.assertRaises(bridge.BridgeError):
                self.controller.on_rpc({"method": "turn/completed", "params": {
                    "threadId": thread, "turn": {"id": turn, "status": status}}})
        sent = []
        with patch.object(bridge, "send", sent.append), patch.object(bridge, "event"):
            self.controller.on_rpc({"method": "turn/completed", "params": {
                "threadId": "thread", "turn": {"id": "turn", "status": "completed"}}})
        self.assertTrue(self.controller.finished)
        self.assertTrue(self.controller.revoked.is_set())
        self.assertEqual(sent, [{"channel": "shutdown"}])

    def test_unexpected_approval_requests_are_not_granted(self):
        with self.assertRaises(bridge.BridgeError):
            self.controller.on_rpc({"id": 10, "method": "item/commandExecution/requestApproval"})

    def test_failures_preserve_only_safe_category_and_http_status(self):
        for error, code, status in [
            (ssl.SSLError("secret TLS detail"), "tls", None),
            (TimeoutError("secret timeout detail"), "timeout", None),
            (OSError("secret transport detail"), "transport", None),
            (bridge.BridgeError("secret upstream body", "rejected", 403), "rejected", 403),
        ]:
            with patch.object(bridge, "event") as emitted:
                bridge.failure(error)
                emitted.assert_called_once_with("failure", code=code, status=status)

    def test_normalized_item_events_are_correlated_and_bounded(self):
        self.controller.thread_id, self.controller.turn_id = "thread", "turn"
        params = {"threadId": "thread", "turnId": "turn", "itemId": "item", "delta": "x" * 9000}
        with patch.object(bridge, "event") as emitted:
            self.controller.on_rpc({"method": "item/agentMessage/delta", "params": params})
            self.assertEqual(emitted.call_count, 3)
            self.assertTrue(all(len(call.kwargs["text"]) <= 4096 for call in emitted.call_args_list))
        with self.assertRaises(bridge.BridgeError):
            self.controller.on_rpc({"method": "item/agentMessage/delta", "params": {**params, "turnId": "other"}})

    def test_events_redact_host_and_task_credentials(self):
        output = io.BytesIO()
        with patch.dict(bridge.os.environ, {"CIRCUS_MODEL_API_KEY": "secret-host-key", "CIRCUS_TASK_TOKEN": "secret-task-key"}):
            with patch.object(bridge.sys, "stderr", types.SimpleNamespace(buffer=output)):
                bridge.event("text", text="secret-host-key secret-task-key")
        self.assertNotIn(b"secret", output.getvalue())
        self.assertEqual(json.loads(output.getvalue())["text"], "[REDACTED] [REDACTED]")

    def test_tool_final_output_falls_back_only_when_no_deltas_were_received(self):
        self.controller.thread_id, self.controller.turn_id = "thread", "turn"
        params = {"threadId": "thread", "turnId": "turn", "item": {
            "id": "command", "type": "commandExecution", "exitCode": 1,
            "aggregatedOutput": "assertion failed\n"}}
        with patch.object(bridge, "event") as emitted:
            self.controller.on_rpc({"method": "item/completed", "params": params})
            self.assertEqual([call.args for call in emitted.call_args_list], [("tool_output",), ("tool",)])
            self.assertEqual(emitted.call_args_list[0].kwargs["text"], "assertion failed\n")
        with patch.object(bridge, "event") as emitted:
            self.controller.on_rpc({"method": "item/commandExecution/outputDelta", "params": {
                "threadId": "thread", "turnId": "turn", "itemId": "command", "delta": "assertion failed\n"}})
            self.controller.on_rpc({"method": "item/completed", "params": params})
            self.assertEqual([call.args for call in emitted.call_args_list], [("tool_output",), ("tool",)])


if __name__ == "__main__":
    unittest.main()

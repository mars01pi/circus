"""Real HTTPS MCP transport + host authorization, using test-only credentials."""

import base64
import http.server
import json
import ssl
import subprocess
import tempfile
import threading
import types
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
bridge = types.ModuleType("circus_mcp_test")
exec(compile((ROOT / "src/codex_bridge.py").read_text() + "\n" +
             (ROOT / "src/mcp.py").read_text(), "circus-mcp-test", "exec"), bridge.__dict__)


class McpService:
    """An actual TLS/HTTP service implementing the tested MCP subset, not a SaaS."""

    def __init__(self):
        self.directory = tempfile.TemporaryDirectory(prefix="circus-mcp-test-")
        root = Path(self.directory.name)
        key, cert = root / "key.pem", root / "cert.pem"
        subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                        "-subj", "/CN=localhost", "-addext", "subjectAltName=IP:127.0.0.1",
                        "-keyout", str(key), "-out", str(cert)], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=20)
        self.cert = str(cert)
        self.requests, self.model_requests = [], []
        self.status, self.sse, self.tool_error = 200, False, False
        self.model = None
        self.payload_override = None
        fixture = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args):
                pass

            def do_POST(self):
                self.close_connection = True
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                if self.path in ("/v1/responses", "/v1/messages?beta=true") and fixture.model:
                    fixture.model_requests.append(body)
                    payload = fixture.model(body)
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                    return
                fixture.requests.append((self.path, dict(self.headers), body))
                if self.headers.get("Authorization") != "Bearer host-mcp-password":
                    self.send_error(403)
                    return
                if body["method"] == "notifications/initialized":
                    self.send_response(202)
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                result = fixture.result(body)
                envelope = fixture.payload_override or {"jsonrpc": "2.0", "id": body["id"], "result": result}
                payload = json.dumps(envelope).encode()
                if fixture.sse:
                    payload = b": keepalive\n\nevent: message\ndata: " + payload + b"\n\n"
                self.send_response(fixture.status)
                self.send_header("Content-Type", "text/event-stream" if fixture.sse else "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.send_header("Location", "https://unapproved.invalid/stolen")
                self.send_header("MCP-Session-Id", "test-session")
                self.end_headers()
                self.wfile.write(payload)

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(cert, key)
        self.server.socket = context.wrap_socket(self.server.socket, server_side=True)
        self.worker = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.worker.start()
        self.base = f"https://127.0.0.1:{self.server.server_port}"

    def result(self, rpc):
        method = rpc["method"]
        if method == "initialize":
            return {"protocolVersion": "2025-03-26", "capabilities": {"tools": {}},
                    "serverInfo": {"name": "circus-test", "version": "1"}}
        if method == "tools/list":
            return {"tools": [
                {"name": "read_issue", "description": "Read a repository issue", "inputSchema": {
                    "type": "object", "properties": {"repo": {"type": "string"}, "number": {"type": "integer"}},
                    "required": ["repo", "number"]}},
                {"name": "delete_issue", "inputSchema": {"type": "object", "properties": {}}}]}
        if method == "tools/call":
            if self.tool_error:
                return {"isError": True, "content": [{"type": "text", "text": "password: host-mcp-password"}]}
            assert rpc["params"] == {"name": "read_issue", "arguments": {"repo": "approved/repo", "number": 42}}
            return {"content": [{"type": "text", "text": "Issue 42: fix addition. host-mcp-password"}]}
        raise AssertionError("unexpected service method")

    def config(self):
        return {"id": "issues", "url": self.base + "/mcp", "caFile": self.cert,
                "headers": {"Authorization": "Bearer host-mcp-password"},
                "tools": [{"name": "read_issue", "allowedArguments": ["number"],
                           "fixedArguments": {"repo": "approved/repo"}}]}

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.worker.join(timeout=2)
        self.directory.cleanup()


class McpTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.service = McpService()
        cls.addClassCleanup(cls.service.close)

    def setUp(self):
        self.service.requests.clear()
        self.service.status, self.service.sse, self.service.tool_error = 200, False, False
        self.service.payload_override = None
        self.broker = bridge.McpBroker([self.service.config()], threading.Event())
        self.addCleanup(self.broker.close)
        initialized = self.call("initialize", {"protocolVersion": "2025-03-26", "capabilities": {},
                                               "clientInfo": {"name": "test", "version": "1"}})
        self.assertIn("result", initialized)

    def call(self, method, params=None):
        return self.broker.handle({"jsonrpc": "2.0", "id": 1, "method": method, "params": params or {}})

    def test_tls_and_authentication_with_scoped_catalog_and_call(self):
        result = self.call("tools/list", {"_meta": {"progressToken": 0}})["result"]
        self.assertEqual([tool["name"] for tool in result["tools"]], ["c0__read_issue"])
        schema = result["tools"][0]["inputSchema"]
        self.assertEqual(set(schema["properties"]), {"number"})
        self.assertFalse(schema["additionalProperties"])
        output = self.call("tools/call", {"name": "c0__read_issue", "arguments": {"number": 42},
                     "_meta": {"claudecode/toolUseId": "toolu_test", "progressToken": 2,
                               "x-codex-turn-metadata": {"model": "test", "reasoning_effort": "low"}}})
        self.assertIn("Issue 42", str(output))
        self.assertNotIn("host-mcp-password", str(output))
        self.assertEqual(self.service.requests[-1][2]["params"]["arguments"]["repo"], "approved/repo")
        self.assertEqual(self.service.requests[-1][1]["MCP-Session-Id"], "test-session")

    def test_unknown_tools_arguments_methods_and_destination_are_rejected(self):
        self.call("tools/list")
        before = len(self.service.requests)
        for method, params in [
            ("tools/call", {"name": "c0__delete_issue", "arguments": {}}),
            ("tools/call", {"name": "c0__read_issue", "arguments": {"repo": "other/repo", "number": 42}}),
            ("tools/call", {"name": "c0__read_issue", "arguments": {"number": 42, "url": "https://evil.invalid"}}),
            ("tools/call", {"name": "c0__read_issue", "arguments": {}, "headers": {"Authorization": "guest"}}),
            ("resources/read", {"uri": "file:///etc/passwd"}),
            ("sampling/createMessage", {}),
        ]:
            with self.subTest(method=method, params=params):
                self.assertIn("error", self.call(method, params))
        self.assertEqual(len(self.service.requests), before)

    def test_json_and_sse_are_supported_without_following_redirects(self):
        self.service.sse = True
        self.assertIn("result", self.call("tools/list"))
        self.assertIn("result", self.call("tools/call", {"name": "c0__read_issue", "arguments": {"number": 42}}))
        self.service.status = 302
        self.assertIn("error", self.call("tools/call", {"name": "c0__read_issue", "arguments": {"number": 42}}))
        self.assertTrue(all(path == "/mcp" for path, _, _ in self.service.requests))

    def test_untrusted_ca_and_provider_initiated_requests_fail_closed(self):
        config = self.service.config()
        config["caFile"] = None
        endpoint = bridge.McpEndpoint(config, threading.Event())
        with self.assertRaises(ssl.SSLCertVerificationError):
            endpoint.initialize()
        self.service.payload_override = {"jsonrpc": "2.0", "id": 99, "method": "sampling/createMessage", "params": {}}
        self.assertIn("error", self.call("tools/list"))

    def test_errors_and_results_do_not_echo_credentials(self):
        self.service.tool_error = True
        output = self.call("tools/call", {"name": "c0__read_issue", "arguments": {"number": 42}})
        self.assertTrue(output["result"]["isError"])
        self.assertNotIn("host-mcp-password", str(output))
        self.assertIn("withheld", str(output))

    def test_revocation_and_output_budget(self):
        self.call("tools/list")
        before = len(self.service.requests)
        self.broker.close()
        self.assertIn("error", self.call("tools/call", {"name": "c0__read_issue", "arguments": {"number": 42}}))
        self.assertEqual(len(self.service.requests), before)
        with patch.object(bridge, "MCP_TOTAL", 1), self.assertRaises(bridge.BridgeError):
            self.call("ping")

    def test_invalid_metadata_returns_the_original_rpc_id(self):
        output = self.broker.handle({"jsonrpc": "2.0", "id": "client-42", "method": "tools/list",
                                     "params": {"_meta": {"unexpected": "not authority"}}})
        self.assertEqual(output["id"], "client-42")
        self.assertIn("error", output)
        sent = []
        request = {"channel": "mcp_request", "id": 1, "token": "task-token", "body": base64.b64encode(
            json.dumps({"jsonrpc": "2.0", "id": 42, "method": "ping", "unexpected": True}).encode()).decode()}
        with patch.object(bridge, "send", sent.append):
            self.broker.request(request, "task-token")
            self.broker.worker.join(timeout=2)
        self.assertEqual(sent[0]["body"]["id"], 42)
        self.assertIn("error", sent[0]["body"])

    def test_task_token_replay_and_unknown_wire_fields(self):
        request = {"channel": "mcp_request", "id": 1, "token": "task-token", "body": base64.b64encode(
            json.dumps({"jsonrpc": "2.0", "id": 2, "method": "ping", "params": {}}).encode()).decode()}
        for extra in [{"token": "other-task"}, {"url": "https://evil.invalid"}]:
            with self.assertRaises(bridge.BridgeError):
                self.broker.request({**request, **extra}, "task-token")
        sent = []
        with patch.object(bridge, "send", sent.append):
            self.broker.request(request, "task-token")
            self.broker.worker.join(timeout=2)
        self.assertEqual(sent[0]["body"]["result"], {})
        with self.assertRaises(bridge.BridgeError):
            self.broker.request(request, "task-token")


if __name__ == "__main__":
    unittest.main()

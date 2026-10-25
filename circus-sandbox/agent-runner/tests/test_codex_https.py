"""Real local HTTPS transport tests using a temporary, test-only CA certificate."""

import http.server
import importlib.util
import ssl
import subprocess
import tempfile
import threading
from pathlib import Path
import unittest
from unittest.mock import patch

SOURCE = Path(__file__).resolve().parents[1] / "src/codex_bridge.py"
SPEC = importlib.util.spec_from_file_location("circus_codex_https", SOURCE)
bridge = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bridge)


class HttpsTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.directory = tempfile.TemporaryDirectory(prefix="circus-https-test-")
        cls.addClassCleanup(cls.directory.cleanup)
        root = Path(cls.directory.name)
        key, cert = root / "key.pem", root / "cert.pem"
        # An ephemeral test certificate; never installed into a system trust store.
        subprocess.run([
            "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
            "-subj", "/CN=localhost", "-addext", "subjectAltName=IP:127.0.0.1",
            "-keyout", str(key), "-out", str(cert),
        ], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=20)
        cls.client_context = ssl.create_default_context(cafile=str(cert))
        cls.server_context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        cls.server_context.load_cert_chain(cert, key)
        cls.cert = str(cert)

    def setUp(self):
        test = self
        self.requests = []
        self.status = 200
        self.content_type = "text/event-stream"
        self.payload = b'event: response.completed\ndata: {"type":"response.completed"}\n\n'

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                test.requests.append((self.path, dict(self.headers), body))
                self.send_response(test.status)
                self.send_header("Content-Type", test.content_type)
                self.send_header("Location", "https://unapproved.invalid/stolen")
                self.send_header("Content-Length", str(len(test.payload)))
                self.end_headers()
                self.wfile.write(test.payload)

        self.server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        self.server.socket = self.server_context.wrap_socket(self.server.socket, server_side=True)
        self.worker = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.worker.start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.worker.join, 2)
        self.addCleanup(self.server.shutdown)
        self.endpoint = bridge.ModelEndpoint(
            f"https://127.0.0.1:{self.server.server_port}/approved/v1", "host-only-key")

    def test_tls_verification_is_required(self):
        with self.assertRaises(ssl.SSLCertVerificationError):
            self.endpoint.forward(b"{}", lambda *_: None)
        self.assertEqual(self.requests, [])

    def test_verified_stream_uses_only_host_route_and_credentials(self):
        chunks = []
        self.endpoint.ca_file = self.cert
        self.content_type = "Text/Event-Stream; charset=utf-8"
        self.endpoint.forward(b'{"stream":true}', lambda kind, data: chunks.append((kind, data)))
        self.assertEqual(chunks[0], ("http_start", None))
        self.assertEqual(chunks[-1], ("http_end", None))
        self.assertEqual(b"".join(data for kind, data in chunks if kind == "http_chunk"), self.payload)
        path, headers, body = self.requests[0]
        self.assertEqual(path, "/approved/v1/responses")
        self.assertEqual(headers["Authorization"], "Bearer host-only-key")
        self.assertEqual(body, b'{"stream":true}')

    def test_redirect_is_rejected_and_not_followed(self):
        self.status = 302
        with patch.object(bridge.ssl, "create_default_context", return_value=self.client_context):
            with self.assertRaisesRegex(bridge.BridgeError, "model service rejected"):
                self.endpoint.forward(b"{}", lambda *_: None)
        self.assertEqual(len(self.requests), 1)

    def test_response_budget_is_enforced_on_real_http_reads(self):
        with patch.object(bridge.ssl, "create_default_context", return_value=self.client_context):
            with patch.object(bridge, "MAX_RESPONSE", 8), self.assertRaisesRegex(bridge.BridgeError, "budget"):
                self.endpoint.forward(b"{}", lambda *_: None)


if __name__ == "__main__":
    unittest.main()

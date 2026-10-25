"""Opt-in real Docker + pinned CLI + local HTTPS fixture; no real model inference.

Build the claude_agent example first; set CIRCUS_DOCKER_SANDBOX=1,
CIRCUS_DOCKER_ENDPOINT, CIRCUS_CLAUDE_IMAGE and CIRCUS_CLAUDE_EXAMPLE.
"""

import http.server
import importlib.util
import json
import os
from pathlib import Path
import ssl
import subprocess
import tempfile
import threading
import unittest

SPEC = importlib.util.spec_from_file_location("claude_messages", Path(__file__).parent / "fixtures/claude_messages.py")
fixture = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(fixture)


@unittest.skipUnless(os.environ.get("CIRCUS_DOCKER_SANDBOX") == "1", "requires explicit real Docker")
class ClaudeDockerTests(unittest.TestCase):
    def test_messages_tools_artifacts_and_cleanup_over_verified_https(self):
        with tempfile.TemporaryDirectory(prefix="circus-claude-https-") as directory:
            root = Path(directory)
            key, cert = root / "key.pem", root / "cert.pem"
            subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                            "-subj", "/CN=localhost", "-addext", "subjectAltName=IP:127.0.0.1",
                            "-keyout", str(key), "-out", str(cert)], check=True,
                           stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=20)
            peer, failures = fixture.ScriptedMessages(), []

            class Handler(http.server.BaseHTTPRequestHandler):
                def log_message(self, *_args):
                    pass

                def do_POST(self):
                    try:
                        assert self.path == "/approved/v1/messages?beta=true", self.path
                        assert self.headers.get("X-Api-Key") == "host-only-claude-sentinel"
                        assert self.headers.get("anthropic-version") == "2023-06-01"
                        assert "Authorization" not in self.headers
                        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                        data = peer.reply(body)
                        self.send_response(200)
                        self.send_header("Content-Type", "text/event-stream")
                        self.send_header("Content-Length", str(len(data)))
                        self.end_headers()
                        for offset in range(0, len(data), 37):
                            self.wfile.write(data[offset:offset + 37])
                            self.wfile.flush()
                    except Exception as error:
                        failures.append(str(error))
                        self.send_error(500)

            server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(cert, key)
            server.socket = context.wrap_socket(server.socket, server_side=True)
            worker = threading.Thread(target=server.serve_forever, daemon=True)
            worker.start()
            try:
                environment = {**os.environ, "CIRCUS_AGENT_IMAGE": os.environ["CIRCUS_CLAUDE_IMAGE"],
                               "CIRCUS_MODEL_BASE_URL": f"https://127.0.0.1:{server.server_port}/approved/v1",
                               "CIRCUS_MODEL": "circus-fixture", "CIRCUS_MODEL_API_KEY": "host-only-claude-sentinel",
                               "CIRCUS_MODEL_CA_FILE": str(cert),
                               "CIRCUS_AGENT_PROMPT": "Fix answer.py, run assertions, and write result.txt."}
                result = subprocess.run([os.environ["CIRCUS_CLAUDE_EXAMPLE"]], env=environment,
                                        capture_output=True, text=True, timeout=210)
            finally:
                server.shutdown()
                server.server_close()
                worker.join(2)
            self.assertEqual(failures, [])
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotIn("host-only-claude-sentinel", result.stdout + result.stderr)
            self.assertEqual(peer.calls, 2)
            self.assertIn("task completed; 2 artifacts retrieved", result.stdout)
            self.assertIn("answer.py: 28 bytes", result.stdout)
            self.assertIn("result.txt: 3 bytes\n42\n", result.stdout)
            self.assertIn("tool: Started", result.stderr)
            self.assertIn("tool: Completed", result.stderr)
            instances = [line.removeprefix("task sandbox: ") for line in result.stderr.splitlines()
                         if line.startswith("task sandbox: ")]
            self.assertEqual(len(instances), 1)
            inspected = subprocess.run(["docker", "--host", os.environ["CIRCUS_DOCKER_ENDPOINT"], "inspect", instances[0]],
                                       capture_output=True, text=True, timeout=10)
            self.assertNotEqual(inspected.returncode, 0)
            self.assertRegex(inspected.stderr.lower(), "no such (object|container)")


if __name__ == "__main__":
    unittest.main()

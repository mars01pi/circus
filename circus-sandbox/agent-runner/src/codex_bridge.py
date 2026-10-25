"""Private Codex/model adapter. Loaded with an explicit host or guest entrypoint.

The guest has no external network. The host accepts only bounded Responses requests
to its operator-configured HTTPS endpoint; the upstream key never crosses the pipe.
"""

import base64
import hmac
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
import queue
import ssl
import subprocess
import sys
import threading
from urllib.parse import urlsplit

MAX_BODY = 2 * 1024 * 1024
MAX_FRAME = 3 * 1024 * 1024
MAX_RESPONSE = 8 * 1024 * 1024
MAX_TOTAL_RESPONSE = 16 * 1024 * 1024
MAX_REQUESTS = 16
CHUNK = 8192
WRITE_LOCK = threading.Lock()
EVENT_LOCK = threading.Lock()


class BridgeError(Exception):
    """Only fixed, non-sensitive diagnostics may be exposed to the operator."""

    def __init__(self, message, code="protocol", status=None):
        super().__init__(message)
        self.code, self.status = code, status


def event(kind, **fields):
    """The host's stderr is a private typed event pipe, never arbitrary logs."""
    secrets = [os.environ.get(name, "") for name in ("CIRCUS_MODEL_API_KEY", "CIRCUS_TASK_TOKEN")]
    for name, value in fields.items():
        if isinstance(value, str):
            for secret in secrets:
                if secret:
                    value = value.replace(secret, "[REDACTED]")
            fields[name] = value
    wire = json.dumps({"kind": kind, **fields}, separators=(",", ":"), allow_nan=False).encode() + b"\n"
    if len(wire) > 64 * 1024:
        raise BridgeError("event frame exceeded its budget", "budget")
    with EVENT_LOCK:
        sys.stderr.buffer.write(wire)
        sys.stderr.buffer.flush()


def failure(error):
    if isinstance(error, BridgeError):
        code, status = error.code, error.status
    elif isinstance(error, ssl.SSLError):
        code, status = "tls", None
    elif isinstance(error, TimeoutError):
        code, status = "timeout", None
    elif isinstance(error, (OSError, http.client.HTTPException)):
        code, status = "transport", None
    else:
        code, status = "controller", None
    event("failure", code=code, status=status)


def text_events(kind, text, **fields):
    if not isinstance(text, str):
        raise BridgeError("invalid text event")
    for offset in range(0, len(text), 4096):
        event(kind, text=text[offset:offset + 4096], **fields)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise BridgeError("duplicate JSON key")
        result[key] = value
    return result


def decode_json(data):
    try:
        return json.loads(data, object_pairs_hook=unique_object,
                          parse_constant=lambda _: (_ for _ in ()).throw(BridgeError("non-finite JSON number")))
    except (ValueError, UnicodeError, RecursionError) as error:
        raise BridgeError("invalid JSON") from error


def send(message):
    wire = json.dumps(message, separators=(",", ":"), allow_nan=False).encode() + b"\n"
    if len(wire) > MAX_FRAME:
        raise BridgeError("outbound frame too large")
    with WRITE_LOCK:
        sys.stdout.buffer.write(wire)
        sys.stdout.buffer.flush()


def frames(stream):
    while True:
        line = stream.readline(MAX_FRAME + 1)
        if not line:
            return
        if len(line) > MAX_FRAME or not line.endswith(b"\n"):
            raise BridgeError("invalid frame boundary or size")
        message = decode_json(line)
        if not isinstance(message, dict):
            raise BridgeError("frame must be an object")
        yield message


def unpack(value, limit):
    if not isinstance(value, str) or len(value) > (limit + 2) // 3 * 4:
        raise BridgeError("invalid encoded body size")
    try:
        body = base64.b64decode(value, validate=True)
    except (ValueError, UnicodeError) as error:
        raise BridgeError("invalid body encoding") from error
    if len(body) > limit:
        raise BridgeError("body too large")
    return body


def required(name, limit):
    value = os.environ.get(name, "")
    if not value or len(value.encode()) > limit or any(ord(c) < 32 for c in value):
        raise BridgeError(f"invalid {name}")
    return value


def thread_id(value):
    """Native agent thread ids are opaque, bounded and never path-like."""
    if (not isinstance(value, str) or not 1 <= len(value) <= 128
            or not all(c.isascii() and (c.isalnum() or c in "-_") for c in value)):
        raise BridgeError("invalid agent thread id")
    return value


class ModelEndpoint:
    def __init__(self, base_url, key, ca_file=None):
        try:
            url = urlsplit(base_url)
            port = url.port or 443
        except ValueError as error:
            raise BridgeError("invalid model endpoint") from error
        if (url.scheme != "https" or not url.hostname or url.username or url.password
                or url.query or url.fragment or any(ord(c) <= 32 for c in base_url)
                or "\\" in base_url or any(part in (".", "..") for part in url.path.split("/"))):
            raise BridgeError("model endpoint must be an explicit HTTPS base URL")
        if not key or any(ord(c) < 32 or ord(c) > 126 for c in key):
            raise BridgeError("invalid upstream API key")
        self.host, self.port = url.hostname, port
        self.path = url.path.rstrip("/") + "/responses"
        self.key = key
        self.ca_file = ca_file

    def request_headers(self):
        return {"Authorization": "Bearer " + self.key,
                "Content-Type": "application/json", "Accept": "text/event-stream"}

    def forward(self, body, emit):
        # http.client does not follow redirects or read environment proxy settings.
        # Destination and Authorization come exclusively from host configuration.
        try:
            context = ssl.create_default_context(cafile=self.ca_file)
        except (OSError, ssl.SSLError) as error:
            raise BridgeError("model trust configuration failed", "tls") from error
        connection = http.client.HTTPSConnection(self.host, self.port, timeout=30, context=context)
        try:
            connection.request("POST", self.path, body=body, headers=self.request_headers())
            response = connection.getresponse()
            if response.status != 200:
                # Never echo upstream errors, headers or redirects into guest logs.
                raise BridgeError("model service rejected the request", "rejected", response.status)
            if response.getheader("Content-Type", "").split(";", 1)[0].strip().lower() != "text/event-stream":
                raise BridgeError("model service must return a Responses event stream")
            emit("http_start", None)
            total = 0
            while chunk := response.read1(CHUNK):
                total += len(chunk)
                if total > MAX_RESPONSE:
                    raise BridgeError("model response exceeded its budget", "budget")
                emit("http_chunk", chunk)
            emit("http_end", None)
        finally:
            connection.close()


class HostController:
    def __init__(self, provider, model, token, prompt, resume=None):
        self.provider, self.model, self.token, self.prompt = provider, model, token, prompt
        self.resume = thread_id(resume) if resume is not None else None
        self.thread_id = self.turn_id = None
        self.finished = False
        self.seen = set()
        self.total = 0
        self.worker = None
        self.revoked = threading.Event()
        self.text_seen = set()
        self.output_seen = set()
        self.mcp = None

    def rpc(self, request_id, method, params):
        message = {"method": method, "params": params}
        if request_id is not None:
            message["id"] = request_id
        send({"channel": "rpc", "message": message})

    def validate_request(self, message):
        request_id = message.get("id")
        if (type(request_id) is not int or request_id <= 0 or request_id in self.seen
                or len(self.seen) >= MAX_REQUESTS):
            raise BridgeError("invalid model request id or request budget exhausted", "budget")
        if message.get("method") != "POST" or message.get("path") != "/v1/responses":
            raise BridgeError("model route is not allowed")
        token = message.get("token")
        if not isinstance(token, str) or not hmac.compare_digest(token.encode(), self.token.encode()):
            raise BridgeError("invalid task credential")
        raw = unpack(message.get("body"), MAX_BODY)
        body = decode_json(raw)
        if (not isinstance(body, dict) or body.get("model") != self.model
                or body.get("stream") is not True or body.get("background", False) is not False):
            raise BridgeError("model request violates task policy")
        # Persisting provider-side responses is never enabled by guest input.
        body["store"] = False
        encoded = json.dumps(body, separators=(",", ":"), allow_nan=False).encode()
        if len(encoded) > MAX_BODY:
            raise BridgeError("model body exceeded its budget")
        self.seen.add(request_id)
        return request_id, encoded

    def model_request(self, message):
        # A new HTTP request can arrive just after the prior worker emitted EOF.
        # Give that worker a bounded chance to release the single-request slot.
        if self.worker and self.worker.is_alive():
            self.worker.join(timeout=0.1)
        if self.finished or self.revoked.is_set() or (self.worker and self.worker.is_alive()):
            raise BridgeError("model channel is unavailable or busy")
        request_id, body = self.validate_request(message)

        def emit(kind, chunk):
            if self.revoked.is_set():
                raise BridgeError("task credential revoked")
            response = {"channel": kind, "id": request_id}
            if chunk is not None:
                self.total += len(chunk)
                if self.total > MAX_TOTAL_RESPONSE:
                    raise BridgeError("task response budget exhausted", "budget")
                response["body"] = base64.b64encode(chunk).decode("ascii")
            send(response)

        def forward():
            try:
                self.provider.forward(body, emit)
            except Exception as error:
                if not self.revoked.is_set():
                    failure(error)
                    try:
                        send({"channel": "http_error", "id": request_id})
                    except OSError:
                        pass

        self.worker = threading.Thread(target=forward, daemon=True)
        self.worker.start()

    def on_rpc(self, message):
        if not isinstance(message, dict):
            raise BridgeError("invalid app-server message")
        if "id" in message:
            if "method" in message:
                # This unattended example never grants an unexpected server request.
                raise BridgeError("app-server requested unsupported client interaction", "unsupported_interaction")
            if "error" in message:
                raise BridgeError("app-server rejected a request")
            result = message.get("result", {})
            if message["id"] == 1:
                if not result.get("userAgent"):
                    raise BridgeError("invalid initialize response")
                self.rpc(None, "initialized", {})
                if self.resume is None:
                    self.rpc(2, "thread/start", {
                        "model": self.model, "modelProvider": "circus", "cwd": "/workspace/project",
                        "approvalPolicy": "never",
                    })
                else:
                    # A persisted thread already carries its model, provider, cwd
                    # and sandbox policy; only the bounded id crosses this seam.
                    self.rpc(2, "thread/resume", {"threadId": self.resume})
            elif message["id"] == 2:
                thread = result.get("thread")
                if thread is not None and not isinstance(thread, dict):
                    raise BridgeError("invalid thread response")
                actual = thread.get("id") if isinstance(thread, dict) else None
                if self.resume is not None:
                    if actual is None:
                        actual = self.resume
                    elif actual != self.resume:
                        raise BridgeError("mismatched resumed thread")
                if not isinstance(actual, str) or not actual:
                    raise BridgeError("missing thread id")
                self.thread_id = thread_id(actual)
                event("agent_session", id=self.thread_id)
                self.rpc(3, "turn/start", {
                    "threadId": self.thread_id, "input": [{"type": "text", "text": self.prompt}],
                    "approvalPolicy": "never",
                    # The existing Docker policy is the enforcement boundary.
                    "sandboxPolicy": {"type": "externalSandbox", "networkAccess": "restricted"},
                })
            elif message["id"] == 3:
                turn_id = result.get("turn", {}).get("id")
                if self.turn_id is not None and turn_id != self.turn_id:
                    raise BridgeError("mismatched turn response")
                self.turn_id = turn_id
            else:
                raise BridgeError("unexpected response id")
        elif message.get("method") == "mcpServer/startupStatus/updated":
            params = message.get("params", {})
            if self.mcp is not None and params.get("name") == "circus" and params.get("status") == "failed":
                raise BridgeError("authorized MCP connection could not initialize")
        elif message.get("method") in ("turn/started", "turn/completed"):
            params = message.get("params", {})
            turn = params.get("turn", {})
            turn_id = turn.get("id")
            if (params.get("threadId") != self.thread_id or not isinstance(turn_id, str)
                    or (self.turn_id is not None and self.turn_id != turn_id)):
                raise BridgeError("mismatched turn notification")
            self.turn_id = turn_id
            if message["method"] == "turn/started":
                event("running")
            if message["method"] == "turn/completed":
                if turn.get("status") != "completed":
                    raise BridgeError("agent turn did not complete successfully", "agent_failed")
                self.finished = True
                self.revoked.set()
                event("turn_completed")
                send({"channel": "shutdown"})
        elif message.get("method") in ("item/agentMessage/delta", "item/commandExecution/outputDelta",
                                        "item/started", "item/completed"):
            self.on_item(message)

    def on_item(self, message):
        params = message.get("params", {})
        if (params.get("threadId") != self.thread_id or self.turn_id is None
                or params.get("turnId") != self.turn_id or self.finished):
            raise BridgeError("mismatched item notification")
        method = message["method"]
        if method.endswith("/delta") or method.endswith("/outputDelta"):
            item_id = params.get("itemId")
            if not isinstance(item_id, str) or not 0 < len(item_id) <= 256:
                raise BridgeError("invalid item id")
            if method == "item/agentMessage/delta":
                self.text_seen.add(item_id)
                text_events("text", params.get("delta"))
            else:
                self.output_seen.add(item_id)
                text_events("tool_output", params.get("delta"), id=item_id)
            return
        item = params.get("item", {})
        item_id, kind = item.get("id"), item.get("type")
        if not isinstance(item_id, str) or not 0 < len(item_id) <= 256:
            raise BridgeError("invalid item id")
        if kind == "agentMessage" and method == "item/completed" and item_id not in self.text_seen:
            text_events("text", item.get("text", ""))
        elif kind in ("commandExecution", "fileChange", "mcpToolCall", "dynamicToolCall", "collabAgentToolCall"):
            if kind == "commandExecution" and method == "item/completed" and item_id not in self.output_seen:
                text_events("tool_output", item.get("aggregatedOutput") or "", id=item_id)
            code = item.get("exitCode")
            if code is not None and (type(code) is not int or not -(2**63) <= code < 2**63):
                raise BridgeError("invalid tool exit code")
            event("tool", id=item_id, tool=kind,
                  phase="started" if method == "item/started" else "completed", exit_code=code)

    def run(self):
        self.rpc(1, "initialize", {"clientInfo": {"name": "circus_agent_example", "version": "0.1.0"}})
        for message in frames(sys.stdin.buffer):
            channel = message.get("channel")
            if channel == "rpc":
                self.on_rpc(message.get("message"))
            elif channel == "http_request":
                self.model_request(message)
            elif channel == "mcp_request" and self.mcp is not None:
                self.mcp.request(message, self.token)
            elif channel == "rpc_closed":
                if not self.finished:
                    raise BridgeError("app-server output closed before turn completion")
            elif channel == "exit":
                if not self.finished or message.get("code") != 0:
                    raise BridgeError("agent bridge exited before successful completion")
                return
            else:
                raise BridgeError("unexpected guest channel")
        raise BridgeError("guest output closed before completion")


def main_host(provider=None):
    controller = None
    try:
        model = required("CIRCUS_MODEL", 256)
        token = required("CIRCUS_TASK_TOKEN", 256)
        # Prompts may contain newlines; never log their content.
        prompt = os.environ.get("CIRCUS_AGENT_PROMPT", "")
        if not prompt or len(prompt.encode()) > 64 * 1024:
            raise BridgeError("invalid prompt size")
        if provider is None:
            provider = ModelEndpoint(required("CIRCUS_MODEL_BASE_URL", 2048),
                                     required("CIRCUS_MODEL_API_KEY", 4096),
                                     os.environ.get("CIRCUS_MODEL_CA_FILE"))
        controller = HostController(provider, model, token, prompt,
                                    os.environ.get("CIRCUS_RESUME_THREAD") or None)
        mcp_config = decode_json(os.environ.get("CIRCUS_MCP_CONFIG", "[]"))
        if mcp_config:
            controller.mcp = McpBroker(mcp_config, controller.revoked)
        controller.run()
    except Exception as error:
        # Upstream/client/library errors may include credentials or response content.
        failure(error)
        raise SystemExit(1) from None
    finally:
        if controller:
            controller.revoked.set()
            if controller.mcp is not None:
                controller.mcp.close()


class GuestServer(ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 4
    def __init__(self, token, handler=None):
        super().__init__(("127.0.0.1", 0), handler or GuestHandler)
        self.token, self.next_id = token, 0
        self.pending = {}
        self.pending_mcp, self.next_mcp_id = {}, 0
        self.slots = threading.BoundedSemaphore(4)
        self.ids = threading.Lock()

    def process_request(self, request, client_address):
        request.settimeout(60)
        if not self.slots.acquire(blocking=False):
            self.shutdown_request(request)
            return
        try:
            super().process_request(request, client_address)
        except Exception:
            self.slots.release()
            raise

    def process_request_thread(self, request, client_address):
        try:
            super().process_request_thread(request, client_address)
        finally:
            self.slots.release()


class GuestHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    routes = ("/v1/responses",)
    auth_header = "Authorization"
    auth_prefix = "Bearer "

    def log_message(self, *_args):
        pass

    def request_metadata(self):
        return {}

    def do_POST(self):
        if self.path == "/mcp":
            return self.mcp_post()
        self.close_connection = True
        self.connection.settimeout(30)
        if (self.path not in self.routes or self.headers.get("Transfer-Encoding")
                or self.headers.get("Content-Encoding")
                or len(self.headers.get_all("Content-Length", [])) != 1
                or not hmac.compare_digest(self.headers.get(self.auth_header, "").encode(),
                                           (self.auth_prefix + self.server.token).encode())):
            self.send_error(403)
            return
        try:
            length = int(self.headers["Content-Length"])
        except ValueError:
            self.send_error(400)
            return
        with self.server.ids:
            if not 0 < length <= MAX_BODY or self.server.next_id >= MAX_REQUESTS:
                self.send_error(413)
                return
            request_id = self.server.next_id = self.server.next_id + 1
        responses = queue.Queue(maxsize=32)
        self.server.pending[request_id] = responses
        started = False
        try:
            body = self.rfile.read(length)
            if len(body) != length:
                raise BridgeError("incomplete model request")
            send({"channel": "http_request", "id": request_id, "method": "POST", "path": self.path,
                  "token": self.server.token, "body": base64.b64encode(body).decode("ascii"),
                  **self.request_metadata()})
            while True:
                message = responses.get(timeout=35)
                kind = message["channel"]
                if kind == "http_start" and not started:
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Connection", "close")
                    self.end_headers()
                    started = True
                elif kind == "http_chunk" and started:
                    self.wfile.write(unpack(message.get("body"), CHUNK))
                    self.wfile.flush()
                elif kind == "http_end" and started:
                    return
                else:
                    raise BridgeError("model stream failed")
        except Exception:
            if not started:
                self.send_error(502, "model channel unavailable")
        finally:
            self.server.pending.pop(request_id, None)


    def mcp_post(self):
        self.close_connection = True
        self.connection.settimeout(60)
        if (os.environ.get("CIRCUS_MCP_ENABLED") != "1"
                or self.headers.get("Transfer-Encoding") or self.headers.get("Content-Encoding")
                or len(self.headers.get_all("Content-Length", [])) != 1
                or not hmac.compare_digest(self.headers.get("Authorization", "").encode(),
                                           ("Bearer " + self.server.token).encode())):
            self.send_error(403)
            return
        try:
            length = int(self.headers["Content-Length"])
            with self.server.ids:
                if not 0 < length <= 1024 * 1024 or self.server.next_mcp_id >= 64:
                    raise BridgeError("MCP request budget exceeded")
                request_id = self.server.next_mcp_id = self.server.next_mcp_id + 1
            body = self.rfile.read(length)
            if len(body) != length:
                raise BridgeError("incomplete MCP request")
            responses = queue.Queue(maxsize=1)
            self.server.pending_mcp[request_id] = responses
            try:
                send({"channel": "mcp_request", "id": request_id, "token": self.server.token,
                      "body": base64.b64encode(body).decode("ascii")})
                result = responses.get(timeout=55)["body"]
            finally:
                self.server.pending_mcp.pop(request_id, None)
            data = b"" if result is None else json.dumps(result, allow_nan=False).encode()
            self.send_response(204 if result is None else 200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except Exception:
            self.send_error(502, "MCP channel unavailable")


def guest_config(port):
    if type(port) is not int or not 1 <= port <= 65535:
        raise BridgeError("invalid loopback port")
    result = "\n".join([
        'model_provider="circus"', 'web_search="disabled"',
        'model_providers.circus={name="Circus task channel",'
        f'base_url="http://127.0.0.1:{port}/v1",'
        'env_key="CIRCUS_TASK_TOKEN",wire_api="responses",requires_openai_auth=false,'
        'supports_websockets=false,request_max_retries=0,stream_max_retries=0}',
    ]) + "\n"
    if os.environ.get("CIRCUS_MCP_ENABLED") == "1":
        result += ('[mcp_servers.circus]\ncommand="python3"\n'
                   f'args=["-I","-B","/workspace/mcp_guest.py","{port}"]\n'
                   'env_vars=["CIRCUS_TASK_TOKEN"]\nrequired=true\nstartup_timeout_sec=60\ntool_timeout_sec=60\n')
    return result


def main_guest():
    token = required("CIRCUS_TASK_TOKEN", 256)
    server = GuestServer(token)
    # The fixed image creates a fresh CODEX_HOME. A restored workspace archive
    # can contain the previous run's config, so replace that regular file in
    # place. O_NOFOLLOW rejects the only ambiguous node the archive cannot carry
    # anyway (a symlink), and mode 0600 keeps the token-free config private.
    path = "/workspace/.circus/home/.codex/config.toml"
    flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(path, flags, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as config:
        config.write(guest_config(server.server_address[1]))
    with subprocess.Popen(["codex", "app-server", "--listen", "stdio://"],
                          shell=False, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                          stderr=subprocess.DEVNULL, cwd="/workspace/project") as process:
        def output():
            try:
                for message in frames(process.stdout):
                    send({"channel": "rpc", "message": message})
                send({"channel": "rpc_closed"})
            except Exception:
                send({"channel": "error"})

        reader = threading.Thread(target=output, daemon=True)
        web = threading.Thread(target=server.serve_forever, daemon=True)
        reader.start()
        web.start()
        try:
            for message in frames(sys.stdin.buffer):
                channel = message.get("channel")
                if channel == "rpc":
                    wire = json.dumps(message["message"], allow_nan=False).encode() + b"\n"
                    process.stdin.write(wire)
                    process.stdin.flush()
                elif channel in ("http_start", "http_chunk", "http_end", "http_error"):
                    pending = server.pending.get(message.get("id"))
                    if pending is None:
                        raise BridgeError("unknown model response id")
                    pending.put(message, timeout=5)
                elif channel == "mcp_response":
                    pending = server.pending_mcp.get(message.get("id"))
                    if pending is None:
                        raise BridgeError("unknown MCP response id")
                    pending.put(message, timeout=5)
                elif channel == "shutdown":
                    break
                else:
                    raise BridgeError("unexpected host channel")
        finally:
            server.shutdown()
            server.server_close()
            process.stdin.close()
            try:
                code = process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                code = process.wait(timeout=5)
            reader.join(timeout=5)
        send({"channel": "exit", "code": code})

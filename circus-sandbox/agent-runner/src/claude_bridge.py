"""Claude Code adapter, appended to the shared bounded host/guest transport.

The fixed CLI speaks Anthropic Messages, not OpenAI Responses. Only the host
injects the real API key; the guest receives a revocable task-local credential.
"""


class ClaudeModelEndpoint(ModelEndpoint):
    def __init__(self, base_url, key, ca_file=None):
        super().__init__(base_url, key, ca_file)
        self.path = urlsplit(base_url).path.rstrip("/") + "/messages?beta=true"
        self.metadata = {}

    def request_headers(self):
        return {"X-Api-Key": self.key, "Content-Type": "application/json",
                "Accept": "text/event-stream", **self.metadata}


class ClaudeHostController(HostController):
    def __init__(self, provider, model, token, prompt):
        super().__init__(provider, model, token, prompt)
        self.session_id = None
        self.tools = {}
        self.streamed_text = False
        self.mcp = None

    def validate_request(self, message):
        request_id = message.get("id")
        if (type(request_id) is not int or request_id <= 0 or request_id in self.seen
                or len(self.seen) >= MAX_REQUESTS):
            raise BridgeError("invalid model request id or request budget exhausted", "budget")
        if message.get("method") != "POST" or message.get("path") not in ClaudeGuestHandler.routes:
            raise BridgeError("model route is not allowed")
        token = message.get("token")
        if not isinstance(token, str) or not hmac.compare_digest(token.encode(), self.token.encode()):
            raise BridgeError("invalid task credential")
        body = decode_json(unpack(message.get("body"), MAX_BODY))
        if (not isinstance(body, dict) or body.get("model") != self.model
                or body.get("stream") is not True):
            raise BridgeError("model request violates task policy")
        metadata = message.get("headers", {})
        if not isinstance(metadata, dict) or set(metadata) - {"anthropic-version", "anthropic-beta"}:
            raise BridgeError("model headers are not allowed")
        if metadata.get("anthropic-version") != "2023-06-01":
            raise BridgeError("unsupported Anthropic API version")
        for value in metadata.values():
            if (not isinstance(value, str) or not 0 < len(value) <= 2048
                    or any(ord(c) < 32 or ord(c) > 126 for c in value)):
                raise BridgeError("invalid model header")
        # No host, URL, authorization, cookie, proxy or arbitrary headers cross
        # this boundary. The version/beta headers carry Messages protocol data.
        encoded = json.dumps(body, separators=(",", ":"), allow_nan=False).encode()
        if len(encoded) > MAX_BODY:
            raise BridgeError("model body exceeded its budget", "budget")
        self.provider.metadata = metadata
        self.seen.add(request_id)
        return request_id, encoded

    def on_stream(self, message):
        if not isinstance(message, dict) or self.finished:
            raise BridgeError("unexpected Claude Code event")
        kind = message.get("type")
        session = message.get("session_id")
        if session is not None:
            if not isinstance(session, str) or not 0 < len(session) <= 256:
                raise BridgeError("invalid Claude Code session")
            if self.session_id is not None and session != self.session_id:
                raise BridgeError("mismatched Claude Code session")
            self.session_id = session
        if kind == "system" and message.get("subtype") == "init":
            if message.get("model") != self.model or message.get("mcp_server_errors"):
                raise BridgeError("Claude Code configuration was not accepted")
            if self.mcp is not None and not any(
                    server.get("name") == "circus" and server.get("status") == "connected"
                    for server in message.get("mcp_servers", [])):
                raise BridgeError("Claude Code did not connect its authorized MCP bridge")
            event("running")
        elif kind == "system" and message.get("subtype") == "permission_denied":
            raise BridgeError("Claude Code requires an unsupported approval", "unsupported_interaction")
        elif kind == "stream_event":
            payload = message.get("event", {})
            if payload.get("type") == "message_start":
                self.streamed_text = False
            elif payload.get("type") == "content_block_delta":
                delta = payload.get("delta", {})
                if delta.get("type") == "text_delta":
                    self.streamed_text = True
                    text_events("text", delta.get("text"))
        elif kind == "assistant":
            for block in message.get("message", {}).get("content", []):
                if block.get("type") == "text" and not self.streamed_text:
                    text_events("text", block.get("text"))
                elif block.get("type") == "tool_use":
                    tool_id, name = block.get("id"), block.get("name")
                    if (not isinstance(tool_id, str) or not 0 < len(tool_id) <= 256
                            or not isinstance(name, str) or not 0 < len(name) <= 256
                            or tool_id in self.tools or len(self.tools) >= 256):
                        raise BridgeError("invalid Claude Code tool event")
                    self.tools[tool_id] = name
                    event("tool", id=tool_id, tool=name, phase="started", exit_code=None)
        elif kind == "user":
            for block in message.get("message", {}).get("content", []):
                if block.get("type") != "tool_result":
                    continue
                tool_id = block.get("tool_use_id")
                name = self.tools.pop(tool_id, None)
                if name is None:
                    raise BridgeError("unexpected Claude Code tool result")
                content = block.get("content", "")
                if isinstance(content, str):
                    text_events("tool_output", content, id=tool_id)
                elif isinstance(content, list):
                    for part in content:
                        if part.get("type") == "text":
                            text_events("tool_output", part.get("text"), id=tool_id)
                # Tool result success is not necessarily a process exit code.
                event("tool", id=tool_id, tool=name, phase="completed", exit_code=None)
        elif kind == "result":
            if message.get("permission_denials"):
                raise BridgeError("Claude Code requires an unsupported approval", "unsupported_interaction")
            if message.get("subtype") != "success" or message.get("is_error") is not False or self.tools:
                raise BridgeError("Claude Code task failed", "agent_failed")
            self.finished = True
            self.revoked.set()
            event("turn_completed")
            send({"channel": "shutdown"})

    def run(self):
        send({"channel": "start", "prompt": self.prompt})
        for message in frames(sys.stdin.buffer):
            channel = message.get("channel")
            if channel == "claude_event":
                self.on_stream(message.get("message"))
            elif channel == "http_request":
                self.model_request(message)
            elif channel == "mcp_request" and self.mcp is not None:
                self.mcp.request(message, self.token)
            elif channel == "rpc_closed":
                if not self.finished:
                    raise BridgeError("Claude Code output closed before completion")
            elif channel == "exit":
                if not self.finished or message.get("code") != 0:
                    raise BridgeError("Claude Code exited before successful completion")
                return
            else:
                raise BridgeError("unexpected guest channel")
        raise BridgeError("guest output closed before completion")


def main_claude_host(provider=None):
    controller = None
    try:
        if os.environ.get("CIRCUS_RESUME_THREAD"):
            # Claude runs with --no-session-persistence; a requested resume must
            # fail closed instead of silently starting an unrelated turn.
            raise BridgeError("Claude Code session resume is unsupported")
        model = required("CIRCUS_MODEL", 256)
        token = required("CIRCUS_TASK_TOKEN", 256)
        prompt = os.environ.get("CIRCUS_AGENT_PROMPT", "")
        if not prompt or len(prompt.encode()) > 64 * 1024:
            raise BridgeError("invalid prompt size")
        if provider is None:
            provider = ClaudeModelEndpoint(required("CIRCUS_MODEL_BASE_URL", 2048),
                                           required("CIRCUS_MODEL_API_KEY", 4096),
                                           os.environ.get("CIRCUS_MODEL_CA_FILE"))
        controller = ClaudeHostController(provider, model, token, prompt)
        connections = decode_json(os.environ.get("CIRCUS_MCP_CONFIG", "[]"))
        if connections:
            controller.mcp = McpBroker(connections, controller.revoked)
        controller.run()
    except Exception as error:
        failure(error)
        raise SystemExit(1) from None
    finally:
        if controller:
            controller.revoked.set()
            if controller.mcp is not None:
                controller.mcp.close()


class ClaudeGuestHandler(GuestHandler):
    routes = ("/v1/messages", "/v1/messages?beta=true")
    auth_header = "X-Api-Key"
    auth_prefix = ""

    def request_metadata(self):
        return {"headers": {name: self.headers[name] for name in
                            ("anthropic-version", "anthropic-beta") if name in self.headers}}


def claude_command(port=None, token=None):
    # Fixed tools run only inside the enforced Docker boundary. Bare mode avoids
    # project/user settings, hooks, plugins, login files and implicit MCP servers.
    command = ["claude", "--bare", "-p", "--output-format", "stream-json", "--verbose",
            "--include-partial-messages", "--no-session-persistence", "--strict-mcp-config",
            "--model", required("CIRCUS_MODEL", 256), "--permission-mode", "dontAsk",
            "--tools", "Bash,Read,Write,Edit,Glob,Grep", "--allowedTools", "Bash,Read,Write,Edit,Glob,Grep"]
    if os.environ.get("CIRCUS_MCP_ENABLED") == "1":
        if type(port) is not int or not 1 <= port <= 65535 or not token:
            raise BridgeError("missing MCP bridge configuration")
        command[-1] += ",mcp__circus__*"
        config = {"mcpServers": {"circus": {
            "command": "python3", "args": ["-I", "-B", "/workspace/mcp_guest.py", str(port)],
            "env": {"CIRCUS_TASK_TOKEN": token}}}}
        command.extend(["--mcp-config", json.dumps(config, separators=(",", ":"))])
    return command


def main_claude_guest():
    token = required("CIRCUS_TASK_TOKEN", 256)
    server = GuestServer(token, ClaudeGuestHandler)
    incoming = frames(sys.stdin.buffer)
    start = next(incoming, None)
    if not isinstance(start, dict) or start.get("channel") != "start":
        raise BridgeError("missing task start")
    prompt = start.get("prompt")
    if not isinstance(prompt, str) or not 0 < len(prompt.encode()) <= 64 * 1024:
        raise BridgeError("invalid prompt")
    environment = os.environ.copy()
    environment.update({
        "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
        "ANTHROPIC_API_KEY": token,
        "ANTHROPIC_DEFAULT_HAIKU_MODEL": required("CIRCUS_MODEL", 256),
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK": "1",
        "CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL": "1",
        "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST": "1",
        "DISABLE_AUTOUPDATER": "1",
    })
    with subprocess.Popen(claude_command(server.server_port, token), shell=False, env=environment,
                          stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                          stderr=subprocess.DEVNULL, cwd="/workspace/project") as process:
        def output():
            try:
                for message in frames(process.stdout):
                    send({"channel": "claude_event", "message": message})
                send({"channel": "rpc_closed"})
            except Exception:
                send({"channel": "error"})

        reader = threading.Thread(target=output, daemon=True)
        web = threading.Thread(target=server.serve_forever, daemon=True)
        reader.start()
        web.start()
        process.stdin.write(prompt.encode())
        process.stdin.close()
        try:
            for message in incoming:
                channel = message.get("channel")
                if channel in ("http_start", "http_chunk", "http_end", "http_error"):
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
            try:
                code = process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                code = process.wait(timeout=5)
            reader.join(timeout=5)
        send({"channel": "exit", "code": code})

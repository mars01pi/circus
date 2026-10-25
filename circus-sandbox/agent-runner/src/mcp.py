"""Host-only, task-scoped MCP proxy. Concatenated with codex_bridge.py.

Only HTTPS Streamable HTTP tools are supported. Trusted operators choose the
endpoint, headers and argument policy. Neither guest RPC nor server metadata can
choose a new destination, command, credential or privilege grant.
"""

import time

MCP_PROTOCOL = "2025-03-26"
MCP_BYTES = 1024 * 1024
MCP_REQUESTS = 64
MCP_TOTAL = 4 * 1024 * 1024
MCP_TIMEOUT = 15


def mcp_object(value, allowed, required=()):
    if not isinstance(value, dict) or set(value) - set(allowed) or not set(required) <= set(value):
        raise BridgeError("invalid MCP object")
    return value


class McpEndpoint:
    def __init__(self, config, revoked):
        url = urlsplit(config["url"])
        # Rust validates host configuration before allocation. Validate again at
        # this private process boundary; never accept a URL from guest messages.
        if (url.scheme != "https" or not url.hostname or url.username or url.password
                or url.query or url.fragment):
            raise BridgeError("invalid MCP endpoint")
        self.host, self.port, self.path = url.hostname, url.port or 443, url.path or "/"
        self.headers = config["headers"]
        self.context = ssl.create_default_context(cafile=config.get("caFile"))
        self.revoked, self.session = revoked, None
        self.active, self.next_id = None, 0
        self.ready = False

    def close(self):
        if self.active:
            self.active.close()

    def post(self, method, params, notification=False):
        if self.revoked.is_set():
            raise BridgeError("MCP task permission revoked")
        self.next_id += 1
        if self.next_id > 96:
            raise BridgeError("MCP upstream request budget exceeded")
        rpc = {"jsonrpc": "2.0", "method": method, "params": params}
        if not notification:
            rpc["id"] = self.next_id
        body = json.dumps(rpc, separators=(",", ":"), allow_nan=False).encode()
        if len(body) > MCP_BYTES:
            raise BridgeError("MCP request budget exceeded")
        headers = {**self.headers, "Content-Type": "application/json",
                   "Accept": "application/json, text/event-stream"}
        if self.ready:
            headers["MCP-Protocol-Version"] = MCP_PROTOCOL
        if self.session:
            headers["MCP-Session-Id"] = self.session
        connection = http.client.HTTPSConnection(self.host, self.port,
                                                timeout=MCP_TIMEOUT, context=self.context)
        self.active = connection
        deadline = time.monotonic() + MCP_TIMEOUT
        try:
            connection.request("POST", self.path, body=body, headers=headers)
            socket = connection.sock
            response = connection.getresponse()
            if notification:
                if response.status not in (200, 202, 204):
                    raise BridgeError("MCP notification rejected")
                return None
            if response.status != 200 or response.getheader("Content-Encoding"):
                raise BridgeError("MCP service rejected request")
            session = response.getheader("MCP-Session-Id")
            if session is not None:
                if ((self.session is not None and session != self.session) or len(session) > 256
                        or not session or not all(33 <= ord(c) <= 126 for c in session)):
                    raise BridgeError("invalid MCP session")
                self.session = session
            content_type = response.getheader("Content-Type", "").split(";", 1)[0].strip().lower()
            if content_type not in ("application/json", "text/event-stream"):
                raise BridgeError("unsupported MCP content type")
            data = bytearray()
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or self.revoked.is_set():
                    raise BridgeError("MCP request timed out or revoked")
                socket.settimeout(remaining)
                chunk = response.read1(min(8192, MCP_BYTES + 1 - len(data)))
                if not chunk:
                    break
                data.extend(chunk)
                if len(data) > MCP_BYTES:
                    raise BridgeError("MCP response budget exceeded")
                if content_type == "text/event-stream":
                    normalized = bytes(data).replace(b"\r\n", b"\n")
                    while b"\n\n" in normalized:
                        frame, normalized = normalized.split(b"\n\n", 1)
                        payload = b"\n".join(line[5:].lstrip(b" ") for line in frame.splitlines() if line.startswith(b"data:"))
                        if payload:
                            return self.result(decode_json(payload), rpc["id"])
                    # The full input remains bounded even for comment-only streams.
            if content_type == "text/event-stream":
                raise BridgeError("incomplete MCP SSE response")
            return self.result(decode_json(data), rpc["id"])
        finally:
            self.active = None
            connection.close()

    @staticmethod
    def result(message, request_id):
        mcp_object(message, ("jsonrpc", "id", "result", "error"), ("jsonrpc", "id"))
        if message["jsonrpc"] != "2.0" or type(message["id"]) is not int or message["id"] != request_id:
            raise BridgeError("MCP response correlation failed")
        if "error" in message or "result" not in message or not isinstance(message["result"], dict):
            # Provider errors can contain passwords and are never forwarded.
            raise BridgeError("MCP provider returned an error")
        return message["result"]

    def initialize(self):
        if self.ready:
            return
        result = self.post("initialize", {"protocolVersion": MCP_PROTOCOL, "capabilities": {},
                                          "clientInfo": {"name": "circus-host", "version": "0.1.0"}})
        if result.get("protocolVersion") != MCP_PROTOCOL:
            raise BridgeError("unsupported MCP protocol version")
        capabilities = result.get("capabilities", {})
        if not isinstance(capabilities, dict) or "tools" not in capabilities:
            raise BridgeError("MCP service does not expose tools")
        self.ready = True
        self.post("notifications/initialized", {}, notification=True)


class McpBroker:
    def __init__(self, configs, revoked):
        self.revoked, self.worker = revoked, None
        self.configs, self.endpoints, self.tools = configs, {}, {}
        self.seen, self.total, self.initialized = set(), 0, False
        self.secrets = [value for config in configs for value in config["headers"].values()]
        self.secrets += [value.split(" ", 1)[1] for value in self.secrets if " " in value]

    def redact(self, value):
        if isinstance(value, str):
            for secret in self.secrets:
                if secret:
                    value = value.replace(secret, "[REDACTED]")
            return value
        if isinstance(value, list):
            return [self.redact(item) for item in value]
        if isinstance(value, dict):
            return {self.redact(key): self.redact(item) for key, item in value.items()}
        return value

    def close(self):
        self.revoked.set()
        for endpoint in list(self.endpoints.values()):
            endpoint.close()

    def discover(self):
        if self.tools:
            return
        discovered = {}
        for index, config in enumerate(self.configs):
            if self.revoked.is_set():
                raise BridgeError("MCP permission revoked")
            if config["id"] not in self.endpoints:
                self.endpoints[config["id"]] = McpEndpoint(config, self.revoked)
            endpoint = self.endpoints[config["id"]]
            endpoint.initialize()
            tools, cursor, cursors = [], None, set()
            for page in range(4):
                listing = endpoint.post("tools/list", {"cursor": cursor} if cursor is not None else {})
                batch = listing.get("tools")
                if not isinstance(batch, list) or len(tools) + len(batch) > 512:
                    raise BridgeError("MCP tool catalog exceeds supported bounds")
                tools.extend(batch)
                cursor = listing.get("nextCursor")
                if cursor is None:
                    break
                if not isinstance(cursor, str) or not 0 < len(cursor) <= 1024 or cursor in cursors or page == 3:
                    raise BridgeError("MCP tool pagination exceeds supported bounds")
                cursors.add(cursor)
            names = set()
            for tool in tools:
                if not isinstance(tool, dict) or not isinstance(tool.get("name"), str) or tool["name"] in names:
                    raise BridgeError("invalid MCP tool catalog")
                names.add(tool["name"])
            for policy in config["tools"]:
                matches = [tool for tool in tools if tool["name"] == policy["name"]]
                if len(matches) != 1:
                    raise BridgeError("approved MCP tool not available")
                tool = matches[0]
                schema = tool.get("inputSchema", {})
                properties = schema.get("properties", {})
                required = schema.get("required", [])
                allowed, fixed = set(policy["allowedArguments"]), policy["fixedArguments"]
                if (schema.get("type") != "object" or not isinstance(properties, dict)
                        or not isinstance(required, list) or not all(isinstance(key, str) for key in required)
                        or (set(required) - allowed - set(fixed))
                        or (allowed | set(fixed)) - set(properties)):
                    raise BridgeError("MCP tool schema conflicts with host argument policy")
                name = f"c{index}__{policy['name']}"
                exposed = {"name": name, "description": f"{config['id']}: {policy['name']}. Host-authorized resources only.",
                           "inputSchema": {"type": "object", "properties": {key: properties[key] for key in sorted(allowed)},
                                           "required": [key for key in required if key in allowed], "additionalProperties": False}}
                discovered[name] = (endpoint, policy, exposed)
        self.tools = discovered

    def handle(self, rpc):
        mcp_object(rpc, ("jsonrpc", "id", "method", "params"), ("jsonrpc", "method"))
        if rpc["jsonrpc"] != "2.0" or not isinstance(rpc["method"], str):
            raise BridgeError("invalid MCP request")
        method, params = rpc["method"], rpc.get("params", {})
        request_id = rpc.get("id")
        if method == "notifications/initialized":
            if "id" in rpc or params != {} or not self.initialized:
                raise BridgeError("invalid MCP initialized notification")
            return None
        if not (type(request_id) is int and request_id >= 0 or
                isinstance(request_id, str) and 0 < len(request_id) <= 64):
            raise BridgeError("invalid MCP RPC id")
        try:
            if isinstance(params, dict) and "_meta" in params:
                params = dict(params)
                metadata = mcp_object(params.pop("_meta"), (
                    "progressToken", "callId", "threadId", "sessionId", "windowId", "itemId",
                    "x-codex-turn-metadata", "claudecode/toolUseId"))
                if len(json.dumps(metadata, allow_nan=False).encode()) > 8192:
                    raise BridgeError("MCP metadata budget exceeded")
                for key in ("callId", "threadId", "sessionId", "windowId", "itemId", "claudecode/toolUseId"):
                    if key in metadata and (not isinstance(metadata[key], str) or len(metadata[key]) > 256):
                        raise BridgeError("invalid MCP correlation metadata")
                if "x-codex-turn-metadata" in metadata:
                    mcp_object(metadata["x-codex-turn-metadata"], (
                        "session_id", "thread_id", "model", "reasoning_effort", "turn_id", "turn_started_at_unix_ms",
                        "sandbox", "sandbox_mode", "auto_review_enabled", "node_repl_auto_review_required",
                        "node_repl_disabled", "codex_version"))
                progress = metadata.get("progressToken")
                if progress is not None and not (type(progress) is int or
                        isinstance(progress, str) and 0 < len(progress) <= 64):
                    raise BridgeError("invalid MCP progress metadata")
                # MCP transport progress metadata grants no authority and is not
                # forwarded. The host issues its own upstream request IDs.
            if self.revoked.is_set():
                raise BridgeError("MCP task permission revoked")
            if method == "initialize":
                mcp_object(params, ("protocolVersion", "capabilities", "clientInfo"), ("protocolVersion", "capabilities", "clientInfo"))
                if self.initialized or not isinstance(params["protocolVersion"], str):
                    raise BridgeError("invalid MCP initialization")
                self.initialized = True
                result = {"protocolVersion": MCP_PROTOCOL, "capabilities": {"tools": {}},
                          "serverInfo": {"name": "circus-task-tools", "version": "0.1.0"}}
            elif not self.initialized:
                raise BridgeError("MCP session is not initialized")
            elif method == "ping":
                mcp_object(params, ())
                result = {}
            elif method == "tools/list":
                mcp_object(params, ())
                self.discover()
                result = {"tools": [entry[2] for entry in self.tools.values()]}
            elif method == "tools/call":
                mcp_object(params, ("name", "arguments"), ("name",))
                if not isinstance(params["name"], str):
                    raise BridgeError("invalid MCP tool name")
                self.discover()
                if params["name"] not in self.tools:
                    raise BridgeError("MCP tool is not authorized")
                endpoint, policy, _ = self.tools[params["name"]]
                arguments = mcp_object(params.get("arguments", {}), policy["allowedArguments"])
                result = endpoint.post("tools/call", {"name": policy["name"],
                                       "arguments": {**arguments, **policy["fixedArguments"]}})
                # Provider tool failures may embed authentication diagnostics.
                if result.get("isError"):
                    result = {"content": [{"type": "text", "text": "MCP tool failed; details withheld by host."}], "isError": True}
            else:
                raise BridgeError("MCP method is not allowed")
            response = {"jsonrpc": "2.0", "id": request_id, "result": self.redact(result)}
        except Exception:
            response = {"jsonrpc": "2.0", "id": request_id,
                        "error": {"code": -32000, "message": "MCP request denied or service unavailable."}}
        wire = json.dumps(response, separators=(",", ":"), allow_nan=False).encode()
        self.total += len(wire)
        if len(wire) > MCP_BYTES or self.total > MCP_TOTAL:
            raise BridgeError("MCP task output budget exceeded")
        return response

    def request(self, message, token):
        mcp_object(message, ("channel", "id", "token", "body"), ("channel", "id", "token", "body"))
        request_id = message["id"]
        if (type(request_id) is not int or request_id <= 0 or request_id in self.seen
                or len(self.seen) >= MCP_REQUESTS or not isinstance(message["token"], str)
                or not hmac.compare_digest(message["token"].encode(), token.encode()) or self.revoked.is_set()):
            raise BridgeError("invalid MCP task authority or request budget")
        if self.worker and self.worker.is_alive():
            self.worker.join(timeout=0.1)
        if self.worker and self.worker.is_alive():
            raise BridgeError("MCP task channel busy")
        self.seen.add(request_id)
        rpc = decode_json(unpack(message["body"], MCP_BYTES))

        def work():
            try:
                result = self.handle(rpc)
                if not self.revoked.is_set():
                    send({"channel": "mcp_response", "id": request_id, "body": result})
            except Exception:
                if not self.revoked.is_set():
                    rpc_id = rpc.get("id") if isinstance(rpc, dict) else None
                    if not (type(rpc_id) is int or isinstance(rpc_id, str) and 0 < len(rpc_id) <= 64):
                        rpc_id = None
                    send({"channel": "mcp_response", "id": request_id, "body": {"jsonrpc": "2.0", "id": rpc_id,
                          "error": {"code": -32000, "message": "MCP channel rejected the request."}}})
        self.worker = threading.Thread(target=work, daemon=True)
        self.worker.start()

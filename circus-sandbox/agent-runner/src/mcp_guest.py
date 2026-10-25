"""Credential-free MCP stdio adapter inside the task sandbox.

Its only destination is the task's loopback relay. Upstream addresses, credentials,
resource grants, and tool policies remain in the host controller.
"""

import http.client
import json
import os
import sys

LIMIT = 1024 * 1024


def main():
    port = int(sys.argv[1])
    if not 1 <= port <= 65535:
        raise ValueError("invalid loopback port")
    token = os.environ["CIRCUS_TASK_TOKEN"]
    while line := sys.stdin.buffer.readline(LIMIT + 1):
        if len(line) > LIMIT or not line.endswith(b"\n"):
            raise ValueError("MCP frame budget exceeded")
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=60)
        try:
            connection.request("POST", "/mcp", body=line, headers={
                "Authorization": "Bearer " + token, "Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read(LIMIT + 1)
            if len(data) > LIMIT or response.status not in (200, 204):
                raise ValueError("MCP channel unavailable")
            if response.status == 200:
                value = json.loads(data)
                sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
                sys.stdout.flush()
        finally:
            connection.close()


if __name__ == "__main__":
    try:
        main()
    except Exception:
        # Never echo HTTP bodies, tokens or arbitrary server diagnostics.
        sys.stderr.write("Circus MCP channel unavailable.\n")
        sys.exit(1)

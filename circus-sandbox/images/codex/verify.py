"""Offline image acceptance: real tools, writable runtime dirs, and Codex stdio."""

import json
import os
from pathlib import Path
import selectors
import subprocess
import tempfile
import time


def run(*argv):
    return subprocess.check_output(argv, text=True, timeout=10).strip()


def response(selector, state, request_id, deadline):
    while True:
        while b"\n" in state["stdout"]:
            line, state["stdout"] = state["stdout"].split(b"\n", 1)
            message = json.loads(line)
            if message.get("id") == request_id:
                if "error" in message:
                    raise RuntimeError(f"Codex rejected request {request_id}: {message['error']}")
                return message["result"]
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("Codex did not finish its offline handshake")
        events = selector.select(remaining)
        for key, _ in events:
            chunk = os.read(key.fileobj.fileno(), 4096)
            if not chunk:
                selector.unregister(key.fileobj)
                if key.data == "stdout":
                    raise RuntimeError(f"Codex output closed: {state['stderr'].decode(errors='replace')}")
                continue
            state["total"] += len(chunk)
            if state["total"] > 128 * 1024:
                raise RuntimeError("Codex handshake exceeded its output budget")
            state[key.data] += chunk


def check_app_server():
    with subprocess.Popen(
        ["codex", "app-server", "--listen", "stdio://"],
        cwd="/workspace/project",
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    ) as process, selectors.DefaultSelector() as selector:
        selector.register(process.stdout, selectors.EVENT_READ, "stdout")
        selector.register(process.stderr, selectors.EVENT_READ, "stderr")
        state = {"stdout": b"", "stderr": b"", "total": 0}
        deadline = time.monotonic() + 20

        def send(message):
            process.stdin.write(json.dumps(message).encode() + b"\n")
            process.stdin.flush()

        try:
            send({
                "id": 1,
                "method": "initialize",
                "params": {"clientInfo": {"name": "circus_image_check", "version": "0.1.0"}},
            })
            initialized = response(selector, state, 1, deadline)
            if not initialized.get("userAgent"):
                raise RuntimeError("Codex initialize returned no userAgent")
            send({"method": "initialized", "params": {}})
            send({"id": 2, "method": "account/read", "params": {"refreshToken": False}})
            account = response(selector, state, 2, deadline)
            if "account" not in account or account["account"] is not None:
                raise RuntimeError("Image check requires a fresh, unauthenticated home")
        finally:
            # This is a startup check, not an agent turn. Reap the child on every path.
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)


def main():
    if os.getuid() == 0:
        raise RuntimeError("Agent image must run as a non-root user")
    for name in [
        "HOME", "CODEX_HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME",
        "XDG_STATE_HOME", "TMPDIR", "NPM_CONFIG_CACHE",
    ]:
        directory = Path(os.environ[name]).resolve()
        if not directory.is_relative_to("/workspace"):
            raise RuntimeError(f"{name} must be inside the bounded workspace")
        if directory.stat().st_mode & 0o077:
            raise RuntimeError(f"{name} must be private to the sandbox user")
        with tempfile.TemporaryFile(dir=directory) as probe:
            probe.write(b"writable")
    if (Path(os.environ["CODEX_HOME"]) / "auth.json").exists():
        raise RuntimeError("Fresh image unexpectedly contains Codex credentials")

    expected = json.loads(Path("/opt/circus/agent/package.json").read_text())["dependencies"]["@openai/codex"]
    codex = run("codex", "--version")
    if codex != f"codex-cli {expected}":
        raise RuntimeError(f"Unexpected Codex binary version: {codex}")
    node = run("node", "--version")
    if node != "v24.18.0":
        raise RuntimeError(f"Unexpected Node version: {node}")
    project = Path("/workspace/project")
    if (project / "image-check.txt").exists():
        raise RuntimeError("Image check requires a fresh workspace")
    run("git", "-C", str(project), "init", "--quiet")
    (project / "image-check.txt").write_text("circus image tools ready\n")
    if run("rg", "--no-config", "--fixed-strings", "circus image tools ready", str(project / "image-check.txt")) != "circus image tools ready":
        raise RuntimeError("ripgrep could not read the project file")
    check_app_server()
    print(json.dumps({
        "codex": codex, "node": node, "python": run("python3", "--version"),
        "git": run("git", "--version"), "uid": os.getuid(), "app_server": "ready",
    }, sort_keys=True))


if __name__ == "__main__":
    main()

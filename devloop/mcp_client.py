"""Minimal MCP stdio client for driving mcp_server.exe from scripts.

Standard library only (Python 3.10+). See docs/notes/devloop.md.

    py devloop/mcp_client.py tools                      # list tool names
    py devloop/mcp_client.py call create_part
    py devloop/mcp_client.py call extrude_sketch '{"depth_mm": 10}'
    py devloop/mcp_client.py run devloop/scenarios/smoke.py [more.py ...]
    py devloop/mcp_client.py run --no-reset ...         # keep the open documents

`run` first makes sure SolidWorks is up with the add-in listening and closes
every open document, discarding changes (devloop/sw.ps1 start + reset), so
each run starts from the same state.

Each run gets its own folder under devloop/out/ with transcript.jsonl (every
request and result in full), server.log (mcp_server's stderr, RUST_LOG=debug
unless set) and the images tools returned, as PNG files.

A scenario is a Python file run with `mcp` (a Client) and `ToolError` in
scope:

    part = mcp.call("create_part")
    mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
    faces = mcp.call("inspect_model", what="faces").json
    mcp.call("extrude_sketch", depth_mm=-1, expect_error=True)

One server process lives for the whole run, so ids from inspect_model stay
valid between calls, as they do for a real client. A tool error raises
ToolError unless expect_error=True; any exception fails the run.
"""

from __future__ import annotations

import base64
import json
import os
import queue
import subprocess
import sys
import threading
import time
import traceback
from datetime import datetime
from pathlib import Path

DEVLOOP = Path(__file__).resolve().parent
ROOT = DEVLOOP.parent
DEFAULT_SERVER = ROOT / "mcp_server" / "target" / "debug" / "mcp_server.exe"
OUT = DEVLOOP / "out"

# The server's own gRPC timeout is 60 s; leave room for it to report that.
CALL_TIMEOUT_S = 90
# Printed per result; the transcript always has everything.
PRINT_LIMIT = 4000


class ToolError(Exception):
    pass


class Result:
    def __init__(self, tool: str, raw: dict, images: list[Path]):
        self.tool = tool
        self.raw = raw
        self.is_error = bool(raw.get("isError"))
        self.text = "\n".join(c.get("text", "") for c in raw.get("content", []) if c.get("type") == "text")
        self.images = images

    @property
    def json(self):
        """The text content parsed as JSON (most inspect tools return JSON)."""
        return json.loads(self.text)

    def __repr__(self):
        return f"Result({self.tool}, error={self.is_error}, text={self.text[:200]!r})"


class Client:
    def __init__(self, run_dir: Path, server: Path = DEFAULT_SERVER):
        if not server.exists():
            raise FileNotFoundError(f"{server} not found -- build it first (build.ps1 -Target server)")
        self.run_dir = run_dir
        run_dir.mkdir(parents=True, exist_ok=True)
        self._transcript = open(run_dir / "transcript.jsonl", "a", encoding="utf-8")
        self._stderr = open(run_dir / "server.log", "ab")
        env = dict(os.environ)
        env.setdefault("RUST_LOG", "debug")
        self._proc = subprocess.Popen(
            [str(server)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self._stderr, env=env
        )
        self._next_id = 1
        self._step = 0
        self._responses: "queue.Queue[dict | None]" = queue.Queue()
        threading.Thread(target=self._read, daemon=True).start()

        init = self._request("initialize", {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "sld_mcp-devloop", "version": "0"},
        })
        self.server_info = init.get("serverInfo", {})
        self._notify("notifications/initialized")

    # ---- JSON-RPC plumbing

    def _read(self):
        for line in self._proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                self._log({"unparsed_stdout": line.decode("utf-8", "replace")})
                continue
            if "id" in message and ("result" in message or "error" in message):
                self._responses.put(message)
            else:
                self._log({"server_message": message})
        self._responses.put(None)  # stdout closed: the server exited

    def _send(self, message: dict):
        self._proc.stdin.write((json.dumps(message) + "\n").encode("utf-8"))
        self._proc.stdin.flush()

    def _notify(self, method: str, params: dict | None = None):
        self._send({"jsonrpc": "2.0", "method": method, **({"params": params} if params else {})})

    def _request(self, method: str, params: dict, timeout: float = CALL_TIMEOUT_S) -> dict:
        request_id = self._next_id
        self._next_id += 1
        self._send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"{method}: no response within {timeout} s (SolidWorks blocked by a dialog? "
                                   "run: devloop\\sw.ps1 screenshot)")
            try:
                message = self._responses.get(timeout=remaining)
            except queue.Empty:
                continue
            if message is None:
                raise RuntimeError(f"mcp_server exited (code {self._proc.poll()}); see {self.run_dir / 'server.log'}")
            if message.get("id") != request_id:
                continue  # a late answer to a request that timed out
            if "error" in message:
                raise ToolError(f"{method}: JSON-RPC error {message['error']}")
            return message["result"]

    def _log(self, entry: dict):
        entry = {"time": datetime.now().isoformat(timespec="milliseconds"), **entry}
        self._transcript.write(json.dumps(entry) + "\n")
        self._transcript.flush()

    # ---- API

    def tools(self) -> list[dict]:
        return self._request("tools/list", {}).get("tools", [])

    def call(self, tool: str, expect_error: bool = False, **arguments) -> Result:
        self._step += 1
        step = self._step
        print(f"\n--- [{step}] {tool} {json.dumps(arguments) if arguments else ''}", flush=True)
        started = time.monotonic()
        try:
            raw = self._request("tools/call", {"name": tool, "arguments": arguments})
        except ToolError as error:
            # rmcp reports invalid_params and friends as JSON-RPC errors.
            raw = {"isError": True, "content": [{"type": "text", "text": str(error)}]}
        elapsed = time.monotonic() - started

        # Images go to files; the transcript keeps their path instead of the base64.
        images = []
        logged_content = []
        for index, content in enumerate(raw.get("content", [])):
            if content.get("type") == "image":
                ext = {"image/png": "png", "image/jpeg": "jpg"}.get(content.get("mimeType"), "bin")
                path = self.run_dir / f"{step:02d}-{tool}-{index}.{ext}"
                path.write_bytes(base64.b64decode(content["data"]))
                images.append(path)
                content = {"type": "image", "saved_to": str(path)}
            logged_content.append(content)
        result = Result(tool, raw, images)
        self._log({"step": step, "tool": tool, "arguments": arguments, "seconds": round(elapsed, 3),
                   "result": {**raw, "content": logged_content}})

        status = "ERROR" if result.is_error else "ok"
        text = result.text if len(result.text) <= PRINT_LIMIT else result.text[:PRINT_LIMIT] + " [...]"
        print(f"    {status} in {elapsed:.1f} s", flush=True)
        if text:
            print("    " + text.replace("\n", "\n    "), flush=True)
        for path in result.images:
            print(f"    image: {path}", flush=True)

        if result.is_error and not expect_error:
            raise ToolError(f"{tool} failed: {result.text}")
        if expect_error and not result.is_error:
            raise ToolError(f"{tool} was expected to fail but succeeded: {result.text}")
        return result

    def close(self):
        try:
            self._proc.stdin.close()
            self._proc.wait(timeout=10)
        except Exception:
            self._proc.kill()
        self._transcript.close()
        self._stderr.close()


def sw(action: str):
    """Runs devloop/sw.ps1 <action> (Windows PowerShell, which its COM calls need)."""
    command = ["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(DEVLOOP / "sw.ps1"), action]
    if subprocess.run(command).returncode != 0:
        raise RuntimeError(f"sw.ps1 {action} failed")


def new_run_dir(name: str) -> Path:
    return OUT / f"{datetime.now():%Y%m%d-%H%M%S}-{name}"


def main(argv: list[str]) -> int:
    if len(argv) < 2 or argv[1] not in ("tools", "call", "run"):
        print(__doc__)
        return 2
    command = argv[1]
    reset = "--no-reset" not in argv
    argv = [arg for arg in argv if arg != "--no-reset"]
    name = command if command != "call" else f"call-{argv[2] if len(argv) > 2 else ''}"
    if command == "run":
        name = "+".join(Path(p).stem for p in argv[2:]) or "run"
    if command == "run":
        sw("start")
        if reset:
            sw("reset")
    run_dir = new_run_dir(name)
    print(f"run folder: {run_dir}")
    client = Client(run_dir)
    try:
        if command == "tools":
            for tool in client.tools():
                print(f"{tool['name']}: {tool.get('description', '').splitlines()[0][:110]}")
        elif command == "call":
            arguments = json.loads(argv[3]) if len(argv) > 3 else {}
            client.call(argv[2], **arguments)
        else:
            for path in argv[2:]:
                print(f"\n=== scenario {path}")
                source = Path(path).read_text(encoding="utf-8")
                exec(compile(source, path, "exec"), {"mcp": client, "ToolError": ToolError, "__name__": "__scenario__"})
                print(f"\n=== {path}: PASSED")
    except Exception:
        traceback.print_exc()
        print(f"\nFAILED -- run folder: {run_dir}")
        return 1
    finally:
        client.close()
    print(f"\nOK -- run folder: {run_dir}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))

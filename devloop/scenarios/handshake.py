# Finding the add-in against real SolidWorks (docs/notes/architecture.md,
# "Finding the add-in"): the instance file it publishes (an OS-chosen port, a
# token), instance files of dead or non-SolidWorks processes ignored and
# deleted, calls with no token or a wrong one refused, and the same server
# process following the add-in to its new port after it's unloaded and
# loaded again. Run with SLD_MCP_PORT unset:
#   py devloop/mcp_client.py run devloop/scenarios/handshake.py

import json
import os
import re
import subprocess
import tempfile
from pathlib import Path

DEVLOOP = Path(__file__).resolve().parent.parent
SWCOM = DEVLOOP / "swcom.ps1"
DLL = DEVLOOP.parent / "sld_mcp" / "x64" / "Debug" / "sld_mcp.dll"
INSTANCES = Path(os.environ["LOCALAPPDATA"]) / "sld_mcp" / "instances"
Client = type(mcp)

assert "SLD_MCP_PORT" not in os.environ, "run with SLD_MCP_PORT unset: it pins the port"


def com(code: str) -> str:
    """Runs PowerShell `code` against SolidWorks through swcom.ps1 (see there)."""
    script = mcp.run_dir / f"com-{len(list(mcp.run_dir.glob('com-*.ps1'))) + 1}.ps1"
    script.write_text(code, encoding="utf-8")
    p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(SWCOM),
                        "-CodeFile", str(script)], capture_output=True, text=True, timeout=150)
    print(f"    com {script.name}: {p.stdout.strip()}")
    assert p.returncode == 0, f"{script.name} failed: {p.stderr.strip()}"
    return p.stdout.strip()


def solidworks_pid() -> int:
    out = subprocess.run(["tasklist", "/FI", "IMAGENAME eq SLDWORKS.exe", "/FO", "CSV", "/NH"],
                         capture_output=True, text=True).stdout
    pids = [int(row.split('","')[1]) for row in out.splitlines() if row.startswith('"')]
    assert len(pids) == 1, f"expected one SolidWorks, got {pids}"
    return pids[0]


def instance_file(pid: int) -> dict | None:
    path = INSTANCES / f"{pid}.json"
    return json.loads(path.read_text()) if path.exists() else None


def call_with(env: dict, name: str, **kwargs):
    """get_active_document_name through a second server process run with `env` added."""
    saved = dict(os.environ)
    os.environ.update(env)
    try:
        other = Client(mcp.run_dir / name)
    finally:
        os.environ.clear()
        os.environ.update(saved)
    try:
        return other.call("get_active_document_name", **kwargs)
    finally:
        other.close()


# --- The instance file of the running SolidWorks
pid = solidworks_pid()
info = instance_file(pid)
assert info, f"no {INSTANCES / f'{pid}.json'}"
print(f"    instance file: {info}")
assert info["pid"] == pid
assert re.fullmatch(r"[0-9a-f]{32}", info["token"]), info["token"]
assert 0 < info["port"] < 65536
assert re.fullmatch(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ", info["started"]), info["started"]
assert not list(INSTANCES.glob("*.tmp")), "a .tmp file left behind"

# --- Left-behind files: a dead pid, and a newer one whose pid is a live
# process that isn't SolidWorks (this one). Neither may be used; both go.
dead = INSTANCES / "4000000.json"
dead.write_text(json.dumps({"pid": 4000000, "port": 1, "token": "0" * 32, "started": "2099-01-01T00:00:00Z"}))
impostor = INSTANCES / f"{os.getpid()}.json"
impostor.write_text(json.dumps({"pid": os.getpid(), "port": 1, "token": "0" * 32,
                                "started": "2099-01-01T00:00:01Z"}))
mcp.call("get_active_document_name")
assert not dead.exists(), "dead pid's file kept"
assert not impostor.exists(), "non-SolidWorks pid's file kept"
assert instance_file(pid) == info, "SolidWorks' own file changed"

# --- Refused calls. A LOCALAPPDATA with no instance file and SLD_MCP_PORT at
# the add-in: no token.
with tempfile.TemporaryDirectory() as empty:
    r = call_with({"LOCALAPPDATA": empty, "SLD_MCP_PORT": str(info["port"])}, "no-token", expect_error=True)
    assert "refused the call" in r.text and "no connection token" in r.text, r.text

# One with a file for that SolidWorks and port but another token.
with tempfile.TemporaryDirectory() as fake:
    (Path(fake) / "sld_mcp" / "instances").mkdir(parents=True)
    (Path(fake) / "sld_mcp" / "instances" / f"{pid}.json").write_text(json.dumps({**info, "token": "f" * 32}))
    r = call_with({"LOCALAPPDATA": fake}, "wrong-token", expect_error=True)
    assert "refused the call" in r.text and "wrong connection token" in r.text, r.text

# No instance file at all: says so.
with tempfile.TemporaryDirectory() as empty:
    r = call_with({"LOCALAPPDATA": empty}, "no-file", expect_error=True)
    assert "no running SolidWorks has the add-in loaded" in r.text, r.text

# --- Unload: the file goes and calls say there's no add-in. Load again: a
# new token (and port), and the same server process finds it.
dll = str(DLL).replace("'", "''")
print("    UnloadAddIn ->", com(f"Com $sw UnloadAddIn @('{dll}')"))
assert instance_file(pid) is None, "instance file kept after unloading"
r = mcp.call("get_active_document_name", expect_error=True)
assert "can't reach the sld_mcp add-in" in r.text and "no running SolidWorks" in r.text, r.text

print("    LoadAddIn ->", com(f"Com $sw LoadAddIn @('{dll}')"))
reloaded = instance_file(pid)
assert reloaded, "no instance file after loading again"
print(f"    instance file: {reloaded}")
assert reloaded["token"] != info["token"]
mcp.call("get_active_document_name")
part = mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=20, height_mm=10)
mcp.call("extrude_sketch", depth_mm=5)

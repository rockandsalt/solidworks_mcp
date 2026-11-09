# open_document imports (STEP, IGES, Parasolid) and recognize_features
# against real SolidWorks (docs/notes/files.md, "Importing"): a 60 x 40 x 10
# block with a blind hole, a counterbore, a countersink, a plain through cut,
# a fillet and a chamfer is recognized as made, exported in each format,
# imported again (no dialog, 3D Interconnect left as the user had it, the
# body an Imported1 feature of the same volume) and recognized the same; a
# cut built on an imported face is recognized as a new hole and kept through
# Save As and reopening; refusals; an import is named after its file and
# refused next to an open document of that name; an assembly STEP is refused
# where its parts would be saved over existing files, and from a folder of
# its own comes in as an assembly whose parts are saved there.
# Run with:
#   py devloop/mcp_client.py run devloop/scenarios/importing.py

import math
import re
import shutil
import subprocess
import time
from pathlib import Path

SWCOM = Path(__file__).resolve().parent.parent / "swcom.ps1"


def com(code: str) -> str:
    """Runs PowerShell `code` against SolidWorks through swcom.ps1 (see there)."""
    script = mcp.run_dir / f"com-{len(list(mcp.run_dir.glob('com-*.ps1'))) + 1}.ps1"
    script.write_text(code, encoding="utf-8")
    p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(SWCOM),
                        "-CodeFile", str(script)], capture_output=True, text=True, timeout=150)
    print(f"    com {script.name}: {p.stdout.strip()}")
    assert p.returncode == 0, f"{script.name} failed: {p.stderr.strip()}"
    return p.stdout.strip()


def sw_dialogs() -> list[str]:
    p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
                        str(SWCOM.parent / "sw.ps1"), "dialogs"], capture_output=True, text=True, timeout=60)
    return [line.strip() for line in p.stdout.splitlines() if line.strip()]


def no_dialog_left(timeout_s: float = 120) -> None:
    """No SolidWorks dialog open, waiting out the untitled one that shows for a
    while after the first import of a SolidWorks session (files.md)."""
    start = time.monotonic()
    while dialogs := sw_dialogs():
        assert time.monotonic() - start < timeout_s, f"dialogs still open: {dialogs}"
        time.sleep(2)
    if time.monotonic() - start > 1:
        print(f"    a dialog closed by itself after {time.monotonic() - start:.0f} s")


def open_titles() -> list[str]:
    """Titles of every document SolidWorks has loaded."""
    text = com("foreach ($d in @(Com $sw GetDocuments)) { if ($d -ne $null) { Com $d GetTitle } }")
    return sorted(line.strip() for line in text.splitlines() if line.strip())


def interconnect() -> str:
    return com("Com $sw GetUserPreferenceToggle @(691)")


def volume() -> float:
    bodies = mcp.call("inspect_model", what="bodies").json["bodies"]
    assert len(bodies) == 1, bodies
    return bodies[0]["volume_mm3"]


def recognized(within=None) -> dict:
    r = mcp.call("recognize_features", **({"within": within} if within else {}))
    print("    summary:", r.json["summary"])
    return r.json


def holes_key(j: dict) -> list:
    """The holes as (kind, Ø, through, depth, cbore/csink sizes), rounded for imports."""
    def one(h):
        extra = h.get("counterbore") or h.get("countersink") or {}
        return (h["kind"], round(h["diameter_mm"], 2), h["through"], round(h["depth_mm"], 2),
                tuple(round(v, 1) for v in extra.values()), round(h.get("drill_point_angle_deg", 0), 1))
    return sorted(one(h) for h in j.get("holes", []))


def fillets_key(j: dict) -> list:
    return sorted((f["kind"], round(f["radius_mm"], 2), len(f["faces"])) for f in j.get("fillets", []))


def chamfers_key(j: dict) -> list:
    return sorted((c["kind"], tuple(round(v, 2) for v in c.get("legs_mm", []))) for c in j.get("chamfers", []))


out = mcp.run_dir.resolve()
block = out / "imp-block.SLDPRT"

# ---- The block, made with features ---------------------------------------------
# 60 x 40 box 10 deep along X (the Front Plane faces +X on this machine): x 0..10,
# y -30..30, z -20..20.
mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=60, height_mm=40)
mcp.call("extrude_sketch", depth_mm=10)
faces = mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]


def face_at(axis: int, value: float) -> str:
    hits = [f["id"] for f in faces
            if abs(f["normal"][axis]) > 0.99 and abs(f["point_on_plane_mm"][axis] - value) < 1e-6]
    assert len(hits) == 1, (axis, value, faces)
    return hits[0]


far, near = face_at(0, 10), face_at(0, 0)
mcp.call("hole_wizard", kind="hole", size="M5", on=far, depth_mm=5, diameter_mm=5, locations=[[10, -20, 10]])
r = mcp.call("hole_wizard", kind="counterbore", size="M4", on=near, through_all=True)
print("    counterbore:", r.text)
mcp.call("hole_wizard", kind="countersink", size="M5", on=far, locations=[[10, 20, -10]], through_all=True,
         countersink_angle_deg=90)
mcp.call("sketch", shape="circle", on=far, center=[10, -20, -10], diameter_mm=8)
mcp.call("extrude_sketch", cut=True, through_all=True)
edges = mcp.call("inspect_model", what="edges", curve_type="line").json["edges"]


def long_edge(x: float, z: float) -> str:
    hits = [e["id"] for e in edges if abs(e["length_mm"] - 60) < 1e-6
            and abs(e["start_mm"][0] - x) < 1e-6 and abs(e["start_mm"][2] - z) < 1e-6]
    assert len(hits) == 1, (x, z, edges)
    return hits[0]


fillet_edge, chamfer_edge = long_edge(10, 20), long_edge(0, -20)
mcp.call("fillet_edges", edges=[fillet_edge], radius_mm=3)
mcp.call("chamfer_edges", edges=[chamfer_edge], distance_mm=2)
mcp.call("save_document", path=str(block))

native_volume = volume()
native = recognized()
assert "imported" not in native, native
kinds = sorted(h["kind"] for h in native["holes"])
assert kinds == ["counterbore", "countersink", "simple", "simple"], native["holes"]
by_kind = {}
for h in native["holes"]:
    by_kind.setdefault(h["kind"], []).append(h)
blind = [h for h in by_kind["simple"] if not h["through"]]
assert len(blind) == 1 and blind[0]["diameter_mm"] == 5 and blind[0]["depth_mm"] == 5, native["holes"]
assert blind[0]["entry_mm"] == [10, -20, 10] and blind[0]["direction"] == [-1, 0, 0], blind[0]
assert 110 < blind[0]["drill_point_angle_deg"] < 130, blind[0]  # the wizard's 118 degree point
cut = [h for h in by_kind["simple"] if h["through"]]
assert len(cut) == 1 and cut[0]["diameter_mm"] == 8 and cut[0]["depth_mm"] == 10, cut
cbore = by_kind["counterbore"][0]
assert cbore["through"] and cbore["entry_mm"] == [0, 0, 0] and cbore["direction"] == [1, 0, 0], cbore
assert cbore["counterbore"]["diameter_mm"] > cbore["diameter_mm"] > 4, cbore
csink = by_kind["countersink"][0]
assert csink["through"] and csink["countersink"]["angle_deg"] == 90, csink
assert csink["entry_mm"] == [10, 20, -10] and csink["direction"] == [-1, 0, 0], csink
assert fillets_key(native) == [("round", 3.0, 1)], native.get("fillets")
assert chamfers_key(native) == [("planar", (2.0, 2.0))], native.get("chamfers")
assert "other_faces" not in native, native["other_faces"]
x_planes = [p for p in native["planes"] if abs(p["direction"][0]) > 0.99]
assert sorted(f["at_mm"] * p["direction"][0] for p in x_planes for f in p["faces"]) == [0, 10], x_planes

before_3di = interconnect()
for ext in ["STEP", "IGS", "X_T"]:
    mcp.call("save_document", path=str(out / f"imp-block.{ext}"))

# ---- Importing each format ---------------------------------------------------------
mcp.call("close_document")
for ext in ["STEP", "IGS", "X_T"]:
    r = mcp.call("open_document", path=str(out / f"imp-block.{ext}"))
    print(f"    {ext}:", r.text)
    assert r.text.startswith(f"imported imp-block.{ext} as ") and "(part, never saved)" in r.text, r.text
    no_dialog_left()
    assert interconnect() == before_3di, "3D Interconnect wasn't put back"
    features = mcp.call("inspect_model", what="features").json["features"]
    imported = [f for f in features if f.get("imported")]
    assert len(imported) == 1 and imported[0]["type"] == "BaseBody", features
    v = volume()
    assert abs(v - native_volume) < 1e-4 * native_volume, (ext, v, native_volume)
    got = recognized()
    assert got["imported"] == [imported[0]["name"]], got
    assert holes_key(got) == holes_key(native), (ext, holes_key(got), holes_key(native))
    assert fillets_key(got) == fillets_key(native), (ext, got.get("fillets"))
    assert chamfers_key(got) == chamfers_key(native), (ext, got.get("chamfers"))
    if ext != "X_T":
        mcp.call("close_document", discard_changes=True)

# ---- Building on the imported Parasolid ------------------------------------------
x10 = [f["face"] for p in got["planes"] if abs(p["direction"][0]) > 0.99 for f in p["faces"]
       if abs(f["at_mm"] * p["direction"][0] - 10) < 1e-6]
assert len(x10) == 1, got["planes"]
mcp.call("sketch", shape="circle", on=x10[0], center=[10, 0, 12], diameter_mm=4)
mcp.call("extrude_sketch", cut=True, depth_mm=3)
v = volume()
assert abs(native_volume - v - math.pi * 4 * 3) < 1e-3, (native_volume, v)
# The imported feature's own faces don't include the cut's; the body has both.
assert not [h for h in recognized(within=imported[0]["name"])["holes"] if h["diameter_mm"] == 4]
body = mcp.call("inspect_model", what="bodies").json["bodies"][0]["id"]
after = recognized(within=body)
new = [h for h in after["holes"] if h["diameter_mm"] == 4]
assert len(new) == 1 and not new[0]["through"] and new[0]["depth_mm"] == 3, after["holes"]
assert new[0]["entry_mm"] == [10, 0, 12] and new[0]["direction"] == [-1, 0, 0], new
built = out / "imp-built.SLDPRT"
mcp.call("save_document", path=str(built))
mcp.call("close_document")
mcp.call("open_document", path=str(built))
features = mcp.call("inspect_model", what="features").json["features"]
assert any(f.get("imported") for f in features) and any(f["name"].startswith("Cut-Extrude") for f in features), features
assert abs(volume() - v) < 1e-3
mcp.call("close_document")

# ---- Refusals ------------------------------------------------------------------------
r = mcp.call("open_document", path=str(out / "imp-block.STEP"), read_only=True, expect_error=True)
assert "read_only" in r.text, r.text
r = mcp.call("open_document", path=str(out / "imp-block.stl"), expect_error=True)
assert ".X_T" in r.text and ".STL" not in r.text, r.text
r = mcp.call("open_document", path=str(out / "no-such.step"), expect_error=True)
assert "no file" in r.text, r.text

# ---- Names: after the file, never one an open document has --------------------
# SolidWorks would open a second "imp-block.SLDPRT", which close_document and
# activating by title couldn't tell apart.
mcp.call("open_document", path=str(out / "imp-block.STEP"))
r = mcp.call("open_document", path=str(out / "imp-block.STEP"), expect_error=True)
assert "a document called imp-block.SLDPRT is open (never saved)" in r.text, r.text
mcp.call("close_document", discard_changes=True)
mcp.call("open_document", path=str(block))
r = mcp.call("open_document", path=str(out / "imp-block.X_T"), expect_error=True)
assert f"is open ({block})" in r.text.replace("\\\\", "\\"), r.text
assert open_titles() == ["imp-block.SLDPRT"], open_titles()
mcp.call("close_document")
other = out / "Other Name.STEP"
shutil.copy(out / "imp-block.STEP", other)
r = mcp.call("open_document", path=str(other))
assert "as Other Name.SLDPRT (part, never saved)" in r.text, r.text
mcp.call("close_document", discard_changes=True)

# ---- An assembly STEP ---------------------------------------------------------------
mcp.call("create_assembly")
r = mcp.call("insert_components", file=str(block))
first = re.findall(r"^- (component:\d+):", r.text, re.M)
assert len(first) == 1, r.text
mcp.call("insert_components", copy_of=first[0], at=[{"origin_mm": [0, 80, 0]}])
mcp.call("save_document", path=str(out / "imp-asm.SLDASM"))
mcp.call("save_document", path=str(out / "imp-asm.STEP"))
com("Com $sw CloseAllDocuments @($true) | Out-Null")
working_dir = com("Com $sw GetCurrentWorkingDirectory")
# Its part would be saved as imp-block.SLDPRT next to it, over the real one.
r = mcp.call("open_document", path=str(out / "imp-asm.STEP"), expect_error=True)
assert "files of their names already" in r.text and str(block) in r.text.replace("\\\\", "\\"), r.text
assert open_titles() == [], open_titles()
assert block.stat().st_mtime < (out / "imp-asm.STEP").stat().st_mtime, "the real part was written"
# From a folder of its own it comes in, and its parts are saved there.
alone = out / "imp-asm-alone"
alone.mkdir()
shutil.copy(out / "imp-asm.STEP", alone / "imp-asm.STEP")
r = mcp.call("open_document", path=str(alone / "imp-asm.STEP"))
print("    assembly:", r.text)
assert "(assembly, never saved)" in r.text and ".SLDPRT files next to it" in r.text, r.text
no_dialog_left()
assert com("Com $sw GetCurrentWorkingDirectory") == working_dir, "the working directory wasn't put back"
components = mcp.call("inspect_model", what="components").json["components"]
assert len(components) == 2, components
got = recognized()
assert holes_key(got) == sorted(holes_key(native) * 2), (holes_key(got), holes_key(native))
# Its never-saved part goes next to the assembly: not where the block is.
before = block.stat().st_mtime
r = mcp.call("save_document", path=str(out / "imp-asm-kept.SLDASM"), expect_error=True)
assert "files of their names already" in r.text and str(block) in r.text.replace("\\\\", "\\"), r.text
assert block.stat().st_mtime == before and not (out / "imp-asm-kept.SLDASM").exists()
mcp.call("save_document", path=str(alone / "imp-asm-kept.SLDASM"))
assert (alone / "imp-block.SLDPRT").is_file(), sorted(p.name for p in alone.iterdir())
assert block.stat().st_mtime == before, "the real part was written"
assert com("Com $sw GetCurrentWorkingDirectory") == working_dir, "the working directory wasn't put back"
com("Com $sw CloseAllDocuments @($true) | Out-Null")
for f in alone.iterdir():
    f.unlink()
alone.rmdir()

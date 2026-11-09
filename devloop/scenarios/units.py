# Document units against real SolidWorks (docs/notes/units.md): an inch
# (IPS) part made with create_part units=ips while the tools keep taking mm
# (its box checked in mm), an ANSI Inch counterbore picked by its size, a
# feature control frame whose 0.127 mm tolerance is written as 0.005,
# document_units reading and switching units, decimals and the drafting
# standard (each read back through COM); then an inch drawing of it: ANSI B
# by default, a dimensioned view, an ASME border and title block by
# default (ISO again after switching the drawing to MMGS), a frame on the
# sheet, render. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/units.py

import json
import subprocess
from pathlib import Path

SWCOM = Path(__file__).resolve().parent.parent / "swcom.ps1"


def com(code: str) -> str:
    """Runs PowerShell `code` against SolidWorks through swcom.ps1 (see there)."""
    script = mcp.run_dir / f"com-{len(list(mcp.run_dir.glob('com-*.ps1'))) + 1}.ps1"
    script.write_text(code, encoding="utf-8")
    p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(SWCOM),
                        "-CodeFile", str(script)], capture_output=True, text=True, timeout=150)
    assert p.returncode == 0, f"{script.name} failed: {p.stderr.strip()}"
    return p.stdout.strip()


def prefs():
    """(unit system, length unit, length decimals, drafting standard) of the active document."""
    out = com("$e = Com (Com $sw ActiveDoc -Get) Extension -Get\n"
              "@(263, 47, 49, 13) | ForEach-Object { Com $e GetUserPreferenceInteger @([int]$_, [int]0) }")
    return tuple(int(v) for v in out.split())


def entry(text):
    """The JSON line of a reply."""
    return json.loads(text.splitlines()[-1])


def near(a, b, tol=0.01):
    return len(a) == len(b) and all(abs(x - y) <= tol for x, y in zip(a, b))


# ---- An inch part: 2 x 1 x 0.5 in, given in mm.
r = mcp.call("create_part", units="ips", decimals=3)
print("   ", r.text)
assert "units IPS (inch, pound, second): lengths in inches, 3 decimals" in r.text, r.text
units_ips = prefs()
assert units_ips[:3] == (3, 3, 3), units_ips
assert "units IPS" in mcp.call("get_active_document_name").text
mcp.call("sketch", shape="rectangle", width_mm=50.8, height_mm=25.4)
mcp.call("extrude_sketch", depth_mm=12.7)
body = mcp.call("inspect_model", what="bodies").json["bodies"][0]["bbox_mm"]
size = [round(b - a, 3) for a, b in zip(body["min"], body["max"])]
assert sorted(size) == [12.7, 25.4, 50.8], body
planes = mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]


def face(normal):
    return [f["id"] for f in planes if near(f["normal"], normal, 1e-6)][0]


front, back = face([1, 0, 0]), face([-1, 0, 0])

# A 1/4 counterbore: the size makes it ANSI Inch (no standard given).
r = mcp.call("hole_wizard", kind="counterbore", size="1/4", on=front, locations=[[12.7, 12.7, 0]], through_all=True)
print("   ", r.text.splitlines()[0])
assert "ANSI Inch" in r.text, r.text
# M5 in the inch part stays ISO.
r = mcp.call("hole_wizard", kind="hole", size="M5", on=front, locations=[[12.7, -12.7, 0]], depth_mm=5)
print("   ", r.text.splitlines()[0])
assert "ISO" in r.text, r.text

# A frame's tolerance is text: 0.127 mm is written as 0.005 (in).
r = mcp.call("add_geometric_tolerance", characteristic="flatness", tolerance=0.127, on=back)
print("   ", r.text.splitlines()[0])
assert "0.127 mm is written as 0.005 (inches" in r.text, r.text
e = entry(r.text)
assert [f["callout"] for f in e["frames"]] == ["flatness | 0.005"], r.text
pmi = mcp.call("inspect_annotations").json
assert "is in inches" in pmi.get("document_units", ""), pmi
# Text goes as typed.
r = mcp.call("add_geometric_tolerance", characteristic="flatness", tolerance="0.004", on=front)
assert [f["callout"] for f in entry(r.text)["frames"]] == ["flatness | 0.004"] and "written as" not in r.text, r.text

# ---- document_units: read, switch to MMGS and back with ANSI.
r = mcp.call("document_units")
print("   ", r.text)
assert "IPS" in r.text and "stay in mm" in r.text, r.text
r = mcp.call("document_units", units="mmgs")
print("   ", r.text)
assert "MMGS" in r.text and "(were IPS" in r.text, r.text
assert prefs()[:2] == (5, 0), prefs()
r = mcp.call("document_units", units="ips", decimals=4, drafting_standard="ansi")
print("   ", r.text)
assert prefs() == (3, 3, 4, 1), prefs()
assert "drafting standard ANSI" in r.text, r.text
r = mcp.call("document_units", units="ips")
assert "(unchanged)" in r.text, r.text
mcp.call("document_units", decimals=9, expect_error=True)
# The geometry didn't move.
body = mcp.call("inspect_model", what="bodies").json["bodies"][0]["bbox_mm"]
assert sorted(round(b - a, 3) for a, b in zip(body["min"], body["max"])) == [12.7, 25.4, 50.8], body
mcp.call("save_document", path=str(mcp.run_dir.resolve() / "units-block.SLDPRT"))

r = mcp.call("create_assembly", units="ips")
assert "units IPS" in r.text, r.text
r = mcp.call("create_part")
print("   ", r.text)
assert "; units " in r.text, r.text

# ---- An inch drawing: ANSI B unless a size is asked for.
r = mcp.call("create_drawing", units="ips", decimals=3, drafting_standard="ansi")
print("   ", r.text)
assert "B (431.8 x 279.4 mm)" in r.text and "units IPS" in r.text and "drafting standard ANSI" in r.text, r.text
assert prefs() == (3, 3, 3, 1), prefs()
mcp.call("create_drawing_view", model=str(mcp.run_dir.resolve() / "units-block.SLDPRT"), at_mm=[200, 150], scale="1:1")
view = [v for s in mcp.call("inspect_drawing", view="Drawing View1").json["sheets"] for v in s["views"]
        if v["name"] == "Drawing View1"][0]
lines = [e for e in view["entities"] if e["type"] == "line"]
bottom = min((e for e in lines if abs(e["start"][1] - e["end"][1]) < 0.01), key=lambda e: e["start"][1])
r = mcp.call("add_drawing_dimensions", dimensions=[{"kind": "length", "entities": [bottom["id"]],
                                                    "at_mm": [200, bottom["start"][1] - 15]}])
print("   ", r.text.splitlines()[0])
info = mcp.call("inspect_drawing", view="Drawing View1").json
print("    inspect_drawing document_units:", info.get("document_units"))
assert "is in inches" in info.get("document_units", ""), info

r = mcp.call("draw_sheet_border")
print("   ", r.text.splitlines()[0])
assert "ASME Y14.1" in r.text and "4 x 2 zones" in r.text and "frame [12.7, 12.7]..[419.1, 266.7] mm" in r.text, r.text
r = mcp.call("draw_title_block")
print("   ", r.text.splitlines()[0])
assert "[241.3, 12.7]..[419.1," in r.text, r.text  # 7 in wide, in the frame's corner
r = mcp.call("add_geometric_tolerance", characteristic="flatness", tolerance=0.05, at_mm=[60, 60])
assert "0.05 mm is written as 0.002 (inches" in r.text, r.text
r = mcp.call("render_drawing")
print("    render:", r.images)
assert r.images

# Switched to MMGS, the defaults are ISO again.
mcp.call("document_units", units="mmgs", drafting_standard="iso")
r = mcp.call("draw_sheet_border")
print("   ", r.text.splitlines()[0])
assert "ISO 5457" in r.text and "frame [20, 10]" in r.text, r.text
r = mcp.call("render_drawing")
print("    render:", r.images)

# A size asked for wins over ANSI B; mm drawings keep A3.
r = mcp.call("create_drawing", units="ips", paper_size="ANSI_C")
assert "C (558.8 x 431.8 mm)" in r.text, r.text
r = mcp.call("create_drawing", units="mmgs")
assert "A3 (420 x 297 mm)" in r.text, r.text

com("Com $sw CloseAllDocuments @($true) | Out-Null")

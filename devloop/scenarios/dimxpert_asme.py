# ASME PMI on a part (docs/notes/dimxpert.md, "ASME"): an inch ANSI block
# with a pattern of two .25 holes; the block tolerance table unset, then
# set (.X / .XX / .XXX, angles) and given to untoleranced dimensions by the
# decimals they show (`decimals` picks .XXX), in DimXpert's own view too;
# an explicit tolerance left alone; no row for 4 decimals; standard=iso in
# the ASME part; ISO 2768 asked in it (a remark); a geometric scheme with a
# composite position frame on the pattern, read back as composite with its
# diameter zones and MMC; composite refused under ISO. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/dimxpert_asme.py

import json
import subprocess
import time
from pathlib import Path

SWCOM = Path(__file__).resolve().parent.parent / "swcom.ps1"


def sw_dialogs() -> list[str]:
    p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
                        str(SWCOM.parent / "sw.ps1"), "dialogs"], capture_output=True, text=True, timeout=60)
    return [line.strip() for line in p.stdout.splitlines() if line.strip()]


def no_dialog_left(timeout_s: float = 60) -> None:
    """No SolidWorks dialog open, waiting out the fresh-session bubble (files.md)."""
    start = time.monotonic()
    while dialogs := sw_dialogs():
        assert time.monotonic() - start < timeout_s, f"dialogs still open: {dialogs}"
        time.sleep(2)


def entry(text):
    return json.loads(text.splitlines()[-1])


def near(a, b, tol=1e-6):
    return len(a) == len(b) and all(abs(x - y) <= tol for x, y in zip(a, b))


def block():
    """A 2 x 1 x .5 in block (x 0..12.7 mm, y -25.4..25.4, z -12.7..12.7)
    with two .25 holes along x at y = +-6.35 mm; face ids by normal."""
    mcp.call("create_part", units="ips", drafting_standard="ansi")
    mcp.call("sketch", shape="rectangle", width_mm=50.8, height_mm=25.4)
    mcp.call("extrude_sketch", depth_mm=12.7)
    for y in (6.35, -6.35):
        planes = mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]
        front = [f["id"] for f in planes if near(f["normal"], [1, 0, 0])][0]
        mcp.call("sketch", shape="circle", on=front, center=[12.7, y, 0], diameter_mm=6.35)
        mcp.call("extrude_sketch", cut=True, through_all=True)
    faces = mcp.call("inspect_model", what="faces").json["faces"]
    by = {name: [f["id"] for f in faces if f["type"] == "plane" and near(f["normal"], n)][0]
          for name, n in [("back", [-1, 0, 0]), ("left", [0, -1, 0]), ("bottom", [0, 0, -1])]}
    holes = [f for f in faces if f["type"] == "cylinder"]
    by["hole"] = [f["id"] for f in holes if f["bbox_mm"]["min"][1] > 0][0]   # the one at y = +6.35
    return by


def dimxpert_limits(display_name):
    """DimXpert's own limits (mm) of the dimension shown as `display_name`."""
    code = f'''
Sub main()
    Set doc = Application.SldWorks.ActiveDoc
    Set dxp = doc.Extension.DimXpertManager(doc.ConfigurationManager.ActiveConfiguration.Name, False).DimXpertPart
    anns = dxp.GetAnnotations
    For i = 0 To UBound(anns)
        If anns(i).GetDisplayEntity.GetName = "{display_name}" Then
            Dim u As Double, l As Double
            anns(i).GetUpperAndLowerLimit u, l
            McpPrint l & "|" & u
        End If
    Next
End Sub
'''
    line = [l for l in mcp.call("run_macro", code=code).text.splitlines() if l.count("|") == 1][0]
    return [float(x) for x in line.split("|")]


f = block()

# ---- An ANSI part follows ASME; its block table is the template's placeholder.
g = mcp.call("pmi_general_tolerance").json
print("   ", g)
assert g["standard"] == "ASME Y14.5" and "block tolerance" in g["applies"], g
assert "not set" in g["block_tolerance"], g

# Without a table, a dimension keeps DimXpert's default, and says why.
r = mcp.call("add_pmi_dimension", kind="location", on=[f["bottom"], f["hole"]])
print("   ", r.text.splitlines()[0])
assert "no block tolerance table yet" in r.text, r.text
r = mcp.call("delete_drawing_annotations", names=[entry(r.text)["name"]])

# ---- The table: .X +-.1, .XX +-.01, .XXX +-.005, angles +-.5 deg.
r = mcp.call("pmi_general_tolerance", block=[{"decimals": 1, "plus_minus_mm": 2.54},
                                             {"decimals": 2, "plus_minus_mm": 0.254},
                                             {"decimals": 3, "plus_minus_mm": 0.127}], block_angle_deg=0.5)
g = r.json
print("   ", r.text)
assert [row["decimals"] for row in g["block_tolerance"]] == [1, 2, 3], g
assert g["block_tolerance"][1]["plus_minus"] == "0.01 inches (0.254 mm)" and g["block_angle_deg"] == 0.5, g
assert "remarks" not in g, g

# The pattern's .25 diameter shows 2 decimals: +-.01 (6.096..6.604 mm), in
# DimXpert's view too.
r = mcp.call("add_pmi_dimension", kind="size", on=[f["hole"]])
print("   ", r.text.splitlines()[0])
e = entry(r.text)
size = e["name"]
assert e["measures"] == "diameter" and e["value_mm"] == 6.35, r.text
assert e["tolerance"] == {"type": "symmetric", "plus_minus_mm": 0.254, "limits_mm": [6.096, 6.604]}, r.text
assert "block tolerance for 2 decimals" in r.text, r.text
lo, hi = dimxpert_limits(size)
assert abs(lo - 6.096) < 1e-6 and abs(hi - 6.604) < 1e-6, (lo, hi)

# A location shown with 3 decimals: +-.005.
r = mcp.call("add_pmi_dimension", kind="location", on=[f["left"], f["hole"]], decimals=3)
print("   ", r.text.splitlines()[0])
e = entry(r.text)
assert e["value_mm"] == 31.75 and e["tolerance"]["plus_minus_mm"] == 0.127, r.text
assert "block tolerance for 3 decimals" in r.text, r.text

# An angle: the angular row.
r = mcp.call("add_pmi_dimension", kind="angle", on=[f["bottom"], f["left"]])
e = entry(r.text)
angle = e["name"]
assert e["tolerance"] == {"type": "symmetric", "plus_minus_deg": 0.5, "limits_deg": [89.5, 90.5]}, r.text

# An explicit tolerance stays as given.
r = mcp.call("add_pmi_dimension", kind="location", on=[f["bottom"], f["hole"]], tolerance="limit",
             upper_mm=0.05, lower_mm=-0.05)
e = entry(r.text)
explicit = e["name"]
assert e["tolerance"]["type"] == "limit" and "block tolerance" not in r.text, r.text
mcp.call("delete_drawing_annotations", names=[explicit])

# No row for 4 decimals: DimXpert's default, and why.
r = mcp.call("add_pmi_dimension", kind="location", on=[f["bottom"], f["hole"]], decimals=4)
assert "no row for 4 decimals" in r.text, r.text
mcp.call("delete_drawing_annotations", names=[entry(r.text)["name"]])

# standard=iso in the ASME part: DimXpert's own default, no block row.
r = mcp.call("add_pmi_dimension", kind="location", on=[f["bottom"], f["hole"]], standard="iso")
assert "block tolerance" not in r.text and "DimXpert's default tolerance" in r.text, r.text
mcp.call("delete_drawing_annotations", names=[entry(r.text)["name"], angle])

# ISO 2768 asked in the ASME part: done, with a remark.
r = mcp.call("pmi_general_tolerance", iso2768="m")
assert r.json["iso2768_class"] == "m (medium)" and "ISO 2768 is ISO's" in r.json["remarks"][0], r.text
no_dialog_left()

# ---- Geometric scheme, ASME: a composite position frame on the pattern,
# .020 to A|B|C and .010 to A, both at MMC.
f = block()
r = mcp.call("auto_dimension_scheme", datums=[f["back"], f["bottom"], f["left"]], tolerancing="geometric",
             position_mm=0.508, composite_position_mm=0.254)
print("   ", r.text.splitlines()[0])
added = entry(r.text)["annotations"]
pattern = [a for a in added if a["type"] == "gtol" and len(a.get("frames", [])) == 2]
assert pattern, added
frame = pattern[0]
print("   ", json.dumps(frame["frames"]))
assert frame.get("composite") is True, frame
assert [x["callout"] for x in frame["frames"]] == ["position | dia .02 (M) | A | B | C", "position | dia .01 (M) | A"], frame
assert "2 decimals" not in r.text, r.text

# Composite under ISO: refused before anything is changed.
r = mcp.call("auto_dimension_scheme", datums=[f["back"]], tolerancing="geometric", standard="iso",
             composite_position_mm=0.254, expect_error=True)
assert "composite frames are ASME" in r.text, r.text

no_dialog_left()
p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(SWCOM.parent / "sw.ps1"),
                    "reset", "-Force"], capture_output=True, text=True, timeout=120)
assert p.returncode == 0, p.stderr
print("dimxpert_asme scenario OK")

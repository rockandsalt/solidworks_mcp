# add_pmi_dimension / auto_dimension_scheme / pmi_general_tolerance against
# real SolidWorks (docs/notes/dimxpert.md): DimXpert (MBD) dimensions on a
# part's faces -- a hole's size, locations, an angle, a basic dimension --
# with DimXpert's default tolerance, the part's ISO 2768 class, a fit and a
# bilateral tolerance (checked in DimXpert's own view too, through a
# read-only macro); text outside the part or where asked; what DimXpert
# refuses; the class changed afterwards; deleted by name; both auto
# schemes. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/dimxpert.py

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
    """No SolidWorks dialog open, waiting out the untitled notification bubble
    a fresh SolidWorks session shows for a few seconds (files.md); a dialog
    DimXpert left open would stay."""
    start = time.monotonic()
    while dialogs := sw_dialogs():
        assert time.monotonic() - start < timeout_s, f"dialogs still open: {dialogs}"
        time.sleep(2)
    if time.monotonic() - start > 1:
        print(f"    a dialog closed by itself after {time.monotonic() - start:.0f} s")


def entry(text):
    """The JSON line of a reply."""
    return json.loads(text.splitlines()[-1])


def near(a, b, tol=1e-6):
    return len(a) == len(b) and all(abs(x - y) <= tol for x, y in zip(a, b))


def dims():
    """name -> inspect_annotations entry, dimensions only."""
    return {a["name"]: a for a in mcp.call("inspect_annotations", kind="dimension").json["annotations"]}


def block():
    """A 40 x 20 x 10 block (x 0..10, y -20..20, z -10..10 in this template)
    with a 6 mm hole through it along x at y = 5; face ids by normal."""
    mcp.call("create_part")
    mcp.call("sketch", shape="rectangle", width_mm=40, height_mm=20)
    mcp.call("extrude_sketch", depth_mm=10)
    planes = mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]
    front = [f["id"] for f in planes if near(f["normal"], [1, 0, 0])][0]
    mcp.call("sketch", shape="circle", on=front, center=[10, 5, 0], diameter_mm=6)
    mcp.call("extrude_sketch", cut=True, through_all=True)
    faces = mcp.call("inspect_model", what="faces").json["faces"]
    by = {name: [f["id"] for f in faces if f["type"] == "plane" and near(f["normal"], n)][0]
          for name, n in [("front", [1, 0, 0]), ("back", [-1, 0, 0]), ("left", [0, -1, 0]), ("right", [0, 1, 0]),
                          ("top", [0, 0, 1]), ("bottom", [0, 0, -1])]}
    by["hole"] = [f["id"] for f in faces if f["type"] == "cylinder"][0]
    return by


def dimxpert_view(display_name):
    """DimXpert's own tolerance of the dimension shown as `display_name`
    (e.g. "TXD1"): (type, fit code, upper, lower) in document units."""
    code = f'''
Sub main()
    Set swApp = Application.SldWorks
    Set doc = swApp.ActiveDoc
    Set mgr = doc.Extension.DimXpertManager(doc.ConfigurationManager.ActiveConfiguration.Name, False)
    Set dxp = mgr.DimXpertPart
    anns = dxp.GetAnnotations
    For i = 0 To UBound(anns)
        If anns(i).GetDisplayEntity.GetName = "{display_name}" Then
            Dim u As Double, l As Double
            anns(i).GetUpperAndLowerLimit u, l
            McpPrint anns(i).DimensionType & "|" & anns(i).LimitsAndFitsCode & "|" & u & "|" & l
        End If
    Next
End Sub
'''
    r = mcp.call("run_macro", code=code)
    line = [l for l in r.text.splitlines() if l.count("|") == 3][0]
    t, fit, u, l = line.split("|")
    return int(t), fit, float(u), float(l)


def outside_block(p, slack=1.0):
    x, y, z = p
    return not (-slack < x < 10 + slack and -20 - slack < y < 20 + slack and -10 - slack < z < 10 + slack)


f = block()

# ---- The template's defaults: ISO 2768 general tolerance method, but each
# kind of dimension with DimXpert's own +/- (seen 2026-11-12).
r = mcp.call("pmi_general_tolerance")
g = r.json
print("   ", r.text)
assert g["standard"] == "ISO 1101" and g["iso2768_class"] == "m (medium)" and "ISO 2768" in g["applies"], g
assert g["own_default"]["diameter"] == {"type": "symmetric", "plus_minus_mm": 0.25}, g

# ---- A size dimension on the hole with that default: outside the part.
r = mcp.call("add_pmi_dimension", kind="size", on=[f["hole"]])
print("   ", r.text.splitlines()[0])
e = entry(r.text)
size6 = e["name"]
assert e["dimxpert"] and e["measures"] == "diameter" and e["value_mm"] == 6.0, r.text
assert e["dimxpert_feature"] == "Simple Hole1" and e["attached"] == [f["hole"]], r.text
assert e["tolerance"] == {"type": "symmetric", "plus_minus_mm": 0.25, "limits_mm": [5.75, 6.25]}, r.text
assert "DimXpert's default tolerance" in r.text, r.text
assert outside_block(e["position_mm"]), e["position_mm"]

# ---- ISO 2768-m for the part: every kind under it.
r = mcp.call("pmi_general_tolerance", iso2768="m")
g = r.json
assert g["iso2768_class"] == "m (medium)" and "own_default" not in g, g
assert {"diameter", "location_distance", "location_angle", "fillet_radius"} <= set(g["general_tolerance"]), g

# A location dimension from the left face to the hole: 25 mm, ISO 2768-m
# (6..30 mm: +/-0.2).
r = mcp.call("add_pmi_dimension", kind="location", on=[f["left"], f["hole"]])
print("   ", r.text.splitlines()[0])
e = entry(r.text)
loc25 = e["name"]
assert e["measures"] == "length" and e["value_mm"] == 25.0, r.text
assert e["tolerance"] == {"type": "general", "plus_minus_mm": 0.2, "limits_mm": [24.8, 25.2]}, r.text
assert sorted(e["attached"]) == sorted([f["left"], f["hole"]]) and "general tolerance" in r.text, r.text
assert outside_block(e["position_mm"]), e["position_mm"]

# The size dimension kept its own +/- (it isn't under the general tolerance).
assert dims()[size6]["tolerance"]["type"] == "symmetric"

# From the back face, across the hole's axis: DimXpert makes an angle.
r = mcp.call("add_pmi_dimension", kind="location", on=[f["back"], f["hole"]])
e = entry(r.text)
assert e["measures"] == "angle" and e["value_deg"] == 90.0 and "DimXpert made an angle" in r.text, r.text
r = mcp.call("delete_drawing_annotations", names=[e["name"]])
assert "deleted 1 annotation(s)" in r.text, r.text

# ---- A bilateral tolerance from the bottom face: 10 +0.1/-0.05, in
# DimXpert's own view too; its text where asked (DimXpert keeps it in its
# annotation view's plane: two of three coordinates as asked).
r = mcp.call("add_pmi_dimension", kind="location", on=[f["bottom"], f["hole"]], tolerance="bilateral",
             upper_mm=0.1, lower_mm=-0.05, at_mm=[5, 9, -30])
print("   ", r.text.splitlines()[0])
e = entry(r.text)
loc10 = e["name"]
p = e["position_mm"]
assert e["value_mm"] == 10.0 and sum(abs(a - b) < 0.5 for a, b in zip(p, [5, 9, -30])) >= 2, r.text
assert e["tolerance"] == {"type": "bilateral", "upper_mm": 0.1, "lower_mm": -0.05, "limits_mm": [9.95, 10.1]}, r.text
t, fit, u, l = dimxpert_view(loc10)
print("    DimXpert's view of the bilateral:", t, fit, u, l)
assert abs(u - 10.1) < 1e-6 and abs(l - 9.95) < 1e-6, (t, fit, u, l)

# ---- A fit on the hole: set_dimension_tolerance on the size dimension
# finds its display dimension (diameter) and DimXpert takes it on the rebuild.
full = dims()[size6]["dimension"]
r = mcp.call("set_dimension_tolerance", name=full, tolerance="fit", hole_fit="H7")
print("   ", r.text)
assert r.json["measures"] == "diameter" and r.json["tolerance"] == {"type": "fit", "hole_fit": "H7"}, r.text
t, fit, u, l = dimxpert_view(size6)
print("    DimXpert's view of the fit:", t, fit, u, l)
assert t in (3, 4), (t, fit, u, l)  # swDimXpertDimTolType_ISOLimitsAndFits(NoNominal)

# ---- Basic location from the top face; an angle between bottom and left.
r = mcp.call("add_pmi_dimension", kind="basic", on=[f["top"], f["hole"]])
e = entry(r.text)
basic = e["name"]
assert e["value_mm"] == 10.0 and e["tolerance"] == {"type": "basic"}, r.text
r = mcp.call("add_pmi_dimension", kind="angle", on=[f["bottom"], f["left"]])
e = entry(r.text)
angle = e["name"]
assert e["measures"] == "angle" and e["value_deg"] == 90.0 and e["tolerance"]["type"] == "general", r.text
assert e["tolerance"]["limits_deg"][0] < 90 < e["tolerance"]["limits_deg"][1], r.text

# ---- The hole located again, from the right face: DimXpert warns that the
# part is over-constrained (a modal dialog, closed); refused, nothing added.
count = len(dims())
r = mcp.call("add_pmi_dimension", kind="location", on=[f["right"], f["hole"]], expect_error=True)
print("   ", r.text)
assert "overconstrained the part" in r.text and "deleted" in r.text, r.text
assert len(dims()) == count, (count, sorted(dims()))
no_dialog_left()

# ---- Refused, nothing added.
count = len(dims())
r = mcp.call("add_pmi_dimension", kind="size", on=[f["hole"], f["top"]], expect_error=True)
assert "one face" in r.text, r.text
r = mcp.call("add_pmi_dimension", kind="size", on=["edge:1"], expect_error=True)
assert "go on faces" in r.text, r.text
r = mcp.call("add_pmi_dimension", kind="size", on=[f["hole"]], plus_minus_mm=0.1, expect_error=True)
assert "go with `tolerance`" in r.text, r.text
r = mcp.call("add_pmi_dimension", kind="basic", on=[f["top"], f["hole"]], tolerance="symmetric", plus_minus_mm=0.1,
             expect_error=True)
assert "basic dimension takes no tolerance" in r.text, r.text
r = mcp.call("add_pmi_dimension", kind="basic", on=[f["hole"]], expect_error=True)
assert "basic dimension takes two faces" in r.text, r.text
r = mcp.call("add_pmi_dimension", kind="size", on=[f["hole"]], tolerance="fit", hole_fit="7H", expect_error=True)
assert "isn't an ISO fit class" in r.text, r.text
# A plane alone has no size.
r = mcp.call("add_pmi_dimension", kind="size", on=[f["top"]], expect_error=True)
print("   ", r.text)
assert "DimXpert made no dimension" in r.text, r.text
assert len(dims()) == count, (count, sorted(dims()))

# ---- The class changed afterwards: the general ones follow (25: +/-0.1 at
# f), the rest keep theirs.
r = mcp.call("pmi_general_tolerance", iso2768="f")
assert r.json["iso2768_class"] == "f (fine)", r.text
now = dims()
assert now[loc25]["tolerance"] == {"type": "general", "plus_minus_mm": 0.1, "limits_mm": [24.9, 25.1]}, now[loc25]
assert now[loc10]["tolerance"]["type"] == "bilateral" and now[size6]["tolerance"]["type"] == "fit", now
assert now[basic]["tolerance"] == {"type": "basic"}, now[basic]
print("    dimensions:", {n: (a.get("value_mm", a.get("value_deg")), a["tolerance"]) for n, a in now.items()})

# ---- Deleted by name.
r = mcp.call("delete_drawing_annotations", names=[angle, basic])
assert "deleted 2 annotation(s)" in r.text, r.text
assert angle not in dims()
no_dialog_left()

# ---- Auto scheme, plus / minus, datums back, bottom, left.
f = block()
r = mcp.call("auto_dimension_scheme", datums=[f["back"], f["bottom"], f["left"]])
print("   ", r.text.splitlines()[0])
added = entry(r.text)["annotations"]
values = sorted(a.get("value_mm", 0) for a in added if a["type"] == "dimension")
assert values == [6.0, 10.0, 10.0, 20.0, 25.0, 40.0], values
assert all(a["dimxpert"] for a in added), added

# ---- Auto scheme, geometric: datums A, B, C on the faces asked, the hole
# located to them.
f = block()
r = mcp.call("auto_dimension_scheme", datums=[f["back"], f["bottom"], f["left"]], tolerancing="geometric")
print("   ", r.text.splitlines()[0])
added = entry(r.text)["annotations"]
datums = {a["label"]: a["attached"] for a in added if a["type"] == "datum"}
assert datums == {"A": [f["back"]], "B": [f["bottom"]], "C": [f["left"]]}, datums
position = [a for a in added if a["type"] == "gtol" and a["attached"] == [f["hole"]]]
assert position and position[0]["frames"][0]["characteristic"] == "position", added
assert position[0]["frames"][0]["datums"] == ["A", "B", "C"], position
# Its MMC, which DimXpert's frames (2022 format) only state in their XML.
assert position[0]["frames"][0]["tolerance"] == {"value": "0.50", "diameter_zone": True, "modifier": "MMC"}, position
assert "datums A on" in r.text, r.text

# A datum face given twice.
r = mcp.call("auto_dimension_scheme", datums=[f["back"], f["back"]], expect_error=True)
assert "face of its own" in r.text, r.text

no_dialog_left()
# The next scenario of the run starts from nothing open.
p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(SWCOM.parent / "sw.ps1"),
                    "reset", "-Force"], capture_output=True, text=True, timeout=120)
assert p.returncode == 0, p.stderr
print("dimxpert scenario OK")

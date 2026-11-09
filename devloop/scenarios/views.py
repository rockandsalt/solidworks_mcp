# create_drawing_view / edit_drawing_view against real SolidWorks
# (docs/notes/drawings.md, "Views"): views of a part with default and given
# model, orientation, position, scale, display and name; a view kept clear of
# the others; another sheet in the background; editing orientation (the
# projected view follows, its own can't be set), scale (sheet / own / parent),
# display (own / parent), configuration, position (a projected view slides
# along its alignment), name and model; refusals that change nothing. The
# part's second configuration and the projected view are made through COM (no
# tool makes them). Run with:
#   py devloop/mcp_client.py run devloop/scenarios/views.py

import subprocess
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


def sheets():
    return {s["name"]: s for s in mcp.call("inspect_drawing").json["sheets"]}


def views(sheet="Sheet1"):
    return {v["name"]: v for v in sheets()[sheet]["views"]}


def entities(view):
    found = [v for s in mcp.call("inspect_drawing", view=view).json["sheets"] for v in s["views"] if v["name"] == view]
    return found[0]["entities"]


def center(view):
    (x0, y0), (x1, y1) = view["outline"]
    return [(x0 + x1) / 2, (y0 + y1) / 2]


def size(view):
    (x0, y0), (x1, y1) = view["outline"]
    return [x1 - x0, y1 - y0]


def overlap(a, b):
    (ax0, ay0), (ax1, ay1) = a["outline"]
    (bx0, by0), (bx1, by1) = b["outline"]
    return ax0 < bx1 and bx0 < ax1 and ay0 < by1 and by0 < ay1


def near(a, b, tol=0.5):
    return all(abs(x - y) <= tol for x, y in zip(a, b))


def axes(view):
    return (view.get("sheet_right"), view.get("sheet_up"), view.get("toward_viewer"))


# ---- The parts: a 40 x 20 x 10 box with a boss (as in sheets.py), a second
# configuration without the boss; and a plain 60 x 30 x 5 plate, closed.
mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
faces = mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]
top = [f["id"] for f in faces if f["normal"][2] > 0.99][0]
mcp.call("sketch", shape="circle", on=top, center=[5, 0, 10], diameter_mm=6)
mcp.call("extrude_sketch", depth_mm=8)
part = mcp.run_dir.resolve() / "views-block.SLDPRT"
mcp.call("save_document", path=str(part))
com(f"""$p = Com $sw ActiveDoc -Get
$cm = Com $p ConfigurationManager -Get
if (-not (Com $cm AddConfiguration2 @('NoBoss', '', '', 0, '', '', $true))) {{ throw 'AddConfiguration2 failed' }}
$x = Com $p Extension -Get
$none = [Runtime.InteropServices.DispatchWrapper]::new([object]$null)
if (-not (Com $x SelectByID2 @('Boss-Extrude2', 'BODYFEATURE', 0.0, 0.0, 0.0, $false, 0, $none, 0))) {{ throw 'select failed' }}
if (-not (Com $p EditSuppress2)) {{ throw 'EditSuppress2 failed' }}
Com $p ShowConfiguration2 @('Default') | Out-Null
'configurations: ' + ((Com $p GetConfigurationNames) -join ', ')""")
mcp.call("save_document")

mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=60, height_mm=30)
mcp.call("extrude_sketch", depth_mm=5)
plate = mcp.run_dir.resolve() / "views-plate.SLDPRT"
mcp.call("save_document", path=str(plate))
com(f"Com $sw CloseDoc @('{plate}')")

mcp.call("create_drawing", sheet_format="a3 - iso")

# ---- A first view with every default: the one open part, *Front, the
# sheet's middle. SolidWorks may rescale the sheet to fit it, and says so.
r = mcp.call("create_drawing_view")
assert r.text.startswith("created Drawing View1 on Sheet1: *Front of views-block.SLDPRT (configuration Default)"), r.text
assert "(sheet scale)" in r.text and "hidden lines removed" in r.text, r.text
v = views()
front = v["Drawing View1"]
assert front["type"] == "named" and front["scale_from"] == "sheet", front
assert near(center(front), [210, 148.5], 1.0), front
sheet_scale = sheets()["Sheet1"]["scale"]
if sheet_scale != "1:1":
    assert "set the sheet's scale from 1:1 to " + sheet_scale in r.text, r.text
front_axes = axes(front)

# ---- A second one, no position: put clear of the first, near the middle.
# A scale given leaves the sheet's alone.
r = mcp.call("create_drawing_view", orientation="top", scale="1:1")
assert "created Drawing View2 on Sheet1: *Top of views-block.SLDPRT" in r.text and "scale 1:1," in r.text, r.text
assert "put clear of the sheet's other views" in r.text, r.text
v = views()
assert not overlap(v["Drawing View1"], v["Drawing View2"]), v
assert sheets()["Sheet1"]["scale"] == sheet_scale and "scale_from" not in v["Drawing View2"], v["Drawing View2"]
assert axes(v["Drawing View2"]) != front_axes, (axes(v["Drawing View2"]), front_axes)
top_circles = [e for e in entities("Drawing View2") if e["type"] == "circle"]
assert top_circles, "the boss should show as a circle from above"

# ---- Everything given: a named, shaded isometric at 1:2 in the upper right.
r = mcp.call("create_drawing_view", model="views-block", orientation="isometric", at_mm=[340, 230], scale="1:2",
             display="shaded_with_edges", name="Iso", configuration="default")
assert r.text.startswith("created Iso on Sheet1: *Isometric of views-block.SLDPRT (configuration Default), scale 1:2, shaded with edges, centered at [340, 230] mm"), r.text
iso = views()["Iso"]
assert iso["display"] == "shaded_with_edges" and iso["scale"] == "1:2" and "model_axes_on_sheet" in iso, iso
assert near(center(iso), [340, 230], 1.0), iso

# ---- Refusals on create leave no view behind.
for args, expected in [
    ({"orientation": "nope"}, 'no named view "nope"'),
    ({"configuration": "Huge"}, 'no configuration "Huge" (its configurations: '),
    ({"model": "nope"}, 'no open part or assembly is called "nope"'),
    ({"model": str(plate.with_suffix(".SLDASM"))}, "there's no file"),
    ({"name": "Iso"}, 'already has a view named "Iso"'),
    ({"sheet": "Sheet9"}, 'no sheet named "Sheet9"'),
    ({"scale": "parent"}, "projected views"),
]:
    r = mcp.call("create_drawing_view", expect_error=True, **args)
    assert expected in r.text, (args, r.text)
assert sorted(views()) == ["Drawing View1", "Drawing View2", "Iso"], views()

# ---- A projected view of the front, to its right (through COM: no tool
# makes projected views yet).
fx, fy = center(views()["Drawing View1"])
com(f"""$d = Com $sw ActiveDoc -Get
$x = Com $d Extension -Get
$none = [Runtime.InteropServices.DispatchWrapper]::new([object]$null)
if (-not (Com $x SelectByID2 @('Drawing View1', 'DRAWINGVIEW', 0.0, 0.0, 0.0, $false, 0, $none, 0))) {{ throw 'select failed' }}
$p = Com $d CreateUnfoldedViewAt3 @({(fx + 90) / 1000}, {fy / 1000}, 0.0, $false)
if (-not $p) {{ throw 'CreateUnfoldedViewAt3 failed' }}
Com $p GetName2""")
v = views()
projected = [n for n in v if v[n]["type"] == "projected"][0]
assert v[projected]["from_view"] == "Drawing View1" and v[projected]["scale_from"] == "parent", v[projected]
assert v[projected].get("display_from") == "parent", v[projected]
side_axes = axes(v[projected])

# ---- Orientation: the front view turns, the projected view turns with it.
r = mcp.call("edit_drawing_view", view="Drawing View1", orientation="bottom")
assert r.text.startswith("changed Drawing View1 on Sheet1: *Bottom of views-block.SLDPRT"), r.text
assert f"{projected} (made from it) follow it" in r.text and "stale" in r.text, r.text
v = views()
assert v["Drawing View1"]["orientation"] == "*Bottom" and axes(v["Drawing View1"]) != front_axes, v["Drawing View1"]
assert near(center(v["Drawing View1"]), [fx, fy], 1.0), v["Drawing View1"]
assert axes(v[projected]) != side_axes, (axes(v[projected]), side_axes)
r = mcp.call("edit_drawing_view", view=projected, orientation="top", expect_error=True)
assert "is derived from Drawing View1" in r.text, r.text
mcp.call("edit_drawing_view", view="Drawing View1", orientation="front")
assert axes(views()["Drawing View1"]) == front_axes

# ---- Scale: own, then back to the sheet's; the projected view follows its
# parent, takes its own, and goes back to the parent's.
r = mcp.call("edit_drawing_view", view="Drawing View1", scale="1:5")
assert "scale 1:5," in r.text, r.text
v = views()
assert v["Drawing View1"]["scale"] == "1:5" and "scale_from" not in v["Drawing View1"], v["Drawing View1"]
assert v[projected]["scale"] == "1:5" and v[projected]["scale_from"] == "parent", v[projected]
mcp.call("edit_drawing_view", view=projected, scale="1:1")
assert views()[projected]["scale"] == "1:1" and "scale_from" not in views()[projected]
mcp.call("edit_drawing_view", view=projected, scale="parent")
assert views()[projected]["scale"] == "1:5" and views()[projected]["scale_from"] == "parent"
r = mcp.call("edit_drawing_view", view="Drawing View1", scale="sheet")
v = views()
assert v["Drawing View1"]["scale"] == sheet_scale and v["Drawing View1"]["scale_from"] == "sheet", v["Drawing View1"]
r = mcp.call("edit_drawing_view", view="Drawing View1", scale="parent", expect_error=True)
assert "isn't derived from another view" in r.text, r.text

# ---- Display: the projected view follows its parent's until given its own.
mcp.call("edit_drawing_view", view="Drawing View1", display="hidden_lines_visible")
v = views()
assert v["Drawing View1"]["display"] == "hidden_lines_visible" and v[projected]["display"] == "hidden_lines_visible", v
mcp.call("edit_drawing_view", view=projected, display="wireframe")
v = views()
assert v[projected]["display"] == "wireframe" and "display_from" not in v[projected], v[projected]
mcp.call("edit_drawing_view", view=projected, display="parent")
v = views()
assert v[projected]["display"] == "hidden_lines_visible" and v[projected]["display_from"] == "parent", v[projected]

# ---- Position: a projected view only slides along its alignment.
px, py = center(views()[projected])
r = mcp.call("edit_drawing_view", view=projected, at_mm=[px + 20, py + 40])
assert "stays aligned with Drawing View1" in r.text, r.text
assert near(center(views()[projected]), [px + 20, py], 1.0), views()[projected]
mcp.call("edit_drawing_view", view="Iso", at_mm=[330, 220])
assert near(center(views()["Iso"]), [330, 220], 1.0), views()["Iso"]

# ---- Configuration: the boss is gone from the top view.
r = mcp.call("edit_drawing_view", view="Drawing View2", configuration="noboss")
assert "(configuration NoBoss)" in r.text, r.text
assert views()["Drawing View2"]["configuration"] == "NoBoss"
assert not [e for e in entities("Drawing View2") if e["type"] == "circle"], "the boss should be gone"
mcp.call("edit_drawing_view", view="Drawing View2", configuration="Default")

# ---- Name.
r = mcp.call("edit_drawing_view", view="Drawing View2", rename="Top view")
assert r.text.startswith("changed Top view on Sheet1"), r.text
assert "Top view" in views() and "Drawing View2" not in views()

# ---- Model: the isometric shows the plate (a file that isn't open).
r = mcp.call("edit_drawing_view", view="Iso", model=str(plate))
print("    replace model:", r.text)
assert "of views-plate.SLDPRT" in r.text, r.text
assert views()["Iso"]["model"] == "views-plate.SLDPRT"

# ---- Refusals on edit change nothing; a failing step puts the earlier ones back.
before = views()
for args, expected in [
    ({"view": "Drawing View1", "scale": "1:5", "orientation": "nope"}, 'no named view "nope"'),
    ({"view": "Drawing View1", "configuration": "Huge"}, 'no configuration "Huge"'),
    ({"view": "Drawing View1", "rename": "Iso"}, 'already has a view named "Iso"'),
    ({"view": "Drawing View1", "display": "shaded", "rename": "a@b"}, '"a@b"'),
    ({"view": "Nope", "scale": "1:1"}, 'no view named "Nope"'),
    ({"view": "Drawing View1"}, "nothing to change"),
    ({"view": "Drawing View1", "display": "parent"}, "isn't derived from another view"),
]:
    r = mcp.call("edit_drawing_view", expect_error=True, **args)
    assert expected in r.text, (args, r.text)
after = views()
keys = ["orientation", "scale", "scale_from", "display", "configuration", "outline", "model"]
assert {n: {k: v.get(k) for k in keys} for n, v in after.items()} == \
    {n: {k: v.get(k) for k in keys} for n, v in before.items()}, (before, after)

# ---- Another sheet, in the background: Sheet1 stays active.
mcp.call("add_sheet")
mcp.call("activate_sheet", sheet="Sheet1")
# The drawing shows two models now (the plate since the replace): name one.
r = mcp.call("create_drawing_view", sheet="Sheet2", model="views-block", orientation="right", scale="1:1")
assert "on Sheet2" in r.text and "(Sheet1 stays the active sheet)" in r.text, r.text
s = sheets()
assert s["Sheet1"]["active"] and len(s["Sheet2"]["views"]) == 1, s
r = mcp.call("edit_drawing_view", view=s["Sheet2"]["views"][0]["name"], display="shaded")
assert "(Sheet1 stays the active sheet)" in r.text and sheets()["Sheet1"]["active"], r.text

mcp.call("render_drawing", max_px=1200)
assert not (open_dialogs := sw_dialogs()), open_dialogs

# ---- Not a drawing.
mcp.call("create_part")
r = mcp.call("create_drawing_view", expect_error=True)
assert "isn't a drawing" in r.text, r.text

# create_projected_view, create_section_view, create_detail_view,
# create_auxiliary_view, crop_view, break_view against real SolidWorks
# (docs/notes/drawings.md, "Derived views"): which way each looks, where each
# goes, sizes at scales other than 1:1, labels, refusals that leave nothing
# behind (a stray sketch entity would swell the parent's outline), an assembly
# view and a first-angle sheet. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/derivedviews.py

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


def name_of(reply):
    """The new view's name, from "created <name> on Sheet...: ..."."""
    return reply.text.split("created ", 1)[1].split(" on Sheet", 1)[0]


def hole_of(view):
    return [e for e in entities(view) if e["type"] == "circle"][0]


# ---- The part: x 0..20, y -60..60, z -20..20, its +y end inclined, a 12 mm
# hole through x at y = -20. Seen from the front +Y is right, +Z up, +X toward
# the viewer: on the sheet the hole is 20 mm left of the view's position.
mcp.call("create_part")
mcp.call("sketch_line", plane="front", points=[[-60, -20], [40, -20], [60, 20], [-60, 20]], closed=True)
mcp.call("extrude_sketch", depth_mm=20)
faces = mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]
side_face = [f["id"] for f in faces if f["normal"][0] > 0.99][0]
mcp.call("sketch_circle", on=side_face, center=[20, -20, 0], diameter_mm=12)
mcp.call("cut_extrude_sketch", through_all=True)
part = mcp.run_dir.resolve() / "derived-plate.SLDPRT"
mcp.call("save_document", path=str(part))

mcp.call("create_assembly")
mcp.call("insert_components", file=str(part))
mcp.call("insert_components", file=str(part), at=[{"origin_mm": [40, 0, 0]}])
assembly = mcp.run_dir.resolve() / "derived-pair.SLDASM"
mcp.call("save_document", path=str(assembly))
mcp.call("create_drawing", sheet_format="a3 - iso")

# ---- Projected views go where asked and show what third angle says.
mcp.call("create_drawing_view", model=str(part), at_mm=[150, 170], scale="1:1", name="P")
for side, toward in [("right", "+Y"), ("left", "-Y"), ("above", "+Z"), ("below", "-Z")]:
    r = mcp.call("create_projected_view", view="P", side=side)
    new = name_of(r)
    assert "projected from P" in r.text, r.text
    v = views()
    assert v[new]["type"] == "projected" and v[new]["from_view"] == "P" and v[new]["scale_from"] == "parent", v[new]
    assert v[new]["toward_viewer"] == toward, (side, v[new])
    assert not overlap(v[new], v["P"]), (side, v[new])
    px, py = center(v["P"])
    x, y = center(v[new])
    if side in ("right", "left"):
        assert abs(y - py) < 1 and ((x > px) == (side == "right")), (side, v[new])
    else:
        assert abs(x - px) < 1 and ((y > py) == (side == "above")), (side, v[new])
    no_others = [n for n in v if n != new and overlap(v[new], v[n])]
    assert not no_others, (new, no_others)
r = mcp.call("create_projected_view", view="P", at_mm=[390, 170], name="Far")
assert near(center(views()["Far"]), [390, 170], 1.0), views()["Far"]
for args, expected in [
    ({"view": "Nope"}, 'no view named "Nope"'),
    ({"view": "P", "side": "left", "at_mm": [1, 2]}, "side or at_mm"),
    ({"view": "P", "name": "Far"}, 'already has a view named "Far"'),
    ({"view": "P", "name": "a@b"}, '"a@b"'),
]:
    before = sorted(views())
    r = mcp.call("create_projected_view", expect_error=True, **args)
    assert expected in r.text, (args, r.text)
    assert sorted(views()) == before, (args, views().keys())

# ---- Section: a horizontal cut through the hole, the view below: it looks up.
# At 2:1, so a position error in the sketch would show.
mcp.call("add_sheet")
mcp.call("create_drawing_view", model=str(part), at_mm=[210, 200], scale="2:1", name="F")
f_outline = views("Sheet2")["F"]["outline"]
hole = hole_of("F")
assert near(hole["center"], [170, 200], 0.1), hole
r = mcp.call("create_section_view", view="F", through=hole["id"], angle_deg=0, side="below")
assert "section of F" in r.text and "scale 2:1 (from F)" in r.text, r.text
a = name_of(r)
v = views("Sheet2")
assert a == "Section View A-A" and v[a]["label"] == "A" and v[a]["type"] == "section", v[a]
assert v[a]["toward_viewer"] == "-Z" and center(v[a])[1] < 150, v[a]
assert not overlap(v[a], v["F"]), v[a]
assert v["F"]["outline"] == f_outline, "the sketch line must be gone into the section"
# It cuts the hole: two lines 24 mm apart on the sheet where the hole's walls are.
walls = [e for e in entities(a) if e["type"] == "line" and abs(e["start"][0] - e["end"][0]) < 0.01
         and abs(e["length_mm"] - 20) < 0.01]
xs = sorted({round(e["start"][0], 1) for e in walls})
assert any(abs(x - 158) < 0.2 for x in xs) and any(abs(x - 182) < 0.2 for x in xs), walls

# Vertical (the default) through the hole by a point, to each side.
left = name_of(mcp.call("create_section_view", view="F", through_mm=[170, 200], side="left"))
v = views("Sheet2")
assert left == "Section View B-B" and v[left]["label"] == "B" and v[left]["toward_viewer"] == "-Y", v[left]
assert center(v[left])[0] < v["F"]["outline"][0][0], v[left]
flipped = name_of(mcp.call("create_section_view", view="F", through_mm=[170, 200], flip_direction=True, label="K",
                           at_mm=[20, 200]))
v = views("Sheet2")
assert v[flipped]["label"] == "K" and v[flipped]["toward_viewer"] == "+Y", v[flipped]
# The default side: as much room either way here, so the right; it looks left.
cut = name_of(mcp.call("create_section_view", view="F", through_mm=[230, 200], surface_only=True))
v = views("Sheet2")
assert center(v[cut])[0] > v["F"]["outline"][1][0] and v[cut]["toward_viewer"] == "+Y", v[cut]
r = mcp.call("create_section_view", view="F", through_mm=[20, 280], side="right", depth_mm=5)
assert "may cut nothing" in r.text, r.text
for args, expected in [
    ({"view": "F", "side": "above"}, "nothing"),
    ({"view": "F", "through": "view_edge:9999"}, "view_edge:9999"),
    ({"view": "F", "label": "k"}, "already uses"),
    ({"view": "F", "depth_mm": -1}, "depth_mm"),
]:
    before = sorted(views("Sheet2"))
    r = mcp.call("create_section_view", expect_error=True, **args)
    assert expected in r.text, (args, r.text)
    after = views("Sheet2")
    assert sorted(after) == before and after["F"]["outline"] == f_outline, (args, after["F"])

# A section of a section (the position isn't the model origin there).
sx, sy = center(views("Sheet2")[left])
twice = name_of(mcp.call("create_section_view", view=left, through_mm=[sx, sy], angle_deg=0, side="below"))
assert views("Sheet2")[twice]["from_view"] == left, views("Sheet2")[twice]

# ---- Detail: the circle around the hole at 2:1, the view at 4:1 by default.
mcp.call("add_sheet")
mcp.call("create_drawing_view", model=str(part), at_mm=[150, 220], scale="2:1", name="D")
d_outline = views("Sheet3")["D"]["outline"]
hole = hole_of("D")
r = mcp.call("create_detail_view", view="D", center=hole["id"], radius_mm=20)
assert "detail of D" in r.text and "scale 4:1" in r.text, r.text
dv = name_of(r)
v = views("Sheet3")
assert v[dv]["type"] == "detail" and v[dv]["label"] and v[dv]["scale"] == "4:1", v[dv]
assert not overlap(v[dv], v["D"]), v[dv]
assert v["D"]["outline"] == d_outline, v["D"]
# 20 mm on the 2:1 parent = 40 mm across at 4:1 (80 + SolidWorks' margin).
assert all(80 <= s <= 110 for s in size(v[dv])), size(v[dv])
# The hole is in the middle of it, at its true size.
circle = hole_of(dv)
assert abs(circle["diameter_mm"] - 12) < 0.01 and near(circle["center"], center(v[dv]), 1.0), (circle, v[dv])
second = name_of(mcp.call("create_detail_view", view="D", center_mm=[190, 220], radius_mm=10, scale="3:1",
                          at_mm=[100, 80], style="no_leader", outline="jagged", label="Z"))
v = views("Sheet3")
assert v[second]["scale"] == "3:1" and v[second]["label"] == "Z", v[second]
assert near(center(v[second]), [100, 80], 1.5), v[second]
for args, expected in [
    ({"view": "D", "radius_mm": 0, "center_mm": [1, 2]}, "radius_mm"),
    ({"view": "D", "radius_mm": 5}, "center"),
    ({"view": "D", "radius_mm": 5, "center_mm": [150, 220], "label": "Z"}, "already uses"),
]:
    before = sorted(views("Sheet3"))
    r = mcp.call("create_detail_view", expect_error=True, **args)
    assert expected in r.text, (args, r.text)
    after = views("Sheet3")
    assert sorted(after) == before and after["D"]["outline"] == d_outline, (args, after["D"])

# ---- Auxiliary: square onto the inclined end, whose face is 44.72 x 20.
mcp.call("add_sheet")
mcp.call("create_drawing_view", model=str(part), at_mm=[150, 170], scale="1:1", name="G")
edges = entities("G")
inclined = [e for e in edges if e["type"] == "line" and abs(e["length_mm"] - 44.7214) < 0.01][0]
level = [e for e in edges if e["type"] == "line" and abs(e["length_mm"] - 120) < 0.01][0]
circle = [e for e in edges if e["type"] == "circle"][0]
r = mcp.call("create_auxiliary_view", view="G", edge=inclined["id"])
assert "auxiliary view of G" in r.text, r.text
aux = name_of(r)
v = views("Sheet4")
assert v[aux]["type"] == "auxiliary" and v[aux]["from_view"] == "G" and "model_axes_on_sheet" in v[aux], v[aux]
assert not overlap(v[aux], v["G"]), v[aux]
# Out past the inclined end: the outward normal is (0.894, -0.447) on the sheet.
gx, gy = center(v["G"])
ax, ay = center(v[aux])
assert (ax - gx) * 0.894 - (ay - gy) * 0.447 > 0, (v[aux], v["G"])
# It stays on the normal through the parent.
assert abs((ax - gx) * 0.447 + (ay - gy) * 0.894) < 3, (v[aux], v["G"])
lengths = sorted(round(e["length_mm"], 1) for e in entities(aux) if e["type"] == "line")
assert 44.7 in lengths and 20.0 in lengths, lengths
r = mcp.call("create_auxiliary_view", view="G", edge=inclined["id"], flip=True, name="Other")
v = views("Sheet4")
ax2, ay2 = center(v["Other"])
assert (ax2 - gx) * 0.894 - (ay2 - gy) * 0.447 < 0, (v["Other"], v["G"])
for args, expected in [
    ({"view": "G", "edge": level["id"]}, "create_projected_view"),
    ({"view": "G", "edge": circle["id"]}, "straight edge"),
    ({"view": "G", "edge": "view_edge:9999"}, "view_edge:9999"),
    ({"view": "P", "edge": inclined["id"]}, "is in G"),
]:
    before = sorted(views("Sheet4"))
    r = mcp.call("create_auxiliary_view", expect_error=True, **args)
    assert expected in r.text, (args, r.text)
    assert sorted(views("Sheet4")) == before, args

# ---- Crop at 1:2: to a rectangle, replaced by a circle, taken off.
mcp.call("add_sheet")
mcp.call("create_drawing_view", model=str(part), at_mm=[100, 200], scale="1:2", name="C")
whole = views("Sheet5")["C"]["outline"]
r = mcp.call("crop_view", view="C", rectangle_mm=[[80, 190], [110, 210]])
assert "cropped C" in r.text, r.text
v = views("Sheet5")["C"]
# 30 x 20 on the sheet around [95, 200], with the margin SolidWorks keeps round it (~9 mm seen).
assert v["cropped"] is True and near(center(v), [95, 200], 1.0), v
assert 30 <= size(v)[0] <= 50 and 20 <= size(v)[1] <= 40, size(v)
print("    cropped entities:", [e["type"] for e in entities("C")])
# Replaced: the rectangle's lines mustn't stay behind (they'd widen the outline).
r = mcp.call("crop_view", view="C", circle_center_mm=[120, 200], circle_radius_mm=8)
v = views("Sheet5")["C"]
assert v["cropped"] is True and near(center(v), [120, 200], 1.0) and size(v)[0] < 40, v
# A jagged outline wanders round the circle, and its margin with it.
r = mcp.call("crop_view", view="C", circle_center_mm=[100, 200], circle_radius_mm=8, outline="jagged")
assert views("Sheet5")["C"]["cropped"] is True
r = mcp.call("crop_view", view="C", remove=True)
assert "took the crop off C" in r.text, r.text
v = views("Sheet5")["C"]
assert "cropped" not in v and near(v["outline"][0], whole[0]) and near(v["outline"][1], whole[1]), v
r = mcp.call("crop_view", view="C", remove=True, expect_error=True)
assert "isn't cropped" in r.text, r.text
# A line of the user's in the view's sketch stays when a crop comes and goes.
com("""$d = Com $sw ActiveDoc -Get
Com $d ActivateView @('C') | Out-Null
$sm = Com $d SketchManager -Get
if (-not (Com $sm CreateLine @([double]0.02, [double]0.02, [double]0, [double]0.03, [double]0.02, [double]0))) { throw 'line' }
Com $d ClearSelection2 @($true) | Out-Null
Com $d ActivateView @('') | Out-Null""")
count = "$v = Com $d GetFirstView; while ((Com $v GetName2) -ne 'C') { $v = Com $v GetNextView }; @(Com (Com $v GetSketch) GetSketchSegments).Count"
counted = lambda: com("$d = Com $sw ActiveDoc -Get; " + count)
assert counted() == "1"
mcp.call("crop_view", view="C", circle_center_mm=[120, 200], circle_radius_mm=8)
mcp.call("crop_view", view="C", remove=True)
assert counted() == "1"

# ---- Break at 2:1: along x, shortened, dimensions true; then a second one
# along y; refusals; taken off.
mcp.call("create_drawing_view", model=str(part), at_mm=[260, 100], scale="2:1", name="B")
whole = views("Sheet5")["B"]
r = mcp.call("break_view", view="B", breaks=[{"along": "x", "from_mm": 240, "to_mm": 300}])
assert "broke B" in r.text and "unbroken" in r.text, r.text
v = views("Sheet5")["B"]
assert v["breaks"] == [{"along": "x", "from_mm": 240.0, "to_mm": 300.0, "style": "zigzag"}], v["breaks"]
assert "broken_note" in v, v
# 60 mm hidden less the 10 mm gap SolidWorks leaves.
assert abs((size(whole)[0] - size(v)[0]) - 50) < 1.5, (size(whole), size(v))
long_edge = [e for e in entities("B") if e["type"] == "line" and abs(e["length_mm"] - 120) < 0.01][0]
r = mcp.call("add_drawing_dimensions", dimensions=[{"kind": "length", "entities": [long_edge["id"]]}])
assert "= 120 mm" in r.text, r.text
r = mcp.call("break_view", view="B", breaks=[{"along": "y", "from_mm": 90, "to_mm": 110}], style="straight")
v = views("Sheet5")["B"]
assert len(v["breaks"]) == 2 and {b["along"] for b in v["breaks"]} == {"x", "y"}, v["breaks"]
assert abs((size(whole)[1] - size(v)[1]) - 10) < 1.5, (size(whole), size(v))
r = mcp.call("crop_view", view="B", circle_center_mm=[260, 100], circle_radius_mm=10, expect_error=True)
assert "broken" in r.text, r.text
assert views("Sheet5")["B"]["outline"] == v["outline"]
mcp.call("render_drawing", max_px=1400)
r = mcp.call("break_view", view="B", remove=True)
assert "took the breaks off B" in r.text, r.text
v = views("Sheet5")["B"]
assert "breaks" not in v and "broken_note" not in v and near(v["outline"][1], whole["outline"][1], 2), v
r = mcp.call("break_view", view="B", remove=True, expect_error=True)
assert "no breaks" in r.text, r.text
mcp.call("crop_view", view="B", circle_center_mm=[260, 100], circle_radius_mm=20)
r = mcp.call("break_view", view="B", breaks=[{"along": "x", "from_mm": 250, "to_mm": 270}], expect_error=True)
assert "cropped" in r.text, r.text
mcp.call("crop_view", view="B", remove=True)
r = mcp.call("crop_view", view=dv, circle_center_mm=[300, 80], circle_radius_mm=5, expect_error=True)
assert "detail or broken" in r.text, r.text
for tool, args, expected in [
    ("crop_view", {"view": "B"}, "what to crop"),
    ("break_view", {"view": "B"}, "ranges"),
    ("crop_view", {"view": "Nope", "remove": True}, 'no view named "Nope"'),
]:
    r = mcp.call(tool, expect_error=True, **args)
    assert expected in r.text, (tool, args, r.text)

# ---- On a sheet in the background: the user's stays.
mcp.call("activate_sheet", sheet="Sheet1")
r = mcp.call("create_section_view", view="B", angle_deg=0, side="above")
assert "(Sheet1 stays the active sheet)" in r.text, r.text
assert sheets()["Sheet1"]["active"]

# ---- Assembly view: every component cut, no dialog.
mcp.call("add_sheet")
mcp.call("create_drawing_view", model=str(assembly), at_mm=[120, 200], scale="1:1", name="A1")
asect = name_of(mcp.call("create_section_view", view="A1", angle_deg=90, through_mm=[100, 200], side="right"))
v = views("Sheet6")
assert v[asect]["type"] == "section" and v[asect]["toward_viewer"] == "+Y", v[asect]
assert not (open_dialogs := sw_dialogs()), open_dialogs

# ---- First angle: a view to the right looks right, a projected view right of
# the front is seen from the left.
mcp.call("add_sheet", projection="first_angle")
mcp.call("create_drawing_view", model=str(part), at_mm=[150, 170], scale="1:1", name="FA")
fasect = name_of(mcp.call("create_section_view", view="FA", through_mm=[110, 170], side="right"))
v = views("Sheet7")
assert v[fasect]["toward_viewer"] == "-Y", v[fasect]
assert center(v[fasect])[0] > v["FA"]["outline"][1][0], v[fasect]
r = mcp.call("create_auxiliary_view", view="FA", edge=[e for e in entities("FA")
             if e["type"] == "line" and abs(e["length_mm"] - 44.7214) < 0.01][0]["id"], name="FAAux")
v = views("Sheet7")
ax, ay = center(v["FAAux"])
gx, gy = center(v["FA"])
assert (ax - gx) * 0.894 - (ay - gy) * 0.447 < 0, (v["FAAux"], v["FA"])

for sheet in ["Sheet1", "Sheet2", "Sheet3", "Sheet4", "Sheet5"]:
    mcp.call("render_drawing", sheet=sheet, max_px=1400)
assert not (open_dialogs := sw_dialogs()), open_dialogs

# ---- Not a drawing.
mcp.call("create_part")
r = mcp.call("create_projected_view", view="P", expect_error=True)
assert "isn't a drawing" in r.text, r.text

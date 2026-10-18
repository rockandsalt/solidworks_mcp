# inspect_drawing / add_drawing_dimensions / insert_model_items against real
# SolidWorks (docs/notes/drawings.md). No MCP tool makes drawings, so the
# part is saved and the drawing, its views and the assembly are made through
# COM (devloop/swcom.ps1). Run with:
#   py devloop/mcp_client.py run devloop/scenarios/drawings.py

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
    print(f"    com {script.name}: {p.stdout.strip()}")
    assert p.returncode == 0, f"{script.name} failed: {p.stderr.strip()}"
    return p.stdout.strip()


def dump(name, value):
    (mcp.run_dir / f"{name}.json").write_text(json.dumps(value, indent=1), encoding="utf-8")


# ---- The part ---------------------------------------------------------------
# 40 x 20 x 10 box (this machine's template: x 0..10, y -20..20, z -10..10),
# a 6 mm boss 8 mm tall on the z=10 face at y=0 (axis along Z), and a 6 mm
# hole through the box along X at y=10.
mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
faces = mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]


def face_at(axis: int, value: float) -> str:
    hits = [f["id"] for f in faces
            if abs(f["normal"][axis]) > 0.99 and abs(f["point_on_plane_mm"][axis] - value) < 1e-6]
    assert len(hits) == 1, (axis, value, faces)
    return hits[0]


far_cap, top = face_at(0, 10), face_at(2, 10)
mcp.call("sketch_circle", on=top, center=[5, 0, 10], diameter_mm=6)
mcp.call("extrude_sketch", depth_mm=8)
mcp.call("sketch_circle", on=far_cap, center=[10, 10, 0], diameter_mm=6)
mcp.call("cut_extrude_sketch", through_all=True)
part = mcp.run_dir / "drawing-block.SLDPRT"
com(f"""$d = Com $sw ActiveDoc -Get
$e = Com $d SaveAs3 @('{part}', 0, 1); if ($e -ne 0) {{ throw "SaveAs3: $e" }}""")

# ---- The assembly: two of them, the second 60 mm off along y and z ----------
assembly = mcp.run_dir / "drawing-pair.SLDASM"
com(f"""$template = Com $sw GetUserPreferenceStringValue @(9)  # swDefaultTemplateAssembly
$a = Com $sw NewDocument @($template, 0, 0, 0); if (-not $a) {{ throw "NewDocument($template) failed" }}
foreach ($o in 0.0, 0.06) {{
    if (-not (Com $a AddComponent5 @('{part}', 0, '', $false, '', 0.0, $o, $o))) {{ throw 'AddComponent5 failed' }}
}}
$e = Com $a SaveAs3 @('{assembly}', 0, 1); if ($e -ne 0) {{ throw "SaveAs3: $e" }}""")

# ---- The drawing ------------------------------------------------------------
# Sheet1: Front at 1:1 at (100, 150) mm, Top at 1:2 at (250, 150) mm.
# Sheet2: the assembly's Front at (150, 150) mm. Sheet1 stays active.
com(f"""function SetProp($o, $name, $value) {{
    [System.__ComObject].InvokeMember($name, [Reflection.BindingFlags]'SetProperty', $null, $o, @($value)) }}
$template = Com $sw GetUserPreferenceStringValue @(10)  # swDefaultTemplateDrawing
$d = Com $sw NewDocument @($template, 12, 0.42, 0.297); if (-not $d) {{ throw "NewDocument($template) failed" }}
$v1 = Com $d CreateDrawViewFromModelView3 @('{part}', '*Front', 0.1, 0.15, 0.0); if (-not $v1) {{ throw 'Front view failed' }}
$v2 = Com $d CreateDrawViewFromModelView3 @('{part}', '*Top', 0.25, 0.15, 0.0); if (-not $v2) {{ throw 'Top view failed' }}
foreach ($v in $v1, $v2) {{ SetProp $v UseSheetScale $false }}
SetProp $v1 ScaleDecimal ([double]1)
SetProp $v2 ScaleDecimal ([double]0.5)
"views: $(Com $v1 Name -Get) $(Com $v1 ScaleDecimal -Get), $(Com $v2 Name -Get) $(Com $v2 ScaleDecimal -Get)"
if (-not (Com $d NewSheet3 @('Sheet2', 12, 13, 1.0, 1.0, $true, '', 0.42, 0.297, ''))) {{ throw 'NewSheet3 failed' }}
$v3 = Com $d CreateDrawViewFromModelView3 @('{assembly}', '*Front', 0.15, 0.15, 0.0); if (-not $v3) {{ throw 'assembly view failed' }}
"asm view: $(Com $v3 Name -Get)"
if (-not (Com $d ActivateSheet @('Sheet1'))) {{ throw 'ActivateSheet failed' }}
Com $d ForceRebuild3 @($false) | Out-Null""")

overview = mcp.call("inspect_drawing").json
dump("overview", overview)


def view_json(name, **kw):
    d = mcp.call("inspect_drawing", view=name, **kw).json
    dump(name.replace(" ", ""), d)
    return [v for s in d["sheets"] for v in s["views"] if v["name"] == name][0]


def pick(entities, type_, **where):
    hits = [e for e in entities if e["type"] == type_ and all(e.get(k) == v for k, v in where.items())]
    assert hits, (type_, where, entities)
    return hits[0]["id"]


def sw_dialogs() -> list[str]:
    """Titles of the dialogs SolidWorks has open (modeless ones don't block the add-in)."""
    p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
                        str(SWCOM.parent / "sw.ps1"), "dialogs"], capture_output=True, text=True, timeout=60)
    return [line.strip() for line in p.stdout.splitlines() if line.strip()]


def of_type(view, type_):
    return [e for e in view["entities"] if e["type"] == type_]


def dims_by_name(view):
    return {a["name"]: a for a in view.get("annotations", []) if a["type"] == "dimension"}


def active_sheets():
    return [s["name"] for s in mcp.call("inspect_drawing").json["sheets"] if s.get("active")]


# ---- inspect_drawing: sheets ----------------------------------------------------
# ISheet::GetProperties2: the template's A3 sheet at 2:1, third angle; Sheet2
# as NewSheet3 made it.
sheets = {s["name"]: s for s in overview["sheets"]}
assert sheets["Sheet1"]["active"] and not sheets["Sheet2"].get("active"), overview
assert sheets["Sheet1"]["paper"] == "A3" and sheets["Sheet1"]["size_mm"] == [420.0, 297.0], overview
assert (sheets["Sheet1"]["scale"], sheets["Sheet1"]["projection"]) == ("2:1", "third_angle"), overview
assert (sheets["Sheet2"]["scale"], sheets["Sheet2"]["projection"]) == ("1:1", "first_angle"), overview

# ---- inspect_drawing: view entities -----------------------------------------
# Front (looking along -X: sheet right = +Y, up = +Z) at 1:1, centered on
# (100, 150): the box's z -10..10 lands on y 136..156, the boss reaches 164.
v1 = view_json("Drawing View1", include_vertices=True)
assert (v1["sheet_right"], v1["sheet_up"], v1["scale"]) == ("+Y", "+Z", "1:1"), v1
e1 = v1["entities"]
bottom = pick(e1, "line", start=[80.0, 136.0], length_mm=40.0)
side = pick(e1, "line", start=[80.0, 156.0], length_mm=20.0)
top_edge = pick(e1, "line", start=[120.0, 156.0], length_mm=40.0)
hole = pick(e1, "circle", center=[110.0, 146.0], diameter_mm=6.0)
boss_top = pick(e1, "circle_seen_edge_on", diameter_mm=6.0)
# The boss's sides. GetVisibleEntities2 gave this (rescaled) view's
# silhouettes without face or end points.
sil = of_type(v1, "silhouette")
assert sorted((s["start"][0], s["length_mm"]) for s in sil) == [(97.0, 8.0), (103.0, 8.0)], sil
assert all(sorted([s["start"][1], s["end"][1]]) == [156.0, 164.0] for s in sil), sil
# Vertices land where the edges do (GetPoint alone is off by the view's
# position).
assert sorted(tuple(v["at"]) for v in of_type(v1, "vertex")) == \
    [(80.0, 136.0), (80.0, 156.0), (120.0, 136.0), (120.0, 156.0)], of_type(v1, "vertex")
# The center mark SolidWorks put on the hole is attached to it.
marks = [a for a in v1["annotations"] if a["type"] == "center_mark"]
assert marks and marks[0]["attached"] == [hole], v1["annotations"]
# Same ids when listed again.
assert view_json("Drawing View1", include_vertices=True)["entities"] == e1

# Top (looking along -Z: right = +Y, up = -X) at 1:2 around (250, 150):
# ModelToViewTransform includes the view's scale.
v2 = view_json("Drawing View2", include_vertices=True)
assert (v2["sheet_right"], v2["sheet_up"], v2["scale"]) == ("+Y", "-X", "1:2"), v2
long_edge = pick(v2["entities"], "line", start=[260.0, 147.5], end=[240.0, 147.5], length_mm=40.0)
pick(v2["entities"], "line", start=[240.0, 147.5], length_mm=10.0)
boss_circle = pick(v2["entities"], "circle", center=[250.0, 150.0], diameter_mm=6.0)
assert sorted(tuple(v["at"]) for v in of_type(v2, "vertex")) == \
    [(240.0, 147.5), (240.0, 152.5), (260.0, 147.5), (260.0, 152.5)], of_type(v2, "vertex")

# The assembly's Front on Sheet2, which isn't showing (the add-in shows it
# for the call). The view puts the assembly origin at (120, 120);
# AddComponent5 centered the blocks' boxes on (0, 0, 0) and (0, 60, 60), so
# their origins are at (-5, 0, -4) and (-5, 60, 56). Edges come in component
# space and need each component's placement.
v3 = view_json("Drawing View3", include_vertices=True)
assert len(v3["entities"]) == 2 * 12, v3["entities"]
asm_long = [e for e in of_type(v3, "line") if e["length_mm"] == 40.0]
assert sorted(e["start"][1] for e in asm_long) == [106.0, 126.0, 166.0, 186.0], asm_long
assert sorted(tuple(e["center"]) for e in of_type(v3, "circle")) == [(130.0, 116.0), (190.0, 176.0)], v3
assert sorted(s["start"][0] for s in of_type(v3, "silhouette")) == [117.0, 123.0, 177.0, 183.0], v3
assert sorted(tuple(v["at"]) for v in of_type(v3, "vertex"))[:2] == [(100.0, 106.0), (100.0, 126.0)], v3
assert active_sheets() == ["Sheet1"]

# ---- add_drawing_dimensions ---------------------------------------------------
r = mcp.call("add_drawing_dimensions", dimensions=[
    {"kind": "horizontal", "entities": [bottom]},
    {"kind": "vertical", "entities": [side]},
    {"kind": "diameter", "entities": [hole]},
    {"kind": "length", "entities": [top_edge], "at_mm": [100, 180]},
    {"kind": "angle", "entities": [bottom, side]},
    {"kind": "distance", "entities": [s["id"] for s in sil]},  # selecting silhouettes
])
assert r.text.startswith("added 6 dimension(s)"), r.text
r = mcp.call("add_drawing_dimensions", dimensions=[
    {"kind": "length", "entities": [long_edge], "at_mm": [250, 135]},
    {"kind": "diameter", "entities": [boss_circle]},
])
assert r.text.startswith("added 2 dimension(s)"), r.text
# The upper block's top edge, on the sheet that isn't showing.
upper = [e for e in asm_long if e["start"][1] == 186.0][0]["id"]
r = mcp.call("add_drawing_dimensions", dimensions=[{"kind": "length", "entities": [upper]}])
assert r.text.startswith("added 1 dimension(s)"), r.text

# What they measure, where their text is (sheet space, as at_mm) and what
# they're attached to.
d1 = dims_by_name(view_json("Drawing View1"))
expect = {"RD1": (40.0, [bottom]), "RD2": (20.0, [side]), "RD3": (6.0, [hole]), "RD4": (40.0, [top_edge])}
for name, (value, attached) in expect.items():
    assert d1[name]["value_mm"] == value and d1[name]["attached"] == attached, (name, d1[name])
# Silhouettes aren't edges, and the two of one face only differ by ordinal:
# not mapped back to ids. SolidWorks shows this one as a diameter.
assert d1["RD6"]["value_mm"] == 6.0 and d1["RD6"]["attached_other"] == 2, d1["RD6"]
assert d1["RD5"]["value_deg"] == 90.0 and sorted(d1["RD5"]["attached"]) == sorted([bottom, side]), d1["RD5"]
assert d1["RD4"]["position"] == [100.0, 180.0], d1["RD4"]
assert d1["RD1"]["position"][1] < 136 and d1["RD2"]["position"][0] < 80, d1  # outside the view
d2 = dims_by_name(view_json("Drawing View2"))
assert d2["RD1"]["value_mm"] == 40.0 and d2["RD1"]["position"] == [250.0, 135.0], d2
assert d2["RD1"]["attached"] == [long_edge] and d2["RD2"]["attached"] == [boss_circle], d2
d3 = dims_by_name(view_json("Drawing View3"))
assert d3["RD1"]["value_mm"] == 40.0 and d3["RD1"]["attached"] == [upper], d3
assert d3["RD1"]["position"][1] > 186.0, d3  # above the upper block, where the default puts it
assert active_sheets() == ["Sheet1"]

# One SolidWorks can't make (the vertical extent of a horizontal edge) takes
# the call's other dimension with it, with no dialog.
r = mcp.call("add_drawing_dimensions", expect_error=True, dimensions=[
    {"kind": "horizontal", "entities": [top_edge]},
    {"kind": "vertical", "entities": [bottom]},
])
print("    impossible:", r.text)
assert dims_by_name(view_json("Drawing View1")).keys() == d1.keys()
assert not sw_dialogs(), sw_dialogs()

# ---- insert_model_items --------------------------------------------------------
# Only the selected view: the box's depth (along X) shows in the Top view.
r = mcp.call("insert_model_items", views=["Drawing View2"])
print("    model items View2:", r.text)
assert r.json["inserted"] == 1 and [v["view"] for v in r.json["views"]] == ["Drawing View2"], r.json
assert [a["dimension"] for a in r.json["views"][0]["added"]] == ["D1@Boss-Extrude1@drawing-block.Part"], r.json
# Every view of the active sheet: the boss's height shows in the Front view,
# the box's depth isn't repeated.
r = mcp.call("insert_model_items")
print("    model items all:", r.text)
added = {v["view"]: [a["dimension"] for a in v["added"]] for v in r.json["views"]}
assert added == {"Drawing View1": ["D1@Boss-Extrude2@drawing-block.Part"], "Drawing View2": []}, added
# ... and not Sheet2's view.
final = mcp.call("inspect_drawing", list_annotations=True).json
dump("final", final)
v3 = [v for s in final["sheets"] for v in s["views"] if v["name"] == "Drawing View3"][0]
assert v3["annotation_counts"] == {"dimension": 1}, v3
assert not sw_dialogs(), sw_dialogs()
# ... though it had items to bring in (so AllViews=TRUE stays on the active
# sheet), on a sheet that isn't showing.
r = mcp.call("insert_model_items", views=["Drawing View3"])
print("    model items View3:", r.text)
assert r.json["inserted"] > 0 and r.json["views"][0]["added"], r.json
assert active_sheets() == ["Sheet1"]

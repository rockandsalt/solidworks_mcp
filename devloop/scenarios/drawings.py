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

# ---- set_dimension_tolerance ----------------------------------------------------
def tol(**kw):
    r = mcp.call("set_dimension_tolerance", **kw)
    print(f"    tolerance {kw['name']}: {r.text}")
    return r.json


# Bilateral on a reference dimension, by its short name; inspect_drawing reads
# back what the reply says.
r = tol(name="RD1@Drawing View1", tolerance="bilateral", upper_mm=0.1, lower_mm=-0.05, tolerance_decimals=2)
assert r["view"] == "Drawing View1" and r["value_mm"] == 40.0, r
assert r["tolerance"] == {"type": "bilateral", "upper_mm": 0.1, "lower_mm": -0.05, "limits_mm": [39.95, 40.1]}, r
assert r["tolerance_decimals"] == 2 and "notes" not in r, r
d1 = dims_by_name(view_json("Drawing View1"))
assert d1["RD1"]["tolerance"] == r["tolerance"], d1["RD1"]
# Symmetric on the hole's diameter, by its full name; an angle in degrees.
r = tol(name=d1["RD3"]["dimension"], tolerance="symmetric", plus_minus_mm=0.02)
assert r["tolerance"]["limits_mm"] == [5.98, 6.02], r
r = tol(name="RD5@Drawing View1", tolerance="symmetric", plus_minus_deg=0.5)
assert r["tolerance"]["plus_minus_deg"] == 0.5, r
r = mcp.call("set_dimension_tolerance", expect_error=True, name="RD5@Drawing View1", tolerance="symmetric",
             plus_minus_mm=0.5)
print("    mm on an angle:", r.text)
# A fit on the boss's diameter in the 1:2 view; limit; then none removes it.
r = tol(name="RD2@Drawing View2", tolerance="fit", shaft_fit="g6")
assert r["tolerance"]["type"] == "fit" and r["tolerance"].get("shaft_fit") == "g6", r
r = tol(name="RD2@Drawing View2", tolerance="limit", upper_mm=0.0, lower_mm=-0.1)
assert r["tolerance"]["limits_mm"] == [5.9, 6.0], r
r = tol(name="RD2@Drawing View2", tolerance="none")
assert "tolerance" not in r, r
# RD1 is in Drawing View1, View2 and View3: the shortest name fits all of them.
r = mcp.call("set_dimension_tolerance", expect_error=True, name="RD1", tolerance="basic")
print("    ambiguous:", r.text)
assert "several" in r.text, r.text
r = mcp.call("set_dimension_tolerance", expect_error=True, name="RD99@Drawing View1", tolerance="basic")
assert "no dimension" in r.text, r.text

# The model's own dimension in a drawing: the boss height. The part shows it.
model_dim = "D1@Boss-Extrude2@drawing-block.Part"
r = tol(name=model_dim, tolerance="bilateral", upper_mm=0.2, lower_mm=0.0)
assert r["view"] == "Drawing View1" and r["value_mm"] == 8.0 and r["feature"] == "Boss-Extrude2", r
assert r["tolerance"]["limits_mm"] == [8.0, 8.2] and r.get("notes"), r
# Insert_model_items put it in the assembly's view too: same dimension.
assert r.get("also_in_views") == ["Drawing View3"], r
d3 = [a for a in dims_by_name(view_json("Drawing View3")).values() if a.get("dimension") == model_dim]
assert d3 and d3[0]["tolerance"]["limits_mm"] == [8.0, 8.2], d3
drawing_title = com("""Com (Com $sw ActiveDoc -Get) GetTitle""")
com(f"""if (-not (Com $sw ActivateDoc @('{part.name}'))) {{ throw 'ActivateDoc part failed' }}""")
# Feature dimensions that aren't displayed aren't annotations
# (inspect_annotations doesn't see them): read the part's through COM.
part_tol = com("""$dim = Com (Com $sw ActiveDoc -Get) Parameter @('D1@Boss-Extrude2')
$t = Com $dim Tolerance -Get
"$(Com $t Type -Get) $(Com $t GetMaxValue) $(Com $t GetMinValue)"
""")
kind_, upper_, lower_ = part_tol.split()
assert kind_ == "2" and abs(float(upper_) - 0.0002) < 1e-12 and abs(float(lower_)) < 1e-12, part_tol
# ... and in the part itself, by the part's name for it.
r = tol(name="D1@Boss-Extrude1", tolerance="symmetric", plus_minus_mm=0.1, decimals=1)
assert r["value_mm"] == 10.0 and r["tolerance"]["limits_mm"] == [9.9, 10.1] and r["decimals"] == 1, r
com(f"""if (-not (Com $sw ActivateDoc @('{drawing_title}')) -and -not (Com $sw ActivateDoc @('{drawing_title.split(' - ')[0]}'))) {{
    throw 'ActivateDoc drawing failed' }}""")
d2 = dims_by_name(view_json("Drawing View2"))
depth = [a for a in d2.values() if a.get("dimension") == "D1@Boss-Extrude1@drawing-block.Part"]
assert depth and depth[0]["tolerance"]["limits_mm"] == [9.9, 10.1], d2
assert not sw_dialogs(), sw_dialogs()
# ---- render_drawing --------------------------------------------------------------
import re
import struct
import zlib


def read_png(path):
    """(width, height, rows of RGB tuples) of an 8-bit RGB/RGBA PNG (stdlib only)."""
    data = Path(path).read_bytes()
    pos, idat, w = 8, b"", 0
    while pos < len(data):
        n, kind = struct.unpack(">I4s", data[pos:pos + 8])
        body = data[pos + 8:pos + 8 + n]
        if kind == b"IHDR":
            w, h, depth, color = struct.unpack(">IIBB", body[:10])
            assert depth == 8 and color in (2, 6), (depth, color)
            bpp = 3 if color == 2 else 4
        elif kind == b"IDAT":
            idat += body
        pos += 12 + n
    raw, stride, rows, prev = zlib.decompress(idat), w * bpp, [], bytearray(w * bpp)
    for y in range(h):
        f, line = raw[y * (stride + 1)], bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
        for i in range(stride):
            a = line[i - bpp] if i >= bpp else 0
            b, c = prev[i], prev[i - bpp] if i >= bpp else 0
            if f == 1: line[i] = (line[i] + a) & 255
            elif f == 2: line[i] = (line[i] + b) & 255
            elif f == 3: line[i] = (line[i] + (a + b) // 2) & 255
            elif f == 4:
                pa, pb, pc = abs(b - c), abs(a - c), abs(a + b - 2 * c)
                line[i] = (line[i] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
        rows.append([tuple(line[i:i + 3]) for i in range(0, stride, bpp)])
        prev = line
    return w, h, rows


def render(**kw):
    r = mcp.call("render_drawing", **kw)
    print(f"    render {kw}: {r.text.splitlines()[0]}")
    assert len(r.images) == 1, r.text
    m = re.search(r"x ([\d.]+)\.\.([\d.]+) mm, y ([\d.]+)\.\.([\d.]+) mm: (\d+) x (\d+) px", r.text)
    x0, x1, y0, y1, w, h = map(float, m.groups())
    pw, ph, rows = read_png(r.images[0])
    assert (pw, ph) == (w, h), (pw, ph, r.text)

    def dark_near(x_mm, y_mm, reach=3):
        """Whether the drawing is drawn (dark pixel) near sheet point (x, y) mm."""
        px = 60 + (x_mm - x0) / (x1 - x0) * (w - 60)
        py = (y1 - y_mm) / (y1 - y0) * (h - 30)
        return any(sum(rows[j][i]) < 3 * 160
                   for j in range(max(0, int(py) - reach), min(int(h) - 30, int(py) + reach + 1))
                   for i in range(max(60, int(px) - reach), min(int(w), int(px) + reach + 1)))
    return r, dark_near


# The whole active sheet: View1's box (x 80..120, y 136..156 mm) is where the
# rulers say, empty paper next to it isn't.
r, dark_near = render()
assert "Sheet1 of" in r.text and "Drawing View1: x" in r.text, r.text
assert dark_near(100, 136) and dark_near(80, 146) and dark_near(120, 146), r.text
assert not dark_near(160, 146, reach=2), "paper between the views should be blank"
# Zoomed on View1 (its outline + 20 mm): sharper, same coordinates.
r, dark_near = render(view="Drawing View1")
assert dark_near(100, 136, reach=2) and dark_near(110, 149, reach=2), r.text  # bottom edge, top of the hole
assert not dark_near(88, 140, reach=2), r.text  # inside the box, inside the angle's arc
# A view on Sheet2 (the sheet is shown for the call, then put back).
r, dark_near = render(view="Drawing View3")
assert "Sheet2 of" in r.text and dark_near(180, 166, reach=2), r.text  # upper block's bottom edge
assert active_sheets() == ["Sheet1"]
# A region of the 1:2 Top view: its 40 mm edge is 20 mm long at y 147.5.
r, dark_near = render(region_mm=[[235, 140], [265, 160]], max_px=800)
assert dark_near(250, 147.5, reach=2) and not dark_near(250, 143, reach=1), r.text
r = mcp.call("render_drawing", expect_error=True, sheet="Sheet9")
assert "no sheet" in r.text, r.text
assert not sw_dialogs(), sw_dialogs()

# Saved, so the next run's reset doesn't refuse over its unsaved tolerances.
com(f"""$p = Com $sw GetOpenDocumentByName @('{part}'); if ($p) {{ Com $p Save2 @($true) | Out-Null }}""")

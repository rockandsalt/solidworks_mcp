# Title blocks, borders, tables and balloons against real SolidWorks
# (docs/notes/drawings.md, "Tables and balloons", "Borders and title
# blocks"), in one run: a plate with four holes, a block, an assembly of the
# plate and two blocks (made through COM); a drawing with the assembly and
# the plate; a border and a title block drawn into the sheet format and
# filled in (drawing and model properties); a BOM placed clear of
# everything, auto and per-component balloons showing its item numbers; a
# hole table of the plate's holes; a general table edited and deleted;
# refusals that change nothing; a stock sheet format's title block filled
# in; custom properties of a part the BOM lists. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/tables.py

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


def save(path):
    com(f"""$d = Com $sw ActiveDoc -Get
$e = Com $d SaveAs3 @('{path}', 0, 1); if ($e -ne 0) {{ throw "SaveAs3: $e" }}""")


def last_json(text):
    return json.loads(text.splitlines()[-1])


def tables(**kw):
    return {t["name"]: t for s in mcp.call("inspect_drawing_tables", **kw).json["sheets"] for t in s["tables"]}


def inside(box, lo, hi, tol=0.05):
    (x0, y0), (x1, y1) = box
    return x0 >= lo[0] - tol and y0 >= lo[1] - tol and x1 <= hi[0] + tol and y1 <= hi[1] + tol


def overlaps(a, b):
    (ax0, ay0), (ax1, ay1) = a
    (bx0, by0), (bx1, by1) = b
    return ax0 < bx1 and bx0 < ax1 and ay0 < by1 and by0 < ay1


# ---- Fixture -------------------------------------------------------------------
# Plate 60 x 40 x 5 (x 0..5, y -30..30, z -20..20) with four 6 mm holes along X
# at (y, z) = (+-20, +-10); block 20 x 10 x 10; assembly: plate + 2 blocks.
mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=60, height_mm=40)
mcp.call("extrude_sketch", depth_mm=5)
faces = mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]
cap = [f["id"] for f in faces if abs(f["normal"][0]) > 0.99 and abs(f["point_on_plane_mm"][0] - 5) < 1e-6][0]
mcp.call("sketch_circle", on=cap, center=[5, -20, -10], diameter_mm=6)
for y, z in ((20, -10), (-20, 10), (20, 10)):
    mcp.call("sketch_circle", sketch_name="Sketch2", center=[5, y, z], diameter_mm=6)
mcp.call("cut_extrude_sketch", through_all=True)
plate = mcp.run_dir / "tables-plate.SLDPRT"
save(plate)
mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=20, height_mm=10)
mcp.call("extrude_sketch", depth_mm=10)
block = mcp.run_dir / "tables-block.SLDPRT"
save(block)
assembly = mcp.run_dir / "tables-pair.SLDASM"
com(f"""$template = Com $sw GetUserPreferenceStringValue @(9)
$a = Com $sw NewDocument @($template, 0, 0, 0); if (-not $a) {{ throw "NewDocument failed" }}
if (-not (Com $a AddComponent5 @('{plate}', 0, '', $false, '', 0.0, 0.0, 0.0))) {{ throw 'plate' }}
foreach ($y in -0.015, 0.015) {{
    if (-not (Com $a AddComponent5 @('{block}', 0, '', $false, '', 0.005, $y, 0.0))) {{ throw 'block' }}
}}
$e = Com $a SaveAs3 @('{assembly}', 0, 1); if ($e -ne 0) {{ throw "SaveAs3: $e" }}""")

mcp.call("create_drawing", paper_size="A3")
asm_view = mcp.call("create_drawing_view", model=str(assembly), at_mm=[110, 200], scale="1:1")
plate_view = mcp.call("create_drawing_view", model=str(plate), at_mm=[260, 200], scale="1:1")
views = {v["name"]: v for s in mcp.call("inspect_drawing").json["sheets"] for v in s["views"]}
asm_name = [n for n, v in views.items() if "tables-pair" in v.get("model", "")][0]
plate_name = [n for n, v in views.items() if "tables-plate" in v.get("model", "")][0]
print("views:", asm_name, plate_name)

# ---- Border ------------------------------------------------------------------------
r = mcp.call("draw_sheet_border")
print(r.text)
assert "8 x 6 zones" in r.text, r.text
fmt = mcp.call("inspect_drawing_tables", include_format_lines=True).json["sheets"][0]["format_lines"]
border_lines = [l for l in fmt if l.get("layer") == "BORDER"]
assert len(border_lines) == 32, (len(border_lines), fmt[:5])
assert any(l["from_mm"] == [20.0, 10.0] and l["to_mm"] == [410.0, 10.0] for l in border_lines), border_lines[:4]
# Drawn again: replaced, not doubled.
r = mcp.call("draw_sheet_border")
assert "replaced the border" in r.text, r.text
fmt = mcp.call("inspect_drawing_tables", include_format_lines=True).json["sheets"][0]["format_lines"]
assert len([l for l in fmt if l.get("layer") == "BORDER"]) == 32, fmt

# ---- Title block -------------------------------------------------------------------
r = mcp.call("draw_title_block")
print(r.text)
assert "[230, 10]..[410, 51]" in r.text, r.text
r = mcp.call("fill_title_block", values={"DrawnBy": "MW", "Title": "PLATE ASSY", "Material": "Steel"})
print(r.text)
fields = {f["property"]: f for f in last_json(r.text)["fields"]}
assert fields["DrawnBy"]["of"] == "drawing" and fields["DrawnBy"]["value"] == "MW", fields
assert fields["Material"]["of"] == "model" and fields["Material"]["value"] == "Steel", fields
assert "MW" in fields["DrawnBy"]["note_shows"], fields["DrawnBy"]
r = mcp.call("fill_title_block", values={"Nope": "x"}, expect_error=True)
assert r.is_error and "shows no property" in r.text, r.text

# ---- BOM and balloons ------------------------------------------------------------
r = mcp.call("add_drawing_table", kind="bom", view=asm_name)
print(r.text)
bom = last_json(r.text)
rows = [row["cells"] if isinstance(row, dict) else row for row in bom["rows"]]
assert rows[0][:2] == ["ITEM NO.", "PART NUMBER"], rows
assert [row[1] for row in rows[1:]] == ["tables-plate", "tables-block"], rows
assert rows[2][-1] == "2", rows
title_block_box = [[230, 10], [410, 51]]
assert inside(bom["box_mm"], [20, 10], [410, 287]) and not overlaps(bom["box_mm"], title_block_box), bom["box_mm"]
for v in views.values():
    assert not overlaps(bom["box_mm"], v["outline"]), (bom["box_mm"], v)

r = mcp.call("add_balloons", view=asm_name)
print(r.text)
balloons = last_json(r.text)
shown = sorted((b["balloon"]["shows"], b["component"]) for b in balloons)
assert [s for s, _ in shown] == ["1", "2"], shown
assert "Item numbers come from" in r.text, r.text
r = mcp.call("add_balloons", view=asm_name, components=["tables-block-2"], at_mm=[[150, 250]], style="box")
print(r.text)
b = last_json(r.text)[0]
assert b["balloon"]["shows"] == "2" and b["at_mm"] == [150.0, 250.0], b
r = mcp.call("add_balloons", view=asm_name, components=["nothing-1"], expect_error=True)
assert r.is_error and "shows no component" in r.text, r.text

# Refused: a BOM's rows follow the assembly.
r = mcp.call("edit_drawing_table", name=bom["name"], delete_rows=[1], expect_error=True)
assert r.is_error and "come from the model" in r.text, r.text
# Typing over an item number renumbers the item (as in the UI); its balloon follows.
mcp.call("edit_drawing_table", name=bom["name"], cells=[{"row": 1, "column": 0, "text": "9"}])
shown = {a["balloon"]["component"]: a["balloon"]["shows"]
         for s in mcp.call("inspect_drawing_annotations", kind="balloon").json["sheets"] for a in s["annotations"]}
print("balloons after renumbering:", shown)
assert "9" in shown.values(), shown
mcp.call("edit_drawing_table", name=bom["name"], cells=[{"row": 1, "column": 0, "text": "1"}])

# ---- Hole table ------------------------------------------------------------------
pv = [v for s in mcp.call("inspect_drawing", view=plate_name, include_vertices=True).json["sheets"] for v in s["views"]
      if v["name"] == plate_name][0]
circles = [e for e in pv["entities"] if e["type"] == "circle"]
assert len(circles) == 4, [e["type"] for e in pv["entities"]]
verts = [e for e in pv["entities"] if e["type"] == "vertex"]
corner = min(verts, key=lambda e: e["at"][0] + e["at"][1])
r = mcp.call("add_drawing_table", kind="hole", view=plate_name, origin=corner["id"], holes=[c["id"] for c in circles])
print(r.text)
hole = last_json(r.text)
hrows = [row["cells"] if isinstance(row, dict) else row for row in hole["rows"]]
assert hrows[0][:3] == ["TAG", "X LOC", "Y LOC"], hrows
locs = sorted((float(r_[1]), float(r_[2])) for r_ in hrows[1:])
assert locs == [(10, 10), (10, 30), (50, 10), (50, 30)], locs
assert "tagged the holes" in r.text, r.text
assert not overlaps(hole["box_mm"], bom["box_mm"]) and not overlaps(hole["box_mm"], title_block_box), r.text

# ---- General table -------------------------------------------------------------
r = mcp.call("add_drawing_table", kind="general", rows=[["PT", "X", "Y"], ["P1", "10.0", "25.0"], ["P2", "50.0", "25.0"]])
print(r.text)
gen = last_json(r.text)
assert gen["rows"] == [["PT", "X", "Y"], ["P1", "10.0", "25.0"], ["P2", "50.0", "25.0"]], gen["rows"]
r = mcp.call("edit_drawing_table", name=gen["name"], cells=[{"row": 1, "column": 1, "text": "12.5"}],
             add_rows=[["P3", "30.0", "5.0"]], delete_rows=[2], title="COORDINATES", move_by_mm=[0, -5])
print(r.text)
gen2 = last_json(r.text)
assert gen2["rows"] == [["PT", "X", "Y"], ["P1", "12.5", "25.0"], ["P3", "30.0", "5.0"]], gen2["rows"]
assert abs(gen2["at_mm"][1] - (gen["at_mm"][1] - 5)) < 0.01, (gen["at_mm"], gen2["at_mm"])
r = mcp.call("edit_drawing_table", name=gen["name"], cells=[{"row": 7, "column": 0, "text": "x"}], expect_error=True)
assert r.is_error and "isn't in" in r.text, r.text
assert tables()[gen["name"]]["rows"] == gen2["rows"]

mcp.call("render_drawing", max_px=1600)

mcp.call("delete_drawing_annotations", names=[gen["name"]])
assert gen["name"] not in tables()
# A hole table goes with its tags and origin marker.
mcp.call("delete_drawing_annotations", names=[hole["name"]])
left = [a["type"] for s in mcp.call("inspect_drawing_annotations", view=plate_name).json["sheets"] for a in s["annotations"]]
assert hole["name"] not in tables() and "note" not in left and "datum_origin" not in left, left

# ---- Custom properties of a part the BOM lists -------------------------------
r = mcp.call("custom_properties", document=str(plate), set={"Description": "BASE PLATE"})
print(r.text)
assert "tables-plate" in r.json["document"], r.text

# ---- A stock sheet format's title block ---------------------------------------
mcp.call("add_sheet", sheet_format="a3 - iso")
mcp.call("create_drawing_view", model=str(plate), scale="1:1")
r = mcp.call("inspect_title_block", sheet="Sheet2")
print(r.text)
f2 = {f["property"]: f for f in r.json["fields"]}
assert f2["Description"]["of"] == "model" and f2["DrawnBy"]["of"] == "drawing", f2.keys()
# A new sheet takes $PRPSHEET values from the sheet Document Properties name
# (Sheet1, whose first view shows the assembly), not from its own view.
assert r.json["model"].startswith("tables-pair"), r.json["model"]
r = mcp.call("fill_title_block", sheet="Sheet2", values={"Description": "PLATE, 4 HOLES", "CheckedBy": "QA"})
f2 = {f["property"]: f for f in last_json(r.text)["fields"]}
assert "PLATE, 4 HOLES" in f2["Description"]["note_shows"], f2["Description"]
assert f2["CheckedBy"]["value"] == "QA", f2["CheckedBy"]
mcp.call("render_drawing", sheet="Sheet2", max_px=1600)

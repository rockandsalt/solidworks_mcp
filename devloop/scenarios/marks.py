# add_center_marks / add_centerlines / add_surface_finish_symbol /
# add_weld_symbol / revision tables against real SolidWorks (drawings.md,
# "Center marks and centerlines", "Surface finish and weld symbols", "Tables
# and balloons"): a plate with three holes in a row, four on a bolt circle and
# a boss. Marks at 1:2 (centers checked on the sheet), a marked circle left
# alone, linear and circular groups replacing their circles' marks, a row
# refused as circular, throughout (no circle twice), then a render and another
# view's entities (a sequence that once hung). Centerlines between silhouettes,
# between edges, SolidWorks' own, the refusals. Surface finish with a leader,
# standing, free; weld symbols by side in ISO and ANSI; both read back. A
# revision table: revisions, cells, symbols, a header row refused, a second
# table and a later sheet's revisions refused, a template without a zone.
# Both symbols on a part. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/marks.py

import json


def entry(text):
    """The JSON line of a reply."""
    return json.loads(text.splitlines()[-1])


def near(a, b, tol=0.05):
    return len(a) == len(b) and all(abs(x - y) <= tol for x, y in zip(a, b))


def plane(axis):
    faces = mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]
    return [f["id"] for f in faces if abs(f["normal"][axis] - 1) < 1e-6][0]


def annotations(in_=None):
    """Every annotation of the drawing, or those of one view."""
    out = []
    for s in mcp.call("inspect_drawing_annotations").json["sheets"]:
        out += [a for a in s.get("annotations", []) if in_ is None or a.get("in") == f"view {in_}"]
    return out


def clear(in_):
    names = [a["name"] for a in annotations(in_)]
    if names:
        mcp.call("delete_drawing_annotations", names=names)
    assert not annotations(in_)


def view_of(name):
    r = mcp.call("inspect_drawing", view=name)
    return [v for s in r.json["sheets"] for v in s["views"] if v["name"] == name][0]


def table_named(name):
    tables = [t for s in mcp.call("inspect_drawing_tables").json["sheets"] for t in s.get("tables", [])]
    return [t for t in tables if t["name"] == name][0]


# ---- The fixture: 100 x 60 x 10, x 0..10 in this template.
mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=100, height_mm=60)
mcp.call("extrude_sketch", depth_mm=10)
ROW = [[10, -30, 20], [10, 0, 20], [10, 30, 20]]
BOLTS = [[10, 15, -6], [10, 0, 9], [10, -15, -6], [10, 0, -21]]
for center in ROW + BOLTS:
    mcp.call("sketch_circle", on=plane(0), center=center, diameter_mm=4)
    mcp.call("cut_extrude_sketch", through_all=True)
mcp.call("sketch_circle", on=plane(0), center=[10, 0, -6], diameter_mm=16)
mcp.call("extrude_sketch", depth_mm=15)
model = str(mcp.run_dir / "marks-plate.SLDPRT")
mcp.call("save_document", path=model)

mcp.call("create_drawing")
mcp.call("create_drawing_view", model=model, at_mm=[100, 200], scale="1:2")
mcp.call("create_drawing_view", model=model, at_mm=[280, 200], scale="1:1", orientation="*Top")
mcp.call("create_drawing_view", model=model, at_mm=[280, 90], scale="1:1", orientation="*Top")

view1 = view_of("Drawing View1")
circles = [e for e in view1["entities"] if e["type"] == "circle" and abs(e["diameter_mm"] - 4) < 0.01]
assert len(circles) == 7, circles
# On the sheet +Y is right and +Z up, at 1:2 from the view's origin at [100, 200].
def sheet_of(center):
    return [100 + center[1] / 2, 200 + center[2] / 2]
def circle_at(center):
    return [c for c in circles if near(c["center"], sheet_of(center))][0]["id"]
row = [circle_at(c) for c in ROW]
bolts = [circle_at(c) for c in BOLTS]

# ---- The template marked the circles when the view was made: left alone.
before = len(annotations("Drawing View1"))
assert before >= 7, before
r = mcp.call("add_center_marks", on=[row[0]])
assert "Already had one" in r.text and row[0] in r.text and entry(r.text) == [], r.text
assert len(annotations("Drawing View1")) == before

# ---- One mark, its center on the sheet at the view's 1:2.
clear("Drawing View1")
r = mcp.call("add_center_marks", on=[row[0]])
e = entry(r.text)
assert len(e) == 1 and e[0]["type"] == "center_mark" and e[0]["group"] == "single", r.text
assert near(e[0]["centers_mm"][0], sheet_of(ROW[0])), r.text
assert row[0] in e[0]["attached"], r.text
assert "at_mm" not in e[0], r.text
single = e[0]["name"]
r = mcp.call("add_center_marks", on=[row[0]])
assert entry(r.text) == [] and "Already had one" in r.text, r.text

# ---- A linear group takes its circle's own mark's place.
r = mcp.call("add_center_marks", on=row, group="linear")
e = entry(r.text)
assert len(e) == 1 and e[0]["group"] == "linear" and len(e[0]["centers_mm"]) == 3, r.text
for got, center in zip(sorted(e[0]["centers_mm"]), sorted(sheet_of(c) for c in ROW)):
    assert near(got, center), r.text
assert single in r.text and "replaced" in r.text, r.text
names = [a["name"] for a in annotations("Drawing View1")]
assert names == [e[0]["name"]], names
linear = e[0]["name"]
# A circle of it can't get into another group without the rest.
r = mcp.call("add_center_marks", on=row[:2], group="linear", expect_error=True)
assert r.is_error and linear in r.text, r.text
assert [a["name"] for a in annotations("Drawing View1")] == [linear]

# ---- A circular group over the bolt circle; a row refused.
r = mcp.call("add_center_marks", on=row, group="circular", expect_error=True)
assert r.is_error and "in a row" in r.text, r.text
r = mcp.call("add_center_marks", on=bolts, group="circular")
e = entry(r.text)
assert len(e) == 1 and e[0]["group"] == "circular" and len(e[0]["centers_mm"]) == 5, r.text
# The holes' centers and the bolt circle's own.
for center in BOLTS + [[10, 0, -6]]:
    assert any(near(got, sheet_of(center)) for got in e[0]["centers_mm"]), r.text
r = mcp.call("add_center_marks", on=[row[0]], group="circular", expect_error=True)
assert r.is_error, r.text

# ---- Throughout: every other circle, none twice.
clear("Drawing View1")
r = mcp.call("add_center_marks", on=[row[0]])
r = mcp.call("add_center_marks", view="Drawing View1")
e = entry(r.text)
assert "had a mark already" in r.text, r.text
marks = annotations("Drawing View1")
on = [i for a in marks for i in a.get("attached", [])]
assert len(on) == len(set(on)) and set(row + bolts) <= set(on), marks

# ---- The sequence that once hung the add-in.
mcp.call("render_drawing")
view2 = view_of("Drawing View2")

# ---- Centerlines.
silhouettes = [e for e in view2["entities"] if e["type"] == "silhouette"]
assert len(silhouettes) == 2, view2["entities"]
pair = [s["id"] for s in silhouettes]
assert not annotations("Drawing View2")
r = mcp.call("add_centerlines", between=[pair])
e = entry(r.text)
assert len(e) == 1 and e[0]["type"] == "centerline", r.text
assert len(annotations("Drawing View2")) == 1
clear("Drawing View2")
lines = [x for x in view2["entities"] if x["type"] == "line"]
across = [x for x in lines if abs(x["start"][1] - x["end"][1]) < 0.01]
upright = [x for x in lines if abs(x["start"][0] - x["end"][0]) < 0.01]
r = mcp.call("add_centerlines", between=[[across[0]["id"], across[1]["id"]]])
assert len(entry(r.text)) == 1, r.text
# Already there: left alone.
r = mcp.call("add_centerlines", between=[[across[0]["id"], across[1]["id"]]])
assert entry(r.text) == [] and "Already had one" in r.text, r.text
clear("Drawing View2")
for wrong in ([across[0]["id"], upright[0]["id"]], [pair[0], pair[0]]):
    r = mcp.call("add_centerlines", between=[wrong], expect_error=True)
    assert r.is_error, r.text
assert not annotations("Drawing View2")
r = mcp.call("add_centerlines", view="Drawing View3")
assert len(entry(r.text)) >= 1 and entry(r.text)[0]["type"] == "centerline", r.text
r = mcp.call("render_drawing", view="Drawing View3")

# ---- Surface finish and weld symbols on a sheet of their own.
mcp.call("add_sheet", name="Symbols")
mcp.call("create_drawing_view", model=model, at_mm=[100, 220], scale="1:1")
view4 = view_of("Drawing View4")
clear("Drawing View4")
edges = [x for x in view4["entities"] if x["type"] == "line"]
bottom = min((x for x in edges if abs(x["start"][1] - x["end"][1]) < 0.01), key=lambda x: x["start"][1])
top = max((x for x in edges if abs(x["start"][1] - x["end"][1]) < 0.01), key=lambda x: x["start"][1])

r = mcp.call("add_surface_finish_symbol", kind="machining_required", roughness="Ra 3.2", lay="parallel",
             production_method="ground", on=bottom["id"], at_mm=[60, 150])
e = entry(r.text)
assert e["type"] == "surface_finish" and e["finish"] == "machining_required" and e["lay"] == "parallel", r.text
assert e["roughness"] == "Ra 3.2" and e["production_method"] == "ground", r.text
assert near(e["at_mm"], [60, 150]) and len(e["leaders"]) == 1 and e["attached"] == [bottom["id"]], r.text
assert "point" in r.text, r.text
# Default place: off the view, not over it.
r = mcp.call("add_surface_finish_symbol", roughness="Ra 1.6", on=bottom["id"], arrow_at_mm=[120, bottom["start"][1]])
e = entry(r.text)
assert e["at_mm"][1] < bottom["start"][1] and near(e["leaders"][0]["arrow_at_mm"], [120, bottom["start"][1]], 0.5), r.text
# Standing on the edge.
r = mcp.call("add_surface_finish_symbol", kind="no_machining", on=top["id"], no_leader=True, arrow_at_mm=[80, top["start"][1]])
e = entry(r.text)
assert e["finish"] == "no_machining" and not e.get("leaders") and e["attached"] == [top["id"]], r.text
assert near(e["at_mm"], [80, top["start"][1]], 0.5), r.text
# Free on the sheet: SolidWorks makes it at the sheet's corner, moved here.
r = mcp.call("add_surface_finish_symbol", roughness="Ra 6.3", at_mm=[350, 30])
e = entry(r.text)
assert near(e["at_mm"], [350, 30]) and "sheet" in e["in"], r.text
free_finish = e["name"]
for wrong in ({"no_leader": True}, {"no_leader": True, "on": top["id"], "at_mm": [1, 2]}):
    r = mcp.call("add_surface_finish_symbol", expect_error=True, **wrong)
    assert r.is_error, r.text
assert free_finish in mcp.call("delete_drawing_annotations", names=[free_finish]).text

r = mcp.call("add_weld_symbol", on=bottom["id"], at_mm=[160, 150], process="135", all_around=True, field_weld=True,
             arrow_side={"symbol": "fillet", "size": "a5", "length": "3x50-100", "contour": "flat"},
             other_side={"symbol": "v", "size": "6"})
e = entry(r.text)
assert e["type"] == "weld_symbol" and near(e["at_mm"], [160, 150]) and len(e["leaders"]) == 1, r.text
# ISO: the arrow side is what SolidWorks calls above.
assert e["above_line"] == {"symbol": "fillet", "size": "a5", "length": "3x50-100", "contour": "flat"}, r.text
assert e["below_line"] == {"symbol": "v", "size": "6"}, r.text
assert e["process"] == "135" and e["all_around"] and e["field_weld"], r.text
r = mcp.call("add_weld_symbol", on=bottom["id"], at_mm=[240, 150], both_sides=True, arrow_side={"symbol": "fillet", "size": "4"})
e = entry(r.text)
assert e["both_sides"] and e["above_line"]["symbol"] == "fillet", r.text
for wrong in ({"arrow_side": {"symbol": "fillet"}},
              {"on": bottom["id"]},
              {"on": bottom["id"], "arrow_side": {"symbol": "stud"}},
              {"on": bottom["id"], "both_sides": True, "other_side": {"symbol": "v"}}):
    r = mcp.call("add_weld_symbol", expect_error=True, **wrong)
    assert r.is_error, r.text
count = len(annotations("Drawing View4"))
mcp.call("render_drawing")

# ANSI: the arrow side below the line; ANSI's own symbols.
mcp.call("document_units", drafting_standard="ansi")
r = mcp.call("add_weld_symbol", on=bottom["id"], at_mm=[160, 90], arrow_side={"symbol": "fillet", "size": "5"},
             other_side={"symbol": "stud", "size": "8"})
e = entry(r.text)
assert e["below_line"] == {"symbol": "fillet", "size": "5"} and e["above_line"] == {"symbol": "stud", "size": "8"}, r.text
mcp.call("render_drawing")
mcp.call("document_units", drafting_standard="iso")
assert len(annotations("Drawing View4")) == count + 1

# ---- A revision table, on the first sheet.
mcp.call("activate_sheet", sheet="Sheet1")
r = mcp.call("add_drawing_table", kind="revision", at_mm=[400, 280], revision_symbol="triangle", revisions=[
    {"description": "first issue", "approved": "MW", "symbols_at_mm": [[30, 120]]},
    {"revision": "A1", "zone": "B3", "description": "hole moved", "date": "2026-11-01"},
])
assert "drawing_properties" in r.text, r.text
t = entry(r.text)
name = t["name"]
assert t["kind"] == "revision" and t["anchor"] == "top_right" and near(t["at_mm"], [400, 280]), r.text
assert t["title"] == "REVISIONS" and t["header_rows"] == 1 and [c.get("type") for c in t["columns"]] == \
    ["zone", "revision", "description", "date", "approved"], r.text
assert t["rows"][1][1] == "A" and t["rows"][1][2] == "first issue" and t["rows"][1][3] != "" and t["rows"][1][4] == "MW", r.text
assert t["rows"][2] == ["B3", "A1", "hole moved", "2026-11-01", ""], r.text
symbols = [a for a in annotations() if a.get("text") == ["A"]]
assert len(symbols) == 1 and near(symbols[0]["at_mm"], [30, 120]), symbols

r = mcp.call("edit_drawing_table", name=name, add_revisions=[{"description": "third"}], cells=[{"row": 1, "column": 2, "text": "released"}])
t = entry(r.text)
assert len(t["rows"]) == 4 and t["rows"][3][2] == "third" and t["rows"][1][2] == "released", r.text
# The column titles are no revision: nothing happens to the table.
r = mcp.call("edit_drawing_table", name=name, delete_rows=[0], expect_error=True)
assert r.is_error and "header" in r.text, r.text
assert table_named(name)["title"] == "REVISIONS" and len(table_named(name)["rows"]) == 4
r = mcp.call("edit_drawing_table", name=name, add_rows=[["x"]], expect_error=True)
assert r.is_error and "revision" in r.text, r.text
# A revision goes with its symbols.
r = mcp.call("edit_drawing_table", name=name, delete_rows=[1])
assert len(entry(r.text)["rows"]) == 3, r.text
assert not [a for a in annotations() if a.get("text") == ["A"]]
r = mcp.call("add_drawing_table", kind="revision", expect_error=True)
assert r.is_error and name in r.text, r.text

# A later sheet's table says "See Sheet1", and revisions don't go in it.
r = mcp.call("add_drawing_table", kind="revision", sheet="Symbols", revisions=[{"description": "no"}], expect_error=True)
assert r.is_error and "first sheet" in r.text, r.text
r = mcp.call("add_drawing_table", kind="revision", sheet="Symbols", at_mm=[400, 280])
later = entry(r.text)
assert "See Sheet1" in later["rows"][-1], r.text
r = mcp.call("edit_drawing_table", name=later["name"], sheet="Symbols", add_revisions=[{"description": "no"}], expect_error=True)
assert r.is_error and "first sheet" in r.text, r.text

# A template without a zone column, placed by default near the top right.
mcp.call("delete_drawing_annotations", names=[name])
r = mcp.call("add_drawing_table", kind="revision", template="no zone column", revisions=[{"description": "one"}])
t = entry(r.text)
assert "zone" not in [c.get("type") for c in t["columns"]], r.text
assert t["at_mm"][0] > 300, r.text
rows = len(t["rows"])
r = mcp.call("edit_drawing_table", name=t["name"], add_revisions=[{"zone": "B3", "description": "two"}], expect_error=True)
assert r.is_error and "no column" in r.text, r.text
assert len(table_named(t["name"])["rows"]) == rows
mcp.call("render_drawing")

# ---- On a part. (Scenarios of a run share SolidWorks: nothing stays open.)
mcp.call("close_document", discard_changes=True)
mcp.call("open_document", path=model)
face = plane(2)
r = mcp.call("add_surface_finish_symbol", roughness="Ra 3.2", on=face)
assert "surface_finish" in r.text and face in r.text, r.text
edge = mcp.call("inspect_model", what="edges", limit=1).json["edges"][0]["id"]
r = mcp.call("add_weld_symbol", on=edge, arrow_side={"symbol": "fillet", "size": "5"})
assert "weld_symbol" in r.text and edge in r.text, r.text
r = mcp.call("add_weld_symbol", arrow_side={"symbol": "fillet"}, expect_error=True)
assert r.is_error, r.text
listed = mcp.call("inspect_annotations").json["annotations"]
assert {a["type"] for a in listed} == {"surface_finish", "weld_symbol"}, listed
finish = [a for a in listed if a["type"] == "surface_finish"][0]
assert finish["roughness"] == "Ra 3.2", finish
mcp.call("delete_drawing_annotations", names=[a["name"] for a in listed])
assert not mcp.call("inspect_annotations").json["annotations"]
mcp.call("close_document", discard_changes=True)

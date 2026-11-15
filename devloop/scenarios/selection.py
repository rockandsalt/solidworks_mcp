# selection against real SolidWorks (docs/notes/selection.md): a 40 x 20 x
# 10 block; a "user" pick on its top face (SelectByID2 at a point, through
# run_macro) read back as that face's id and the pick point, which then
# place a hole with hole_wizard; whether a read-only tool keeps the pick;
# selecting faces / edges / features / bodies / sketch lines / vertices by
# id and reading them back; append, clear, refusals; then an assembly of two
# blocks, a pick on the second one's face reported in assembly space with
# its component, and a component selected by id; then a drawing of the
# block: a "user" pick on a view edge read back with inspect_drawing's id,
# dimensioned with it; the view and a dimension picked. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/selection.py

import re

out = mcp.run_dir.resolve()
block_file = out / "selection-block.SLDPRT"
asm_file = out / "selection-top.SLDASM"


def close(a, b, tol=1e-3):
    return all(abs(x - y) <= tol for x, y in zip(a, b))


def selection(**kw):
    r = mcp.call("selection", **kw)
    print(f"    selection {kw}: {r.text}")
    return r.json


def user_picks_face(point_mm):
    """Selects whatever face is at `point_mm` as a click there would."""
    x, y, z = (c / 1000 for c in point_mm)
    r = mcp.call("run_macro", code=f'''
Sub main()
    Set doc = Application.SldWorks.ActiveDoc
    doc.ClearSelection2 True
    McpPrint "picked=" & doc.Extension.SelectByID2("", "FACE", {x!r}, {y!r}, {z!r}, False, 0, Nothing, 0)
End Sub
''')
    assert "picked=True" in r.text, r.text


# ---- A block -------------------------------------------------------------------------------------
mcp.call("create_part")
sketch_reply = mcp.call("sketch", shape="rectangle", width_mm=40, height_mm=20).text
line = re.findall(r"\bline:\d+", sketch_reply)[0]
mcp.call("extrude_sketch", depth_mm=10)
model = mcp.call("inspect_model").json
body = model["bodies"][0]
boss = [f["name"] for f in model["features"] if f["type"] == "Extrusion"][0]
lo, hi = body["bbox_mm"]["min"], body["bbox_mm"]["max"]
size = [h - l for l, h in zip(lo, hi)]
thin = size.index(min(size))  # the 10 mm axis
faces = mcp.call("inspect_model", what="faces").json["faces"]


def cap(value):
    return [f for f in faces if "normal" in f and abs(f["normal"][thin]) > 0.99 and abs(f["point_on_plane_mm"][thin] - value) < 1e-6][0]


top, bottom = cap(hi[thin]), cap(lo[thin])
edge = mcp.call("inspect_model", what="edges", within=top["id"]).json["edges"][0]

# Nothing selected.
j = selection(clear=True)
assert j["selected"] == [] and "nothing is selected" in j["note"], j

# ---- The user picks the top face; the hole goes where they clicked ----------------------------
pick = [(l + h) / 2 for l, h in zip(lo, hi)]
pick[thin] = hi[thin]
others = [a for a in range(3) if a != thin]
pick[others[0]] += 7
pick[others[1]] += 3
user_picks_face(pick)
j = selection()
assert len(j["selected"]) == 1, j
picked = j["selected"][0]
assert picked["id"] == top["id"], (picked, top)
assert picked["type"] == "plane" and picked["feature"] == boss, picked
# Pick points come from the pick ray at screen resolution: ~0.02 mm off here.
assert close(picked["picked_at_mm"], pick, 0.1), (picked["picked_at_mm"], pick)

# A read-only tool in between: is the pick still there?
mcp.call("inspect_model", what="bodies")
kept = selection()["selected"]
print("    pick kept after inspect_model:", [i.get("id") for i in kept])

r = mcp.call("hole_wizard", kind="hole", size="M5", on=picked["id"], depth_mm=5,
             locations=[picked["picked_at_mm"]])
print("    hole:", r.text)
assert "1 M5 hole" in r.text, r.text
centers = [[float(c) for c in m.split(",")] for m in re.findall(r"point:\d+ at \[([^\]]+)\]", r.text)]
assert len(centers) == 1 and close(centers[0], picked["picked_at_mm"], 0.01), (centers, picked)
holes = [f for f in mcp.call("inspect_model", what="faces", surface_type="cylinder").json["faces"]
         if f.get("hole")]
assert len(holes) == 1, holes
# The hole changed the body: it has a new id now.
body = mcp.call("inspect_model", what="bodies").json["bodies"][0]

# ---- Selecting by id, read back -----------------------------------------------------------------
j = selection(select=[bottom["id"], edge["id"], boss])
got = j["selected"]
assert [i["id"] for i in got[:2]] == [bottom["id"], edge["id"]], got
assert got[2]["name"] == boss and got[2]["type"] == "Extrusion", got
# Nothing was clicked: no pick points.
assert all("picked_at_mm" not in i for i in got), got

j = selection(select=[body["id"]], append=True)
assert len(j["selected"]) == 4 and j["selected"][3]["id"] == body["id"], j
assert all("picked_at_mm" not in i for i in j["selected"]), j
assert j["selected"][3]["solid"] is True, j

j = selection(select=[line])
assert len(j["selected"]) == 1, j
s = j["selected"][0]
assert s["id"] == line and s["type"] == "line" and s["sketch"] == "Sketch1", s

vertex = edge["start_vertex"]
j = selection(select=[vertex])
v = j["selected"][0]
assert v["id"] == vertex and close(v["at_mm"], edge["start_mm"]), (v, edge)

j = selection(select=["top"])
assert j["selected"][0]["type"] == "RefPlane", j

j = selection(clear=True)
assert j["selected"] == [], j

# Refusals.
mcp.call("selection", select=["face:99999"], expect_error=True)
mcp.call("selection", select=["No Such Feature"], expect_error=True)
mcp.call("selection", clear=True, select=[top["id"]], expect_error=True)
mcp.call("selection", append=True, expect_error=True)
mcp.call("save_document", path=str(block_file))
# Saving a new part the first time gives its faces and edges new persist ids (features and
# bodies keep theirs; seen 2026-11-14): the old ones still resolve, but don't match what a
# drawing's views say they draw. So the drawing part below takes the saved part's.
faces = mcp.call("inspect_model", what="faces").json["faces"]
top, bottom = cap(hi[thin]), cap(lo[thin])
hole = [f for f in faces if f.get("hole")][0]
hole_edges = [e["id"] for e in mcp.call("inspect_model", what="edges", within=hole["id"]).json["edges"]]
# A corner of each cap (its outline's lines have end vertices; the hole's rim has none).
cap_vertex = {c["id"]: [e for e in mcp.call("inspect_model", what="edges", within=c["id"]).json["edges"]
                        if "start_vertex" in e][0]["start_vertex"] for c in (top, bottom)}

# ---- An assembly of two blocks -----------------------------------------------------------------
mcp.call("create_assembly")
mcp.call("save_document", path=str(asm_file))
long_axis = size.index(max(size))
shift = [0.0, 0.0, 0.0]
shift[long_axis] = size[long_axis] + 5
first = re.findall(r"^- (component:\d+):", mcp.call("insert_components", file=str(block_file)).text, re.M)[0]
r = mcp.call("insert_components", file=str(block_file), at=[{"origin_mm": shift}], fixed=False)
second = re.findall(r"^- (component:\d+):", r.text, re.M)[0]

# The user picks the second block's top face, away from the hole.
apick = [c + d for c, d in zip(pick, shift)]
apick[others[0]] -= 14
user_picks_face(apick)
j = selection()
assert len(j["selected"]) == 1, j
f = j["selected"][0]
assert f["id"].startswith("face:") and f["component"] == second, (f, second)
# Zoomed out further than the part: coarser (0.26 mm off here).
assert close(f["picked_at_mm"], apick, 0.5), (f["picked_at_mm"], apick)
# Its geometry is in assembly space, like inspect_model's.
assert abs(f["point_on_plane_mm"][thin] - hi[thin]) < 1e-3 and f["normal"][thin] > 0.99, f
assert abs(f["bbox_mm"]["min"][long_axis] - (lo[long_axis] + shift[long_axis])) < 1e-3, f

j = selection(select=[second])
c = j["selected"][0]
assert c["id"] == second and c["name"].startswith("selection-block"), c

# Feature names resolve in an assembly too (FeatureByName), for selection and references alike.
planes = [f["name"] for f in mcp.call("inspect_model").json["features"]
          if f["type"] == "RefPlane" and "component" not in f][:2]
assert len(planes) == 2, planes
j = selection(select=[planes[0]])
assert j["selected"][0]["name"] == planes[0], j
r = mcp.call("measure", entities=planes)
print("    measure", planes, ":", r.text)
assert "90" in r.text, r.text
selection(clear=True)
mcp.call("save_document")

# ---- A drawing of the block: the user picks a view edge -------------------------------------------
mcp.call("create_drawing", sheet_format="a3 - iso")
mcp.call("create_drawing_view", model=str(block_file), at_mm=[150, 150], scale="1:1")
dv = [v for s in mcp.call("inspect_drawing", view="Drawing View1").json["sheets"] for v in s["views"]
      if v["name"] == "Drawing View1"][0]
lines = [e for e in dv["entities"] if e["type"] == "line"]
target = max(lines, key=lambda e: e["length_mm"])
# A third of the way along, off any other edge's end.
at = [a + (b - a) / 3 for a, b in zip(target["start"], target["end"])]


def user_picks(type_, point_mm=(0, 0), name=""):
    """Selects what SelectByID2 finds at `point_mm` (sheet mm) or by `name`, as a click would."""
    x, y = (c / 1000 for c in point_mm)
    r = mcp.call("run_macro", code=f'''
Sub main()
    Set doc = Application.SldWorks.ActiveDoc
    doc.ClearSelection2 True
    McpPrint "picked=" & doc.Extension.SelectByID2("{name}", "{type_}", {x!r}, {y!r}, 0, False, 0, Nothing, 0)
End Sub
''')
    assert "picked=True" in r.text, r.text


user_picks("EDGE", at)
j = selection()
# SelectByID2 of a view's edge selects the view too (before the edge).
picked = [i for i in j["selected"] if "drawing_view" not in i]
assert len(picked) == 1, j
e = picked[0]
print("    picked", e, "target", target, "at", at)
assert e["id"] == target["id"] and e["view"] == "Drawing View1", (e, target)
assert e["start"] == target["start"] and e["end"] == target["end"], (e, target)
assert close(e["picked_at_mm"], at, 0.5), (e["picked_at_mm"], at)

# The id goes straight into add_drawing_dimensions.
r = mcp.call("add_drawing_dimensions", dimensions=[{"kind": "length", "entities": [e["id"]]}])
print("    dimension:", r.text)
dim = re.findall(r"(\S+@Drawing View1)\S* =", r.text)
assert dim, r.text

# Selected by id, read back; nothing clicked.
j = selection(select=[target["id"]])
assert [i["id"] for i in j["selected"]] == [target["id"]] and "picked_at_mm" not in j["selected"][0], j

# The view itself, and the dimension just added.
user_picks("DRAWINGVIEW", name="Drawing View1")
j = selection()
assert j["selected"] == [{"drawing_view": "Drawing View1"}], j
user_picks("DIMENSION", name=dim[0])
j = selection()
d = j["selected"][0]
assert d["type"] == "dimension" and d["dimension"].startswith(dim[0]) and d["view"] == "Drawing View1", j
assert d["attached"] == [target["id"]], j

# ---- Drawing <-> model ---------------------------------------------------------------------------
# Each view entity says what it is in the part: the part's id for it and its features.
assert target["model_id"].startswith("edge:") and boss in target["features"], target
j = selection(select=[target["id"]])
s = j["selected"][0]
assert s["model_id"] == target["model_id"] and s["features"] == target["features"], j
assert s["model"].lower() == block_file.name.lower(), j

# Model -> drawing: the part's ids and feature names select what the views draw of them.
j = selection(select=[target["model_id"]])
assert [i["id"] for i in j["selected"]] == [target["id"]], j
drawn_by_boss = {e["id"] for e in dv["entities"] if boss in e.get("features", [])}
j = selection(select=[boss])
assert {i["id"] for i in j["selected"]} == drawn_by_boss and drawn_by_boss, (j, drawn_by_boss)
boss_id = [f["id"] for f in model["features"] if f["name"] == boss][0]
j = selection(select=[boss_id])
assert {i["id"] for i in j["selected"]} == drawn_by_boss, j
# A face: the edges around it. The view faces one of the caps (the one with the hole: its
# outline and the hole's rim, everything the view draws); the other is behind it.
seen_caps = {}
for cap_face in (top, bottom):
    try:
        j = selection(select=[cap_face["id"]])
        seen_caps[cap_face["id"]] = [i["id"] for i in j["selected"]]
    except ToolError as error:
        assert "isn't drawn" in str(error), error
assert len(seen_caps) == 1, seen_caps
seen_cap, seen_edges = next(iter(seen_caps.items()))
assert set(seen_edges) == {e["id"] for e in dv["entities"]}, (seen_caps, dv)
# A corner of the cap the view faces: drawn, as a view vertex.
corner = cap_vertex[seen_cap]
j = selection(select=[corner])
assert [i["model_id"] for i in j["selected"]] == [corner], j
assert j["selected"][0]["id"].startswith("view_vertex:"), j
# A hole edge the view doesn't draw (the blind hole's far end at least): refused, and the
# user's pick (the view and an edge of it: listing the view's silhouettes picks in it, and
# a pick on a selected view starts dragging it) is still selected and hasn't moved.
drawn_models = {e.get("model_id") for e in dv["entities"]}
hidden = [e for e in hole_edges if e not in drawn_models]
assert hidden, (hole_edges, drawn_models)
user_picks("EDGE", at)
r = mcp.call("selection", select=[hidden[0]], expect_error=True)
assert "isn't drawn" in r.text, r.text
j = selection()
assert [i.get("id") for i in j["selected"] if "drawing_view" not in i] == [target["id"]], j
assert {"drawing_view": "Drawing View1"} in j["selected"], j
again = [e for s in mcp.call("inspect_drawing", view="Drawing View1").json["sheets"] for v in s["views"]
         if v["name"] == "Drawing View1" for e in v["entities"] if e["id"] == target["id"]]
assert again and again[0]["start"] == target["start"] and again[0]["end"] == target["end"], (again, target)

# The feature picked under the view in the drawing's tree: the part's feature, with the part's id.
user_picks("BODYFEATURE", name=f"{boss}@{block_file.stem}-1@Drawing View1")
j = selection()
f = j["selected"][0]
assert f["id"] == boss_id and f["name"] == boss and f["view"] == "Drawing View1", (j, boss_id)

# Refusals: planes and the origin in a drawing; a view entity with them.
mcp.call("selection", select=["top"], expect_error=True)
mcp.call("selection", select=[target["id"], "top"], expect_error=True)
j = selection(clear=True)
assert j["selected"] == [] and "inspect_drawing" in j["note"], j

# ---- An assembly view: which component's edge ------------------------------------------------------
mcp.call("create_drawing_view", model=str(asm_file), at_mm=[300, 150], scale="1:1", name="Assembly")
av = [v for s in mcp.call("inspect_drawing", view="Assembly").json["sheets"] for v in s["views"]
      if v["name"] == "Assembly"][0]
by_component = {}
for e in av["entities"]:
    assert e.get("model_id", "").startswith("edge:") and boss in e["features"], e
    by_component.setdefault(e["component"], []).append(e)
assert len(by_component) == 2, by_component.keys()
# The same edge of each block: its own model id, and selecting that id finds that one.
for comp, entities in by_component.items():
    e = max(entities, key=lambda e: e.get("length_mm", 0))
    j = selection(select=[e["model_id"]])
    assert [i["id"] for i in j["selected"]] == [e["id"]], (comp, e, j)
assert len({e["model_id"] for e in av["entities"]}) == len(av["entities"]), av["entities"]
asm_edge = max(av["entities"], key=lambda e: e.get("length_mm", 0))
selection(clear=True)

# ---- A pin: silhouettes are their cylinder's face ----------------------------------------------------
pin_file = out / "selection-pin.SLDPRT"
mcp.call("create_part")
mcp.call("sketch", shape="circle", center=[0, 0], diameter_mm=10)
mcp.call("extrude_sketch", depth_mm=30)
mcp.call("save_document", path=str(pin_file))
cylinder = mcp.call("inspect_model", what="faces", surface_type="cylinder").json["faces"][0]
mcp.call("create_drawing", sheet_format="a3 - iso")
for orientation, at in [("top", [120, 150]), ("right", [250, 150])]:
    mcp.call("create_drawing_view", model=str(pin_file), at_mm=at, scale="1:1", orientation=orientation)
# Listing silhouettes picks in the view, and a pick on something selected starts dragging it:
# list them with a view selected (TrackerEscape cancels the drag).
user_picks("DRAWINGVIEW", name="Drawing View1")
pin_views = [v for s in mcp.call("inspect_drawing").json["sheets"] for v in s["views"]]
silhouettes = []
for v in pin_views:
    listed = [x for s in mcp.call("inspect_drawing", view=v["name"]).json["sheets"] for x in s["views"]
              if x["name"] == v["name"]][0]
    silhouettes += [e for e in listed["entities"] if e["id"].startswith("silhouette:")]
print("    pin silhouettes:", silhouettes)
assert silhouettes and all(e["model_id"] == cylinder["id"] for e in silhouettes), (silhouettes, cylinder)
# Selecting the face: its silhouettes and rims in both views (selecting a silhouette with its
# view selects the view too: an item without an id).
j = selection(select=[cylinder["id"]])
got = {i["id"] for i in j["selected"] if i.get("id")}
sil_sheet1 = {e["id"] for e in silhouettes}
assert sil_sheet1 <= got, (j, silhouettes)

# ---- Back in the models: the drawing's model ids are inspect_model's ----------------------------------
mcp.call("open_document", path=str(block_file))
j = selection(select=[target["model_id"]])
assert j["selected"][0]["id"] == target["model_id"], j
block_edges = {e["id"] for e in mcp.call("inspect_model", what="edges").json["edges"]}
assert target["model_id"] in block_edges, (target, block_edges)
mcp.call("open_document", path=str(asm_file))
asm_edges = {e["id"] for e in mcp.call("inspect_model", what="edges").json["edges"]}
assert asm_edge["model_id"] in asm_edges, (asm_edge, sorted(asm_edges))
selection(clear=True)

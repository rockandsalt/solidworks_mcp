# selection against real SolidWorks (docs/notes/selection.md): a 40 x 20 x
# 10 block; a "user" pick on its top face (SelectByID2 at a point, through
# run_macro) read back as that face's id and the pick point, which then
# place a hole with hole_wizard; whether a read-only tool keeps the pick;
# selecting faces / edges / features / bodies / sketch lines / vertices by
# id and reading them back; append, clear, refusals; then an assembly of two
# blocks, a pick on the second one's face reported in assembly space with
# its component, and a component selected by id. Run with:
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
    return [f for f in faces if abs(f["normal"][thin]) > 0.99 and abs(f["point_on_plane_mm"][thin] - value) < 1e-6][0]


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

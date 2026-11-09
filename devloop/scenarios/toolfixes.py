# The tool fixes from the 2026-11-08 token benchmark's drawing runs
# (docs/notes/mcp-best-practices.md, "Next improvements" 1): a center
# rectangle comes with width and height dimensions; create_drawing_view on a
# never-saved part names the save_document call, which saves it by `document`
# without leaving the drawing; a projected view takes side and at_mm together
# when they agree; delete_drawing_annotations / edit_drawing_annotation tell
# apart model dimensions sharing a short name ("D1" of a sketch and of an
# extrude) by full name, and deleting one leaves the other. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/toolfixes.py


def dims_in(view):
    j = mcp.call("inspect_drawing_annotations", view=view, kind="dimension").json
    return [a for s in j["sheets"] for a in s["annotations"]]


# ---- A rectangle's dimensions.
mcp.call("create_part")
r = mcp.call("sketch", shape="rectangle", width_mm=50, height_mm=30)
assert "Dimensions: D1@Sketch1" in r.text and "50 mm" in r.text and "30 mm" in r.text, r.text
sketch = mcp.call("inspect_sketch", sketch_name="Sketch1").json
values = sorted(round(d["value_mm"]) for d in sketch["dimensions"])
assert values == [30, 50], sketch["dimensions"]
mcp.call("extrude_sketch", depth_mm=10)
faces = mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]
top = [f["id"] for f in faces if f["normal"][2] > 0.99][0]
mcp.call("sketch", shape="circle", on=top, center=[0, 0, 10], diameter_mm=10)
mcp.call("extrude_sketch", cut=True, through_all=True)
part_title = mcp.call("get_active_document_name").text.strip()
print(f"    part: {part_title}")

# ---- A never-saved part: the error names the call, which works from the drawing.
mcp.call("create_drawing", sheet_format="a3 - iso")
drawing_title = mcp.call("get_active_document_name").text.strip()
r = mcp.call("create_drawing_view", orientation="front", expect_error=True)
assert "never saved" in r.text and "save_document document=" in r.text and ".SLDPRT" in r.text, r.text
part = mcp.run_dir.resolve() / "fixes-block.SLDPRT"
r = mcp.call("save_document", document=part_title.split(" ")[0], path=str(part))
assert part.exists(), r.text
assert mcp.call("get_active_document_name").text.strip() == drawing_title, "the drawing should stay active"
mcp.call("create_drawing_view", orientation="front", at_mm=[120, 180], scale="1:1", name="F")

# ---- Projected: side and at_mm together.
r = mcp.call("create_drawing_view", type="projected", view="F", side="below", at_mm=[120, 90], name="T")
assert "projected from F" in r.text, r.text
r = mcp.call("create_drawing_view", type="projected", view="F", side="right", at_mm=[120, 280], expect_error=True)
assert "isn't right of F" in r.text, r.text
mcp.call("create_drawing_view", type="projected", view="F", side="right", name="R")

# ---- Model dimensions sharing a short name.
mcp.call("insert_model_items")
shared = None
for view in ["F", "T", "R"]:
    dims = dims_in(view)
    print(f"    {view}: {[(d['name'], d.get('dimension')) for d in dims]}")
    by_name = {}
    for d in dims:
        by_name.setdefault(d["name"], []).append(d)
    pairs = [ds for ds in by_name.values() if len({d.get("dimension") for d in ds}) > 1]
    if pairs:
        shared = (view, pairs[0])
        break
assert shared, "no view shows two model dimensions of one short name"
view, (a, b) = shared[0], shared[1][:2]
r = mcp.call("delete_drawing_annotations", names=[a["name"]], view=view, expect_error=True)
assert "full name" in r.text and a["dimension"] in r.text and b["dimension"] in r.text, r.text
# Without the trailing "@<document>". A model dimension shown in two views
# has one full name: the view tells those apart.
short_full = a["dimension"].rsplit("@", 1)[0]
r = mcp.call("edit_drawing_annotation", name=short_full, move_by_mm=[0, 5], expect_error=True)
assert "say which sheet or view" in r.text, r.text
r = mcp.call("edit_drawing_annotation", name=short_full, view=view, move_by_mm=[0, 5])
r = mcp.call("delete_drawing_annotations", names=[short_full], view=view)
left = {d.get("dimension") for d in dims_in(view)}
assert a["dimension"] not in left and b["dimension"] in left, left
print("    deleted", a["dimension"], "kept", b["dimension"])

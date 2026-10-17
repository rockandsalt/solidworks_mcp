# Smoke test of the whole chain: new part, sketch, boss, a hole cut through
# a face found with inspect_model, render. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/smoke.py

mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)

overview = mcp.call("inspect_model").text
# The boss grows along the sketch normal; cut through its far cap.
caps = mcp.call("inspect_model", what="faces", within="Boss-Extrude1", surface_type="plane").json["faces"]
assert len(caps) == 6, f"a rectangular boss has 6 planar faces, got {len(caps)}"
face_id = caps[0]["id"]

mcp.call("sketch_circle", on=face_id, center=[0, 0], diameter_mm=8)
mcp.call("cut_extrude_sketch", through_all=True)

overview = mcp.call("inspect_model").text
assert "Cut-Extrude1" in overview, "the cut isn't in the feature tree"
assert mcp.call("render_views").images, "render_views returned no image"

# Bad input is rejected before it reaches SolidWorks.
mcp.call("extrude_sketch", depth_mm=-1, expect_error=True)

# Bugs a test agent found on 2026-10-17 (docs/notes/devloop.md), checked
# against real SolidWorks. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/regressions.py

import math

# A 60 x 40 x 8 plate on the Front plane: the boss grows along the sketch normal.
mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=60, height_mm=40)
mcp.call("extrude_sketch", depth_mm=8)

# 1. `equal` takes more than 3 entities (it was capped at 3, like dimensions).
# Two sizes, so the relation has to change something.
mcp.call("sketch_circle", plane="front", center=[-22, -12], diameter_mm=6)
for x, y in ((22, -12), (-22, 12), (22, 12)):
    mcp.call("sketch_circle", sketch_name="Sketch2", center=[x, y], diameter_mm=5)
circles = [e["id"] for e in mcp.call("inspect_sketch", sketch_name="Sketch2").json["segments"]
           if e["id"].startswith("circle:")]
assert len(circles) == 4, circles
mcp.call("add_sketch_relations", relations=[{"kind": "equal", "entities": circles}])

# 3. A diameter dimension is reported as a diameter (it said "radius").
mcp.call("dimension_sketch", dimensions=[{"kind": "diameter", "entities": [circles[0]], "value_mm": 6}])
dims = mcp.call("inspect_sketch", sketch_name="Sketch2").json["dimensions"]
assert len(dims) == 1 and dims[0]["type"] == "diameter", f"expected one diameter dimension: {dims}"

# 2. A cut from a sketch on the plane the plate grows away from: the default
# direction (against the normal) removes nothing, so it must flip and say so.
cut = mcp.call("cut_extrude_sketch", sketch_name="Sketch2", through_all=True)
# (Cut-Extrude2, not 1: the attempt the default way used up the name.)
assert "created Cut-Extrude" in cut.text and "reversed" in cut.text, cut.text
volume = mcp.call("inspect_model").json["bodies"][0]["volume_mm3"]
expected = 60 * 40 * 8 - 4 * math.pi * 3**2 * 8
assert abs(volume - expected) < 0.01, f"volume {volume}, expected {expected}"

# An explicit direction is kept even when it removes nothing: on the plate's
# far face, along the outward normal, there's nothing.
far_face = [f for f in mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]
            if abs(f["area_mm2"] - (60 * 40 - 4 * math.pi * 9)) < 0.01
            and max(f["normal"], key=abs) > 0]
assert len(far_face) == 1, far_face
mcp.call("sketch_circle", on=far_face[0]["id"], center=[0, 0], diameter_mm=4)
mcp.call("cut_extrude_sketch", depth_mm=2, reverse_direction=True, expect_error=True)
# Without it, the same sketch cuts into the plate.
mcp.call("cut_extrude_sketch", depth_mm=2)

mcp.call("render_views", views=["right", "isometric"])

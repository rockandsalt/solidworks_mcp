# Bugs a test agent found on 2026-10-17 (docs/notes/devloop.md), checked
# against real SolidWorks. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/regressions.py

import math
import re

# A 60 x 40 x 8 plate on the Front plane: the boss grows along the sketch normal.
mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=60, height_mm=40)
mcp.call("extrude_sketch", depth_mm=8)

# 1. `equal` takes more than 3 entities (it was capped at 3, like dimensions).
# Two sizes, so the relation has to change something.
mcp.call("sketch", shape="circle", plane="front", center=[-22, -12], diameter_mm=6)
for x, y in ((22, -12), (-22, 12), (22, 12)):
    mcp.call("sketch", shape="circle", sketch_name="Sketch2", center=[x, y], diameter_mm=5)
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
cut = mcp.call("extrude_sketch", cut=True, sketch_name="Sketch2", through_all=True)
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
mcp.call("sketch", shape="circle", on=far_face[0]["id"], center=[0, 0], diameter_mm=4)
mcp.call("extrude_sketch", cut=True, depth_mm=2, reverse_direction=True, expect_error=True)
# Without it, the same sketch cuts into the plate.
mcp.call("extrude_sketch", cut=True, depth_mm=2)

mcp.call("render_views", views=["right", "isometric"])

# 4. Ids belong to their document (2026-10-18): a second part hands out the
# same sketch-entity bytes, which used to come back as the first part's ids
# ("line:8" for a circle) and resolve in the wrong document.
first_part_face = far_face[0]["id"]
mcp.call("create_part")
mcp.call("sketch", shape="circle", plane="front", center=[0, 0], diameter_mm=6)
mcp.call("sketch", shape="circle", sketch_name="Sketch1", center=[20, 0], diameter_mm=5)
new_ids = [e["id"] for e in mcp.call("inspect_sketch", sketch_name="Sketch1").json["segments"]]
assert len(new_ids) == 2 and all(i.startswith("circle:") for i in new_ids), new_ids
assert not set(new_ids) & set(circles), (new_ids, circles)
# The first part's ids are refused here, saying why.
r = mcp.call("sketch", shape="circle", on=first_part_face, center=[0, 0], diameter_mm=2, expect_error=True)
assert "is from another document" in r.text, r.text
r = mcp.call("add_sketch_relations", relations=[{"kind": "equal", "entities": [circles[0], new_ids[0]]}],
             expect_error=True)
assert "is from another document" in r.text, r.text

# A new sketch right after a dimension or relation closed a sketch: the first InsertSketch did
# nothing, in the same part and in a new one ("SolidWorks didn't open a sketch on the plane").
mcp.call("create_part")
r = mcp.call("sketch", shape="line", points=[[0, 0], [10, 0]])
line = re.findall(r"line:\d+", r.text)[0]
mcp.call("dimension_sketch", dimensions=[{"kind": "length", "entities": [line], "value_mm": 30}])
r = mcp.call("sketch", shape="line", points=[[0, 0], [10, 0]], plane="top")
line = re.findall(r"line:\d+", r.text)[0]
mcp.call("add_sketch_relations", relations=[{"kind": "horizontal", "entities": [line]}])
mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=10, height_mm=10)

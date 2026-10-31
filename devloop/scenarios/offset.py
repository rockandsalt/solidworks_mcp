# offset_sketch_entities: a rectangle outward, inward and to both sides, a
# contour from one of its lines, a whole sketch, a chain's side by the first
# entity, a line into a slot with caps, construction, the dimension driven by
# set_dimension, a ring extruded and checked by volume, a sketch a feature
# uses; then model edges and faces into new and existing sketches; refusals
# (too far inward, an inverted circle, wrong ids) must leave nothing behind.
# Run with:
#   py devloop/mcp_client.py run devloop/scenarios/offset.py
#
# Sketches are on the Front plane, in sketch coordinates.

import math
import re

TOL = 1e-3  # relative, for volumes


def volume():
    return sum(b["volume_mm3"] for b in mcp.call("inspect_model", what="bodies").json["bodies"])


def check_volume(expected, what):
    got = volume()
    assert abs(got - expected) <= TOL * expected, f"{what}: volume {got}, expected {expected}"


def features():
    return [f["name"] for f in mcp.call("inspect_model", what="features").json["features"]]


def ids(text, kind):
    return sorted(set(re.findall(rf"\b{kind}:\d+", text)))


def sketch_name(r):
    return re.search(r"Sketch\d+", r.text).group(0)


def sketch(name):
    return mcp.call("inspect_sketch", sketch_name=name).json


def new_part(text):
    return text.split("New:")[1].split(". ")[0]


def spans(name, new_ids=None):
    """(x min, x max, y min, y max) of the sketch's lines and arcs, or of those in new_ids."""
    xs, ys = [], []
    for s in sketch(name)["segments"]:
        if new_ids is not None and s["id"] not in new_ids:
            continue
        for key in ("start_mm", "end_mm"):
            if key in s:
                xs.append(s[key][0])
                ys.append(s[key][1])
        if s["type"] == "circle":
            xs += [s["center_mm"][0] - s["radius_mm"], s["center_mm"][0] + s["radius_mm"]]
            ys += [s["center_mm"][1] - s["radius_mm"], s["center_mm"][1] + s["radius_mm"]]
    return min(xs), max(xs), min(ys), max(ys)


def close(a, b, tol=1e-6):
    return len(a) == len(b) and all(abs(x - y) <= tol for x, y in zip(a, b))


def rectangle(width=40, height=20):
    r = mcp.call("sketch_line", points=[[-width / 2, -height / 2], [width / 2, -height / 2], [width / 2, height / 2], [-width / 2, height / 2]], closed=True)
    return sketch_name(r), ids(r.text, "line")


# ---- Sketch entities: which way, how many -----------------------------------

mcp.call("create_part")

name, lines = rectangle()
r = mcp.call("offset_sketch_entities", entities=lines, distance_mm=5)
assert r.text.startswith(f"offset {', '.join(lines)} by 5 mm outward in {name}. New: "), r.text
assert f"{name} now has 2 closed and 0 open contours" in r.text, r.text
new = ids(new_part(r.text), "line")
assert len(new) == 4 and not set(new) & set(lines), r.text
assert close(spans(name, new), (-25, 25, -15, 15)), spans(name, new)
dims = sketch(name)["dimensions"]
assert len(dims) == 1 and abs(dims[0]["value_mm"] - 5) < 1e-6, dims

# The dimension drives the offset.
r = mcp.call("set_dimension", name=dims[0]["name"], value_mm=8)
assert close(spans(name, new), (-28, 28, -18, 18)), (r.text, spans(name, new))

name, lines = rectangle()
r = mcp.call("offset_sketch_entities", entities=lines, distance_mm=5, side="inward", add_dimension=False)
new = ids(new_part(r.text), "line")
assert close(spans(name, new), (-15, 15, -5, 5)), spans(name, new)
assert "is a dimension" not in r.text and not sketch(name).get("dimensions"), r.text

name, lines = rectangle()
r = mcp.call("offset_sketch_entities", entities=lines[:1], distance_mm=2, side="both", chain=True)
assert len(ids(new_part(r.text), "line")) == 8, r.text
assert f"{name} now has 3 closed and 0 open contours" in r.text, r.text
assert close(spans(name), (-22, 22, -12, 12)), spans(name)

# A whole sketch, by name: both contours but not the construction line.
name, lines = rectangle()
mcp.call("sketch_circle", sketch_name=name, center=[60, 0], radius_mm=5)
mcp.call("sketch_line", sketch_name=name, points=[[-30, 30], [30, 30]], construction=True)
r = mcp.call("offset_sketch_entities", sketch_name=name, distance_mm=1)
assert r.text.startswith(f"offset the whole sketch by 1 mm outward in {name}."), r.text
assert len(ids(new_part(r.text), "line")) == 4 and len(ids(new_part(r.text), "circle")) == 1, r.text
circles = sorted(s["radius_mm"] for s in sketch(name)["segments"] if s["type"] == "circle")
assert close(circles, [5, 6]), circles

# An open chain goes to the right of its first entity, in the order given.
r = mcp.call("sketch_line", points=[[-10, 0], [10, 0]])
name = sketch_name(r)
[first] = ids(r.text, "line")
r = mcp.call("sketch_line", sketch_name=name, points=[[10, 10], [10, 0]])
[second] = ids(r.text, "line")
r = mcp.call("offset_sketch_entities", entities=[first, second], distance_mm=2)
new = ids(new_part(r.text), "line")
assert close(spans(name, new), (-10, 12, -2, 10)), spans(name, new)

r = mcp.call("sketch_line", points=[[-10, 0], [10, 0]])
name = sketch_name(r)
[first] = ids(r.text, "line")
r = mcp.call("sketch_line", sketch_name=name, points=[[10, 10], [10, 0]])
[second] = ids(r.text, "line")
r = mcp.call("offset_sketch_entities", entities=[second, first], distance_mm=2)
new = ids(new_part(r.text), "line")
assert close(spans(name, new), (-10, 8, 2, 10)), spans(name, new)

# Caps: a line into a closed slot.
r = mcp.call("sketch_line", points=[[-10, 0], [10, 0]])
name = sketch_name(r)
[center] = ids(r.text, "line")
r = mcp.call("offset_sketch_entities", entities=[center], distance_mm=5, side="both", caps="arcs", base_construction=True)
assert len(ids(new_part(r.text), "line")) == 2 and len(ids(new_part(r.text), "arc")) == 2, r.text
assert f"{name} now has 1 closed and 0 open contours" in r.text, r.text
info = sketch(name)
assert next(s for s in info["segments"] if s["id"] == center).get("construction"), info["segments"]
r = mcp.call("extrude_sketch", sketch_name=name, depth_mm=2)
slot = (20 * 10 + math.pi * 25) * 2
check_volume(slot, "a line into a slot")

name, lines = rectangle()
r = mcp.call("offset_sketch_entities", entities=lines, distance_mm=2, construction=True)
new = ids(new_part(r.text), "line")
assert all(s.get("construction") for s in sketch(name)["segments"] if s["id"] in new), sketch(name)["segments"]

# A spline offsets like the rest.
r = mcp.call("sketch_spline", points=[[0, 0], [10, 5], [20, 0]])
name = sketch_name(r)
[spline] = ids(r.text, "spline")
r = mcp.call("offset_sketch_entities", entities=[spline], distance_mm=1)
assert len(ids(new_part(r.text), "spline")) == 1, r.text

# ---- A ring, and a sketch a feature uses ------------------------------------

mcp.call("create_part")
r = mcp.call("sketch_circle", center=[0, 0], radius_mm=10)
ring = sketch_name(r)
[outer] = ids(r.text, "circle")
r = mcp.call("offset_sketch_entities", entities=[outer], distance_mm=2, side="inward")
[inner] = ids(new_part(r.text), "circle")
r = mcp.call("extrude_sketch", sketch_name=ring, depth_mm=10)
check_volume(math.pi * (100 - 64) * 10, "offset ring")

# Inside the used sketch: one more contour; the feature rebuilds with it.
r = mcp.call("offset_sketch_entities", entities=[inner], distance_mm=1, side="inward", add_dimension=False)
assert f"{ring} now has 3 closed" in r.text, r.text
r = mcp.call("delete_sketch_entities", entities=ids(new_part(r.text), "circle"))
check_volume(math.pi * (100 - 64) * 10, "after offsetting in a used sketch")

# ---- Refusals on sketch entities: nothing changes ---------------------------

name, lines = rectangle()
before = sketch(name)
for bad, why in [
    (dict(entities=lines, distance_mm=15, side="inward"), "couldn't offset"),
    (dict(entities=lines, distance_mm=0), "more than 0"),
    (dict(entities=lines, distance_mm=-5), "more than 0"),
    (dict(entities=lines, distance_mm=5, caps="arcs"), "caps only"),
    (dict(entities=lines, distance_mm=5, plane="top"), "new sketch"),
    (dict(entities=["Boss-Extrude1"], distance_mm=5), "sketch entity id"),
    (dict(distance_mm=5), "give the entities"),
]:
    r = mcp.call("offset_sketch_entities", expect_error=True, **bad)
    assert r.is_error and why in r.text, (bad, r.text)
after = sketch(name)
assert len(after["segments"]) == 4 and len(after["segments"]) == len(before["segments"]), after["segments"]
assert not after.get("dimensions"), after

# SolidWorks itself would turn these inside out without saying so.
r = mcp.call("sketch_circle", center=[0, 0], radius_mm=10)
name = sketch_name(r)
[circle] = ids(r.text, "circle")
for side in ("inward", "both"):
    r = mcp.call("offset_sketch_entities", entities=[circle], distance_mm=12, side=side, expect_error=True)
    assert r.is_error and "inside out" in r.text, r.text
    info = sketch(name)
    assert len(info["segments"]) == 1 and not info.get("dimensions"), info
r = mcp.call("sketch_point", sketch_name=name, points=[[1, 1]])
[point] = ids(r.text, "point")
r = mcp.call("offset_sketch_entities", entities=[point], distance_mm=1, expect_error=True)
assert r.is_error and "is a point" in r.text, r.text

# A sketch with only construction geometry.
r = mcp.call("sketch_line", points=[[0, 0], [10, 0]], construction=True)
r = mcp.call("offset_sketch_entities", sketch_name=sketch_name(r), distance_mm=1, expect_error=True)
assert r.is_error and "nothing to offset" in r.text, r.text

# ---- Model edges and faces --------------------------------------------------

mcp.call("create_part")
r = mcp.call("sketch_line", points=[[-20, -10], [20, -10], [20, 10], [-20, 10]], closed=True)
plate = sketch_name(r)
mcp.call("sketch_circle", sketch_name=plate, center=[0, 0], radius_mm=4)
mcp.call("extrude_sketch", sketch_name=plate, depth_mm=10)
plate_volume = (40 * 20 - math.pi * 16) * 10
check_volume(plate_volume, "plate")

faces = mcp.call("inspect_model", what="faces", surface_type="plane", limit=100).json["faces"]
face = next(f for f in faces if abs(f["area_mm2"] - (40 * 20 - math.pi * 16)) < 1e-3)["id"]
edges = mcp.call("inspect_model", what="edges", within=face, limit=20).json["edges"]
hole_edge = next(e for e in edges if e["type"] == "circle")["id"]
outer_edges = [e["id"] for e in edges if e["type"] != "circle"]
assert len(outer_edges) == 4, edges

# A face: a new sketch on itself, its outer loop only, fully defined.
r = mcp.call("offset_sketch_entities", entities=[face], distance_mm=2, side="inward")
lip = sketch_name(r)
assert r.text.startswith(f"offset {face} by 2 mm inward in {lip} (new sketch)."), r.text
assert "outer loop only" in r.text and f"{lip} now has 1 closed" in r.text, r.text
assert len(ids(new_part(r.text), "line")) == 4, r.text
assert sketch(lip)["status"] == "fully defined", sketch(lip)["status"]
r = mcp.call("cut_extrude_sketch", sketch_name=lip, depth_mm=1)
check_volume(plate_volume - (36 * 16 - math.pi * 16) * 1, "cut inside the offset face")

# Edges into a new sketch on a plane; the hole's edge grows outward. New edge
# and face ids after the cut.
faces = mcp.call("inspect_model", what="faces", surface_type="plane", limit=100).json["faces"]
edges = mcp.call("inspect_model", what="edges", limit=100).json["edges"]
rims = [e for e in edges if e["type"] == "circle"]
assert rims, edges
r = mcp.call("offset_sketch_entities", entities=[rims[0]["id"]], distance_mm=1, plane="front")
rim_sketch = sketch_name(r)
assert "(new sketch)" in r.text and "is a dimension" not in r.text, r.text
circle = next(s for s in sketch(rim_sketch)["segments"] if s["type"] == "circle")
assert abs(circle["radius_mm"] - 5) < 1e-6, circle

# Into an existing sketch made after the feature.
r = mcp.call("sketch_point", plane="front", points=[[50, 50]])
later = sketch_name(r)
r = mcp.call("offset_sketch_entities", entities=[rims[0]["id"]], distance_mm=1, sketch_name=later)
assert "(new sketch)" not in r.text and len(ids(new_part(r.text), "circle")) == 1, r.text

# Refusals on the model: no sketch left behind, and the sketch made before the
# edge's feature can't reach it.
tree = features()
for bad, why in [
    (dict(entities=[rims[0]["id"]], distance_mm=1), "say where"),
    (dict(entities=[rims[0]["id"]], distance_mm=1, plane="front", side="inward", construction=True), "only go with sketch"),
    (dict(entities=[rims[0]["id"]], distance_mm=1, plane="front", add_dimension=True), "only goes with sketch"),
    (dict(entities=[rims[0]["id"], "line:1"], distance_mm=1, plane="front"), "not both"),
    (dict(entities=[rims[0]["id"]], distance_mm=50, side="inward", plane="front"), "couldn't offset"),
    (dict(entities=[rims[0]["id"]], distance_mm=1, sketch_name=plate), "being edited"),
]:
    r = mcp.call("offset_sketch_entities", expect_error=True, **bad)
    assert r.is_error and why in r.text, (bad, r.text)
assert features() == tree, (tree, features())
check_volume(plate_volume - (36 * 16 - math.pi * 16) * 1, "after model refusals")

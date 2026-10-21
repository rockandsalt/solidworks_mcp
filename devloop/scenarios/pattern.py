# linear_pattern_sketch_entities / circular_pattern_sketch_entities /
# linear_pattern_features / circular_pattern_features: sketch circles, arcs,
# lines and points copied in rows, grids and round a center (positions
# checked against inspect_sketch), holes patterned in a grid along the
# default planes, along an edge and a sketch line, and round a disc's face,
# rim edge and a reference axis (hole positions and volumes checked), bodies
# patterned into separate bodies, and refusals that must leave nothing
# behind. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/pattern.py
#
# Sketches are on the Front plane, in sketch coordinates; model positions are
# mapped back into them through the sketch axes inspect_sketch reports (in
# this template Front is the YZ plane).

import math
import re

TOL = 1e-3  # relative, for volumes


def bodies():
    return mcp.call("inspect_model", what="bodies").json["bodies"]


def volume():
    return sum(b["volume_mm3"] for b in bodies())


def check_volume(expected, what):
    got = volume()
    assert abs(got - expected) <= TOL * expected, f"{what}: volume {got}, expected {expected}"


def features():
    return [f["name"] for f in mcp.call("inspect_model", what="features").json["features"]]


def ids(text, kind):
    return sorted(set(re.findall(rf"\b{kind}:\d+", text)))


def new_ids(text, kind):
    return ids(text.split("New:")[1].split(". Each copy")[0], kind)


def sketch_name(r):
    return re.search(r"Sketch\d+", r.text).group(0)


def sketch(name):
    return mcp.call("inspect_sketch", sketch_name=name).json


def close(a, b, tol=1e-4):
    return all(abs(x - y) <= tol for x, y in zip(a, b))


def same_set(got, expected):
    """Whether two lists of 2D points match, in any order."""
    key = lambda p: (round(p[0], 3), round(p[1], 3))  # noqa: E731
    got, expected = sorted(got, key=key), sorted(expected, key=key)
    return len(got) == len(expected) and all(close(g, e) for g, e in zip(got, expected))


def same_points(got, expected, what):
    assert same_set(got, expected), (what, got, expected)


def circle_centers(name):
    return [s["center_mm"] for s in sketch(name)["segments"] if s["type"] == "circle"]


def rotate(p, c, deg):
    a = math.radians(deg)
    dx, dy = p[0] - c[0], p[1] - c[1]
    return [c[0] + dx * math.cos(a) - dy * math.sin(a), c[1] + dx * math.sin(a) + dy * math.cos(a)]


# ---- Sketch: linear ---------------------------------------------------------

mcp.call("create_part")
r = mcp.call("sketch_circle", center=[10, 10], radius_mm=2)
grid = sketch_name(r)
[circle] = ids(r.text, "circle")
axes = sketch(grid)["sketch_axes"]
ORIGIN, AX, AY = axes["origin_mm"], axes["x"], axes["y"]


def to_sketch(p):
    d = [p[i] - ORIGIN[i] for i in range(3)]
    return [sum(d[i] * AX[i] for i in range(3)), sum(d[i] * AY[i] for i in range(3))]


r = mcp.call("linear_pattern_sketch_entities", entities=[circle], count=3, spacing_mm=10, count2=2, spacing2_mm=8)
assert r.text.startswith(f"patterned {circle} in {grid} (3 x 2 instances, 10 mm apart at 0 degrees and 8 mm "
                         "apart at 90 degrees). New: "), r.text
assert len(new_ids(r.text, "circle")) == 5, r.text
assert f"{grid} now has 6 closed and 0 open contours" in r.text, r.text
same_points(circle_centers(grid), [[x, y] for x in (10, 20, 30) for y in (10, 18)], "grid")

# A point and a line, leftwards: standalone points are reported, line ends not.
r = mcp.call("sketch_point", sketch_name=grid, points=[[0, -20]])
[point] = ids(r.text, "point")
r = mcp.call("sketch_line", sketch_name=grid, points=[[0, -30], [0, -25]])
[line] = ids(r.text, "line")
r = mcp.call("linear_pattern_sketch_entities", entities=[point, line], count=3, spacing_mm=10, angle_deg=180)
assert len(new_ids(r.text, "line")) == 2 and len(new_ids(r.text, "point")) == 2 + 4, r.text
info = sketch(grid)
for x in (-10, -20):
    assert sum(1 for p in info["points"] if close(p["at_mm"], [x, -20])) == 1, (x, info["points"])
    assert any(s["type"] == "line" and close(s["start_mm"] + s["end_mm"], [x, -30, x, -25]) for s in info["segments"])
# A column only (count 1, count2 rows) along 45 degrees.
r = mcp.call("linear_pattern_sketch_entities", entities=[point], count=1, count2=2, spacing2_mm=10, angle2_deg=45)
[p45] = new_ids(r.text, "point")
assert any(p["id"] == p45 and close(p["at_mm"], [10 / math.sqrt(2), -20 + 10 / math.sqrt(2)]) for p in sketch(grid)["points"])

# Refusals: nothing may change.
before = sketch(grid)
for bad, why in [
    (dict(entities=[circle], count=1), "count x count2"),
    (dict(entities=[circle], count=3), "give spacing_mm"),
    (dict(entities=[circle], count=2, spacing_mm=5, count2=2, spacing2_mm=5, angle2_deg=180), "parallel"),
    (dict(entities=["Boss-Extrude1"], count=2, spacing_mm=5), "a sketch entity id"),
]:
    r = mcp.call("linear_pattern_sketch_entities", expect_error=True, **bad)
    assert r.is_error and why in r.text, (bad, r.text)
after = sketch(grid)
assert len(after["segments"]) == len(before["segments"]) and len(after["points"]) == len(before["points"])

# ---- Sketch: circular -------------------------------------------------------

# A circle, an arc (box beyond its ends: it passes a quadrant) and a point,
# round a center that isn't the origin; the arc runs from 0 to 135 degrees.
r = mcp.call("sketch_circle", center=[30, 5], radius_mm=3)
ring = sketch_name(r)
[c0] = ids(r.text, "circle")
r = mcp.call("sketch_arc", sketch_name=ring, center=[40, 5], start=[44, 5],
             end=[40 - 4 / math.sqrt(2), 5 + 4 / math.sqrt(2)])
[arc] = ids(r.text, "arc")
r = mcp.call("sketch_point", sketch_name=ring, points=[[35, -5]])
[pt] = ids(r.text, "point")
center = [5, 5]
r = mcp.call("circular_pattern_sketch_entities", entities=[c0, arc, pt], center=center, count=4)
assert "(4 instances round [5, 5], 90 degrees apart counterclockwise)" in r.text, r.text
assert len(new_ids(r.text, "circle")) == 3 and len(new_ids(r.text, "arc")) == 3, r.text
info = sketch(ring)
same_points(circle_centers(ring), [rotate([30, 5], center, 90 * k) for k in range(4)], "circles round [5, 5]")
same_points([s["center_mm"] for s in info["segments"] if s["type"] == "arc"],
            [rotate([40, 5], center, 90 * k) for k in range(4)], "arcs round [5, 5]")
for k in range(4):
    assert any(close(p["at_mm"], rotate([35, -5], center, 90 * k)) for p in info["points"]), (k, info["points"])

# Stepped and clockwise, round the origin; spread over 90 degrees.
r = mcp.call("sketch_circle", center=[20, 0], radius_mm=1)
fan = sketch_name(r)
[c1] = ids(r.text, "circle")
mcp.call("circular_pattern_sketch_entities", entities=[c1], count=3, spacing_deg=30, reverse_direction=True)
same_points(circle_centers(fan), [rotate([20, 0], [0, 0], -30 * k) for k in range(3)], "clockwise 30")
r = mcp.call("sketch_circle", center=[0, 20], radius_mm=1)
spread = sketch_name(r)
[c2] = ids(r.text, "circle")
mcp.call("circular_pattern_sketch_entities", entities=[c2], count=3, angle_deg=90)
same_points(circle_centers(spread), [rotate([0, 20], [0, 0], 45 * k) for k in range(3)], "spread over 90")

for bad, why in [
    (dict(entities=[c2], count=4, spacing_deg=120), "don't fit in one turn"),
    (dict(entities=[c2], count=4, spacing_deg=30, angle_deg=90), "not both"),
    (dict(entities=[c2], count=1), "count must be 2"),
]:
    r = mcp.call("circular_pattern_sketch_entities", expect_error=True, **bad)
    assert r.is_error and why in r.text, (bad, r.text)
assert len(circle_centers(spread)) == 3

# ---- Features: linear --------------------------------------------------------

mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=100, height_mm=60)
mcp.call("extrude_sketch", depth_mm=10)
plate = 100 * 60 * 10
r = mcp.call("sketch_circle", center=[-40, -20], radius_mm=2)
r = mcp.call("cut_extrude_sketch", sketch_name=sketch_name(r), through_all=True)
cut = re.search(r"Cut-Extrude\d+", r.text).group(0)
hole = math.pi * 2 ** 2 * 10
check_volume(plate - hole, "one hole")


def hole_centers(radius):
    faces = mcp.call("inspect_model", what="faces", surface_type="cylinder", limit=500).json["faces"]
    return [to_sketch(f["axis_point_mm"]) for f in faces if abs(f["radius_mm"] - radius) < 1e-6]


# Along the default planes' normals (Right = sketch x, Top = sketch y here).
r = mcp.call("linear_pattern_features", features=[cut], direction="right", count=5, spacing_mm=20,
             direction2="top", count2=3, spacing2_mm=20)
assert re.match(rf"created LPattern\d+: 5 instances of {cut} along Right Plane, 20 mm apart, times 3 along "
                r"Top Plane, 20 mm apart \(15 in all\)\. The part has 1 solid body\.", r.text), r.text
check_volume(plate - 15 * hole, "5 x 3 grid")
same_points(hole_centers(2), [[x, y] for x in (-40, -20, 0, 20, 40) for y in (-20, 0, 20)], "grid holes")

# Along a straight edge (either way fits) and along a sketch line.
r = mcp.call("sketch_circle", center=[0, 25], radius_mm=1)
r = mcp.call("cut_extrude_sketch", sketch_name=sketch_name(r), through_all=True)
small = re.search(r"Cut-Extrude\d+", r.text).group(0)
small_hole = math.pi * 1 ** 2 * 10
edges = mcp.call("inspect_model", what="edges", curve_type="line", limit=500).json["edges"]


def along_x(e):
    a, b = to_sketch(e["start_mm"]), to_sketch(e["end_mm"])
    return abs(a[1] - b[1]) < 1e-6 and abs(a[0] - b[0]) > 50


edge = next(e["id"] for e in edges if along_x(e))
r = mcp.call("linear_pattern_features", features=[small], direction=edge, count=2, spacing_mm=10)
assert f"along {edge}, 10 mm apart" in r.text, r.text
check_volume(plate - 15 * hole - 2 * small_hole, "small hole along an edge")
xs = sorted(round(c[0], 6) for c in hole_centers(1))
assert xs in ([0, 10], [-10, 0]), xs

r = mcp.call("sketch_line", points=[[0, 0], [0, 10]])
[guide] = ids(r.text, "line")
r = mcp.call("sketch_circle", center=[-25, 0], radius_mm=1.5)
r = mcp.call("cut_extrude_sketch", sketch_name=sketch_name(r), through_all=True)
mid = re.search(r"Cut-Extrude\d+", r.text).group(0)
mid_hole = math.pi * 1.5 ** 2 * 10
r = mcp.call("linear_pattern_features", features=[mid], direction=guide, count=2, spacing_mm=20)
check_volume(plate - 15 * hole - 2 * small_hole - 2 * mid_hole, "along a sketch line")
ys = sorted(round(c[1], 6) for c in hole_centers(1.5))
assert ys in ([0, 20], [-20, 0]), ys

# Refusals: nothing may be added.
tree = features()
cylinder = mcp.call("inspect_model", what="faces", surface_type="cylinder", limit=1).json["faces"][0]["id"]
for bad, why in [
    (dict(features=[cut], direction=cylinder, count=2, spacing_mm=5), "isn't flat"),
    (dict(features=["NoSuchFeature"], direction="right", count=2, spacing_mm=5), "NoSuchFeature"),
    (dict(features=["Sketch1"], direction="right", count=2, spacing_mm=5), "is a sketch"),
    (dict(features=[cut], direction="right", count=1, spacing_mm=5), "count must be 2"),
    (dict(features=[cut], direction="right", count=2, spacing_mm=5, count2=2), "need direction2"),
    (dict(direction="right", count=2, spacing_mm=5), "give the features or the bodies"),
]:
    r = mcp.call("linear_pattern_features", expect_error=True, **bad)
    assert r.is_error and why in r.text, (bad, r.text)
# A copy of the hole 200 mm away lands beyond the plate and cuts nothing.
r = mcp.call("linear_pattern_features", features=[cut], direction="right", count=2, spacing_mm=200,
             reverse_direction=True, expect_error=True)
assert r.is_error and ("deleted again" in r.text or "couldn't pattern" in r.text), r.text
assert features() == tree, (tree, features())
check_volume(plate - 15 * hole - 2 * small_hole - 2 * mid_hole, "after refusals")

# ---- Features: circular ------------------------------------------------------

mcp.call("create_part")
r = mcp.call("sketch_circle", center=[0, 0], radius_mm=30)
mcp.call("extrude_sketch", sketch_name=sketch_name(r), depth_mm=10)
disc = math.pi * 30 ** 2 * 10
r = mcp.call("sketch_circle", center=[20, 0], radius_mm=2)
r = mcp.call("cut_extrude_sketch", sketch_name=sketch_name(r), through_all=True)
bolt = re.search(r"Cut-Extrude\d+", r.text).group(0)
rim = mcp.call("inspect_model", what="faces", surface_type="cylinder", limit=500).json["faces"]
rim = next(f["id"] for f in rim if abs(f["radius_mm"] - 30) < 1e-6)
r = mcp.call("circular_pattern_features", features=[bolt], axis=rim, count=6)
assert re.match(rf"created CirPattern\d+: 6 instances of {bolt} round {rim}, 60 degrees apart\. The part has "
                r"1 solid body\.", r.text), r.text
check_volume(disc - 6 * hole, "6 bolt holes")
same_points(hole_centers(2), [rotate([20, 0], [0, 0], 60 * k) for k in range(6)], "bolt circle")

# Round the rim edge, spread over 90 degrees: 0, 45, 90 one way or the other.
r = mcp.call("sketch_circle", center=[10, 0], radius_mm=1)
r = mcp.call("cut_extrude_sketch", sketch_name=sketch_name(r), through_all=True)
inner = re.search(r"Cut-Extrude\d+", r.text).group(0)
rim_edge = next(e["id"] for e in mcp.call("inspect_model", what="edges", curve_type="circle", limit=500).json["edges"]
                if abs(e["radius_mm"] - 30) < 1e-6)
r = mcp.call("circular_pattern_features", features=[inner], axis=rim_edge, count=3, angle_deg=90)
assert "45 degrees apart, spread over 90 degrees" in r.text, r.text
check_volume(disc - 6 * hole - 3 * small_hole, "inner holes")
got = hole_centers(1)
assert any(same_set(got, [rotate([10, 0], [0, 0], s * 45 * k) for k in range(3)]) for s in (1, -1)), got

# Round a reference axis (Top and Right planes meet along the disc's axis), stepped.
r = mcp.call("create_reference_axis", references=["top", "right"])
axis = re.search(r"Axis\d+", r.text).group(0)
r = mcp.call("sketch_circle", center=[0, -15], radius_mm=1.5)
r = mcp.call("cut_extrude_sketch", sketch_name=sketch_name(r), through_all=True)
third = re.search(r"Cut-Extrude\d+", r.text).group(0)
r = mcp.call("circular_pattern_features", features=[third], axis=axis, count=2, spacing_deg=30)
assert f"round {axis}, 30 degrees apart." in r.text, r.text
check_volume(disc - 6 * hole - 3 * small_hole - 2 * mid_hole, "stepped round an axis")
got = hole_centers(1.5)
assert any(same_set(got, [rotate([0, -15], [0, 0], s * 30 * k) for k in range(2)]) for s in (1, -1)), got

tree = features()
flat = mcp.call("inspect_model", what="faces", surface_type="plane", limit=1).json["faces"][0]["id"]
for bad, why in [
    (dict(features=[bolt], axis=flat, count=3), "isn't cylindrical or conical"),
    (dict(features=[bolt], axis="front", count=3), "is a plane"),
    (dict(features=[bolt], axis=axis, count=4, spacing_deg=120), "don't fit"),
]:
    r = mcp.call("circular_pattern_features", expect_error=True, **bad)
    assert r.is_error and why in r.text, (bad, r.text)
assert features() == tree, (tree, features())

# ---- Bodies -------------------------------------------------------------------

mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=20, height_mm=10)
mcp.call("extrude_sketch", depth_mm=10)
[b] = bodies()
r = mcp.call("linear_pattern_features", bodies=[b["id"]], direction="right", count=3, spacing_mm=30)
assert "3 instances of 1 body along Right Plane" in r.text and "3 solid bodies" in r.text, r.text
check_volume(3 * 20 * 10 * 10, "three blocks")
r = mcp.call("linear_pattern_features", bodies=[b["id"]], features=["Boss-Extrude1"], direction="right", count=2,
             spacing_mm=30, expect_error=True)
assert r.is_error and "not both" in r.text, r.text

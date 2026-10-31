# create_helix and 3D sketches (sketch_* with sketch_3d). Helixes: each way of
# defining one (ends and length checked against the formula), clockwise,
# reversed, tapered, a spiral; a wire and a profile swept along one, a thread
# cut with one, checked by volume; refusals and deleting. 3D sketches: a chain
# of lines, an arc and a spline drawn over several calls (positions read back
# through inspect_sketch, no stray arc points), a rod swept along it (volume),
# a guide curve for a loft, relations along the axes, dimensions, deleting
# entities; refusals. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/helix3d.py
#
# The template's default planes don't face the standard way (devloop.md), so
# directions come from the planes' normals: u = Front's, v = Right's, w = Top's.

import json
import math
import re

TOL = 1e-3  # relative, for volumes and lengths
EPS = 1e-3  # mm, for positions


def add(*vs): return [sum(c) for c in zip(*vs)]
def sub(a, b): return [x - y for x, y in zip(a, b)]
def scale(a, k): return [x * k for x in a]
def dot(a, b): return sum(x * y for x, y in zip(a, b))
def norm(a): return math.sqrt(dot(a, a))


def at(a=0.0, b=0.0, c=0.0):
    """Model point a*u + b*v + c*w, in mm."""
    return add(scale(u, a), scale(v, b), scale(w, c))


def close(a, b, what):
    assert norm(sub(a, b)) <= EPS, f"{what}: {a}, expected {b}"


def body():
    bodies = mcp.call("inspect_model", what="bodies").json["bodies"]
    assert len(bodies) == 1, bodies
    return bodies[0]


def check_volume(expected, what):
    got = body()["volume_mm3"]
    assert abs(got - expected) <= TOL * expected, f"{what}: volume {got}, expected {expected}"


def features():
    return [f["name"] for f in mcp.call("inspect_model", what="features").json["features"]]


def circle(center_a=0.0, center_b=0.0, radius=10.0):
    """A circle on Top around a*u + b*v; returns the sketch's name."""
    r = mcp.call("sketch_circle", plane="top", center=at(center_a, center_b), radius_mm=radius)
    return re.search(r"Sketch\d+", r.text).group(0)


def ends(r):
    """A create_helix reply's start and end points."""
    m = re.search(r"runs from (\[[^\]]*\]) mm to (\[[^\]]*\]) mm, ([\d.]+) mm long", r.text)
    assert m, r.text
    return json.loads(m.group(1)), json.loads(m.group(2)), float(m.group(3))


def made(r):
    """The JSON of the features a reference geometry call made."""
    return json.loads(r.text[r.text.rindex("\n[") + 1:])


def feature_of(r):
    return re.search(r"created (\S+)", r.text).group(1)


def helix_length(radius, pitch, turns):
    return turns * math.hypot(2 * math.pi * radius, pitch)


def check_length(got, expected, what):
    assert abs(got - expected) <= TOL * expected, f"{what}: length {got}, expected {expected}"


mcp.call("create_part")
planes = {f["name"]: f for f in mcp.call("inspect_model", what="features").json["features"]}
u, v, w = (planes[n]["normal"] for n in ("Front Plane", "Right Plane", "Top Plane"))

# ---- Each definition: 4 turns of 5 mm pitch on a 20 mm circle, 20 mm high ---

expected = helix_length(10, 5, 4)
r = mcp.call("create_helix", pitch_mm=5, revolutions=4, sketch_name=circle())
assert "4 turns" in r.text and "pitch 5 mm" in r.text and "height 20 mm" in r.text, r.text
start, end, length = ends(r)
check_length(length, expected, "pitch + revolutions")
along = dot(sub(end, start), w)
assert abs(abs(along) - 20) <= EPS, (start, end)
# Whole turns: the end is straight above the start, one radius off the axis.
close(sub(end, start), scale(w, along), "whole turns")
assert abs(norm(start) - 10) <= EPS, start
first_along = along

# The latest unused sketch by default; height + revolutions gives the pitch.
circle(50)
r = mcp.call("create_helix", height_mm=30, revolutions=3)
assert "pitch 10 mm" in r.text and "height 30 mm" in r.text, r.text
check_length(ends(r)[2], helix_length(10, 10, 3), "height + revolutions")

# Height + pitch gives the turns.
r = mcp.call("create_helix", height_mm=20, pitch_mm=4, sketch_name=circle(100))
assert "5 turns" in r.text, r.text
check_length(ends(r)[2], helix_length(10, 4, 5), "height + pitch")

# Half a turn: the end is across the circle from the start.
r = mcp.call("create_helix", pitch_mm=10, revolutions=0.5, sketch_name=circle(150))
start, end, length = ends(r)
c = at(150)
close(sub(end, c), add(scale(sub(start, c), -1), scale(w, dot(sub(end, c), w))), "half a turn")

# Reversed: to the other side of the plane.
r = mcp.call("create_helix", pitch_mm=5, revolutions=4, reverse_direction=True, sketch_name=circle(200))
start, end, _ = ends(r)
assert abs(dot(sub(end, start), w) + first_along) <= EPS, (start, end)

# Clockwise, three quarters of a turn: ends on the other side from counterclockwise.
c = at(250)
r = mcp.call("create_helix", pitch_mm=4, revolutions=0.75, sketch_name=circle(250))
ccw_start, ccw_end, _ = ends(r)
r = mcp.call("create_helix", pitch_mm=4, revolutions=0.75, clockwise=True, sketch_name=circle(250, 50))
cw_start, cw_end, _ = ends(r)
c2 = at(250, 50)
ccw_turn = dot(sub(ccw_start, c), sub(ccw_end, c)), dot(sub(cw_start, c2), sub(cw_end, c2))
assert abs(ccw_turn[0] - ccw_turn[1]) <= EPS, ccw_turn
level = lambda p, centre: sub(sub(p, centre), scale(w, dot(sub(p, centre), w)))
assert norm(sub(level(ccw_end, c), level(cw_end, c2))) > 1, (ccw_end, cw_end)

# Start angle: a quarter of the way round, still one radius from the axis.
c = at(300)
r = mcp.call("create_helix", pitch_mm=5, revolutions=2, sketch_name=circle(300))
s0 = sub(ends(r)[0], c)
r = mcp.call("create_helix", pitch_mm=5, revolutions=2, start_angle_deg=90, sketch_name=circle(350))
s90 = sub(ends(r)[0], at(350))
assert abs(dot(s0, s90)) <= EPS and abs(norm(s90) - 10) <= EPS, (s0, s90)

# Taper: the radius grows outward by height * tan(angle), or shrinks inward.
taper = lambda outward, a: mcp.call("create_helix", height_mm=20, revolutions=4, taper_deg=10,
                                     taper_outward=outward, sketch_name=circle(a))
grown = 10 + 20 * math.tan(math.radians(10))
shrunk = 10 - 20 * math.tan(math.radians(10))
for outward, a, radius in ((True, 400, grown), (False, 450, shrunk)):
    _, end, _ = ends(taper(outward, a))
    assert abs(norm(level(end, at(a))) - radius) <= EPS, (outward, end, radius)

# A spiral stays in the sketch's plane; the radius grows by the pitch per turn.
c = at(500)
r = mcp.call("create_helix", spiral=True, pitch_mm=4, revolutions=3, sketch_name=circle(500))
assert "(spiral)" in r.text and "height" not in r.text.split(";")[0], r.text
start, end, _ = ends(r)
assert abs(dot(sub(end, c), w)) <= EPS and abs(norm(sub(end, c)) - 22) <= EPS, (start, end)

# ---- Refusals leave nothing behind -----------------------------------------

line_sketch = re.search(r"Sketch\d+", mcp.call("sketch_line", plane="top", points=[at(0, 100), at(10, 100)]).text).group(0)
two = circle(0, 150)
mcp.call("sketch_circle", sketch_name=two, center=at(0, 180), radius_mm=5)
before = features()
for args, needle in (
    (dict(sketch_name=line_sketch, pitch_mm=5, revolutions=4), "exactly one circle"),
    (dict(sketch_name=two, pitch_mm=5, revolutions=4), "2 circle"),
    (dict(sketch_name="NoSuchSketch", pitch_mm=5, revolutions=4), "NoSuchSketch"),
    (dict(sketch_name=two, pitch_mm=5), "exactly two"),
    (dict(sketch_name=two, pitch_mm=5, height_mm=20, revolutions=4), "exactly two"),
    (dict(sketch_name=two, pitch_mm=-5, revolutions=4), "more than 0"),
    (dict(sketch_name=two, spiral=True, height_mm=5, revolutions=4), "spiral"),
    (dict(sketch_name=two, pitch_mm=5, revolutions=4, taper_outward=True), "taper_deg"),
):
    r = mcp.call("create_helix", expect_error=True, **args)
    assert r.is_error and needle in r.text, (args, r.text)
assert features() == before

# What SolidWorks itself refuses: narrowing to nothing; a very steep taper.
narrow = circle(0, 200, radius=2)
r = mcp.call("create_helix", sketch_name=narrow, height_mm=50, revolutions=5, taper_deg=60, expect_error=True)
print("taper to nothing:", r.is_error, r.text)
if r.is_error:
    assert "couldn't make a helix" in r.text or "deleted again" in r.text, r.text

# ---- Deleting: the sketch can't go while the helix uses it ------------------

r = mcp.call("create_helix", pitch_mm=5, revolutions=4, sketch_name=circle(0, 250))
victim = feature_of(r)
assert victim in features(), features()
r = mcp.call("delete_features", features=[victim])
assert "Kept what it had absorbed" in r.text, r.text
assert victim not in features(), features()

# ---- A wire along a helix: pi r^2 * length ----------------------------------

mcp.call("create_part")
r = mcp.call("create_helix", pitch_mm=5, revolutions=4, sketch_name=circle())
path = feature_of(r)
start, end, length = ends(r)
r = mcp.call("sweep_sketch", path=path, circle_diameter_mm=2)
assert path in r.text, r.text
check_volume(math.pi * 1 ** 2 * length, "wire along a helix")
mcp.call("render_views")

# ---- A square profile through the start, on the default plane holding it ----

mcp.call("create_part")
r = mcp.call("create_helix", pitch_mm=5, revolutions=3, sketch_name=circle())
path = feature_of(r)
start, _, _ = ends(r)
plane = next(p for p, n in (("front", u), ("right", v)) if abs(dot(start, n)) <= EPS)
radial = scale(start, 1 / norm(start))
h = 1.5
corners = [add(scale(radial, 10 + a), scale(w, b)) for a, b in ((-h, -h), (h, -h), (h, h), (-h, h))]
mcp.call("sketch_line", plane=plane, closed=True, points=corners)
r = mcp.call("sweep_sketch", path=path)
assert path in r.text, r.text
# Each section stays in a plane through the axis, so Pappus: area * 2 pi R * turns.
check_volume(3 * 3 * 2 * math.pi * 10 * 3, "square wire along a helix")

# ---- A thread: a cylinder with a groove cut along a helix -------------------

mcp.call("create_part")
base = circle(radius=10)
mcp.call("extrude_sketch", sketch_name=base, depth_mm=30)
solid = body()["volume_mm3"]
r = mcp.call("create_helix", pitch_mm=5, revolutions=3, sketch_name=circle())
path = feature_of(r)
r = mcp.call("cut_sweep_sketch", path=path, circle_diameter_mm=1.5)
assert "Cut-Sweep" in r.text, r.text
got = body()["volume_mm3"]
assert 0 < solid - got < solid * 0.2, (solid, got)

# ---- 3D sketch: lines, then more lines, an arc and a spline, in later calls --

mcp.call("create_part")
p0, p1, p2, p3 = [0, 0, 0], [40, 0, 0], [40, 30, 0], [40, 30, 50]
r = mcp.call("sketch_line", sketch_3d=True, points=[p0, p1, p2])
assert "new 3D sketch" in r.text and "not extruded" in r.text and "Sketch axes" not in r.text, r.text
path = re.search(r"3DSketch\d+", r.text).group(0)
assert "0 closed and 1 open" in r.text, r.text
r = mcp.call("sketch_line", sketch_name=path, points=[p2, p3])
assert "0 closed and 1 open" in r.text, r.text  # joined to the chain
s = mcp.call("inspect_sketch", sketch_name=path).json
assert s["is_3d"] is True and "sketch_axes" not in s, s
assert len(s["segments"]) == 3 and len(s["points"]) == 4, s
assert sorted(tuple(p["at_mm"]) for p in s["points"]) == sorted(tuple(map(float, p)) for p in (p0, p1, p2, p3)), s["points"]
assert mcp.call("inspect_sketch").json["sketch"] == path  # the latest, 3D or not

# A rod along it: about pi r^2 * 120, the sharp corners mitred by SolidWorks, so
# only roughly; exactly along the smooth paths below.
r = mcp.call("sweep_sketch", path=path, circle_diameter_mm=4)
got = body()["volume_mm3"]
assert abs(got - math.pi * 2 ** 2 * 120) <= 0.05 * math.pi * 2 ** 2 * 120, got

# An arc, tangent or not: its three points set its plane, off every default one.
mcp.call("create_part")
a, b, t = [30, 0, 0], [0, 30, 30], [20, 20, 20]
r = mcp.call("sketch_arc", sketch_3d=True, start=a, end=b, through=t)
arc_sketch = re.search(r"3DSketch\d+", r.text).group(0)
s = mcp.call("inspect_sketch", sketch_name=arc_sketch).json
assert [g["type"] for g in s["segments"]] == ["arc"], s
arc = s["segments"][0]
close(arc["start_mm"], a, "arc start")
close(arc["end_mm"], b, "arc end")
# start, end, center: the hidden point on the arc's axis isn't listed.
assert len(s["points"]) == 3, s["points"]
center, radius = arc["center_mm"], arc["radius_mm"]
for p in (a, b, t):
    assert abs(norm(sub(p, center)) - radius) <= EPS, (p, center, radius)
# Add a spline on from its end and a stand-alone point.
spline_points = [b, [-10, 50, 40], [-30, 60, 70]]
r = mcp.call("sketch_spline", sketch_name=arc_sketch, points=spline_points)
assert "0 closed and 1 open" in r.text, r.text
r = mcp.call("sketch_point", sketch_name=arc_sketch, points=[[1, 2, 3]])
assert "point:" in r.text, r.text
s = mcp.call("inspect_sketch", sketch_name=arc_sketch).json
assert any(p["at_mm"] == [1.0, 2.0, 3.0] for p in s["points"]), s["points"]
spline = next(g for g in s["segments"] if g["type"] == "spline")
close(spline["end_mm"], spline_points[-1], "spline end")

# A rod swept along an arc in space: pi r^2 * arc length, whatever its plane.
mcp.call("create_part")
r = mcp.call("sketch_arc", sketch_3d=True, start=a, end=b, through=t)
path = re.search(r"3DSketch\d+", r.text).group(0)
arc = mcp.call("inspect_sketch", sketch_name=path).json["segments"][0]
r = mcp.call("sweep_sketch", path=path, circle_diameter_mm=4)
assert path in r.text, r.text
check_volume(math.pi * 2 ** 2 * arc["length_mm"], "rod along a 3D arc")

# ---- A spline through space as a sweep path: the volume follows its length --

mcp.call("create_part")
r = mcp.call("sketch_spline", sketch_3d=True, points=[[0, 0, 0], [20, 10, 10], [40, 0, 30], [60, -10, 30]])
path = re.search(r"3DSketch\d+", r.text).group(0)
length = mcp.call("inspect_sketch", sketch_name=path).json["segments"][0]["length_mm"]
mcp.call("sweep_sketch", path=path, circle_diameter_mm=3)
check_volume(math.pi * 1.5 ** 2 * length, "rod along a 3D spline")

# ---- A 3D guide curve for a loft --------------------------------------------

mcp.call("create_part")
r = mcp.call("sketch_circle", plane="front", center=at(0, 0), radius_mm=10)
first = re.search(r"Sketch\d+", r.text).group(0)
[p] = made(mcp.call("create_reference_plane", references=[{"entity": "front", "offset_mm": 50}]))
r = mcp.call("sketch_circle", on=p["id"], center=at(dot(p["point_on_plane_mm"], u), 0), radius_mm=10)
second = re.search(r"Sketch\d+", r.text).group(0)
offset = dot(p["point_on_plane_mm"], u)
r = mcp.call("sketch_spline", sketch_3d=True, points=[at(0, 10), at(offset / 2, 13, 2), at(offset, 10)])
guide = re.search(r"3DSketch\d+", r.text).group(0)
r = mcp.call("loft_sketch", profiles=[first, second], guide_curves=[guide])
# A guide on a circle moves the whole section along it (architecture.md), so the
# volume stays that of the straight cylinder within the guide's small bow.
check_volume(math.pi * 10 ** 2 * abs(offset), "cylinder along a 3D guide")

# ---- Relations, dimensions, deleting ----------------------------------------

mcp.call("create_part")
r = mcp.call("sketch_line", sketch_3d=True, points=[[0, 0, 0], [30, 2, 1], [32, 40, 3], [33, 41, 60]])
path = re.search(r"3DSketch\d+", r.text).group(0)
lines = re.findall(r"line:\d+", r.text)
r = mcp.call("add_sketch_relations", relations=[
    {"kind": "along_x", "entities": [lines[0]]},
    {"kind": "along_y", "entities": [lines[1]]},
    {"kind": "along_z", "entities": [lines[2]]},
])
assert "added 3 relations" in r.text, r.text
s = mcp.call("inspect_sketch", sketch_name=path).json
assert sorted(x["type"] for x in s["relations"]) == ["along x", "along y", "along z"], s["relations"]
by_id = {g["id"]: g for g in s["segments"]}
for line, axis in zip(lines, range(3)):
    d = sub(by_id[line]["end_mm"], by_id[line]["start_mm"])
    off = [abs(x) for i, x in enumerate(d) if i != axis]
    assert max(off) <= EPS, (line, axis, d)

r = mcp.call("dimension_sketch", dimensions=[{"kind": "length", "entities": [lines[0]], "value_mm": 25}])
name = re.search(r"D\d+@\S+", r.text).group(0)
assert path in name, r.text
s = mcp.call("inspect_sketch", sketch_name=path).json
g = {g["id"]: g for g in s["segments"]}[lines[0]]
assert abs(g["length_mm"] - 25) <= EPS, g
mcp.call("set_dimension", name=name, value_mm=35)
g = {g["id"]: g for g in mcp.call("inspect_sketch", sketch_name=path).json["segments"]}[lines[0]]
assert abs(g["length_mm"] - 35) <= EPS, g

# Refused: what only makes sense in a plane.
for tool, args, needle in (
    ("add_sketch_relations", dict(relations=[{"kind": "horizontal", "entities": [lines[1]]}]), "along_x"),
    ("dimension_sketch", dict(dimensions=[{"kind": "horizontal", "entities": [lines[1]]}]), "no horizontal"),
    ("sketch_circle", dict(sketch_name=path, center=[0, 0, 0], radius_mm=5), "can't take a circle"),
    ("sketch_arc", dict(sketch_name=path, start=[0, 0, 0], end=[5, 0, 0], center=[2, 2, 2]), "through"),
    ("sketch_line", dict(sketch_name=path, points=[[0, 0], [5, 5]]), "[x, y, z]"),
    ("sketch_line", dict(sketch_name=path, points=[[1, 1, 1], [1, 1, 1]]), "same point"),
    ("sketch_line", dict(sketch_3d=True, points=[[0, 0], [5, 5]]), "[x, y, z]"),
    ("sketch_line", dict(sketch_3d=True, plane="top", points=[[0, 0, 0], [5, 5, 5]]), "plane"),
    ("sketch_line", dict(sketch_3d=True, sketch_name=path, points=[[0, 0, 0], [5, 5, 5]]), "sketch_3d"),
    ("extrude_sketch", dict(sketch_name=path, depth_mm=5), "3D sketch"),
    ("create_helix", dict(sketch_name=path, pitch_mm=5, revolutions=2), "3D sketch"),
    ("fully_define_sketch", dict(sketch_name=path), "3D sketch"),
    ("mirror_sketch_entities", dict(sketch_name=path, entities=[lines[0]], axis=lines[1]), "3D sketch"),
    ("mirror_sketch_entities", dict(entities=[lines[0]], axis=lines[1]), "3D sketch"),
):
    r = mcp.call(tool, expect_error=True, **args)
    assert r.is_error and needle in r.text, (tool, args, r.text)
s2 = mcp.call("inspect_sketch", sketch_name=path).json
assert len(s2["segments"]) == 3, s2["segments"]

# A failed new 3D sketch is gone again.
before = features()
r = mcp.call("sketch_circle", sketch_3d=True, center=[0, 0, 0], radius_mm=5, expect_error=True)
assert r.is_error and "can't take a circle" in r.text, r.text
assert features() == before, (before, features())

# A horizontal relation is still fine in 2D, along_x isn't.
r = mcp.call("sketch_line", plane="top", points=[at(0, 0), at(10, 1)])
flat_line = re.search(r"line:\d+", r.text).group(0)
r = mcp.call("add_sketch_relations", relations=[{"kind": "along_x", "entities": [flat_line]}], expect_error=True)
assert r.is_error and "3D sketches" in r.text, r.text
mcp.call("add_sketch_relations", relations=[{"kind": "horizontal", "entities": [flat_line]}])

# Deleting from a 3D sketch, and the whole sketch.
r = mcp.call("delete_sketch_entities", entities=[lines[2]])
assert "fully_define_sketch" not in r.text, r.text
s = mcp.call("inspect_sketch", sketch_name=path).json
assert len(s["segments"]) == 2, s["segments"]
mcp.call("delete_features", features=[path])
assert path not in features()

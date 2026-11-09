# loft_sketch, also with cut: a square frustum (two squares), the same
# with normal tangency at both ends, a pyramid (square to a point), a block
# with a frustum-shaped pocket cut into it (plain and normal-tangent, both
# ways round), a cylinder bent along a guide curve, and a closed ring through three planes round an axis, each checked
# by the body's volume and box; refusals must leave nothing behind. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/loft.py
#
# The template's default planes don't face the standard way (devloop.md), so
# everything is drawn in model coordinates along the planes' normals from
# inspect_model: u = Front's normal (the loft direction), v = Right's normal,
# w = Top's normal (both lying in Front).

import json
import math
import re

TOL = 1e-3  # relative, for volumes


def add(*vs): return [sum(c) for c in zip(*vs)]
def scale(a, k): return [x * k for x in a]
def dot(a, b): return sum(x * y for x, y in zip(a, b))
def cross(a, b): return [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
def unit(a): return scale(a, 1 / math.sqrt(dot(a, a)))


def at(a=0.0, b=0.0, c=0.0):
    """Model point a*u + b*v + c*w, in mm."""
    return add(scale(u, a), scale(v, b), scale(w, c))


def body():
    bodies = mcp.call("inspect_model", what="bodies").json["bodies"]
    assert len(bodies) == 1, bodies
    return bodies[0]


def check_volume(expected, what):
    got = body()["volume_mm3"]
    assert abs(got - expected) <= TOL * expected, f"{what}: volume {got}, expected {expected}"
    return got


def extent(direction):
    """The body's size along a model axis direction (u, v or w), mm."""
    box = body()["bbox_mm"]
    return sum(abs(d) * (hi - lo) for d, lo, hi in zip(direction, box["min"], box["max"]))


def features():
    return [f["name"] for f in mcp.call("inspect_model", what="features").json["features"]]


def sketch_name(r):
    return re.search(r"Sketch\d+", r.text).group(0)


def made(r):
    """The JSON of the features a reference geometry call made."""
    return json.loads(r.text[r.text.rindex("\n[") + 1:])


def new_part():
    """A new part, and a plane parallel to Front 30 mm away: its id and its u."""
    global u, v, w
    mcp.call("create_part")
    planes = {f["name"]: f for f in mcp.call("inspect_model", what="features").json["features"]}
    u, v, w = (planes[n]["normal"] for n in ("Front Plane", "Right Plane", "Top Plane"))
    [p] = made(mcp.call("create_reference_geometry", geometry="plane", references=[{"entity": "front", "offset_mm": 30}]))
    d = dot(p["point_on_plane_mm"], u)
    assert abs(abs(d) - 30) < 1e-6, p
    return p["id"], d


def square(a, size, center_v=0.0, center_w=0.0, **target):
    """A closed square at u = a, in a new sketch on `target` (plane= / on=)."""
    h = size / 2
    return sketch_name(mcp.call("sketch", shape="line", closed=True, points=[
        at(a, center_v - h, center_w - h), at(a, center_v + h, center_w - h),
        at(a, center_v + h, center_w + h), at(a, center_v - h, center_w + h)], **target))


frustum = 30 / 3 * (20 * 20 + 10 * 10 + 20 * 10)

# ---- Boss: a 20 -> 10 square frustum, 30 long -------------------------------

far, d = new_part()
base = square(0, 20, plane="front")
top_ = square(d, 10, on=far)
r = mcp.call("loft_sketch", profiles=[base, top_])
assert r.text.startswith("created Loft") and f"through {base} and {top_}." in r.text, r.text
check_volume(frustum, "frustum")
assert abs(extent(u) - 30) < 1e-3 and abs(extent(v) - 20) < 1e-3 and abs(extent(w) - 20) < 1e-3

# ---- The same with normal tangency at both ends: the sides leave and meet the
# squares square-on, so the body bulges (more volume) but stays in the box ---

far, d = new_part()
base = square(0, 20, plane="front")
top_ = square(d, 10, on=far)
r = mcp.call("loft_sketch", profiles=[base, top_], start_normal=True, end_normal=True)
assert "normal to the first and last profiles" in r.text, r.text
normal_loft = got = body()["volume_mm3"]
assert got > frustum * 1.01, (got, frustum)
assert abs(extent(u) - 30) < 1e-3 and abs(extent(v) - 20) < 1e-3, (extent(u), extent(v))
# The other way round (against the sketches' normals): the same shape.
far, d = new_part()
base = square(0, 20, plane="front")
top_ = square(d, 10, on=far)
mcp.call("loft_sketch", profiles=[top_, base], start_normal=True, end_normal=True)
check_volume(got, "normal tangency, reversed")
assert abs(extent(u) - 30) < 1e-3 and abs(extent(v) - 20) < 1e-3, (extent(u), extent(v))

# ---- A pyramid: square to a point --------------------------------------------

far, d = new_part()
base = square(0, 20, plane="front")
tip = sketch_name(mcp.call("sketch", shape="point", on=far, points=[at(d)]))
mcp.call("loft_sketch", profiles=[base, tip])
check_volume(20 * 20 * 30 / 3, "pyramid")

# ---- Cut: a 40 x 40 x 30 block (itself a loft) with the frustum cut out ----

far, d = new_part()
mcp.call("loft_sketch", profiles=[square(0, 40, plane="front"), square(d, 40, on=far)])
block = check_volume(40 * 40 * 30, "block")
r = mcp.call("loft_sketch", cut=True, profiles=[square(0, 20, plane="front"), square(d, 10, on=far)])
assert r.text.startswith("created Cut-Loft") and "cutting a loft" in r.text, r.text
check_volume(block - frustum, "block with a pocket")

# The same pocket with normal tangency, both ways round: the boss's shape.
for reverse in (False, True):
    far, d = new_part()
    mcp.call("loft_sketch", profiles=[square(0, 40, plane="front"), square(d, 40, on=far)])
    profiles = [square(0, 20, plane="front"), square(d, 10, on=far)]
    mcp.call("loft_sketch", cut=True, profiles=profiles[::-1] if reverse else profiles, start_normal=True, end_normal=True)
    check_volume(block - normal_loft, f"block with a normal-tangent pocket, reversed={reverse}")

# ---- A guide curve: two 20 mm circles joined along an arc bowing 5 mm out ----
# A circle has no corner to hold, so the whole section follows the guide: the
# same volume as the straight cylinder, its centroid the arc's mean offset.

far, d = new_part()
c1 = sketch_name(mcp.call("sketch", shape="circle", plane="front", center=at(0), radius_mm=10))
c2 = sketch_name(mcp.call("sketch", shape="circle", on=far, center=at(d), radius_mm=10))
guide = sketch_name(mcp.call("sketch", shape="arc", plane="top", start=at(0, 10), end=at(d, 10), through=at(d / 2, 15)))
r = mcp.call("loft_sketch", profiles=[c1, c2], guide_curves=[guide])
assert f"along guide curve {guide}" in r.text, r.text
check_volume(math.pi * 10 ** 2 * 30, "cylinder along a guide")
R = (15 ** 2 + 5 ** 2) / (2 * 5)  # the arc's radius; its center is R - 5 below the chord
mean_offset = (15 * math.sqrt(R * R - 225) + R * R * math.asin(15 / R)) / 30 - (R - 5)
shift = dot(body()["centroid_mm"], v)
assert abs(shift - mean_offset) < 0.01, (shift, mean_offset)

# ---- Refusals leave nothing behind -------------------------------------------

before = features()
v0 = body()["volume_mm3"]
r = mcp.call("loft_sketch", profiles=["NoSuchSketch", c2], expect_error=True)
assert r.is_error and "NoSuchSketch" in r.text, r.text
r = mcp.call("loft_sketch", profiles=[c1], expect_error=True)
assert r.is_error and "at least 2" in r.text, r.text
r = mcp.call("loft_sketch", profiles=[c1, c1], expect_error=True)
assert r.is_error and "more than once" in r.text, r.text
r = mcp.call("loft_sketch", profiles=[c1, c2], guide_curves=[c2], expect_error=True)
assert r.is_error and "more than once" in r.text, r.text
r = mcp.call("loft_sketch", profiles=["feature:1", c2], expect_error=True)
assert r.is_error and "feature-tree name" in r.text, r.text
loft = next(f for f in before if f.startswith("Loft"))
r = mcp.call("loft_sketch", profiles=[c1, loft], expect_error=True)
assert r.is_error and "2D sketch" in r.text, r.text
r = mcp.call("loft_sketch", profiles=[c1, c2, guide], closed=True, start_normal=True, expect_error=True)
assert r.is_error and "closed" in r.text, r.text
# An open line as a profile, and a guide curve that misses the profiles.
line = sketch_name(mcp.call("sketch", shape="line", on=far, points=[at(d, 30, 30), at(d, 40, 30)]))
r = mcp.call("loft_sketch", profiles=[c1, line], expect_error=True)
assert r.is_error and "couldn't loft" in r.text, r.text
miss = sketch_name(mcp.call("sketch", shape="arc", plane="top", start=at(0, 50), end=at(d, 50), through=at(d / 2, 55)))
r = mcp.call("loft_sketch", profiles=[c1, c2], guide_curves=[miss], expect_error=True)
assert r.is_error and "couldn't loft" in r.text, r.text
after = features()
assert not any(f.startswith(("Loft", "Cut-Loft")) for f in after if f not in before), (before, after)
assert abs(body()["volume_mm3"] - v0) <= TOL * v0

# ---- Closed: a 10 x 10 ring through Front and two planes at 60 / 120 degrees
# about the axis where Front and Top meet (along v), centers 30 mm out ------

mcp.call("create_part")
planes = {f["name"]: f for f in mcp.call("inspect_model", what="features").json["features"]}
u, v, w = (planes[n]["normal"] for n in ("Front Plane", "Right Plane", "Top Plane"))
[axis] = made(mcp.call("create_reference_geometry", geometry="axis", references=["front", "top"]))
profiles = [square(0, 10, 0, 30, plane="front")]
for angle in (120, 60):
    [p] = made(mcp.call("create_reference_geometry", geometry="plane",
                        references=[{"entity": "front", "angle_deg": angle}, {"entity": axis["id"]}]))
    e = unit(cross(p["normal"], v))
    if dot(e, w) > 0:
        e = scale(e, -1)  # the side 120 / 240 degrees round from Front's w
    c = scale(e, 30)
    profiles.append(sketch_name(mcp.call("sketch", shape="line", on=p["id"], closed=True, points=[
        add(c, scale(v, -5), scale(e, -5)), add(c, scale(v, 5), scale(e, -5)),
        add(c, scale(v, 5), scale(e, 5)), add(c, scale(v, -5), scale(e, 5))])))
r = mcp.call("loft_sketch", profiles=profiles, closed=True)
assert f"closed back to {profiles[0]}" in r.text, r.text
ring = 10 * 10 * 2 * math.pi * 30
got = body()["volume_mm3"]
assert 0.7 * ring < got < 1.3 * ring, (got, ring)
assert abs(extent(v) - 10) < 1e-3, extent(v)

mcp.call("render_views")

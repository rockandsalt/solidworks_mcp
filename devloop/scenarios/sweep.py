# sweep_sketch, also with cut: a square bar swept along a straight path
# (default profile), the same bar twisted, a round rod along a bent path
# (circle_diameter_mm), grooves cut with a circle and with a named profile
# sketch, each checked by the body's volume (and, twisted, its box);
# refusals must leave nothing behind. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/sweep.py
#
# The template's default planes don't face the standard way (devloop.md), so
# everything is drawn in model coordinates along the planes' normals from
# inspect_model: u = Front's normal (the sweep direction, lying in Top),
# v = Right's normal, w = Top's normal (both lying in Front).

import math
import re

TOL = 1e-3  # relative, for volumes


def add(*vs): return [sum(c) for c in zip(*vs)]
def scale(a, k): return [x * k for x in a]


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


def features():
    return [f["name"] for f in mcp.call("inspect_model", what="features").json["features"]]


def sketch_name(r):
    return re.search(r"Sketch\d+", r.text).group(0)


def square(plane, center_v, center_w, size, **kwargs):
    """A closed square on `plane` (in Front: spanned by v and w)."""
    h = size / 2
    return mcp.call("sketch", shape="line", plane=plane, closed=True, points=[
        at(0, center_v - h, center_w - h), at(0, center_v + h, center_w - h),
        at(0, center_v + h, center_w + h), at(0, center_v - h, center_w + h)], **kwargs)


def straight_path(offset_v, length):
    """A line along u on Top, offset by offset_v along v."""
    return sketch_name(mcp.call("sketch", shape="line", plane="top", points=[at(0, offset_v), at(length, offset_v)]))


mcp.call("create_part")
planes = {f["name"]: f for f in mcp.call("inspect_model", what="features").json["features"]}
u, v, w = (planes[n]["normal"] for n in ("Front Plane", "Right Plane", "Top Plane"))

# ---- Boss: a 10 x 10 bar, 100 long; the profile found by default -----------

path = straight_path(0, 100)
profile = sketch_name(square("front", 0, 0, 10))
r = mcp.call("sweep_sketch", path=path)
assert r.text.startswith("created Sweep") and f"{profile} along {path}" in r.text, r.text
bar = 10 * 10 * 100
check_volume(bar, "square bar")

# ---- Cuts: a round groove (half a 4 mm circle) along one side, a 2 x 2
# square groove along the other with a named profile sketch ------------------

groove_path = straight_path(5, 100)
r = mcp.call("sweep_sketch", cut=True, path=groove_path, circle_diameter_mm=4)
assert "Cut-Sweep" in r.text and "a 4 mm circle" in r.text, r.text
round_groove = math.pi * 2 ** 2 / 2 * 100
check_volume(bar - round_groove, "bar with a round groove")

square_path = straight_path(-5, 100)
square_profile = sketch_name(square("front", -5, 0, 4))
mcp.call("sketch", shape="line", plane="front", points=[at(0, 20, 20), at(0, 30, 20)])  # a decoy: newest unused
r = mcp.call("sweep_sketch", cut=True, path=square_path, sketch_name=square_profile)
assert square_profile in r.text, r.text
check_volume(bar - round_groove - 2 * 4 * 100, "bar with two grooves")

# ---- Refusals leave nothing behind -----------------------------------------

before = features()
v0 = body()["volume_mm3"]
r = mcp.call("sweep_sketch", path="NoSuchSketch", expect_error=True)
assert r.is_error and "NoSuchSketch" in r.text, r.text
r = mcp.call("sweep_sketch", path=square_path, sketch_name=square_path, expect_error=True)
assert r.is_error, r.text
r = mcp.call("sweep_sketch", path=path, circle_diameter_mm=3, sketch_name=profile, expect_error=True)
assert r.is_error and "not both" in r.text, r.text
r = mcp.call("sweep_sketch", path="feature:1", expect_error=True)
assert r.is_error and "feature-tree name" in r.text, r.text
# A 30 mm rod round a 5 mm bend would cross itself.
r = mcp.call("sketch", shape="arc", plane="top", start=at(0, 40), end=at(10, 40), through=at(5, 45))
tight = sketch_name(r)
r = mcp.call("sweep_sketch", path=tight, circle_diameter_mm=30, expect_error=True)
assert r.is_error and "couldn't sweep" in r.text, r.text
after = features()
assert not any(f.startswith(("Sweep", "Cut-Sweep")) for f in after if f not in before), (before, after)
assert abs(body()["volume_mm3"] - v0) <= TOL * v0

# ---- Twist: the same bar turned 45 degrees end to end ----------------------

mcp.call("create_part")
path = straight_path(0, 100)
square("front", 0, 0, 10)
r = mcp.call("sweep_sketch", path=path, twist_deg=45)
assert "45 degree twist" in r.text, r.text
check_volume(bar, "twisted bar")
box = body()["bbox_mm"]
extents = sorted((round(hi - lo, 3) for lo, hi in zip(box["min"], box["max"])), reverse=True)
assert extents == [100, round(10 * math.sqrt(2), 3), round(10 * math.sqrt(2), 3)], extents

# ---- A 6 mm rod along a bent path: line, quarter arc (r 20), line ----------

mcp.call("create_part")
r = mcp.call("sketch", shape="line", plane="top", points=[at(0, 0), at(50, 0)])
bent = sketch_name(r)
c = 20 * math.sqrt(0.5)
mcp.call("sketch", shape="arc", sketch_name=bent, start=at(50, 0), end=at(70, 20), through=at(50 + c, 20 - c))
mcp.call("sketch", shape="line", sketch_name=bent, points=[at(70, 20), at(70, 60)])
r = mcp.call("sweep_sketch", path=bent, circle_diameter_mm=6)
assert "a 6 mm circle" in r.text, r.text
check_volume(math.pi * 3 ** 2 * (50 + math.pi * 20 / 2 + 40), "bent rod")

mcp.call("render_views")

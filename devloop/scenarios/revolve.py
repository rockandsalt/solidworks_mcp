# revolve_sketch, also with cut: boss and cut revolves about the
# sketch's own centerline, a sketch line given by id and a reference axis,
# full and partial turns, mid-plane and reversed, each checked by the body's
# volume; refusals must leave nothing behind. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/revolve.py
#
# Everything is sketched on the Front plane in sketch coordinates, so it
# doesn't matter which way the template's planes face (devloop.md).

import math
import re

TOL = 1e-3  # relative, for volumes


def volume():
    bodies = mcp.call("inspect_model", what="bodies").json["bodies"]
    assert len(bodies) == 1, bodies
    return bodies[0]["volume_mm3"]


def check_volume(expected, what):
    got = volume()
    assert abs(got - expected) <= TOL * expected, f"{what}: volume {got}, expected {expected}"


def feature_names():
    return [f["name"] for f in mcp.call("inspect_model", what="features").json["features"]]


def line_id(r):
    """The (only) line id a sketch line call reports."""
    [lid] = set(re.findall(r"\bline:\d+", r.text))
    return lid


# ---- Boss: a stepped shaft about the sketch's centerline -------------------

mcp.call("create_part")
# Half profile on the sketch X axis: r = 15 for 25 mm, then r = 10 for 15 mm.
r = mcp.call("sketch", shape="line", points=[[0, 0], [40, 0], [40, 10], [25, 10], [25, 15], [0, 15]], closed=True)
shaft_sketch = re.search(r"Sketch\d+", r.text).group(0)
r = mcp.call("sketch", shape="line", sketch_name=shaft_sketch, points=[[-10, 0], [50, 0]], construction=True)
centerline = line_id(r)

r = mcp.call("revolve_sketch")
assert "Revolve" in r.text and "centerline of " + shaft_sketch in r.text and centerline in r.text, r.text
assert "360 degrees" in r.text, r.text
shaft_volume = math.pi * (15 ** 2 * 25 + 10 ** 2 * 15)
check_volume(shaft_volume, "stepped shaft")
assert any(f.startswith("Revolve") for f in feature_names())

# The shaft's round faces: two cylinders round the sketch X axis.
cylinders = mcp.call("inspect_model", what="faces", surface_type="cylinder").json["faces"]
assert len(cylinders) == 2, cylinders
radii = sorted(round(f["radius_mm"], 6) for f in cylinders)
assert radii == [10, 15], radii

# ---- Cut: a groove, about a reference axis through the shaft ---------------

big = next(f for f in cylinders if abs(f["radius_mm"] - 15) < 1e-6)
r = mcp.call("create_reference_geometry", geometry="axis", references=[big["id"]])
axis_id = re.search(r"feature:\d+", r.text).group(0)

# 5 mm wide, 3 mm deep groove at x 5..10 (sketch), drawn past the surface.
r = mcp.call("sketch", shape="line", points=[[5, 12], [10, 12], [10, 20], [5, 20]], closed=True)
groove_sketch = re.search(r"Sketch\d+", r.text).group(0)
r = mcp.call("revolve_sketch", cut=True, axis=axis_id)
assert "Cut-Revolve" in r.text or "Revolve" in r.text, r.text
groove = math.pi * (15 ** 2 - 12 ** 2) * 5
check_volume(shaft_volume - groove, "grooved shaft")

# ---- Partial cut about a sketch line given by id ---------------------------

# A 90-degree slice of the thin end (r = 10, x 30..35), about the shaft
# sketch's centerline (in the revolved, absorbed sketch).
mcp.call("sketch", shape="line", points=[[30, 0], [35, 0], [35, 12], [30, 12]], closed=True)
r = mcp.call("revolve_sketch", cut=True, angle_deg=90, axis=centerline)
assert "90 degrees" in r.text and centerline in r.text, r.text
slice_ = math.pi * 10 ** 2 * 5 / 4
check_volume(shaft_volume - groove - slice_, "sliced shaft")

# ---- Refusals leave nothing behind -----------------------------------------

before = feature_names()
v = volume()

# No centerline in the sketch, no axis.
mcp.call("sketch", shape="line", points=[[0, 20], [10, 20], [10, 25], [0, 25]], closed=True)
r = mcp.call("revolve_sketch", expect_error=True)
assert r.is_error and "centerline" in r.text, r.text
# A circle isn't an axis (refused in the server).
r = mcp.call("sketch", shape="circle", center=[100, 100], diameter_mm=4)
circle = re.search(r"\bcircle:\d+", r.text).group(0)
r = mcp.call("revolve_sketch", axis=circle, expect_error=True)
assert r.is_error and "arc or circle" in r.text, r.text
# Bad angle.
r = mcp.call("revolve_sketch", angle_deg=400, expect_error=True)
assert r.is_error and "angle" in r.text, r.text
# A profile crossing its axis: SolidWorks refuses.
r = mcp.call("sketch", shape="line", points=[[60, -5], [70, -5], [70, 5], [60, 5]], closed=True)
crossing = re.search(r"Sketch\d+", r.text).group(0)
mcp.call("sketch", shape="line", sketch_name=crossing, points=[[55, 0], [75, 0]], construction=True)
r = mcp.call("revolve_sketch", expect_error=True)
assert r.is_error and "couldn't revolve" in r.text, r.text

after = feature_names()
assert not any(f.startswith(("Revolve", "Cut-Revolve")) for f in after if f not in before), (before, after)
assert abs(volume() - v) <= TOL * v

# ---- Partial boss: 90 degrees, then mid-plane and reversed -----------------

for kwargs in ({}, {"mid_plane": True}, {"reverse_direction": True}):
    mcp.call("create_part")
    r = mcp.call("sketch", shape="line", points=[[0, 5], [20, 5], [20, 10], [0, 10]], closed=True)
    sk = re.search(r"Sketch\d+", r.text).group(0)
    r = mcp.call("sketch", shape="line", sketch_name=sk, points=[[-5, 0], [25, 0]], construction=True)
    r = mcp.call("revolve_sketch", angle_deg=90, **kwargs)
    check_volume(math.pi * (10 ** 2 - 5 ** 2) * 20 / 4, f"quarter ring {kwargs}")

mcp.call("render_views")

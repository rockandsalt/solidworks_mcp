# sketch_slot / sketch_polygon / sketch_ellipse, sketch_fillet / sketch_chamfer,
# trim_sketch_entity / extend_sketch_entity and convert_entities: each shape read
# back and extruded (volume), which corner or piece each edit touches, driving
# what they dimension, refusals leaving the sketch as it was, on a plane other
# than Front and given in model coordinates, a converted edge following its
# original. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/sketch_tools.py

import math
import re
import subprocess
from pathlib import Path

TOL = 1e-3  # relative, for volumes
SW = Path(__file__).resolve().parent.parent / "sw.ps1"


def volume():
    return sum(b["volume_mm3"] for b in mcp.call("inspect_model", what="bodies").json["bodies"])


def check_volume(expected, what):
    got = volume()
    assert abs(got - expected) <= TOL * expected, f"{what}: volume {got}, expected {expected}"


def features():
    return [f["name"] for f in mcp.call("inspect_model", what="features").json["features"]]


def found(text, kind):
    return re.findall(rf"\b{kind}:\d+", text)


def sketch_name(r):
    return re.search(r"\b(?:3D)?Sketch\d+", r.text).group(0)


def sketch(name):
    return mcp.call("inspect_sketch", sketch_name=name).json


def by_id(name):
    return {s["id"]: s for s in sketch(name)["segments"]}


def part_after(text, label):
    """The ids listed after e.g. "New:", up to the end of that sentence."""
    m = re.search(rf"{label}: (.*?)\.( |$)", text)
    return m.group(1) if m else ""


def near(a, b, tol=1e-3):
    return all(abs(x - y) <= tol for x, y in zip(a, b)) and len(a) == len(b)


def geometry(name):
    """A sketch's segments, comparable before and after a refusal."""
    return sorted(
        (s["type"], s.get("length_mm"), tuple(s.get("start_mm", ())), tuple(s.get("end_mm", ())))
        for s in sketch(name)["segments"]
    )


def refused(tool, expect, **args):
    r = mcp.call(tool, expect_error=True, **args)
    assert r.is_error and expect in r.text, (tool, args, r.text)


def refused_unchanged(name, tool, expect, **args):
    before = geometry(name)
    refused(tool, expect, **args)
    assert geometry(name) == before, (tool, before, geometry(name))


def rectangle(**where):
    """A closed 40 x 20 rectangle: its sketch, its four lines (bottom, right, top, left) and the
    point at each corner by its coordinates."""
    r = mcp.call("sketch_line", points=[[-20, -10], [20, -10], [20, 10], [-20, 10]], closed=True, **where)
    name = sketch_name(r)
    lines = found(part_after(r.text, "Ids"), "line")
    assert len(lines) == 4, r.text
    return name, lines, corners(name)


def corners(name):
    return {tuple(p["at_mm"]): p["id"] for p in sketch(name)["points"]}


def no_dialogs():
    out = subprocess.run(
        ["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(SW), "dialogs"],
        capture_output=True, text=True, timeout=60,
    ).stdout.strip()
    assert not out or "no " in out.lower(), f"dialogs open: {out}"


# ---- Slots -------------------------------------------------------------------

mcp.call("create_part")

r = mcp.call("sketch_slot", start=[-20, 0], end=[20, 0], width_mm=6)
name = sketch_name(r)
assert r.text.startswith(f"drew a slot 6 mm wide in {name} (new sketch"), r.text
assert f"{name} now has 1 closed and 0 open" in r.text, r.text
s = sketch(name)["segments"]
assert sum(x["type"] == "line" for x in s) == 3 and sum(x["type"] == "arc" for x in s) == 2, s
assert sum(1 for x in s if x.get("construction")) == 1, s
assert sorted(x["radius_mm"] for x in s if x["type"] == "arc") == [3, 3], s
assert sorted(tuple(x["center_mm"]) for x in s if x["type"] == "arc") == [(-20, 0), (20, 0)], s
# Every segment reported with an id; the middle point too.
assert len(found(part_after(r.text, "Ids"), "line")) == 3 and len(found(part_after(r.text, "Ids"), "arc")) == 2, r.text
assert any(p["at_mm"] == [0, 0] for p in sketch(name)["points"]), sketch(name)["points"]
mcp.call("extrude_sketch", sketch_name=name, depth_mm=10)
check_volume((40 * 6 + math.pi * 9) * 10, "straight slot")

# Curved: by center (counterclockwise, then clockwise the long way) and through a point.
mcp.call("create_part")
r = mcp.call("sketch_slot", start=[20, 0], end=[0, 20], center=[0, 0], width_mm=6)
name = sketch_name(r)
assert "a curved slot 6 mm wide" in r.text, r.text
middle = next(x for x in sketch(name)["segments"] if x.get("construction"))
assert middle["type"] == "arc" and abs(middle["length_mm"] - 10 * math.pi) < 1e-3, middle
mcp.call("extrude_sketch", sketch_name=name, depth_mm=10)
check_volume((10 * math.pi * 6 + math.pi * 9) * 10, "slot along a quarter arc")

mcp.call("create_part")
r = mcp.call("sketch_slot", start=[20, 0], end=[0, 40], center=[0, 0], clockwise=True, width_mm=6)
name = sketch_name(r)
middle = next(x for x in sketch(name)["segments"] if x.get("construction"))
assert abs(middle["length_mm"] - 30 * math.pi) < 1e-3, middle  # end moved onto the radius
assert any(near(x["center_mm"], [0, 20]) for x in sketch(name)["segments"] if x["type"] == "arc" and x["radius_mm"] == 3), sketch(name)

r = mcp.call("sketch_slot", start=[120, 0], end=[100, 20], through=[100 + 20 * math.sqrt(0.5), 20 * math.sqrt(0.5)], width_mm=4, sketch_name=name)
assert f"{name} now has 2 closed" in r.text, r.text

# Dimensions drive it.
mcp.call("create_part")
r = mcp.call("sketch_slot", start=[-20, 0], end=[20, 0], width_mm=6, add_dimensions=True)
name = sketch_name(r)
dims = {round(d["value_mm"], 3): d["name"] for d in sketch(name)["dimensions"]}
assert set(dims) == {6, 40}, dims
mcp.call("set_dimension", name=dims[6], value_mm=8)
assert all(x["radius_mm"] == 4 for x in sketch(name)["segments"] if x["type"] == "arc"), sketch(name)

# Construction.
r = mcp.call("sketch_slot", start=[-20, 30], end=[20, 30], width_mm=6, construction=True, sketch_name=name)
assert sum(1 for x in sketch(name)["segments"] if not x.get("construction")) == 4, sketch(name)["segments"]  # the first's

before = features()
for args, expect in [
    (dict(start=[0, 0], end=[10, 0], width_mm=0), "width_mm"),
    (dict(start=[0, 0], end=[0, 0], width_mm=4), "same point"),
    (dict(start=[0, 0], end=[10, 0], width_mm=4, clockwise=True), "clockwise"),
    (dict(start=[0, 0], end=[10, 0], center=[5, 5], through=[5, 1], width_mm=4), "not both"),
    (dict(start=[10, 0], end=[0, 10], center=[0, 0], width_mm=20), "narrower"),
    (dict(start=[0, 0], end=[20, 0], through=[10, 0], width_mm=4), "straight line"),
    (dict(start=[10, 0], end=[20, 0], center=[0, 0], width_mm=4), "same angle"),
]:
    refused("sketch_slot", expect, **args)
assert features() == before, features()

# ---- Polygons ----------------------------------------------------------------

mcp.call("create_part")

r = mcp.call("sketch_polygon", center=[0, 0], sides=6, corner_radius_mm=10)
name = sketch_name(r)
assert "drew a polygon with 6 sides" in r.text and f"{name} now has 1 closed" in r.text, r.text
assert len(found(part_after(r.text, "Ids"), "line")) == 6 and len(found(part_after(r.text, "Ids"), "circle")) == 1, r.text
s = sketch(name)["segments"]
circle = next(x for x in s if x["type"] == "circle")
assert circle.get("construction") and abs(circle["radius_mm"] - 10) < 1e-3, circle  # through the corners
assert any(near(p["at_mm"], [10, 0]) for p in sketch(name)["points"]), sketch(name)["points"]
mcp.call("extrude_sketch", sketch_name=name, depth_mm=10)
check_volume(1.5 * math.sqrt(3) * 100 * 10, "hexagon by its corner")

# By its flats, turned so a side's middle is where the angle says; the circle sizes it.
mcp.call("create_part")
r = mcp.call("sketch_polygon", center=[5, 7], sides=6, flat_radius_mm=10)
name = sketch_name(r)
s = sketch(name)["segments"]
circle = next(x for x in s if x["type"] == "circle")
assert abs(circle["radius_mm"] - 10) < 1e-3 and near(circle["center_mm"], [5, 7]), circle  # touches the sides
side = next(x for x in s if x["type"] == "line" and abs(x["start_mm"][0] - 15) < 1e-3 and abs(x["end_mm"][0] - 15) < 1e-3)
assert abs(side["length_mm"] - 20 / math.sqrt(3)) < 1e-3, side
mcp.call("dimension_sketch", dimensions=[{"kind": "diameter", "entities": [circle["id"]], "value_mm": 30}])
s = sketch(name)["segments"]
circle = next(x for x in s if x["type"] == "circle")
assert abs(circle["radius_mm"] - 15) < 1e-3, circle  # nothing holds the center, so it can drift
side_x = circle["center_mm"][0] + 15
assert any(x["type"] == "line" and abs(x["start_mm"][0] - side_x) < 1e-3 and abs(x["end_mm"][0] - side_x) < 1e-3 for x in s), s

mcp.call("create_part")
r = mcp.call("sketch_polygon", center=[0, 0], sides=4, corner_radius_mm=10, angle_deg=45)
name = sketch_name(r)
assert any(near(p["at_mm"], [10 * math.sqrt(0.5), 10 * math.sqrt(0.5)]) for p in sketch(name)["points"]), sketch(name)["points"]
mcp.call("extrude_sketch", sketch_name=name, depth_mm=10)
check_volume(200 * 10, "a square by its corner, level")

mcp.call("create_part")
for sides in (3, 40):
    r = mcp.call("sketch_polygon", center=[sides * 10, 0], sides=sides, corner_radius_mm=5)
    assert len(found(part_after(r.text, "Ids"), "line")) == sides, r.text

before = features()
for args, expect in [
    (dict(center=[0, 0], sides=2, corner_radius_mm=5), "3 to 40"),
    (dict(center=[0, 0], sides=41, corner_radius_mm=5), "3 to 40"),
    (dict(center=[0, 0], sides=6), "exactly one"),
    (dict(center=[0, 0], sides=6, corner_radius_mm=5, flat_radius_mm=4), "exactly one"),
    (dict(center=[0, 0], sides=6, corner_radius_mm=-1), "positive"),
]:
    refused("sketch_polygon", expect, **args)
assert features() == before, features()

# ---- Ellipses ----------------------------------------------------------------

mcp.call("create_part")

r = mcp.call("sketch_ellipse", center=[1, 2], major_radius_mm=20, minor_radius_mm=10)
name = sketch_name(r)
assert "an ellipse of 20 x 10 mm radii" in r.text and f"{name} now has 1 closed" in r.text, r.text
[ellipse] = found(part_after(r.text, "Ids"), "ellipse")
e = by_id(name)[ellipse]
assert near(e["center_mm"], [1, 2]) and near(e["major_mm"], [21, 2]) and near(e["minor_mm"], [1, 12]), e
assert e["major_radius_mm"] == 20 and e["minor_radius_mm"] == 10 and "start_mm" not in e, e
mcp.call("extrude_sketch", sketch_name=name, depth_mm=10)
check_volume(math.pi * 200 * 10, "ellipse")

mcp.call("create_part")
r = mcp.call("sketch_ellipse", center=[0, 0], major_radius_mm=20, minor_radius_mm=10, angle_deg=30)
name = sketch_name(r)
[ellipse] = found(part_after(r.text, "Ids"), "ellipse")
e = by_id(name)[ellipse]
assert near(e["major_mm"], [20 * math.cos(math.radians(30)), 10]), e
assert near(e["minor_mm"], [-5, 10 * math.cos(math.radians(30))]), e

# Its axis ends take dimensions.
r = mcp.call("dimension_sketch", dimensions=[{"kind": "distance", "entities": [e["center"], e["major"]], "value_mm": 25}])
assert by_id(name)[ellipse]["major_radius_mm"] == 25, sketch(name)

# A part of one, by trimming: an ellipse with ends.
mcp.call("create_part")
r = mcp.call("sketch_ellipse", center=[0, 0], major_radius_mm=20, minor_radius_mm=10)
name = sketch_name(r)
[ellipse] = found(part_after(r.text, "Ids"), "ellipse")
mcp.call("sketch_line", points=[[-30, 0], [30, 0]], sketch_name=name)
r = mcp.call("trim_sketch_entity", entity=ellipse, at=[0, 10])
assert r.text.startswith(f"trimmed {ellipse} in {name}."), r.text
[half] = found(part_after(r.text, "Changed"), "ellipse")
e = by_id(name)[half]
assert sorted(abs(e["start_mm"][0]) for _ in [0]) == [20] and e["start_mm"][1] == 0 and e["end_mm"][1] == 0, e

before = features()
for args, expect in [
    (dict(center=[0, 0], major_radius_mm=10, minor_radius_mm=10), "sketch_circle"),
    (dict(center=[0, 0], major_radius_mm=5, minor_radius_mm=10), "more than"),
    (dict(center=[0, 0], major_radius_mm=5, minor_radius_mm=0), "more than"),
]:
    refused("sketch_ellipse", expect, **args)
assert features() == before, features()

# None of them in a 3D sketch.
r = mcp.call("sketch_line", points=[[0, 0, 0], [10, 0, 5]], sketch_3d=True)
sketch3d = sketch_name(r)
for tool, args in [
    ("sketch_slot", dict(start=[0, 0, 0], end=[10, 0, 0], width_mm=2)),
    ("sketch_polygon", dict(center=[0, 0, 0], sides=6, corner_radius_mm=2)),
    ("sketch_ellipse", dict(center=[0, 0, 0], major_radius_mm=4, minor_radius_mm=2)),
]:
    refused_unchanged(sketch3d, tool, "3D sketch", sketch_name=sketch3d, **args)

# ---- Fillets -----------------------------------------------------------------

mcp.call("create_part")

# One corner, by its point: only that corner, and the two lines shorten.
name, (bottom, right, top, left), at = rectangle()
r = mcp.call("sketch_fillet", entities=[at[(20, 10)]], radius_mm=3)
assert r.text.startswith(f"filleted {at[(20, 10)]} with radius 3 mm in {name}."), r.text
[arc] = found(part_after(r.text, "New"), "arc")
assert sorted(found(part_after(r.text, "Changed"), "line")) == sorted([right, top]), r.text
assert " = 3 mm" in r.text and f"{name} now has 1 closed and 0 open" in r.text, r.text
s = by_id(name)
assert len(s) == 5 and s[arc]["radius_mm"] == 3 and near(s[arc]["center_mm"], [17, 7]), s
assert s[right]["length_mm"] == 17 and s[top]["length_mm"] == 37 and s[bottom]["length_mm"] == 40, s
mcp.call("extrude_sketch", sketch_name=name, depth_mm=10)
check_volume((800 - (1 - math.pi / 4) * 9) * 10, "a filleted corner")

# Its dimension drives it, through the feature that uses the sketch. Held by dimensions, as
# otherwise the lines just move out to make room (nothing keeps them where they were).
mcp.call("create_part")
name, (bottom, right, top, left), at = rectangle()
mcp.call(
    "dimension_sketch",
    dimensions=[
        {"kind": "length", "entities": [bottom], "value_mm": 40},
        {"kind": "length", "entities": [right], "value_mm": 20},
        {"kind": "horizontal", "entities": [at[(-20, -10)], "origin"], "value_mm": 20},
        {"kind": "vertical", "entities": [at[(-20, -10)], "origin"], "value_mm": 10},
    ],
)
mcp.call("add_sketch_relations", relations=[{"kind": "horizontal", "entities": [bottom, top]}, {"kind": "vertical", "entities": [left, right]}])
r = mcp.call("sketch_fillet", entities=[at[(20, 10)]], radius_mm=3)
dim = re.search(r"(\S+) = 3 mm", r.text).group(1)
mcp.call("extrude_sketch", sketch_name=name, depth_mm=10)
mcp.call("set_dimension", name=dim, value_mm=5)
check_volume((800 - (1 - math.pi / 4) * 25) * 10, "the fillet made bigger")

# A sketch a feature uses, with its corner filleted afterwards.
r = mcp.call("sketch_fillet", entities=[at[(-20, -10)]], radius_mm=2)
assert "fail" not in r.text.lower(), r.text
check_volume((800 - (1 - math.pi / 4) * (25 + 4)) * 10, "another corner, on the used sketch")

# Two corners at once, equal; two entities for the corner between them.
mcp.call("create_part")
name, (bottom, right, top, left), at = rectangle()
r = mcp.call("sketch_fillet", entities=[at[(20, 10)], at[(-20, -10)]], radius_mm=3)
assert len(found(part_after(r.text, "New"), "arc")) == 2, r.text
assert any(x["type"] == "arc" and near(x["center_mm"], [-17, -7]) for x in sketch(name)["segments"]), sketch(name)
assert len(sketch(name)["dimensions"]) == 1, sketch(name)["dimensions"]

r = mcp.call("sketch_fillet", entities=[top, left], radius_mm=2)
[arc] = found(part_after(r.text, "New"), "arc")
assert near(by_id(name)[arc]["center_mm"], [-18, 8]), by_id(name)[arc]

# Lines that don't touch yet grow to it.
r = mcp.call("sketch_line", points=[[0, 0], [10, 0]])
far = sketch_name(r)
[a] = found(part_after(r.text, "Ids"), "line")
r = mcp.call("sketch_line", points=[[15, 5], [15, 20]], sketch_name=far)
[b] = found(part_after(r.text, "Ids"), "line")
r = mcp.call("sketch_fillet", entities=[a, b], radius_mm=3)
s = by_id(far)
assert s[a]["length_mm"] == 12 and s[b]["length_mm"] == 17, s

# What's dimensioned to the corner keeps it, on a point.
mcp.call("create_part")
name, (bottom, right, top, left), at = rectangle()
mcp.call("dimension_sketch", dimensions=[{"kind": "length", "entities": [right], "value_mm": 20}])
r = mcp.call("sketch_fillet", entities=[at[(20, 10)]], radius_mm=3)
assert any(p["at_mm"] == [20, 10] for p in sketch(name)["points"]), sketch(name)["points"]
assert found(part_after(r.text, "New"), "point"), r.text
assert not sketch(name)["dimensions"][0].get("driven"), sketch(name)["dimensions"]

# Refusals: no room, one corner of two with no room, crossing, parallel, a line's far end.
mcp.call("create_part")
name, (bottom, right, top, left), at = rectangle()
refused_unchanged(name, "sketch_fillet", "couldn't fillet", entities=[at[(20, 10)]], radius_mm=30)
refused_unchanged(name, "sketch_fillet", "only 1 of the 2", entities=[at[(20, 10)], at[(20, -10)]], radius_mm=12)
assert not sketch(name).get("dimensions"), sketch(name)["dimensions"]
refused_unchanged(name, "sketch_fillet", "two entities make one", entities=[right], radius_mm=3)
refused_unchanged(name, "sketch_fillet", "not both", entities=[right, at[(20, 10)]], radius_mm=3)
refused_unchanged(name, "sketch_fillet", "radius_mm", entities=[at[(20, 10)]], radius_mm=0)
refused_unchanged(name, "sketch_fillet", "couldn't fillet", entities=[bottom, top], radius_mm=3)  # parallel
r = mcp.call("sketch_line", points=[[-20, 0], [20, 0]])
cross = sketch_name(r)
[a] = found(part_after(r.text, "Ids"), "line")
r = mcp.call("sketch_line", points=[[0, -10], [0, 10]], sketch_name=cross)
[b] = found(part_after(r.text, "Ids"), "line")
refused_unchanged(cross, "sketch_fillet", "couldn't fillet", entities=[a, b], radius_mm=3)
no_dialogs()

# ---- Chamfers ----------------------------------------------------------------

mcp.call("create_part")

# Equal, at a point; the sharp corner stays as a point.
name, (bottom, right, top, left), at = rectangle()
r = mcp.call("sketch_chamfer", entities=[at[(20, 10)]], distance_mm=3)
assert r.text.startswith(f"chamfered {at[(20, 10)]} in {name}."), r.text
[new] = found(part_after(r.text, "New"), "line")
s = by_id(name)
assert abs(s[new]["length_mm"] - 3 * math.sqrt(2)) < 1e-3 and s[right]["length_mm"] == 17 and s[top]["length_mm"] == 37, s
assert any(p["at_mm"] == [20, 10] for p in sketch(name)["points"]), sketch(name)["points"]
dims = sketch(name)["dimensions"]
assert len(dims) == 2, dims
mcp.call("extrude_sketch", sketch_name=name, depth_mm=10)
check_volume((800 - 4.5) * 10, "an equal chamfer")
mcp.call("set_dimension", name=dims[0]["name"], value_mm=5)
assert sketch(name)["dimensions"][0]["value_mm"] == 5, sketch(name)["dimensions"]

# Two distances: the first along the first entity, whichever way round; also where the first
# entity given isn't the older one.
mcp.call("create_part")
name, (bottom, right, top, left), at = rectangle()
mcp.call("sketch_chamfer", entities=[right, top], distance_mm=3, second_distance_mm=5)
s = by_id(name)
assert s[right]["length_mm"] == 17 and s[top]["length_mm"] == 35, s
mcp.call("sketch_chamfer", entities=[left, top], distance_mm=3, second_distance_mm=5)
s = by_id(name)
assert s[left]["length_mm"] == 17 and s[top]["length_mm"] == 30, s
mcp.call("sketch_chamfer", entities=[bottom, left], distance_mm=2, second_distance_mm=4)
s = by_id(name)
assert s[bottom]["length_mm"] == 38 and s[left]["length_mm"] == 13, s

# A distance and an angle from the first entity.
mcp.call("create_part")
name, (bottom, right, top, left), at = rectangle()
mcp.call("sketch_chamfer", entities=[top, right], distance_mm=3, angle_deg=30)
s = by_id(name)
assert s[top]["length_mm"] == 37 and abs(s[right]["length_mm"] - (20 - 3 * math.tan(math.radians(30)))) < 1e-3, s

# Several corners.
mcp.call("create_part")
name, _, at = rectangle()
r = mcp.call("sketch_chamfer", entities=[at[(20, 10)], at[(-20, -10)]], distance_mm=3)
assert len(found(part_after(r.text, "New"), "line")) == 2, r.text

mcp.call("create_part")
name, (bottom, right, top, left), at = rectangle()
refused_unchanged(name, "sketch_chamfer", "two different sides", entities=[at[(20, 10)]], distance_mm=3, second_distance_mm=5)
refused_unchanged(name, "sketch_chamfer", "not both", entities=[right, top], distance_mm=3, second_distance_mm=5, angle_deg=30)
refused_unchanged(name, "sketch_chamfer", "couldn't chamfer", entities=[at[(20, 10)]], distance_mm=30)
refused_unchanged(name, "sketch_chamfer", "couldn't chamfer", entities=[bottom, top], distance_mm=3)
refused_unchanged(name, "sketch_chamfer", "angle_deg", entities=[right, top], distance_mm=3, angle_deg=180)
no_dialogs()

# ---- Trimming ----------------------------------------------------------------

mcp.call("create_part")


def crossed():
    """A line across two verticals."""
    r = mcp.call("sketch_line", points=[[-20, 0], [20, 0]])
    name = sketch_name(r)
    [h] = found(part_after(r.text, "Ids"), "line")
    r = mcp.call("sketch_line", points=[[-10, -10], [-10, 10]], sketch_name=name)
    [v1] = found(part_after(r.text, "Ids"), "line")
    r = mcp.call("sketch_line", points=[[10, -10], [10, 10]], sketch_name=name)
    [v2] = found(part_after(r.text, "Ids"), "line")
    return name, h, v1, v2


# The middle piece: two lines left, one of them new.
name, h, v1, v2 = crossed()
r = mcp.call("trim_sketch_entity", entity=h, at=[0, 0])
assert r.text.startswith(f"trimmed {h} in {name}."), r.text
[new] = found(part_after(r.text, "New"), "line")
assert found(part_after(r.text, "Changed"), "line") == [h], r.text
s = by_id(name)
assert s[h]["length_mm"] == 10 and s[new]["length_mm"] == 10 and len(s) == 4, s
assert sorted([tuple(s[h]["start_mm"]), tuple(s[new]["end_mm"])]) == [(-20, 0), (20, 0)], s

# An end piece; a point off the entity counts by the nearest point on it.
name, h, v1, v2 = crossed()
r = mcp.call("trim_sketch_entity", entity=h, at=[-15, 0])
s = by_id(name)
assert s[h]["length_mm"] == 30 and len(s) == 3, s
r = mcp.call("trim_sketch_entity", entity=h, at=[15, 50])
s = by_id(name)
assert s[h]["length_mm"] == 20 and len(s) == 3, s
assert near(s[h]["start_mm"], [-10, 0]) and near(s[h]["end_mm"], [10, 0]), s

# Nothing crosses it: gone, and said so.
r = mcp.call("sketch_line", points=[[0, 0], [10, 0]])
name = sketch_name(r)
[a] = found(part_after(r.text, "Ids"), "line")
r = mcp.call("sketch_circle", center=[50, 50], radius_mm=5, sketch_name=name)
r = mcp.call("trim_sketch_entity", entity=a, at=[5, 0])
assert found(part_after(r.text, "Gone"), "line") == [a], r.text
assert all(x["type"] != "line" for x in sketch(name)["segments"]), sketch(name)
refused("trim_sketch_entity", "any more", entity=a, at=[5, 0], sketch_name=name)

# A circle becomes an arc, with a new id.
r = mcp.call("sketch_circle", center=[0, 0], radius_mm=10)
name = sketch_name(r)
[c] = found(part_after(r.text, "Ids"), "circle")
mcp.call("sketch_line", points=[[-20, 0], [20, 0]], sketch_name=name)
r = mcp.call("trim_sketch_entity", entity=c, at=[0, 10])
[arc] = found(part_after(r.text, "Changed"), "arc")
s = by_id(name)
assert s[arc]["type"] == "arc" and abs(s[arc]["length_mm"] - 10 * math.pi) < 1e-3, s
assert all(p[1] <= 0 for p in [s[arc]["start_mm"], s[arc]["end_mm"]]) and c not in s, s

# Trim a sketch into a closed shape and use it.
mcp.call("create_part")
r = mcp.call("sketch_circle", center=[0, 0], radius_mm=10)
name = sketch_name(r)
[c] = found(part_after(r.text, "Ids"), "circle")
r = mcp.call("sketch_line", points=[[-20, 0], [20, 0]], sketch_name=name)
[h] = found(part_after(r.text, "Ids"), "line")
mcp.call("trim_sketch_entity", entity=c, at=[0, 10])
r = mcp.call("trim_sketch_entity", entity=h, at=[-15, 0])
r = mcp.call("trim_sketch_entity", entity=h, at=[15, 0])
assert f"{name} now has 1 closed and 0 open" in r.text, r.text
mcp.call("extrude_sketch", sketch_name=name, depth_mm=10)
check_volume(math.pi * 50 * 10, "a half disc made by trimming")

# Corners: both grow; crossing and parallel refused.
mcp.call("create_part")
r = mcp.call("sketch_line", points=[[0, 0], [10, 0]])
name = sketch_name(r)
[a] = found(part_after(r.text, "Ids"), "line")
r = mcp.call("sketch_line", points=[[20, 5], [20, 20]], sketch_name=name)
[b] = found(part_after(r.text, "Ids"), "line")
r = mcp.call("trim_sketch_entity", entity=a, corner_with=b)
assert sorted(found(part_after(r.text, "Changed"), "line")) == sorted([a, b]), r.text
s = by_id(name)
assert s[a]["length_mm"] == 20 and s[b]["length_mm"] == 20, s
refused_unchanged(name, "trim_sketch_entity", "can't meet", entity=a, corner_with=b)  # already meet

name, h, v1, v2 = crossed()
refused_unchanged(name, "trim_sketch_entity", "cross", entity=h, corner_with=v1)
refused_unchanged(name, "trim_sketch_entity", "can't meet", entity=v1, corner_with=v2)
refused_unchanged(name, "trim_sketch_entity", "exactly one", entity=h)
refused_unchanged(name, "trim_sketch_entity", "exactly one", entity=h, at=[0, 0], corner_with=v1)
refused_unchanged(name, "trim_sketch_entity", "itself", entity=h, corner_with=h)
no_dialogs()

# ---- Extending ---------------------------------------------------------------

mcp.call("create_part")


def between():
    """A short line between two verticals."""
    r = mcp.call("sketch_line", points=[[0, 0], [10, 0]])
    name = sketch_name(r)
    [a] = found(part_after(r.text, "Ids"), "line")
    r = mcp.call("sketch_line", points=[[20, -10], [20, 10]], sketch_name=name)
    r = mcp.call("sketch_line", points=[[-20, -10], [-20, 10]], sketch_name=name)
    return name, a


# The end nearest `at` grows up to what's there, and on past it when asked again.
name, a = between()
r = mcp.call("extend_sketch_entity", entity=a, at=[8, 0])
assert r.text.startswith(f"extended {a} in {name}."), r.text
assert found(part_after(r.text, "Changed"), "line") == [a], r.text
s = by_id(name)
assert near(s[a]["start_mm"], [0, 0]) and near(s[a]["end_mm"], [20, 0]), s[a]

name, a = between()
mcp.call("extend_sketch_entity", entity=a, at=[1, 0])
s = by_id(name)
assert near(s[a]["start_mm"], [-20, 0]) and near(s[a]["end_mm"], [10, 0]), s[a]

r = mcp.call("sketch_line", points=[[0, 0], [10, 0]])
name = sketch_name(r)
[a] = found(part_after(r.text, "Ids"), "line")
mcp.call("sketch_line", points=[[20, -10], [20, 10]], sketch_name=name)
mcp.call("sketch_line", points=[[30, -10], [30, 10]], sketch_name=name)
mcp.call("extend_sketch_entity", entity=a, at=[10, 0])
mcp.call("extend_sketch_entity", entity=a, at=[20, 0])
assert by_id(name)[a]["length_mm"] == 30, by_id(name)[a]

# An arc, up to a line.
r = mcp.call("sketch_arc", start=[10, 0], end=[math.sqrt(75), 5], center=[0, 0])
name = sketch_name(r)
[arc] = found(part_after(r.text, "Ids"), "arc")
mcp.call("sketch_line", points=[[-20, 8], [20, 8]], sketch_name=name)
mcp.call("extend_sketch_entity", entity=arc, at=[math.sqrt(75), 5])
assert near(by_id(name)[arc]["end_mm"], [6, 8]), by_id(name)[arc]

# Nothing in the way of that end (SolidWorks would grow the other one), or of either.
r = mcp.call("sketch_line", points=[[0, 0], [10, 0]])
name = sketch_name(r)
[a] = found(part_after(r.text, "Ids"), "line")
refused_unchanged(name, "extend_sketch_entity", "nothing is in the way", entity=a, at=[8, 0])
mcp.call("sketch_line", points=[[-20, -10], [-20, 10]], sketch_name=name)
refused_unchanged(name, "extend_sketch_entity", "the other end", entity=a, at=[8, 0])
refused_unchanged(name, "extend_sketch_entity", "at has 1", entity=a, at=[8])
no_dialogs()

# ---- Another plane, model coordinates ----------------------------------------
# Sketch coordinates aren't model coordinates here, whichever way the template's planes face.

mcp.call("create_part")
r = mcp.call("sketch_line", points=[[-20, 0], [20, 0]], plane="right")
name = sketch_name(r)
[h] = found(part_after(r.text, "Ids"), "line")
r = mcp.call("sketch_line", points=[[10, -10], [10, 10]], sketch_name=name)
[v] = found(part_after(r.text, "Ids"), "line")
axes = sketch(name)["sketch_axes"]


def model(x, y):
    return [axes["origin_mm"][i] + x * axes["x"][i] + y * axes["y"][i] for i in range(3)]


mcp.call("trim_sketch_entity", entity=h, at=[15, 0])
assert near(by_id(name)[h]["end_mm"], [10, 0]), by_id(name)[h]
r = mcp.call("sketch_line", points=[[-20, 5], [0, 5]], sketch_name=name)
[short] = found(part_after(r.text, "Ids"), "line")
mcp.call("extend_sketch_entity", entity=short, at=[0, 5])
assert near(by_id(name)[short]["end_mm"], [10, 5]), by_id(name)[short]

# The same in model coordinates, projected; off the plane doesn't matter.
r = mcp.call("sketch_line", points=[[-20, 7], [20, 7]], sketch_name=name)
[other] = found(part_after(r.text, "Ids"), "line")
at_model = model(15, 7)
at_model = [at_model[i] + 3 * axes["normal"][i] for i in range(3)]
mcp.call("trim_sketch_entity", entity=other, at=at_model)
assert near(by_id(name)[other]["end_mm"], [10, 7]), by_id(name)[other]

r = mcp.call("sketch_slot", start=model(40, 0), end=model(60, 0), width_mm=4, sketch_name=name)
assert any(x["type"] == "arc" and near(x["center_mm"], [60, 0]) for x in sketch(name)["segments"]), sketch(name)
r = mcp.call("sketch_polygon", center=model(40, 30), sides=6, corner_radius_mm=4, sketch_name=name)
assert any(x["type"] == "circle" and near(x["center_mm"], [40, 30]) for x in sketch(name)["segments"]), sketch(name)
r = mcp.call("sketch_ellipse", center=model(70, 30), major_radius_mm=4, minor_radius_mm=2, sketch_name=name)
assert any(x["type"] == "ellipse" and near(x["center_mm"], [70, 30]) for x in sketch(name)["segments"]), sketch(name)

# Fillets on a sketch of a face.
mcp.call("create_part")
base, _, _ = rectangle()
mcp.call("extrude_sketch", sketch_name=base, depth_mm=10)
top_face = next(f for f in mcp.call("inspect_model", what="faces", surface_type="plane", limit=100).json["faces"] if abs(f["area_mm2"] - 800) < 1e-3)["id"]
r = mcp.call("sketch_line", on=top_face, points=[[-5, -5], [5, -5], [5, 5], [-5, 5]], closed=True)
name = sketch_name(r)
at = corners(name)
mcp.call("sketch_fillet", entities=[next(iter(at.values()))], radius_mm=2)
mcp.call("cut_extrude_sketch", sketch_name=name, depth_mm=2)
check_volume(8000 - (100 - (1 - math.pi / 4) * 4) * 2, "a filleted pocket")

# ---- Converting --------------------------------------------------------------

mcp.call("create_part")
r = mcp.call("sketch_line", points=[[-20, -10], [20, -10], [20, 10], [-20, 10]], closed=True)
plate = sketch_name(r)
plate_lines = found(part_after(r.text, "Ids"), "line")
dim_line = plate_lines[0]
mcp.call("dimension_sketch", dimensions=[{"kind": "length", "entities": [dim_line], "value_mm": 40}])
plate_dim = sketch(plate)["dimensions"][0]["name"]
mcp.call("sketch_circle", sketch_name=plate, center=[5, 0], radius_mm=4)
mcp.call("extrude_sketch", sketch_name=plate, depth_mm=10)
plate_volume = (800 - math.pi * 16) * 10
check_volume(plate_volume, "plate")

faces = mcp.call("inspect_model", what="faces", surface_type="plane", limit=100).json["faces"]
face = next(f for f in faces if abs(f["area_mm2"] - (800 - math.pi * 16)) < 1e-3)["id"]
edges = mcp.call("inspect_model", what="edges", within=face, limit=20).json["edges"]
hole_edge = next(e for e in edges if e["type"] == "circle")["id"]
outer_edge = next(e for e in edges if e["type"] != "circle")["id"]
before = features()

# A face: a new sketch on itself, with its hole by default, or without; fully defined.
r = mcp.call("convert_entities", entities=[face])
name = sketch_name(r)
assert r.text.startswith(f"converted {face} into {name} (new sketch)."), r.text
assert f"{name} now has 2 closed and 0 open" in r.text, r.text
assert len(found(part_after(r.text, "New"), "line")) == 4 and len(found(part_after(r.text, "New"), "circle")) == 1, r.text
assert sketch(name)["status"] == "fully defined", sketch(name)["status"]
mcp.call("delete_features", features=[name])

r = mcp.call("convert_entities", entities=[face], loops="inner")
name = sketch_name(r)
assert f"{name} now has 1 closed and 0 open" in r.text and not found(part_after(r.text, "New"), "line"), r.text
assert len(found(part_after(r.text, "New"), "circle")) == 1, r.text
mcp.call("delete_features", features=[name])

r = mcp.call("convert_entities", entities=[face], loops="outer")
name = sketch_name(r)
assert f"{name} now has 1 closed and 0 open" in r.text, r.text
assert len(found(part_after(r.text, "New"), "line")) == 4 and not found(part_after(r.text, "New"), "circle"), r.text
mcp.call("cut_extrude_sketch", sketch_name=name, depth_mm=1)
check_volume(plate_volume - (800 - math.pi * 16) * 1, "a cut through what the outer loop converted")
for feature in reversed([f for f in features() if f not in before]):  # the cut keeps its sketch
    mcp.call("delete_features", features=[feature])
assert features() == before, features()
check_volume(plate_volume, "the plate again")

faces = mcp.call("inspect_model", what="faces", surface_type="plane", limit=100).json["faces"]
face = next(f for f in faces if abs(f["area_mm2"] - (800 - math.pi * 16)) < 1e-3)["id"]
edges = mcp.call("inspect_model", what="edges", within=face, limit=20).json["edges"]
hole_edge = next(e for e in edges if e["type"] == "circle")["id"]
outer_edge = next(e for e in edges if e["type"] != "circle")["id"]

# An edge, on a plane: only itself.
r = mcp.call("convert_entities", entities=[outer_edge], on=face)
name = sketch_name(r)
assert len(found(part_after(r.text, "New"), "line")) == 1, r.text
refused("convert_entities", "only goes with faces", entities=[hole_edge], sketch_name=name, loops="outer")
r = mcp.call("convert_entities", entities=[hole_edge], sketch_name=name)
assert r.text.startswith(f"converted {hole_edge} into {name}."), r.text
assert "(new sketch)" not in r.text and len(found(part_after(r.text, "New"), "circle")) == 1, r.text
mcp.call("delete_features", features=[name])

faces = mcp.call("inspect_model", what="faces", surface_type="plane", limit=100).json["faces"]
face = next(f for f in faces if abs(f["area_mm2"] - (800 - math.pi * 16)) < 1e-3)["id"]
r = mcp.call("convert_entities", entities=[face], loops="outer")
name = sketch_name(r)


# Follows its original (nothing keeps the plate's other sides square, so it's compared to them).
def line_lengths(of):
    return sorted(round(x["length_mm"], 2) for x in sketch(of)["segments"] if x["type"] == "line")


assert line_lengths(name) == line_lengths(plate) == [20, 20, 40, 40], (line_lengths(name), line_lengths(plate))
mcp.call("set_dimension", name=plate_dim, value_mm=50)
assert 50 in line_lengths(plate) and line_lengths(name) == line_lengths(plate), (line_lengths(name), line_lengths(plate))
mcp.call("set_dimension", name=plate_dim, value_mm=40)
mcp.call("delete_features", features=[name])

# An entity of another sketch, onto a plane.
faces = mcp.call("inspect_model", what="faces", surface_type="plane", limit=100).json["faces"]
face = next(f for f in faces if abs(f["area_mm2"] - (800 - math.pi * 16)) < 1e-3)["id"]
r = mcp.call("convert_entities", entities=[dim_line], on=face)
name = sketch_name(r)
[converted] = found(part_after(r.text, "New"), "line")
assert by_id(name)[converted]["length_mm"] == 40, by_id(name)
mcp.call("delete_features", features=[name])

# Into a new 3D sketch: edges stay where they are, and a rod can follow them.
faces = mcp.call("inspect_model", what="faces", surface_type="plane", limit=100).json["faces"]
face = next(f for f in faces if abs(f["area_mm2"] - (800 - math.pi * 16)) < 1e-3)["id"]
edges = mcp.call("inspect_model", what="edges", within=face, limit=20).json["edges"]
outer_edge = next(e for e in edges if e["type"] != "circle")["id"]
r = mcp.call("convert_entities", entities=[outer_edge], sketch_3d=True)
name = sketch_name(r)
assert name.startswith("3DSketch") and "(new 3D sketch)" in r.text, r.text
s = sketch(name)
assert s["is_3d"] and len(s["segments"]) == 1, s
edge = next(e for e in edges if e["id"] == outer_edge)
ends = [s["segments"][0]["start_mm"], s["segments"][0]["end_mm"]]
assert all(any(near(p, edge[k]) for k in ("start_mm", "end_mm")) for p in ends), (ends, edge)
mcp.call("delete_features", features=[name])

# Refusals leave nothing behind.
before = features()
r = mcp.call("sketch_line", points=[[50, 50], [60, 50]])
own_name = sketch_name(r)
[own] = found(part_after(r.text, "Ids"), "line")
before = features()
refused("convert_entities", "being drawn in", entities=[own], sketch_name=own_name)
refused("convert_entities", "say where", entities=[outer_edge])
refused("convert_entities", "not both", entities=[outer_edge], sketch_name=own_name, plane="top")
refused("convert_entities", "not both", entities=[outer_edge], sketch_3d=True, sketch_name=own_name)
refused("convert_entities", "isn't on a plane", entities=[outer_edge], sketch_3d=True, plane="top")
refused("convert_entities", "give the entities", entities=[], sketch_name=own_name)
refused("convert_entities", "be converted", entities=["origin"], sketch_name=own_name)
refused("convert_entities", "unknown", entities=["edge:99999"], sketch_name=own_name)
assert features() == before, features()
assert len(sketch(own_name)["segments"]) == 1, sketch(own_name)

# An edge coming straight at the sketch plane has nothing to draw.
top_face = next(f for f in mcp.call("inspect_model", what="faces", surface_type="plane", limit=100).json["faces"] if abs(f["area_mm2"] - (800 - math.pi * 16)) < 1e-3)
normal = top_face["normal"]
side_edges = mcp.call("inspect_model", what="edges", curve_type="line", limit=100).json["edges"]
upright = next(e for e in side_edges if abs(e["length_mm"] - 10) < 1e-3)["id"]
r = mcp.call("convert_entities", entities=[upright], on=top_face["id"], expect_error=True)
assert r.is_error and "couldn't convert" in r.text, r.text
assert features() == before, features()
no_dialogs()

# A face with no holes is fine with loops "all", though its second pass finds nothing.
mcp.call("create_part")
r = mcp.call("sketch_slot", start=[-20, 0], end=[20, 0], width_mm=10)
mcp.call("extrude_sketch", sketch_name=sketch_name(r), depth_mm=5)
slot_area = 40 * 10 + math.pi * 25
face = next(f for f in mcp.call("inspect_model", what="faces", surface_type="plane", limit=100).json["faces"] if abs(f["area_mm2"] - slot_area) < 1e-3)["id"]
r = mcp.call("convert_entities", entities=[face])
assert len(found(part_after(r.text, "New"), "line")) == 2 and len(found(part_after(r.text, "New"), "arc")) == 2, r.text
assert "now has 1 closed and 0 open" in r.text, r.text
no_dialogs()

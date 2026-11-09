# edit_fillet / edit_pattern / edit_sweep against real SolidWorks: each kind of
# change checked in the geometry (volumes, hole positions, boxes), and changes
# SolidWorks can't build leave the feature as it was (docs/notes/editing.md).
# Run with:
#   py devloop/mcp_client.py run devloop/scenarios/editing_features.py

import math
import re
import subprocess
from pathlib import Path

SWCOM = Path(__file__).resolve().parent.parent / "swcom.ps1"
TOL = 1e-3  # relative, for volumes


def sw_dialogs() -> list[str]:
    p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
                        str(SWCOM.parent / "sw.ps1"), "dialogs"], capture_output=True, text=True, timeout=60)
    return [line.strip() for line in p.stdout.splitlines() if line.strip()]


def bodies():
    return mcp.call("inspect_model", what="bodies").json["bodies"]


def volume():
    return sum(b["volume_mm3"] for b in bodies())


def check_volume(expected, what):
    got = volume()
    assert abs(got - expected) <= TOL * expected, f"{what}: volume {got}, expected {expected}"


def sketch_name(r):
    return re.search(r"Sketch\d+", r.text).group(0)


def edges(**filters):
    return mcp.call("inspect_model", what="edges", limit=500, **filters).json["edges"]


def fillet_area(r):
    """Cross-section a fillet of radius r removes from a right-angle edge."""
    return r * r * (1 - math.pi / 4)


def dot(a, b):
    return sum(x * y for x, y in zip(a, b))


# ---- Fillets ----------------------------------------------------------------
# A 40 x 20 x 10 box; its four 10 mm edges.

LENGTH = 10
mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=LENGTH)
box = 40 * 20 * LENGTH
short = [e for e in edges() if e["type"] == "line" and abs(e["length_mm"] - LENGTH) < 1e-6]
assert len(short) == 4, short
e1, e2, e3, e4 = (e["id"] for e in short)
plane_faces = mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]
ends = [f["id"] for f in plane_faces if abs(f["normal"][0]) > 0.99]  # the 40 x 20 faces, 10 apart
assert len(ends) == 2, plane_faces
r = mcp.call("fillet_edges", edges=[e1], radius_mm=3)
check_volume(box - fillet_area(3) * LENGTH, "Fillet1")

# Only the feature: a report listing what it rounds, by id.
r = mcp.call("edit_fillet", feature="Fillet1")
print("    report:", r.text)
assert r.text.startswith("Fillet1 (constant-radius fillet): 3 mm radius, tangent propagation on edge:"), r.text
assert "Nothing was changed" in r.text and "D1@Fillet1" in r.text, r.text
rounded = re.findall(r"edge:\d+", r.text.split(" on ", 1)[1].split(".", 1)[0])
assert len(rounded) == 1, rounded

r = mcp.call("edit_fillet", feature="Fillet1", radius_mm=2)
print("    radius:", r.text)
assert "was 3 mm radius, tangent propagation on" in r.text and "now 2 mm radius" in r.text, r.text
check_volume(box - fillet_area(2) * LENGTH, "radius 2")

# Another edge, by its inspect_model id from before the fillet.
r = mcp.call("edit_fillet", feature="Fillet1", add_edges=[e2])
print("    add:", r.text)
check_volume(box - 2 * fillet_area(2) * LENGTH, "two edges")

# Drop the first one, by the id the report gave.
r = mcp.call("edit_fillet", feature="Fillet1", remove_edges=rounded)
print("    remove:", r.text)
check_volume(box - fillet_area(2) * LENGTH, "second edge only")
r = mcp.call("edit_fillet", feature="Fillet1")
assert len(re.findall(r"edge:\d+", r.text.split(" on ", 1)[1].split(".", 1)[0])) == 1, r.text

# A whole new set.
r = mcp.call("edit_fillet", feature="Fillet1", edges=[e3, e4], tangent_propagation=False)
print("    replace:", r.text)
assert "now 2 mm radius on" in r.text and "tangent propagation on edge" not in r.text.split("now", 1)[1], r.text
check_volume(box - 2 * fillet_area(2) * LENGTH, "edges 3 and 4")

# All four.
r = mcp.call("edit_fillet", feature="Fillet1", edges=[e1, e2, e3, e4])
check_volume(box - 4 * fillet_area(2) * LENGTH, "all four edges")

# Faces: every edge of both end faces. Rounding both sides of the 10 mm box
# by more than 5 mm can't be made (SolidWorks lets one fillet overflow its
# faces, fillet.py, so 11 mm on the four edges above still builds): put back.
r = mcp.call("edit_fillet", feature="Fillet1", edges=ends)
print("    faces:", r.text)
assert f"now 2 mm radius on {ends[0]}, {ends[1]}." in r.text or f"now 2 mm radius on {ends[1]}, {ends[0]}." in r.text, r.text
v0 = volume()
assert v0 < box - 4 * fillet_area(2) * LENGTH, v0
r = mcp.call("edit_fillet", feature="Fillet1", radius_mm=6, expect_error=True)
print("    too big:", r.text)
assert r.is_error and ("unchanged" in r.text or "put back" in r.text), r.text
check_volume(v0, "after the refused radius")
r = mcp.call("edit_fillet", feature="Fillet1", edges=[e3, e4])
check_volume(box - 2 * fillet_area(2) * LENGTH, "edges 3 and 4 again")
v0 = volume()
r = mcp.call("edit_fillet", feature="Fillet1")
assert r.text.startswith("Fillet1 (constant-radius fillet): 2 mm radius on edge:"), r.text

# An edge the fillet itself made isn't there where it's built.
made = [e["id"] for e in edges() if e["type"] in ("arc", "circle")]
assert made, "no arc edges after the fillet"
r = mcp.call("edit_fillet", feature="Fillet1", add_edges=[made[0]], expect_error=True)
print("    own edge:", r.text)
assert r.is_error and "there where" in r.text, r.text
check_volume(v0, "after the refused edge")

r = mcp.call("edit_fillet", feature="Boss-Extrude1", radius_mm=1, expect_error=True)
assert r.is_error and "not a fillet" in r.text, r.text

# ---- Linear pattern -----------------------------------------------------------
# A 100 x 60 x 10 plate, a 4 mm hole at (-40, -20), copied along Right.

mcp.call("create_part")
planes = {f["name"]: f for f in mcp.call("inspect_model", what="features").json["features"]}
right_n, top_n = planes["Right Plane"]["normal"], planes["Top Plane"]["normal"]
mcp.call("sketch", shape="rectangle", width_mm=100, height_mm=60)
mcp.call("extrude_sketch", depth_mm=10)
plate = 100 * 60 * 10
r = mcp.call("sketch", shape="circle", center=[-40, -20], radius_mm=2)
r = mcp.call("extrude_sketch", cut=True, sketch_name=sketch_name(r), through_all=True)
cut = re.search(r"Cut-Extrude\d+", r.text).group(0)
hole = math.pi * 2 ** 2 * 10
r = mcp.call("linear_pattern_features", features=[cut], direction="right", count=5, spacing_mm=20)
pattern = re.search(r"LPattern\d+", r.text).group(0)
check_volume(plate - 5 * hole, "5 holes")


def hole_axes(radius):
    faces = mcp.call("inspect_model", what="faces", surface_type="cylinder", limit=500).json["faces"]
    return [f["axis_point_mm"] for f in faces if abs(f["radius_mm"] - radius) < 1e-6]


def spread(points, normal):
    values = [dot(p, normal) for p in points]
    return round(max(values) - min(values), 3)


r = mcp.call("edit_pattern", feature=pattern)
print("    report:", r.text)
assert r.text.startswith(f"{pattern} (linear pattern of {cut}): 5 instances along "), r.text
assert "20 mm apart." in r.text and "Nothing was changed" in r.text, r.text

r = mcp.call("edit_pattern", feature=pattern, count=3, spacing_mm=30)
print("    count/spacing:", r.text)
assert "was 5 instances along" in r.text and "now 3 instances along" in r.text and "30 mm apart" in r.text, r.text
check_volume(plate - 3 * hole, "3 holes")
assert spread(hole_axes(2), right_n) == 60, hole_axes(2)

r = mcp.call("edit_pattern", feature=pattern, direction2="top", count2=2, spacing2_mm=20)
print("    second direction:", r.text)
assert "times 2 along" in r.text and "(6 in all)" in r.text.split("now", 1)[1], r.text
check_volume(plate - 6 * hole, "3 x 2 holes")
assert spread(hole_axes(2), top_n) == 20, hole_axes(2)

r = mcp.call("edit_pattern", feature=pattern, count2=1)
assert "times" not in r.text.split("now", 1)[1], r.text
check_volume(plate - 3 * hole, "back to 3 holes")

# Along Top instead of Right: the holes line up across the plate.
r = mcp.call("edit_pattern", feature=pattern, direction="top", spacing_mm=20)
print("    direction:", r.text)
check_volume(plate - 3 * hole, "3 holes along Top")
axes = hole_axes(2)
assert spread(axes, right_n) == 0 and spread(axes, top_n) == 40, axes

# Copies off the plate find nothing to cut: put back.
v0 = volume()
r = mcp.call("edit_pattern", feature=pattern, direction="right", spacing_mm=30, reverse_direction=True,
             expect_error=True)
print("    off the plate:", r.text)
assert r.is_error, r.text
check_volume(v0, "after the refused reverse")

# Copy another cut, made above the pattern by rolling back.
mcp.call("rollback", before=pattern)
r = mcp.call("sketch", shape="circle", center=[-30, -20], radius_mm=1)  # copied along Top to y 0, 20
r = mcp.call("extrude_sketch", cut=True, sketch_name=sketch_name(r), through_all=True)
small = re.search(r"Cut-Extrude\d+", r.text).group(0)
mcp.call("rollback")
r = mcp.call("edit_pattern", feature=pattern, features=[small])
print("    seeds:", r.text)
assert r.text.startswith(f"changed {pattern} (linear pattern of {small})"), r.text
small_hole = math.pi * 1 ** 2 * 10
check_volume(plate - hole - 3 * small_hole, "the small hole copied")

r = mcp.call("edit_pattern", feature=pattern, angle_deg=90, expect_error=True)
assert r.is_error, r.text

# ---- Circular pattern ------------------------------------------------------

mcp.call("create_part")
r = mcp.call("sketch", shape="circle", center=[0, 0], radius_mm=30)
mcp.call("extrude_sketch", sketch_name=sketch_name(r), depth_mm=10)
disc = math.pi * 30 ** 2 * 10
r = mcp.call("sketch", shape="circle", center=[20, 0], radius_mm=2)
r = mcp.call("extrude_sketch", cut=True, sketch_name=sketch_name(r), through_all=True)
bolt = re.search(r"Cut-Extrude\d+", r.text).group(0)
rim = next(f["id"] for f in mcp.call("inspect_model", what="faces", surface_type="cylinder").json["faces"]
           if abs(f["radius_mm"] - 30) < 1e-6)
r = mcp.call("circular_pattern_features", features=[bolt], axis=rim, count=6)
circ = re.search(r"CirPattern\d+", r.text).group(0)
check_volume(disc - 6 * hole, "6 bolt holes")

r = mcp.call("edit_pattern", feature=circ)
print("    report:", r.text)
assert r.text.startswith(f"{circ} (circular pattern of {bolt}): 6 instances round "), r.text
assert "evenly all the way round" in r.text, r.text

r = mcp.call("edit_pattern", feature=circ, count=8)
print("    count:", r.text)
assert "now 8 instances round" in r.text and "evenly all the way round" in r.text.split("now", 1)[1], r.text
check_volume(disc - 8 * hole, "8 bolt holes")

r = mcp.call("edit_pattern", feature=circ, count=3, angle_deg=90)
print("    angle:", r.text)
assert "spread evenly over 90 degrees" in r.text, r.text
check_volume(disc - 3 * hole, "3 holes over 90 degrees")

r = mcp.call("edit_pattern", feature=circ, spacing_deg=30)
print("    spacing:", r.text)
assert "30 degrees apart" in r.text.split("now", 1)[1], r.text
check_volume(disc - 3 * hole, "3 holes 30 degrees apart")

r = mcp.call("edit_pattern", feature=circ, spacing_mm=5, expect_error=True)
assert r.is_error, r.text
r = mcp.call("edit_pattern", feature=circ, count2=2, expect_error=True)
assert r.is_error and "circular" in r.text, r.text

# ---- Sweeps -----------------------------------------------------------------
# A 10 x 10 bar swept 100 along u (Front's normal, in Top), as sweep.py.

mcp.call("create_part")
planes = {f["name"]: f for f in mcp.call("inspect_model", what="features").json["features"]}
u, v, w = (planes[n]["normal"] for n in ("Front Plane", "Right Plane", "Top Plane"))


def at(a=0.0, b=0.0, c=0.0):
    return [a * x + b * y + c * z for x, y, z in zip(u, v, w)]


def straight_path(length):
    return sketch_name(mcp.call("sketch", shape="line", plane="top", points=[at(0, 0), at(length, 0)]))


def extents():
    box = bodies()[0]["bbox_mm"]
    return sorted((round(hi - lo, 3) for lo, hi in zip(box["min"], box["max"])), reverse=True)


path = straight_path(100)
profile = sketch_name(mcp.call("sketch", shape="line", plane="front", closed=True, points=[
    at(0, -5, -5), at(0, 5, -5), at(0, 5, 5), at(0, -5, 5)]))
r = mcp.call("sweep_sketch", path=path, sketch_name=profile)
sweep = re.search(r"Sweep\d+", r.text).group(0)
check_volume(10 * 10 * 100, "bar")
short_path = straight_path(60)

r = mcp.call("edit_sweep", feature=sweep)
print("    report:", r.text)
assert r.text.startswith(f"{sweep} (sweep): {profile} along {path}."), r.text

r = mcp.call("edit_sweep", feature=sweep, path=short_path)
print("    path:", r.text)
assert f"was {profile} along {path}; now {profile} along {short_path}." in r.text, r.text
check_volume(10 * 10 * 60, "shorter bar")

r = mcp.call("edit_sweep", feature=sweep, circle_diameter_mm=6)
print("    circle:", r.text)
assert f"now a 6 mm circle along {short_path}." in r.text, r.text
check_volume(math.pi * 3 ** 2 * 60, "rod")

r = mcp.call("edit_sweep", feature=sweep, sketch_name=profile, twist_deg=45)
print("    profile + twist:", r.text)
assert f"now {profile} along {short_path}, twisted 45 degrees." in r.text, r.text
check_volume(10 * 10 * 60, "twisted bar")
# SolidWorks' box of the twisted faces is a little loose (60.36 x 14.15 seen).
assert all(abs(a - b) < 0.5 for a, b in zip(extents(), [60, 10 * math.sqrt(2), 10 * math.sqrt(2)])), extents()

r = mcp.call("edit_sweep", feature=sweep, twist_deg=0)
assert "twisted" not in r.text.split("now", 1)[1], r.text
assert all(abs(a - b) < 0.05 for a, b in zip(extents(), [60, 10, 10])), extents()

v0 = volume()
r = mcp.call("edit_sweep", feature=sweep, sketch_name=short_path, expect_error=True)
print("    profile = path:", r.text)
assert r.is_error, r.text
r = mcp.call("edit_sweep", feature=sweep, circle_diameter_mm=4, twist_deg=10, expect_error=True)
assert r.is_error and "twisted" in r.text, r.text
r = mcp.call("edit_sweep", feature="Boss-Extrude1", expect_error=True)
assert r.is_error, r.text
check_volume(v0, "after the refusals")

problems = mcp.call("diagnose_model").json.get("problems", [])
assert not problems, problems
assert not (open_dialogs := sw_dialogs()), open_dialogs

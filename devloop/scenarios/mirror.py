# mirror_sketch_entities / mirror_features: half a profile mirrored about its
# centerline into a closed one (extruded, checked by volume), a circle and a
# point mirrored about a horizontal centerline (positions checked), a cut
# mirrored about the Right plane and the base about its end face, bodies
# mirrored about faces with and without merging (body counts and volumes),
# and refusals that must leave nothing behind. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/mirror.py
#
# Sketches are on the Front plane in sketch coordinates; sketch x = 0 lies in
# the Right plane whichever way the template's planes face.

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


def sketch_name(r):
    return re.search(r"Sketch\d+", r.text).group(0)


def sketch(name):
    return mcp.call("inspect_sketch", sketch_name=name).json


def segment(info, sid):
    return next(s for s in info["segments"] if s["id"] == sid)


def close(a, b, tol=1e-6):
    if isinstance(a, list):
        return len(a) == len(b) and all(close(x, y, tol) for x, y in zip(a, b))
    return abs(a - b) <= tol


def plane_faces():
    return mcp.call("inspect_model", what="faces", surface_type="plane", limit=500).json["faces"]


def face_at(axis, value):
    """The planar face whose normal is along model `axis` (0..2) and that lies at `value`."""
    [face] = [
        f for f in plane_faces()
        if abs(abs(f["normal"][axis]) - 1) < 1e-9 and abs(f["point_on_plane_mm"][axis] - value) < 1e-6
    ]
    return face["id"]


# ---- Sketch: half a profile, mirrored about its centerline, closes ---------

mcp.call("create_part")
r = mcp.call("sketch", shape="line", points=[[0, 0], [20, 0], [20, 10], [0, 10]])
profile = sketch_name(r)
half = ids(r.text, "line")
assert len(half) == 3, r.text
r = mcp.call("sketch", shape="line", sketch_name=profile, points=[[0, -5], [0, 15]], construction=True)
[centerline] = ids(r.text, "line")
assert sketch(profile)["contours"] == {"closed": 0, "open": 1}
# The model axes along the Front plane's sketch x and y.
axes = sketch(profile)["sketch_axes"]
SX, SY = (max(range(3), key=lambda i: abs(axes[a][i])) for a in ("x", "y"))

r = mcp.call("mirror_sketch_entities", entities=half, axis=centerline)
assert r.text.startswith(f"mirrored {', '.join(half)} about {centerline} in {profile}. New: "), r.text
# The vertical side is copied; the two lines that end on the axis at a right
# angle are extended across it instead (SolidWorks' own behavior).
new_text, extended_text = r.text.split("New:")[1].split("Extended across the axis")
[copy] = ids(new_text, "line")
# Only the copy's two end points: not the midpoints SolidWorks adds on the axis.
assert copy not in half and len(ids(new_text, "point")) == 2, r.text
assert ids(extended_text.split("on the axis):")[1].split(". ")[0], "line") == [half[0], half[2]], r.text
assert f"{profile} now has 1 closed and 0 open contours" in r.text, r.text

info = sketch(profile)
c = segment(info, copy)
assert close(sorted([c["start_mm"], c["end_mm"]]), [[-20, 0], [-20, 10]]), c
for sid in (half[0], half[2]):
    assert abs(segment(info, sid)["length_mm"] - 40) < 1e-6, segment(info, sid)
symmetric = [rel for rel in info["relations"] if rel["type"] == "symmetric"]
assert symmetric, info["relations"]

r = mcp.call("extrude_sketch", sketch_name=profile, depth_mm=5)
block = 40 * 10 * 5
check_volume(block, "mirrored profile extruded")

# ---- Sketch: a circle and a point about a horizontal centerline ------------

# SolidWorks numbers segments per type, so this circle and line have the
# same GetID: their ids must still tell them apart (they didn't before
# 2026-10-20, when segment refs got their type).
r = mcp.call("sketch", shape="circle", center=[10, 30], radius_mm=3)
marks = sketch_name(r)
[circle] = ids(r.text, "circle")
r = mcp.call("sketch", shape="point", sketch_name=marks, points=[[5, 25]])
[point] = ids(r.text, "point")
r = mcp.call("sketch", shape="line", sketch_name=marks, points=[[-20, 20], [20, 20]], construction=True)
[horizontal] = ids(r.text, "line")
before = sketch(marks)
assert sorted(s["id"] for s in before["segments"]) == sorted([circle, horizontal]), before["segments"]

# Refusals first: nothing may change.
for bad, why in [
    (dict(entities=[circle], axis=circle), "a sketch line"),
    (dict(entities=[circle, horizontal], axis=horizontal), "include the axis"),
    (dict(entities=[circle], axis=centerline), ""),  # the axis is in another sketch
    (dict(entities=["Boss-Extrude1"], axis=horizontal), "a sketch entity id"),
]:
    r = mcp.call("mirror_sketch_entities", expect_error=True, **bad)
    assert r.is_error and why in r.text, (bad, r.text)
after = sketch(marks)
assert len(after["segments"]) == len(before["segments"]) and len(after["points"]) == len(before["points"])

r = mcp.call("mirror_sketch_entities", entities=[circle, point], axis=horizontal)
new_text = r.text.split("New:")[1].split(". Each copy")[0]
[new_circle] = ids(new_text, "circle")
new_points = ids(new_text, "point")
info = sketch(marks)
c = segment(info, new_circle)
assert close(c["center_mm"], [10, 10]) and abs(c["radius_mm"] - 3) < 1e-6, c
standalone = [p for p in info["points"] if close(p["at_mm"], [5, 15])]
assert len(standalone) == 1 and standalone[0]["id"] in new_points, (new_points, info["points"])

# ---- Features: a cut mirrored about the Right plane ------------------------

r = mcp.call("sketch", shape="circle", center=[10, 5], radius_mm=2)
hole_sketch = sketch_name(r)
r = mcp.call("extrude_sketch", cut=True, sketch_name=hole_sketch, through_all=True)
cut = re.search(r"Cut-Extrude\d+", r.text).group(0)
hole = math.pi * 2 ** 2 * 5
check_volume(block - hole, "one hole")

r = mcp.call("mirror_features", plane="right", features=[cut])
assert re.match(rf"created Mirror\d+: {cut} mirrored about \S.* The part has 1 solid body\.", r.text), r.text
check_volume(block - 2 * hole, "mirrored hole")
holes = mcp.call("inspect_model", what="faces", surface_type="cylinder").json["faces"]
xs = sorted(round(abs(f["axis_point_mm"][SX]), 6) for f in holes)
assert xs == [10, 10] and len({round(f["axis_point_mm"][SX], 6) for f in holes}) == 2, holes

# Refusals: nothing may be added.
tree = features()
edge = mcp.call("inspect_model", what="edges", limit=1).json["edges"][0]["id"]
cylinder = holes[0]["id"]
for bad, why in [
    (dict(plane=edge, features=[cut]), "a plane --"),
    (dict(plane=cylinder, features=[cut]), "flat"),
    (dict(plane="right", features=["NoSuchFeature"]), "NoSuchFeature"),
    (dict(plane="right", features=[profile]), "is a sketch"),
    (dict(plane="right", features=[cut], merge=False), "merge only applies"),
    (dict(plane="right"), "give the features or the bodies"),
]:
    r = mcp.call("mirror_features", expect_error=True, **bad)
    assert r.is_error and why in r.text, (bad, r.text)
# A hole mirrored about the top face lands beyond it and cuts nothing.
top_y = max(f["point_on_plane_mm"][SY] for f in plane_faces() if abs(abs(f["normal"][SY]) - 1) < 1e-9)
r = mcp.call("mirror_features", plane=face_at(SY, top_y), features=[cut], expect_error=True)
assert r.is_error and "changed nothing, so it was deleted again" in r.text, r.text
assert features() == tree, (tree, features())
check_volume(block - 2 * hole, "after refusals")

# ---- Features: the base mirrored about its end face ------------------------

end_x = max(f["point_on_plane_mm"][SX] for f in plane_faces() if abs(abs(f["normal"][SX]) - 1) < 1e-9)
r = mcp.call("mirror_features", plane=face_at(SX, end_x), features=["Boss-Extrude1"])
assert "Boss-Extrude1 mirrored about face" in r.text and "1 solid body" in r.text, r.text
check_volume(2 * block - 2 * hole, "mirrored base")
[b] = bodies()
assert abs(b["bbox_mm"]["max"][SX] - b["bbox_mm"]["min"][SX] - 80) < 1e-6, b

# ---- Bodies: mirrored about faces, kept separate and merged ----------------

mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=20, height_mm=10)
mcp.call("extrude_sketch", depth_mm=10)
cube = 20 * 10 * 10
[b] = bodies()
end_x = b["bbox_mm"]["max"][SX]

r = mcp.call("mirror_features", plane=face_at(SX, end_x), bodies=[b["id"]], merge=False)
assert "1 body mirrored about face" in r.text and "2 solid bodies" in r.text, r.text
check_volume(2 * cube, "two bodies")
assert len(bodies()) == 2

# The original again, about its other end face, merged: still two bodies.
r = mcp.call("mirror_features", plane=face_at(SX, b["bbox_mm"]["min"][SX]), bodies=[b["id"]])
assert "2 solid bodies" in r.text, r.text
check_volume(3 * cube, "merged third copy")
spans = sorted(round(x["bbox_mm"]["max"][SX] - x["bbox_mm"]["min"][SX], 6) for x in bodies())
assert spans == [20, 40], spans

for bad, why in [
    (dict(plane="right", bodies=["Boss-Extrude1"]), "a body id"),
    (dict(plane="right", bodies=[b["id"]], features=["Boss-Extrude1"]), "not both"),
    (dict(plane="right", features=[b["id"]]), "is a body"),
]:
    r = mcp.call("mirror_features", expect_error=True, **bad)
    assert r.is_error and why in r.text, (bad, r.text)

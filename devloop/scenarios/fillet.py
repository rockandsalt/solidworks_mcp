# fillet_edges / chamfer_edges on a 40 x 20 x 10 box: a fillet and three
# kinds of chamfer on its four short edges, each checked by the body's volume
# (the distance-distance chamfer also by which side face got which distance,
# flipped and not); refusals that must leave nothing behind; a tangent chain
# filleted with and without propagation (faces per fillet); then a face
# fillet on a disc's rim, checked by Pappus. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/fillet.py

import math

TOL = 1e-3  # relative, for volumes
LENGTH = 10  # the box's short edges


def body():
    bodies = mcp.call("inspect_model", what="bodies").json["bodies"]
    assert len(bodies) == 1, bodies
    return bodies[0]


def volume():
    return body()["volume_mm3"]


def check_volume(expected, what):
    got = volume()
    assert abs(got - expected) <= TOL * expected, f"{what}: volume {got}, expected {expected}"


def features():
    return [f["name"] for f in mcp.call("inspect_model", what="features").json["features"]]


def edges(**filters):
    return mcp.call("inspect_model", what="edges", limit=500, **filters).json["edges"]


def faces(**filters):
    return {f["id"]: f for f in mcp.call("inspect_model", what="faces", limit=500, **filters).json["faces"]}


def fillet_area(r):
    """Cross-section a fillet of radius r removes from a right-angle edge."""
    return r * r * (1 - math.pi / 4)


# ---- The box and its four short edges --------------------------------------

mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=LENGTH)
box = 40 * 20 * LENGTH
check_volume(box, "box")
short = [e for e in edges() if e["type"] == "line" and abs(e["length_mm"] - LENGTH) < 1e-6]
assert len(short) == 4, short
e1, e2, e3, e4 = short
box_faces = faces()

# ---- Fillet, then an equal-distance chamfer --------------------------------

r = mcp.call("fillet_edges", edges=[e1["id"]], radius_mm=3)
assert r.text.startswith("created Fillet1: a 3 mm radius on " + e1["id"]), r.text
expected = box - fillet_area(3) * LENGTH
check_volume(expected, "filleted box")

# The other edges' ids still work after the fillet.
r = mcp.call("chamfer_edges", edges=[e2["id"]], distance_mm=2)
assert "Chamfer1: 2 mm x 45 degrees (equal distance)" in r.text, r.text
expected -= 2 * 2 / 2 * LENGTH
check_volume(expected, "chamfered box")

# ---- Distance-distance: 2 mm into one face, 4 into the other; flip swaps them


# Faces the chamfer changes get new ids, so the side faces are told apart by
# their normals (one plane face per direction on the box).
def areas_by_normal():
    return {tuple(f["normal"]): f["area_mm2"] for f in faces(surface_type="plane").values()}


def cutbacks(edge):
    """How far (mm) the chamfer on `edge` cut back into each of its two side faces."""
    now = areas_by_normal()
    return {n: round((before_area[n] - now[n]) / LENGTH, 3) for n in sides}


sides = [tuple(box_faces[f]["normal"]) for f in e3["faces"]]
before_area = areas_by_normal()
r = mcp.call("chamfer_edges", edges=[e3["id"]], distance_mm=2, distance2_mm=4)
assert "2 mm x 4 mm on" in r.text, r.text
check_volume(expected - 2 * 4 / 2 * LENGTH, "distance-distance chamfer")
plain = cutbacks(e3)
assert sorted(plain.values()) == [2, 4], plain
mcp.call("delete_features", features=["Chamfer2"])
check_volume(expected, "chamfer deleted")
r = mcp.call("chamfer_edges", edges=[e3["id"]], distance_mm=2, distance2_mm=4, flip=True)
assert "2 mm x 4 mm, flipped" in r.text, r.text
flipped = cutbacks(e3)
assert flipped == {f: 6 - d for f, d in plain.items()}, (plain, flipped)
expected -= 2 * 4 / 2 * LENGTH
check_volume(expected, "flipped distance-distance chamfer")

# ---- Angle-distance: 2 mm along one face, 30 degrees ------------------------

r = mcp.call("chamfer_edges", edges=[e4["id"]], distance_mm=2, angle_deg=30)
assert "2 mm x 30 degrees" in r.text, r.text
# The angle is measured from the face the distance is on: the other leg is
# 2 tan(30) (flip moves the 2 mm to the other face; seen 2026-10-19).
expected -= 2 * (2 * math.tan(math.radians(30))) / 2 * LENGTH
check_volume(expected, "angle-distance chamfer")

# ---- Refusals leave nothing behind -----------------------------------------

before = features()
# One edge with a radius bigger than its faces works (SolidWorks lets the
# fillet overflow), but rounding both sides of the 10 mm plate by more than
# 5 mm doesn't.
plate_sides = [f for f, info in faces(surface_type="plane").items() if abs(info["normal"][0]) > 0.99]
assert len(plate_sides) == 2, plate_sides
r = mcp.call("fillet_edges", edges=plate_sides, radius_mm=6, expect_error=True)
assert r.is_error and "couldn't fillet" in r.text, r.text
r = mcp.call("chamfer_edges", edges=plate_sides, distance_mm=6, expect_error=True)
assert r.is_error and "couldn't chamfer" in r.text, r.text
top = max((e for e in edges() if e["type"] == "line"), key=lambda e: e["length_mm"])
r = mcp.call("fillet_edges", edges=[body()["id"]], radius_mm=1, expect_error=True)
assert r.is_error and "an edge or face id" in r.text, r.text
r = mcp.call("fillet_edges", edges=[e1["id"]], radius_mm=1, expect_error=True)  # filleted away
assert r.is_error, r.text
print("filleted-away edge:", r.text)
r = mcp.call("chamfer_edges", edges=[top["id"]], distance_mm=1, flip=True, expect_error=True)
assert r.is_error and "flip" in r.text, r.text
assert features() == before, (before, features())
check_volume(expected, "after refusals")

# ---- Tangent chains: the box's end faces run line - fillet arc - line ------
# Each end face's outline has e1's fillet arc between two lines; filleting one
# of those lines rounds the whole chain (3 faces) unless propagation is off.


def chain_line(face_id):
    """A line edge of the face that ends where the fillet arc does."""
    outline = edges(within=face_id)
    arcs = [e for e in outline if e["type"] != "line"]
    assert len(arcs) == 1, outline
    ends = [arcs[0]["start_mm"], arcs[0]["end_mm"]]
    near = lambda p: any(math.dist(p, q) < 1e-6 for q in ends)
    lines = [e for e in outline if e["type"] == "line" and (near(e["start_mm"]) or near(e["end_mm"]))]
    assert lines, outline
    return lines[0]["id"]


end_faces = [f for f, info in faces(surface_type="plane").items()
             if any(e["type"] != "line" for e in edges(within=f)) and info["area_mm2"] < 40 * 20]
assert len(end_faces) == 2, end_faces
r = mcp.call("fillet_edges", edges=[chain_line(end_faces[0])], radius_mm=1)
chain = r.text.split(":")[0].removeprefix("created ")
n = len(faces(within=chain))
assert n == 3, f"propagated fillet has {n} faces"
r = mcp.call("fillet_edges", edges=[chain_line(end_faces[1])], radius_mm=1, tangent_propagation=False)
single = r.text.split(":")[0].removeprefix("created ")
n = len(faces(within=single))
assert n == 1, f"unpropagated fillet has {n} faces"

# ---- A disc's rim rounded through its face (Pappus) -------------------------

mcp.call("create_part")
mcp.call("sketch_circle", plane="front", center=[0, 0], diameter_mm=40)
mcp.call("extrude_sketch", depth_mm=10)
disc = math.pi * 20 ** 2 * 10
check_volume(disc, "disc")
flat = [f for f, info in faces(surface_type="plane").items()]
assert len(flat) == 2, flat
r = mcp.call("fillet_edges", edges=[flat[0]], radius_mm=2)
assert flat[0] in r.text, r.text
# The removed corner's centroid sits r (10 - 3 pi) / (12 - 3 pi) in from the rim.
inset = 2 * (10 - 3 * math.pi) / (12 - 3 * math.pi)
check_volume(disc - 2 * math.pi * (20 - inset) * fillet_area(2), "disc with a rounded rim")
r = mcp.call("chamfer_edges", edges=[flat[1]], distance_mm=1)
# A 1 x 1 triangle, centroid 1/3 in from the rim.
check_volume(disc - 2 * math.pi * (20 - inset) * fillet_area(2) - 2 * math.pi * (20 - 1 / 3) * 0.5,
             "disc with a rounded and a chamfered rim")

mcp.call("render_views")

# hole_wizard against real SolidWorks: face mode with several locations,
# with none (the face's center), each hole kind, the user's own sketch of
# points as the placement sketch, and the refusals, which must leave nothing
# behind. Written as a characterization test before HoleWizard.cpp was split
# into steps (2026-10-26). Run with:
#   py devloop/mcp_client.py run devloop/scenarios/holes.py
#
# 60 x 40 x 10 plate. With this machine's template it spans x 0..10,
# y -30..30, z -20..20 (docs/notes/devloop.md: Front Plane faces +X).

import re


def features() -> list[str]:
    return [f["name"] for f in mcp.call("inspect_model", what="features").json["features"]]


def centers(text: str) -> list[list[float]]:
    """The hole centers' model positions listed in a hole_wizard reply."""
    return [[float(c) for c in m.split(",")] for m in re.findall(r"point:\d+ at \[([^\]]+)\]", text)]


def same_points(got, expected, what):
    key = lambda p: tuple(round(c, 3) for c in p)
    assert sorted(map(key, got)) == sorted(map(key, expected)), f"{what}: centers {got}, expected {expected}"


def sketch_name(text: str) -> str:
    m = re.search(r"\b(Sketch\d+)\b", text)
    assert m, text
    return m.group(1)


mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=60, height_mm=40)  # Sketch1
mcp.call("extrude_sketch", depth_mm=10)  # Boss-Extrude1
faces = mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]


def face_at(axis: int, value: float) -> str:
    hits = [f["id"] for f in faces
            if abs(f["normal"][axis]) > 0.99 and abs(f["point_on_plane_mm"][axis] - value) < 1e-6]
    assert len(hits) == 1, (axis, value, faces)
    return hits[0]


far, near = face_at(0, 10), face_at(0, 0)

# ---- Face mode --------------------------------------------------------------
# Three locations in one feature; the points are projected onto the face, so
# x = 7 lands on x = 10.
r = mcp.call("hole_wizard", kind="hole", size="M5", on=far, depth_mm=5,
             locations=[[10, -20, 10], [7, 0, 10], [10, 20, 10]])
print("    3 holes:", r.text)
assert r.text.startswith("created ") and "3 M5 holes" in r.text, r.text
same_points(centers(r.text), [[10, -20, 10], [10, 0, 10], [10, 20, 10]], "3 holes")

# No locations: one hole at the center of the face's box (through the plate,
# so the far face's center is taken from here on).
r = mcp.call("hole_wizard", kind="counterbore", size="M4", on=near, through_all=True)
print("    counterbore:", r.text)
assert "1 M4 counterbore" in r.text and "counterbore Ø" in r.text and "through all" in r.text, r.text
same_points(centers(r.text), [[0, 0, 0]], "counterbore")

r = mcp.call("hole_wizard", kind="tapped", size="M6", on=far, locations=[[10, -20, -10]], depth_mm=8,
             thread_depth_mm=6, cosmetic_thread=True)
print("    tapped:", r.text)
assert "1 M6 tapped" in r.text and "thread depth 6" in r.text, r.text

r = mcp.call("hole_wizard", kind="countersink", size="M5", on=far, locations=[[10, 20, -10]], through_all=True,
             countersink_angle_deg=90)
print("    countersink:", r.text)
assert "1 M5 countersink" in r.text and "x 90°" in r.text, r.text

before = features()
# Location 2 is beyond the plate. (A location inside an earlier hole is NOT
# refused: seen 2026-10-26, [10, 0, 10] in the first M5 hole was accepted and
# got a second hole, although the ray check is meant to catch that.)
r = mcp.call("hole_wizard", kind="hole", size="M5", on=far, depth_mm=5,
             locations=[[10, 10, 0], [10, 50, 0]], expect_error=True)
print("    off the face:", r.text)
assert "hole location 2" in r.text and "on that face" in r.text, r.text
r = mcp.call("hole_wizard", kind="hole", size="M5", on=far, depth_mm=5, locations=[[10, 50, 0]], expect_error=True)
assert "hole location 1" in r.text, r.text
r = mcp.call("hole_wizard", kind="hole", size="M77", on=far, depth_mm=5, locations=[[10, 10, -15]], expect_error=True)
print("    bad size:", r.text)
assert "exists for that standard" in r.text, r.text
assert features() == before, "a refused hole left something behind"

# ---- Sketch mode ------------------------------------------------------------
# Two points plus a construction line: the line's ends aren't holes.
r = mcp.call("sketch", shape="point", on=far, points=[[10, -10, 0], [10, 10, 0]])
points_sketch = sketch_name(r.text)
mcp.call("sketch", shape="line", sketch_name=points_sketch, points=[[10, -25, -15], [10, 25, -15]], construction=True)
r = mcp.call("hole_wizard", kind="hole", size="M3", sketch_name=points_sketch, depth_mm=4)
print("    from sketch:", r.text)
assert "2 M3 holes" in r.text and f"placement sketch {points_sketch}" in r.text, r.text
assert "now belongs to the hole feature" in r.text, r.text
same_points(centers(r.text), [[10, -10, 0], [10, 10, 0]], "from sketch")

before = features()
# Used by a feature already: the boss's sketch, and the one just used.
for used in ("Sketch1", points_sketch):
    r = mcp.call("hole_wizard", kind="hole", size="M3", sketch_name=used, depth_mm=4, expect_error=True)
    assert "already used by a feature" in r.text, r.text
# A real (non-construction) line.
r = mcp.call("sketch", shape="point", on=far, points=[[10, 0, -15]])
bad = sketch_name(r.text)
mcp.call("sketch", shape="line", sketch_name=bad, points=[[10, -25, 15], [10, -15, 15]])
r = mcp.call("hole_wizard", kind="hole", size="M3", sketch_name=bad, depth_mm=4, expect_error=True)
print("    real line:", r.text)
assert "construction geometry" in r.text, r.text
# Only construction geometry: nothing to put holes at.
r = mcp.call("sketch", shape="line", on=far, points=[[10, -25, 5], [10, 25, 5]], construction=True)
empty = sketch_name(r.text)
r = mcp.call("hole_wizard", kind="hole", size="M3", sketch_name=empty, depth_mm=4, expect_error=True)
assert "no sketch points" in r.text, r.text
# Validation before SolidWorks.
mcp.call("hole_wizard", kind="hole", size="M3", sketch_name=empty, on=far, depth_mm=4, expect_error=True)
mcp.call("hole_wizard", kind="hole", size="M3", on=far, depth_mm=-1, expect_error=True)
after = features()
assert [f for f in after if f not in before] == [bad, empty], f"refusals changed the tree: {before} -> {after}"

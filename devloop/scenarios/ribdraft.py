# rib_sketch and draft_faces. Ribs: a web in a tray (x 0..10, floor x 0..2, cavity
# y +-18, z +-8, open at x = 10), a gusset in an L bracket, chains on a plate, each
# checked by the volume worked out by hand. Drafts: a 40 x 20 x 10 box's walls about
# its base, each way, about a plane through the middle, with propagation. Refusals
# must leave the tree and the body as they were, and a refused rib its sketch usable.
# Run with:
#   py devloop/mcp_client.py run devloop/scenarios/ribdraft.py

import math
import re
import subprocess
import time
from pathlib import Path

SW = Path(__file__).resolve().parent.parent / "sw.ps1"
TOL = 2e-3  # relative, for volumes
BOX = 40 * 20 * 10
TRAY = BOX - 36 * 16 * 8
WEB = 36 * 4  # the web's area: wall to wall, down from x = 6 to the floor at x = 2


def inspect(what, **kwargs):
    return mcp.call("inspect_model", what=what, **kwargs).json[what]


def volume():
    return sum(b["volume_mm3"] for b in inspect("bodies"))


def check_volume(expected, what):
    got = volume()
    assert abs(got - expected) <= TOL * expected, f"{what}: volume {got}, expected {expected}"


def features():
    return [f["name"] for f in inspect("features")]


def faces():
    return inspect("faces", limit=500)


def sketch_name(r):
    return re.search(r"Sketch\d+", r.text).group(0)


def rib_name(r):
    return re.search(r"Rib\d+", r.text).group(0)


def no_dialogs(when):
    """No dialog stays open: a rebuild can flash an untitled one that closes by itself."""
    for _ in range(5):
        p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(SW), "dialogs"],
                           capture_output=True, text=True, timeout=60)
        if p.stdout.strip() == "":
            return
        print(f"    {when}: dialog open: {p.stdout.strip()}")
        time.sleep(2)
    assert False, f"{when}: dialogs stay open: {p.stdout.strip()}"


def refused(tool, expected_text, **args):
    tree, v = features(), volume()
    r = mcp.call(tool, expect_error=True, **args)
    assert r.is_error and expected_text in r.text, r.text
    assert features() == tree, (features(), tree)
    assert abs(volume() - v) < 1e-6, (volume(), v)
    return r


def block(depth=10):
    """40 x 20 on the Front plane, its depth along +x (this template's planes, see devloop.md)."""
    mcp.call("create_part")
    mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
    mcp.call("extrude_sketch", depth_mm=depth)
    [b] = inspect("bodies")
    assert b["bbox_mm"]["min"] == [0.0, -20.0, -10.0] and b["bbox_mm"]["max"] == [depth, 20.0, 10.0], b


def tray():
    block()
    open_face = next(f["id"] for f in faces() if abs(f["area_mm2"] - 800) < 1e-6 and f["normal"][0] > 0)
    mcp.call("shell_body", thickness_mm=2, open_faces=[open_face])
    check_volume(TRAY, "tray")


def plane_at(reference, offset):
    mcp.call("create_reference_plane", references=[{"entity": reference, "offset_mm": offset}])
    return [f["id"] for f in inspect("features") if f["type"] == "RefPlane"][-1]


def web_sides():
    """Where the web's two big faces are along z."""
    return sorted(f["point_on_plane_mm"][2] for f in faces() if f["type"] == "plane" and abs(f["normal"][2]) > 0.9
                  and abs(f["area_mm2"] - WEB) < 1e-3)


# ---- A web across a tray: within the sketch plane ----------------------------------

tray()
web = sketch_name(mcp.call("sketch_line", plane="top", points=[[6, -5, 0], [6, 5, 0]]))
r = mcp.call("rib_sketch", thickness_mm=2)  # the newest unused sketch; its ends carry on to the walls
print("    " + r.text)
no_dialogs("after the first rib")
assert re.match(rf"created Rib\d+ from {web}: 2 mm thick, adding 288\.000 mm\^3\.", r.text), r.text
assert "under defined" in r.text, r.text
check_volume(TRAY + WEB * 2, "web")
assert web_sides() == [-1.0, 1.0], web_sides()
assert [f["type"] for f in inspect("features") if f["name"] == rib_name(r)] == ["Rib"]
assert "reversed way" in r.text, r.text  # for this line the default way has no material

# delete_features takes it out, leaving the sketch to use again; asking for the
# way that works isn't reported as a flip, and the other way isn't tried for you.
assert "rib" in mcp.call("delete_features", features=[rib_name(r)]).text
check_volume(TRAY, "web deleted")
r = mcp.call("rib_sketch", thickness_mm=2, sketch_name=web, reverse_direction=True)
assert "reversed way" not in r.text, r.text
check_volume(TRAY + WEB * 2, "web, way asked for")
mcp.call("delete_features", features=[rib_name(r)])

# All on one side, and the other.
r = mcp.call("rib_sketch", thickness_mm=2, sketch_name=web, side="one")
assert "on one side of the sketch" in r.text, r.text
check_volume(TRAY + WEB * 2, "one side")
one = web_sides()
mcp.call("delete_features", features=[rib_name(r)])
r = mcp.call("rib_sketch", thickness_mm=2, sketch_name=web, side="other")
check_volume(TRAY + WEB * 2, "other side")
other = web_sides()
mcp.call("delete_features", features=[rib_name(r)])
assert sorted([one, other]) == [[-2.0, 0.0], [0.0, 2.0]], (one, other)

# Drafted: thinner towards the floor, or with draft_outward thicker. 3 degrees a side over the 4 mm.
taper = 4 * math.tan(math.radians(3))
r = mcp.call("rib_sketch", thickness_mm=2, sketch_name=web, draft_deg=3)
check_volume(TRAY + WEB * (2 - taper), "drafted")
mcp.call("delete_features", features=[rib_name(r)])
r = mcp.call("rib_sketch", thickness_mm=2, sketch_name=web, draft_deg=3, draft_outward=True)
check_volume(TRAY + WEB * (2 + taper), "drafted outward")
mcp.call("delete_features", features=[rib_name(r)])

# ---- Normal to the sketch: the same web, drawn seen from above -------------------------

above = plane_at("front", 6)
web2 = sketch_name(mcp.call("sketch_line", on=above, points=[[6, -5, 0], [6, 5, 0]]))
r = mcp.call("rib_sketch", thickness_mm=2, sketch_name=web2, normal_to_sketch=True)
print("    " + r.text)
check_volume(TRAY + WEB * 2, "web, normal to the sketch")
mcp.call("delete_features", features=[rib_name(r)])
refused("rib_sketch", "couldn't make a rib", thickness_mm=2, sketch_name=web2, normal_to_sketch=True,
        reverse_direction=True)

# ---- A gusset: a triangle in an L bracket ----------------------------------------------

mcp.call("create_part")
mcp.call("sketch_line", plane="top", closed=True, points=[
    [0, -20, 0], [0, 20, 0], [5, 20, 0], [5, -15, 0], [30, -15, 0], [30, -20, 0]])
mcp.call("extrude_sketch", depth_mm=20)
bracket = 40 * 5 * 20 + 25 * 5 * 20
check_volume(bracket, "bracket")
middle = plane_at("top", 10)
hypotenuse = sketch_name(mcp.call("sketch_line", on=middle, points=[[5, 5, 10], [25, -15, 10]]))
r = mcp.call("rib_sketch", thickness_mm=3, sketch_name=hypotenuse)
print("    " + r.text)
check_volume(bracket + 20 * 20 / 2 * 3, "gusset")
mcp.call("delete_features", features=[rib_name(r)])
# A line short of both walls makes the same gusset.
short = sketch_name(mcp.call("sketch_line", on=middle, points=[[10, 0, 10], [20, -10, 10]]))
r = mcp.call("rib_sketch", thickness_mm=3, sketch_name=short)
check_volume(bracket + 20 * 20 / 2 * 3, "gusset from a short line")

# ---- Chains over a plate: they must come down to it ------------------------------------

block(depth=5)
plate = 40 * 20 * 5
r = mcp.call("rib_sketch", thickness_mm=2, sketch_name=sketch_name(
    mcp.call("sketch_line", plane="top", points=[[5, -20, 0], [15, -20, 0], [15, 20, 0], [5, 20, 0]])))
check_volume(plate + 40 * 10 * 2, "a staple on the plate")
mcp.call("delete_features", features=[rib_name(r)])
roof = sketch_name(mcp.call("sketch_line", plane="top", points=[[5, -20, 0], [15, 0, 0], [5, 20, 0]]))
r = mcp.call("rib_sketch", thickness_mm=2, sketch_name=roof)
check_volume(plate + 40 * 10 / 2 * 2, "a roof on the plate")
mcp.call("delete_features", features=[rib_name(r)])

# ---- Rib refusals: free ends make nothing, and the sketch stays usable -----------------

free = sketch_name(mcp.call("sketch_line", plane="top", points=[[15, -10, 0], [15, 10, 0]]))
r = refused("rib_sketch", "tried both ways", thickness_mm=2, sketch_name=free)
print("    " + r.text)
refused("rib_sketch", "tried both ways", thickness_mm=2, sketch_name=sketch_name(
    mcp.call("sketch_line", plane="top", points=[[100, 100, 0], [120, 110, 0]])))
assert free in features()
refused("rib_sketch", "more than 0", thickness_mm=0, sketch_name=roof)
refused("rib_sketch", "under 90", thickness_mm=2, sketch_name=roof, draft_deg=90)
refused("rib_sketch", "needs draft_deg", thickness_mm=2, sketch_name=roof, draft_outward=True)
refused("rib_sketch", "feature-tree name", thickness_mm=2, sketch_name="line:1")
refused("rib_sketch", "NoSuchSketch", thickness_mm=2, sketch_name="NoSuchSketch")
no_dialogs("after the rib refusals")
mcp.call("rib_sketch", thickness_mm=2, sketch_name=roof)
check_volume(plate + 40 * 10 / 2 * 2, "a roof after the refusals")

mcp.call("create_part")
mcp.call("sketch_line", plane="top", points=[[5, -20, 0], [15, 0, 0], [5, 20, 0]])
r = mcp.call("rib_sketch", thickness_mm=2, expect_error=True)
assert r.is_error and "no solid body" in r.text, r.text

# ---- Draft on one wall: a wedge off a side, about the base ----------------------------

ANGLE = 5
TAN = math.tan(math.radians(ANGLE))
WEDGE = 10 * (10 * TAN) / 2 * 20  # off an end: 10 tall, 20 wide


def ends_and_flats():
    fs = faces()
    return ([f["id"] for f in fs if abs(f["area_mm2"] - 200) < 1e-6],
            [f["id"] for f in fs if abs(f["area_mm2"] - 800) < 1e-6])


block()
ends, flats = ends_and_flats()
r = mcp.call("draft_faces", angle_deg=ANGLE, faces=[ends[0]], neutral_plane=flats[0])
print("    " + r.text)
no_dialogs("after the first draft")
assert r.text.startswith(f"created Draft1: 5 degrees on {ends[0]} about {flats[0]};"), r.text
one_way = volume() - BOX
assert abs(abs(one_way) - WEDGE) < TOL * WEDGE, (one_way, WEDGE)
assert ("smaller" in r.text) == (one_way < 0), r.text
assert [f["type"] for f in inspect("features") if f["name"] == "Draft1"] == ["Draft"]

# delete_features takes it out; flip goes the other way.
assert "draft" in mcp.call("delete_features", features=["Draft1"]).text
check_volume(BOX, "draft deleted")
ends, flats = ends_and_flats()
r = mcp.call("draft_faces", angle_deg=ANGLE, faces=[ends[0]], neutral_plane=flats[0], flip=True)
print("    flipped: " + r.text)
other_way = volume() - BOX
assert one_way * other_way < 0, (one_way, other_way)
assert abs(abs(other_way) - WEDGE) < TOL * WEDGE, other_way

# ---- A neutral plane through the middle: one half grows, the other shrinks ------------

block()
ends, flats = ends_and_flats()
mcp.call("create_reference_plane", references=[{"entity": flats[0], "relation": "mid_plane"},
                                                {"entity": flats[1], "relation": "mid_plane"}])
plane = [f["name"] for f in inspect("features") if f["type"] == "RefPlane"][-1]
r = mcp.call("draft_faces", angle_deg=ANGLE, faces=[ends[0]], neutral_plane=plane)
print(f"    mid plane: {r.text}")
assert "volume stayed the same" in r.text, r.text
check_volume(BOX, "the two halves cancel out")
assert sum(f["area_mm2"] for f in faces()) != 2 * (800 + 400 + 200), "the wall leans"

# ---- Two walls, and a default plane as the neutral plane --------------------------------

block()
ends, flats = ends_and_flats()
mcp.call("draft_faces", angle_deg=ANGLE, faces=ends, neutral_plane=flats[0])
assert abs(abs(volume() - BOX) - 2 * WEDGE) < TOL * 2 * WEDGE, volume()

block()
ends, flats = ends_and_flats()
mcp.call("draft_faces", angle_deg=ANGLE, faces=ends, neutral_plane="front")  # the base of this box
assert abs(abs(volume() - BOX) - 2 * WEDGE) < TOL * 2 * WEDGE, volume()

# ---- Propagation, on a box with a hole through it ----------------------------------------

HOLE = math.pi * 5 ** 2 * 10


def holed():
    block()
    mcp.call("sketch_circle", plane="front", center=[0, 0, 0], radius_mm=5)
    mcp.call("cut_extrude_sketch", through_all=True)
    check_volume(BOX - HOLE, "holed box")
    fs = faces()
    return (next(f["id"] for f in fs if f["type"] == "cylinder"),
            [f["id"] for f in fs if abs(f["area_mm2"] - 200) < 1e-6],
            [f["id"] for f in fs if f["type"] == "plane" and f["area_mm2"] > 700])


def kinds():
    return sorted(f["type"] for f in faces())


# What each wall gains, each leaning out by TAN per mm over the 10 mm height.
OUTER = 6000 * TAN + 4 * TAN ** 2 * 1000 / 3  # the 40 x 20 outline growing
INNER = math.pi * (500 * TAN - TAN ** 2 * 1000 / 3)  # the hole narrowing

# The faces given are drafted too, whatever their loop.
for propagation, wall_of_hole, expected, surface in (("all", True, OUTER + INNER, "cone"),
                                                     ("all", False, OUTER + INNER, "cone"),
                                                     ("outer", False, OUTER, "cylinder"),
                                                     ("inner", True, INNER, "cone"),
                                                     ("outer", True, OUTER + INNER, "cone")):
    hole, ends, flats = holed()
    mcp.call("draft_faces", angle_deg=ANGLE, faces=[hole if wall_of_hole else ends[0]],
             neutral_plane=flats[0], propagation=propagation)
    check_volume(BOX - HOLE + expected, propagation)
    assert surface in kinds(), (propagation, kinds())

# Tangent: a filleted corner goes along, so more than the wall alone changes.
block()
short = next(e for e in inspect("edges", limit=100) if e["type"] == "line" and abs(e["length_mm"] - 10) < 1e-6)
mcp.call("fillet_edges", edges=[short["id"]], radius_mm=3)
fs = faces()
flats = [f["id"] for f in fs if f["type"] == "plane" and f["area_mm2"] > 700]
wall = next(f["id"] for f in fs if f["type"] == "plane" and abs(f["area_mm2"] - 37 * 10) < 1e-6)  # next to the fillet
before = volume()
r = mcp.call("draft_faces", angle_deg=ANGLE, faces=[wall], neutral_plane=flats[0], propagation="tangent")
print(f"    tangent: {r.text[:80]}; change {volume() - before}, kinds {kinds()}")
assert "cone" in kinds(), kinds()  # the fillet leans with the wall

# ---- Draft refusals ---------------------------------------------------------------------

block()
ends, flats = ends_and_flats()
edge = inspect("edges", limit=1)[0]["id"]
refused("draft_faces", "under 90", angle_deg=90, faces=[ends[0]], neutral_plane=flats[0])
refused("draft_faces", "under 90", angle_deg=0, faces=[ends[0]], neutral_plane=flats[0])
refused("draft_faces", "at least one", angle_deg=5, faces=[], neutral_plane=flats[0])
refused("draft_faces", "a face id", angle_deg=5, faces=[edge], neutral_plane=flats[0])
refused("draft_faces", "a face id", angle_deg=5, faces=["top"], neutral_plane=flats[0])
refused("draft_faces", "both", angle_deg=5, faces=[ends[0], flats[0]], neutral_plane=flats[0])
refused("draft_faces", "a plane", angle_deg=5, faces=[ends[0]], neutral_plane=edge)
refused("draft_faces", "NoSuchPlane", angle_deg=5, faces=[ends[0]], neutral_plane="NoSuchPlane")
# A face parallel to the neutral plane has nothing to pivot about.
r = refused("draft_faces", "", angle_deg=5, faces=[flats[1]], neutral_plane=flats[0])
print("    parallel face: " + r.text)
# 80 degrees leaning in runs past the opposite wall, 57 mm over the 40: SolidWorks makes
# the wedge that's left, not an error.
r = mcp.call("draft_faces", angle_deg=80, faces=[ends[0]], neutral_plane=flats[0], flip=one_way > 0)
check_volume(40 * (40 * math.tan(math.radians(10))) / 2 * 20, "a wedge")
mcp.call("delete_features", features=[re.search(r"Draft[0-9]+", r.text).group(0)])
ends, flats = ends_and_flats()
no_dialogs("at the end")

# A plain draft still works after the refusals.
mcp.call("draft_faces", angle_deg=ANGLE, faces=[ends[0]], neutral_plane=flats[0])
assert abs(abs(volume() - BOX) - WEDGE) < TOL * WEDGE, volume()
mcp.call("render_views")

# ---- A picture of a web ------------------------------------------------------------------

tray()
mcp.call("sketch_line", plane="top", points=[[6, -5, 0], [6, 5, 0]])
mcp.call("rib_sketch", thickness_mm=2)
mcp.call("render_views")

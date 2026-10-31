# shell_body on a 40 x 20 x 10 box: open and closed, inward and outward, walls
# with their own thicknesses, a fillet carried through to the inside, a disc,
# one body of two, each checked by the body's volume and box; refusals (walls
# too thick, walls that exactly fill the body and change nothing, wrong ids)
# must leave nothing behind, also no thicknesses for the next shell. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/shell.py

import math
import subprocess
import time
from pathlib import Path

SW = Path(__file__).resolve().parent.parent / "sw.ps1"
TOL = 1e-3  # relative, for volumes
BOX = 40 * 20 * 10


def bodies():
    return mcp.call("inspect_model", what="bodies").json["bodies"]


def volumes():
    return sorted(b["volume_mm3"] for b in bodies())


def check_volumes(expected, what):
    got = volumes()
    assert len(got) == len(expected), f"{what}: {got}, expected {expected}"
    for g, e in zip(got, sorted(expected)):
        assert abs(g - e) <= TOL * e, f"{what}: volumes {got}, expected {expected}"


def box_size():
    [b] = bodies()
    return sorted(hi - lo for lo, hi in zip(b["bbox_mm"]["min"], b["bbox_mm"]["max"]))


def check_box(expected, what):
    got = box_size()
    assert all(abs(g - e) < 1e-3 for g, e in zip(got, sorted(expected))), f"{what}: box {got}, expected {expected}"


def features():
    return [f["name"] for f in mcp.call("inspect_model", what="features").json["features"]]


def faces():
    return mcp.call("inspect_model", what="faces", limit=500).json["faces"]


def block():
    """The box. Returns a big face (800 mm2, the "top") and the two ends (200 mm2)."""
    mcp.call("create_part")
    mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
    mcp.call("extrude_sketch", depth_mm=10)
    return top_and_ends()


def top_and_ends():
    fs = faces()
    top = next(f["id"] for f in fs if abs(f["area_mm2"] - 800) < 1e-6)
    ends = [f["id"] for f in fs if abs(f["area_mm2"] - 200) < 1e-6]
    assert len(ends) == 2, fs
    return top, ends


def two_blocks():
    """Two separate boxes, 100 mm apart along the Right plane's normal."""
    mcp.call("create_part")
    planes = {f["name"]: f for f in mcp.call("inspect_model", what="features").json["features"]}
    right = planes["Right Plane"]["normal"]
    mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
    mcp.call("extrude_sketch", depth_mm=10)
    mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20, center_mm=[100 * c for c in right])
    mcp.call("extrude_sketch", depth_mm=10)
    assert len(bodies()) == 2


def no_dialogs(when):
    """No dialog stays open: the first shell after a start can flash an untitled one that closes by itself."""
    for _ in range(5):
        p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(SW), "dialogs"],
                           capture_output=True, text=True, timeout=60)
        if p.stdout.strip() == "":
            return
        print(f"    {when}: dialog open: {p.stdout.strip()}")
        time.sleep(2)
    assert False, f"{when}: dialogs stay open: {p.stdout.strip()}"


def refused(expected_text, **args):
    """shell_body must fail, leaving the tree and the body as they were."""
    tree, vs = features(), volumes()
    r = mcp.call("shell_body", expect_error=True, **args)
    assert r.is_error and expected_text in r.text, r.text
    assert features() == tree, (features(), tree)
    assert volumes() == vs, (volumes(), vs)
    return r


# ---- Open at one face: a tray ------------------------------------------------

top, ends = block()
r = mcp.call("shell_body", thickness_mm=2, open_faces=[top])
assert r.text.startswith(f"created Shell1: 2 mm walls inside the body, open at {top}."), r.text
no_dialogs("after the first shell")
check_volumes([BOX - 36 * 16 * 8], "tray")
check_box([40, 20, 10], "tray")
assert "Shell1" in features()

# delete_features takes it out again.
mcp.call("delete_features", features=["Shell1"])
check_volumes([BOX], "shell deleted")

# Walls half the plate's thickness still work when open at one face.
top, ends = top_and_ends()
mcp.call("shell_body", thickness_mm=5, open_faces=[top])
check_volumes([BOX - 30 * 10 * 5], "5 mm walls")

# ---- Two openings ------------------------------------------------------------

top, ends = block()
mcp.call("shell_body", thickness_mm=2, open_faces=[top, ends[0], top])
check_volumes([BOX - 38 * 16 * 8], "open at two faces")

# ---- Closed all round ----------------------------------------------------------

block()
r = mcp.call("shell_body", thickness_mm=2)
assert "closed all round, hollow inside" in r.text, r.text
check_volumes([BOX - 36 * 16 * 6], "closed")
check_box([40, 20, 10], "closed")
assert len(faces()) == 12, faces()

# ---- Outward: the box's faces become the inside --------------------------------

top, ends = block()
r = mcp.call("shell_body", thickness_mm=2, open_faces=[top], outward=True)
assert "2 mm walls outside the body" in r.text, r.text
check_volumes([44 * 24 * 12 - BOX], "outward, open")
check_box([44, 24, 12], "outward, open")

block()
mcp.call("shell_body", thickness_mm=2, outward=True)
check_volumes([44 * 24 * 14 - BOX], "outward, closed")
check_box([44, 24, 14], "outward, closed")

# ---- Walls of their own thickness ---------------------------------------------

top, ends = block()
r = mcp.call("shell_body", thickness_mm=2, open_faces=[top],
             face_thicknesses=[{"face": ends[0], "thickness_mm": 5}, {"face": ends[1], "thickness_mm": 3}])
assert "(5 mm at" in r.text and "3 mm at" in r.text, r.text
check_volumes([BOX - 8 * 32 * 16], "two thicker ends")

# One thicker wall, thinner than the rest, closed, and outward.
top, ends = block()
mcp.call("shell_body", thickness_mm=3, open_faces=[top], face_thicknesses=[{"face": ends[0], "thickness_mm": 1}])
check_volumes([BOX - 7 * 36 * 14], "one thinner end")

top, ends = block()
mcp.call("shell_body", thickness_mm=2, face_thicknesses=[{"face": ends[0], "thickness_mm": 5}])
check_volumes([BOX - 6 * 33 * 16], "closed with a thicker end")

top, ends = block()
mcp.call("shell_body", thickness_mm=2, open_faces=[top], outward=True,
         face_thicknesses=[{"face": ends[0], "thickness_mm": 5}])
check_volumes([12 * 47 * 24 - BOX], "outward with a thicker end")
check_box([47, 24, 12], "outward with a thicker end")

# ---- Refusals leave nothing behind ---------------------------------------------

top, ends = block()
refused("thick", thickness_mm=15, open_faces=[top])
refused("thick", thickness_mm=6)
# Walls that exactly fill the body: SolidWorks makes a Shell1 that does nothing.
refused("changed nothing", thickness_mm=5)
refused("thick", thickness_mm=2, open_faces=[top], face_thicknesses=[{"face": ends[0], "thickness_mm": 45}])
edge =mcp.call("inspect_model", what="edges", limit=1).json["edges"][0]["id"]
[box_body] = bodies()
refused("a face id", thickness_mm=2, open_faces=[edge])
refused("a face id", thickness_mm=2, open_faces=[box_body["id"]])
refused("a face id", thickness_mm=2, open_faces=["top"])
refused("more than 0", thickness_mm=0, open_faces=[top])
refused("more than 0", thickness_mm=2, face_thicknesses=[{"face": ends[0], "thickness_mm": -1}])
refused("both", thickness_mm=2, open_faces=[top], face_thicknesses=[{"face": top, "thickness_mm": 5}])
refused("more than once", thickness_mm=2,
        face_thicknesses=[{"face": ends[0], "thickness_mm": 5}, {"face": ends[0], "thickness_mm": 3}])
refused("closed shell", thickness_mm=2, open_faces=[top], body=box_body["id"])
refused("a body id", thickness_mm=2, body=top)
no_dialogs("after the refusals")

# The refused 45 mm wall's thickness doesn't reach the next shell.
mcp.call("shell_body", thickness_mm=2, open_faces=[top])
check_volumes([BOX - 36 * 16 * 8], "a plain shell after the refusals")

# A face the shell removed is gone.
refused("", thickness_mm=1, open_faces=[top])

# ---- A fillet carries through to the inside (r 3 outside, r 1 inside) ------------

top, ends = block()
short = next(e for e in mcp.call("inspect_model", what="edges", limit=100).json["edges"]
             if e["type"] == "line" and abs(e["length_mm"] - 10) < 1e-6)
mcp.call("fillet_edges", edges=[short["id"]], radius_mm=3)
top = next(f["id"] for f in faces() if f["area_mm2"] > 700)  # the fillet changed its id and area


def corner(r):
    """Cross-section a radius r takes from a right-angle corner."""
    return r * r * (1 - math.pi / 4)


mcp.call("shell_body", thickness_mm=2, open_faces=[top])
outer = BOX - corner(3) * 10
check_volumes([outer - (36 * 16 * 8 - corner(1) * 8)], "filleted box")

# ---- A disc: flat and curved walls ---------------------------------------------

mcp.call("create_part")
mcp.call("sketch_circle", plane="front", center=[0, 0], radius_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
fs = faces()
flat = next(f["id"] for f in fs if f["type"] == "plane")
rim = next(f["id"] for f in fs if f["type"] == "cylinder")
disc = math.pi * 20 ** 2 * 10
mcp.call("shell_body", thickness_mm=2, open_faces=[flat], face_thicknesses=[{"face": rim, "thickness_mm": 4}])
check_volumes([disc - math.pi * 16 ** 2 * 8], "cup")

# ---- One body of two -------------------------------------------------------------

two_blocks()
refused("solid bodies", thickness_mm=2)
r = mcp.call("shell_body", thickness_mm=2, body=bodies()[0]["id"])
assert "closed all round" in r.text, r.text
check_volumes([BOX - 36 * 16 * 6, BOX], "closed, one body named")

two_blocks()
big = [f["id"] for f in faces() if abs(f["area_mm2"] - 800) < 1e-6]
mcp.call("shell_body", thickness_mm=2, open_faces=[big[0]])
check_volumes([BOX - 36 * 16 * 8, BOX], "open, one of two bodies")

two_blocks()
one, other = ({"big": next(f["id"] for f in fs if abs(f["area_mm2"] - 800) < 1e-6),
               "end": next(f["id"] for f in fs if abs(f["area_mm2"] - 200) < 1e-6)}
              for fs in (mcp.call("inspect_model", what="faces", within=b["id"], limit=20).json["faces"]
                         for b in bodies()))
refused("more than one body", thickness_mm=2, open_faces=[one["big"], other["big"]])
refused("more than one body", thickness_mm=2, open_faces=[one["big"]],
        face_thicknesses=[{"face": other["end"], "thickness_mm": 5}])
# Closed with a wall of its own: SolidWorks makes nothing, or with the body
# selected the wrong walls.
refused("several solid bodies", thickness_mm=2, face_thicknesses=[{"face": one["end"], "thickness_mm": 5}])
refused("closed shell", thickness_mm=2, body=bodies()[0]["id"],
        face_thicknesses=[{"face": one["end"], "thickness_mm": 5}])
mcp.call("shell_body", thickness_mm=2, open_faces=[one["big"]],
         face_thicknesses=[{"face": one["end"], "thickness_mm": 5}])
check_volumes([BOX - 8 * 33 * 16, BOX], "open with a thicker wall, one of two bodies")

no_dialogs("at the end")

# ---- A picture of the last solid tray -------------------------------------------

top, ends = block()
mcp.call("shell_body", thickness_mm=2, open_faces=[top])
mcp.call("render_views")

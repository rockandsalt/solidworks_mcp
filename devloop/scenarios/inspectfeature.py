# inspect_feature against real SolidWorks: one feature of each kind it reads,
# its definition checked against what made it, with ids of the edges / faces it
# uses; reading changes nothing and leaves nothing rolled back
# (docs/notes/inspection-gaps.md).
# Run with:
#   py devloop/mcp_client.py run devloop/scenarios/inspectfeature.py

import json
import math
import re
import subprocess
from pathlib import Path

SW = Path(__file__).resolve().parent.parent / "sw.ps1"


def add(*vs): return [sum(c) for c in zip(*vs)]
def scale(a, k): return [x * k for x in a]
def dot(a, b): return sum(x * y for x, y in zip(a, b))


def inspect(what, **kwargs):
    return mcp.call("inspect_model", what=what, **kwargs).json.get(what, [])


def volume():
    return sum(b["volume_mm3"] for b in inspect("bodies"))


def feature(name):
    """inspect_feature's JSON, checking that reading changed nothing."""
    v = volume()
    j = mcp.call("inspect_feature", feature=name).json
    print(f"    {name}: {json.dumps(j)}")
    assert abs(volume() - v) < 1e-6, (name, volume(), v)
    rolled = [f["name"] for f in inspect("features") if f.get("rolled_back")]
    assert not rolled, (name, rolled)
    return j


def sketch_name(r):
    return re.search(r"Sketch\d+", r.text).group(0)


def created(r):
    return re.match(r"created (.+?)(?:: | \(| by )", r.text).group(1)


def axes():
    global u, v, w
    planes = {f["name"]: f for f in inspect("features")}
    u, v, w = (planes[n]["normal"] for n in ("Front Plane", "Right Plane", "Top Plane"))


def at(a=0.0, b=0.0, c=0.0):
    """Model point a*u + b*v + c*w, in mm."""
    return add(scale(u, a), scale(v, b), scale(w, c))


def square(a, size, **target):
    h = size / 2
    return sketch_name(mcp.call("sketch_line", closed=True, points=[
        at(a, -h, -h), at(a, h, -h), at(a, h, h), at(a, -h, h)], **target))


def dims(j):
    """Name (without the "@Part1.Part" SolidWorks appends) -> value."""
    return {"@".join(d["name"].split("@")[:2]): d.get("value_mm", d.get("value_deg", d.get("value"))) for d in j["dimensions"]}


# ---- A plate: extrude, cut, mirror, pattern, fillet, chamfers ----------------
# 60 x 40 on Front, 10 deep along +x (this template's planes, see devloop.md).

mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=60, height_mm=40)
mcp.call("extrude_sketch", depth_mm=10)
j = feature("Boss-Extrude1")
assert j["type"] == "boss-extrude" and j["type_name"] == "Extrusion", j
assert j["definition"]["direction1"] == {"end_condition": "blind", "depth_mm": 10.0}, j
assert j["definition"]["sketch"] == "Sketch1" and "cut" not in j["definition"], j
assert dims(j) == {"D1@Boss-Extrude1": 10.0}, j
assert "edit_extrude" in j["note"] and "set_dimension" in j["note"], j

r = mcp.call("sketch_circle", center=[10, 5], radius_mm=2)
r = mcp.call("cut_extrude_sketch", sketch_name=sketch_name(r), through_all=True)
cut = re.search(r"Cut-Extrude\d+", r.text).group(0)
j = feature(cut)
assert j["definition"]["cut"] is True and j["definition"]["direction1"]["end_condition"] == "through_all", j

r = mcp.call("mirror_features", plane="right", features=[cut])
j = feature(created(r))
assert j["type"] == "mirror" and j["definition"]["features"] == [cut], j
assert j["definition"]["plane"] == "Right Plane", j

r = mcp.call("linear_pattern_features", features=[cut], direction="right", count=2, spacing_mm=8)
j = feature(created(r))
d = j["definition"]
assert d["kind"] == "linear" and d["features"] == [cut], j
assert d["direction1"]["count"] == 2 and d["direction1"]["spacing_mm"] == 8.0, j
assert d["direction1"]["along"] == "Right Plane" and "direction2" not in d, j
assert "edit_pattern" in j["note"], j
assert dims(j) == {"D1@LPattern1": 2, "D3@LPattern1": 8.0}, j  # the count is a number, not mm


def corner_edges():
    """The plate's 10 mm corner edges still there: at y = +-30, z = +-20."""
    return [e["id"] for e in inspect("edges", curve_type="line", limit=500)
            if abs(e["length_mm"] - 10) < 1e-6
            and abs(abs(e["start_mm"][1]) - 30) < 1e-6 and abs(abs(e["start_mm"][2]) - 20) < 1e-6]


edge = corner_edges()[0]
mcp.call("fillet_edges", edges=[edge], radius_mm=3)
j = feature("Fillet1")
d = j["definition"]
assert d["kind"] == "constant-radius fillet" and d["radius_mm"] == 3.0, j
# Read as the model is before the fillet: the edge it rounds, by its old id.
assert d["edges"] == [edge], (d, edge)
assert dims(j) == {"D1@Fillet1": 3.0}, j
assert "rollback before" in j["note"] and "edit_fillet" in j["note"], j

edge = corner_edges()[0]
mcp.call("chamfer_edges", edges=[edge], distance_mm=2)
j = feature("Chamfer1")
d = j["definition"]
# Equal distance is made as angle-distance at 45 degrees (architecture.md).
assert d["kind"] == "angle-distance" and d["distance_mm"] == 2.0 and d["angle_deg"] == 45.0, j
assert d["edges"] == [edge], (d, edge)

edge = corner_edges()[0]
mcp.call("chamfer_edges", edges=[edge], distance_mm=2, distance2_mm=3)
j = feature("Chamfer2")
d = j["definition"]
assert d["kind"] == "distance-distance" and sorted([d["distance_mm"], d["distance2_mm"]]) == [2.0, 3.0], j

# Kinds it doesn't read: dimensions only.
j = feature("Front Plane")
assert "definition" not in j and "isn't read" in j["note"], j
for bad in ["NoSuchFeature", edge]:
    r = mcp.call("inspect_feature", feature=bad, expect_error=True)
    assert r.is_error, r.text

# ---- A tray with a web: shell and rib ----------------------------------------

mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
open_face = next(f["id"] for f in inspect("faces", limit=500) if abs(f["area_mm2"] - 800) < 1e-6 and f["normal"][0] > 0)
mcp.call("shell_body", thickness_mm=2, open_faces=[open_face])
j = feature("Shell1")
d = j["definition"]
assert d["thickness_mm"] == 2.0 and d["removed_faces"] == [open_face] and "outward" not in d, (d, open_face)

web = sketch_name(mcp.call("sketch_line", plane="top", points=[[6, -5, 0], [6, 5, 0]]))
r = mcp.call("rib_sketch", thickness_mm=2)
rib = re.search(r"Rib\d+", r.text).group(0)
j = feature(rib)
d = j["definition"]
assert d["thickness_mm"] == 2.0 and d["side"] == "both" and d["sketch"] == web, j  # not its hidden copy "Sketch2<2>"

# ---- A block with a draft ------------------------------------------------------

mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
fs = inspect("faces", limit=500)
ends = [f["id"] for f in fs if abs(f["area_mm2"] - 200) < 1e-6]
flats = [f["id"] for f in fs if abs(f["area_mm2"] - 800) < 1e-6]
mcp.call("draft_faces", angle_deg=5, faces=[ends[0]], neutral_plane=flats[0])
j = feature("Draft1")
d = j["definition"]
assert d["kind"] == "neutral plane" and d["angle_deg"] == 5.0 and d["propagation"] == "none", j
assert d["faces"] == [ends[0]] and d["neutral_plane"] == flats[0], (d, ends, flats)

# ---- A stepped shaft: revolve --------------------------------------------------

mcp.call("create_part")
r = mcp.call("sketch_line", points=[[0, 0], [40, 0], [40, 10], [25, 10], [25, 15], [0, 15]], closed=True)
shaft = sketch_name(r)
mcp.call("sketch_line", sketch_name=shaft, points=[[-10, 0], [50, 0]], construction=True)
r = mcp.call("revolve_sketch")
j = feature(re.search(r"Revolve\d+", r.text).group(0))
d = j["definition"]
assert j["type"] == "revolve" and d["sketch"] == shaft and d["angle_deg"] == 360.0, j
assert d["type"] == "one direction" and d["axis"] != "nothing", j

# ---- A swept bar ------------------------------------------------------------------

mcp.call("create_part")
axes()
path = sketch_name(mcp.call("sketch_line", plane="top", points=[at(0, 0), at(100, 0)]))
profile = square(0, 10, plane="front")
r = mcp.call("sweep_sketch", path=path)
j = feature(created(r))
d = j["definition"]
assert j["type"] == "sweep" and d == {"profile": profile, "path": path}, j
assert "edit_sweep" in j["note"], j

# ---- A lofted frustum ---------------------------------------------------------------

mcp.call("create_part")
axes()
r = mcp.call("create_reference_plane", references=[{"entity": "front", "offset_mm": 30}])
[plane] = json.loads(r.text[r.text.rindex("\n[") + 1:])
offset = dot(plane["point_on_plane_mm"], u)
base = square(0, 20, plane="front")
top = square(offset, 10, on=plane["id"])
r = mcp.call("loft_sketch", profiles=[base, top])
j = feature(created(r))
d = j["definition"]
assert d["profiles"] == [base, top] and d["start_tangency"] == "none" and d["end_tangency"] == "none", j
assert "cut" not in d and "guide_curves" not in d, j
assert j["type"] == "loft" and j["dimensions"] == [], j  # not Plane1's offset

# ---- A helix ------------------------------------------------------------------------

mcp.call("create_part")
axes()
circle = sketch_name(mcp.call("sketch_circle", plane="top", center=at(), radius_mm=10))
r = mcp.call("create_helix", pitch_mm=5, revolutions=4, sketch_name=circle)
j = feature(created(r))
d = j["definition"]
assert j["type"] == "helix" and d["defined_by"] == "pitch and revolutions", j
assert d["pitch_mm"] == 5.0 and d["revolutions"] == 4.0 and d["height_mm"] == 20.0, j
assert 4 in dims(j).values(), j  # the turns

# ---- A Hole Wizard counterbore ------------------------------------------------------

mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=60, height_mm=40)
mcp.call("extrude_sketch", depth_mm=10)
far = next(f["id"] for f in inspect("faces", surface_type="plane")
           if abs(f["normal"][0]) > 0.99 and abs(f["point_on_plane_mm"][0] - 10) < 1e-6)
r = mcp.call("hole_wizard", kind="counterbore", size="M4", on=far, through_all=True)
j = feature(created(r))
d = j["definition"]
assert j["type"] == "hole" and d["size"] == "M4" and d["through_all"] is True, j
assert d["fastener"].startswith("ISO") and d["counterbore_diameter_mm"] > d["diameter_mm"] > 4, j

p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(SW), "dialogs"],
                   capture_output=True, text=True, timeout=60)
assert p.stdout.strip() == "", f"dialogs open: {p.stdout.strip()}"

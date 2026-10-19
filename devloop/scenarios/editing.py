# edit_extrude / edit_mate against real SolidWorks: that each kind of change
# lands in the geometry (body boxes, volumes, component positions), and that
# a change SolidWorks rejects or can't solve leaves the feature as it was
# (docs/notes/editing.md). The assembly is made through COM as in
# deleting.py. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/editing.py

import math
import re
import subprocess
from pathlib import Path

SWCOM = Path(__file__).resolve().parent.parent / "swcom.ps1"


def com(code: str) -> str:
    """Runs PowerShell `code` against SolidWorks through swcom.ps1 (see there)."""
    script = mcp.run_dir / f"com-{len(list(mcp.run_dir.glob('com-*.ps1'))) + 1}.ps1"
    script.write_text(code, encoding="utf-8")
    p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(SWCOM),
                        "-CodeFile", str(script)], capture_output=True, text=True, timeout=150)
    print(f"    com {script.name}: {p.stdout.strip()}")
    assert p.returncode == 0, f"{script.name} failed: {p.stderr.strip()}"
    return p.stdout.strip()


def sw_dialogs() -> list[str]:
    p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
                        str(SWCOM.parent / "sw.ps1"), "dialogs"], capture_output=True, text=True, timeout=60)
    return [line.strip() for line in p.stdout.splitlines() if line.strip()]


def body():
    bodies = mcp.call("inspect_model", what="bodies").json["bodies"]
    assert len(bodies) == 1, bodies
    return bodies[0]


def x_range():
    b = body()["bbox_mm"]
    return round(b["min"][0], 3), round(b["max"][0], 3)


def volume():
    return body()["volume_mm3"]


def close(a, b, tol=1e-3):
    return abs(a - b) <= tol * max(1.0, abs(b))


# ---- Extrudes ------------------------------------------------------------------
# 40 x 20 box, 10 deep. With this machine's template the Front Plane faces +X
# (docs/notes/devloop.md), so the extrude's depth is the box's X extent.
mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
print("    body:", body())
assert x_range() == (0, 10), x_range()

# Only `feature`: reports, changes nothing.
r = mcp.call("edit_extrude", feature="Boss-Extrude1")
print("    report:", r.text)
assert r.text.startswith("Boss-Extrude1 (boss-extrude of Sketch1): blind 10 mm."), r.text
assert "D1@Boss-Extrude1" in r.text and "Nothing was changed" in r.text, r.text

r = mcp.call("edit_extrude", feature="Boss-Extrude1", depth_mm=25)
print("    depth:", r.text)
assert "was blind 10 mm; now blind 25 mm." in r.text, r.text
assert x_range() == (0, 25), x_range()

r = mcp.call("edit_extrude", feature="Boss-Extrude1", end_condition="mid_plane", depth_mm=30)
assert "now mid plane 30 mm in total." in r.text, r.text
assert x_range() == (-15, 15), x_range()

r = mcp.call("edit_extrude", feature="Boss-Extrude1", end_condition="blind", depth_mm=10, reverse_direction=True)
assert "now blind 10 mm, reversed." in r.text, r.text
assert x_range() == (-10, 0), x_range()

# A second direction, by id this time.
boss = [f["id"] for f in mcp.call("inspect_model", what="features").json["features"] if f["name"] == "Boss-Extrude1"][0]
r = mcp.call("edit_extrude", feature=boss, both_directions=True, depth2_mm=4)
print("    both:", r.text)
assert "second direction blind 4 mm" in r.text, r.text
assert x_range() == (-10, 4), x_range()
r = mcp.call("edit_extrude", feature=boss, both_directions=False, reverse_direction=False)
assert x_range() == (0, 10), x_range()
full = volume()
assert close(full, 40 * 20 * 10), full

# Draft makes the box smaller (inward taper), and 0 takes it off again.
r = mcp.call("edit_extrude", feature=boss, draft_deg=5)
assert "draft 5 deg inward" in r.text, r.text
assert volume() < full - 1, volume()
mcp.call("edit_extrude", feature=boss, draft_deg=0)
assert close(volume(), full), volume()

# A cut on the +X cap: blind 3 mm, then through all.
faces = mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]
cap = [f["id"] for f in faces if abs(f["normal"][0] - 1) < 1e-6]
assert len(cap) == 1, faces
mcp.call("sketch_circle", on=cap[0], center=[10, 0, 0], diameter_mm=6)
mcp.call("cut_extrude_sketch", depth_mm=3)
assert close(volume(), full - math.pi * 9 * 3), volume()
r = mcp.call("edit_extrude", feature="Cut-Extrude1", end_condition="through_all")
print("    cut:", r.text)
assert "(cut-extrude of Sketch2): was blind 3 mm; now through all." in r.text, r.text
assert close(volume(), full - math.pi * 9 * 10), volume()

# Reversed, the cut points out of the part into nothing: rejected, unchanged.
r = mcp.call("edit_extrude", feature="Cut-Extrude1", reverse_direction=True, expect_error=True)
print("    reversed cut:", r.text)
assert "unchanged" in r.text or "put back" in r.text, r.text
assert mcp.call("edit_extrude", feature="Cut-Extrude1").text.startswith(
    "Cut-Extrude1 (cut-extrude of Sketch2): through all."), "the rejected edit changed the cut"
assert close(volume(), full - math.pi * 9 * 10), volume()

# Keep the material outside the circle instead: a 6 mm rod is left.
r = mcp.call("edit_extrude", feature="Cut-Extrude1", flip_side_to_cut=True)
assert "flipped side to cut" in r.text, r.text
assert close(volume(), math.pi * 9 * 10), volume()
mcp.call("edit_extrude", feature="Cut-Extrude1", flip_side_to_cut=False)

# Wrong targets and contradictions.
r = mcp.call("edit_extrude", feature="Sketch1", depth_mm=5, expect_error=True)
assert "not a boss- or cut-extrude" in r.text, r.text
r = mcp.call("edit_extrude", feature="Boss-Extrude1", flip_side_to_cut=True, expect_error=True)
assert "only applies to cuts" in r.text, r.text
r = mcp.call("edit_extrude", feature="Cut-Extrude1", depth_mm=5, expect_error=True)
assert "end at a depth" in r.text, r.text
r = mcp.call("edit_extrude", feature="Nope1", depth_mm=5, expect_error=True)
assert "no feature named" in r.text, r.text


# ---- Mates ---------------------------------------------------------------------
# Two 30 x 30 x 10 blocks and an assembly plane 50 mm off the assembly's Front
# Plane, as in deleting.py.
mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=30, height_mm=30)
mcp.call("extrude_sketch", depth_mm=10)
block = mcp.run_dir / "edit-block.SLDPRT"
plane_name = com(f"""$d = Com $sw ActiveDoc -Get
$e = Com $d SaveAs3 @('{block}', 0, 1); if ($e -ne 0) {{ throw "SaveAs3: $e" }}
$template = Com $sw GetUserPreferenceStringValue @(9)  # swDefaultTemplateAssembly
$a = Com $sw NewDocument @($template, 0, 0, 0); if (-not $a) {{ throw "NewDocument($template) failed" }}
foreach ($x in 0.0, 0.08) {{
    if (-not (Com $a AddComponent5 @('{block}', 0, '', $false, '', $x, 0.0, 0.0))) {{ throw 'AddComponent5 failed' }}
}}
$none = [Runtime.InteropServices.DispatchWrapper]::new([object]$null)
if (-not (Com (Com $a Extension -Get) SelectByID2 @('Front Plane', 'PLANE', 0.0, 0.0, 0.0, $false, 0, $none, 0))) {{ throw 'no Front Plane' }}
$p = Com (Com $a FeatureManager -Get) InsertRefPlane @(8, 0.05, 0, 0.0, 0, 0.0)  # swRefPlaneReferenceConstraint_Distance
if (-not $p) {{ throw 'InsertRefPlane failed' }}
Com $p Name -Get""").splitlines()[-1]

overview = mcp.call("inspect_model").json
components = [c["id"] for c in sorted(overview["components"], key=lambda c: not c["fixed"])]
assert len(components) == 2, overview
plane1 = [f["id"] for f in overview["features"] if f["name"] == plane_name][0]
planes = [{f["name"]: f["id"] for f in mcp.call("inspect_model", what="features", within=c).json["features"]}
          for c in components]
mcp.call("add_mate", kind="coincident", entities=[plane1, planes[1]["Front Plane"]])  # Coincident1
mcp.call("add_mate", kind="coincident", entities=[planes[0]["Top Plane"], planes[1]["Top Plane"]])  # Coincident2
mcp.call("add_mate", kind="parallel", entities=[planes[0]["Right Plane"], planes[1]["Right Plane"]])  # Parallel1


def placed():
    """Component id -> (origin, x axis, y axis, z axis), assembly mm."""
    comps = mcp.call("inspect_model", what="components").json["components"]
    print("    components:", comps)
    return {c["id"]: c for c in comps}


def mates():
    return {m["name"]: m for m in mcp.call("inspect_model", what="mates").json["mates"]}


def origin(c):
    return placed()[c]["origin_mm"]


print("    mates:", mates())

def edited(r, old):
    """The mate's name and id after an edit_mate reply (a new kind renames it)."""
    m = re.match(r"(?:edited (\S+) \(now (feature:\d+)\)|replaced \S+ with (\S+) \((feature:\d+);)", r.text)
    assert m, r.text
    name, id_ = (m.group(1), m.group(2)) if m.group(1) else (m.group(3), m.group(4))
    assert name == old or not r.text.startswith("edited"), r.text
    return name, id_


# The plane mate becomes a 20 mm distance: a new mate named after its type
# replaces it, and the moving block shifts along the Front Plane's normal
# (X here).
x_before = origin(components[1])[0]
r = mcp.call("edit_mate", mate="Coincident1", kind="distance", distance_mm=20)
print("    distance:", r.text)
assert r.text.startswith("replaced Coincident1 with Distance"), r.text
assert "was coincident" in r.text and "now distance" in r.text and "20 mm" in r.text, r.text
plane_mate, plane_mate_id = edited(r, "Coincident1")
assert "Coincident1" not in mates() and plane_mate in mates(), mates()
assert close(abs(origin(components[1])[0] - x_before), 20), origin(components[1])
# Just the value, by the id the reply gave: same mate, same name.
r = mcp.call("edit_mate", mate=plane_mate_id, distance_mm=35)
print("    value:", r.text)
assert r.text.startswith(f"edited {plane_mate} ") and "now distance" in r.text and "35 mm" in r.text, r.text
plane_mate, plane_mate_id = edited(r, plane_mate)
assert close(abs(origin(components[1])[0] - x_before), 35), origin(components[1])
# And back to coincident.
r = mcp.call("edit_mate", mate=plane_mate_id, kind="coincident")
assert "now coincident" in r.text, r.text
plane_mate, plane_mate_id = edited(r, plane_mate)
assert close(origin(components[1])[0], x_before), origin(components[1])


# The parallel mate becomes a distance mate between two faces: the block's
# -Y side 0 mm from the fixed block's +Y side, facing it (anti-aligned; the
# parallel mate's "aligned" would be kept otherwise), so they sit side by side.
def side_face(c, ny):
    faces = mcp.call("inspect_model", what="faces", within=c, surface_type="plane").json["faces"]
    hits = [f["id"] for f in faces if abs(f["normal"][1] - ny) < 1e-6]
    assert len(hits) == 1, faces
    return hits[0]


r = mcp.call("edit_mate", mate="Parallel1", kind="distance", distance_mm=0, alignment="anti_aligned",
             entities=[side_face(components[0], 1), side_face(components[1], -1)])
print("    entities:", r.text)
assert "now distance" in r.text and "face:" in r.text, r.text
side_mate, _ = edited(r, "Parallel1")
y = [origin(c)[1] for c in components]
assert close(abs(y[1] - y[0]), 30), y

# Same kind, new entities: the distance between the Right Planes instead.
# Keeping "anti_aligned" would turn the block against the other mates:
# unsolvable, so the old entities are put back and nothing moves.
moved_to = origin(components[1])
r = mcp.call("edit_mate", mate=side_mate, entities=[planes[0]["Right Plane"], planes[1]["Right Plane"]],
             expect_error=True)
print("    unsolvable:", r.text)
assert "put back" in r.text, r.text
m = mates()[side_mate]
assert m["type"] == "distance" and "error" not in m, m
assert origin(components[1]) == moved_to, origin(components[1])
# Aligned, 0 mm puts the blocks in line again.
r = mcp.call("edit_mate", mate=side_mate, entities=[planes[0]["Right Plane"], planes[1]["Right Plane"]],
             alignment="aligned")
print("    same kind, new entities:", r.text)
side_mate, _ = edited(r, side_mate)
assert "feature:" in r.text, r.text
assert close(origin(components[1])[1], y[0]), [origin(c) for c in components]

# A coincident between the two blocks' Front Planes contradicts the plane
# mate (the block's Front Plane is on PLANE1, 50 mm away): undone.
before = mates()[side_mate]
moved_to = origin(components[1])
r = mcp.call("edit_mate", mate=side_mate, kind="coincident",
             entities=[planes[0]["Front Plane"], planes[1]["Front Plane"]], expect_error=True)
print("    over-defined:", r.text)
assert "is unchanged" in r.text, r.text
after = mates()
assert side_mate in after and after[side_mate]["type"] == before["type"], (before, after)
assert not [m for m in after.values() if "error" in m or m.get("suppressed")], after
assert origin(components[1]) == moved_to, "the undone edit left the block moved"

# Turning the block over: the other alignment of the top planes. With the
# front, top and right planes all mated aligned that can't be solved (put
# back, nothing moves); without the side mate it can.
m = mates()["Coincident2"]
flipped = "aligned" if m.get("alignment") == "anti_aligned" else "anti_aligned"
axes_before = placed()[components[1]].get("axes")
r = mcp.call("edit_mate", mate="Coincident2", alignment=flipped, expect_error=True)
assert "put back" in r.text, r.text
assert mates()["Coincident2"]["alignment"] == m["alignment"], mates()["Coincident2"]
assert placed()[components[1]].get("axes") == axes_before
mcp.call("delete_features", features=[side_mate])
r = mcp.call("edit_mate", mate="Coincident2", alignment=flipped)
print("    alignment:", r.text)
edited(r, "Coincident2")
assert mates()["Coincident2"]["alignment"] == flipped, mates()["Coincident2"]
axes_after = placed()[components[1]].get("axes")
print("    axes:", axes_before, "->", axes_after)
assert axes_after != axes_before, "the block didn't turn over"

# Lock needs components; nothing at all, or a non-mate, is refused.
r = mcp.call("edit_mate", mate=plane_mate, kind="lock", expect_error=True)
assert "two components" in r.text, r.text
r = mcp.call("edit_mate", mate=plane_mate, expect_error=True)
assert "nothing to change" in r.text, r.text
r = mcp.call("edit_mate", mate=plane1, distance_mm=3, expect_error=True)
assert "not a mate" in r.text, r.text

problems = mcp.call("diagnose_model").json.get("problems", [])
assert not problems, problems
assert not (open_dialogs := sw_dialogs()), open_dialogs

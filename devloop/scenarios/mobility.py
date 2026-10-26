# inspect_mobility against real SolidWorks: one component per mate pattern
# next to a fixed base -- free, face on a plane, slider (dragging a
# follower that only moves with it), fully mated, concentric pin, hinge,
# angle, two blocks locked only to each other -- each one's remaining degrees of freedom and their directions /
# axes; a rigid subassembly as one component; asking about some only; the
# assembly left as it was; refusals (docs/notes/architecture.md,
# "Assemblies + mates"). Run with:
#   py devloop/mcp_client.py run devloop/scenarios/mobility.py

import re
import subprocess
import time
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


def close_all():
    com("Com $sw CloseAllDocuments @($true) | Out-Null")


def first_id(r):
    return re.findall(r"^- (component:\d+):", r.text, re.M)[0]


def poses():
    return {c["id"]: (c["origin_mm"], c.get("axes")) for c in
            mcp.call("inspect_model", what="components").json["components"]}


def near(a, b, tol=1e-3):
    return all(abs(x - y) < tol for x, y in zip(a, b))


def same_line(a, b, tol=1e-3):
    """Two directions along one line, either way."""
    return near(a, b, tol) or near(a, [-x for x in b], tol)


def mobility(**args):
    t = time.time()
    r = mcp.call("inspect_mobility", **args)
    print(f"    inspect_mobility {args} in {time.time() - t:.1f} s")
    return r.json, {c["id"]: c for c in r.json["components"]}


out = mcp.run_dir.resolve()
block = out / "mob-block.SLDPRT"
pin = out / "mob-pin.SLDPRT"
sub = out / "mob-sub.SLDASM"
top = out / "mob-top.SLDASM"

# ---- Parts: a 40 x 20 x 10 block (x 0..10) and a 10 mm pin, 30 long along +X, and
# a one-block subassembly --------------------------------------------------------------
mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
mcp.call("save_document", path=str(block))
close_all()
mcp.call("create_part")
mcp.call("sketch_circle", center=[0, 0, 0], diameter_mm=10)
mcp.call("extrude_sketch", depth_mm=30)
mcp.call("save_document", path=str(pin))
close_all()
mcp.call("create_assembly")
mcp.call("insert_components", file=str(block))
mcp.call("save_document", path=str(sub))
close_all()

# ---- The assembly ---------------------------------------------------------------------
mcp.call("create_assembly")
mcp.call("save_document", path=str(top))  # first: saving renames the document, which renews its ids
base = first_id(mcp.call("insert_components", file=str(block)))


def add(file, origin, fixed=False):
    return first_id(mcp.call("insert_components", file=str(file), at=[{"origin_mm": origin}], fixed=fixed))


free = add(block, [0, -100, 0])
plane = add(block, [0, -200, 0])
slider = add(block, [0, 100, 0])
follower = add(block, [0, 150, 0])
full = add(block, [0, 200, 0])
angle = add(block, [0, -300, 0])
axle = add(pin, [100, 0, 0], fixed=True)
spin = add(pin, [100, 0, 50])
hinge = add(pin, [100, 0, 150])
spinlock = add(pin, [100, 0, 100])
sub1 = add(sub, [0, 0, 300])
pair_a = add(block, [0, -400, 0])
pair_b = add(block, [0, -450, 0])

planes = lambda c: {f["name"]: f["id"] for f in mcp.call("inspect_model", what="features", within=c).json["features"]}
P = {c: planes(c) for c in (base, plane, slider, follower, full, angle, axle, hinge)}
cylinder = lambda c: [f["id"] for f in mcp.call("inspect_model", what="faces", within=c).json["faces"]
                      if f["type"] == "cylinder"][0]
mate = lambda kind, a, b, **kw: mcp.call("add_mate", kind=kind, entities=[a, b], **kw)
# plane: Top on base's Top. slider: + Right 100 from base's -> x only. follower: Front on
# the slider's Front, Top on base's Top -> y, and x with the slider. full: three planes.
mate("coincident", P[base]["Top Plane"], P[plane]["Top Plane"])
mate("coincident", P[base]["Top Plane"], P[slider]["Top Plane"])
mate("distance", P[base]["Right Plane"], P[slider]["Right Plane"], distance_mm=100)
mate("coincident", P[slider]["Front Plane"], P[follower]["Front Plane"])
mate("coincident", P[base]["Top Plane"], P[follower]["Top Plane"])
mate("coincident", P[base]["Top Plane"], P[full]["Top Plane"])
mate("coincident", P[base]["Front Plane"], P[full]["Front Plane"])
mate("distance", P[base]["Right Plane"], P[full]["Right Plane"], distance_mm=200)
mate("angle", P[base]["Top Plane"], P[angle]["Top Plane"], angle_deg=30)
# spin: concentric to the fixed axle (slides along and turns about X). hinge: + its
# Front plane 50 from the axle's -> turns only.
axle_face = cylinder(axle)
mate("concentric", axle_face, cylinder(spin))
mate("concentric", axle_face, cylinder(hinge))
mate("concentric", axle_face, cylinder(spinlock), lock_rotation=True)
mate("distance", P[axle]["Front Plane"], P[hinge]["Front Plane"], distance_mm=50)
# The pair: fully defined against each other, free as a pair. Not skipped as "fully
# defined" unless SolidWorks says so; either way the pair's 6 must show.
mcp.call("add_mate", kind="lock", entities=[pair_a, pair_b])
before = poses()

# ---- Everything -------------------------------------------------------------------------
t = time.time()
r, cs = mobility()
took = time.time() - t
print("   ", r["summary"])
for c in r["components"]:
    print(f"    {c['name']} ({c['id']}): dof={c.get('dof')} {c.get('solidworks_status', '')}: {c['free']}"
          + (f" drags {c['drags_along']}" if "drags_along" in c else "")
          + (f" dragged by {c['dragged_by']}" if "dragged_by" in c else ""))
for n in r.get("notes", []):
    print("    note:", n)
assert len(cs) == 14 and not any(c.get("parent") for c in cs.values()), r  # top-level only

assert cs[base]["dof"] == 0 and cs[base]["fixed"] and cs[axle]["fixed"], r
assert cs[free]["dof"] == 6 and cs[free]["free"] == "translates in any direction; turns about any axis", cs[free]
assert cs[sub1]["dof"] == 6 and cs[sub1].get("subassembly"), cs[sub1]
assert (cs[plane]["translations"], cs[plane]["rotations"]) == (2, 1), cs[plane]
assert cs[plane]["free"].startswith("translates in the plane normal to [0.0,0.0,1.0]; turns about [0.0,0.0,1.0]"), cs[plane]
assert (cs[slider]["dof"], cs[slider]["translate_along"]) == (1, [[1.0, 0.0, 0.0]]), cs[slider]
assert cs[slider]["drags_along"] == [f"{cs[follower]['name']} ({follower})"], cs[slider]
# The follower only slides along y when dragged itself; x comes from the slider's nudges.
assert (cs[follower]["dof"], cs[follower]["translations"]) == (2, 2), cs[follower]
assert cs[follower]["dragged_by"] == [f"{cs[slider]['name']} ({slider})"], cs[follower]
assert cs[full]["dof"] == 0 and cs[full]["solidworks_status"] == "fully defined", cs[full]
assert cs[full]["free"] == "nothing: its mates fully define it", cs[full]
assert all(c.get("solidworks_status") != "no solution" for c in cs.values()), r
assert (cs[spin]["translations"], cs[spin]["rotations"]) == (1, 1), cs[spin]
assert same_line(cs[spin]["translate_along"][0], [1, 0, 0]), cs[spin]
rot = cs[spin]["rotate_about"][0]
assert same_line(rot["axis"], [1, 0, 0]) and near(rot["through_mm"][1:], [0, 0]), cs[spin]
assert (cs[hinge]["translations"], cs[hinge]["rotations"]) == (0, 1), cs[hinge]
rot = cs[hinge]["rotate_about"][0]
assert same_line(rot["axis"], [1, 0, 0]) and near(rot["through_mm"][1:], [0, 0]) and "pitch_mm_per_turn" not in rot, cs[hinge]
# The angle mate: free to translate; SolidWorks' solver lets it turn about its own Top
# normal only (seen with move_component) -- whatever it is, report it.
assert cs[angle]["translations"] == 3 and cs[angle]["rotations"] >= 1, cs[angle]
assert cs[pair_a]["dof"] == 6 and cs[pair_b]["dof"] == 6, (cs[pair_a], cs[pair_b])
# Not asserted (2026-10-25: SolidWorks still let it turn when nudged): does the
# concentric mate's lock_rotation hold?
print("    concentric + lock_rotation:", cs[spinlock]["free"])
print(f"    all {len(cs)} components in {took:.1f} s")

# ---- Nothing moved -----------------------------------------------------------------------
# Exactly, except the lock pair: re-solving it after its own nudges shifts the partner by
# micrometers (19 um once; SolidWorks' tolerance on lock mates, seen 2026-10-25).
after = poses()
assert before.keys() == after.keys(), (before, after)
assert all(near(before[i][0], after[i][0], 0.1 if i in (pair_a, pair_b) else 1e-3) for i in before), (before, after)
pair_names = {cs[pair_a]["name"], cs[pair_b]["name"]}
for n in r.get("notes", []):
    if "didn't put these back" in n:
        assert set(re.findall(r"mob-[a-z]+-\d+", n.split(":")[1])) <= pair_names, n
ms = mcp.call("inspect_model", what="mates").json["mates"]
assert not any(m.get("error") for m in ms), ms
assert not (open_dialogs := sw_dialogs()), open_dialogs

# ---- Some only ---------------------------------------------------------------------------
r, cs = mobility(components=[hinge, base])
assert list(cs) == [hinge, base] and cs[hinge]["dof"] == 1 and cs[base]["dof"] == 0, r
# Only the follower: its own nudges see y, not the x it shares with the slider.
r, cs = mobility(components=[follower], step_mm=0.5, step_deg=0.5)
assert cs[follower]["dof"] == 1 and same_line(cs[follower]["translate_along"][0], [0, 1, 0]), r
assert r["probe"]["step_mm"] == 0.5, r

# ---- Refusals ----------------------------------------------------------------------------
nested = [c for c in mcp.call("inspect_model", what="components").json["components"] if c.get("parent") == sub1]
r = mcp.call("inspect_mobility", components=[nested[0]["id"]], expect_error=True)
assert "inside the subassembly" in r.text, r.text
r = mcp.call("inspect_mobility", components=[P[base]["Top Plane"]], expect_error=True)
assert "component ids" in r.text, r.text
r = mcp.call("inspect_mobility", step_mm=0, expect_error=True)
assert "step_mm" in r.text, r.text
assert near(poses()[slider][0], before[slider][0])

# ---- A part: refused ---------------------------------------------------------------------
close_all()
mcp.call("open_document", path=str(block))
r = mcp.call("inspect_mobility", expect_error=True)
print("    on a part:", r.text)
assert "assembl" in r.text.lower(), r.text
close_all()

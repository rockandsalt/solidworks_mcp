# Limit mates and collision checks along a motion, against real SolidWorks
# (docs/notes/architecture.md, "Assemblies + mates"): add_mate's limit
# distance / angle mates (reported with their range by inspect_model),
# edit_mate making a plain mate a limit mate, changing and removing limits,
# move_component holding a limit mate at its value, sweep_mate driving a
# limit mate over its range with the collisions found where the geometry
# says (first contact, volume, the hinge's own overlap left out), stopping
# at the first contact and keeping it, move_component check_collisions /
# stop_at_collision, refusals changing nothing; saved and reopened with the
# limits. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/limits.py

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


def components():
    return {c["id"]: c for c in mcp.call("inspect_model", what="components").json.get("components", [])}


def mates():
    return {m["name"]: m for m in mcp.call("inspect_model", what="mates").json.get("mates", [])}


def first_id(r):
    return re.findall(r"^- (component:\d+):", r.text, re.M)[0]


def near(a, b, tol=1e-3):
    return all(abs(x - y) < tol for x, y in zip(a, b))


def poses():
    return {i: (c["origin_mm"], c.get("axes")) for i, c in components().items()}


def same(a, b):
    return a.keys() == b.keys() and all(
        near(a[i][0], b[i][0]) and (a[i][1] is None) == (b[i][1] is None)
        and (a[i][1] is None or all(near(a[i][1][k], b[i][1][k], 1e-5) for k in "xyz")) for i in a)


def call(tool, **args):
    t = time.time()
    r = mcp.call(tool, **args)
    print(f"    {tool} {args} ({time.time() - t:.1f} s):\n      " + r.text.replace("\n", "\n      "))
    return r


def x_axis(cid):
    return components()[cid]["axes"]["x"] if "axes" in components()[cid] else [1.0, 0.0, 0.0]


out = mcp.run_dir.resolve()
block = out / "lim-block.SLDPRT"
top = out / "lim-top.SLDASM"

# ---- The part: 40 x 20 on Front, 10 deep along +X (x 0..10, y -20..20, z -10..10),
# with a hinge axis where Front and Right meet (along Z through the origin) ------------
mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
mcp.call("create_reference_geometry", geometry="axis", references=["front", "right"], name="Hinge")
mcp.call("save_document", path=str(block))
close_all()

# ---- The assembly ---------------------------------------------------------------------
# base (fixed, origin). slider [20,60,0]: Top on base's Top, Right 60 from base's Right
# (y held), Front planes a limit distance 0..40 apart starting at 20 (x 20..60).
# obstacle (fixed) [55,60,0]: in the slider's lane past its stroke. spinner at the
# origin, hinged on base's Hinge axis (+ Top on Top), Front planes a limit angle 0..90
# from 0: it overlaps the base all along (left out of collisions). stop (fixed)
# [12,0,0]: 2 mm past the spinner's end face; turning either way, the spinner's
# corner (10, -+20) reaches x = 12 at 5.6 deg. rover [0,-80,0]: Top on Top, free in
# the plane, 20 mm from the base's side.
mcp.call("create_assembly")
mcp.call("save_document", path=str(top))
base = first_id(mcp.call("insert_components", file=str(block)))
slider = first_id(mcp.call("insert_components", file=str(block), at=[{"origin_mm": [20, 60, 0]}]))
obstacle = first_id(mcp.call("insert_components", file=str(block), at=[{"origin_mm": [55, 60, 0]}], fixed=True))
spinner = first_id(mcp.call("insert_components", file=str(block), at=[{"origin_mm": [0, 0, 0]}]))
stop = first_id(mcp.call("insert_components", file=str(block), at=[{"origin_mm": [12, 0, 0]}], fixed=True))
rover = first_id(mcp.call("insert_components", file=str(block), at=[{"origin_mm": [0, -80, 0]}]))
pl = {c: {f["name"]: f["id"] for f in mcp.call("inspect_model", what="features", within=c).json["features"]}
      for c in (base, slider, spinner, rover)}
mcp.call("add_mate", kind="coincident", entities=[pl[base]["Top Plane"], pl[slider]["Top Plane"]])
mcp.call("add_mate", kind="distance", distance_mm=60, entities=[pl[base]["Right Plane"], pl[slider]["Right Plane"]])
r = call("add_mate", kind="distance", min_mm=0, max_mm=40, distance_mm=20,
         entities=[pl[base]["Front Plane"], pl[slider]["Front Plane"]])
assert "limits 0 .. 40 mm" in r.text and "sweep_mate" in r.text, r.text
stroke = re.search(r"added (\S+) \((feature:\d+)\)", r.text).group(1)
mcp.call("add_mate", kind="coincident", entities=[pl[base]["Hinge"], pl[spinner]["Hinge"]])
mcp.call("add_mate", kind="coincident", entities=[pl[base]["Top Plane"], pl[spinner]["Top Plane"]])
r = call("add_mate", kind="angle", min_deg=0, max_deg=90, entities=[pl[base]["Front Plane"], pl[spinner]["Front Plane"]])
assert "limits 0 .. 90 deg" in r.text, r.text
swing = re.search(r"added (\S+) \((feature:\d+)\)", r.text).group(1)
mcp.call("add_mate", kind="coincident", entities=[pl[base]["Top Plane"], pl[rover]["Top Plane"]])

ms = mates()
print("    mates:", {n: {k: v for k, v in m.items() if k in ("type", "distance_mm", "angle_deg", "min_mm", "max_mm", "min_deg", "max_deg")} for n, m in ms.items()})
assert ms[stroke]["type"] == "distance" and ms[stroke]["distance_mm"] == 20.0, ms[stroke]
assert ms[stroke]["min_mm"] == 0.0 and ms[stroke]["max_mm"] == 40.0, ms[stroke]
assert ms[swing]["type"] == "angle" and ms[swing]["min_deg"] == 0.0 and ms[swing]["max_deg"] == 90.0, ms[swing]
assert "min_mm" not in ms["Distance1"], ms["Distance1"]
assert not any(m.get("error") for m in ms.values()), ms
start = poses()
assert near(start[slider][0], [20, 60, 0]) and near(start[spinner][0], [0, 0, 0]), start

# ---- Refusals at add_mate (nothing added) -----------------------------------------------
for args, why in [
    (dict(kind="distance", min_mm=0, max_mm=40, distance_mm=50), "isn't within the limits"),
    (dict(kind="distance", min_mm=30, max_mm=10), "must satisfy"),
    (dict(kind="distance", min_mm=5), "both min_mm and max_mm"),
    (dict(kind="coincident", min_deg=0, max_deg=10), "only angle mates"),
    (dict(kind="angle", min_deg=0, max_deg=200), "must satisfy"),
]:
    r = mcp.call("add_mate", entities=[pl[base]["Right Plane"], pl[rover]["Right Plane"]], expect_error=True, **args)
    assert why in r.text, (args, r.text)
assert mates().keys() == ms.keys()

# ---- move_component keeps a limit mate at its value -------------------------------------
r = call("move_component", component=slider, translate_mm=[10, 0, 0])
assert "it didn't move" in r.text, r.text
assert same(poses(), start)

# ---- edit_mate: limits widened, a plain mate made a limit mate and back -----------------
r = call("edit_mate", mate=stroke, max_mm=50)
assert "limits 0 .. 50 mm" in r.text and f"edited {stroke}" in r.text, r.text
r = call("edit_mate", mate="Distance1", min_mm=50, max_mm=70)
assert "edited Distance1" in r.text and "limits 50 .. 70 mm" in r.text, r.text
assert mates()["Distance1"]["max_mm"] == 70.0
r = mcp.call("edit_mate", mate="Distance1", min_mm=0, max_mm=40, expect_error=True)
assert "isn't within its limits" in r.text and "give a value in the range" in r.text, r.text
r = call("edit_mate", mate="Distance1", remove_limits=True)
assert "limits" not in r.text.split("; now", 1)[1], r.text
m = mates()["Distance1"]
assert "min_mm" not in m and m["distance_mm"] == 60.0, m
r = mcp.call("edit_mate", mate="Coincident1", remove_limits=True, expect_error=True)
assert "has no limits" in r.text, r.text
assert same(poses(), start), (start, poses())

# ---- sweep_mate: the slider over its stroke (0..50 every 5 mm) --------------------------
# x = value; the obstacle starts at x = 55, so the slider (10 long) touches at 45 (not an
# overlap) and overlaps 5 x 40 x 20 = 4000 mm³ at 50.
r = call("sweep_mate", mate=stroke, steps=10)
assert f"Swept {stroke}" in r.text and "from 0 mm to 50 mm in 10 steps (every 5 mm" in r.text, r.text
assert "first contact between 45 mm and 50 mm" in r.text and "most 4000 mm³ at 50 mm" in r.text, r.text
assert components()[obstacle]["name"] in r.text and "Restored" in r.text, r.text
assert same(poses(), start), (start, poses())
# Stopped at the contact and kept there: at 45.
r = call("sweep_mate", mate=stroke, steps=10, stop_at_collision=True, restore=False)
assert "went back to 45 mm" in r.text and "is left at 45 mm" in r.text, r.text
assert near(components()[slider]["origin_mm"], [45, 60, 0]), components()[slider]
assert mates()[stroke]["distance_mm"] == 45.0
# A narrower range, from where it is: clear.
r = call("sweep_mate", mate=stroke, from_mm=10, to_mm=40, steps=3)
assert "Collisions: none" in r.text and "4 positions" in r.text, r.text
r = call("sweep_mate", mate=stroke, from_mm=45, to_mm=20, steps=1, restore=False)
assert near(components()[slider]["origin_mm"], [20, 60, 0]), components()[slider]
assert same(poses(), start), (start, poses())

# ---- sweep_mate: the spinner about its hinge (0..90 every 5 deg) ------------------------
# It overlaps the base all along (they share the hinge): left out. The stop is hit
# between 5 and 10 deg.
r = call("sweep_mate", mate=swing, steps=18)
assert "from 0 deg to 90 deg in 18 steps (every 5 deg" in r.text, r.text
assert "first contact between 5 deg and 10 deg" in r.text, r.text
assert re.search(r"Already overlapping before the sweep \(left out above\): .*" + components()[base]["name"], r.text), r.text
assert components()[stop]["name"] in r.text.split("Already")[0], r.text
assert "Restored" in r.text, r.text
assert same(poses(), start), (start, poses())
r = call("sweep_mate", mate=swing, steps=18, stop_at_collision=True, restore=False)
assert "went back to 5 deg" in r.text, r.text
ax = x_axis(spinner)
assert abs(abs(ax[1]) - 0.0871557) < 1e-4 and abs(ax[0] - 0.9961947) < 1e-4, ax  # turned 5 deg
r = call("sweep_mate", mate=swing, from_deg=5, to_deg=0, steps=1, restore=False)
assert same(poses(), start), (start, poses())

# ---- sweep_mate on a plain distance mate: needs the end --------------------------------
r = mcp.call("sweep_mate", mate="Distance1", expect_error=True)
assert "so it has no range of its own" in r.text and "60 mm" in r.text, r.text
r = call("sweep_mate", mate="Distance1", to_mm=70, steps=2)
assert "from 60 mm to 70 mm" in r.text and "Collisions: none" in r.text and "Restored" in r.text, r.text
assert components()[slider]["name"] in r.text.split("Moved by the sweep:")[1], r.text

# ---- sweep_mate refusals, changing nothing ---------------------------------------------
for args, why in [
    (dict(mate=stroke, from_mm=0, to_mm=60), "only goes from 0 mm to 50 mm"),
    (dict(mate=swing, to_mm=10), "is an angle mate: give the range in degrees"),
    (dict(mate="Coincident1"), "only distance and angle mates"),
    (dict(mate=stroke, from_mm=20, to_mm=20), "nothing to sweep"),
    (dict(mate=pl[base]["Front Plane"]), "belongs to a component"),
]:
    r = mcp.call("sweep_mate", expect_error=True, **args)
    assert why in r.text, (args, r.text)
assert same(poses(), start), (start, poses())

# ---- move_component check_collisions: the rover pushed 50 mm into the base -------------
# Its top edge (y -60) reaches the base (y -20) at 40 mm: overlaps 10 x 5 x 20 = 1000 mm³
# at 45, 2000 at 50; the spinner, parked on the base, too.
r = call("move_component", component=rover, translate_mm=[0, 50, 0], steps=10, check_collisions=True, restore=True)
assert "first contact between 40 mm" in r.text and "most 2000 mm³ at 50 mm" in r.text, r.text
assert components()[base]["name"] in r.text and "Restored" in r.text, r.text
assert same(poses(), start), (start, poses())
r = call("move_component", component=rover, translate_mm=[0, 50, 0], steps=10, stop_at_collision=True)
assert "Stopped at the first collision (step 9 of 10) and went back to step 8" in r.text, r.text
assert near(components()[rover]["origin_mm"], [0, -40, 0]), components()[rover]
r = call("move_component", component=rover, translate_mm=[0, -40, 0], check_collisions=True)
assert "Collisions: none" in r.text, r.text
assert same(poses(), start), (start, poses())

ms = mates()
assert not any(m.get("error") for m in ms.values()), ms
assert not (open_dialogs := sw_dialogs()), open_dialogs

# ---- Saved and reopened with the limits --------------------------------------------------
mcp.call("save_document")
close_all()
mcp.call("open_document", path=str(top))
after = mates()
assert after[stroke]["min_mm"] == 0.0 and after[stroke]["max_mm"] == 50.0, after[stroke]
assert after[swing]["max_deg"] == 90.0 and "min_mm" not in after["Distance1"], after
close_all()

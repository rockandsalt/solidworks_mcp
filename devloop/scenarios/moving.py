# move_component against real SolidWorks: a slider its mates hold to one
# axis (a requested move partly dropped, a follower mated to it coming
# along, a turn it can't make), a free block turned in place and checked by
# its box, a spinner turned in steps about the one axis its mate allows and
# not tilted, "what if" moves put back with restore, a subassembly moved,
# and refusals (a fixed component, one inside a subassembly, a face, no
# motion) changing nothing (docs/notes/architecture.md, "Assemblies +
# mates"). Run with:
#   py devloop/mcp_client.py run devloop/scenarios/moving.py

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


def close_all():
    com("Com $sw CloseAllDocuments @($true) | Out-Null")


def components():
    return {c["id"]: c for c in mcp.call("inspect_model", what="components").json.get("components", [])}


def poses():
    return {i: (c["origin_mm"], c.get("axes")) for i, c in components().items()}


def first_id(r):
    return re.findall(r"^- (component:\d+):", r.text, re.M)[0]


def near(a, b, tol=1e-3):
    return all(abs(x - y) < tol for x, y in zip(a, b))


def same(a, b):
    """Two poses() alike to 1e-3 mm (and their axes to 1e-6)."""
    return a.keys() == b.keys() and all(
        near(a[i][0], b[i][0]) and (a[i][1] is None) == (b[i][1] is None)
        and (a[i][1] is None or all(near(a[i][1][k], b[i][1][k], 1e-6) for k in "xyz")) for i in a)


def move(**args):
    r = mcp.call("move_component", **args)
    print(f"    move {args}:\n      " + r.text.replace("\n", "\n      "))
    return r


out = mcp.run_dir.resolve()
block = out / "move-block.SLDPRT"
sub = out / "move-sub.SLDASM"
top = out / "move-top.SLDASM"

# ---- The part (40 x 20 on Front, 10 deep along +X: x 0..10, y -20..20, z -10..10)
# and a one-block subassembly ----------------------------------------------------------
mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
mcp.call("save_document", path=str(block))
close_all()
mcp.call("create_assembly")
mcp.call("insert_components", file=str(block))
mcp.call("save_document", path=str(sub))
close_all()

# ---- The assembly ---------------------------------------------------------------------
# base: fixed. slider: Top on base's Top, Right 60 from base's Right -> only x free.
# follower: Front on slider's Front (x tied to it), Top on base's Top -> y free.
# free: no mates. spinner: Top on base's Top -> slides in that plane, turns about Z.
mcp.call("create_assembly")
mcp.call("save_document", path=str(top))  # first: saving renames the document, which renews its ids
base = first_id(mcp.call("insert_components", file=str(block)))
slider = first_id(mcp.call("insert_components", file=str(block), at=[{"origin_mm": [0, 60, 0]}]))
follower = first_id(mcp.call("insert_components", file=str(block), at=[{"origin_mm": [0, 120, 0]}]))
free = first_id(mcp.call("insert_components", file=str(block), at=[{"origin_mm": [0, -80, 0]}]))
spinner = first_id(mcp.call("insert_components", file=str(block), at=[{"origin_mm": [0, -160, 0]}]))
sub1 = first_id(mcp.call("insert_components", file=str(sub), at=[{"origin_mm": [0, 0, 200]}]))
pl = {c: {f["name"]: f["id"] for f in mcp.call("inspect_model", what="features", within=c).json["features"]}
      for c in (base, slider, follower, spinner)}
mcp.call("add_mate", kind="coincident", entities=[pl[base]["Top Plane"], pl[slider]["Top Plane"]])
mcp.call("add_mate", kind="distance", distance_mm=60, entities=[pl[base]["Right Plane"], pl[slider]["Right Plane"]])
mcp.call("add_mate", kind="coincident", entities=[pl[slider]["Front Plane"], pl[follower]["Front Plane"]])
mcp.call("add_mate", kind="coincident", entities=[pl[base]["Top Plane"], pl[follower]["Top Plane"]])
mcp.call("add_mate", kind="coincident", entities=[pl[base]["Top Plane"], pl[spinner]["Top Plane"]])
start = poses()
assert near(start[slider][0], [0, 60, 0]) and near(start[follower][0], [0, 120, 0]), start

# ---- The slider: the y / z of the move dropped, the follower comes along -------------
r = move(component=slider, translate_mm=[10, 5, 3])
assert "Solver: its mates held it back: it ended 5.831 mm" in r.text, r.text
assert f"- {components()[follower]['name']} ({follower}): origin [0.0,120.0,0.0] -> [10.0,120.0,0.0] mm" in r.text, r.text
p = poses()
assert near(p[slider][0], [10, 60, 0]) and near(p[follower][0], [10, 120, 0]), p
assert near(p[free][0], start[free][0]) and near(p[spinner][0], start[spinner][0]), p
r = move(component=slider, translate_mm=[-10, 0, 0])
assert "Solver: reached the requested pose" in r.text, r.text
assert near(poses()[slider][0], [0, 60, 0])
# A turn the mates forbid (Right planes 60 apart stay parallel). About the
# box center (5, 60, 0) it also asks the origin to move (0.30, -1.71) mm:
# the solver keeps the 0.30 along X, the follower with it.
r = move(component=slider, axis=[0, 0, 1], angle_deg=20)
got = re.search(r"^Got: .*$", r.text, re.M).group(0)
assert "held it back" in r.text and "turned" not in got and "Also moved" in r.text, r.text
x = components()[slider]["origin_mm"][0]
assert abs(x - 5 * (1 - 0.9396926)) < 1e-3, x
move(component=slider, translate_mm=[-x, 0, 0])
# About its own origin: nothing of it is allowed.
r = move(component=slider, axis=[0, 0, 1], angle_deg=20, center_mm=[0, 60, 0])
assert "Solver: it didn't move" in r.text and "Also moved" not in r.text, r.text
assert same(poses(), start), (start, poses())
# The follower along y (free); along x the slider would have to come along.
r = move(component=follower, translate_mm=[0, 20, 0])
assert "Solver: reached the requested pose" in r.text and "Also moved" not in r.text, r.text
r = move(component=follower, translate_mm=[15, 0, 0])
print("    (the follower pushed along x: SolidWorks", "moved the slider)" if "Also moved" in r.text else "kept it)")
r = move(component=follower, translate_mm=[0, -20, 0])
assert "reached" in r.text, r.text

# ---- The free block turned a quarter in place: about its box center ------------------
r = move(component=free, axis=[0, 0, 1], angle_deg=90)
assert "Solver: reached the requested pose" in r.text and "axis through [5.0,-80.0,0.0] mm, in 6 steps" in r.text, r.text
c = components()[free]
assert near(c["origin_mm"], [5, -85, 0]) and near(c["axes"]["x"], [0, 1, 0], 1e-6), c
assert near(c["bbox_mm"]["min"], [-15, -85, -10]) and near(c["bbox_mm"]["max"], [25, -75, 10]), c
# And back, about a given point, with a translation: the same place as at the start.
r = move(component=free, axis=[0, 0, 1], angle_deg=-90, center_mm=[5, -80, 0], translate_mm=[0, 0, 0])
assert "reached" in r.text, r.text
c = components()[free]
assert near(c["origin_mm"], [0, -80, 0]) and "axes" not in c, c

# ---- The spinner: turns about Z in steps, can't tilt -----------------------------------
r = move(component=spinner, axis=[0, 0, 1], angle_deg=120, steps=4)
assert "reached" in r.text and "in 4 steps" in r.text, r.text
c = components()[spinner]
assert near(c["axes"]["x"], [-0.5, 0.866025, 0], 1e-5) and abs(c["origin_mm"][2]) < 1e-3, c
r = move(component=spinner, axis=[1, 0, 0], angle_deg=30)
assert "reached" not in r.text, r.text
c = components()[spinner]
assert abs(c["axes"]["z"][2] - 1) < 1e-6 and abs(c["origin_mm"][2]) < 1e-3, c

# ---- restore: where the move would go, nothing kept ------------------------------------
before = poses()
r = move(component=slider, translate_mm=[50, 0, 0], restore=True)
assert r.text.startswith("Tried moving") and "reached" in r.text and "Also moved" in r.text, r.text
assert "Restored: every component is back" in r.text and "out of date" not in r.text, r.text
assert same(poses(), before), (before, poses())
r = move(component=spinner, translate_mm=[0, 0, 25], restore=True)
assert "it didn't move" in r.text and "Restored" in r.text, r.text
assert same(poses(), before), (before, poses())

# ---- A subassembly moves as a whole ---------------------------------------------------
r = move(component=sub1, translate_mm=[0, 0, 10])
assert "reached" in r.text, r.text
cs = components()
assert near(cs[sub1]["origin_mm"], [0, 0, 210]), cs[sub1]
nested = [c for c in cs.values() if c.get("parent") == sub1]
assert len(nested) == 1 and near(nested[0]["origin_mm"], [0, 0, 210]), nested

# ---- Refusals, changing nothing -------------------------------------------------------
before = poses()
r = mcp.call("move_component", component=base, translate_mm=[10, 0, 0], expect_error=True)
assert "is fixed" in r.text and "fix_components" in r.text, r.text
r = mcp.call("move_component", component=nested[0]["id"], translate_mm=[10, 0, 0], expect_error=True)
assert "inside the subassembly" in r.text, r.text
r = mcp.call("move_component", component=pl[base]["Top Plane"], translate_mm=[10, 0, 0], expect_error=True)
assert "component id" in r.text, r.text
r = mcp.call("move_component", component=free, expect_error=True)
assert "the motion to try" in r.text, r.text
r = mcp.call("move_component", component=free, angle_deg=30, expect_error=True)
assert "needs the axis" in r.text, r.text
assert same(poses(), before), (before, poses())
ms = mcp.call("inspect_model", what="mates").json["mates"]
assert not any(m.get("error") for m in ms), ms
assert not (open_dialogs := sw_dialogs()), open_dialogs

# ---- Saved and reopened where they were moved -----------------------------------------
saved = {c["name"]: c["origin_mm"] for c in components().values()}
mcp.call("save_document")
close_all()
mcp.call("open_document", path=str(top))
after = {c["name"]: c["origin_mm"] for c in components().values()}
print("    reopened:", after)
assert after.keys() == saved.keys() and all(near(after[n], saved[n]) for n in saved), (saved, after)
close_all()

# fix_components against real SolidWorks: fixing and floating components,
# ones already in that state left alone, a distance mate that can't solve
# while both its components are fixed and moves the one floated afterwards,
# a whole subassembly fixed and floated, the parts inside one (rigid or
# flexible) refused, and the states surviving a save and reopen
# (docs/notes/architecture.md, "Assemblies + mates"). The subassembly is made
# flexible through COM. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/fixing.py

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


def mates():
    return {m["name"]: m for m in mcp.call("inspect_model", what="mates").json.get("mates", [])}


def first_id(r):
    return re.findall(r"^- (component:\d+):", r.text, re.M)[0]


def side_face(c, ny):
    faces = mcp.call("inspect_model", what="faces", within=c, surface_type="plane").json["faces"]
    hits = [f["id"] for f in faces if abs(f["normal"][1] - ny) < 1e-6]
    assert len(hits) == 1, faces
    return hits[0]


out = mcp.run_dir.resolve()
block = out / "fix-block.SLDPRT"
sub = out / "fix-sub.SLDASM"
top = out / "fix-top.SLDASM"

# ---- The part, and a subassembly of two (the first fixed, the second floating) ----
# 40 x 20 on Front, 10 deep along +X (Front faces +X on this machine): y -20..20.
mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
mcp.call("save_document", path=str(block))
close_all()
mcp.call("create_assembly")
mcp.call("insert_components", file=str(block))
mcp.call("insert_components", file=str(block), at=[{"origin_mm": [0, 60, 0]}])
mcp.call("save_document", path=str(sub))
close_all()

# ---- The assembly: a fixed base, a mover 20 mm off it, the subassembly ------------
mcp.call("create_assembly")
mcp.call("save_document", path=str(top))  # first: saving renames the document, which renews its ids
base = first_id(mcp.call("insert_components", file=str(block)))
mover = first_id(mcp.call("insert_components", file=str(block), at=[{"origin_mm": [0, 60, 0]}]))
sub1 = first_id(mcp.call("insert_components", file=str(sub), at=[{"origin_mm": [0, 0, 200]}]))
planes = {c: {f["name"]: f["id"] for f in mcp.call("inspect_model", what="features", within=c).json["features"]}
          for c in (base, mover)}
mcp.call("add_mate", kind="coincident", entities=[planes[base]["Top Plane"], planes[mover]["Top Plane"]])
r = mcp.call("add_mate", kind="distance", distance_mm=20, alignment="anti_aligned",
             entities=[side_face(base, 1), side_face(mover, -1)])
dim = re.search(r'set_dimension name="([^"]+)"', r.text).group(1)
cs = components()
assert cs[base]["fixed"] and not cs[mover]["fixed"] and not cs[sub1]["fixed"], cs
assert abs(cs[mover]["origin_mm"][1] - 60) < 1e-3, cs[mover]

# ---- Fix, and again: left alone -------------------------------------------------
r = mcp.call("fix_components", components=[mover], fixed=True)
print("    fix:", r.text)
assert r.text.startswith(f"fixed 1 component:\n- {mover}: ") and "(fixed)" in r.text, r.text
assert components()[mover]["fixed"]
r = mcp.call("fix_components", components=[mover, base, mover], fixed=True)
print("    again:", r.text)
assert r.text.startswith("fixed 0 components.") and "Already fixed, left alone" in r.text, r.text
assert mover in r.text and base in r.text, r.text

# ---- Both fixed: a longer distance can't solve; floating the mover solves it -------
r = mcp.call("set_dimension", name=dim, value_mm=30)
print("    longer:", r.text)
ms = mates()
print("    mates:", ms)
assert any(m.get("error") for m in ms.values()), ms
assert abs(components()[mover]["origin_mm"][1] - 60) < 1e-3
r = mcp.call("fix_components", components=[mover], fixed=False)
print("    float:", r.text)
assert r.text.startswith("floated 1 component:") and "moved by the rebuild" in r.text, r.text
c = components()[mover]
assert not c["fixed"] and abs(c["origin_mm"][1] - 70) < 1e-3, c
ms = mates()
assert not any(m.get("error") for m in ms.values()), ms

# Fixing it where it is breaks nothing.
r = mcp.call("fix_components", components=[mover], fixed=True)
assert r.text.startswith("fixed 1 component:") and "failing" not in r.text and "moved" not in r.text, r.text

# ---- Subassemblies: the whole one, and the parts inside ----------------------------
# A rigid subassembly's parts report the subassembly's state (IsFixed), not
# their own in its file: floating while it floats, fixed once it's fixed.
nested = sorted((c for c in components().values() if c.get("parent") == sub1), key=lambda c: c["name"])
assert len(nested) == 2 and not any(c["fixed"] for c in nested), nested
r = mcp.call("fix_components", components=[sub1], fixed=True)
assert r.text.startswith("fixed 1 component:"), r.text
nested = sorted((c for c in components().values() if c.get("parent") == sub1), key=lambda c: c["name"])
assert all(c["fixed"] for c in nested), nested
r = mcp.call("fix_components", components=[nested[0]["id"]], fixed=False, expect_error=True)
assert "inside the subassembly" in r.text, r.text
r = mcp.call("fix_components", components=[sub1], fixed=False)
assert r.text.startswith("floated 1 component:"), r.text
nested = sorted((c for c in components().values() if c.get("parent") == sub1), key=lambda c: c["name"])
assert not any(c["fixed"] for c in nested), nested
# Flexible: its parts still report floating (even fix-block-1, fixed in the
# subassembly's file), and are refused too -- SolidWorks ignores Fix / Float
# on them.
name = components()[sub1]["name"]
solving = com(f"""$asm = Com $sw ActiveDoc -Get
$c = Com $asm GetComponentByName @('{name}')
$null = Com $asm ClearSelection2 @($true)
if (-not (Com $c Select4 @($false, [Runtime.InteropServices.DispatchWrapper]::new([object]$null), $false))) {{ throw 'Select4 failed' }}
if (-not (Com $asm CompConfigProperties3 @(2, 1, $true, $false))) {{ throw 'CompConfigProperties3 failed' }}
$null = Com $asm ClearSelection2 @($true)
Com $c Solving -Get""")
assert solving == "1", solving  # swComponentFlexibleSolving
nested = sorted((c for c in components().values() if c.get("parent") == sub1), key=lambda c: c["name"])
assert not any(c["fixed"] for c in nested), nested
r = mcp.call("fix_components", components=[nested[1]["id"]], fixed=True, expect_error=True)
assert "inside the subassembly" in r.text, r.text

# ---- Refusals ------------------------------------------------------------------------
r = mcp.call("fix_components", components=[planes[base]["Top Plane"]], fixed=True, expect_error=True)
assert "component id" in r.text, r.text
r = mcp.call("fix_components", components=[], fixed=True, expect_error=True)
assert "list the components" in r.text, r.text
assert not (open_dialogs := sw_dialogs()), open_dialogs

# ---- Saved and reopened ---------------------------------------------------------------
before = {c["name"]: c["fixed"] for c in components().values()}
mcp.call("save_document")
close_all()
mcp.call("open_document", path=str(top))
after = {c["name"]: c["fixed"] for c in components().values()}
print("    reopened:", after)
assert after == before, (before, after)
assert not (open_dialogs := sw_dialogs()), open_dialogs
close_all()

# insert_components against real SolidWorks: parts, a subassembly, several
# instances of one part in one call, copies of a part and of a subassembly,
# placements (origin and turned axes, checked by component boxes), the first
# component of an empty assembly fixed, configurations, and refusals that
# must insert nothing (docs/notes/architecture.md, "Assemblies + mates").
# The part's second configuration is made through COM. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/components.py

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
    return mcp.call("inspect_model", what="components").json.get("components", [])


def ids_of(r):
    """The component ids an insert_components reply lists, in order."""
    return re.findall(r"^- (component:\d+):", r.text, re.M)


def by_id(cs):
    return {c["id"]: c for c in cs}


def close(a, b, tol=1e-3):
    return all(abs(x - y) <= tol for x, y in zip(a, b))


def bbox(c):
    return c["bbox_mm"]["min"], c["bbox_mm"]["max"]


out = mcp.run_dir.resolve()
block = out / "comp-block.SLDPRT"
sub = out / "comp-sub.SLDASM"
top = out / "comp-top.SLDASM"

# ---- The part: 40 x 20 rectangle on Front, 10 deep along +X ----------------------
# (Front faces +X on this machine: local box x 0..10, y -20..20, z -10..10.)
mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
mcp.call("save_document", path=str(block))
b = mcp.call("inspect_model", what="bodies").json["bodies"][0]["bbox_mm"]
local_lo, local_hi = b["min"], b["max"]
print("    part box:", local_lo, local_hi)
assert close(local_lo, [0, -20, -10]) and close(local_hi, [10, 20, 10]), (local_lo, local_hi)
com(f"""$e = 0; $w = 0
$p = Com $sw ActiveDoc -Get
if (-not (Com $p AddConfiguration3 @('Long', '', '', 0))) {{ throw 'AddConfiguration3 failed' }}
Com $p ShowConfiguration2 @('Default') | Out-Null
Com $p Save | Out-Null""")
close_all()

# ---- A subassembly: the part twice, the second a copy of the first -------------
mcp.call("create_assembly")
r = mcp.call("insert_components", file=str(block))
print("    first:", r.text)
first = ids_of(r)
assert len(first) == 1 and "(fixed)" in r.text, r.text
r = mcp.call("insert_components", copy_of=first[0], at=[{"origin_mm": [0, 60, 0]}])
second = ids_of(r)
assert len(second) == 1 and "(fixed)" not in r.text, r.text
cs = by_id(components())
assert len(cs) == 2 and cs[first[0]]["fixed"] and not cs[second[0]]["fixed"], cs
assert close(cs[second[0]]["origin_mm"], [0, 60, 0]), cs[second[0]]
lo, hi = bbox(cs[second[0]])
assert close(lo, [0, 40, -10]) and close(hi, [10, 80, 10]), (lo, hi)
mcp.call("save_document", path=str(sub))
close_all()

# ---- The top assembly ---------------------------------------------------------
mcp.call("create_assembly")
r = mcp.call("insert_components", file=str(sub))
print("    subassembly:", r.text)
assert "subassembly" in r.text and "(fixed)" in r.text, r.text
sub1 = ids_of(r)[0]
cs = components()
assert len(cs) == 3, cs  # the subassembly and its two parts
assert by_id(cs)[sub1].get("subassembly"), cs
assert sum(1 for c in cs if c.get("parent") == sub1) == 2, cs

# Three instances of the part in one call: floating, since the assembly
# isn't empty; the last turned so its X runs along +Y (a quarter turn about Z).
r = mcp.call("insert_components", file=str(block), at=[
    {"origin_mm": [100, 0, 0]},
    {"origin_mm": [100, 0, 50]},
    {"origin_mm": [200, 0, 0], "x_axis": [0, 1, 0]},
])
print("    three:", r.text)
three = ids_of(r)
assert len(three) == 3 and "(fixed)" not in r.text, r.text
cs = by_id(components())
assert len(cs) == 6, cs
for i, origin in zip(three, [[100, 0, 0], [100, 0, 50], [200, 0, 0]]):
    assert close(cs[i]["origin_mm"], origin) and not cs[i]["fixed"], cs[i]
lo, hi = bbox(cs[three[0]])
assert close(lo, [100, -20, -10]) and close(hi, [110, 20, 10]), (lo, hi)
lo, hi = bbox(cs[three[2]])  # x -> +Y, y -> -X
assert close(lo, [180, 0, -10]) and close(hi, [220, 10, 10]), (lo, hi)
assert close(cs[three[2]]["axes"]["x"], [0, 1, 0]) and close(cs[three[2]]["axes"]["y"], [-1, 0, 0]), cs[three[2]]

# Upside down, with both axes given, and fixed on request.
r = mcp.call("insert_components", file=str(block), fixed=True,
             at=[{"origin_mm": [0, 0, -100], "x_axis": [1, 0, 0], "z_axis": [0, 0, -1]}])
flipped = ids_of(r)[0]
c = by_id(components())[flipped]
assert c["fixed"] and close(c["axes"]["y"], [0, -1, 0]), c

# Another instance of the subassembly, and of a part inside it (goes in at the top).
r = mcp.call("insert_components", copy_of=sub1, at=[{"origin_mm": [0, 0, 200]}])
assert "subassembly" in r.text, r.text
nested = next(c["id"] for c in components() if c.get("parent") == sub1)
r = mcp.call("insert_components", copy_of=nested, at=[{"origin_mm": [0, -100, 0]}])
nested_copy = by_id(components())[ids_of(r)[0]]
assert "parent" not in nested_copy and close(nested_copy["origin_mm"], [0, -100, 0]), nested_copy

# Configurations.
r = mcp.call("insert_components", file=str(block), configuration="Long", at=[{"origin_mm": [300, 0, 0]}])
assert 'configuration "Long"' in r.text, r.text
assert by_id(components())[ids_of(r)[0]]["configuration"] == "Long"
count = len(components())

# ---- Refusals: nothing inserted -------------------------------------------------
r = mcp.call("insert_components", file=str(block), configuration="Nope", expect_error=True)
assert "no configuration" in r.text and "Long" in r.text, r.text
r = mcp.call("insert_components", file=str(out / "nope.SLDPRT"), expect_error=True)
assert "no file" in r.text, r.text
r = mcp.call("insert_components", file="comp-block.SLDPRT", expect_error=True)
assert "absolute" in r.text, r.text
r = mcp.call("insert_components", file=str(out / "comp-block.SLDDRW"), expect_error=True)
assert "only parts" in r.text, r.text
r = mcp.call("insert_components", copy_of="face:1", expect_error=True)
assert "component id" in r.text, r.text
r = mcp.call("insert_components", file=str(block), copy_of=sub1, expect_error=True)
assert "not both" in r.text, r.text
r = mcp.call("insert_components", file=str(block), at=[{"x_axis": [1, 0, 0], "z_axis": [1, 1, 0]}], expect_error=True)
assert "perpendicular" in r.text, r.text
mcp.call("save_document", path=str(top))
r = mcp.call("insert_components", file=str(top), expect_error=True)
assert "itself" in r.text, r.text
assert len(components()) == count, components()
assert not (open_dialogs := sw_dialogs()), open_dialogs

# The new ids work with the other tools.
r = mcp.call("add_mate", kind="lock", entities=[three[0], three[1]])
assert "lock" in r.text, r.text

# ---- Saved and reopened -----------------------------------------------------------
mcp.call("save_document")
before = len(components())
close_all()
mcp.call("open_document", path=str(top))
after = components()
assert len(after) == before, (before, len(after))
assert not (open_dialogs := sw_dialogs()), open_dialogs
close_all()

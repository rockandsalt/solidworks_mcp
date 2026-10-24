# check_interference against real SolidWorks: blocks placed to overlap by a
# known amount, to touch, and to stay clear, plus a subassembly overlapping
# one of them; checked in the assembly (volumes and boxes against the
# component boxes, narrowing by id / name / subassembly, touching faces) and
# from a drawing of it (by name, view picking, refusals)
# (docs/notes/architecture.md, "Assemblies + mates"). Run with:
#   py devloop/mcp_client.py run devloop/scenarios/interference.py

import json
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


def insert(file, origin):
    r = mcp.call("insert_components", file=str(file), at=[{"origin_mm": list(origin)}], fixed=False)
    return re.findall(r"^- (component:\d+):", r.text, re.M)[0]


def check(**kw):
    r = mcp.call("check_interference", **kw)
    print(f"    check {kw}: {r.text[:1500]}")
    return r.json


def box(c):
    return c["bbox_mm"]["min"], c["bbox_mm"]["max"]


def overlap(a, b):
    """The intersection of two (min, max) boxes."""
    lo = [max(x, y) for x, y in zip(a[0], b[0])]
    hi = [min(x, y) for x, y in zip(a[1], b[1])]
    return lo, hi


def volume(b):
    v = 1.0
    for l, h in zip(*b):
        v *= h - l
    return v


def close(a, b, tol=1e-3):
    return all(abs(x - y) <= tol for x, y in zip(a, b))


def shifted(axis, by):
    v = [0.0, 0.0, 0.0]
    v[axis] = by
    return v


def involves(item, who):
    """Whether `who` (an id, or a name in a drawing) is one of the interference's components."""
    return any(c == who or c.split(" (")[0] == who for c in item["components"])


def same_volume(got, want):
    return abs(got - want) <= 1e-6 * want + 1e-3


out = mcp.run_dir.resolve()
block = out / "ifr-block.SLDPRT"
sub = out / "ifr-sub.SLDASM"
top = out / "ifr-top.SLDASM"

# ---- Parts ------------------------------------------------------------------------
mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
mcp.call("save_document", path=str(block))
close_all()
# A subassembly with one block in it, at its origin.
mcp.call("create_assembly")
mcp.call("insert_components", file=str(block))
mcp.call("save_document", path=str(sub))
close_all()

# ---- The assembly -------------------------------------------------------------------
mcp.call("create_assembly")
mcp.call("save_document", path=str(top))  # first: saving renames the document, which renews its ids
r = mcp.call("insert_components", file=str(block))
base = re.findall(r"^- (component:\d+):", r.text, re.M)[0]
cs = components()
lo, hi = box(cs[base])
size = [h - l for l, h in zip(lo, hi)]
print("    block size:", size)
assert sorted(round(s, 3) for s in size) == [10, 20, 40], size
# Overlapping: half along axis 0, three quarters along axis 1.
over = insert(block, [size[0] / 2, size[1] / 4, 0])
# Touching the base (and the overlapping one) on its axis-2 face, and far away.
touch = insert(block, shifted(2, size[2]))
far = insert(block, shifted(2, 10 * size[2]))
# The subassembly's block overlapping the base by half along axis 0 (and
# touching `over`'s face there).
subc = insert(sub, shifted(0, -size[0] / 2))
mcp.call("save_document")
cs = components()
nested = next(i for i, c in cs.items() if c.get("parent") == subc)
print("    components:", json.dumps(cs)[:2000])
want_over = overlap(box(cs[base]), box(cs[over]))
want_sub = overlap(box(cs[base]), box(cs[nested]))
print("    expected:", want_over, volume(want_over), want_sub, volume(want_sub))

# ---- In the assembly ------------------------------------------------------------------
j = check()
items = j["interferences"]
assert j["checked_components"] == 5, j
assert len(items) == 2, items
assert not any(i.get("touching") for i in items), items
# Biggest first: the subassembly's block overlaps by half, `over` by 3/8.
first, second = items
assert involves(first, base) and involves(first, nested), first
assert involves(second, base) and involves(second, over), second
assert same_volume(first["volume_mm3"], volume(want_sub)), (first, volume(want_sub))
assert same_volume(second["volume_mm3"], volume(want_over)), (second, volume(want_over))
# The interference body's box is in assembly space.
assert close(second["bbox_mm"]["min"], want_over[0]) and close(second["bbox_mm"]["max"], want_over[1]), second
assert close(first["bbox_mm"]["min"], want_sub[0]) and close(first["bbox_mm"]["max"], want_sub[1]), first
assert "2 interferences" in j["summary"], j
assert {c["id"] for c in j["components"]} == {base, over, nested}, j["components"]

# Narrowed: by id, by a subassembly (covers its block), by name.
assert len(check(components=[over])["interferences"]) == 1
assert check(components=[far])["interferences"] == []
assert "no interference involving" in check(components=[far])["summary"]
only_sub = check(components=[subc])["interferences"]
assert len(only_sub) == 1 and involves(only_sub[0], nested), only_sub
by_name = check(components=[cs[over]["name"]])["interferences"]
assert len(by_name) == 1 and involves(by_name[0], over), by_name

# Touching faces: `touch` against the base and `over`, `over` against the sub's block.
j = check(include_touching=True)
touching = [i for i in j["interferences"] if i.get("touching")]
print("    touching:", touching)
assert len(j["interferences"]) == 2 + len(touching), j
assert any(involves(i, touch) and involves(i, base) for i in touching), touching
assert not any(involves(i, far) for i in j["interferences"]), j
# touch-base, touch-over, touch-sub's block, over-sub's block.
assert len(touching) == 4, touching
# The option doesn't stick (the manager keeps its options between checks).
j = check()
assert len(j["interferences"]) == 2 and not any(i.get("touching") for i in j["interferences"]), j

# Ending the check marks a saved assembly modified; the reply says so.
mcp.call("save_document")
j = check()
assert "modified" in j, j
j = check()
assert "modified" not in j, j  # already marked

# The subassembly as one solid: its block's overlap is reported against it.
j = check(treat_subassemblies_as_components=True)
print("    subassemblies as components:", j)
assert len(j["interferences"]) == 2, j

# Refusals.
r = mcp.call("check_interference", view="Drawing View1", expect_error=True)
assert "only goes with a drawing" in r.text, r.text
r = mcp.call("check_interference", components=["face:1"], expect_error=True)
assert r.is_error, r.text
assert not (open_dialogs := sw_dialogs()), open_dialogs
# Nothing moved.
after = components()
assert all(close(box(after[i])[0], box(cs[i])[0]) for i in cs), after

# ---- From a drawing -----------------------------------------------------------------
# Closed first: the drawing's assemblies are then loaded only as its
# references (not visible), as when a drawing is opened by itself.
close_all()
mcp.call("create_drawing", paper_size="A3")
mcp.call("create_drawing_view", model=str(top), at_mm=[150, 200], scale="1:2")
views = {v["name"]: v for s in mcp.call("inspect_drawing").json["sheets"] for v in s["views"]}
top_view = [n for n, v in views.items() if "ifr-top" in v.get("model", "")][0]
j = check()
assert j["view"] == top_view and j["assembly"].lower().startswith("ifr-top"), j
assert len(j["interferences"]) == 2 and "components" not in j, j
assert all(not c.startswith("component:") for i in j["interferences"] for c in i["components"]), j
assert same_volume(j["interferences"][1]["volume_mm3"], volume(want_over)), j
by_name = check(components=[cs[over]["name"]])["interferences"]
assert len(by_name) == 1, by_name
r = mcp.call("check_interference", components=[over], view=top_view, expect_error=True)
assert "by name" in r.text, r.text
r = mcp.call("check_interference", view="Nope", expect_error=True)
assert "no view named" in r.text, r.text

# A second assembly and a part on the sheet: the view has to be named.
mcp.call("create_drawing_view", model=str(sub), at_mm=[320, 200], scale="1:1")
mcp.call("create_drawing_view", model=str(block), at_mm=[320, 80], scale="1:1")
views = {v["name"]: v for s in mcp.call("inspect_drawing").json["sheets"] for v in s["views"]}
sub_view = [n for n, v in views.items() if "ifr-sub" in v.get("model", "")][0]
block_view = [n for n, v in views.items() if "ifr-block" in v.get("model", "")][0]
r = mcp.call("check_interference", expect_error=True)
assert "2 assemblies" in r.text, r.text
assert len(check(view=top_view)["interferences"]) == 2
assert check(view=sub_view)["interferences"] == []
r = mcp.call("get_active_document_name")
assert "drawing" in r.text.lower(), r.text  # still the drawing after showing / hiding the assembly
r = mcp.call("check_interference", view=block_view, expect_error=True)
assert "shows a part" in r.text, r.text
assert not (open_dialogs := sw_dialogs()), open_dialogs

# ---- A part isn't an assembly -------------------------------------------------------
close_all()
mcp.call("open_document", path=str(block))
r = mcp.call("check_interference", expect_error=True)
assert "is a part" in r.text, r.text
close_all()

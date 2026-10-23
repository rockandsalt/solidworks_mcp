# replace_components against real SolidWorks: a block swapped for a longer
# revision of itself (a Save As copy: same faces, so every mate should come
# through), then for an unrelated disc (face mates can't), every instance of
# a file at once, and refusals that must change nothing
# (docs/notes/architecture.md, "Assemblies + mates"). Run with:
#   py devloop/mcp_client.py run devloop/scenarios/replacing.py

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


def replaced_ids(r):
    """The new component ids a replace_components reply lists, in order."""
    return re.findall(r"^- .* -> (component:\d+):", r.text, re.M)


def close(a, b, tol=1e-3):
    return all(abs(x - y) <= tol for x, y in zip(a, b))


def size(c):
    lo, hi = c["bbox_mm"]["min"], c["bbox_mm"]["max"]
    return [h - l for l, h in zip(lo, hi)]


def side_face(c, ny):
    faces = mcp.call("inspect_model", what="faces", within=c, surface_type="plane").json["faces"]
    hits = [f["id"] for f in faces if abs(f["normal"][1] - ny) < 1e-6]
    assert len(hits) == 1, faces
    return hits[0]


out = mcp.run_dir.resolve()
block = out / "rep-block.SLDPRT"
long = out / "rep-long.SLDPRT"
disc = out / "rep-disc.SLDPRT"
sub = out / "rep-sub.SLDASM"
top = out / "rep-top.SLDASM"

# ---- Parts ------------------------------------------------------------------------
# The block: 40 x 20 on Front, 10 deep along +X (Front faces +X on this machine).
mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
mcp.call("save_document", path=str(block))
# Its revision: saved under a new name (the same faces inside), then 30 deep.
mcp.call("save_document", path=str(long))
mcp.call("edit_extrude", feature="Boss-Extrude1", depth_mm=30)
mcp.call("save_document")
# Something else entirely: a disc, 30 across, 10 deep.
mcp.call("create_part")
mcp.call("sketch_circle", center=[0, 0], diameter_mm=30)
mcp.call("extrude_sketch", depth_mm=10)
mcp.call("save_document", path=str(disc))
close_all()
# A subassembly with one block in it.
mcp.call("create_assembly")
mcp.call("insert_components", file=str(block))
mcp.call("save_document", path=str(sub))
close_all()

# ---- The assembly: a fixed block and a mated one -----------------------------------
mcp.call("create_assembly")
mcp.call("save_document", path=str(top))  # first: saving renames the document, which renews its ids
r = mcp.call("insert_components", file=str(block))
base = re.findall(r"^- (component:\d+):", r.text, re.M)[0]
r = mcp.call("insert_components", file=str(block), at=[{"origin_mm": [0, 60, 0]}])
mover = re.findall(r"^- (component:\d+):", r.text, re.M)[0]
planes = {c: {f["name"]: f["id"] for f in mcp.call("inspect_model", what="features", within=c).json["features"]}
          for c in (base, mover)}
mcp.call("add_mate", kind="coincident", entities=[planes[base]["Top Plane"], planes[mover]["Top Plane"]])
mcp.call("add_mate", kind="coincident", alignment="anti_aligned",
         entities=[side_face(base, 1), side_face(mover, -1)])
mcp.call("add_mate", kind="parallel", entities=[planes[base]["Right Plane"], planes[mover]["Right Plane"]])
before_mates = mates()
print("    mates:", before_mates)
assert len(before_mates) == 3 and all(m.get("error") is None for m in before_mates.values()), before_mates
cs = components()
assert close(cs[mover]["origin_mm"][1:2], [40]), cs[mover]  # its -Y side on the base's +Y side

# ---- The revision: every mate comes through --------------------------------------
r = mcp.call("replace_components", components=[mover], file=str(long))
print("    revision:", r.text)
assert r.text.startswith("replaced 1 component with rep-long.SLDPRT"), r.text
assert "Mates: 3 of 3 re-attached" in r.text and "Unresolved" not in r.text, r.text
(mover,) = replaced_ids(r)
cs = components()
assert len(cs) == 2 and cs[mover]["file"] == "rep-long.SLDPRT", cs
assert close(size(cs[mover]), [30, 40, 20]), cs[mover]  # 30 deep now
assert close(cs[mover]["origin_mm"][1:2], [40]), cs[mover]
assert not cs[mover]["fixed"] and cs[base]["fixed"], cs
m = mates()
assert set(m) == set(before_mates) and all(x.get("error") is None for x in m.values()), m

# ---- The disc: the face mate has nothing to hold on to ------------------------------
r = mcp.call("replace_components", components=[mover], file=str(disc))
print("    disc:", r.text)
assert r.text.startswith("replaced 1 component with rep-disc.SLDPRT"), r.text
assert "Unresolved mates" in r.text, r.text
(mover,) = replaced_ids(r)
assert components()[mover]["file"] == "rep-disc.SLDPRT"
assert not (open_dialogs := sw_dialogs()), open_dialogs
print("    mates now:", mates())
diag = mcp.call("diagnose_model")
print("    diagnose:", diag.text[:2000])

# ---- Every instance of a file ------------------------------------------------------
r = mcp.call("insert_components", copy_of=base, at=[{"origin_mm": [0, -100, 0]}, {"origin_mm": [0, -200, 0]}])
copies = re.findall(r"^- (component:\d+):", r.text, re.M)
assert len(copies) == 2, r.text
r = mcp.call("replace_components", components=[base], file=str(long), all_instances=True)
print("    all instances:", r.text)
assert r.text.startswith("replaced 3 components with rep-long.SLDPRT"), r.text
new_ids = replaced_ids(r)
assert len(new_ids) == 3, r.text
cs = components()
assert len(cs) == 4, cs
assert sum(1 for c in cs.values() if c["file"] == "rep-long.SLDPRT") == 3, cs
assert sum(1 for c in cs.values() if c["file"] == "rep-disc.SLDPRT") == 1, cs
assert [cs[i]["fixed"] for i in new_ids].count(True) == 1, [cs[i] for i in new_ids]
count = len(cs)

# ---- Refusals: nothing changes -----------------------------------------------------
r = mcp.call("insert_components", file=str(sub), at=[{"origin_mm": [0, 0, 200]}])
cs = components()
count = len(cs)
nested = next(i for i, c in cs.items() if c.get("parent"))
r = mcp.call("replace_components", components=[nested], file=str(disc), expect_error=True)
assert "inside the subassembly" in r.text, r.text
r = mcp.call("replace_components", components=[new_ids[0]], file=str(long), expect_error=True)
assert "already" in r.text, r.text
r = mcp.call("replace_components", components=[new_ids[0]], file=str(block), configuration="Nope", expect_error=True)
assert "no configuration" in r.text, r.text
mcp.call("save_document")
r = mcp.call("replace_components", components=[new_ids[0]], file=str(top), expect_error=True)
assert "itself" in r.text, r.text
r = mcp.call("replace_components", components=["face:1"], file=str(block), expect_error=True)
assert "component ids" in r.text, r.text
r = mcp.call("replace_components", components=[new_ids[0]], file=str(out / "nope.SLDPRT"), expect_error=True)
assert "no file" in r.text, r.text
cs = components()
assert len(cs) == count and sum(1 for c in cs.values() if c["file"] == "rep-long.SLDPRT") == 3, cs

# A whole subassembly can be replaced by a part.
sub1 = next(i for i, c in cs.items() if c.get("subassembly"))
r = mcp.call("replace_components", components=[sub1], file=str(block))
print("    subassembly:", r.text)
assert r.text.startswith("replaced 1 component with rep-block.SLDPRT"), r.text
cs = components()
assert len(cs) == count - 1 and not any(c.get("subassembly") for c in cs.values()), cs
assert not (open_dialogs := sw_dialogs()), open_dialogs

# ---- Saved and reopened -------------------------------------------------------------
mcp.call("save_document")
files = sorted(c["file"] for c in cs.values())
close_all()
mcp.call("open_document", path=str(top))
assert sorted(c["file"] for c in components().values()) == files, components()
assert not (open_dialogs := sw_dialogs()), open_dialogs
close_all()

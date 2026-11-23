# The assembly tree and its search (inspect_model what="tree", search,
# properties, where), feature search through every component, a component's
# sketch through inspect_sketch, mate entities and folders, and
# check_clearance (docs/notes/assembly-tree.md). Run with:
#   py devloop/mcp_client.py run devloop/scenarios/asmtree.py
#
# Layout (the part's box is x 0..10 on this machine, Front faces +X):
#   tree-sub: block at the origin, three pins at y 100 / 200 / 300
#   top: tree-sub (fixed), block A at x 13 (3 mm from the sub's block),
#        block B at x 23 (touching A), mated face to face where they touch.

import subprocess
from pathlib import Path

SWCOM = Path(__file__).resolve().parent.parent / "swcom.ps1"


def com(code: str) -> str:
    script = mcp.run_dir / f"com-{len(list(mcp.run_dir.glob('com-*.ps1'))) + 1}.ps1"
    script.write_text(code, encoding="utf-8")
    p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(SWCOM),
                        "-CodeFile", str(script)], capture_output=True, text=True, timeout=150)
    print(f"    com {script.name}: {p.stdout.strip()}")
    assert p.returncode == 0, f"{script.name} failed: {p.stderr.strip()}"
    return p.stdout.strip()


def close_all():
    com("Com $sw CloseAllDocuments @($true) | Out-Null")


def close(a, b, tol=1e-3):
    return all(abs(x - y) <= tol for x, y in zip(a, b))


def inspect(**kw):
    return mcp.call("inspect_model", **kw).json


out = mcp.run_dir.resolve()
block = out / "tree-block.SLDPRT"
pin = out / "tree-pin.SLDPRT"
sub = out / "tree-sub.SLDASM"
top = out / "tree-top.SLDASM"

# ---- Parts with properties ----------------------------------------------------
mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
mcp.call("custom_properties", set={"Vendor": "Acme", "Description": "Block 40x20"})
mcp.call("save_document", path=str(block))

mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=10, height_mm=10)
mcp.call("extrude_sketch", depth_mm=10)
mcp.call("custom_properties", set={"Vendor": "Globex", "Description": "Pin"})
mcp.call("save_document", path=str(pin))
close_all()

# ---- The subassembly ----------------------------------------------------------
mcp.call("create_assembly")
mcp.call("insert_components", file=str(block))
mcp.call("insert_components", file=str(pin), at=[
    {"origin_mm": [0, 100, 0]}, {"origin_mm": [0, 200, 0]}, {"origin_mm": [0, 300, 0]}])
mcp.call("save_document", path=str(sub))
close_all()

# ---- The top assembly ---------------------------------------------------------
mcp.call("create_assembly")
mcp.call("insert_components", file=str(sub))
mcp.call("insert_components", file=str(block), at=[{"origin_mm": [13, 0, 0]}, {"origin_mm": [23, 0, 0]}])
mcp.call("save_document", path=str(top))

cs = inspect(what="components")["components"]
blocks = sorted((c for c in cs if c.get("file") == "tree-block.SLDPRT" and not c.get("parent")),
                key=lambda c: c["origin_mm"][0])
a, b = blocks
print("    blocks:", a["name"], a["origin_mm"], b["name"], b["origin_mm"])
assert close(a["origin_mm"], [13, 0, 0]) and close(b["origin_mm"], [23, 0, 0]), (a, b)

# Face to face where A and B touch (x = 23): A's +X face, B's -X face.
fa = inspect(what="faces", within=a["id"], normal=[1, 0, 0])["faces"][0]["id"]
fb = inspect(what="faces", within=b["id"], normal=[-1, 0, 0])["faces"][0]["id"]
mcp.call("add_mate", kind="coincident", entities=[fa, fb])

# ---- The tree -----------------------------------------------------------------
t = inspect(what="tree")
print("    summary:", t["summary"])
s = t["summary"]
assert s["components"] == 7 and s["levels"] == 2, s
files = {(f["file"], f["count"]) for f in s["files"]}
assert ("tree-pin.SLDPRT", 3) in files and ("tree-block.SLDPRT", 3) in files, files
assert ("tree-sub.SLDASM", 1) in files, files
top_nodes = t["tree"]
print("    top:", top_nodes)
# The two loose blocks are alike: one node with count 2; the subassembly has
# its block and one node for the three pins.
grouped = [n for n in top_nodes if n.get("count") == 2]
assert len(grouped) == 1 and grouped[0]["file"] == "tree-block.SLDPRT", top_nodes
subnode = next(n for n in top_nodes if n.get("file") == "tree-sub.SLDASM")
assert subnode.get("fixed") is True, subnode
pins = [n for n in subnode["children"] if n.get("file") == "tree-pin.SLDPRT"]
assert len(pins) == 1 and pins[0]["count"] == 3, subnode
for n in top_nodes + subnode["children"]:
    for flag in ("virtual", "envelope", "toolbox", "pattern_instance", "mirror_instance"):
        assert flag not in n, (flag, n)

t1 = inspect(what="tree", depth=1)
subnode = next(n for n in t1["tree"] if n.get("file") == "tree-sub.SLDASM")
assert "children" not in subnode and subnode["children_not_shown"] == 4, subnode

found = inspect(what="tree", search="pin")
print("    search pin:", found["tree"])
assert found["matched"] == 3 and len(found["tree"]) == 1, found
assert found["tree"][0]["children"][0]["count"] == 3, found

# ---- Properties ---------------------------------------------------------------
globex = inspect(what="components", where={"Vendor": "globex"})["components"]
print("    Vendor=Globex:", [(c["name"], c.get("properties")) for c in globex])
assert len(globex) == 3 and all(c["properties"]["Vendor"] == "Globex" for c in globex), globex
everything = inspect(what="components", properties=["*"], search="tree-block")["components"]
assert len(everything) == 3, everything
assert all(c["properties"].get("Description") == "Block 40x20" for c in everything), everything
paged = inspect(what="components", search="*", limit=2)
assert len(paged["components"]) == 2 and "offset=2" in paged["page_note"], paged
none = inspect(what="components", search="nothing-like-this")
assert none["components"] == [] and "note" in none, none

# ---- Features in every component ---------------------------------------------
sketches = inspect(what="features", search="Sketch*")
print("    sketches:", [(f["name"], f.get("component")) for f in sketches["features"]])
assert len(sketches["features"]) == 6, sketches  # one in each of the 6 parts
assert all(f.get("component") for f in sketches["features"]), sketches
in_a = next(f for f in sketches["features"] if f["component"] == a["id"])

# A component's sketch by its id. SolidWorks gives its frame in the
# component's coordinates; the server places it: A sits at x 13.
own = mcp.call("inspect_sketch", sketch_name=in_a["id"]).json
print("    A's sketch:", own.get("component"), own.get("sketch_axes"))
assert own["component"]["id"] == a["id"], own
assert close(own["sketch_axes"]["origin_mm"], [13, 0, 0]), own["sketch_axes"]
lines = [s for s in own["segments"] if not s.get("construction")]
assert len(lines) == 4 and own["contours"]["closed"] == 1, own

# ---- Mates: entities and folders ----------------------------------------------
mates = inspect(what="mates")["mates"]
print("    mates:", mates)
mate = next(m for m in mates if m["type"] == "coincident")
kinds = [e.get("kind") for e in mate["entities"]]
assert kinds == ["face", "face"], mate
assert {e["id"] for e in mate["entities"]} == {fa, fb}, (mate, fa, fb)
assert "folder" not in mate, mate

com(f"""$a = Com $sw ActiveDoc -Get
$none = [Runtime.InteropServices.DispatchWrapper]::new([object]$null)
if (-not (Com (Com $a Extension -Get) SelectByID2 @('{mate["name"]}', 'MATE', 0.0, 0.0, 0.0, $false, 0, $none, 0))) {{ throw 'no mate' }}
$folder = Com (Com $a FeatureManager -Get) InsertFeatureTreeFolder2 @(2)
if (-not $folder) {{ throw 'no folder' }}
Com $folder Name -Get""")
mate = next(m for m in inspect(what="mates")["mates"] if m["name"] == mate["name"])
print("    mate in folder:", mate.get("folder"))
assert mate.get("folder"), mate

# ---- Clearance ------------------------------------------------------------------
c = mcp.call("check_clearance", clearance_mm=5).json
print("    clearance 5:", c)
closer = c["closer_than_clearance"]
assert len(closer) == 1 and abs(closer[0]["gap_mm"] - 3) < 1e-3, c
assert a["id"] in " ".join(closer[0]["components"]), c
touching = c.get("touching_or_overlapping", [])
assert len(touching) == 1 and b["id"] in " ".join(touching[0]["components"]), c
c2 = mcp.call("check_clearance", clearance_mm=2).json
assert c2["closer_than_clearance"] == [], c2
only_b = mcp.call("check_clearance", clearance_mm=5, components=[b["id"]]).json
assert only_b["closer_than_clearance"] == [] and len(only_b["touching_or_overlapping"]) == 1, only_b

# A part is refused.
mcp.call("create_part")
r = mcp.call("check_clearance", clearance_mm=5, expect_error=True)
assert r.is_error and "assembly" in r.text, r.text
r = mcp.call("inspect_model", what="tree").json
assert "note" in r, r

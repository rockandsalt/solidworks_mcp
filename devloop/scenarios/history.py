# set_suppression in a part / rollback / reorder_feature against real
# SolidWorks (docs/notes/editing.md): a 40 x 20 x 10 block with an 8 mm hole;
# the hole suppressed and unsuppressed (volume); the bar rolled back before
# the hole (volume, rolled_back in inspect_model), a 4 mm hole made while
# rolled back going in at the bar, the bar after the block, forward by the
# rolled-back hole's id, to the end; refusals; saved and reopened rolled
# back; the small hole moved after the big one and back by id, a cut refused
# above the block it's built on, each checked by tree order and volume.
# Run with:
#   py devloop/mcp_client.py run devloop/scenarios/history.py

import math

out = mcp.run_dir.resolve()
block = out / "history-block.SLDPRT"


def features():
    return mcp.call("inspect_model", what="features").json["features"]


def feature(name):
    return next(f for f in features() if f["name"] == name)


def volume():
    return mcp.call("inspect_model", what="bodies").json["bodies"][0]["volume_mm3"]


def order(*names):
    listed = [f["name"] for f in features()]
    return [listed.index(n) for n in names]


def rolled_back():
    return [f["name"] for f in features() if f.get("rolled_back")]


def close(a, b, tol=1e-2):
    return abs(a - b) <= tol


BLOCK = 40 * 20 * 10
BIG = math.pi * 4**2 * 10
SMALL = math.pi * 2**2 * 10

# ---- The part ------------------------------------------------------------------
mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
caps = mcp.call("inspect_model", what="faces", within="Boss-Extrude1", surface_type="plane").json["faces"]
cap = max(caps, key=lambda f: f["area_mm2"])  # a 40 x 20 face
mcp.call("sketch", shape="circle", on=cap["id"], center=[0, 0], diameter_mm=8)
mcp.call("extrude_sketch", cut=True, through_all=True)
assert close(volume(), BLOCK - BIG), volume()
assert rolled_back() == [], rolled_back()

# ---- Suppressing a feature of a part -------------------------------------------
r = mcp.call("set_suppression", items=["Cut-Extrude1"], suppressed=True)
print("    ", r.text)
assert close(volume(), BLOCK), volume()
assert feature("Cut-Extrude1").get("suppressed"), feature("Cut-Extrude1")
r = mcp.call("set_suppression", items=["Cut-Extrude1"], suppressed=False)
assert close(volume(), BLOCK - BIG), volume()

# ---- Rollback ------------------------------------------------------------------
r = mcp.call("rollback", before="Cut-Extrude1")
print("    ", r.text)
assert r.text.startswith("moved the rollback bar to before Cut-Extrude1.") and "Sketch2 (sketch), Cut-Extrude1 (" in r.text, r.text
assert close(volume(), BLOCK), volume()
assert "Cut-Extrude1" in rolled_back() and "Boss-Extrude1" not in rolled_back(), rolled_back()
big_id = feature("Cut-Extrude1")["id"]

# A feature made while rolled back goes in at the bar.
caps = mcp.call("inspect_model", what="faces", within="Boss-Extrude1", surface_type="plane").json["faces"]
cap = max(caps, key=lambda f: f["area_mm2"])  # a 40 x 20 face
mcp.call("sketch", shape="circle", on=cap["id"], center=[8, 0], diameter_mm=4)
r = mcp.call("extrude_sketch", cut=True, through_all=True)
print("    ", r.text)
small = next(f["name"] for f in features() if f["name"].startswith("Cut-Extrude") and f["name"] != "Cut-Extrude1")
assert close(volume(), BLOCK - SMALL), volume()
a, b = order(small, "Cut-Extrude1")
assert a < b, (small, a, b)
assert "Cut-Extrude1" in rolled_back() and small not in rolled_back(), rolled_back()

r = mcp.call("rollback", after="Boss-Extrude1")
print("    ", r.text)
assert close(volume(), BLOCK), volume()
assert small in rolled_back() and "Cut-Extrude1" in rolled_back(), rolled_back()

# Forward again by the id the big hole had (a rolled-back feature's id).
r = mcp.call("rollback", after=big_id)
print("    ", r.text)
assert r.text.startswith("rolled forward to the end") or "Cut-Extrude1" not in r.text, r.text
assert close(volume(), BLOCK - BIG - SMALL), volume()
assert rolled_back() == [], rolled_back()
r = mcp.call("rollback")
assert r.text.startswith("nothing was rolled back"), r.text

# Refusals.
mcp.call("rollback", before="Nope", expect_error=True)
mcp.call("rollback", before="Boss-Extrude1", after="Cut-Extrude1", expect_error=True)
mcp.call("rollback", before=cap["id"], expect_error=True)
assert rolled_back() == [], rolled_back()

# Saved rolled back, reopened rolled back.
mcp.call("rollback", before="Cut-Extrude1")
mcp.call("save_document", path=str(block))
mcp.call("close_document")
mcp.call("open_document", path=str(block))
assert rolled_back() and "Cut-Extrude1" in rolled_back(), rolled_back()
assert close(volume(), BLOCK - SMALL), volume()
mcp.call("rollback")
assert close(volume(), BLOCK - BIG - SMALL), volume()

# ---- Reorder ---------------------------------------------------------------------
r = mcp.call("reorder_feature", feature=small, after="Cut-Extrude1")
print("    ", r.text)
assert r.text.startswith(f"moved {small} (") and "now last, after Cut-Extrude1." in r.text, r.text
a, b = order("Cut-Extrude1", small)
assert a < b, (a, b)
assert close(volume(), BLOCK - BIG - SMALL), volume()

small_id = feature(small)["id"]
r = mcp.call("reorder_feature", feature=small_id, before="Cut-Extrude1")
print("    ", r.text)
assert "now between Boss-Extrude1 and Cut-Extrude1" in r.text, r.text
a, b = order(small, "Cut-Extrude1")
assert a < b, (a, b)

# A cut can't go above the block it's cut from; the tree stays as it was.
before = [f["name"] for f in features()]
r = mcp.call("reorder_feature", feature="Cut-Extrude1", before="Boss-Extrude1", expect_error=True)
print("    ", r.text)
assert "Boss-Extrude1" in r.text, r.text
assert [f["name"] for f in features()] == before
mcp.call("reorder_feature", feature="Cut-Extrude1", before="Cut-Extrude1", expect_error=True)
mcp.call("reorder_feature", feature="Cut-Extrude1", expect_error=True)
mcp.call("reorder_feature", feature="Front Plane", after="Cut-Extrude1", expect_error=True)
mcp.call("reorder_feature", feature="Nope", after="Cut-Extrude1", expect_error=True)
assert close(volume(), BLOCK - BIG - SMALL), volume()
mcp.call("save_document")

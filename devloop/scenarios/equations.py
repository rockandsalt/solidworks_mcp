# equations / design_table against real SolidWorks (docs/notes/equations.md):
# a block with a hole; global variables driving the depth through a chain,
# changed, written with units, and the refusals (a global still in use, an
# unknown name, an equation SolidWorks refuses -- leaving everything as it
# was); set_dimension refused on a driven dimension; then a design table
# making configurations with their own depth, hole suppression, global
# variable and description, read back and checked by body box and volume in
# each, its refusals, a value SolidWorks objects to, rewritten without a
# row, saved and reopened, and deleted. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/equations.py

import json

out = mcp.run_dir.resolve()
block = out / "eq-block.SLDPRT"


def close(a, b, tol=1e-3):
    return abs(a - b) <= tol


def body():
    b = mcp.call("inspect_model", what="bodies").json["bodies"][0]
    return b["bbox_mm"], b["volume_mm3"]


def json_of(r):
    # The reply's last line; a sentence may come before it.
    return json.loads(r.text.splitlines()[-1])


def listing():
    return {e["name"]: e for e in json_of(mcp.call("equations"))["equations"]}


def configs():
    return {c["name"]: c for c in mcp.call("configurations").json["configurations"]}


def feature(name):
    fs = mcp.call("inspect_model", what="features").json["features"]
    return next(f for f in fs if f["name"] == name)


# ---- The part: 40 x 20 block, 10 deep, an 8 mm hole through it ------------------
mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
caps = mcp.call("inspect_model", what="faces", within="Boss-Extrude1", surface_type="plane").json["faces"]
mcp.call("sketch_circle", on=caps[0]["id"], center=[0, 0], diameter_mm=8)
mcp.call("cut_extrude_sketch", through_all=True)
box, holed = body()
depth_axis = next(i for i in range(3) if close(box["max"][i] - box["min"][i], 10))


def depth():
    b, _ = body()
    return b["max"][depth_axis] - b["min"][depth_axis]


# ---- Equations and global variables ---------------------------------------------
r = mcp.call("equations")
assert r.text.startswith("the document has no equations"), r.text
assert "millimeters (the document's length unit)" in r.text, r.text

r = mcp.call("equations", set=[{"name": "Width", "expression": "50"},
                               {"name": "Depth", "expression": "\"Width\" / 5"},
                               {"name": "D1@Boss-Extrude1", "expression": "\"Depth\""}])
print("    ", r.text)
assert r.text.startswith('set "Width" = 50; "Depth" = "Width" / 5; "D1@Boss-Extrude1" = "Depth"; the model was rebuilt.'), r.text
eq = {e["name"]: e for e in json_of(r)["equations"]}
assert eq["Width"] == {"name": "Width", "equation": '"Width" = 50', "value": 50.0, "global_variable": True}, eq
assert eq["Depth"]["value"] == 10.0 and eq["D1@Boss-Extrude1"]["value_mm"] == 10.0, eq
assert close(depth(), 10), depth()

mcp.call("equations", set=[{"name": "width", "expression": "100"}])  # names ignore case
eq = listing()
assert list(eq) == ["Width", "Depth", "D1@Boss-Extrude1"], eq  # replaced in place
assert eq["D1@Boss-Extrude1"]["value_mm"] == 20.0, eq
assert close(depth(), 20), depth()

# Written with units, whatever the document's.
mcp.call("equations", set=[{"name": "Width", "expression": "4in"}])
assert close(listing()["D1@Boss-Extrude1"]["value_mm"], 20.32), listing()
assert close(depth(), 20.32), depth()
mcp.call("equations", set=[{"name": "Width", "expression": "100"}])

# A driven dimension can't be set directly: the equation would put it back.
r = mcp.call("set_dimension", name="D1@Boss-Extrude1", value_mm=33, expect_error=True)
assert "driven by the equation" in r.text, r.text
assert close(depth(), 20), depth()

# Refusals, each leaving the equations as they were.
before = listing()
r = mcp.call("equations", delete=["Depth"], expect_error=True)
assert '"Depth" is used by "D1@Boss-Extrude1"' in r.text, r.text
r = mcp.call("equations", set=[{"name": "A", "expression": "\"Nope\" * 2"}], expect_error=True)
assert "no global variable" in r.text, r.text
r = mcp.call("equations", delete=["Nope"], expect_error=True)
assert 'it has "Width", "Depth", "D1@Boss-Extrude1"' in r.text, r.text
# SolidWorks' own refusal, after a change it took: all or nothing.
r = mcp.call("equations", set=[{"name": "Width", "expression": "60"},
                               {"name": "D9@Sketch9", "expression": "3"}], expect_error=True)
print("    ", r.text)
assert 'SolidWorks refused the equation "D9@Sketch9" = 3 ("D9@Sketch9" is neither' in r.text, r.text
r = mcp.call("equations", set=[{"name": "B", "expression": "= = 3"}], expect_error=True)
assert "check the syntax" in r.text, r.text
assert listing() == before, (listing(), before)
assert close(depth(), 20), depth()

# Deleting the chain together frees the dimension again.
r = mcp.call("equations", delete=["D1@Boss-Extrude1", "Depth"])
assert r.text.startswith('deleted the equations of "D1@Boss-Extrude1", "Depth"; the model was rebuilt.'), r.text
assert list(listing()) == ["Width"]
mcp.call("set_dimension", name="D1@Boss-Extrude1", value_mm=10)
assert close(depth(), 10), depth()
mcp.call("equations", set=[{"name": "Width", "expression": "50"}])

# ---- Design table ------------------------------------------------------------------
r = mcp.call("design_table")
assert r.text.startswith("the document has no design table."), r.text

r = mcp.call("design_table", action="write", columns=["D1@Nope"], rows=[{"configuration": "X", "values": [1]}],
             expect_error=True)
assert "no dimension of the document" in r.text, r.text
r = mcp.call("design_table", action="write", columns=["$VALUE@Nope@equations"], rows=[{"configuration": "X"}],
             expect_error=True)
assert 'no global variable "Nope"' in r.text, r.text
r = mcp.call("design_table", action="write", columns=["$STATE@Nope"], rows=[{"configuration": "X"}],
             expect_error=True)
assert 'no feature "Nope"' in r.text, r.text
assert mcp.call("design_table").text.startswith("the document has no")
assert list(configs()) == ["Default"]

columns = ["D1@Boss-Extrude1", "$STATE@Cut-Extrude1", "$VALUE@Width@equations", "$DESCRIPTION"]
r = mcp.call("design_table", action="write", columns=columns, rows=[
    {"configuration": "Default", "values": [10, "U", 50, "base"]},
    {"configuration": "Long", "values": [30, "U", 60]},
    {"configuration": "Plain", "values": [15, "S"]},
])
print("    ", r.text)
assert "SolidWorks created the configurations \"Long\", \"Plain\"." in r.text, r.text
assert "dialogs" not in r.text and "problem" not in r.text, r.text
t = json_of(r)
assert t["columns"] == columns, t
assert t["rows"][1] == {"configuration": "Long", "values": {
    "D1@Boss-Extrude1": "30", "$STATE@Cut-Extrude1": "U", "$VALUE@Width@equations": "60"}}, t
cs = configs()
assert set(cs) == {"Default", "Long", "Plain"}, cs
assert cs["Default"].get("description") == "base", cs
assert "Cut-Extrude1" in cs["Plain"].get("suppressed_features", []), cs

mcp.call("configurations", action="activate", name="Long")
assert close(depth(), 30), depth()
assert listing()["Width"]["value"] == 60.0, listing()
mcp.call("configurations", action="activate", name="Plain")
assert close(depth(), 15), depth()
assert feature("Cut-Extrude1").get("suppressed")
_, solid = body()
assert close(solid, 15 * 40 * 20, 1), solid
mcp.call("configurations", action="activate", name="Default")
assert close(depth(), 10) and listing()["Width"]["value"] == 50.0
r = mcp.call("set_dimension", name="D1@Boss-Extrude1", value_mm=12, expect_error=True)
assert "design table" in r.text, r.text

# A value SolidWorks objects to: what it says comes back, nothing hangs.
r = mcp.call("design_table", action="write", columns=["D1@Boss-Extrude1", "$STATE@Cut-Extrude1"], rows=[
    {"configuration": "Default", "values": [10, "maybe"]},
])
print("    ", r.text)
assert "a feature state is invalid" in r.text and "dialogs" in r.text, r.text

# Rewritten without Long: Long stays, with its values.
r = mcp.call("design_table", action="write", columns=["D1@Boss-Extrude1"], rows=[
    {"configuration": "Default", "values": [10]},
    {"configuration": "Plain", "values": [16]},
])
assert "created" not in r.text and [row["configuration"] for row in json_of(r)["rows"]] == ["Default", "Plain"], r.text
assert "Long" in configs()
mcp.call("configurations", action="activate", name="Plain")
assert close(depth(), 16), depth()
mcp.call("configurations", action="activate", name="Default")

# Kept in the file.
mcp.call("save_document", path=str(block))
mcp.call("close_document")
mcp.call("open_document", path=str(block))
t = json_of(mcp.call("design_table"))
assert t["exists"] and [row["configuration"] for row in t["rows"]] == ["Default", "Plain"], t
assert list(listing()) == ["Width"]

r = mcp.call("design_table", action="delete")
assert r.text.startswith("deleted the design table"), r.text
assert not json_of(mcp.call("design_table"))["exists"]
assert set(configs()) == {"Default", "Long", "Plain"}
mcp.call("set_dimension", name="D1@Boss-Extrude1", value_mm=12)
assert close(depth(), 12), depth()

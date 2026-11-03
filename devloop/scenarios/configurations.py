# configurations / set_suppression / set_dimension per configuration against
# real SolidWorks (docs/notes/configurations.md): a block with a hole; a
# copy and a derived configuration; a depth set in one configuration, by
# SolidWorks' own choice, in all, and in an unknown one (refused); the hole
# suppressed in one configuration and unsuppressed again by the id it had,
# each checked by body box and volume in the active configuration; deleting
# the active one (refused) and a parent (its derived one goes with it); then
# an assembly of two blocks, the second suppressed in one assembly
# configuration and unsuppressed by its id, checked by inspect_model. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/configurations.py

out = mcp.run_dir.resolve()
block = out / "cfg-block.SLDPRT"


def listing():
    r = mcp.call("configurations")
    return r.json


def by_name(listing_json):
    return {c["name"]: c for c in listing_json["configurations"]}


def body():
    b = mcp.call("inspect_model", what="bodies").json["bodies"][0]
    return b["bbox_mm"], b["volume_mm3"]


def feature(name):
    fs = mcp.call("inspect_model", what="features").json["features"]
    return next(f for f in fs if f["name"] == name)


def close(a, b, tol=1e-3):
    return abs(a - b) <= tol


# ---- The part: 40 x 20 block, 10 deep (along +X here), an 8 mm hole through it --
mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
caps = mcp.call("inspect_model", what="faces", within="Boss-Extrude1", surface_type="plane").json["faces"]
mcp.call("sketch_circle", on=caps[0]["id"], center=[0, 0], diameter_mm=8)
mcp.call("cut_extrude_sketch", through_all=True)
box, volume = body()
depth_axis = next(i for i in range(3) if close(box["max"][i] - box["min"][i], 10))
print("    depth runs along axis", depth_axis, "volume", volume)


def depth():
    b, _ = body()
    return b["max"][depth_axis] - b["min"][depth_axis]


assert close(depth(), 10), depth()

j = listing()
assert [c["name"] for c in j["configurations"]] == ["Default"] and j["active"] == "Default", j
assert "dimensions_that_differ" not in j, j

# ---- Creating, activating -------------------------------------------------------
r = mcp.call("configurations", action="create", name="Long")
assert r.text.startswith('created configuration "Long" (a copy') and '"Long" is now the active one' in r.text, r.text
r = mcp.call("configurations", action="create", name="Long-kid", parent="Long", description="kid of Long")
assert "derived from \"Long\"" in r.text, r.text
cs = by_name(listing())
assert cs["Long-kid"].get("derived_from") == "Long" and cs["Long-kid"].get("description") == "kid of Long", cs
assert cs["Long-kid"].get("active"), cs
mcp.call("configurations", action="create", name="long", expect_error=True)  # names ignore case
mcp.call("configurations", action="create", name="X", parent="Nope", expect_error=True)
mcp.call("configurations", action="activate", name="Nope", expect_error=True)
r = mcp.call("configurations", action="activate", name="default")
assert listing()["active"] == "Default", r.text

# ---- Dimension values per configuration -----------------------------------------
r = mcp.call("set_dimension", name="D1@Boss-Extrude1", value_mm=30, configurations=["Long"])
print("    ", r.text)
assert "By configuration: Default = 10 mm, Long = 30 mm, Long-kid = 30 mm." in r.text, r.text
assert close(depth(), 10), depth()  # the active one keeps its value
# SolidWorks' own choice once the value differs: only the active configuration.
r = mcp.call("set_dimension", name="D1@Boss-Extrude1", value_mm=12)
assert "Default = 12 mm, Long = 30 mm, Long-kid = 30 mm" in r.text, r.text
assert close(depth(), 12), depth()
r = mcp.call("set_dimension", name="D1@Boss-Extrude1", value_mm=15, all_configurations=True)
assert "Default = 15 mm, Long = 15 mm, Long-kid = 15 mm" in r.text, r.text
# One value for all again: SolidWorks' choice is every configuration.
r = mcp.call("set_dimension", name="D1@Boss-Extrude1", value_mm=16)
assert "Default = 16 mm, Long = 16 mm, Long-kid = 16 mm" in r.text, r.text
r = mcp.call("set_dimension", name="D1@Boss-Extrude1", value_mm=30, configurations=["long"])
assert "Default = 16 mm, Long = 30 mm, Long-kid = 30 mm" in r.text, r.text
r = mcp.call("set_dimension", name="D1@Boss-Extrude1", value_mm=50, configurations=["Nope"], expect_error=True)
assert "no configuration \"Nope\"" in r.text, r.text
mcp.call("set_dimension", name="D1@Boss-Extrude1", value_mm=50, configurations=["Long"], all_configurations=True,
         expect_error=True)
j = listing()
assert j["dimensions_that_differ"] == [
    {"name": "D1@Boss-Extrude1", "values_mm": {"Default": 16.0, "Long": 30.0, "Long-kid": 30.0}}], j

# ---- Suppression per configuration ------------------------------------------------
_, full = body()
r = mcp.call("set_suppression", items=["Cut-Extrude1"], suppressed=True, configurations=["Long"])
print("    ", r.text)
assert r.text.startswith('suppressed Cut-Extrude1 (feature) in "Long".'), r.text
assert not feature("Cut-Extrude1").get("suppressed")  # Default keeps its hole
cs = by_name(listing())
assert "Cut-Extrude1" in cs["Long"].get("suppressed_features", []), cs
assert "Cut-Extrude1" in cs["Long-kid"].get("suppressed_features", []), cs  # follows its parent
assert "Cut-Extrude1" not in cs["Default"].get("suppressed_features", []), cs

mcp.call("configurations", action="activate", name="Long")
assert close(depth(), 30), depth()
cut = feature("Cut-Extrude1")
assert cut.get("suppressed"), cut
_, solid = body()
assert close(solid, 30 * 40 * 20, 1), solid  # no hole
# Unsuppressed by the id it has while suppressed (doesn't resolve through SolidWorks).
r = mcp.call("set_suppression", items=[cut["id"]], suppressed=False)
assert r.text.startswith('unsuppressed Cut-Extrude1 (feature) in "Long".'), r.text
_, holed = body()
assert holed < solid - 100, (holed, solid)
r = mcp.call("set_suppression", items=[cut["id"]], suppressed=False)
assert "already unsuppressed" in r.text, r.text
# Its absorbed sketch isn't marked suppressed with it (SolidWorks), so nothing else is named.
r = mcp.call("set_suppression", items=["Cut-Extrude1"], suppressed=True)
assert r.text.startswith('suppressed Cut-Extrude1 (feature) in "Long".') and "With them" not in r.text, r.text
assert not feature("Sketch2").get("suppressed")
mcp.call("set_suppression", items=["Front Plane"], suppressed=True, expect_error=True)
mcp.call("set_suppression", items=[caps[0]["id"]], suppressed=True, expect_error=True)
mcp.call("set_suppression", items=["Nope"], suppressed=True, expect_error=True)
mcp.call("set_suppression", items=["Cut-Extrude1"], suppressed=True, configurations=["Nope"], expect_error=True)

# ---- Deleting ------------------------------------------------------------------------
r = mcp.call("configurations", action="delete", name="Long", expect_error=True)
assert "active" in r.text, r.text
mcp.call("configurations", action="activate", name="Default")
r = mcp.call("configurations", action="delete", name="Long")
assert 'and the ones derived from it: "Long-kid"' in r.text, r.text
j = listing()
assert [c["name"] for c in j["configurations"]] == ["Default"], j
assert close(depth(), 16), depth()
mcp.call("save_document", path=str(block))

# ---- An assembly: the second block suppressed in one configuration ----------------
mcp.call("create_assembly")
mcp.call("insert_components", file=str(block))
mcp.call("insert_components", file=str(block), at=[{"origin_mm": [0, 60, 0]}])


def components():
    return {c["name"]: c for c in mcp.call("inspect_model", what="components").json["components"]}


second = components()["cfg-block-2"]["id"]
mcp.call("configurations", action="create", name="One")
r = mcp.call("set_suppression", items=[second], suppressed=True)
print("    ", r.text)
assert r.text.startswith('suppressed cfg-block-2 (component) in "One".'), r.text
assert components()["cfg-block-2"].get("state") == "suppressed"
cs = by_name(listing())
assert cs["One"].get("suppressed_components") == ["cfg-block-2"] and "suppressed_components" not in cs["Default"], cs
assert listing()["active"] == "One"

# In other configurations: activated in turn, then back to the active one.
r = mcp.call("set_suppression", items=["cfg-block-2"], suppressed=True, all_configurations=True)
assert 'suppressed cfg-block-2 (component) in "Default"; it was already suppressed in "One".' in r.text, r.text
assert listing()["active"] == "One"
cs = by_name(listing())
assert cs["Default"].get("suppressed_components") == ["cfg-block-2"], cs
# Unsuppressed by the id it had before (a suppressed component's id doesn't resolve).
r = mcp.call("set_suppression", items=[second], suppressed=False, configurations=["Default"])
assert r.text.startswith('unsuppressed cfg-block-2 (component) in "Default".'), r.text
assert components()["cfg-block-2"].get("state") == "suppressed"  # still in One, the active one
mcp.call("configurations", action="activate", name="Default")
assert components()["cfg-block-2"].get("state") is None
mcp.call("set_suppression", items=["cfg-block-1/whatever"], suppressed=True, expect_error=True)

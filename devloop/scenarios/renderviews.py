# render_views' own view controls (render.rs; architecture.md "GetMesh /
# rendering"): views from any direction, section, hide, transparent, zoom_to,
# all done in mcp_server on the add-in's mesh, nothing changed in SolidWorks;
# and the mesh reused while GetMesh says the geometry is unchanged (an undo,
# hiding a body, another document, moving or hiding a component fetch it
# again). Check the PNGs by eye too. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/renderviews.py

import re


def cache_hits():
    log = (mcp.run_dir / "server.log").read_text(encoding="utf-8", errors="replace")
    return log.count("using the cached mesh")


def render(fresh, **args):
    """render_views; `fresh`: whether the add-in has to tessellate again."""
    before = cache_hits()
    r = mcp.call("render_views", **args)
    assert r.images, r.text
    hit = cache_hits() > before
    assert hit != fresh, f"expected a cache {'miss' if fresh else 'hit'} for {args}"
    triangles = int(r.text.split(" triangles")[0].rsplit(" ", 1)[1])
    span = float(re.search(r"each tile spans ([\d.]+) mm", r.text).group(1))
    return r, triangles, span


def first_id(r):
    return re.findall(r"^- (component:\d+):", r.text, re.M)[0]


# ---- a part: 40 x 20 x 10 block with an 8 mm hole through its middle
mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
caps = mcp.call("inspect_model", what="faces", within="Boss-Extrude1", surface_type="plane").json["faces"]
extrude = caps[0]["normal"]
mcp.call("sketch", shape="circle", on=caps[0]["id"], center=[0, 0], diameter_mm=8)
mcp.call("extrude_sketch", cut=True, through_all=True)
hole = mcp.call("inspect_model", what="faces", within="Cut-Extrude1").json["faces"][0]["id"]
# A side face's normal: perpendicular to the hole, so a section along it halves the hole.
side = next(f["normal"] for f in caps if abs(sum(a * b for a, b in zip(f["normal"], extrude))) < 1e-6)

r, whole, whole_span = render(True)
r, n, span = render(False)
assert (n, span) == (whole, whole_span)

r, _, _ = render(False, views=[{"direction": [1, 1, 0]}, "front", {"direction": [0, 0, -1], "up": [1, 0, 0]}])
assert "From (0.71, 0.71, 0.00)" in r.text and "Front (+X right, +Y up)" in r.text, r.text
assert "+X up" in r.text, r.text

# Section: default point = the middle, which is on the hole's axis.
r, _, _ = render(False, section={"normal": side})
assert "Section through" in r.text and "Section (angled)" in r.text, r.text
r, _, _ = render(False, section={"normal": extrude}, views=["isometric"])
assert "cuts away everything" in mcp.call(
    "render_views", section={"normal": side, "point_mm": [-500 * c for c in side]}, expect_error=True).text

# Hide, transparent, zoom.
r, n, _ = render(False, hide=[hole], views=["isometric"])
assert n < whole and f"{hole}: left out" in r.text, r.text
r, _, _ = render(False, highlight=[hole], transparent=["others"])
assert "Everything not highlighted is transparent" in r.text and f"{hole}: orange" in r.text, r.text
r, _, span = render(False, zoom_to=["Cut-Extrude1"], highlight=["Cut-Extrude1"])
assert span < whole_span / 2 and "Cut-Extrude1: framed" in r.text, (span, r.text)
r, _, _ = render(False, zoom_to=["No-Such-Feature"], views=["front"])
assert "No-Such-Feature: not visible" in r.text, r.text

# Refused before drawing.
body = mcp.call("inspect_model", what="bodies").json["bodies"][0]["id"]
assert "leaves nothing" in mcp.call("render_views", hide=[body], expect_error=True).text
assert "no length" in mcp.call("render_views", views=[{"direction": [0, 0, 0]}], expect_error=True).text
assert "along the view direction" in mcp.call(
    "render_views", views=[{"direction": [1, 0, 0], "up": [2, 0, 0]}], expect_error=True).text
assert "no length" in mcp.call("render_views", section={"normal": [0, 0, 0]}, expect_error=True).text

# What changes the geometry fetches it again.
mcp.call("undo")  # the cut
r, n, _ = render(True)
assert n < whole, (n, whole)
render(False)
mcp.call("set_visibility", items=[body], visible=False)
assert "nothing to draw" in mcp.call("render_views", expect_error=True).text
mcp.call("set_visibility", items=[body], visible=True)
render(True)

part = mcp.run_dir / "renderviews-block.SLDPRT"
mcp.call("save_document", path=str(part))
render(True)  # the key includes the document's path
render(False)

# Another document has its own key, even with the same geometry.
mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
render(True)
mcp.call("close_document", discard_changes=True)
render(True)  # back to the block

# ---- an assembly: two blocks
mcp.call("create_assembly")
mcp.call("insert_components", file=str(part))
second = first_id(mcp.call("insert_components", file=str(part), at=[{"origin_mm": [0, 0, 40]}]))
first = next(c["id"] for c in mcp.call("inspect_model", what="components").json["components"] if c["id"] != second)
r, both, both_span = render(True)
render(False)
r, n, _ = render(False, hide=[second])
assert n * 2 == both and f"{second}: left out" in r.text, (n, both, r.text)
r, _, span = render(False, zoom_to=[second], transparent=[first], highlight=[second])
assert span < both_span and f"{first}: transparent" in r.text, (span, both_span, r.text)
# Across both blocks: the highlighted one's cut in dark orange, the other's tan.
r, _, _ = render(False, section={"normal": extrude}, highlight=[second])
# Moving a component changes its placement, hiding it in SolidWorks the bodies shown.
mcp.call("move_component", component=second, translate_mm=[0, 0, 40])
r, n, span = render(True)
assert n == both and span > both_span, (n, both, span, both_span)
mcp.call("set_visibility", items=[second], visible=False)
r, n, _ = render(True)
assert n * 2 == both, (n, both)
mcp.call("set_visibility", items=[second], visible=True)
render(True)
mcp.call("save_document", path=str(mcp.run_dir / "renderviews-assembly.SLDASM"))

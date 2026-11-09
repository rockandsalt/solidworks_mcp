# Reference geometry (create_reference_geometry planes, axes, points and
# coordinate systems), from model geometry and from sketch entities,
# every result checked against the geometry it was made from. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/reference.py
#
# The template's default planes don't face the standard way (devloop.md),
# so nothing here assumes which way Front / Top / Right point: directions
# come from inspect_model.

import math

TOL = 1e-3  # mm


def sub(a, b): return [x - y for x, y in zip(a, b)]
def add(a, b): return [x + y for x, y in zip(a, b)]
def scale(a, k): return [x * k for x in a]
def dot(a, b): return sum(x * y for x, y in zip(a, b))
def cross(a, b): return [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
def norm(a): return math.sqrt(dot(a, a))
def unit(a): return scale(a, 1 / norm(a))
def close(a, b, tol=TOL): return norm(sub(a, b)) < tol
def parallel(a, b): return abs(abs(dot(unit(a), unit(b))) - 1) < 1e-6


def features():
    return {f["name"]: f for f in mcp.call("inspect_model", what="features").json["features"]}


def on_plane(point, plane):
    return abs(dot(sub(point, plane["point_on_plane_mm"]), plane["normal"])) < TOL


def made(r):
    """The JSON of the features a reference geometry call made."""
    import json
    return json.loads(r.text[r.text.rindex("\n[") + 1:])


# A 40 x 20 x 10 block on the Front plane, with a 6 mm hole through it.
mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)

# The default planes now say where they are.
before = features()
front, top, right = before["Front Plane"], before["Top Plane"], before["Right Plane"]
for p in (front, top, right):
    assert p["type"] == "RefPlane" and abs(norm(p["normal"]) - 1) < 1e-6, p
    assert on_plane([0, 0, 0], p), f"{p['name']} should pass through the origin: {p}"
assert abs(dot(front["normal"], top["normal"])) < 1e-6 and abs(dot(top["normal"], right["normal"])) < 1e-6

planes = mcp.call("inspect_model", what="faces", within="Boss-Extrude1", surface_type="plane").json["faces"]
# The two caps: the faces whose normal is the Front plane's.
caps = [f for f in planes if parallel(f["normal"], front["normal"])]
assert len(caps) == 2, caps
cap, other_cap = caps
mcp.call("sketch", shape="circle", on=cap["id"], center=[0, 0], diameter_mm=6)
mcp.call("extrude_sketch", cut=True, through_all=True)

# Edges carry their vertices now.
edges = mcp.call("inspect_model", what="edges", curve_type="line").json["edges"]
assert edges and all("start_vertex" in e and "end_vertex" in e for e in edges), edges[:3]
vertex_at = {}
for e in edges:
    vertex_at[e["start_vertex"]] = e["start_mm"]
    vertex_at[e["end_vertex"]] = e["end_mm"]
assert len(vertex_at) == 8, f"a block has 8 vertices: {vertex_at}"
circles = mcp.call("inspect_model", what="edges", curve_type="circle").json["edges"]
assert len(circles) == 2 and all("start_vertex" not in e for e in circles), circles

# ---- Planes from model geometry --------------------------------------------

r = mcp.call("create_reference_geometry", geometry="plane", references=[{"entity": "top", "offset_mm": 20}])
[p] = made(r)
assert p["type"] == "RefPlane" and parallel(p["normal"], top["normal"]), p
assert abs(abs(dot(p["point_on_plane_mm"], top["normal"])) - 20) < TOL, p
assert "on=\"" + p["id"] + "\"" in r.text, r.text
offset_plane = p

# Offset from a face, flipped: on the other side of the cap.
r = mcp.call("create_reference_geometry", geometry="plane", references=[{"entity": cap["id"], "offset_mm": 5}])
[p5] = made(r)
r = mcp.call("create_reference_geometry", geometry="plane", references=[{"entity": cap["id"], "offset_mm": 5, "flip": True}])
[p5f] = made(r)
d1 = dot(sub(p5["point_on_plane_mm"], cap["point_on_plane_mm"]), cap["normal"])
d2 = dot(sub(p5f["point_on_plane_mm"], cap["point_on_plane_mm"]), cap["normal"])
assert abs(abs(d1) - 5) < TOL and abs(d1 + d2) < TOL, (d1, d2)

# At 30 degrees to the cap, about one of its edges.
cap_edge = next(e for e in edges if cap["id"] in e["faces"])
r = mcp.call("create_reference_geometry", geometry="plane",
             references=[{"entity": cap["id"], "angle_deg": 30}, {"entity": cap_edge["id"]}])
[pa] = made(r)
angle = math.degrees(math.acos(min(1, abs(dot(pa["normal"], cap["normal"])))))
assert abs(angle - 30) < 1e-3, angle
assert on_plane(cap_edge["start_mm"], pa) and on_plane(cap_edge["end_mm"], pa), pa

# Through three vertices: a diagonal plane through the block.
v = sorted(vertex_at)
corner = vertex_at[v[0]]
far = max(v, key=lambda k: norm(sub(vertex_at[k], corner)))
third = next(k for k in v if k not in (v[0], far)
             and norm(cross(sub(vertex_at[k], corner), sub(vertex_at[far], corner))) > 1)
r = mcp.call("create_reference_geometry", geometry="plane",
             references=[{"entity": v[0]}, {"entity": far}, {"entity": third}], name="Diagonal")
[pd] = made(r)
assert pd["name"] == "Diagonal", pd
assert all(on_plane(vertex_at[k], pd) for k in (v[0], far, third)), pd

# Midway between the two caps, i.e. half the block's depth in from each.
r = mcp.call("create_reference_geometry", geometry="plane", references=[{"entity": cap["id"], "relation": "mid_plane"},
                                                   {"entity": other_cap["id"], "relation": "mid_plane"}])
[pm] = made(r)
mid = scale(add(cap["point_on_plane_mm"], other_cap["point_on_plane_mm"]), 0.5)
assert parallel(pm["normal"], cap["normal"]) and on_plane(mid, pm), (pm, mid)

# A sketch goes on a new plane, with the plane's normal.
mcp.call("sketch", shape="circle", on=offset_plane["id"], center=[0, 0], diameter_mm=4)
info = mcp.call("inspect_sketch").json
assert parallel(info["sketch_axes"]["normal"], offset_plane["normal"]), info["sketch_axes"]

# ---- From sketch entities ---------------------------------------------------

# A line and a point in a sketch on Top, given in model coordinates.
u = unit(cross(top["normal"], front["normal"]))
w = unit(cross(top["normal"], u))
a, b = add(scale(u, 30), scale(w, 5)), add(scale(u, 50), scale(w, 25))
mcp.call("sketch", shape="line", plane="top", points=[a, b])
mcp.call("sketch", shape="point", sketch_name=mcp.call("inspect_sketch").json["sketch"], points=[add(scale(u, 60), scale(w, -10))])
path = mcp.call("inspect_sketch").json
axes = path["sketch_axes"]
def to_model(xy):
    return add(axes["origin_mm"], add(scale(axes["x"], xy[0]), scale(axes["y"], xy[1])))
line = next(s for s in path["segments"] if s["type"] == "line")
lone = next(p for p in path["points"] if p["id"] not in (line["start"], line["end"]))
end_at = to_model(line["end_mm"])
assert close(end_at, b), (end_at, b)

# Normal to the line at its end point: the plane a sweep profile goes on.
r = mcp.call("create_reference_geometry", geometry="plane", references=[{"entity": line["id"], "relation": "perpendicular"},
                                                   {"entity": line["end"]}])
[pn] = made(r)
assert parallel(pn["normal"], sub(b, a)) and on_plane(b, pn), pn

# An axis along the sketch line, one along the hole, one where two planes meet.
r = mcp.call("create_reference_geometry", geometry="axis", references=[line["id"]])
[ax] = made(r)
assert ax["type"] == "RefAxis" and parallel(ax["axis"], sub(b, a)), ax
assert norm(cross(sub(ax["axis_point_mm"], a), unit(sub(b, a)))) < TOL, ax
hole = next(f for f in mcp.call("inspect_model", what="faces", surface_type="cylinder").json["faces"] if f.get("hole"))
r = mcp.call("create_reference_geometry", geometry="axis", references=[hole["id"]], name="Hole Axis")
[hx] = made(r)
assert hx["name"] == "Hole Axis" and parallel(hx["axis"], hole["axis"]), hx
assert norm(cross(sub(hx["axis_point_mm"], hole["axis_point_mm"]), unit(hole["axis"]))) < TOL, hx
r = mcp.call("create_reference_geometry", geometry="axis", references=["front", "top"])
[fx] = made(r)
assert parallel(fx["axis"], cross(front["normal"], top["normal"])), fx

# ---- Points -----------------------------------------------------------------

hole_edge = circles[0]
r = mcp.call("create_reference_geometry", geometry="point", references=[hole_edge["id"]])
[pc] = made(r)
assert close(pc["at_mm"], hole_edge["center_mm"]), (pc, hole_edge)

r = mcp.call("create_reference_geometry", geometry="point", references=[other_cap["id"]])
[pf] = made(r)
assert on_plane(pf["at_mm"], {"point_on_plane_mm": other_cap["point_on_plane_mm"], "normal": other_cap["normal"]}), pf

# Halfway along an edge (checks how the percentage is passed), and 3 spread evenly.
e = edges[0]
r = mcp.call("create_reference_geometry", geometry="point", references=[e["id"]], percent=50)
[ph] = made(r)
assert close(ph["at_mm"], scale(add(e["start_mm"], e["end_mm"]), 0.5)), (ph, e)
r = mcp.call("create_reference_geometry", geometry="point", references=[e["id"]], count=3, name="Station")
spread = made(r)
assert [p["name"] for p in spread] == ["Station", "Station2", "Station3"], spread
for p in spread:
    t = dot(sub(p["at_mm"], e["start_mm"]), unit(sub(e["end_mm"], e["start_mm"])))
    assert norm(cross(sub(p["at_mm"], e["start_mm"]), unit(sub(e["end_mm"], e["start_mm"])))) < TOL, p
r = mcp.call("create_reference_geometry", geometry="point", references=[e["id"]], distance_mm=4)
[p4] = made(r)
assert abs(min(norm(sub(p4["at_mm"], e["start_mm"])), norm(sub(p4["at_mm"], e["end_mm"]))) - 4) < TOL, (p4, e)

# At a sketch point, and the same point projected onto a cap.
lone_at = to_model(lone["at_mm"])
r = mcp.call("create_reference_geometry", geometry="point", references=[lone["id"]])
[ps] = made(r)
assert close(ps["at_mm"], lone_at), (ps, lone_at)
r = mcp.call("create_reference_geometry", geometry="point", references=[cap["id"], lone["id"]])
[pp] = made(r)
cap_plane = {"point_on_plane_mm": cap["point_on_plane_mm"], "normal": cap["normal"]}
assert on_plane(pp["at_mm"], cap_plane), pp
assert parallel(sub(pp["at_mm"], lone_at), cap["normal"]) or close(pp["at_mm"], lone_at), pp

# Where an edge crosses a plane: the mid plane cuts the block's edges along the hole.
long_edge = next(x for x in edges if parallel(sub(x["end_mm"], x["start_mm"]), cap["normal"]))
r = mcp.call("create_reference_geometry", geometry="point", references=[long_edge["id"], pm["id"]])
[pi] = made(r)
assert on_plane(pi["at_mm"], pm), (pi, pm)

# At a vertex, and at the origin.
r = mcp.call("create_reference_geometry", geometry="point", references=[v[0]])
[pv] = made(r)
assert close(pv["at_mm"], corner), (pv, corner)
r = mcp.call("create_reference_geometry", geometry="point", references=["origin"])
[po] = made(r)
assert close(po["at_mm"], [0, 0, 0]), po

# ---- Coordinate systems -----------------------------------------------------

r = mcp.call("create_reference_geometry", geometry="coordinate_system", origin=v[0], x_axis=long_edge["id"], name="Fixture CS")
[cs] = made(r)
assert cs["type"] == "CoordSys" and cs["name"] == "Fixture CS", cs
assert close(cs["origin_mm"], corner) and parallel(cs["x_axis"], cap["normal"]), cs
assert abs(dot(cs["x_axis"], cs["y_axis"])) < 1e-6 and parallel(cross(cs["x_axis"], cs["y_axis"]), cs["z_axis"]), cs
r = mcp.call("create_reference_geometry", geometry="coordinate_system", origin=v[0], x_axis=long_edge["id"], flip_x=True)
[csf] = made(r)
assert close(scale(csf["x_axis"], -1), cs["x_axis"], 1e-6), (csf, cs)
# From a sketch point, with Z along the sketch line.
r = mcp.call("create_reference_geometry", geometry="coordinate_system", origin=lone["id"], z_axis=line["id"])
[css] = made(r)
assert close(css["origin_mm"], lone_at) and parallel(css["z_axis"], sub(b, a)), css

# inspect_model reports the new features the same way.
now = features()
assert parallel(now["Hole Axis"]["axis"], hole["axis"]) and close(now["Fixture CS"]["origin_mm"], corner)
assert close(now[pc["name"]]["at_mm"], hole_edge["center_mm"])

# ---- Refusals: nothing is added ---------------------------------------------

count = len(now)
# One vertex doesn't make a plane.
mcp.call("create_reference_geometry", geometry="plane", references=[{"entity": v[0]}], expect_error=True)
# Two parallel faces don't make an axis.
mcp.call("create_reference_geometry", geometry="axis", references=[cap["id"], other_cap["id"]], expect_error=True)
# A sketch isn't a reference feature (add-in), a body isn't a reference (server).
mcp.call("create_reference_geometry", geometry="axis", references=[now["Boss-Extrude1"]["id"]], expect_error=True)
body = mcp.call("inspect_model", what="bodies").json["bodies"][0]["id"]
mcp.call("create_reference_geometry", geometry="point", references=[body], expect_error=True)
mcp.call("create_reference_geometry", geometry="plane", references=[{"entity": "top", "relation": "distance"}], expect_error=True)
mcp.call("create_reference_geometry", geometry="coordinate_system", x_axis="front", y_axis="top", z_axis="right", expect_error=True)
assert len(features()) == count, "a refused call left a feature behind"

# New planes can be deleted like any feature.
mcp.call("delete_features", features=[pd["id"]])
assert "Diagonal" not in features()

mcp.call("render_views", views=["isometric"])

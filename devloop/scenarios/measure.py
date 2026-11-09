# measure / mass_properties against real SolidWorks: a 40 x 20 x 10 block
# (faces, edges, vertices, a plane; mass, center of mass and inertia against
# the box formulas, a coordinate system at a corner, the default-density
# note, a material), then a 6 mm hole (circular edge, arc option), then an
# assembly of two blocks 5 mm apart (distance between components, total
# mass and center of gravity, `only`, hidden components)
# (docs/notes/measure.md). Run with:
#   py devloop/mcp_client.py run devloop/scenarios/measure.py

import math
import re

TOL = 1e-3


def close(a, b, tol=TOL):
    if isinstance(a, (int, float)):
        return abs(a - b) <= tol * max(1.0, abs(b))
    return all(abs(x - y) <= tol * max(1.0, abs(y)) for x, y in zip(a, b))


def measure(*entities, **kw):
    r = mcp.call("measure", entities=list(entities), **kw)
    print(f"    measure {entities} {kw}: {r.text}")
    return r.json


def mass(**kw):
    r = mcp.call("mass_properties", **kw)
    print(f"    mass_properties {kw}: {r.text}")
    return r.json


out = mcp.run_dir.resolve()
block_file = out / "measure-block.SLDPRT"
asm_file = out / "measure-top.SLDASM"

# ---- A block -----------------------------------------------------------------------
mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
body = mcp.call("inspect_model", what="bodies").json["bodies"][0]
lo, hi = body["bbox_mm"]["min"], body["bbox_mm"]["max"]
size = [h - l for l, h in zip(lo, hi)]
center = [(h + l) / 2 for l, h in zip(lo, hi)]
print("    block:", lo, hi)
assert sorted(round(s, 3) for s in size) == [10, 20, 40], size
thin = size.index(min(size))  # the 10 mm axis

planes = mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]


def face_at(axis, value):
    hits = [f["id"] for f in planes
            if abs(f["normal"][axis]) > 0.99 and abs(f["point_on_plane_mm"][axis] - value) < 1e-6]
    assert len(hits) == 1, (axis, value, planes)
    return hits[0]


cap_lo, cap_hi = face_at(thin, lo[thin]), face_at(thin, hi[thin])
side = face_at((thin + 1) % 3, hi[(thin + 1) % 3])

# One face: its area and perimeter.
j = measure(cap_lo)
other = [s for i, s in enumerate(size) if i != thin]
assert close(j["area_mm2"], other[0] * other[1]), j
assert close(j["perimeter_mm"], 2 * sum(other)), j
assert "total_area_mm2" not in j, j

# Two parallel faces: 10 apart, parallel, closest points across the block.
j = measure(cap_lo, cap_hi)
assert close(j["distance_mm"], 10) and close(j["normal_distance_mm"], 10), j
assert j.get("parallel") is True and close(j.get("angle_deg", 0), 0), j
a, b = j["closest_points_mm"]
assert close(abs(b[thin] - a[thin]), 10) and close(j["closest_distance_mm"], 10), j
assert close(abs(j["delta_mm"][thin]), 10), j

# A cap and a side meet at 90 degrees (IMeasure gives no distance for
# faces that intersect; the closest points do).
j = measure(cap_lo, side)
assert close(j["angle_deg"], 90) and j.get("perpendicular") is True and j.get("intersect") is True, j
assert close(j["closest_distance_mm"], 0), j

# An edge's length, and two opposite corners.
edges = mcp.call("inspect_model", what="edges", curve_type="line").json["edges"]
longest = max(edges, key=lambda e: e["length_mm"])
assert close(measure(longest["id"])["length_mm"], 40)
vertex_at = {}
for e in edges:
    vertex_at[e["start_vertex"]] = e["start_mm"]
    vertex_at[e["end_vertex"]] = e["end_mm"]
corner = next(v for v, p in vertex_at.items() if close(p, lo))
opposite = next(v for v, p in vertex_at.items() if close(p, hi))
j = measure(corner, opposite)
assert close(j["distance_mm"], math.sqrt(40 ** 2 + 20 ** 2 + 10 ** 2)), j
assert close([abs(d) for d in j["delta_mm"]], size), j

# A face to a default plane, by name (refgeom's references).
j = measure(cap_hi, "front")
print("    cap to front:", j)
assert "distance_mm" in j or "normal_distance_mm" in j, j

# Refused: nothing, an unknown id, a face as mass_properties' `only`.
assert mcp.call("measure", entities=[], expect_error=True).is_error
assert "unknown id" in mcp.call("measure", entities=["face:9999"], expect_error=True).text
assert "component or body" in mcp.call("mass_properties", only=[cap_lo], expect_error=True).text

# ---- Mass properties of the block ---------------------------------------------------------
j = mass()
volume = size[0] * size[1] * size[2]
assert close(j["volume_mm3"], volume), j
assert close(j["density_kg_m3"], 1000) and "note" in j, j  # no material yet
m = j["mass_kg"]
assert close(m, volume * 1e-9 * 1000), j
assert close(j["center_of_mass_mm"], center), (j, center)
assert close(j["surface_area_mm2"], 2 * (size[0] * size[1] + size[1] * size[2] + size[0] * size[2])), j
# Box: I_i = m / 12 * (sum of the squares of the other two sides), kg mm^2.
box_i = [m / 12 * (size[(i + 1) % 3] ** 2 + size[(i + 2) % 3] ** 2) for i in range(3)]
inertia = j["inertia_kg_mm2"]
assert close(sorted(inertia["principal_moments"]), sorted(box_i)), (inertia, box_i)
at_c = inertia["at_center_of_mass"]
assert close([at_c[i][i] for i in range(3)], box_i), (at_c, box_i)
assert all(abs(at_c[r][c]) < 1e-6 for r in range(3) for c in range(3) if r != c), at_c
# At the origin: the parallel axis theorem on the diagonal.
at_o = inertia["at_coordinate_system"]
for i in range(3):
    o1, o2 = center[(i + 1) % 3], center[(i + 2) % 3]
    assert close(at_o[i][i], box_i[i] + m * (o1 ** 2 + o2 ** 2)), (i, at_o, center)

# Relative to a coordinate system at the corner (axes as the model's): the
# center of mass is half the size away, and the products of inertia are
# +m*dx*dy (SolidWorks' Pxy = +integral of x*y dm, seen 2026-11-03).
mcp.call("create_reference_geometry", geometry="coordinate_system", origin=corner, name="Corner CS")
j = mass(coordinate_system="Corner CS")
d = [s / 2 for s in size]
assert close(j["center_of_mass_mm"], d), j
at_cs = j["inertia_kg_mm2"]["at_coordinate_system"]
assert close(at_cs[0][1], m * d[0] * d[1]) and close(at_cs[1][2], m * d[1] * d[2]), (at_cs, m, d)
assert close(at_cs[0][2], m * d[0] * d[2]) and close(at_cs[1][0], at_cs[0][1]), (at_cs, m, d)
r = mcp.call("mass_properties", coordinate_system="Nope", expect_error=True)
assert r.is_error and "Nope" in r.text, r.text

# A material: density and mass follow.
mcp.call("part_material", material="AISI 1020")
j = mass()
assert "note" not in j and j["density_kg_m3"] > 7000, j
assert close(j["mass_kg"], j["density_kg_m3"] * volume * 1e-9), j

# ---- A hole: circular edges, and where distances to them are taken ----------------------------
mcp.call("sketch", shape="circle", on=cap_hi, center=[0, 0], diameter_mm=6)
mcp.call("extrude_sketch", cut=True, through_all=True)
circles = mcp.call("inspect_model", what="edges", curve_type="circle").json["edges"]
assert len(circles) == 2, circles
j = measure(circles[0]["id"])
# A full circle gets a diameter only (IMeasure's Radius is for arcs).
assert close(j["diameter_mm"], 6), j
assert close(j.get("length_mm", j.get("arc_length_mm", 0)), 6 * math.pi), j
mcp.call("save_document", path=str(block_file))
side_now = face_at((thin + 1) % 3, hi[(thin + 1) % 3])
c_center = measure(circles[0]["id"], side_now)
c_min = measure(circles[0]["id"], side_now, arc="min")
c_max = measure(circles[0]["id"], side_now, arc="max")
print("    arc options:", c_center, c_min, c_max)
assert close(c_center["distance_mm"] - c_min["distance_mm"], 3), (c_center, c_min)
assert close(c_max["distance_mm"] - c_center["distance_mm"], 3), (c_center, c_max)
j = mass()
hole_volume = math.pi * 9 * 10
assert close(j["volume_mm3"], volume - hole_volume), j
# The center of mass against the body's centroid (IBody2::GetMassProperties).
centroid = mcp.call("inspect_model", what="bodies").json["bodies"][0]["centroid_mm"]
assert close(j["center_of_mass_mm"], centroid), (j, centroid)
part_mass = j["mass_kg"]
part_com = j["center_of_mass_mm"]
mcp.call("save_document")

# ---- An assembly of two blocks 5 mm apart -------------------------------------------------------
mcp.call("create_assembly")
mcp.call("save_document", path=str(asm_file))
long_axis = size.index(max(size))
shift = [0.0, 0.0, 0.0]
shift[long_axis] = size[long_axis] + 5
first = re.findall(r"^- (component:\d+):", mcp.call("insert_components", file=str(block_file)).text, re.M)[0]
r = mcp.call("insert_components", file=str(block_file), at=[{"origin_mm": shift}], fixed=False)
second = re.findall(r"^- (component:\d+):", r.text, re.M)[0]
mcp.call("save_document")

j = measure(first, second)
assert close(j["distance_mm"], 5), j
assert close(abs(j["delta_mm"][long_axis]), 5), j

j = mass()
assert j["document"].lower().startswith("measure-top"), j
assert close(j["mass_kg"], 2 * part_mass), (j, part_mass)
want = [c + s / 2 for c, s in zip(part_com, shift)]
assert close(j["center_of_mass_mm"], want), (j, want)
one = mass(only=[second])
assert close(one["mass_kg"], part_mass) and one["measured"], one
assert close(one["center_of_mass_mm"], [c + s for c, s in zip(part_com, shift)]), one

# Hidden components count unless exclude_hidden.
mcp.call("set_visibility", items=[second], visible=False)
assert close(mass()["mass_kg"], 2 * part_mass)
assert close(mass(exclude_hidden=True)["mass_kg"], part_mass)
mcp.call("set_visibility", items=[second], visible=True)
mcp.call("save_document")

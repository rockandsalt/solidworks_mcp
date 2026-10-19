# delete_features / delete_sketch_entities against real SolidWorks: what
# GetChildren reports as dependents, what each `children` mode leaves, the
# fixed features, and what goes with deleted sketch segments
# (docs/notes/deleting.md). Fillets, assemblies and mate folders have no MCP
# tool, so they're made through COM (devloop/swcom.ps1). Run with:
#   py devloop/mcp_client.py run devloop/scenarios/deleting.py

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


def feature_names() -> list[str]:
    return [f["name"] for f in mcp.call("inspect_model", what="features").json["features"]]


def feature_id(name: str) -> str:
    ids = [f["id"] for f in mcp.call("inspect_model", what="features").json["features"] if f["name"] == name]
    assert len(ids) == 1, f"{name}: {ids}"
    return ids[0]


def fillet_edge(point_mm):
    """Fillets the edge through `point_mm` (model mm), 2 mm."""
    x, y, z = (c / 1000 for c in point_mm)
    out = com(f"""$d = Com $sw ActiveDoc -Get; $x = Com $d Extension -Get
$none = [Runtime.InteropServices.DispatchWrapper]::new([object]$null)
if (-not (Com $x SelectByID2 @('', 'EDGE', {x}, {y}, {z}, $false, 1, $none, 0))) {{ throw 'no edge there' }}
$f = Com (Com $d FeatureManager -Get) FeatureFillet3 @(195, 0.002, 0, 0, $null, $null, $null)
if (-not $f) {{ throw 'FeatureFillet3 returned null' }}
Com $f Name -Get""")
    return out.splitlines()[-1]


# ---- Features in a part -----------------------------------------------------
# 40 x 20 x 10 box. With this machine's template the box spans x 0..10,
# y -20..20, z -10..10 (docs/notes/devloop.md: Front Plane faces +X).
mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)  # Sketch1
mcp.call("extrude_sketch", depth_mm=10)  # Boss-Extrude1
faces = mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]


def face_at(axis: int, value: float) -> str:
    hits = [f["id"] for f in faces
            if abs(f["normal"][axis]) > 0.99 and abs(f["point_on_plane_mm"][axis] - value) < 1e-6]
    assert len(hits) == 1, (axis, value, faces)
    return hits[0]


far_cap, top, bottom = face_at(0, 10), face_at(2, 10), face_at(2, -10)
mcp.call("sketch_circle", on=far_cap, center=[10, 0, 0], diameter_mm=6)  # Sketch2
mcp.call("cut_extrude_sketch", depth_mm=3)  # Cut-Extrude1
mcp.call("sketch_circle", on=top, center=[5, 0, 10], diameter_mm=4)  # Sketch3, used by nothing
hole = mcp.call("hole_wizard", kind="hole", size="M4", on=bottom, locations=[[5, 10, -10]], depth_mm=5)
print("    hole:", hole.text)
fillet = fillet_edge([5, 20, 10])  # the edge between the y=20 side and the top
tree = mcp.call("inspect_model").json["features"]
print("    tree:", [(f["name"], f["type"], f.get("children")) for f in tree])
hole_name = [f["name"] for f in tree if f["type"] == "HoleWzd"]
assert len(hole_name) == 1, tree
hole_name = hole_name[0]

# The default planes and the origin are refused by the add-in, by name or id.
for target in ("Front Plane", feature_id("Top Plane")):
    r = mcp.call("delete_features", features=[target], expect_error=True)
    assert "one of the default planes" in r.text, r.text
r = mcp.call("delete_features", features=["Origin"], expect_error=True)
assert "is the origin" in r.text, r.text

# Refused by default, naming every dependent: the sketches on the box's faces,
# the cut and hole made from them, the fillet -- but not the extrude's own
# (absorbed) Sketch1.
r = mcp.call("delete_features", features=["Boss-Extrude1"], expect_error=True)
for name in ("Sketch2", "Cut-Extrude1", "Sketch3", hole_name, fillet):
    assert name in r.text, f"{name} not named as a dependent: {r.text}"
assert "Sketch1" not in r.text, r.text
# A sketch's dependent is the feature made from it.
r = mcp.call("delete_features", features=["Sketch1"], expect_error=True)
assert "Boss-Extrude1" in r.text, r.text
assert feature_names().count("Boss-Extrude1") == 1, "a refusal deleted something"

# The hole's placement and profile sketches are absorbed (not dependents
# above), but unlike an extrude's sketch SolidWorks deletes them with the
# hole even without swDelete_Absorbed.
before = set(feature_names())
r = mcp.call("delete_features", features=[feature_id(hole_name)])
assert before - set(feature_names()) == {hole_name}
assert r.text.startswith(f"deleted 3 features: {hole_name} (hole), Sketch") and "Kept" not in r.text, r.text

mcp.call("delete_features", features=[feature_id(fillet)])
assert fillet not in feature_names()

# delete_absorbed takes the cut's sketch with it.
r = mcp.call("delete_features", features=["Cut-Extrude1"], delete_absorbed=True)
assert {"Cut-Extrude1", "Sketch2"}.isdisjoint(feature_names()), r.text

# children="keep" (DeleteSelection2 options = 0) really keeps them.
before = set(feature_names())
r = mcp.call("delete_features", features=["Boss-Extrude1"], children="keep")
after = set(feature_names())
print("    keep:", r.text)
assert before - after == {"Boss-Extrude1"}, before - after
assert "Kept the features that depended on it (as asked): Sketch3" in r.text, r.text

# children="delete" takes the dependents, but not the absorbed sketch.
mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
faces = mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]
mcp.call("sketch_circle", on=face_at(0, 10), center=[10, 0, 0], diameter_mm=6)
mcp.call("cut_extrude_sketch", depth_mm=3)
r = mcp.call("delete_features", features=["Boss-Extrude1"], children="delete")
print("    children=delete:", r.text)
names = feature_names()
assert {"Boss-Extrude1", "Sketch2", "Cut-Extrude1"}.isdisjoint(names) and "Sketch1" in names, names

# A hole placed on a sketch of our own: that sketch is the hole's parent, not
# absorbed (the hole has its own 3D sketch + sketch), and stays.
mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
faces = mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]
mcp.call("sketch_point", on=face_at(0, 10), points=[[10, 5, 0]])  # Sketch2
mcp.call("hole_wizard", kind="hole", size="M4", sketch_name="Sketch2", depth_mm=5)
hole_name = [f["name"] for f in mcp.call("inspect_model").json["features"] if f["type"] == "HoleWzd"][0]
r = mcp.call("delete_features", features=[hole_name])
assert r.text.startswith(f"deleted 3 features: {hole_name} (hole), 3DSketch") and " Sketch2 " not in r.text, r.text
assert "Sketch2" in feature_names()


# ---- Sketch entities --------------------------------------------------------
def sketch():
    return mcp.call("inspect_sketch", sketch_name="Sketch1").json


def sw_dialogs() -> list[str]:
    """Titles of the dialogs SolidWorks has open (modeless ones don't block the add-in)."""
    p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
                        str(SWCOM.parent / "sw.ps1"), "dialogs"], capture_output=True, text=True, timeout=60)
    return [line.strip() for line in p.stdout.splitlines() if line.strip()]


mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)  # line:1..4 sides, 5/6 diagonals
mcp.call("extrude_sketch", depth_mm=10)
old = {e["id"] for e in sketch()["segments"]}
mcp.call("sketch_line", sketch_name="Sketch1", points=[[-15, 14], [15, 14]], construction=True)
line = [e for e in sketch()["segments"] if e["id"] not in old]
assert len(line) == 1, line
line = line[0]
mcp.call("dimension_sketch", dimensions=[{"kind": "length", "entities": [line["id"]], "value_mm": 30}])
mcp.call("add_sketch_relations", relations=[{"kind": "horizontal", "entities": [line["id"]]}])
old_points = {p["id"] for p in sketch()["points"]}
mcp.call("sketch_point", sketch_name="Sketch1", points=[[0, -14]])
before = sketch()
lone = [p["id"] for p in before["points"] if p["id"] not in old_points]
assert len(lone) == 1, lone
assert len(before["dimensions"]) == 1, before["dimensions"]
print("    before:", before)

# A corner is an end point of two sides: refused, naming them. (Looked up:
# ids number on across documents, so it isn't point:1 after other scenarios.)
corner = next(s["start"] for s in before["segments"] if s.get("name") == "Line1")
r = mcp.call("delete_sketch_entities", entities=[corner], expect_error=True)
assert "end or center point of" in r.text and "Line1" in r.text, r.text

# The line and its (unshared) end points, dimension and relation go
# together; its start point given too is skipped, not selected.
r = mcp.call("delete_sketch_entities", entities=[line["id"], line["start"]])
dim = before["dimensions"][0]["name"]
assert r.text.startswith(f"deleted 2 entities from Sketch1 ({line['id']}, {line['start']}), along with "
                         f"2 end/center points, 1 dimension ({dim}), 1 relation."), r.text
after = sketch()
by_id = lambda s: {e["id"]: e for e in s["segments"] + s["points"]}
gone = set(by_id(before)) - set(by_id(after))
assert gone == {line["id"], line["start"], line["end"]}, gone
assert not after["dimensions"], after["dimensions"]
# Everything else is still there under the same id: GetID is the same on the
# closed and the opened sketch (else the wrong entity would have gone).
for id_, e in by_id(after).items():
    assert {k: v for k, v in e.items() if k != "status"} == \
        {k: v for k, v in by_id(before)[id_].items() if k != "status"}, (id_, e, by_id(before)[id_])

r = mcp.call("delete_sketch_entities", entities=lone)
assert r.text.startswith(f"deleted 1 entity from Sketch1 ({lone[0]})."), r.text

# A side of the extruded rectangle: the corners stay (the other sides use
# them), its horizontal relation goes, the profile opens and the extrude
# fails -- closing the sketch must not pop up a dialog.
before = sketch()
side = next(s["id"] for s in before["segments"] if s.get("name") == "Line1")
r = mcp.call("delete_sketch_entities", entities=[side])
print("    side:", r.text)
assert r.text.startswith(f"deleted 1 entity from Sketch1 ({side}), along with 1 relation."), r.text
assert "0 closed contours and 1 open contour" in r.text, r.text
assert "Now failing to rebuild: Boss-Extrude1" in r.text, r.text
after = sketch()
assert set(by_id(before)) - set(by_id(after)) == {side}
assert not (open_dialogs := sw_dialogs()), open_dialogs
# (diagnose_model also fails if the sketch was left open.)
assert "Boss-Extrude1" in mcp.call("diagnose_model").text


# ---- Mates in an assembly ---------------------------------------------------
# Two copies of a saved block and an assembly plane 50 mm off the assembly's
# Front Plane, made through COM: no MCP tool makes these.
mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=30, height_mm=30)
mcp.call("extrude_sketch", depth_mm=10)
block = mcp.run_dir / "block.SLDPRT"
plane_name = com(f"""$d = Com $sw ActiveDoc -Get
$e = Com $d SaveAs3 @('{block}', 0, 1); if ($e -ne 0) {{ throw "SaveAs3: $e" }}
$template = Com $sw GetUserPreferenceStringValue @(9)  # swDefaultTemplateAssembly
$a = Com $sw NewDocument @($template, 0, 0, 0); if (-not $a) {{ throw "NewDocument($template) failed" }}
foreach ($x in 0.0, 0.08) {{
    if (-not (Com $a AddComponent5 @('{block}', 0, '', $false, '', $x, 0.0, 0.0))) {{ throw 'AddComponent5 failed' }}
}}
$none = [Runtime.InteropServices.DispatchWrapper]::new([object]$null)
if (-not (Com (Com $a Extension -Get) SelectByID2 @('Front Plane', 'PLANE', 0.0, 0.0, 0.0, $false, 0, $none, 0))) {{ throw 'no Front Plane' }}
$p = Com (Com $a FeatureManager -Get) InsertRefPlane @(8, 0.05, 0, 0.0, 0, 0.0)  # swRefPlaneReferenceConstraint_Distance
if (-not $p) {{ throw 'InsertRefPlane failed' }}
Com $p Name -Get""").splitlines()[-1]  # PLANE1 with this assembly template

overview = mcp.call("inspect_model").json
# The fixed one (the first inserted) first; inspect_model's order varies.
components = [c["id"] for c in sorted(overview["components"], key=lambda c: not c["fixed"])]
assert len(components) == 2, overview
plane1 = [f["id"] for f in overview["features"] if f["name"] == plane_name]
assert len(plane1) == 1, overview["features"]
planes = [{f["name"]: f["id"] for f in mcp.call("inspect_model", what="features", within=c).json["features"]}
          for c in components]
mcp.call("add_mate", kind="coincident", entities=[plane1[0], planes[1]["Front Plane"]])  # Coincident1 (component 1 is fixed)
mcp.call("add_mate", kind="coincident", entities=[planes[0]["Top Plane"], planes[1]["Top Plane"]])  # Coincident2
mcp.call("add_mate", kind="parallel", entities=[planes[0]["Right Plane"], planes[1]["Right Plane"]])  # Parallel1

# A component's "Front Plane" isn't the assembly's (fixed) one of the same
# name: matched by object, it's refused as a component's feature.
r = mcp.call("delete_features", features=[planes[0]["Front Plane"]], expect_error=True)
assert "belongs to a component" in r.text, r.text

# The mate on the plane is its dependent.
r = mcp.call("delete_features", features=[plane_name], expect_error=True)
assert "Coincident1" in r.text and "Coincident2" not in r.text, r.text

# Mates are found inside the mates folder, and inside a folder in it.
mates_tree = com("""$a = Com $sw ActiveDoc -Get
if (-not (Com (Com $a FeatureByName @('Parallel1')) Select2 @($false, 0))) { throw 'no Parallel1' }
$folder = Com (Com $a FeatureManager -Get) InsertFeatureTreeFolder2 @(2)  # swFeatureTreeFolder_Containing
if (-not $folder) { throw 'InsertFeatureTreeFolder2 failed' }
# The mates folder's tree, sub-features in [brackets].
function Tree($f) {
    $s = Com $f GetFirstSubFeature; $out = @()
    while ($s) {
        $sub = Tree $s
        $out += (Com $s Name -Get) + $(if ($sub) { "[$sub]" } else { '' })
        $s = Com $s GetNextSubFeature
    }
    $out -join ' '
}
$f = Com $a FirstFeature
while ((Com $f GetTypeName2) -ne 'MateGroup') { $f = Com $f GetNextFeature }
Tree $f""").splitlines()[-1]
# A folder doesn't nest: it's a start and an end marker around its mates,
# all direct sub-features of the mates folder.
assert mates_tree == "Coincident1 Coincident2 Folder1 Parallel1 Folder1___EndTag___", mates_tree
mates = {m["name"]: m["id"] for m in mcp.call("inspect_model", what="mates").json["mates"]}
assert set(mates) == {"Coincident1", "Coincident2", "Parallel1"}, mates
for name in ("Parallel1", "Coincident2"):
    r = mcp.call("delete_features", features=[mates[name]])
    assert r.text.startswith(f"deleted 1 feature: {name} (mate)."), r.text
r = mcp.call("delete_features", features=[plane_name], children="delete")
assert r.text.startswith(f"deleted 2 features: {plane_name}") and "Coincident1" in r.text, r.text
assert not mcp.call("inspect_model", what="mates").json.get("mates"), "mates left"
assert not (open_dialogs := sw_dialogs()), open_dialogs

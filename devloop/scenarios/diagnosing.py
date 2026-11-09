# diagnose_model against real SolidWorks (docs/notes/diagnosing.md): a sketch
# whose face was deleted and the cut made from it, a sketch with a relation
# and a dimension to deleted model edges, a mate that lost its face, and a
# component's broken features seen from the assembly. Converted edges,
# dimensions to model edges, assemblies and saving have no MCP tool, so
# they're made through COM (devloop/swcom.ps1). Run with:
#   py devloop/mcp_client.py run devloop/scenarios/diagnosing.py

import json
import subprocess
import time
from pathlib import Path

SWCOM = Path(__file__).resolve().parent.parent / "swcom.ps1"
INTEROP = r"C:\Program Files\Dassault Systemes\SOLIDWORKS Apps 2025\SOLIDWORKS\api\redist"


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
    """Titles of the dialogs SolidWorks has open (modeless ones don't block the add-in)."""
    p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
                        str(SWCOM.parent / "sw.ps1"), "dialogs"], capture_output=True, text=True, timeout=60)
    return [line.strip() for line in p.stdout.splitlines() if line.strip()]


def whats_wrong_raw() -> str:
    """GetWhatsWrong through the interop assembly: the .NET type of each of
    its three arrays (bool[] = VT_BOOL, int[] = VT_I4, ...) and their items."""
    return com(f"""Add-Type -Path '{INTEROP}\\SolidWorks.Interop.sldworks.dll'
$x = Com (Com $sw ActiveDoc -Get) Extension -Get
$a = [object[]]::new(3)
$ok = [SolidWorks.Interop.sldworks.IModelDocExtension].GetMethod('GetWhatsWrong').Invoke($x, $a)
"ok=$ok"
foreach ($i in 0..2) {{
    $v = $a[$i]
    if ($null -eq $v) {{ "[$i] null"; continue }}
    $items = foreach ($e in $v) {{
        if ($null -eq $e) {{ 'null' }} elseif ($e -is [__ComObject]) {{ Com $e Name -Get }} else {{ "$e" }}
    }}
    "[$i] $($v.GetType().FullName): $($items -join ', ')"
}}""")


def diagnose(**args) -> dict:
    r = mcp.call("diagnose_model", **args)
    print("    diagnose:", json.dumps(r.json, indent=1))
    # Right after SolidWorks starts, an untitled dialog showed for a moment
    # once (on a clean part): look twice.
    if sw_dialogs():
        time.sleep(5)
        assert not sw_dialogs(), f"dialog after diagnose_model: {sw_dialogs()}"
    return r.json


def problem(d: dict, name: str) -> dict:
    hits = [p for p in d.get("problems", []) if p["name"] == name]
    assert len(hits) == 1, (name, d)
    return hits[0]


def faces(**filters) -> list[dict]:
    return mcp.call("inspect_model", what="faces", **filters).json["faces"]


def plane_face(z: float, nz: float = 1, **filters) -> str:
    hits = [f["id"] for f in faces(surface_type="plane", normal=[0, 0, nz], **filters)
            if abs(f["point_on_plane_mm"][2] - z) < 1e-6]
    assert len(hits) == 1, (z, hits)
    return hits[0]


# "Display errors at every rebuild" on, so a What's Wrong dialog would show
# if diagnose_model's rebuild didn't turn it off; restored at the end.
TOGGLES = f"""Add-Type -Path '{INTEROP}\\SolidWorks.Interop.swconst.dll'
$every = [int][SolidWorks.Interop.swconst.swUserPreferenceToggle_e]::swShowErrorsEveryRebuild
$dimInput = [int][SolidWorks.Interop.swconst.swUserPreferenceToggle_e]::swInputDimValOnCreate
"""
before = com(TOGGLES + """"$(Com $sw GetUserPreferenceToggle @($every)) $(Com $sw GetUserPreferenceToggle @($dimInput))"
Com $sw SetUserPreferenceToggle @($every, $true)
Com $sw SetUserPreferenceToggle @($dimInput, $false)""").splitlines()[0].split()
print("    swShowErrorsEveryRebuild, swInputDimValOnCreate were:", before)

try:
    # ---- A part whose cut lost its sketch face ---------------------------------
    # 40 x 20 x 10 box (x 0..10, y -20..20, z -10..10 with this machine's
    # template, docs/notes/devloop.md), a 6 mm cylinder on its top face
    # (z 10..15), a 2 mm hole cut into the cylinder's top.
    mcp.call("create_part")
    mcp.call("sketch", shape="rectangle", width_mm=40, height_mm=20)  # Sketch1
    mcp.call("extrude_sketch", depth_mm=10)  # Boss-Extrude1
    mcp.call("sketch", shape="circle", on=plane_face(10), center=[5, 0, 10], diameter_mm=6)  # Sketch2
    mcp.call("extrude_sketch", depth_mm=5)  # Boss-Extrude2
    mcp.call("sketch", shape="circle", on=plane_face(15), center=[5, 0, 15], diameter_mm=2)  # Sketch3
    mcp.call("extrude_sketch", cut=True, depth_mm=2)  # Cut-Extrude1
    block = mcp.run_dir / "diag-block.SLDPRT"
    com(f"""$d = Com $sw ActiveDoc -Get
$e = Com $d SaveAs3 @('{block}', 0, 1); if ($e -ne 0) {{ throw "SaveAs3: $e" }}""")

    d = diagnose()
    assert d["summary"].startswith("no rebuild errors"), d
    assert "problems" not in d and "note" not in d, d
    print(whats_wrong_raw())  # clean part: all three empty

    # The assembly: two blocks, the second's bottom face on the first's
    # cylinder top (Coincident1, lost when the cylinder goes), and their
    # Right planes parallel (Parallel1, unaffected).
    assembly = com(f"""$template = Com $sw GetUserPreferenceStringValue @(9)  # swDefaultTemplateAssembly
$a = Com $sw NewDocument @($template, 0, 0, 0); if (-not $a) {{ throw "NewDocument($template) failed" }}
foreach ($x in 0.0, 0.08) {{
    if (-not (Com $a AddComponent5 @('{block}', 0, '', $false, '', $x, 0.0, 0.0))) {{ throw 'AddComponent5 failed' }}
}}
Com $a GetTitle""").splitlines()[-1]
    overview = mcp.call("inspect_model").json
    components = [c["id"] for c in sorted(overview["components"], key=lambda c: not c["fixed"])]
    assert len(components) == 2, overview
    # Components are placed by their bounding box center: pick faces by feature.
    cylinder_top = [f["id"] for f in faces(surface_type="plane", normal=[0, 0, 1], within=components[0])
                    if f["feature"] == "Boss-Extrude2"]
    other_bottom = [f["id"] for f in faces(surface_type="plane", normal=[0, 0, -1], within=components[1])
                    if f["feature"] == "Boss-Extrude1"]
    assert len(cylinder_top) == 1 and len(other_bottom) == 1, (cylinder_top, other_bottom)
    mcp.call("add_mate", kind="coincident", entities=[cylinder_top[0], other_bottom[0]], alignment="anti_aligned")
    planes = [{f["name"]: f["id"] for f in mcp.call("inspect_model", what="features", within=c).json["features"]}
              for c in components]
    mcp.call("add_mate", kind="parallel", entities=[planes[0]["Right Plane"], planes[1]["Right Plane"]])
    d = diagnose()
    assert d["summary"].startswith("no rebuild errors"), d

    # Delete the cylinder in the part, keeping what depends on it: Sketch3's
    # face is gone and Cut-Extrude1 fails with it.
    com("if (-not (Com $sw ActivateDoc @('diag-block.SLDPRT'))) { throw 'ActivateDoc failed' }")
    assert mcp.call("diagnose_model").json["document"].lower().startswith("diag-block"), "diag-block.SLDPRT not active"
    r = mcp.call("delete_features", features=["Boss-Extrude2"], children="keep")
    print("    delete:", r.text)
    print(whats_wrong_raw())
    d = diagnose()
    # Rebuilt even though the rebuild fails (EditRebuild3 says false then).
    assert "note" not in d, d
    # Each sketch once, though GetNextFeature also visits absorbed ones.
    assert d["summary"].endswith(" and 3 sketches)"), d["summary"]
    sketch3, cut = problem(d, "Sketch3"), problem(d, "Cut-Extrude1")
    print("    Sketch3:", sketch3, "\n    Cut-Extrude1:", cut)
    # The cut's parents include its own absorbed sketch; the sketch is the
    # root cause, the cut fails because of it.
    assert "Sketch3" in cut.get("parents", []), cut
    assert cut.get("caused_by") == ["Sketch3"], cut
    assert sketch3.get("root_cause"), sketch3
    assert sketch3["sketch"].get("plane_missing"), sketch3
    assert "Edit Sketch Plane" in sketch3["fix"], sketch3
    assert d["problems"][0]["name"] == "Sketch3", "root causes come first"

    # inspect_model shows the error too, pointing at diagnose_model.
    tree = mcp.call("inspect_model").json
    print("    inspect_model:", [(f["name"], f.get("error"), f.get("warning")) for f in tree["features"]],
          tree.get("rebuild_note"))
    assert "diagnose_model" in tree.get("rebuild_note", ""), tree
    # rebuild=false: as of the last rebuild, said so.
    d = diagnose(rebuild=False)
    assert "note" in d and {"Sketch3", "Cut-Extrude1"} <= {p["name"] for p in d["problems"]}, d
    # Save2 returns 64 for a part with rebuild errors, but saves.
    com("$d = Com $sw ActiveDoc -Get; $e = Com $d Save2 @($true); if (Com $d GetSaveFlag) { throw \"Save2: $e\" }")

    # ---- The assembly: the mate lost its face, the component's features fail ----
    com(f"if (-not (Com $sw ActivateDoc @('{assembly}'))) {{ throw 'ActivateDoc failed' }}")
    print(whats_wrong_raw())
    d = diagnose()
    mate = problem(d, "Coincident1")
    print("    Coincident1:", mate)
    entities = mate["mated_entities"]
    assert sum(1 for e in entities if e.get("missing")) == 1, entities
    # The missing one still names its component.
    assert all(e.get("component") or e.get("component_name") for e in entities), entities
    assert "Parallel1" not in [p["name"] for p in d["problems"]], d
    # The mate's own missing face makes it a root cause, not its components.
    assert mate.get("root_cause") and "add_mate" in mate["fix"], mate
    # What's Wrong lists a component whose part fails, not the part's
    # features (so no problem carries a "component" of its own).
    blocks = [p for p in d["problems"] if p["type"] == "Reference"]
    assert blocks and all(p["problem"] == "its part has rebuild errors of its own" for p in blocks), blocks
    assert not [p for p in d["problems"] if p.get("component")], d

    # ---- A sketch with a relation and a dimension to deleted model edges --------
    # Sketch3 on the box's top face: a circle dimensioned to the bottom edge of
    # a block (Boss-Extrude2) standing on that face, and a line converted from
    # another of the block's bottom edges ("On Edge"). Then the block goes.
    mcp.call("create_part")
    mcp.call("sketch", shape="rectangle", width_mm=40, height_mm=20)  # Sketch1
    mcp.call("extrude_sketch", depth_mm=10)  # Boss-Extrude1
    top = plane_face(10)
    mcp.call("sketch", shape="line", on=top, points=[[2, 5, 10], [8, 5, 10], [8, 15, 10], [2, 15, 10]], closed=True)  # Sketch2
    mcp.call("extrude_sketch", depth_mm=5)  # Boss-Extrude2: x 2..8, y 5..15, z 10..15
    mcp.call("sketch", shape="circle", on=plane_face(10), center=[5, -10, 10], diameter_mm=4)  # Sketch3
    com("""$d = Com $sw ActiveDoc -Get; $x = Com $d Extension -Get; $sm = Com $d SketchManager -Get
$none = [Runtime.InteropServices.DispatchWrapper]::new([object]$null)
# Typed: a bare -0.01 argument would come in as a string.
function Pick([string]$type, [double]$x0, [double]$y0, [double]$z0, [bool]$append) {
    if (-not (Com $x SelectByID2 @('', $type, $x0, $y0, $z0, $append, 0, $none, 0))) { throw "nothing ($type) at $x0 $y0 $z0" }
}
if (-not (Com (Com $d FeatureByName @('Sketch3')) Select2 @($false, 0))) { throw 'no Sketch3' }
Com $d EditSketch
# The block's bottom edge along y at x = 8 -> an "On Edge" line.
Pick 'EDGE' 0.008 0.010 0.010 $false
if (-not (Com $sm SketchUseEdge3 @($false, $false))) { throw 'SketchUseEdge3 failed' }
# The circle to the block's bottom edge along x at y = 5.
Pick 'SKETCHSEGMENT' 0.007 (-0.010) 0.010 $false
Pick 'EDGE' 0.005 0.005 0.010 $true
$dim = Com $d AddDimension2 @(0.012, -0.002, 0.010)
if (-not $dim) { throw 'AddDimension2 failed' }
"dimension: $(Com (Com $dim GetDimension2 @(0)) FullName -Get)"
Com $sm InsertSketch @($true)""")
    sketch = mcp.call("inspect_sketch", sketch_name="Sketch3").json
    print("    Sketch3 before:", sketch)
    circle = [e["id"] for e in sketch["segments"] if e["type"] == "circle"]
    line = [e["id"] for e in sketch["segments"] if e["type"] == "line"]
    assert len(circle) == 1 and len(line) == 1, sketch["segments"]
    circle, line = circle[0], line[0]
    # The model edge comes back as a hidden stand-in segment whose id can
    # equal the circle's: it must not be named as the circle.
    converted = [r for r in sketch["relations"] if r["type"] == "converted edge"]
    assert len(converted) == 1 and circle not in converted[0]["entities"], sketch["relations"]
    d = diagnose()
    assert d["summary"].startswith("no rebuild errors"), d

    r = mcp.call("delete_features", features=["Boss-Extrude2"], children="keep")
    print("    delete:", r.text)
    print(whats_wrong_raw())
    d = diagnose()
    sketch3 = problem(d, "Sketch3")
    print("    Sketch3:", sketch3)
    s = sketch3["sketch"]
    assert not s.get("plane_missing"), "its face is still there"
    dangling = [r for r in s.get("relations", []) if r["problem"] == "dangling"]
    print("    dangling relations:", dangling, "\n    dangling_dimensions:", s.get("dangling_dimensions"))
    # Both are on SolidWorks' dangling list (the dimension too, not only via
    # IAnnotation::IsDangling), and the lost edge counts as missing.
    by_kind = {r.get("type", "dimension"): r for r in dangling}
    assert set(by_kind) == {"converted edge", "dimension"}, dangling
    assert by_kind["converted edge"]["entities"] == [line, "missing"], dangling
    assert by_kind["dimension"]["entities"] == [circle, "missing"], dangling
    assert sketch3.get("root_cause"), sketch3
finally:
    # A failure can leave the saved block with unsaved changes, which makes
    # sw.ps1 stop/reset refuse.
    com("""$b = Com $sw GetOpenDocumentByName @('diag-block.SLDPRT')
if ($b -and (Com $b GetSaveFlag)) { Com $b Save2 @($true) | Out-Null }""")
    com(TOGGLES + f"""Com $sw SetUserPreferenceToggle @($every, ${before[0].lower()})
Com $sw SetUserPreferenceToggle @($dimInput, ${before[1].lower()})""")

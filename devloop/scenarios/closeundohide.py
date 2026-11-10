# close_document / undo / set_visibility against real SolidWorks (files.md,
# architecture.md): undo counted in steps on a part, the delete and dimension
# steps checked by the tree and volume, undo in an assembly and a drawing; hiding
# bodies, sketches, reference geometry and components, read back through
# inspect_model and render_views, kept through saving, with refusals changing
# nothing; closing by every kind of name, refused with unsaved changes, the active
# document kept, a part an assembly still uses. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/closeundohide.py

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


def box(depth=10):
    mcp.call("sketch", shape="rectangle", width_mm=20, height_mm=20)
    mcp.call("extrude_sketch", depth_mm=depth)


def features():
    return {f["name"]: f for f in mcp.call("inspect_model", what="features").json.get("features", [])}


def bodies():
    return mcp.call("inspect_model", what="bodies").json.get("bodies", [])


def components():
    return {c["name"]: c for c in mcp.call("inspect_model", what="components").json["components"]}


def active():
    return mcp.call("get_active_document_name").text


def triangles():
    text = mcp.call("render_views").text
    return int(text.split(" triangles")[0].rsplit(" ", 1)[1])


# ---- undo, part
mcp.call("create_part")
box()
assert len(bodies()) == 1
r = mcp.call("undo")
assert "removed Boss-Extrude1 (boss-extrude)" in r.text and "from 4000.000 to 0.000 mm^3" in r.text, r.text
assert "Boss-Extrude1" not in features() and "Sketch1" in features()
assert not bodies()
# The rectangle sketch is several steps of SolidWorks', taken back whole: one
# alone would leave the sketch open for editing, and the other tools stuck.
# 3 steps, plus one per dimension since the rectangle got its width and height
# (2026-11-08, toolfixes.py).
r = mcp.call("undo")
assert "undid 5 steps" in r.text and "removed Sketch1 (sketch)" in r.text, r.text
assert "Sketch1" not in features()
# Nothing left.
r = mcp.call("undo", steps=5)
assert "are as they were" in r.text, r.text
for bad in (0, -1, 101):
    assert mcp.call("undo", steps=bad, expect_error=True).is_error

# An edit of an existing sketch isn't reliable to undo, which the tool description
# warns of (seen 2026-11-01). A line stays, whatever the steps; the reply says so
# and the sketch isn't left open.
mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=20, height_mm=20)
count = len(mcp.call("inspect_sketch", sketch_name="Sketch1").json["segments"])
mcp.call("sketch", shape="line", sketch_name="Sketch1", points=[[30, 30], [40, 40]])
r = mcp.call("undo", steps=3)
assert "are as they were" in r.text, r.text
assert len(mcp.call("inspect_sketch", sketch_name="Sketch1").json["segments"]) == count + 1
# A circle takes the whole sketch with it; the reply says that too.
mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=20, height_mm=20)
mcp.call("sketch", shape="circle", sketch_name="Sketch1", center=[50, 50], radius_mm=2)
r = mcp.call("undo")
assert "removed Sketch1 (sketch)" in r.text, r.text
assert "Sketch1" not in features()

# A delete comes back, a dimension goes back, and the model is usable after.
mcp.call("create_part")
box()
edge = mcp.call("inspect_model", what="edges", limit=1).json["edges"][0]["id"]
mcp.call("fillet_edges", edges=[edge], radius_mm=2)
mcp.call("delete_features", features=["Fillet1"])
r = mcp.call("undo")
assert "restored Fillet1 (fillet)" in r.text, r.text
assert "Fillet1" in features()
r = mcp.call("undo")
assert "removed Fillet1 (fillet)" in r.text, r.text
mcp.call("set_dimension", name="D1@Boss-Extrude1", value_mm=30)
assert abs(bodies()[0]["volume_mm3"] - 12000) < 1e-3
r = mcp.call("undo")
assert "from 12000.000 to 4000.000 mm^3" in r.text, r.text
assert abs(bodies()[0]["volume_mm3"] - 4000) < 1e-3
mcp.call("sketch", shape="rectangle", width_mm=4, height_mm=4, plane="top")
assert "Sketch2" in features()

# ---- set_visibility, part: a sketch, an absorbed sketch, planes, the body
assert features()["Sketch2"].get("hidden") is None
assert features()["Sketch1"].get("hidden") is True  # absorbed by the extrude
assert features()["Front Plane"].get("hidden") is True
r = mcp.call("set_visibility", items=["Sketch2", "Front Plane"], visible=False)
assert r.text.startswith("hid Sketch2 (sketch). Front Plane (reference geometry) already hidden."), r.text
assert features()["Sketch2"]["hidden"] is True
r = mcp.call("set_visibility", items=["Sketch2", "Front Plane"], visible=True)
assert r.text == "showed Sketch2 (sketch), Front Plane (reference geometry).", r.text
f = features()
assert "hidden" not in f["Sketch2"] and "hidden" not in f["Front Plane"]
r = mcp.call("set_visibility", items=[f["Front Plane"]["id"]], visible=False)
assert "hid Front Plane" in r.text, r.text
# A hidden sketch can still be used.
mcp.call("set_visibility", items=["Sketch2"], visible=False)
mcp.call("extrude_sketch", sketch_name="Sketch2", depth_mm=3)

listed = bodies()
assert len(listed) == 1 and "hidden" not in listed[0]
body = listed[0]["id"]
volume = listed[0]["volume_mm3"]
faces_before = triangles()
assert faces_before > 0
r = mcp.call("set_visibility", items=[body], visible=False)
assert r.text.startswith(f"hid {listed[0]['name']} (body)."), r.text
listed = bodies()
assert listed[0]["hidden"] is True and listed[0]["volume_mm3"] == volume
assert "nothing to draw" in mcp.call("render_views", expect_error=True).text
r = mcp.call("set_visibility", items=[listed[0]["id"]], visible=False)
assert "already hidden" in r.text and "Hidden things" not in r.text, r.text
# Hiding a body is one undo step.
r = mcp.call("undo")
assert "are as they were" in r.text, r.text
assert "hidden" not in bodies()[0]
mcp.call("set_visibility", items=[bodies()[0]["id"]], visible=False)
hidden_part = mcp.run_dir / "visibility-hidden.SLDPRT"
mcp.call("save_document", path=str(hidden_part))
mcp.call("close_document")
mcp.call("open_document", path=str(hidden_part))
listed = bodies()
assert listed[0]["hidden"] is True, listed
r = mcp.call("set_visibility", items=[listed[0]["id"]], visible=True)
assert r.text.startswith("showed"), r.text
assert triangles() == faces_before

# Refusals leave everything as it was.
face = mcp.call("inspect_model", what="faces", limit=1).json["faces"][0]["id"]
for items, wanted in (
    (["Boss-Extrude1"], "be hidden"),
    (["Sketch2", "Nope"], "no feature named"),
    ([face], "be hidden"),
    ([], "list what to hide"),
    (["feature:9999"], "feature:9999"),
):
    assert wanted in mcp.call("set_visibility", items=items, visible=False, expect_error=True).text, items
assert "hidden" not in bodies()[0]
assert features()["Sketch2"].get("hidden") is True  # absorbed by Boss-Extrude2 since it was hidden

# ---- close_document: names, refusals, unsaved changes
mcp.call("save_document")
block = hidden_part
mcp.call("create_part")
assert "already" not in active() and "Part" in active()
other = active().split(" ")[0]
r = mcp.call("close_document", document="not open", expect_error=True)
assert "no open document is called" in r.text and block.name in r.text, r.text
r = mcp.call("close_document", document="visibility-hidden.sldprt")  # a file name, in any case; saved, so no refusal
assert r.text.startswith(f"closed {block.name} (part,") and f"{other} (part, never saved) is the active document now." in r.text, r.text
assert active().startswith(other), "closing in the background moved the active document"

mcp.call("open_document", path=str(block))
mcp.call("set_dimension", name="D1@Boss-Extrude1", value_mm=30)
for args in ({}, {"document": block.name}, {"document": str(block)}):
    r = mcp.call("close_document", expect_error=True, **args)
    assert "has unsaved changes" in r.text, r.text
assert active().startswith(block.name)
r = mcp.call("close_document", discard_changes=True)
assert "its unsaved changes are lost" in r.text and f"{other} (part, never saved)" in r.text, r.text
mcp.call("open_document", path=str(block))
assert abs(float(com("$d = Com (Com $sw ActiveDoc -Get) Parameter @('D1@Boss-Extrude1'); Com $d SystemValue -Get").replace(",", ".")) - 0.01) < 1e-9, "the change wasn't lost"
mcp.call("close_document")
assert active().startswith(other)

# A saved document changed and saved closes without a word; a never-saved one
# with work in it is refused, then gone.
box()
assert "has unsaved changes" in mcp.call("close_document", expect_error=True).text
r = mcp.call("close_document", document=other, discard_changes=True)
assert "it was never saved, so it's gone" in r.text, r.text
while "No document is open now." not in r.text:  # the parts from above
    r = mcp.call("close_document", discard_changes=True)
assert "no document is open" in mcp.call("close_document", expect_error=True).text.lower()
assert "no document is open" in mcp.call("undo", expect_error=True).text.lower()

# ---- an assembly: components, a part it still uses, undo
mcp.call("create_part")
box()
part = mcp.run_dir / "visibility-block.SLDPRT"
mcp.call("save_document", path=str(part))
mcp.call("create_assembly")
mcp.call("insert_components", file=str(part))
mcp.call("insert_components", file=str(part), at=[{"origin_mm": [0, 60, 0]}])
assembly = mcp.run_dir / "visibility-assembly.SLDASM"
mcp.call("save_document", path=str(assembly))
c = components()
ids = [c["visibility-block-1"]["id"], c["visibility-block-2"]["id"]]
both = triangles()
r = mcp.call("set_visibility", items=[ids[1]], visible=False)
assert r.text.startswith("hid visibility-block-2 (component)."), r.text
assert components()["visibility-block-2"]["hidden"] is True
assert triangles() * 2 == both
# Hiding a component isn't an undo step (seen 2026-11-01).
mcp.call("undo")
assert components()["visibility-block-2"].get("hidden") is True
body = mcp.call("inspect_model", what="bodies").json["bodies"][0]["id"]
assert "in a component" in mcp.call("set_visibility", items=[body], visible=False, expect_error=True).text
assert "be hidden" in mcp.call("set_visibility", items=["visibility-block-1"], visible=False, expect_error=True).text
r = mcp.call("set_visibility", items=ids, visible=True)
assert r.text.startswith("showed visibility-block-2 (component). visibility-block-1 (component) already shown."), r.text
assert triangles() == both
# Reference geometry of the assembly itself.
r = mcp.call("set_visibility", items=["Top Plane"], visible=True)
assert "showed Top Plane" in r.text, r.text
# A suppressed component is refused.
com(f"""
$doc = Com $sw ActiveDoc -Get
$c = @(Com $doc GetComponents @($true)) | Where-Object {{ (Com $_ Name2 -Get) -eq 'visibility-block-2' }}
Com $c Select4 @($false, [Runtime.InteropServices.DispatchWrapper]::new([object]$null), $false) | Out-Null
Com $doc EditSuppress2 | Out-Null
Com $doc ClearSelection2 @($true) | Out-Null
""")
assert "suppressed" in mcp.call("set_visibility", items=[ids[1]], visible=False, expect_error=True).text
r = mcp.call("undo")
print("    undo of a suppress:", r.text)
assert "suppressed" not in components()["visibility-block-2"].get("state", ""), components()

# A subassembly's parts go with it.
mcp.call("save_document")
mcp.call("create_assembly")
mcp.call("insert_components", file=str(assembly))
c = components()
print("    components:", {n: (v.get("hidden"), v.get("subassembly")) for n, v in c.items()})
sub = next(v["id"] for v in c.values() if v.get("subassembly"))
nested = next(v["id"] for v in c.values() if not v.get("subassembly"))
both = triangles()
mcp.call("set_visibility", items=[sub], visible=False)
assert "nothing to draw" in mcp.call("render_views", expect_error=True).text
mcp.call("set_visibility", items=[sub], visible=True)
assert triangles() == both
mcp.call("set_visibility", items=[nested], visible=False)
assert triangles() * 2 == both
mcp.call("set_visibility", items=[nested], visible=True)
assert "unsaved" in mcp.call("close_document", expect_error=True).text
mcp.call("close_document", discard_changes=True)

# The part's own window closes, the assembly keeps it loaded and changed.
mcp.call("open_document", path=str(part))
mcp.call("set_dimension", name="D1@Boss-Extrude1", value_mm=30)
assert "has unsaved changes" in mcp.call("close_document", expect_error=True).text
r = mcp.call("close_document", discard_changes=True)
assert "keeps it loaded for that document, with its unsaved changes" in r.text, r.text
assert active().startswith(assembly.name), active()
assert abs(mcp.call("inspect_model", what="components").json["components"][0]["bbox_mm"]["max"][0] - 30) < 1e-6
r = mcp.call("close_document", document=assembly.name, discard_changes=True)
assert "No document is open now." in r.text, r.text
com("if (@(Com $sw GetDocuments).Count -gt 1) { throw 'documents left' }; 'count ' + (Com $sw GetDocumentCount)")

# ---- undo in a drawing, whose sheet always counts as an open sketch
mcp.call("create_drawing")
mcp.call("add_drawing_note", text="undo me", at_mm=[50, 50])


def notes():
    return [a for s in mcp.call("inspect_drawing_annotations").json["sheets"] for a in s.get("annotations", [])]


assert len(notes()) == 1, notes()
r = mcp.call("undo")
print("    undo in a drawing:", r.text)
assert not notes(), notes()
mcp.call("close_document", discard_changes=True)

assert not sw_dialogs(), sw_dialogs()

# save_document / open_document against real SolidWorks: save in place and
# Save As, exports, copies, refusing to overwrite, opening (and re-activating)
# parts, an assembly and a drawing, read-only, and an assembly whose part has
# gone missing (docs/notes/files.md). The assembly and drawing are made
# through COM as in drawings.py. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/files.py

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


def x_extent():
    bodies = mcp.call("inspect_model", what="bodies").json["bodies"]
    assert len(bodies) == 1, bodies
    b = bodies[0]["bbox_mm"]
    return round(b["max"][0] - b["min"][0], 3)


def active():
    return mcp.call("get_active_document_name").text


def close_all():
    com("Com $sw CloseAllDocuments @($true) | Out-Null")


out = mcp.run_dir.resolve()
block = out / "files-block.SLDPRT"

# ---- Saving a part ------------------------------------------------------------
# 40 x 20 box, 10 deep along X (the Front Plane faces +X on this machine).
mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
assert "never saved" in active(), active()

r = mcp.call("save_document", expect_error=True)
assert "never been saved" in r.text, r.text
r = mcp.call("save_document", path="files-block.SLDPRT", expect_error=True)
assert "absolute" in r.text, r.text
r = mcp.call("save_document", path=str(out / "files-block.docx"), expect_error=True)
assert "format" in r.text, r.text
r = mcp.call("save_document", path=str(out / "no-such-folder" / "a.SLDPRT"), expect_error=True)
assert "no folder" in r.text, r.text
r = mcp.call("save_document", path=str(out / "files-block.SLDASM"), expect_error=True)
assert "doesn't hold a part" in r.text, r.text

r = mcp.call("save_document", path=str(block))
assert "saved as" in r.text and block.is_file(), r.text
assert str(block) in active() and "files-block" in active(), active()
r = mcp.call("save_document", path=str(block), expect_error=True)
assert "already exists" in r.text, r.text

# Change it and save in place.
before = block.stat().st_mtime_ns
mcp.call("set_dimension", name="D1@Boss-Extrude1", value_mm=15)
r = mcp.call("save_document")
assert "saved files-block" in r.text, r.text
assert block.stat().st_mtime_ns > before, "the file wasn't rewritten"

# ---- Exports and copies -------------------------------------------------------
for name, check in [("files-block.STEP", lambda b: b.startswith(b"ISO-10303-21")),
                    ("files-block.stl", lambda b: len(b) > 84),
                    ("files-block.x_t", lambda b: len(b) > 0),
                    ("files-block.IGS", lambda b: len(b) > 0)]:
    r = mcp.call("save_document", path=str(out / name))
    data = (out / name).read_bytes()
    assert "exported" in r.text and check(data), (r.text, data[:40])
    assert str(block) in active(), "an export changed the active document's file"
r = mcp.call("save_document", path=str(out / "files-block.STEP"), expect_error=True)
assert "already exists" in r.text, r.text
r = mcp.call("save_document", path=str(out / "files-block.STEP"), overwrite=True)
assert "exported" in r.text, r.text
r = mcp.call("save_document", path=str(out / "files-block.pdf"), expect_error=True)
assert "doesn't hold a part" in r.text, r.text

copy = out / "files-block-copy.SLDPRT"
r = mcp.call("save_document", path=str(copy), copy=True)
assert "copy" in r.text and copy.is_file(), r.text
assert str(block) in active(), active()

# ---- Opening ------------------------------------------------------------------
close_all()
assert "no active document" in active(), active()
r = mcp.call("open_document", path=str(block))
assert "opened files-block" in r.text, r.text
assert x_extent() == 15, x_extent()

mcp.call("create_part")
assert "never saved" in active(), active()
r = mcp.call("open_document", path=str(block))
assert "already open" in r.text and str(block) in active(), (r.text, active())
assert x_extent() == 15, x_extent()

r = mcp.call("open_document", path=str(out / "nope.SLDPRT"), expect_error=True)
assert "no file" in r.text, r.text
r = mcp.call("open_document", path=str(out / "files-block.STEP"), expect_error=True)
assert "SolidWorks files" in r.text, r.text

# Read-only: opens, but saving in place is refused.
r = mcp.call("open_document", path=str(copy), read_only=True)
assert "opened files-block-copy" in r.text, r.text
r = mcp.call("save_document", expect_error=True)
print("    read-only save:", r.text)
assert "read-only" in r.text, r.text

# ---- An assembly --------------------------------------------------------------
assembly = out / "files-pair.SLDASM"
mcp.call("open_document", path=str(block))
com(f"""$template = Com $sw GetUserPreferenceStringValue @(9)  # swDefaultTemplateAssembly
$a = Com $sw NewDocument @($template, 0, 0, 0); if (-not $a) {{ throw "NewDocument($template) failed" }}
foreach ($o in 0.0, 0.06) {{
    if (-not (Com $a AddComponent5 @('{block}', 0, '', $false, '', 0.0, $o, $o))) {{ throw 'AddComponent5 failed' }}
}}""")
r = mcp.call("save_document", path=str(assembly))
assert "assembly" in r.text and assembly.is_file(), r.text
r = mcp.call("save_document", path=str(out / "files-pair.STEP"))
assert "exported" in r.text, r.text

close_all()
r = mcp.call("open_document", path=str(assembly))
assert "opened files-pair" in r.text and "assembly" in r.text, r.text
components = mcp.call("inspect_model", what="components").json["components"]
assert len(components) == 2, components

# ---- A drawing ----------------------------------------------------------------
drawing = out / "files-block.SLDDRW"
com(f"""$template = Com $sw GetUserPreferenceStringValue @(10)  # swDefaultTemplateDrawing
$d = Com $sw NewDocument @($template, 12, 0.42, 0.297); if (-not $d) {{ throw "NewDocument($template) failed" }}
if (-not (Com $d CreateDrawViewFromModelView3 @('{block}', '*Front', 0.1, 0.15, 0.0))) {{ throw 'Front view failed' }}""")
r = mcp.call("save_document", path=str(drawing))
assert "drawing" in r.text and drawing.is_file(), r.text
for name in ["files-block.pdf", "files-block.dxf"]:
    r = mcp.call("save_document", path=str(out / name))
    assert "exported" in r.text and "reported" not in r.text and (out / name).stat().st_size > 0, r.text
assert (out / "files-block.pdf").read_bytes().startswith(b"%PDF"), "not a PDF"
r = mcp.call("save_document", path=str(out / "files-drawing.STEP"), expect_error=True)
assert "doesn't hold a drawing" in r.text, r.text

close_all()
r = mcp.call("open_document", path=str(drawing))
assert "opened files-block" in r.text and "drawing" in r.text, r.text
assert mcp.call("inspect_drawing").json, "inspect_drawing on the opened drawing"

# ---- Missing references ---------------------------------------------------------
# The assembly's part renamed away: SolidWorks must not ask where it went.
close_all()
moved = out / "files-block-moved.SLDPRT"
block.rename(moved)
try:
    r = mcp.call("open_document", path=str(assembly))
    assert "opened files-pair" in r.text and "suppressed" in r.text, r.text
    components = mcp.call("inspect_model", what="components").json["components"]
    assert [c.get("state") for c in components] == ["suppressed"] * 2, components
    assert not sw_dialogs(), sw_dialogs()
finally:
    close_all()
    moved.rename(block)

assert not sw_dialogs(), sw_dialogs()

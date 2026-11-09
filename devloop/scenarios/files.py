# save_document / open_document against real SolidWorks: save in place and
# Save As, exports, copies, refusing to overwrite, opening (and re-activating)
# parts, an assembly and a drawing, read-only, and an assembly whose part has
# gone missing; a multi-sheet drawing's PDF (all / named sheets, color); new
# drawings from templates, sheet formats, paper sizes and unit systems
# (docs/notes/files.md). The assembly's components and the
# drawing's view are made through COM as in drawings.py. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/files.py

import re
import subprocess
import zlib
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


def pdf_pages(path: Path) -> int:
    """Page objects in a PDF, looking into compressed (object) streams too."""
    data = path.read_bytes()
    texts = [data]
    for stream in re.finditer(rb"stream\r?\n(.*?)endstream", data, re.S):
        try:
            texts.append(zlib.decompress(stream.group(1)))
        except zlib.error:
            pass
    pages = sum(len(re.findall(rb"/Type\s*/Page(?![a-zA-Z])", t)) for t in texts)
    print(f"    {path.name}: {pages} pages")
    return pages


out = mcp.run_dir.resolve()
block = out / "files-block.SLDPRT"

# ---- Saving a part ------------------------------------------------------------
# 40 x 20 box, 10 deep along X (the Front Plane faces +X on this machine).
mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=40, height_mm=20)
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
# Importing its STEP would make a second files-block.SLDPRT (importing.py).
r = mcp.call("open_document", path=str(out / "files-block.STEP"), expect_error=True)
assert "a document called files-block.SLDPRT is open" in r.text, r.text

# Read-only: opens, but saving in place is refused.
r = mcp.call("open_document", path=str(copy), read_only=True)
assert "opened files-block-copy" in r.text, r.text
r = mcp.call("save_document", expect_error=True)
print("    read-only save:", r.text)
assert "read-only" in r.text, r.text

# ---- An assembly --------------------------------------------------------------
assembly = out / "files-pair.SLDASM"
mcp.call("open_document", path=str(block))
r = mcp.call("create_assembly")
assert r.text.startswith("created assembly "), r.text
assert "assembly" in active() and "never saved" in active(), active()
assert mcp.call("inspect_model", what="components").json.get("components", []) == [], "new assembly not empty"
assert not (open_dialogs := sw_dialogs()), open_dialogs
com(f"""$a = Com $sw ActiveDoc -Get
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

# ---- New drawings -------------------------------------------------------------
close_all()


def sheet():
    sheets = mcp.call("inspect_drawing").json["sheets"]
    assert len(sheets) == 1, sheets
    return sheets[0]


def unit_system():
    return int(com("Com (Com (Com $sw ActiveDoc -Get) Extension -Get) GetUserPreferenceInteger @(263, 0)"))


def doc_count():
    return int(com("Com $sw GetDocumentCount"))


# The stock template has no sheet of its own: A3 by default, no sheet format.
r = mcp.call("create_drawing")
assert r.text.startswith("created drawing ") and "A3 (420 x 297 mm), no sheet format" in r.text, r.text
assert "drawing" in active() and "never saved" in active(), active()
assert (sheet()["paper"], sheet()["size_mm"]) == ("A3", [420.0, 297.0]), sheet()
assert "MMGS" in r.text and unit_system() == 5, r.text

r = mcp.call("create_drawing", template="Drawing", sheet_format="a3 - iso", units="ips")
assert "A3 (420 x 297 mm), sheet format a3 - iso" in r.text and "units IPS" in r.text, r.text
assert sheet()["size_mm"] == [420.0, 297.0] and unit_system() == 3, sheet()
r = mcp.call("create_drawing", sheet_format="a3 - iso", paper_size="A4_portrait")
assert "A4 portrait (210 x 297 mm), sheet format a3 - iso" in r.text, r.text
r = mcp.call("create_drawing", paper_size_mm=[500, 300], units="cgs")
assert "500 x 300 mm" in r.text and "units CGS" in r.text, r.text
assert sheet()["size_mm"] == [500.0, 300.0] and unit_system() == 1, sheet()

# A template with a sheet of its own: NewDocument ignores the paper size
# asked for, so it's set up afterwards; without one the template's stays.
own_template = out / "files-a2-iso.drwdot"
com(f"""$d = Com $sw ActiveDoc -Get
if (-not (Com $d SetupSheet5 @([string]'Sheet1', [int]9, [int]12, 1.0, 1.0, $true, [string](Join-Path (Com $sw GetUserPreferenceStringValue @(7)) 'a2 - iso.slddrt'), [double]0, [double]0, 'Default', $true))) {{ throw 'SetupSheet5 failed' }}
if ((Com $d SaveAs3 @([string]'{own_template}', [int]0, [int]1)) -ne 0) {{ throw 'SaveAs3 failed' }}""")
r = mcp.call("create_drawing", template=str(own_template))
assert "A2 (594 x 420 mm), sheet format a2 - iso" in r.text and "first-angle" in r.text, r.text
r = mcp.call("create_drawing", template=str(own_template), paper_size="A1")
assert "A1 (841 x 594 mm), sheet format a2 - iso" in r.text, r.text
assert sheet()["size_mm"] == [841.0, 594.0], sheet()

# Refusals: unknown names list what there is; a template that makes another
# kind of document leaves nothing open.
before = doc_count()
r = mcp.call("create_drawing", template="no such template", expect_error=True)
assert "there are: " in r.text and "Drawing" in r.text, r.text
r = mcp.call("create_drawing", sheet_format="a3 - nope", expect_error=True)
assert "a3 - iso" in r.text, r.text
part_template = out / "files-part-as.drwdot"
part_template.write_bytes(Path(com("Com $sw GetUserPreferenceStringValue @(8)")).read_bytes())
r = mcp.call("create_drawing", template=str(part_template), expect_error=True)
assert "couldn't create a drawing" in r.text, r.text
r = mcp.call("create_drawing", paper_size="A3", paper_size_mm=[100, 100], expect_error=True)
assert doc_count() == before, f"{doc_count()} documents open, {before} before"
assert not (open_dialogs := sw_dialogs()), open_dialogs

# ---- A drawing ----------------------------------------------------------------
close_all()
drawing = out / "files-block.SLDDRW"
r = mcp.call("create_drawing", paper_size="A3")
com(f"""$d = Com $sw ActiveDoc -Get
if (-not (Com $d CreateDrawViewFromModelView3 @('{block}', '*Front', 0.1, 0.15, 0.0))) {{ throw 'Front view failed' }}""")
r = mcp.call("save_document", path=str(drawing))
assert "drawing" in r.text and drawing.is_file(), r.text
for name in ["files-block.pdf", "files-block.dxf"]:
    r = mcp.call("save_document", path=str(out / name))
    assert "exported" in r.text and "reported" not in r.text and (out / name).stat().st_size > 0, r.text
assert (out / "files-block.pdf").read_bytes().startswith(b"%PDF"), "not a PDF"
assert pdf_pages(out / "files-block.pdf") == 1
r = mcp.call("save_document", path=str(out / "files-drawing.STEP"), expect_error=True)
assert "doesn't hold a drawing" in r.text, r.text

# A drawing's PDF: every sheet by default, or the ones named (in the
# drawing's order, whatever order they're asked in); color for the export
# only. Details stays the active sheet.
mcp.call("add_sheet", name="Details", paper_size="A4")
mcp.call("add_sheet")
mcp.call("activate_sheet", sheet="Sheet1")
mcp.call("activate_sheet", sheet="Details")
color_before = com("Com $sw GetUserPreferenceToggle @(323)")
r = mcp.call("save_document", path=str(out / "files-all.pdf"))
assert "3 pages, one per sheet: Sheet1, Details, Sheet3" in r.text, r.text
assert pdf_pages(out / "files-all.pdf") == 3
r = mcp.call("save_document", path=str(out / "files-two.pdf"), pdf_sheets=["sheet3", "Sheet1"], pdf_color=False)
assert "2 pages, one per sheet: Sheet1, Sheet3; black and white" in r.text, r.text
assert pdf_pages(out / "files-two.pdf") == 2
assert com("Com $sw GetUserPreferenceToggle @(323)") == color_before, "PDF color option not put back"
r = mcp.call("save_document", path=str(out / "files-one.pdf"), pdf_sheets=["Details"], pdf_color=True)
assert "1 page, one per sheet: Details; in color" in r.text, r.text
assert pdf_pages(out / "files-one.pdf") == 1
active_sheets = [s["name"] for s in mcp.call("inspect_drawing").json["sheets"] if s.get("active")]
assert active_sheets == ["Details"], active_sheets
r = mcp.call("save_document", path=str(out / "files-none.pdf"), pdf_sheets=["Nope"], expect_error=True)
assert "no sheet \"Nope\"" in r.text and "Details" in r.text and not (out / "files-none.pdf").exists(), r.text
r = mcp.call("save_document", path=str(out / "files-sheets.dxf"), pdf_sheets=["Sheet1"], expect_error=True)
assert "for a PDF export" in r.text, r.text
# Untested before: a multi-sheet DXF -- one file, or one per sheet?
r = mcp.call("save_document", path=str(out / "files-sheets.dxf"))
print(f"    multi-sheet DXF: {r.text!r}; files: {sorted(p.name for p in out.glob('files-sheets*'))}")
assert not (open_dialogs := sw_dialogs()), open_dialogs

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
    assert not (open_dialogs := sw_dialogs()), open_dialogs
finally:
    close_all()
    moved.rename(block)

assert not (open_dialogs := sw_dialogs()), open_dialogs

# add_sheet / activate_sheet / edit_sheet against real SolidWorks
# (docs/notes/drawings.md, "Sheets"): scale, projection, paper size and sheet
# format of a sheet with views, new sheets copied from the active one or set
# up as asked, activating, renaming, and refusals that change nothing. The
# drawing's three standard views are made through COM (no tool makes views
# yet). Run with:
#   py devloop/mcp_client.py run devloop/scenarios/sheets.py

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


def drawing():
    return mcp.call("inspect_drawing").json


def sheets():
    return {s["name"]: s for s in drawing()["sheets"]}


def setup(sheet):
    return (sheet.get("paper"), sheet["size_mm"], sheet["scale"], sheet["projection"])


def views(sheet):
    return {v["name"]: v for v in sheet["views"]}


def active():
    return [name for name, s in sheets().items() if s.get("active")]


def width_mm(view):
    (x0, _), (x1, _) = view["outline"]
    return x1 - x0


# ---- The part: a 40 x 20 x 10 box (x 0..10 on this machine) with a boss on
# top, so its top and bottom views differ.
mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
faces = mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]
top = [f["id"] for f in faces if f["normal"][2] > 0.99][0]
mcp.call("sketch", shape="circle", on=top, center=[5, 0, 10], diameter_mm=6)
mcp.call("extrude_sketch", depth_mm=8)
part = mcp.run_dir.resolve() / "sheets-block.SLDPRT"
mcp.call("save_document", path=str(part))

# ---- The drawing: A3, a3 - iso, third angle; front, top (above) and right
# views. Create3rdAngleViews2 scales the sheet to fit them (2:1 here).
mcp.call("create_drawing", sheet_format="a3 - iso")
com(f"""$d = Com $sw ActiveDoc -Get
if (-not (Com $d Create3rdAngleViews2 @('{part}'))) {{ throw 'Create3rdAngleViews2 failed' }}""")
s1 = sheets()["Sheet1"]
assert setup(s1) == ("A3", [420.0, 297.0], "2:1", "third_angle"), s1
v = views(s1)
assert [v[n]["type"] for n in sorted(v)] == ["named", "projected", "projected"], v
top_view = [n for n in v if v[n]["type"] == "projected" and v[n]["outline"][0][1] > v["Drawing View1"]["outline"][1][1]][0]
assert v[top_view]["toward_viewer"] == "+Z", v[top_view]  # the top, seen from above
front_width = width_mm(v["Drawing View1"])

# ---- Scale: the views follow (the projected ones through their parent).
r = mcp.call("edit_sheet", scale="1:1")
assert r.text.startswith("set up Sheet1, A3 (420 x 297 mm), sheet format a3 - iso, scale 1:1, third-angle"), r.text
assert "Drawing View1 at 1:1 (sheet scale)" in r.text and "(projected from Drawing View1)" in r.text, r.text
s1 = sheets()["Sheet1"]
assert s1["scale"] == "1:1" and all(x["scale"] == "1:1" for x in s1["views"]), s1
# The 40 mm box went from 80 to 40 sheet mm (outlines have a fixed margin).
assert abs(front_width - width_mm(views(s1)["Drawing View1"]) - 40) < 0.5, (front_width, s1)

# ---- Projection: the view above the front now shows the bottom.
r = mcp.call("edit_sheet", projection="first_angle")
assert "first-angle projection" in r.text and "projected views stayed" in r.text, r.text
s1 = sheets()["Sheet1"]
assert s1["projection"] == "first_angle", s1
assert views(s1)[top_view]["toward_viewer"] == "-Z", views(s1)[top_view]
# ... and draws it: the boss's circle is hidden from below. (Without the
# add-in redrawing it, the view kept the top's lines, circle included.)
seen = [v for s in mcp.call("inspect_drawing", view=top_view).json["sheets"] for v in s["views"] if v["name"] == top_view][0]
assert sorted(e["type"] for e in seen["entities"]) == ["line"] * 4, seen["entities"]
mcp.call("render_drawing", max_px=900)

# ---- Paper size: the format is kept (and said so); views past the edge are flagged.
r = mcp.call("edit_sheet", paper_size="A4")
assert "A4 landscape (297 x 210 mm), sheet format a3 - iso" in r.text, r.text
assert "a3 - iso was kept" in r.text and "reach past the sheet's edges" in r.text and top_view in r.text, r.text
assert setup(sheets()["Sheet1"])[:2] == ("A4 landscape", [297.0, 210.0])

# A sheet format alone brings its own size; both together give the size asked for.
r = mcp.call("edit_sheet", sheet_format="a4 - iso")
assert "A4 portrait (210 x 297 mm), sheet format a4 - iso" in r.text, r.text
r = mcp.call("edit_sheet", sheet_format="a3 - iso", paper_size="A3")
assert "A3 (420 x 297 mm), sheet format a3 - iso" in r.text and "was kept" not in r.text, r.text
assert "reach past" not in r.text, r.text
assert setup(sheets()["Sheet1"]) == ("A3", [420.0, 297.0], "1:1", "first_angle")

# ---- New sheets: a copy of the active one, or set up as asked; each becomes active.
r = mcp.call("add_sheet")
assert r.text.startswith("added Sheet2, A3 (420 x 297 mm), sheet format a3 - iso, scale 1:1, first-angle"), r.text
assert "it's now the active sheet" in r.text and "no views yet" in r.text, r.text
assert active() == ["Sheet2"], active()

r = mcp.call("add_sheet", name="Details", paper_size="A4", sheet_format="a4 - iso", scale="1:5",
             projection="third_angle")
assert r.text.startswith("added Details, A4 landscape (297 x 210 mm), sheet format a4 - iso, scale 1:5, third"), r.text
all_sheets = sheets()
assert list(all_sheets) == ["Sheet1", "Sheet2", "Details"] and active() == ["Details"], all_sheets
assert setup(all_sheets["Details"]) == ("A4 landscape", [297.0, 210.0], "1:5", "third_angle"), all_sheets
r = mcp.call("add_sheet", paper_size_mm=[500, 250])
assert r.text.startswith("added Sheet4, 500 x 250 mm"), r.text

# ---- Activating (names in any case) and changing a sheet in the background.
r = mcp.call("activate_sheet", sheet="sheet1")
assert r.text.startswith("Sheet1 is now the active sheet: Sheet1, A3") and "3 view(s)" in r.text, r.text
assert active() == ["Sheet1"], active()
r = mcp.call("edit_sheet", sheet="Details", scale="1:2", rename="Detail views")
assert "set up Detail views, A4 landscape" in r.text and "scale 1:2" in r.text, r.text
assert "the active sheet is still Sheet1" in r.text, r.text
all_sheets = sheets()
assert list(all_sheets) == ["Sheet1", "Sheet2", "Detail views", "Sheet4"], all_sheets
assert all_sheets["Detail views"]["scale"] == "1:2" and active() == ["Sheet1"], all_sheets

# ---- Refusals change nothing.
before = sheets()
for args, expected in [
    (("add_sheet", {"name": "Sheet1"}), 'already has a sheet named "Sheet1"'),
    (("add_sheet", {"name": "SHEET2"}), 'already has a sheet named "Sheet2"'),
    (("add_sheet", {"sheet_format": "a3 - nope"}), "a4 - iso"),
    (("edit_sheet", {"sheet": "nope", "scale": "1:2"}), "its sheets: Sheet1, Sheet2, Detail views, Sheet4"),
    (("edit_sheet", {"rename": "sheet2"}), 'already has a sheet named "Sheet2"'),
    (("edit_sheet", {"scale": "1/2"}), "numerator:denominator"),
    (("edit_sheet", {"sheet": "Sheet2"}), "nothing to change"),
    (("edit_sheet", {"paper_size": "A3", "paper_size_mm": [100, 100]}), "not both"),
    (("activate_sheet", {"sheet": "nope"}), "no sheet named"),
]:
    r = mcp.call(args[0], expect_error=True, **args[1])
    assert expected in r.text, (args, r.text)
# A name SolidWorks won't take, after a scale it did: the scale is put back.
r = mcp.call("edit_sheet", scale="1:4", rename="a@b", expect_error=True)
assert "a@b" in r.text and "as the sheet" in r.text, r.text
after = sheets()
assert {n: setup(s) for n, s in after.items()} == {n: setup(s) for n, s in before.items()}, (before, after)
assert active() == ["Sheet1"], active()
assert not (open_dialogs := sw_dialogs()), open_dialogs

# ---- Not a drawing.
mcp.call("create_part")
r = mcp.call("add_sheet", expect_error=True)
assert "isn't a drawing" in r.text, r.text

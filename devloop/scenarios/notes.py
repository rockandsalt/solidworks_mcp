# add_drawing_note / inspect_drawing_annotations / edit_drawing_annotation /
# delete_drawing_annotations against real SolidWorks (docs/notes/drawings.md,
# "Notes and leaders"): notes on the sheet, in a view (given in sheet and in
# view coordinates, following the view when it moves) and in the sheet
# format; leaders to edges and a circle with their arrows on the entity;
# style, text, moving, attaching to a view, replacing leaders; a dimension
# moved; refusals that change nothing; the warnings for notes that look
# misplaced; the user's sheet-format edit mode kept; deleting. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/notes.py

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


def annotations(**filters):
    """name -> entry, over every sheet."""
    j = mcp.call("inspect_drawing_annotations", **filters).json
    return {a["name"]: a for s in j["sheets"] for a in s["annotations"]}


def view_info(name):
    j = mcp.call("inspect_drawing_annotations", view=name).json
    return [v for s in j["sheets"] for v in s["views"] if v["name"] == name][0]


def entities(view):
    found = [v for s in mcp.call("inspect_drawing", view=view).json["sheets"] for v in s["views"] if v["name"] == view]
    return found[0]["entities"]


def entry(text):
    """The JSON line of an add/edit reply."""
    return __import__("json").loads(text.splitlines()[-1])


def near(a, b, tol=0.05):
    return all(abs(x - y) <= tol for x, y in zip(a, b))


def overlaps(box, outline):
    (ax0, ay0), (ax1, ay1) = box
    (bx0, by0), (bx1, by1) = outline
    return ax0 < bx1 and bx0 < ax1 and ay0 < by1 and by0 < ay1


# ---- A 40 x 20 x 10 box with a 6 mm boss on top; a drawing with its front
# at 1:1 and its top at 1:2.
mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
faces = mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]
top = [f["id"] for f in faces if f["normal"][2] > 0.99][0]
mcp.call("sketch", shape="circle", on=top, center=[5, 0, 10], diameter_mm=6)
mcp.call("extrude_sketch", depth_mm=8)
mcp.call("save_document", path=str(mcp.run_dir.resolve() / "notes-block.SLDPRT"))
mcp.call("create_drawing", sheet_format="a3 - iso")
mcp.call("create_drawing_view", at_mm=[120, 150], scale="1:1")
mcp.call("create_drawing_view", orientation="top", at_mm=[300, 150], scale="1:2")

# The title block's notes are the sheet format's, not the sheet's.
s1 = mcp.call("inspect_drawing").json["sheets"][0]
assert s1.get("sheet_format_annotation_counts", {}).get("note", 0) > 20, s1
assert "sheet_annotation_counts" not in s1, s1
j = mcp.call("inspect_drawing_annotations", kind="note").json
assert j["sheets"][0]["annotations"] == [] and "sheet format annotation(s)" in j["note"], j
title_block = annotations(include_sheet_format=True, kind="note")
assert title_block and all(a["in"] == "sheet_format" for a in title_block.values()), title_block
assert all("imported" not in a for a in title_block.values()), title_block

# ---- A sheet note, at sheet mm.
r = mcp.call("add_drawing_note", text="SHEET NOTE", at_mm=[50, 250])
assert "on sheet Sheet1: text's upper-left corner at [50, 250] sheet mm" in r.text, r.text
sheet_note = entry(r.text)["name"]
assert entry(r.text)["in"] == "sheet" and "at_view_mm" not in entry(r.text), r.text

# ---- A note in the 1:2 top view, given in the view's own coordinates: 25
# view mm up from the model origin is 12.5 sheet mm.
v2 = view_info("Drawing View2")
ox, oy = v2["view_origin_mm"]
r = mcp.call("add_drawing_note", text="VIEW NOTE", view="Drawing View2", at_view_mm=[0, 25])
e = entry(r.text)
view_note = e["name"]
assert e["in"] == "view Drawing View2" and "(it moves with the view)" in r.text, r.text
assert near(e["at_mm"], [ox, oy + 12.5]) and near(e["at_view_mm"], [0, 25]), (e, v2)

# Moving the view takes its note along (sheet mm change, view mm don't);
# the sheet note stays.
mcp.call("edit_drawing_view", view="Drawing View2", at_mm=[320, 150])
a = annotations()
assert near(a[view_note]["at_mm"], [ox + 20, oy + 12.5]) and near(a[view_note]["at_view_mm"], [0, 25]), a[view_note]
assert near(a[sheet_note]["at_mm"], [50, 250]), a[sheet_note]

# ---- A sheet-format note: same coordinates as the sheet.
r = mcp.call("add_drawing_note", text="FORMAT NOTE", **{"in": "sheet_format"}, at_mm=[100, 50])
e = entry(r.text)
format_note = e["name"]
assert e["in"] == "sheet_format" and near(e["at_mm"], [100, 50]) and "title block layer" in r.text, r.text
assert format_note not in annotations(), "sheet format notes are only listed when asked for"
assert near(annotations(**{"in": "sheet_format"})[format_note]["at_mm"], [100, 50])

# The user's "edit sheet format" mode: a sheet note still goes on the sheet,
# and the mode is back after.
com("""$d = Com $sw ActiveDoc -Get
Com $d EditTemplate | Out-Null
'editSheet=' + (Com $d GetEditSheet)""")
r = mcp.call("add_drawing_note", text="WHILE EDITING FORMAT", at_mm=[50, 230])
assert entry(r.text)["in"] == "sheet", r.text
editing_format_note = entry(r.text)["name"]
assert com("""$d = Com $sw ActiveDoc -Get
'editSheet=' + (Com $d GetEditSheet)
Com $d EditSheet | Out-Null""").startswith("editSheet=False")

# ---- Leaders. The front view's top edge: the arrow at its middle -- where
# the boss's base edge lies hidden under it, so a pick at the point alone
# would get the boss (it did, with SelectByID2) -- and the text outside the
# view.
front = entities("Drawing View1")
lines = [e for e in front if e["type"] == "line"]
top_edge = max((e for e in lines if abs(e["start"][1] - e["end"][1]) < 0.01), key=lambda e: e["start"][1])
x0, x1 = sorted([top_edge["start"][0], top_edge["end"][0]])
on_top = [x0 + 0.75 * (x1 - x0), top_edge["start"][1]]
outline1 = view_info("Drawing View1")["outline"]
r = mcp.call("add_drawing_note", text="LEADER NOTE", attach_to=[top_edge["id"]])
e = entry(r.text)
leader_note = e["name"]
assert "no arrow is at" not in r.text, r.text
assert e["in"] == "view Drawing View1" and len(e["leaders"]) == 1, r.text
assert near(e["leaders"][0]["arrow_at_mm"], [(x0 + x1) / 2, on_top[1]]), (e, top_edge)
assert e.get("attached") == [top_edge["id"]], e
assert "placed next to the first arrow" in r.text, r.text
text_box = [[e["at_mm"][0], e["at_mm"][1] - 3], [e["at_mm"][0] + 30, e["at_mm"][1]]]
assert not overlaps(text_box, outline1), (e, outline1)

# Two leaders, arrows given a little off the edges: moved onto them.
side_edges = sorted((e for e in lines if abs(e["start"][0] - e["end"][0]) < 0.01), key=lambda e: e["start"][0])
left, right = side_edges[0], side_edges[-1]
r = mcp.call("add_drawing_note", text="BOTH SIDES", attach_to=[left["id"], right["id"]],
             arrow_at_mm=[[left["start"][0] - 1, 150], [right["start"][0] + 2, 145]], at_mm=[120, 200])
e = entry(r.text)
two_leaders = e["name"]
# (SolidWorks numbers the leaders in its own order.)
tips = [l["arrow_at_mm"] for l in e["leaders"]]
assert all(any(near(t, p) for t in tips) for p in [[left["start"][0], 150], [right["start"][0], 145]]), e
assert "arrow 1 was moved 1 mm onto" in r.text and "arrow 2 was moved 2 mm onto" in r.text, r.text
# Picked there, so still attached (moving an arrow afterwards would detach it).
assert sorted(e.get("attached", [])) == sorted([left["id"], right["id"]]), e
assert "no arrow is at" not in r.text, r.text

# The boss's circle in the top view: the arrow on its rim.
circle = [e for e in entities("Drawing View2") if e["type"] == "circle"][0]
r = mcp.call("add_drawing_note", text="BOSS", attach_to=[circle["id"]])
e = entry(r.text)
tip, c = e["leaders"][0]["arrow_at_mm"], circle["center"]
assert abs(((tip[0] - c[0]) ** 2 + (tip[1] - c[1]) ** 2) ** 0.5 - circle["diameter_mm"] / 4) < 0.05, (e, circle)
boss_note = e["name"]

# ---- Style, leader and arrow.
r = mcp.call("edit_drawing_annotation", name=leader_note, height_mm=5, font="Arial", bold=True, border="box",
             justification="center", leader="bent", arrow="dot")
e = entry(r.text)
assert e["style"] == {"font": "Arial", "height_mm": 5.0, "bold": True, "justification": "center", "border": "box"}, e
assert e["leader"] == "bent" and e["leaders"][0]["arrow"] == "dot", e
r = mcp.call("edit_drawing_annotation", name=leader_note, document_font=True)
e = entry(r.text)
assert e["style"]["font"] == "document" and e["style"]["height_mm"] == 3.5 and "bold" not in e["style"], e
# A note's angle is part of its own text format (SolidWorks): turning it
# gives it its own font, the document's font turns it back.
r = mcp.call("edit_drawing_annotation", name=leader_note, angle_deg=15)
assert entry(r.text)["style"]["angle_deg"] == 15, r.text
r = mcp.call("edit_drawing_annotation", name=leader_note, document_font=True)
assert "angle_deg" not in entry(r.text)["style"], r.text
r = mcp.call("edit_drawing_annotation", name=leader_note, document_font=True, angle_deg=15, expect_error=True)
assert "turned note has its own font" in r.text, r.text

# Text and a relative move.
before = annotations()[sheet_note]["at_mm"]
r = mcp.call("edit_drawing_annotation", name=sheet_note, text="SHEET NOTE\nSECOND LINE", move_by_mm=[5, -5])
e = entry(r.text)
assert near(e["at_mm"], [before[0] + 5, before[1] - 5]) and e["text"] == ["SHEET NOTE\r\nSECOND LINE"], e

# Into a view: it stays on the paper, then follows the view. SolidWorks
# doesn't move a sheet note into a view (AttachAnnotation says it did), so
# it's made again there under a new name; a view's note moves between
# views and keeps its name.
r = mcp.call("edit_drawing_annotation", name=sheet_note, attach_to_view="Drawing View1")
e = entry(r.text)
assert "made again there" in r.text and e["name"] != sheet_note, r.text
assert e["in"] == "view Drawing View1" and near(e["at_mm"], [before[0] + 5, before[1] - 5]), e
assert e["text"] == ["SHEET NOTE\r\nSECOND LINE"], e
assert "sits" in r.text, "a note 100 mm from its view is flagged"
assert sheet_note not in annotations()
sheet_note = e["name"]
view_note_at = annotations()[view_note]["at_mm"]
r = mcp.call("edit_drawing_annotation", name=view_note, attach_to_view="Drawing View1")
e = entry(r.text)
assert e["name"] == view_note and e["in"] == "view Drawing View1" and near(e["at_mm"], view_note_at), e
mcp.call("edit_drawing_view", view="Drawing View1", at_mm=[130, 150])
a = annotations()
assert near(a[sheet_note]["at_mm"], [before[0] + 15, before[1] - 5]), a[sheet_note]
assert near(a[view_note]["at_mm"], [view_note_at[0] + 10, view_note_at[1]]), a[view_note]
mcp.call("edit_drawing_view", view="Drawing View1", at_mm=[120, 150])

# New leaders for an existing note: it's made again, look and place kept.
old = annotations()[leader_note]
r = mcp.call("edit_drawing_annotation", name=leader_note, attach_to=[left["id"], top_edge["id"]],
             arrow_at_mm=[[left["start"][0], 145], on_top])
e = entry(r.text)
assert "its leaders were made again" in r.text and e["name"] != leader_note, r.text
assert len(e["leaders"]) == 2 and sorted(e["attached"]) == sorted([left["id"], top_edge["id"]]), e
tips = [l["arrow_at_mm"] for l in e["leaders"]]
assert all(any(near(t, p) for t in tips) for p in [[left["start"][0], 145], on_top]), e
assert near(e["at_mm"], old["at_mm"]) and e["text"] == old["text"] and e["leader"] == old["leader"], (old, e)
assert leader_note not in annotations()
leader_note = e["name"]
r = mcp.call("edit_drawing_annotation", name=leader_note, arrow_at_mm=[[1, 2], [3, 4]], expect_error=True)
assert "goes with attach_to" in r.text, r.text

# ---- A dimension can be moved, not given a note's text.
r = mcp.call("add_drawing_dimensions", dimensions=[{"kind": "length", "entities": [top_edge["id"]], "at_mm": [120, 175]}])
dim = [a for a in annotations(kind="dimension").values()][0]
r = mcp.call("edit_drawing_annotation", name=dim["name"], at_mm=[118, 180])
assert near(entry(r.text)["at_mm"], [118, 180]), r.text
r = mcp.call("edit_drawing_annotation", name=dim["name"], text="X", expect_error=True)
assert "isn't a note" in r.text, r.text

# ---- All or nothing: an unknown layer refuses the whole change.
r = mcp.call("edit_drawing_annotation", name=boss_note, text="CHANGED", layer="NO SUCH LAYER", expect_error=True)
assert "NO SUCH LAYER" in r.text, r.text
assert annotations()[boss_note]["text"] == ["BOSS"], annotations()[boss_note]

# ---- Refusals before anything changes.
for args, expected in [
    ({"text": "A", "at_view_mm": [1, 2]}, "a sheet note stays put"),
    ({"text": "A", "in": "sheet", "view": "Drawing View1"}, "belong to a view: leave `view` out"),
    ({"text": "A", "in": "sheet", "attach_to": [top_edge["id"]]}, "belongs to that view"),
    ({"text": "A", "view": "Nope"}, 'no view named "Nope"'),
    ({"text": "A", "sheet": "Nope"}, 'no sheet named "Nope"'),
]:
    r = mcp.call("add_drawing_note", expect_error=True, **args)
    assert expected in r.text, (args, r.text)
r = mcp.call("edit_drawing_annotation", name="DetailItem99999", text="X", expect_error=True)
assert "no annotation named" in r.text, r.text
r = mcp.call("edit_drawing_annotation", name=boss_note, attach_to=[top_edge["id"]], attach_to_view="Drawing View2",
             expect_error=True)
assert "goes in Drawing View1, not Drawing View2" in r.text, r.text
r = mcp.call("edit_drawing_annotation", name=boss_note, attach_to=[top_edge["id"]], expect_error=True)
assert "attach_to_view Drawing View1 puts it there" in r.text, r.text

# ---- Warnings: a sheet note over a view doesn't follow it.
r = mcp.call("add_drawing_note", text="OVER THE VIEW", at_mm=[110, 155])
assert "over Drawing View1 but belongs to the sheet" in r.text, r.text
over_note = entry(r.text)["name"]

mcp.call("render_drawing", max_px=1600)

# ---- Deleting: all found or nothing deleted; title block notes too.
r = mcp.call("delete_drawing_annotations", names=[over_note, "DetailItem99999"], expect_error=True)
assert "no annotation named" in r.text, r.text
assert over_note in annotations()
count_before = len(annotations(include_sheet_format=True))
r = mcp.call("delete_drawing_annotations", names=[over_note, format_note, two_leaders, editing_format_note, dim["name"]])
assert r.text.startswith("deleted 5 annotation(s)"), r.text
after = annotations(include_sheet_format=True)
assert len(after) == count_before - 5 and not {over_note, format_note, two_leaders, dim["name"]} & set(after), after

mcp.call("render_drawing", max_px=1600)
assert not (open_dialogs := sw_dialogs()), open_dialogs

# ---- Not a drawing.
mcp.call("create_part")
r = mcp.call("add_drawing_note", text="A", at_mm=[1, 1], expect_error=True)
assert "isn't a drawing" in r.text, r.text

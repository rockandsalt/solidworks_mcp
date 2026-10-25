# add_datum_symbol / add_geometric_tolerance against real SolidWorks
# (docs/notes/gdt.md): datum feature symbols and feature control frames on
# a part's faces (arrow where asked, text out from the face), on a hole,
# free; composite and stacked frames, a datum feature and a
# projected zone on a frame; the GD&T rules and a taken datum letter
# refused with nothing added; read back by inspect_annotations; deleted
# from the part. Then a drawing of the part: a datum on a view edge, a
# frame with a leader to the hole's circle (placed outside the view), on a
# drawing dimension, free on the sheet; render; the drawing switched to
# ANSI (symbols redrawn, ASME rules by default); delete. ISO vs ASME rules
# in the (ISO) part: composite refused, standard=asme, RFS left off. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/gdt.py

import json
import subprocess
from pathlib import Path

SWCOM = Path(__file__).resolve().parent.parent / "swcom.ps1"


def sw_dialogs() -> list[str]:
    p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
                        str(SWCOM.parent / "sw.ps1"), "dialogs"], capture_output=True, text=True, timeout=60)
    return [line.strip() for line in p.stdout.splitlines() if line.strip()]


def com(code: str) -> str:
    """Runs PowerShell `code` against SolidWorks through swcom.ps1 (see there)."""
    script = mcp.run_dir / f"com-{len(list(mcp.run_dir.glob('com-*.ps1'))) + 1}.ps1"
    script.write_text(code, encoding="utf-8")
    p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(SWCOM),
                        "-CodeFile", str(script)], capture_output=True, text=True, timeout=150)
    assert p.returncode == 0, f"{script.name} failed: {p.stderr.strip()}"
    return p.stdout.strip()


def entry(text):
    """The JSON line of a reply."""
    return json.loads(text.splitlines()[-1])


def near(a, b, tol=0.05):
    return len(a) == len(b) and all(abs(x - y) <= tol for x, y in zip(a, b))


def dist(a, b):
    return sum((x - y) ** 2 for x, y in zip(a, b)) ** 0.5


def pmi(kind=None):
    """name -> inspect_annotations entry."""
    args = {"kind": kind} if kind else {}
    return {a["name"]: a for a in mcp.call("inspect_annotations", **args).json["annotations"]}


def callouts(e):
    return [f["callout"] for f in e.get("frames", [])]


# ---- A 40 x 20 x 10 block (x 0..10 along the Front plane's normal, y
# -20..20, z -10..10 in this template) with a 6 mm hole through it along x.
mcp.call("create_part")
mcp.call("create_rectangle_sketch", width_mm=40, height_mm=20)
mcp.call("extrude_sketch", depth_mm=10)
planes = mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]


def face(normal):
    return [f["id"] for f in planes if near(f["normal"], normal, 1e-6)][0]


front = face([1, 0, 0])
mcp.call("sketch_circle", on=front, center=[10, 5, 0], diameter_mm=6)
mcp.call("cut_extrude_sketch", through_all=True)
faces = mcp.call("inspect_model", what="faces").json["faces"]
planes = [f for f in faces if f["type"] == "plane"]
front, back, top = face([1, 0, 0]), face([-1, 0, 0]), face([0, 0, 1])
hole = [f["id"] for f in faces if f["type"] == "cylinder"][0]

# ---- Datum A on the back face, B (SolidWorks' next letter) on the top.
r = mcp.call("add_datum_symbol", label="A", on=back)
e = entry(r.text)
datum_a = e["name"]
assert e["type"] == "datum" and e["label"] == "A" and e["attached"] == [back], r.text
r = mcp.call("add_datum_symbol", on=top)
e = entry(r.text)
assert e["label"] == "B" and e["attached"] == [top], r.text
# A letter that's taken: refused, nothing added.
count = len(pmi())
r = mcp.call("add_datum_symbol", label="A", on=front, expect_error=True)
assert "already is a datum A" in r.text, r.text
assert len(pmi()) == count

# ---- Flatness on the back face: no datums; the text 10 mm out from the
# face (along -x), its arrow on the face.
r = mcp.call("add_geometric_tolerance", characteristic="flatness", tolerance=0.05, on=back)
print("   ", r.text.splitlines()[0])
e = entry(r.text)
flatness = e["name"]
assert callouts(e) == ["flatness | 0.05"] and e["attached"] == [back], r.text
# (The reply's first line has the arrow: the face's middle.)
assert "arrow drawn at [0, 0, 0] mm, text at [-10, 0, 0] mm" in r.text and "imported" not in e, r.text

# Perpendicularity to A on the top face, the arrow where asked.
r = mcp.call("add_geometric_tolerance", characteristic="perpendicularity", tolerance=0.1, datums=["A"],
             on=top, arrow_at_mm=[5, -12, 10])
print("   ", r.text.splitlines()[0])
e = entry(r.text)
perpendicularity = e["name"]
assert callouts(e) == ["perpendicularity | 0.1 | A"], r.text
# Picked where asked (SolidWorks draws it in *Right's plane, x = 10).
assert "arrow at [5, -12, 10] mm" in r.text and "isn't where asked" not in r.text, r.text

# Position of the hole: diameter zone at MMC to A and B at MMC.
r = mcp.call("add_geometric_tolerance", characteristic="position", tolerance=0.2, diameter_zone=True, modifier="MMC",
             datums=["A", "B(M)"], on=hole)
print("   ", r.text.splitlines()[0])
e = entry(r.text)
position = e["name"]
assert callouts(e) == ["position | dia 0.2 (M) | A | B (M)"] and e["attached"] == [hole], r.text

# A composite position frame: not in ISO, the template's drafting standard
# (refused, nothing added); standard=asme makes one, with a remark that the
# document draws by ISO.
composite = dict(characteristic="position", tolerance=0.3, diameter_zone=True, datums=["A", "B"],
                 second_frame={"characteristic": "position", "tolerance": 0.1, "diameter_zone": True, "datums": ["A"]},
                 composite=True, on=hole, at_mm=[25, 5, 15])
count = len(pmi())
r = mcp.call("add_geometric_tolerance", **composite, expect_error=True)
assert "ISO 1101 has no composite frames" in r.text, r.text
assert len(pmi()) == count
r = mcp.call("add_geometric_tolerance", **composite, standard="asme")
print("   ", r.text.splitlines()[0])
e = entry(r.text)
assert e.get("composite") is True and callouts(e) == ["position | dia 0.3 | A | B", "position | dia 0.1 | A"], r.text
assert "only its depth differs" in r.text, r.text
assert "checked against ASME Y14.5, but the document's drafting standard is ISO" in r.text, r.text
# RFS: no (S) in either standard, so it's left off, tolerance and datum.
r = mcp.call("add_geometric_tolerance", characteristic="position", tolerance=0.25, diameter_zone=True, modifier="RFS",
             datums=["A", "B(S)"], on=hole)
e = entry(r.text)
rfs_frame = e["name"]
assert callouts(e) == ["position | dia 0.25 | A | B"] and "ISO has no (S)" in r.text, r.text
# Concentricity: fine in ISO; ASME 2018 dropped it (a remark, still added).
r = mcp.call("add_geometric_tolerance", characteristic="concentricity", tolerance=0.05, diameter_zone=True,
             datums=["A"], on=hole, standard="asme")
e = entry(r.text)
concentricity = e["name"]
assert callouts(e) == ["concentricity | dia 0.05 | A"] and "Y14.5-2018 dropped concentricity" in r.text, r.text
r = mcp.call("delete_drawing_annotations", names=[rfs_frame, concentricity])
assert "deleted 2 annotation(s)" in r.text, r.text
# A datum feature on a frame, a projected zone.
r = mcp.call("add_geometric_tolerance", characteristic="flatness", tolerance=0.02, datum_feature="C", on=front)
e = entry(r.text)
assert e.get("datum_label") == "C" and callouts(e) == ["flatness | 0.02"], r.text
r = mcp.call("add_geometric_tolerance", characteristic="position", tolerance=0.1, diameter_zone=True, datums=["A"],
             projected_zone_mm=12, on=hole)
e = entry(r.text)
print("   ", r.text.splitlines()[0], e.get("projected_zone_height"))
assert e.get("projected_zone_height") == "12", r.text
# Two frames stacked (not composite).
r = mcp.call("add_geometric_tolerance", characteristic="perpendicularity", tolerance=0.1, datums=["A"],
             second_frame={"characteristic": "flatness", "tolerance": 0.03}, on=top)
e = entry(r.text)
assert callouts(e) == ["perpendicularity | 0.1 | A", "flatness | 0.03"] and not e.get("composite"), r.text

# Not on a dimension in a part: SolidWorks makes a frame with nothing
# attached and nowhere to show (seen 2026-10-24); refused, pointing at the face.
r = mcp.call("add_geometric_tolerance", characteristic="parallelism", tolerance=0.05, datums=["A"],
             dimension="D1@Boss-Extrude1", expect_error=True)
assert "can't go on a dimension" in r.text and "cylindrical face" in r.text, r.text
# A free frame.
r = mcp.call("add_geometric_tolerance", characteristic="cylindricity", tolerance=0.01, at_mm=[0, 30, 30])
print("    free:", r.text.splitlines()[0])
free_frame = entry(r.text)["name"]

# ---- Refused before SolidWorks: the GD&T rules, a drawing's arguments.
count = len(pmi())
for args, says in [
    (dict(characteristic="flatness", tolerance=0.05, datums=["A"], on=back), "never references datums"),
    (dict(characteristic="parallelism", tolerance=0.05, on=back), "needs a datum"),
    (dict(characteristic="position", tolerance=0.1, datums=["A", "B", "C", "D"], on=hole), "at most 3 datums"),
    (dict(characteristic="position", tolerance=-1, on=hole), "above 0"),
    (dict(characteristic="position", tolerance=0.1, composite=True, on=hole), "composite needs second_frame"),
    (dict(characteristic="flatness", tolerance=0.1, on="view_edge:1"), "drawing view's entity"),
    (dict(characteristic="flatness", tolerance=0.1, on=back, at_mm=[1, 2]), "[x, y, z]"),
]:
    r = mcp.call("add_geometric_tolerance", **args, expect_error=True)
    assert says in r.text, (args, r.text)
r = mcp.call("add_datum_symbol", label="D", expect_error=True)
assert "goes on" in r.text, r.text
assert len(pmi()) == count

# ---- Read back by inspect_annotations: the same callouts.
frames = pmi("gtol")
assert callouts(frames[position]) == ["position | dia 0.2 (M) | A | B (M)"], frames[position]
assert callouts(frames[flatness]) == ["flatness | 0.05"], frames[flatness]
assert frames[perpendicularity]["attached"] == [top], frames[perpendicularity]
datums = {a["label"]: a for a in pmi("datum").values()}
assert sorted(datums) == ["A", "B"], datums

# ---- Deleted from the part.
r = mcp.call("delete_drawing_annotations", names=[free_frame, flatness])
assert "deleted 2 annotation(s)" in r.text and "cylindricity | 0.01" in r.text, r.text
left = pmi()
assert free_frame not in left and flatness not in left and position in left, sorted(left)
r = mcp.call("delete_drawing_annotations", names=["NoSuchThing"], expect_error=True)
assert "no annotation named" in r.text, r.text

# ---- A drawing of it: the front at 1:1 (x toward the viewer: the block is
# y 130..170, z 140..160 on the sheet, the hole's circle at [155, 150]).
mcp.call("save_document", path=str(mcp.run_dir.resolve() / "gdt-block.SLDPRT"))
mcp.call("create_drawing")
mcp.call("create_drawing_view", at_mm=[150, 150], scale="1:1")
view = [v for s in mcp.call("inspect_drawing", view="Drawing View1").json["sheets"] for v in s["views"]
        if v["name"] == "Drawing View1"][0]
lines = [e for e in view["entities"] if e["type"] == "line"]
bottom_edge = min((e for e in lines if abs(e["start"][1] - e["end"][1]) < 0.01), key=lambda e: e["start"][1])
circle = [e for e in view["entities"] if e["type"] == "circle"][0]
print(f"    bottom edge {bottom_edge['start']}..{bottom_edge['end']}, circle {circle['center']} dia {circle['diameter_mm']}")

r = mcp.call("add_datum_symbol", label="D", on=bottom_edge["id"])
print("   ", r.text.splitlines()[0])
e = entry(r.text)
drawing_datum = e["name"]
assert e["type"] == "datum" and e["label"] == "D" and e["in"] == "view Drawing View1", r.text
assert e.get("attached") == [bottom_edge["id"]] and "no arrow is at" not in r.text, r.text
# Its box out below the view (the outline's bottom is at 134), under its triangle.
assert near(e["at_mm"], [150, 124], 0.1), e

r = mcp.call("add_geometric_tolerance", characteristic="position", tolerance=0.1, diameter_zone=True, modifier="MMC",
             datums=["D"], on=circle["id"])
print("   ", r.text.splitlines()[0])
e = entry(r.text)
drawing_frame = e["name"]
assert callouts(e) == ["position | dia 0.1 (M) | D"] and e["in"] == "view Drawing View1", r.text
assert len(e["leaders"]) == 1, e
tip = e["leaders"][0]["arrow_at_mm"]
assert abs(dist(tip, circle["center"]) - circle["diameter_mm"] / 2) < 0.05, (tip, circle)
outline = view["outline"]
assert not (outline[0][0] < e["at_mm"][0] < outline[1][0] and outline[0][1] < e["at_mm"][1] < outline[1][1]), (e, outline)

# A taken letter: refused.
r = mcp.call("add_datum_symbol", label="D", on=bottom_edge["id"], expect_error=True)
assert "already is a datum D" in r.text, r.text

# On a drawing dimension: the circle's diameter.
r = mcp.call("add_drawing_dimensions", dimensions=[{"kind": "diameter", "entities": [circle["id"]], "at_mm": [180, 175]}])
dim_name = __import__("re").search(r"RD\d+@Drawing View1", r.text).group(0)
print("    dimension:", dim_name)
r = mcp.call("add_geometric_tolerance", characteristic="position", tolerance=0.05, diameter_zone=True, datums=["D"],
             dimension=dim_name)
print("   ", r.text.splitlines()[0])
e = entry(r.text)
assert e["in"] == "view Drawing View1" and callouts(e) == ["position | dia 0.05 | D"], r.text
# Not a datum symbol: SolidWorks leaves one on a dimension attached to
# nothing, off the view (seen 2026-10-24).
r = mcp.call("add_datum_symbol", label="E", dimension=dim_name, expect_error=True)
assert "can't go on a dimension" in r.text, r.text

# Free on the sheet.
r = mcp.call("add_geometric_tolerance", characteristic="flatness", tolerance=0.02, at_mm=[40, 40])
e = entry(r.text)
sheet_frame = e["name"]
assert e["in"] == "sheet" and near(e["at_mm"], [40, 40]), r.text
# Model ids don't go in a drawing.
r = mcp.call("add_geometric_tolerance", characteristic="flatness", tolerance=0.02, on=back, expect_error=True)
assert "model id" in r.text, r.text

r = mcp.call("render_drawing", view="Drawing View1")
print("    render (ISO):", r.images)

# ---- The drawing switched to ANSI: the frames and datum stay (read back
# the same), drawn the ANSI way (compare the renders); the rules now ASME's,
# so a composite frame is fine without standard, and a datum I is warned of.
def drawing_gdt():
    return {a["name"]: a for s in mcp.call("inspect_drawing_annotations").json["sheets"] for a in s["annotations"]
            if a["type"] in ("gtol", "datum")}

before = drawing_gdt()
r = mcp.call("document_units", drafting_standard="ansi")
assert "drafting standard ANSI" in r.text, r.text
after = drawing_gdt()
assert sorted(after) == sorted(before), (sorted(before), sorted(after))
for name, a in before.items():
    assert callouts(after[name]) == callouts(a) and after[name].get("label") == a.get("label"), (a, after[name])
r = mcp.call("render_drawing", view="Drawing View1")
print("    render (ANSI):", r.images)
r = mcp.call("add_geometric_tolerance", characteristic="position", tolerance=0.3, diameter_zone=True, datums=["D"],
             second_frame={"characteristic": "position", "tolerance": 0.1, "diameter_zone": True, "datums": ["D"]},
             composite=True, at_mm=[60, 40])
e = entry(r.text)
ansi_frame = e["name"]
assert e.get("composite") is True and "checked against" not in r.text, r.text
side_edge = [e for e in lines if abs(e["start"][0] - e["end"][0]) < 0.01][0]
r = mcp.call("add_datum_symbol", label="I", on=side_edge["id"])
ansi_datum = entry(r.text)["name"]
assert "ASME Y14.5 doesn't use I, O and Q" in r.text, r.text

r = mcp.call("delete_drawing_annotations", names=[sheet_frame, ansi_frame, ansi_datum])
assert "deleted 3 annotation(s)" in r.text, r.text

assert not sw_dialogs(), sw_dialogs()
# The next scenario of the run starts from nothing open.
com("Com $sw CloseAllDocuments @($true) | Out-Null")
print("gdt scenario OK")

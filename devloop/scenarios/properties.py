# Document properties against real SolidWorks (docs/notes/drawings.md,
# "Document properties"), in one run: a plate and a block (MCP tools), an
# assembly of both (through COM); a drawing of the plate with a title block
# whose notes use PartNumber / Rev / Material / Description; the plate's
# physical material looked up, refused when misspelled, assigned (mass
# checked), linked to its Material property and changed again (the title
# block follows); part number, revision, description and a material
# designation set by meaning (routed to the title block's names, BOM part
# number set); a link refused and replaced on request; a BOM of the assembly
# listing the part numbers; a BOM part number of a listed part; refusals.
# Run with:
#   py devloop/mcp_client.py run devloop/scenarios/properties.py

import json
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


def save(path):
    com(f"""$d = Com $sw ActiveDoc -Get
$e = Com $d SaveAs3 @('{path}', 0, 1); if ($e -ne 0) {{ throw "SaveAs3: $e" }}""")


def last_json(text):
    return json.loads(text.splitlines()[-1])


def title_block(sheet="Sheet1"):
    return {f["property"]: f for f in mcp.call("inspect_title_block", sheet=sheet).json["fields"]}


def near(a, b, tol=1e-6):
    return abs(a - b) <= tol


# ---- Fixture -------------------------------------------------------------------
# Plate 60 x 40 x 5 = 12 cm^3; block 20 x 10 x 10.
mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=60, height_mm=40)
mcp.call("extrude_sketch", depth_mm=5)
plate = mcp.run_dir / "props-plate.SLDPRT"
save(plate)
mcp.call("create_part")
mcp.call("sketch", shape="rectangle", width_mm=20, height_mm=10)
mcp.call("extrude_sketch", depth_mm=10)
block = mcp.run_dir / "props-block.SLDPRT"
save(block)
assembly = mcp.run_dir / "props-pair.SLDASM"
com(f"""$template = Com $sw GetUserPreferenceStringValue @(9)
$a = Com $sw NewDocument @($template, 0, 0, 0); if (-not $a) {{ throw "NewDocument failed" }}
if (-not (Com $a AddComponent5 @('{plate}', 0, '', $false, '', 0.0, 0.0, 0.0))) {{ throw 'plate' }}
if (-not (Com $a AddComponent5 @('{block}', 0, '', $false, '', 0.005, 0.0, 0.0))) {{ throw 'block' }}
$e = Com $a SaveAs3 @('{assembly}', 0, 1); if ($e -ne 0) {{ throw "SaveAs3: $e" }}""")

mcp.call("create_drawing", paper_size="A3")
mcp.call("create_drawing_view", model=str(plate), at_mm=[120, 180], scale="1:1")
r = mcp.call("draw_title_block", rows=[
    {"cells": [{"label": "PART NO.", "property": "PartNumber", "of": "model"},
               {"label": "REV", "property": "Rev"}]},
    {"cells": [{"label": "MATERIAL", "property": "Material", "of": "model"},
               {"label": "DESCRIPTION", "property": "Description", "of": "model"}]},
])
print(r.text)

# ---- Physical material -----------------------------------------------------------
r = mcp.call("part_material", sheet="*")
print(r.text)
m = last_json(r.text)
assert m["part"] == "props-plate.SLDPRT" and m["material"] is None, m
assert near(m["density_kg_m3"], 1000) and near(m["mass_kg"], 0.012), m
assert "no \"Material\" property" in r.text, r.text

r = mcp.call("part_material", sheet="*", search="6061")
print(r.text)
assert any(x["name"] == "6061 Alloy" and near(x["density_kg_m3"], 2700, 1) for x in last_json(r.text)), r.text
r = mcp.call("part_material", sheet="*", search="*")
assert any(c["class"] == "Steel" for c in last_json(r.text)), r.text

r = mcp.call("part_material", sheet="*", material="AISI 102", expect_error=True)
assert r.is_error and "\"AISI 1020\"" in r.text, r.text
r = mcp.call("part_material", sheet="*", material="6061 alloy")
print(r.text)
m = last_json(r.text)
assert m["material"] == "6061 Alloy" and m["class"] == "Aluminium Alloys", m
assert near(m["density_kg_m3"], 2700) and near(m["mass_kg"], 0.0324), m
assert "Was: no material" in r.text, r.text

# ---- Properties by meaning --------------------------------------------------------
r = mcp.call("drawing_properties", part_number="P-100", revision="B", description="BASE PLATE",
             material="EN AW-6061-T6")
print(r.text)
out = last_json(r.text)
model = {p["name"]: p["value"] for p in out["model"]["properties"]}
drawing = {p["name"]: p["value"] for p in out["drawing"]["properties"]}
assert model == {"PartNumber": "P-100", "Description": "BASE PLATE", "Material": "EN AW-6061-T6"}, model
assert drawing.get("Rev") == "B" and "Revision" not in drawing, drawing
assert out["model"]["bom"] == {"configuration": "Default", "part_number": "P-100", "source": "user specified"}, out
assert out["physical_material"]["material"] == "6061 Alloy", out
assert "still 6061 Alloy" in r.text, r.text
fields = title_block()
assert "P-100" in fields["PartNumber"]["note_shows"] and "B" in fields["Rev"]["note_shows"], fields
assert "EN AW-6061-T6" in fields["Material"]["note_shows"], fields["Material"]

# The physical material, shown through a link: the title block follows it.
r = mcp.call("part_material", sheet="*", link_property=True)
print(r.text)
assert last_json(r.text)["material_property"]["linked"], r.text
assert "6061 Alloy" in title_block()["Material"]["note_shows"]
r = mcp.call("drawing_properties", material="S235JR", expect_error=True)
assert r.is_error and "links" in r.text and "part_material" in r.text, r.text
r = mcp.call("part_material", sheet="*", material="AISI 1020")
print(r.text)
assert near(last_json(r.text)["mass_kg"], 0.0948), r.text
assert "show \"AISI 1020\"" in r.text, r.text
assert "AISI 1020" in title_block()["Material"]["note_shows"]
# Text instead, on request; the physical material stays.
r = mcp.call("drawing_properties", material="S235JR", replace_links=True)
assert "still AISI 1020" in r.text, r.text
m = last_json(mcp.call("part_material", sheet="*").text)
assert m["material"] == "AISI 1020" and not m["material_property"]["linked"], m
assert "S235JR" in title_block()["Material"]["note_shows"]

# Other properties, no title block read; nothing given = a listing.
r = mcp.call("drawing_properties", drawing={"DrawnBy": "MW"}, model={"Finish": "Anodized"})
out = last_json(r.text)
assert {"name": "DrawnBy", "value": "MW"} in out["drawing"]["properties"], out
assert {"name": "Finish", "value": "Anodized"} in out["model"]["properties"], out
r = mcp.call("drawing_properties")
assert "nothing given" in r.text, r.text
r = mcp.call("drawing_properties", revision="C", drawing={"rev": "D"}, expect_error=True)
assert r.is_error and "give it once" in r.text, r.text

# ---- BOM part numbers --------------------------------------------------------------
mcp.call("add_sheet")
views = mcp.call("create_drawing_view", model=str(assembly), at_mm=[150, 150], scale="1:1")
asm_view = [v["name"] for s in mcp.call("inspect_drawing").json["sheets"] if s["name"] == "Sheet2" for v in s["views"]][0]
r = mcp.call("add_drawing_table", kind="bom", view=asm_view)
print(r.text)
rows = [row["cells"] for row in last_json(r.text)["rows"][1:]]
assert sorted(row[1] for row in rows) == ["P-100", "props-block"], rows

r = mcp.call("custom_properties", document=str(block), bom_part_number="BLK-7")
print(r.text)
assert r.json["bom"]["part_number"] == "BLK-7", r.text
tables = [t for s in mcp.call("inspect_drawing_tables").json["sheets"] for t in s["tables"] if t["kind"] == "bom"]
rows = [row["cells"] for row in tables[0]["rows"][1:]]
print("BOM after:", rows)
assert sorted(row[1] for row in rows) == ["BLK-7", "P-100"], rows

# ---- Refusals ----------------------------------------------------------------------
r = mcp.call("part_material", document=str(assembly), expect_error=True)
assert r.is_error and "isn't a part" in r.text, r.text
r = mcp.call("part_material", document=str(plate), material="Unobtainium 9", expect_error=True)
assert r.is_error and "no material" in r.text, r.text
r = mcp.call("custom_properties", document=str(plate), set={"SW-Mass": "1"}, expect_error=True)
assert r.is_error, r.text

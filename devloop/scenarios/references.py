# file_references against real SolidWorks (files.md, "File references"):
# parts in a subfolder, a subassembly using one of them twice, a top assembly
# with the subassembly and the other part, a drawing of it, and a part
# derived from the pin (through swcom.ps1). Open: every level with what's
# loaded, direct only, where-used in the folder (direct users only, not the
# drawing). Then everything closed, the pin moved next to the assemblies and
# the block moved away: read from the drawing file without opening it, the
# pin is found elsewhere than saved and the block is missing. A never-saved
# assembly, and refusals. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/references.py

import os
import stat
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


def refs(**args):
    r = mcp.call("file_references", **args)
    print("    " + r.json["summary"])
    return r.json


def by_name(j):
    return {Path(u["file"]).name: u for u in j["uses"]}


out = mcp.run_dir.resolve()
(out / "parts").mkdir(exist_ok=True)
block, pin = out / "parts" / "ref-block.SLDPRT", out / "parts" / "ref-pin.SLDPRT"
sub, top, drw = out / "ref-sub.SLDASM", out / "ref-top.SLDASM", out / "ref-top.SLDDRW"
derived = out / "ref-derived.SLDPRT"

for path, size in [(block, (40, 20, 10)), (pin, (5, 5, 30))]:
    mcp.call("create_part")
    mcp.call("sketch", shape="rectangle", width_mm=size[0], height_mm=size[1])
    mcp.call("extrude_sketch", depth_mm=size[2])
    mcp.call("save_document", path=str(path))
    mcp.call("close_document")
com(f"""$t = Com $sw GetUserPreferenceStringValue @(8)
$p = Com $sw NewDocument @($t, 0, 0.0, 0.0)
if (-not (Com $p InsertPart3 @('{pin}', 1, 'Default'))) {{ throw 'InsertPart3 failed' }}
if ((Com $p SaveAs3 @('{derived}', 0, 1)) -ne 0) {{ throw 'SaveAs3 failed' }}
Com $sw CloseDoc @((Com $p GetTitle)) | Out-Null""")

mcp.call("create_assembly")
mcp.call("insert_components", file=str(block), at=[{"origin_mm": [0, 0, 0]}, {"origin_mm": [0, 60, 0]}])
mcp.call("save_document", path=str(sub))
mcp.call("close_document")
mcp.call("create_assembly")
mcp.call("insert_components", file=str(sub))
mcp.call("insert_components", file=str(pin), at=[{"origin_mm": [100, 0, 0]}])
mcp.call("save_document", path=str(top))
mcp.call("create_drawing", sheet_format="a3 - iso")
mcp.call("create_drawing_view", model=str(top), orientation="front", at_mm=[200, 150])
mcp.call("save_document", path=str(drw))

# ---- Open: every level, from the active drawing.
j = refs()
assert j["document"] == "ref-top.SLDDRW" and j["file"] == str(drw), j
u = by_name(j)
assert set(u) == {"ref-top.SLDASM", "ref-sub.SLDASM", "ref-block.SLDPRT", "ref-pin.SLDPRT"}, j
assert [u[n]["level"] for n in ["ref-top.SLDASM", "ref-sub.SLDASM", "ref-pin.SLDPRT", "ref-block.SLDPRT"]] == [1, 2, 2, 3], j
assert u["ref-block.SLDPRT"]["used_by"] == [str(sub)] and u["ref-top.SLDASM"]["used_by"] == [str(drw)], j
assert all(x.get("loaded") and not x.get("missing") and "saved_as" not in x for x in j["uses"]), j
assert u["ref-block.SLDPRT"]["type"] == "part" and u["ref-sub.SLDASM"]["type"] == "assembly", j
assert j.get("loaded") is True and "listed" not in j, j

# limit: one file listed, the summary still counts all four.
j = refs(limit=1)
assert len(j["uses"]) == 1 and j["listed"].startswith("1 of 4 files"), j
assert "4 file(s)" in j["summary"], j

# An open document by a path written with forward slashes is read from memory.
j = refs(document=str(top).replace("\\", "/"))
assert j.get("loaded") is True and all(x.get("loaded") for x in j["uses"]), j

# Direct only, and the assembly by name.
j = refs(document="ref-top.SLDASM", all_levels=False)
assert set(by_name(j)) == {"ref-sub.SLDASM", "ref-pin.SLDPRT"}, j
assert "directly" in j["summary"], j

# Where-used: the pin is used directly by the top assembly and the derived part.
j = refs(document=str(pin), where_used_in=str(out))
assert j["uses"] == [] and "uses no other files" in j["summary"], j
w = j["where_used"]
users = {Path(x["file"]).name for x in w["used_by"]}
assert users == {"ref-top.SLDASM", "ref-derived.SLDPRT"}, w
assert w["files_searched"] == 5, w  # block, derived, sub, top, drawing
j = refs(document=str(derived))
assert set(by_name(j)) == {"ref-pin.SLDPRT"}, j

# ---- Closed, the pin moved next to the assemblies, the block gone.
com("Com $sw CloseAllDocuments @($true) | Out-Null")
(out / "away").mkdir(exist_ok=True)
moved_pin = out / "ref-pin.SLDPRT"
pin.rename(moved_pin)
block.rename(out / "away" / "ref-block.SLDPRT")
os.chmod(sub, stat.S_IREAD)
j = refs(document=str(drw))
os.chmod(sub, stat.S_IREAD | stat.S_IWRITE)
assert by_name(j)["ref-sub.SLDASM"].get("read_only"), j
assert "loaded" not in j, j
u = by_name(j)
assert u["ref-pin.SLDPRT"]["file"] == str(moved_pin) and u["ref-pin.SLDPRT"]["saved_as"] == str(pin), j
assert u["ref-block.SLDPRT"].get("missing") and u["ref-block.SLDPRT"]["file"] == str(block), j
assert not any(x.get("loaded") for x in j["uses"]), j
assert "1 missing" in j["summary"] and "found elsewhere than saved" in j["summary"], j
assert not mcp.call("get_active_document_name").text.strip().startswith("ref-"), "nothing should have been opened"

# Where-used of the moved pin: the derived part and the top assembly still
# saved the old path, and find it next to them.
j = refs(document=str(moved_pin), where_used_in=str(out))
w = j["where_used"]
assert {Path(x["file"]).name for x in w["used_by"]} == {"ref-top.SLDASM", "ref-derived.SLDPRT"}, w

# ---- A never-saved assembly.
mcp.call("create_assembly")
mcp.call("insert_components", file=str(moved_pin))
j = refs()
assert j["file"] is None and "(never saved)" in j["summary"] and j.get("unsaved_changes"), j
assert [x["file"] for x in j["uses"]] == [str(moved_pin)], j
r = mcp.call("file_references", where_used_in=str(out), expect_error=True)
assert "never saved" in r.text, r.text

# ---- Refusals.
r = mcp.call("file_references", document="no-such-document", expect_error=True)
assert "no open document" in r.text, r.text
r = mcp.call("file_references", document=str(out / "nope.SLDASM"), expect_error=True)
assert "no file" in r.text, r.text
r = mcp.call("file_references", where_used_in="relative\\folder", expect_error=True)
assert "absolute" in r.text, r.text
mcp.call("close_document", discard_changes=True)

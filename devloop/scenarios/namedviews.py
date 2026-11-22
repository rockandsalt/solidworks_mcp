# Standard orientations by their English names in any UI language
# (docs/notes/drawings.md, "Views"): SolidWorks translates the standard
# views' names ("*Vorderseite" in German) but lists them in a fixed order.
# Every standard orientation and the default on create, one on edit, one of
# a model that isn't open, a view saved in the model, an unknown name. Checks
# against the names the model lists, so it passes in English and German.
# Run with:
#   py devloop/mcp_client.py run devloop/scenarios/namedviews.py

import re
import subprocess
from pathlib import Path

SWCOM = Path(__file__).resolve().parent.parent / "swcom.ps1"


def com(code: str) -> str:
    """Runs PowerShell `code` against SolidWorks through swcom.ps1 (see there)."""
    script = mcp.run_dir / f"com-{len(list(mcp.run_dir.glob('com-*.ps1'))) + 1}.ps1"
    script.write_text(code, encoding="utf-8")
    # The console's code page: a German UI's names have umlauts.
    p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(SWCOM),
                        "-CodeFile", str(script)], capture_output=True, encoding="oem", errors="replace", timeout=150)
    print(f"    com {script.name}: {p.stdout.strip()}")
    assert p.returncode == 0, f"{script.name} failed: {p.stderr.strip()}"
    return p.stdout.strip()


def created(r):
    """The new view's name and orientation, from create_drawing_view's reply."""
    m = re.match(r"created (.+?) on \S+: (.+?) of ", r.text)
    assert m, r.text
    return m.group(1), m.group(2)


def part(name, saved_view=None):
    mcp.call("create_part")
    mcp.call("sketch", shape="rectangle", width_mm=40, height_mm=20)
    mcp.call("extrude_sketch", depth_mm=10)
    path = mcp.run_dir.resolve() / f"{name}.SLDPRT"
    if saved_view:
        com(f"Com (Com $sw ActiveDoc -Get) NameView @('{saved_view}') | Out-Null")
    mcp.call("save_document", path=str(path))
    return path


block = part("namedviews-block", saved_view="Saved1")
names = com("(Com (Com $sw ActiveDoc -Get) GetModelViewNames) -join '|'").split("|")
assert len(names) == 11 and names[-1] == "Saved1", names
plate = part("namedviews-plate")
com(f"Com $sw CloseDoc @('{plate}') | Out-Null")

mcp.call("create_drawing", sheet_format="a3 - iso")

# ---- The default and every standard orientation: the model's own name for it.
first, orientation = created(mcp.call("create_drawing_view", model=str(block), scale="1:2"))
assert orientation == names[1], (orientation, names)
standard = ["front", "back", "left", "right", "top", "bottom", "isometric", "trimetric", "dimetric"]
for i, o in enumerate(standard, start=1):
    _, orientation = created(mcp.call("create_drawing_view", model=str(block), orientation=o, scale="1:2"))
    assert orientation == names[i], (o, orientation, names)
# "*Front" and the localized name itself still work.
assert created(mcp.call("create_drawing_view", model=str(block), orientation="*Top", scale="1:2"))[1] == names[5]
assert created(mcp.call("create_drawing_view", model=str(block), orientation=names[7], scale="1:2"))[1] == names[7]

# ---- Edit: turn the first view to the top.
r = mcp.call("edit_drawing_view", view=first, orientation="top")
assert names[5] in r.text, r.text
views = {v["name"]: v for s in mcp.call("inspect_drawing").json["sheets"] for v in s["views"]}
assert views[first]["orientation"] == names[5], views[first]

# ---- A model that isn't open: SolidWorks loads it for the view.
r = mcp.call("create_drawing_view", model=str(plate), orientation="isometric", scale="1:2")
assert created(r)[1] == names[7], r.text

# ---- A view saved in the model; an unknown name is refused, its views listed.
assert created(mcp.call("create_drawing_view", model=str(block), orientation="saved1", scale="1:2"))[1] == "Saved1"
r = mcp.call("create_drawing_view", model=str(block), orientation="nope", expect_error=True)
assert 'no named view "nope"' in r.text and names[1] in r.text, r.text

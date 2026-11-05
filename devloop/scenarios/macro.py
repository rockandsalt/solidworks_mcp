# run_macro against real SolidWorks (architecture.md, "Macros"): a macro that
# builds a real feature (early binding, enum names, out parameters, helper
# procedures), what it prints, a runtime error and a compile error reported with
# no dialog left behind, dialogs closed, the refusals, a macro outlasting the
# call, and the other tools still working after each. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/macro.py

import shutil
import subprocess
import tempfile
from pathlib import Path

SW = Path(__file__).resolve().parent.parent / "sw.ps1"
TEMP = Path(tempfile.gettempdir())


def no_dialogs():
    p = subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(SW), "dialogs"],
                       capture_output=True, text=True, timeout=60)
    left = [line.strip() for line in p.stdout.splitlines() if line.strip()]
    assert not left, f"dialogs left open: {left}"


def macro_folders():
    return set(TEMP.glob("sld_mcp-macro-*"))


def run(code, **kwargs):
    return mcp.call("run_macro", code=code, **kwargs)


before = macro_folders()
mcp.call("create_part")

# Builds a 20 x 10 x 5 mm block through the API, with what models usually write.
r = run(r"""
Option Explicit
Dim swApp As SldWorks.SldWorks

Type Size
    w As Double
    h As Double
End Type

Function Half(v As Double) As Double
    Half = v / 2
End Function

Sub main()
    Set swApp = Application.SldWorks
    Dim doc As SldWorks.ModelDoc2
    Set doc = swApp.ActiveDoc
    Dim s As Size
    s.w = 0.02: s.h = 0.01

    Dim plane As SldWorks.Feature
    Set plane = doc.FirstFeature
    Do While plane.GetTypeName2 <> "RefPlane"
        Set plane = plane.GetNextFeature
    Loop
    plane.Select2 False, 0
    doc.SketchManager.InsertSketch True
    doc.SketchManager.CreateCenterRectangle 0, 0, 0, Half(s.w), Half(s.h), 0
    doc.SketchManager.InsertSketch True
    doc.ClearSelection2 True
    doc.Extension.SelectByID2 "Sketch1", "SKETCH", 0, 0, 0, False, 0, Nothing, 0
    Dim made As SldWorks.Feature
    Set made = doc.FeatureManager.FeatureExtrusion3(True, False, False, swEndCondBlind, swEndCondBlind, _
        0.005, 0, False, False, False, False, 0, 0, False, False, False, False, True, True, True, _
        swStartSketchPlane, 0, False)
    made.Name = "FromMacro"
    McpPrint "made " & made.Name
    McpPrint 42
    McpPrint "wide " & ChrW(20013)

    ' Out parameters.
    Dim opened As SldWorks.ModelDoc2
    Dim errors As Long, warnings As Long
    Set opened = swApp.OpenDoc6("C:\no\such\file.sldprt", swDocPART, swOpenDocOptions_Silent, "", errors, warnings)
    McpPrint "open errors=" & errors & " nothing=" & (opened Is Nothing)
End Sub
""")
assert "The macro ran." in r.text, r.text
assert "made FromMacro\n42\nwide \u4e2d\nopen errors=2 nothing=True" in r.text, r.text
features = mcp.call("inspect_model", what="features").json["features"]
assert any(f["name"] == "FromMacro" for f in features), features
bodies = mcp.call("inspect_model", what="bodies").json["bodies"]
assert abs(bodies[0]["volume_mm3"] - 1000) < 0.01, bodies
no_dialogs()

# undo takes a macro's feature back.
mcp.call("undo", steps=1)
assert not any(f["name"] == "FromMacro" for f in mcp.call("inspect_model", what="features").json.get("features", []))

# A runtime error, deep in a helper: its message, what was printed, no dialog.
r = run("""
Sub Inner()
    Dim x As Long
    x = 1 / 0
End Sub
Sub main()
    McpPrint "before"
    Inner
    McpPrint "after"
End Sub
""", expect_error=True)
assert r.is_error and "error 11: Division by zero" in r.text and "before" in r.text and "after" not in r.text, r.text
no_dialogs()

# Raised by the macro, with its own handler elsewhere.
r = run("""
Sub main()
    Err.Raise 1000, , "boom"
End Sub
""", expect_error=True)
assert "error 1000: boom" in r.text, r.text

# Doesn't compile: startup unobserved (the API cannot prove why), no dialog.
r = run("""
Option Explicit
Sub main()
    nosuchvariable = 1
End Sub
""", expect_error=True)
assert r.is_error and "startup was not observed" in r.text, r.text
no_dialogs()
r = run("Sub main()\n    If Then Then\nEnd Sub", expect_error=True)
assert "startup was not observed" in r.text, r.text
no_dialogs()

# Dialogs: closed and reported, the safest button pressed, the macro goes on.
r = run("""
Sub main()
    MsgBox "hello from macro"
    McpPrint "yes/no said " & MsgBox("sure?", vbYesNo)
    McpPrint "yes/no/cancel said " & MsgBox("really?", vbYesNoCancel)
End Sub
""")
assert "hello from macro (pressed OK)" in r.text, r.text
assert f"yes/no said {7}" in r.text, r.text  # vbNo
assert f"yes/no/cancel said {2}" in r.text, r.text  # vbCancel
no_dialogs()

# End stops everything, ours too; what it printed still comes back. RunMacro2
# runs such a macro twice, and its second go must do nothing.
r = run("""
Sub main()
    McpPrint "ending"
    End
    McpPrint "not here"
End Sub
""")
assert "The macro ran." in r.text and r.text.count("ending") == 1 and "not here" not in r.text, r.text

# Refused before SolidWorks sees them.
for code, expected in [
    ("Sub other()\nEnd Sub", "one `Sub main()`"),
    ("Sub main()\n    Stop\nEnd Sub", "debugger"),
    ("Sub main()\n    Debug.Assert False\nEnd Sub", "debugger"),
    ('Sub main()\n    McpPrint "\u4e2d"\nEnd Sub', "ChrW(20013)"),
]:
    r = run(code, expect_error=True)
    assert r.is_error and expected in r.text, r.text

# Failed runs preserve diagnostics. These synchronous runs have all returned;
# check the source was retained, then remove only this scenario's folders.
for folder in macro_folders() - before:
    assert (folder / "macro.swb").exists(), folder
    shutil.rmtree(folder)
assert macro_folders() == before, macro_folders() - before

# Outlasts the call: observed start, no completion, partial output retained.
r = run("""
Sub main()
    McpPrint "started"
    Dim t As Single
    t = Timer
    Do While Timer - t < 70
    Loop
    McpPrint "finished"
End Sub
""", expect_error=True)
assert r.is_error and "wrapper started" in r.text and "still running is unknown" in r.text and "finished" not in r.text.split("Output:")[1], r.text
left = macro_folders() - before
assert len(left) == 1, left
mcp.call("get_active_document_name")
assert "finished" in (next(iter(left)) / "output.txt").read_text(encoding="utf-16"), "macro went on"
shutil.rmtree(next(iter(left)))
no_dialogs()

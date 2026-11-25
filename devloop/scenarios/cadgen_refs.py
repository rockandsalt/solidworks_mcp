# The cad-gen-freecad references through the benchmark scorer
# (devloop/bench/score.py): each reference STEP (export_cadgen.py) is
# imported with open_document, measured with inspect_model / mass_properties
# and scored against its FreeCAD numbers; each must score 1.0. Then the
# scorer must see a wrong part: a flange with an extra Ø10 through hole
# (1.2 % of its volume) scores below 1 on volume, and a flange scored
# against a gear reference scores low. The run folder gets report.html
# (scores, and what the benchmark is). Documents are closed one by one
# (GDI objects, devloop.md), and SolidWorks is restarted when its imports
# have piled up threads. About 12 minutes for the 100 parts.
# Set CADGEN_TASKS=<id>,<id> to run only some. Run with:
#   py devloop/mcp_client.py run devloop/scenarios/cadgen_refs.py

import json
import math
import os
import subprocess
import sys
import time
from pathlib import Path

BENCH = Path(__file__).resolve().parent.parent / "bench"
sys.path.insert(0, str(BENCH))
from report import DATASET, Check, write_report  # noqa: E402
from score import geometry_score, measure, summary  # noqa: E402

CADGEN = BENCH / "cadgen"
assert (CADGEN / "tasks.json").exists(), "run devloop/bench/export_cadgen.py first (needs FreeCAD)"
tasks = json.loads((CADGEN / "tasks.json").read_text(encoding="utf-8"))["tasks"]
if os.environ.get("CADGEN_TASKS"):
    wanted = os.environ["CADGEN_TASKS"].split(",")
    tasks = [t for t in tasks if t["id"] in wanted]
missing = [t["id"] for t in tasks if not (CADGEN / t["step"]).exists()]
assert not missing, f"no STEP for {missing}: run devloop/bench/export_cadgen.py"


SW_PS1 = Path(__file__).resolve().parent.parent / "sw.ps1"


def sw_threads() -> int:
    p = subprocess.run(["powershell.exe", "-NoProfile", "-Command", "(Get-Process SLDWORKS).Threads.Count"],
                       capture_output=True, text=True, timeout=60)
    return int(p.stdout.strip() or 0)


def restart(why: str, *flags: str) -> None:
    """sw.ps1 restart (mcp_server follows the new instance), then 30 s: a STEP import in
    the first seconds after the add-in listens hangs (files.md, "Importing")."""
    print(f"    {why}: restart SolidWorks")
    restarts.append(why)
    assert subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File",
                           str(SW_PS1), "restart", *flags]).returncode == 0, "sw.ps1 restart failed"
    time.sleep(30)


hangs, restarts = [], []
started = time.monotonic()


def imported(task: dict) -> None:
    """open_document of the task's STEP; if the import hangs (files.md, "Importing"),
    restart and try once more."""
    path = str(CADGEN / task["step"])
    try:
        mcp.call("open_document", path=path)
    except ToolError as error:
        if "Timeout" not in str(error):
            raise
        hangs.append(task["id"])
        restart(f"{task['id']} import hung", "-Force")
        mcp.call("open_document", path=path)


# ---- Each reference scores 1.0 ---------------------------------------------------
# Each STEP import also leaves ~10 threads in SolidWorks (files.md, "Importing"):
# restart above 400.
results = {}
for n, task in enumerate(tasks):
    if n % 10 == 0 and (threads := sw_threads()) > 400:
        restart(f"{threads} threads")
    imported(task)
    measured = measure(mcp)
    mcp.call("close_document", discard_changes=True)
    results[task["id"]] = geometry_score(task["reference"], measured), measured
    print(f"    {task['id']} {task['name']}: {summary(results[task['id']][0])}")

below = {i: summary(r) for i, (r, _) in results.items() if r["score"] < 1.0}
moments = {i: f"{r['errors']['principal_moments']:.3%}" for i, (r, _) in results.items()
           if not r["gate"] and r["subscores"]["principal_moments"] < 1.0}  # gated ones are in `below`
print(f"    {len(results) - len(below)} of {len(results)} references score 1.0; imports that hung: {hangs or 'none'}")

# ---- A wrong part scores below 1 ---------------------------------------------------
FLANGE, GEAR = "0603c53148", "08d2da3057"  # round mounting flange, spur gear
EXTRA_HOLE_D = 10.0


def key_parameters(task: dict) -> dict:
    """A task's "- name = 12.3mm" lines as {name: 12.3}."""
    return {name.strip("- "): float(value.removesuffix("mm"))
            for name, value in (line.split(" = ") for line in task["key_parameters"].splitlines())}


checks = []  # for the report; asserted below
flange = next((t for t in tasks if t["id"] == FLANGE), None)
if flange:
    size = key_parameters(flange)
    imported(flange)
    bore = [f for f in mcp.call("inspect_model", what="faces", surface_type="cylinder").json["faces"]
            if abs(f["radius_mm"] - size["bore_diameter"] / 2) < 0.01]
    assert bore, "no bore face"
    axis, on_axis = bore[0]["axis"], bore[0]["axis_point_mm"]
    caps = [f for f in mcp.call("inspect_model", what="faces", surface_type="plane").json["faces"]
            if abs(sum(n * a for n, a in zip(f["normal"], axis))) > 0.999]
    cap = max(caps, key=lambda f: f["area_mm2"])
    # A point of the cap midway between the bore and the bolt holes.
    radius = (size["bore_diameter"] + size["bolt_circle_diameter"] - size["bolt_hole_diameter"]) / 4
    u = [axis[1], -axis[0], 0.0] if abs(axis[2]) < 0.9 else [0.0, axis[2], -axis[1]]
    u = [c / math.hypot(*u) for c in u]
    depth = sum((p - o) * a for p, o, a in zip(cap["point_on_plane_mm"], on_axis, axis))
    center = [o + depth * a + radius * c for o, a, c in zip(on_axis, axis, u)]
    mcp.call("sketch", shape="circle", on=cap["id"], center=center, diameter_mm=EXTRA_HOLE_D)
    mcp.call("extrude_sketch", cut=True, through_all=True)
    wrong = geometry_score(flange["reference"], measure(mcp))
    mcp.call("close_document", discard_changes=True)
    print(f"    flange with an extra hole: {summary(wrong)}")
    removed = math.pi * (EXTRA_HOLE_D / 2) ** 2 * size["flange_thickness"] / flange["reference"]["volume_mm3"]
    checks.append(Check(f"Round flange {FLANGE} with an extra Ø{EXTRA_HOLE_D:g} through hole",
                        f"volume error {removed:.3%} (the hole), volume subscore and score below 1",
                        summary(wrong),
                        abs(wrong["errors"]["volume"] - removed) < 1e-3 and wrong["subscores"]["volume"] < 1.0
                        and wrong["score"] < 1.0))

    gear = next(t for t in json.loads((CADGEN / "tasks.json").read_text(encoding="utf-8"))["tasks"]
                if t["id"] == GEAR)
    crossed = geometry_score(gear["reference"], results[FLANGE][1])
    print(f"    flange scored as the gear: {summary(crossed)}")
    checks.append(Check(f"Round flange {FLANGE} scored against spur gear {GEAR}", "score below 0.3",
                        summary(crossed), crossed["score"] < 0.3))

# ---- Report, then the asserts ----------------------------------------------------------
report = mcp.run_dir / "report.html"
mcp.run_dir.mkdir(parents=True, exist_ok=True)
write_report(
    report, "cad-gen-freecad references in SolidWorks", "devloop/scenarios/cadgen_refs.py",
    DATASET + "<p><b>This run has no model in it.</b> It checks the measuring and scoring chain: each FreeCAD "
    "reference, imported into SolidWorks, must score 1.0 against its own FreeCAD numbers, and parts made wrong "
    "on purpose must score lower. If this passes, a low score in a model run is the model's part, not the "
    "scorer.</p>",
    [{"task": t, "result": results[t["id"]][0]} for t in tasks if t["id"] in results],
    [("references at 1.0", f"{len(results) - len(below)} / {len(results)}"),
     ("lowest score", f"{min((r['score'] for r, _ in results.values()), default=0):.3f}"),
     ("wrong parts caught", f"{sum(c.passed for c in checks)} / {len(checks)}"),
     ("imports that hung", len(hangs)), ("SolidWorks restarts", len(restarts)),
     ("minutes", f"{(time.monotonic() - started) / 60:.1f}")],
    checks=checks)
print(f"    report: {report}")

assert not below, f"references below 1.0: {json.dumps(below, indent=1)}"
assert not moments, f"principal moments off by more than 1 %: {moments}"
failed = [c for c in checks if not c.passed]
assert not failed, f"wrong parts not caught: {failed}"

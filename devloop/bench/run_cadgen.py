"""cad-gen-freecad benchmark runner: a model builds parts through the MCP server,
score.py scores them against the FreeCAD references, report.html shows it.

    py devloop/bench/run_cadgen.py --sample 5                  # 5 tasks at random
    py devloop/bench/run_cadgen.py --sample 5 --seed 1234      # the same 5 again
    py devloop/bench/run_cadgen.py --tasks 0603c53148,08d2da3057 --model sonnet

Per task: sw.ps1 reset -Force, then a headless `claude -p` from
%LOCALAPPDATA%\\sld_mcp\\bench-agent (outside the repo, so no CLAUDE.md) with only
this MCP server, Claude Code's tool search, no run_macro and a neutral system
prompt. The prompt is the dataset's description and key parameters, plus where
to save the part. Then the saved file is opened in a fresh SolidWorks state
and scored. Output: devloop/out/<time>-cadgen-bench/ (gitignored): report.html,
results.jsonl, and per task task.md, stream.jsonl, stderr.log, transcript.md, part.SLDPRT
and the scorer's transcript. A passing `mcp_client.py run` deletes saved parts
under devloop/out later (SAVED_FILES); the scores stay.

Needs devloop/bench/cadgen/tasks.json (export_cadgen.py) and a built mcp_server.
"""

import argparse
import html
import json
import os
import random
import shutil
import subprocess
import sys
import time
import uuid
from datetime import datetime
from pathlib import Path

BENCH = Path(__file__).resolve().parent
DEVLOOP = BENCH.parent
sys.path.insert(0, str(BENCH))
sys.path.insert(0, str(DEVLOOP))
from mcp_client import DEFAULT_SERVER, OUT, Client, ToolError, sw  # noqa: E402
from report import DATASET, agent_details, write_report  # noqa: E402
from score import geometry_score, measure, summary  # noqa: E402

SESSION_DIR = Path(os.environ["LOCALAPPDATA"]) / "sld_mcp" / "bench-agent"
SYSTEM_PROMPT = ("You model parts in SOLIDWORKS through the tools of the solidworks MCP server; work only "
                 "through those tools. When the part is finished, save it where the task says, then reply with "
                 "a short summary of what you built.")


def prompt_for(task: dict, part: Path) -> str:
    return (f"Model this part in a new SolidWorks part document.\n\n{task['description']}\n\n"
            f"Key parameters:\n{task['key_parameters']}\n\nWhen it is done, save the part as {part}")


def run_agent(task: dict, task_dir: Path, args) -> dict:
    """One claude -p session; returns what its stream says (turns, cost, tool calls, final text)."""
    part = task_dir / "part.SLDPRT"
    prompt = prompt_for(task, part)
    (task_dir / "task.md").write_text(prompt, encoding="utf-8")
    config = SESSION_DIR / "mcp.json"
    command = [shutil.which("claude") or "claude", "-p", "--session-id", str(uuid.uuid4()),
               "--strict-mcp-config", "--mcp-config", str(config),
               "--tools", "ToolSearch", "--allowedTools", "mcp__solidworks,ToolSearch",
               "--disallowedTools", "mcp__solidworks__run_macro",
               "--append-system-prompt", SYSTEM_PROMPT,
               "--max-budget-usd", str(args.budget), "--output-format", "stream-json", "--verbose"]
    if args.model:
        command += ["--model", args.model]
    stream = task_dir / "stream.jsonl"
    started = time.monotonic()
    timed_out = False
    with open(stream, "wb") as out, open(task_dir / "stderr.log", "wb") as err:
        proc = subprocess.Popen(command, cwd=SESSION_DIR, stdin=subprocess.PIPE, stdout=out, stderr=err)
        try:
            proc.communicate(prompt.encode("utf-8"), timeout=args.timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            subprocess.run(["taskkill", "/PID", str(proc.pid), "/T", "/F"], capture_output=True)
            proc.wait()
    return parse_stream(stream, task_dir, time.monotonic() - started, timed_out, proc.returncode)


def parse_stream(stream: Path, task_dir: Path, seconds: float, timed_out: bool, returncode: int) -> dict:
    calls, errors, lines, result = 0, 0, [], {}
    for raw in stream.read_text(encoding="utf-8", errors="replace").splitlines():
        try:
            event = json.loads(raw)
        except json.JSONDecodeError:
            continue
        if event.get("type") == "assistant":
            for block in event["message"].get("content", []):
                if block.get("type") == "text" and block["text"].strip():
                    lines += [block["text"].strip(), ""]
                elif block.get("type") == "tool_use":
                    calls += block["name"].startswith("mcp__solidworks__")
                    lines.append(f"-> **{block['name'].replace('mcp__solidworks__', '')}** "
                                 f"`{json.dumps(block.get('input'), ensure_ascii=False)}`")
        elif event.get("type") == "user":
            content = event["message"].get("content", [])
            for block in content if isinstance(content, list) else []:
                if block.get("type") != "tool_result":
                    continue
                errors += bool(block.get("is_error"))
                parts = block.get("content")
                text = " ".join(p.get("text", f"[{p.get('type')}]") for p in parts) if isinstance(parts, list) \
                    else str(parts)
                text = " ".join(text.split())
                lines += [f"   <- {'ERROR: ' if block.get('is_error') else ''}{text[:600]}"
                          f"{' [...]' if len(text) > 600 else ''}", ""]
        elif event.get("type") == "result":
            result = event
    (task_dir / "transcript.md").write_text("\n".join(lines), encoding="utf-8")
    if result.get("is_error"):  # an API error (e.g. an unknown --model) has subtype "success", terminal_reason "api_error"
        subtype = result.get("subtype", "")
        agent_error = subtype if subtype.startswith("error") else result.get("terminal_reason") or "error"
    elif timed_out:
        agent_error = "timeout"
    elif returncode or not result:  # claude exited before its result event: the reason is in stderr.log
        agent_error = f"exit code {returncode}, no result (stderr.log)"
    else:
        agent_error = None
    return {"turns": result.get("num_turns"), "cost_usd": result.get("total_cost_usd"),
            "seconds": round(seconds), "tool_calls": calls, "tool_errors": errors,
            "agent_error": agent_error, "final": result.get("result", "")}


def score_part(task: dict, task_dir: Path) -> tuple[dict, dict | None]:
    """Opens the saved part in a fresh SolidWorks state and scores it."""
    part = task_dir / "part.SLDPRT"
    if not part.exists():
        return {"score": 0.0, "subscores": {}, "errors": {}, "gate": "no part saved"}, None
    sw("reset", "-Force")
    mcp = Client(task_dir / "score")
    try:
        mcp.call("open_document", path=str(part))
        measured = measure(mcp)
        mcp.call("close_document", discard_changes=True)
    except ToolError as error:
        return {"score": 0.0, "subscores": {}, "errors": {}, "gate": f"couldn't measure: {error}"[:300]}, None
    finally:
        mcp.close()
    return geometry_score(task["reference"], measured), measured


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    pick = parser.add_mutually_exclusive_group(required=True)
    pick.add_argument("--sample", type=int, help="this many tasks at random")
    pick.add_argument("--tasks", help="task ids, comma-separated")
    parser.add_argument("--seed", type=int, help="seed for --sample (default: a new one, printed)")
    parser.add_argument("--model", help="model alias or id (default: Claude Code's default)")
    parser.add_argument("--budget", type=float, default=3.0, help="max USD per task (default 3)")
    parser.add_argument("--timeout", type=int, default=1200, help="seconds per task (default 1200)")
    args = parser.parse_args()

    all_tasks = json.loads((BENCH / "cadgen" / "tasks.json").read_text(encoding="utf-8"))["tasks"]
    if args.tasks:
        wanted = args.tasks.split(",")
        tasks = [t for t in all_tasks if t["id"] in wanted]
        missing = set(wanted) - {t["id"] for t in tasks}
        if missing:
            parser.error(f"unknown task ids: {', '.join(sorted(missing))}")
        how = f"tasks {args.tasks}"
    else:
        seed = args.seed if args.seed is not None else random.randrange(1 << 31)
        tasks = sorted(random.Random(seed).sample(all_tasks, args.sample), key=lambda t: t["id"])
        how = f"--sample {args.sample} --seed {seed}"
    print(f"{len(tasks)} tasks ({how}): " + ", ".join(f"{t['id']} {t['name']}" for t in tasks))

    if not DEFAULT_SERVER.exists():
        parser.error(f"{DEFAULT_SERVER} not found -- build it first (build.ps1 -Target server)")
    SESSION_DIR.mkdir(parents=True, exist_ok=True)
    (SESSION_DIR / "mcp.json").write_text(json.dumps({"mcpServers": {"solidworks": {
        "command": str(DEFAULT_SERVER)}}}), encoding="ascii")
    run_dir = OUT / f"{datetime.now():%Y%m%d-%H%M%S}-cadgen-bench"
    run_dir.mkdir(parents=True)
    sw("start")

    rows, started = [], time.monotonic()
    for i, task in enumerate(tasks, 1):
        task_dir = run_dir / task["id"]
        task_dir.mkdir()
        print(f"\n[{datetime.now():%H:%M:%S}] {i}/{len(tasks)} {task['id']} {task['name']}", flush=True)
        sw("reset", "-Force")
        agent = run_agent(task, task_dir, args)
        result, measured = score_part(task, task_dir)
        print(f"    {agent['turns']} turns, {agent['tool_calls']} tool calls ({agent['tool_errors']} errors), "
              f"${agent['cost_usd'] or 0:.2f}, {agent['seconds']} s -> {summary(result)}"
              + (f"  [agent: {agent['agent_error']}]" if agent["agent_error"] else ""), flush=True)
        with open(run_dir / "results.jsonl", "a", encoding="utf-8") as f:
            f.write(json.dumps({"id": task["id"], "name": task["name"], "model": args.model, **agent,
                                "score": result, "measured": measured}, ensure_ascii=False) + "\n")
        rows.append({"task": task, "result": result, "agent": agent, "cols": {
            "Turns": agent["turns"] or "", "Tool calls": agent["tool_calls"], "Tool errors": agent["tool_errors"],
            "Cost": f"${agent['cost_usd'] or 0:.2f}", "Minutes": f"{agent['seconds'] / 60:.1f}"},
            "details": agent_details(task["id"], agent["final"], agent["agent_error"])})
        write_report_for(run_dir, rows, args, how, started)
    sw("reset", "-Force")
    print(f"\nreport: {run_dir / 'report.html'}")
    return 0


def write_report_for(run_dir: Path, rows: list, args, how: str, started: float) -> None:
    scores = [r["result"]["score"] for r in rows]
    cost = sum(r["agent"]["cost_usd"] or 0 for r in rows)
    write_report(
        run_dir / "report.html", "cad-gen-freecad benchmark run", f"devloop/bench/run_cadgen.py {how}",
        DATASET + f"<p><b>This run:</b> {len(rows)} tasks ({html.escape(how)}), model "
        f"<code>{html.escape(args.model or 'Claude Code default')}</code>, one headless Claude Code session per task "
        "with only this MCP server (tool search on, <code>run_macro</code> off) and a neutral system prompt. The "
        "prompt is the dataset's description and key parameters, and where to save the part. The saved part is "
        "then opened in SolidWorks and scored.</p>",
        rows,
        [("mean score", f"{sum(scores) / len(scores):.3f}"),
         ("parts at 1.0", f"{sum(s >= 0.999 for s in scores)} / {len(rows)}"),
         ("parts saved", f"{sum(r['result']['gate'] != 'no part saved' for r in rows)} / {len(rows)}"),
         ("cost", f"${cost:.2f}"), ("minutes", f"{(time.monotonic() - started) / 60:.0f}"),
         ("model", args.model or "default")],
        columns=["Turns", "Tool calls", "Tool errors", "Cost", "Minutes"])


if __name__ == "__main__":
    sys.exit(main())

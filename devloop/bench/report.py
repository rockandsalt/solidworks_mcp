# HTML report of a scoring run (devloop/scenarios/cadgen_refs.py, the
# benchmark runner devloop/bench/run_cadgen.py): what the benchmark is, how
# score.py scores, and one row per task. Stdlib only; one self-contained
# file, written into the run folder (devloop/out, gitignored).

import html
from datetime import datetime
from typing import NamedTuple

from score import LINEAR, TIERS, WEIGHTS

ASPECTS = list(TIERS)
LABELS = {"volume": "Volume", "surface_area": "Area", "bbox": "Box", "surface_types": "Surface types",
          "principal_moments": "Moments"}

DATASET = (
    "<p><b>cad-gen-freecad</b> (gNucleus AI, Apache-2.0, on Hugging Face) is 100 parametric parts: flanges, "
    "gears, shafts, nuts, pins, keys, washers and more, each with a description, its key dimensions and a "
    "FreeCAD model. It is the text-to-part set of Parametric CAD Bench. Here it is the borrowed tier of the "
    "benchmark planned in <code>docs/notes/llm_3d_enhancement.md</code>: a model gets the description and "
    "dimensions, builds the part through this MCP server, and the part is scored against the FreeCAD model, "
    "exported to STEP by <code>devloop/bench/export_cadgen.py</code>.</p>")


class Check(NamedTuple):
    """A part made wrong on purpose, for the report's "Wrong parts" section."""
    part: str
    expected: str
    observed: str
    passed: bool

CSS = """
:root { --bg:#fbfbfa; --fg:#1d1d1b; --muted:#6b6b66; --line:#e3e2de; --card:#fff;
  --good:#1f7a4d; --mid:#a86a00; --bad:#b3261e; --bar:#e9e8e4; }
@media (prefers-color-scheme: dark) { :root:not([data-theme="light"]) {
  --bg:#161615; --fg:#ecebe7; --muted:#9a9993; --line:#2e2d2a; --card:#1e1e1c;
  --good:#4cc38a; --mid:#e0a33a; --bad:#f0716a; --bar:#2e2d2a; } }
* { box-sizing:border-box; }
body { margin:0; background:var(--bg); color:var(--fg); font:15px/1.5 system-ui, sans-serif; }
main { max-width:1100px; margin:0 auto; padding:24px 16px 64px; }
h1 { font-size:26px; margin:0 0 4px; } h2 { font-size:18px; margin:32px 0 8px; }
.sub { color:var(--muted); margin:0 0 20px; }
.cards { display:grid; grid-template-columns:repeat(auto-fit,minmax(160px,1fr)); gap:12px; }
.card { background:var(--card); border:1px solid var(--line); border-radius:8px; padding:12px 14px; }
.card b { display:block; font-size:24px; font-variant-numeric:tabular-nums; }
.card span { color:var(--muted); font-size:13px; }
p, li { max-width:75ch; } code { font-size:13px; } a { color:inherit; }
.wrap { overflow-x:auto; border:1px solid var(--line); border-radius:8px; background:var(--card); }
table { border-collapse:collapse; width:100%; font-variant-numeric:tabular-nums; }
th, td { padding:6px 10px; border-bottom:1px solid var(--line); text-align:left; vertical-align:top; }
th.n { text-align:right; }
th { font-size:13px; color:var(--muted); font-weight:600; white-space:nowrap; }
td.n { text-align:right; white-space:nowrap; }
tr:last-child td { border-bottom:0; }
.good { color:var(--good); } .mid { color:var(--mid); } .bad { color:var(--bad); }
.score { display:flex; align-items:center; gap:8px; white-space:nowrap; }
.bar { width:70px; height:6px; background:var(--bar); border-radius:3px; overflow:hidden; }
.bar i { display:block; height:100%; background:currentColor; }
.err { color:var(--muted); font-size:12px; display:block; }
details summary { cursor:pointer; } details pre { white-space:pre-wrap; font-size:12px; color:var(--muted); margin:6px 0 0; }
details .more { font-size:13px; margin-top:6px; max-width:75ch; }
"""


def _cls(v: float) -> str:
    return "good" if v >= 0.999 else "mid" if v >= 0.5 else "bad"


def _score_cell(v: float) -> str:
    return (f'<div class="score {_cls(v)}"><div class="bar"><i style="width:{v * 100:.0f}%"></i></div>'
            f'{v:.3f}</div>')


def _aspect_cells(result: dict) -> str:
    if result["gate"]:
        return f'<td colspan="{len(ASPECTS)}" class="bad">gate: {html.escape(result["gate"])}</td>'
    return "".join(f'<td class="n"><span class="{_cls(result["subscores"][a])}">{result["subscores"][a]:.2f}</span>'
                   f'<span class="err">{result["errors"][a]:.3%}</span></td>' for a in ASPECTS)


def agent_details(task_id: str, final: str, agent_error: str | None) -> str:
    """A benchmark row's "details": the model's reply and links to the task's run files."""
    error = f" ({html.escape(agent_error)})" if agent_error else ""
    reply = html.escape(final[:3000]) or "<i>no reply</i>"
    links = " · ".join(f"<a href='{html.escape(task_id)}/{name}'>{label}</a>" for name, label in [
        ("transcript.md", "transcript"), ("task.md", "prompt"), ("stream.jsonl", "stream"), ("stderr.log", "stderr")])
    return f"<div class='more'><b>Model's reply{error}:</b><pre>{reply}</pre>{links}</div>"


def write_report(path, title: str, subtitle: str, intro: str, rows: list, facts: list, *,
                 checks: list[Check] = (), columns: list = ()) -> None:
    """rows: dicts with "task", "result" (score.geometry_score), optional "cols" ({column: text})
    and "details" (HTML under the prompt); columns: the names of those extra columns;
    checks: a "Wrong parts" section; facts: (label, value) cards."""
    tiers = "".join(
        f"<tr><td>{LABELS[a]}</td><td class='n'>{WEIGHTS.get(a, 'reported')}</td>"
        f"<td class='n'>{TIERS[a][0]:.1%}</td><td class='n'>{TIERS[a][1]:.0%}</td>"
        f"<td>{'linear' if a in LINEAR else 'log'}</td></tr>" for a in ASPECTS)
    body_rows = "".join(
        f"<tr><td><details><summary><code>{html.escape(r['task']['id'])}</code> {html.escape(r['task']['name'])}"
        f"</summary><pre>{html.escape(r['task']['description'])}\n\n{html.escape(r['task']['key_parameters'])}</pre>"
        f"{r.get('details', '')}</details></td>"
        f"<td>{_score_cell(r['result']['score'])}</td>{_aspect_cells(r['result'])}"
        + "".join(f"<td class='n'>{html.escape(str(r.get('cols', {}).get(c, '')))}</td>" for c in columns)
        + "</tr>"
        for r in sorted(rows, key=lambda r: (r["result"]["score"], r["task"]["id"])))
    check_section = "" if not checks else (
        "<h2>Wrong parts</h2><p>A scorer that gives everything 1.0 proves nothing; these parts must score lower.</p>"
        "<div class='wrap'><table><tr><th>Part</th><th>Expected</th><th>Observed</th><th></th></tr>"
        + "".join(f"<tr><td>{html.escape(c.part)}</td><td>{html.escape(c.expected)}</td>"
                  f"<td>{html.escape(c.observed)}</td>"
                  f"<td class='{'good' if c.passed else 'bad'}'>{'pass' if c.passed else 'FAIL'}</td></tr>"
                  for c in checks)
        + "</table></div>")
    cards = "".join(f'<div class="card"><b>{html.escape(str(v))}</b><span>{html.escape(k)}</span></div>'
                    for k, v in facts)
    head = "".join(f"<th class='n'>{LABELS[a]}</th>" for a in ASPECTS) + "".join(
        f"<th class='n'>{html.escape(c)}</th>" for c in columns)
    page = f"""<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>{html.escape(title)}</title><style>{CSS}</style></head><body><main>
<h1>{html.escape(title)}</h1>
<p class="sub">{datetime.now():%Y-%m-%d %H:%M} · {html.escape(subtitle)}</p>
<div class="cards">{cards}</div>

<h2>What this is</h2>
{intro}

<h2>How a part is scored</h2>
<p>The part open in SolidWorks is measured with <code>mass_properties</code> (volume, area, principal moments)
and <code>inspect_model</code> (solid bodies, their box, the area of each face by surface type), and compared
with the reference numbers from FreeCAD. This is the geometry score of the dataset's validator
(<code>gNucleus-AI/freecad-validator</code>, v1), with SolidWorks measurements in place of FreeCAD ones.</p>
<ul>
<li><b>Gate:</b> no saved part, or a different number of solid bodies, scores 0.</li>
<li><b>Each aspect</b> is 1 up to its <i>matched</i> relative error, 0 from its <i>far</i> error, and falls on the
table's ramp between. The box compares the three extents sorted, and the moments are divided by
volume<sup>5/3</sup>, so neither depends on how the part is placed or turned.</li>
<li><b>Surface types:</b> the face area of each type (plane, cylinder, cone, ...), summed difference over both
totals.</li>
<li><b>Score</b> = weighted sum. Aspects marked <i>reported</i> are not in it (the validator's v2 adds the moments).</li>
</ul>
<div class="wrap"><table><tr><th>Aspect</th><th class='n'>Weight</th><th class='n'>Matched</th><th class='n'>Far</th><th>Ramp</th></tr>{tiers}</table></div>
<p>Not in this score yet: the validator's spec check (the named parameters of each part family, such as a
bolt-circle diameter, measured on the part).</p>
{check_section}
<h2>Parts ({len(rows)})</h2>
<p>Lowest score first. Each aspect shows its subscore and, below it, the relative error against the reference.
Click a part for its prompt: the description and key parameters a model gets.</p>
<div class="wrap"><table><tr><th>Part</th><th>Score</th>{head}</tr>{body_rows}</table></div>
</main></body></html>"""
    path.write_text(page, encoding="utf-8")

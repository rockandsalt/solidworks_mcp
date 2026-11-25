# Geometry score of the active SolidWorks part against a cad-gen-freecad
# reference (devloop/bench/cadgen/tasks.json, made by export_cadgen.py).
# Stdlib only: the scenarios and the benchmark import it next to mcp_client.py.
#
# The score is the geometry_similarity of the dataset's validator
# (gNucleus-AI/freecad-validator, scorer "v1", comparators/geometry.py), with
# SolidWorks numbers in place of FreeCAD ones. The numbers are in WEIGHTS and
# TIERS below; report.py prints them.
#   gate: solid count must match (else 0)
#   error per aspect: relative for volume and area; mean relative of the three
#     sorted extents for bbox, and of the principal moments / volume^(5/3);
#     surface_types: area per surface type, sum |ref - cand| / (both totals)
#   subscore: 1 up to the matched error, 0 from the far one, a log10 ramp
#     between (linear for the aspects in LINEAR)
#   score: sum of WEIGHTS x subscore. Aspects without a weight (the moments,
#     the validator's v2 term) are only reported. All of it ignores the frame.

import math

WEIGHTS = {"volume": 0.35, "surface_area": 0.40, "bbox": 0.15, "surface_types": 0.10}
# aspect -> (matched, far) relative error, in the report's column order
TIERS = {"volume": (1e-3, 1e-2), "surface_area": (1e-2, 1e-1), "bbox": (1e-2, 1e-1),
         "surface_types": (5e-3, 0.75), "principal_moments": (1e-2, 1e-1)}
LINEAR = {"surface_types"}


def measure(mcp) -> dict:
    """The active part's numbers, in the keys of a tasks.json reference."""
    bodies = [b for b in mcp.call("inspect_model", what="bodies").json["bodies"] if b.get("solid")]
    mass = mcp.call("mass_properties").json
    faces = mcp.call("inspect_model", what="faces", limit=100000).json["faces"]
    by_type: dict[str, float] = {}
    for f in faces:
        by_type[f["type"]] = by_type.get(f["type"], 0.0) + f["area_mm2"]
    lo = [min(b["bbox_mm"]["min"][i] for b in bodies) for i in range(3)] if bodies else [0.0] * 3
    hi = [max(b["bbox_mm"]["max"][i] for b in bodies) for i in range(3)] if bodies else [0.0] * 3
    volume = mass["volume_mm3"]
    per_mm5 = mass["density_kg_m3"] * 1e-9  # kg·mm² -> mm⁵
    moments = mass.get("inertia_kg_mm2", {}).get("principal_moments", [])
    return {
        "solid_count": len(bodies),
        "volume_mm3": volume,
        "area_mm2": mass["surface_area_mm2"],
        "bbox_mm": {"min": lo, "max": hi},
        "bbox_sorted_mm": sorted(h - l for l, h in zip(lo, hi)),
        "center_of_mass_mm": mass.get("center_of_mass_mm"),
        "principal_moments_normalized": sorted(m / per_mm5 / volume ** (5 / 3) for m in moments)
        if volume > 0 else [],
        "surface_area_by_type": dict(sorted(by_type.items())),
        "faces": len(faces),
    }


def _rel(a: float, b: float) -> float:
    return abs(a - b) / max(abs(a), abs(b), 1e-9)


def _tier(rel: float, matched: float, far: float) -> float:
    if rel <= matched:
        return 1.0
    if rel >= far:
        return 0.0
    return 1.0 - math.log10(rel / matched) / math.log10(far / matched)


def _linear(rel: float, matched: float, far: float) -> float:
    return max(0.0, min(1.0, (far - rel) / (far - matched)))


def _mean_rel(ref: list, cand: list) -> float:
    if len(ref) != len(cand) or not ref:
        return 1.0
    return sum(_rel(a, b) for a, b in zip(ref, cand)) / len(ref)


def geometry_score(ref: dict, cand: dict) -> dict:
    """{"score", "subscores", "errors", "gate"} for measured numbers `cand` against `ref`."""
    if cand["solid_count"] != ref["solid_count"]:
        return {"score": 0.0, "subscores": {}, "errors": {},
                "gate": f"{cand['solid_count']} solid bodies, the reference has {ref['solid_count']}"}
    types_ref, types_cand = ref["surface_area_by_type"], cand["surface_area_by_type"]
    kinds = set(types_ref) | set(types_cand)
    total = sum(types_ref.get(k, 0.0) + types_cand.get(k, 0.0) for k in kinds)
    types_err = sum(abs(types_ref.get(k, 0.0) - types_cand.get(k, 0.0)) for k in kinds) / total if total else 0.0
    errors = {
        "volume": _rel(ref["volume_mm3"], cand["volume_mm3"]),
        "surface_area": _rel(ref["area_mm2"], cand["area_mm2"]),
        "bbox": _mean_rel(ref["bbox_sorted_mm"], cand["bbox_sorted_mm"]),
        "surface_types": types_err,
        "principal_moments": _mean_rel(ref["principal_moments_normalized"], cand["principal_moments_normalized"]),
    }
    subscores = {name: (_linear if name in LINEAR else _tier)(errors[name], *TIERS[name]) for name in TIERS}
    score = sum(w * subscores[name] for name, w in WEIGHTS.items())
    return {"score": score, "subscores": subscores, "errors": errors, "gate": None}


def summary(result: dict) -> str:
    if result["gate"]:
        return f"0.000 (gate: {result['gate']})"
    parts = ", ".join(f"{k} {result['subscores'][k]:.2f} ({result['errors'][k]:.3%})" for k in result["errors"])
    return f"{result['score']:.3f}: {parts}"

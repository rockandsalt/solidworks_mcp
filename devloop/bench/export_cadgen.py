# One-time export of the cad-gen-freecad references (submodule
# devloop/bench/cad-gen-freecad) for SolidWorks: per part a STEP in
# devloop/bench/cadgen/refs/<id>.step, and devloop/bench/cadgen/tasks.json
# with the prompt (description + key_parameters) and the reference numbers
# that the dataset's validator (gNucleus-AI/freecad-validator) compares:
# volume, area, sorted box, solid count, face area per surface type,
# principal moments / volume^(5/3). Same body selection as the validator:
# the first PartDesign::Body with volume, after doc.recompute().
#
# Needs FreeCAD's Python (not the stdlib Python of mcp_client.py), e.g. a
# pixi env with `pixi add freecad pyarrow`:
#   <env>\.pixi\envs\default\python.exe devloop\bench\export_cadgen.py
# The scenario devloop/scenarios/cadgen_refs.py then needs no FreeCAD.

import os
import sys

sys.path.append(os.path.join(os.path.dirname(sys.executable), "Library", "bin"))  # FreeCAD.pyd in a conda env
import FreeCAD  # noqa: E402  first: its init scripts run in __main__ and delete earlier names
import Part  # noqa: E402
import json  # noqa: E402
from pathlib import Path  # noqa: E402

import numpy as np  # noqa: E402
import pyarrow.parquet as pq  # noqa: E402

HERE = Path(__file__).resolve().parent
DATASET = HERE / "cad-gen-freecad"
OUT = HERE / "cadgen"
REFS = OUT / "refs"

# FreeCAD surface classes -> inspect_model face types.
SURFACE_TYPES = {
    "Plane": "plane", "Cylinder": "cylinder", "Cone": "cone", "Sphere": "sphere", "Toroid": "torus",
    "BSplineSurface": "bspline", "BezierSurface": "bspline", "SurfaceOfRevolution": "revolution",
    "SurfaceOfExtrusion": "extrusion", "OffsetSurface": "offset",
}


def scored_body(doc):
    for obj in doc.Objects:
        if obj.TypeId == "PartDesign::Body" and not obj.Shape.isNull() and obj.Shape.Volume > 1e-9:
            return obj
    return None


def features(shape) -> dict:
    solid = shape.Solids[0] if len(shape.Solids) == 1 else shape
    # Tight box: BoundBox is loose around tori (a bearing ring's raceway gave 1.41 mm for its Ø1.07).
    bb = shape.optimalBoundingBox(True, False)
    by_type: dict[str, float] = {}
    for face in shape.Faces:
        kind = SURFACE_TYPES.get(type(face.Surface).__name__, "other")
        by_type[kind] = by_type.get(kind, 0.0) + face.Area
    m = solid.MatrixOfInertia
    tensor = np.array([[m.A11, m.A12, m.A13], [m.A21, m.A22, m.A23], [m.A31, m.A32, m.A33]])
    moments = np.linalg.eigvalsh((tensor + tensor.T) / 2)
    com = solid.CenterOfMass
    return {
        "solid_count": len(shape.Solids),
        "volume_mm3": shape.Volume,
        "area_mm2": shape.Area,
        "bbox_mm": {"min": [bb.XMin, bb.YMin, bb.ZMin], "max": [bb.XMax, bb.YMax, bb.ZMax]},
        "bbox_sorted_mm": sorted([bb.XLength, bb.YLength, bb.ZLength]),
        "center_of_mass_mm": [com.x, com.y, com.z],
        "principal_moments_normalized": sorted(float(max(v, 0.0)) / shape.Volume ** (5 / 3) for v in moments),
        "surface_area_by_type": dict(sorted(by_type.items())),
        "faces": len(shape.Faces),
    }


def main() -> int:
    REFS.mkdir(parents=True, exist_ok=True)
    rows = pq.read_table(DATASET / "data" / "dataset.parquet",
                         columns=["id", "name", "description", "key_parameters", "fcstd_path"]).to_pylist()
    tasks, problems = [], []
    for row in rows:
        doc = FreeCAD.openDocument(str(DATASET / row["fcstd_path"]))
        try:
            body = scored_body(doc)
            stored = body.Shape.Volume if body else 0.0  # as saved by FreeCAD 0.21.2
            doc.recompute()
            body = scored_body(doc)
            if body is None:
                problems.append(f"{row['id']}: no Body with volume after recompute")
                continue
            shape = body.Shape.copy()
            step = REFS / f"{row['id']}.step"
            shape.exportStep(str(step))
            back = Part.read(str(step))
            ref = features(shape)
            drift = abs(ref["volume_mm3"] - stored) / max(stored, 1e-9)
            round_trip = abs(back.Volume - ref["volume_mm3"]) / ref["volume_mm3"]
            if drift > 1e-6:
                problems.append(f"{row['id']}: volume after recompute differs from the saved one by {drift:.2%}")
            if round_trip > 1e-6 or ref["solid_count"] != 1:
                problems.append(f"{row['id']}: STEP round trip {round_trip:.2e}, {ref['solid_count']} solids")
            tasks.append({"id": row["id"], "name": row["name"], "description": row["description"],
                          "key_parameters": row["key_parameters"], "reference": ref,
                          "step": step.relative_to(OUT).as_posix()})
        finally:
            FreeCAD.closeDocument(doc.Name)
    (OUT / "tasks.json").write_text(json.dumps({
        "source": "huggingface.co/datasets/gnucleus-ai/cad-gen-freecad (Apache-2.0, (c) gNucleus AI, Inc.)",
        "freecad": ".".join(FreeCAD.Version()[:3]),
        "tasks": tasks}, indent=1), encoding="utf-8")
    print(f"{len(tasks)} tasks, {sum(os.path.getsize(REFS / (t['id'] + '.step')) for t in tasks) / 1e6:.1f} MB of STEP")
    print("\n".join(problems) or "no problems")
    return 0


if __name__ == "__main__":
    sys.exit(main())

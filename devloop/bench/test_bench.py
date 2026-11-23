# Unit tests of the benchmark's plain-value logic: score.py's ramps and
# weights, and how run_cadgen.py reads a claude -p stream. No SolidWorks,
# no FreeCAD, no model. Run with:
#   py -m unittest discover -s devloop/bench

import copy
import json
import tempfile
import unittest
from pathlib import Path

from run_cadgen import parse_stream
from score import WEIGHTS, _tier, geometry_score

REF = {"solid_count": 1, "volume_mm3": 1000.0, "area_mm2": 600.0, "bbox_sorted_mm": [10.0, 10.0, 10.0],
       "principal_moments_normalized": [0.1667, 0.1667, 0.1667], "surface_area_by_type": {"plane": 600.0}}
HALF_WAY = 10 ** -2.5  # log-midway between a 0.1 % matched and a 1 % far tolerance


def candidate(**changes) -> dict:
    cand = copy.deepcopy(REF)
    cand.update(changes)
    return cand


class Score(unittest.TestCase):
    def test_weights_sum_to_one(self):
        self.assertAlmostEqual(sum(WEIGHTS.values()), 1.0)

    def test_tier_is_one_up_to_matched_zero_from_far_log_between(self):
        self.assertEqual(_tier(1e-3, 1e-3, 1e-2), 1.0)
        self.assertAlmostEqual(_tier(HALF_WAY, 1e-3, 1e-2), 0.5)
        self.assertEqual(_tier(1e-2, 1e-3, 1e-2), 0.0)
        self.assertEqual(_tier(0.5, 1e-3, 1e-2), 0.0)

    def test_identical_part_scores_one(self):
        result = geometry_score(REF, candidate())
        self.assertIsNone(result["gate"])
        self.assertAlmostEqual(result["score"], 1.0)

    def test_volume_half_way_costs_half_its_weight(self):
        result = geometry_score(REF, candidate(volume_mm3=1000.0 * (1 - HALF_WAY)))
        self.assertAlmostEqual(result["subscores"]["volume"], 0.5)
        self.assertAlmostEqual(result["score"], 1 - WEIGHTS["volume"] / 2)

    def test_area_off_ten_percent_loses_all_its_weight(self):
        result = geometry_score(REF, candidate(area_mm2=540.0))
        self.assertEqual(result["subscores"]["surface_area"], 0.0)
        self.assertAlmostEqual(result["score"], 1 - WEIGHTS["surface_area"])

    def test_bbox_is_the_mean_error_of_the_sorted_extents(self):
        one_off = 10.0 * (1 - 3 * 10 ** -1.5)  # one extent off 9.5 %: mean 3.16 %, log-midway of (1 %, 10 %)
        result = geometry_score(REF, candidate(bbox_sorted_mm=[one_off, 10.0, 10.0]))
        self.assertAlmostEqual(result["errors"]["bbox"], 10 ** -1.5)
        self.assertAlmostEqual(result["subscores"]["bbox"], 0.5)

    def test_surface_types_linear_from_half_percent_to_75_percent(self):
        def types_score(moved: float) -> float:  # `moved` mm² of the 600 plane area counted as cylinder
            types = {"plane": 600.0 - moved, "cylinder": moved}
            return geometry_score(REF, candidate(surface_area_by_type=types))["subscores"]["surface_types"]
        self.assertEqual(types_score(2.4), 1.0)  # error 2·2.4 / 1200 = 0.4 %
        self.assertAlmostEqual(types_score(226.5), 0.5)  # error 37.75 %, midway of (0.5 %, 75 %)
        self.assertEqual(types_score(500.0), 0.0)  # error 83 %

    def test_other_solid_count_is_gated_to_zero(self):
        result = geometry_score(REF, candidate(solid_count=2))
        self.assertEqual(result["score"], 0.0)
        self.assertEqual((result["subscores"], result["errors"]), ({}, {}))
        self.assertIn("2 solid bodies", result["gate"])

    def test_principal_moments_are_reported_not_scored(self):
        result = geometry_score(REF, candidate(principal_moments_normalized=[1.0, 1.0, 1.0]))
        self.assertEqual(result["subscores"]["principal_moments"], 0.0)
        self.assertAlmostEqual(result["score"], 1.0)

    def test_missing_moments_count_as_fully_wrong(self):
        result = geometry_score(REF, candidate(principal_moments_normalized=[]))
        self.assertEqual(result["errors"]["principal_moments"], 1.0)


class Stream(unittest.TestCase):
    def parse(self, events: list, returncode: int, timed_out: bool = False) -> dict:
        with tempfile.TemporaryDirectory() as tmp:
            stream = Path(tmp) / "stream.jsonl"
            stream.write_text("\n".join(json.dumps(e) for e in events), encoding="utf-8")
            return parse_stream(stream, Path(tmp), 1.0, timed_out, returncode)

    def test_exit_without_result_is_an_agent_error(self):
        agent = self.parse([], returncode=1)
        self.assertEqual(agent["agent_error"], "exit code 1, no result (stderr.log)")
        self.assertEqual(agent["final"], "")

    def test_successful_session(self):
        agent = self.parse([
            {"type": "assistant", "message": {"content": [
                {"type": "tool_use", "name": "mcp__solidworks__sketch", "input": {}},
                {"type": "tool_use", "name": "ToolSearch", "input": {}}]}},
            {"type": "user", "message": {"content": [{"type": "tool_result", "is_error": True, "content": "x"}]}},
            {"type": "result", "subtype": "success", "is_error": False, "num_turns": 3,
             "total_cost_usd": 0.5, "result": "done"}], returncode=0)
        self.assertEqual((agent["agent_error"], agent["tool_calls"], agent["tool_errors"]), (None, 1, 1))
        self.assertEqual((agent["turns"], agent["cost_usd"], agent["final"]), (3, 0.5, "done"))

    def test_error_result_wins_over_exit_code(self):
        agent = self.parse([{"type": "result", "subtype": "error_max_budget_usd", "is_error": True}], returncode=1)
        self.assertEqual(agent["agent_error"], "error_max_budget_usd")

    def test_api_error_reports_terminal_reason(self):  # what claude -p --model <unknown> streams
        agent = self.parse([{"type": "result", "subtype": "success", "is_error": True, "terminal_reason": "api_error",
                             "api_error_status": 404, "result": "There's an issue with the selected model"}], 0)
        self.assertEqual(agent["agent_error"], "api_error")

    def test_timeout(self):
        self.assertEqual(self.parse([], returncode=1, timed_out=True)["agent_error"], "timeout")


if __name__ == "__main__":
    unittest.main()

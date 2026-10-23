import json
import unittest
from pathlib import Path

from aik_controller.explain import (
    explain_counterfactual,
    explain_distilled,
    explain_heuristic,
    explain_line,
    explain_logistic,
    explain_threshold,
    summarize_real_data_benchmarks,
)
from aik_controller.heuristic import load_thresholds

ROOT = Path(__file__).resolve().parents[2]


class ExplainTests(unittest.TestCase):
    def setUp(self) -> None:
        self.thresholds = load_thresholds(ROOT / "configs" / "heuristic-v0.json")
        self.cand_r5 = json.loads((ROOT / "configs" / "candidate-v0-r5-1600.json").read_text())["params"]
        self.fit_r5 = json.loads((ROOT / "data" / "jobs-v2" / "fit-v0-r5-1600.json").read_text())
        self.log_r5 = self.fit_r5["fitted"]["logistic-v0"]
        self.sample_label_doc = json.loads(
            (ROOT / "data" / "jobs-v2" / "labels-final_test-seed301-r5-1600.json").read_text()
        )

    def test_heuristic_rule_attribution_matches_choice(self) -> None:
        unit = self.sample_label_doc["units"][0]
        exp = explain_heuristic(unit["snapshot"], self.thresholds)
        self.assertEqual(exp["choice"], "latency")
        self.assertEqual(exp["triggered_rule"], "latency_queue_or_wait_high")
        self.assertGreaterEqual(
            exp["signals"]["latency_max_wait_us"],
            exp["signals"]["latency_wait_threshold_us"],
        )

    def test_threshold_explains_margin_and_regime(self) -> None:
        under_unit = next(u for u in self.sample_label_doc["units"] if u["scenario"] == "s12-u35")
        over_unit = next(u for u in self.sample_label_doc["units"] if u["scenario"] == "s12-u80")

        u_exp = explain_threshold(under_unit["snapshot"], self.cand_r5)
        self.assertEqual(u_exp["choice"], "latency")
        self.assertEqual(u_exp["regime"], "under_capacity")
        self.assertLess(u_exp["margin_jobs"], 0.0)

        o_exp = explain_threshold(over_unit["snapshot"], self.cand_r5)
        self.assertEqual(o_exp["choice"], "reclaim")
        self.assertEqual(o_exp["regime"], "overload_shedding")
        self.assertGreater(o_exp["margin_jobs"], 0.0)

    def test_logistic_contributions_sum_to_logit(self) -> None:
        unit = self.sample_label_doc["units"][0]
        exp = explain_logistic(unit["snapshot"], self.log_r5)
        self.assertEqual(exp["choice"], "latency")
        self.assertGreater(exp["probability"], 0.9)
        for prof, details in exp["profiles"].items():
            reconstructed = round(details["bias"] + sum(details["contributions"].values()), 4)
            self.assertAlmostEqual(reconstructed, details["logit"], places=3, msg=prof)

    def test_counterfactual_and_real_data_summary(self) -> None:
        unit = next(u for u in self.sample_label_doc["units"] if u["scenario"] == "s12-u35")
        cf = explain_counterfactual(unit, "latency")
        self.assertTrue(cf["hit"])
        self.assertTrue(cf["feasible"])
        self.assertGreater(cf["p99_speedup_vs_balanced"], 2.0)

        summary = summarize_real_data_benchmarks(ROOT, "r5-1600")
        self.assertEqual(summary["controllers"]["pending-threshold"]["hits"], 21)
        self.assertEqual(summary["controllers"]["pending-threshold"]["units"], 21)
        self.assertEqual(summary["controllers"]["distilled-joint"]["hits"], 21)
        self.assertEqual(summary["controllers"]["distilled-joint"]["feasible_units"], 21)
        self.assertEqual(len(summary["case_studies"]), 4)

        d_exp = explain_distilled(unit["snapshot"])
        self.assertEqual(d_exp["choice"], "latency")
        self.assertEqual(d_exp["regime"], "under_capacity_burst")
        self.assertGreaterEqual(d_exp["confidence_bp"], 6_500)

        line = explain_line(unit["snapshot"], self.thresholds, self.cand_r5, self.log_r5)
        self.assertTrue(line.startswith("EXPLAIN seq="))
        self.assertIn("threshold=latency", line)
        self.assertIn("logistic=latency", line)


if __name__ == "__main__":
    unittest.main()

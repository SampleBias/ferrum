import json
import shutil
import tempfile
import unittest
from pathlib import Path

from aik_controller.fit import (
    FitError,
    _zero_shot_choices,
    calibrate,
    fit_logistic,
    freeze,
    fit_threshold,
    load_config,
    load_labels,
    logistic_choice,
    run,
    score,
    select,
    threshold_choice,
    wilson,
    zero_shot_rows,
)
from aik_controller.splits import SplitError

ROOT = Path(__file__).resolve().parents[2]
DATA = ROOT / "data" / "jobs-v2"
INTEL = [DATA / f"labels-training-seed{seed}-i7-10750h.json" for seed in (101, 102, 103)]
CONFIG = load_config(ROOT)


def snap(pending, wait_us=0, service_us=0):
    return {
        "window_us": 1_000_000,
        "groups": [{"class": "latency", "queue_len": pending, "max_wait_us": wait_us, "cpu_service_us": service_us}],
    }


def summary(feasible, p99):
    return {"feasible": feasible, "p99_median": p99}


UNDER = {
    "scenario": "s4-u15", "seed": 1, "label": ["latency"], "best": "latency",
    "profiles": {
        "balanced": summary(True, 30_000), "latency": summary(True, 15_000),
        "throughput": summary(False, None), "reclaim": summary(True, 31_000),
    },
}
OVER = {
    "scenario": "s4-u80", "seed": 1, "label": ["balanced", "reclaim"], "best": "balanced",
    "profiles": {
        "balanced": summary(True, None), "latency": summary(False, 900_000),
        "throughput": summary(False, None), "reclaim": summary(True, None),
    },
}


class ThresholdTests(unittest.TestCase):
    def test_the_cut_sits_in_the_widest_gap_that_keeps_every_hit(self):
        examples = [(0, ["latency"]), (2, ["latency"]), (26, ["balanced", "reclaim"]), (40, ["balanced", "reclaim"])]
        params = fit_threshold(examples)
        self.assertEqual((params["cut"], params["below"], params["above"]), (14.0, "latency", "balanced"))
        self.assertEqual(params["gap"], [2, 26])
        self.assertEqual(params["training_hits"], 4)
        self.assertEqual(threshold_choice(params, snap(13)), "latency")
        self.assertEqual(threshold_choice(params, snap(14)), "balanced")

    def test_more_hits_beat_a_wider_gap(self):
        examples = [(0, ["latency"]), (1, ["latency"]), (3, ["balanced"]), (30, ["balanced"])]
        self.assertEqual(fit_threshold(examples)["cut"], 2.0)

    def test_a_single_training_value_cannot_place_a_cut(self):
        with self.assertRaises(FitError):
            fit_threshold([(2, ["latency"]), (2, ["balanced"])])


class LogisticTests(unittest.TestCase):
    def spec(self):
        return next(c for c in CONFIG["candidates"] if c["kind"] == "logistic") | {"steps": 300}

    def rows(self):
        under = [snap(p, 40_000, 300_000) for p in (0, 1, 2)]
        over = [snap(p, 900_000, 500_000) for p in (30, 80, 200)]
        names = self.spec()["features"]
        from aik_controller.fit import FEATURES

        return [[FEATURES[n](s) for n in names] for s in under + over], [["latency"]] * 3 + [["balanced", "reclaim"]] * 3

    def test_a_separable_set_is_fit_the_same_way_twice(self):
        rows, labels = self.rows()
        first = fit_logistic(rows, labels, self.spec())
        self.assertEqual(first, fit_logistic(rows, labels, self.spec()))
        self.assertEqual(logistic_choice(first, snap(1, 30_000, 250_000)), "latency")
        # balanced and reclaim share every target, so they tie and catalog order picks balanced.
        self.assertEqual(logistic_choice(first, snap(120, 800_000, 500_000)), "balanced")


class ScoreTests(unittest.TestCase):
    def test_hits_infeasible_abstain_and_regret_are_counted_per_decision(self):
        out = score([(UNDER, "latency", False), (UNDER, "balanced", True), (OVER, "latency", False)])
        self.assertEqual((out["decisions"], out["hits"], out["misses"]), (3, 1, 2))
        self.assertEqual((out["infeasible"], out["abstain"]), (1, 1))
        under = next(row for row in out["units"] if row["scenario"] == "s4-u15")
        self.assertEqual(under["p99_regret_max_us"], 15_000)
        over = next(row for row in out["units"] if row["scenario"] == "s4-u80")
        self.assertIsNone(over["p99_regret_max_us"])

    def test_selection_prefers_feasible_then_hits_then_the_simpler_candidate(self):
        dev = {c["id"]: {"infeasible": 0, "misses": 5} for c in CONFIG["candidates"]}
        dev["pending-threshold"] = {"infeasible": 0, "misses": 0}
        dev["logistic-v0"] = {"infeasible": 0, "misses": 0}
        self.assertEqual(select(CONFIG, dev), "pending-threshold")
        dev["pending-threshold"] = {"infeasible": 1, "misses": 0}
        self.assertEqual(select(CONFIG, dev), "logistic-v0")
        dev["laya-zero-shot"] = None
        with self.assertRaises(FitError):
            select(CONFIG, dev)


class ZeroShotTests(unittest.TestCase):
    def test_a_structural_failure_keeps_the_fallback_and_counts_as_an_abstain(self):
        docs = [{"labels_sha256": "a"}]
        report = {
            "labels_sha256": ["a"], "device": "cpu", "dtype": "torch.float32",
            "rows": [{"key": "k1", "kind": "disagree", "choice": "latency"}, {"key": "k2", "kind": "truncated", "choice": "latency"}],
        }
        self.assertEqual(_zero_shot_choices(report, docs), {"k1": ("latency", False), "k2": ("balanced", True)})
        for bad in ({"labels_sha256": ["b"]}, {"device": "cuda"}, {"dtype": "torch.float16"}):
            with self.subTest(bad), self.assertRaises(FitError):
                _zero_shot_choices(report | bad, docs)

    def test_every_branch_gets_one_forward_after_a_discarded_warmup(self):
        class Agent:
            def __init__(self):
                self.states = []

            def predict(self, state, question):
                self.states.append(state)
                return {
                    "answers": {"resource_profile": {"choice": "balanced", "answer_confidence": 0.4}},
                    "usage": {"input_tokens": 300, "state_tokens_dropped": 0},
                }

        unit = json.loads(INTEL[0].read_text())["units"][0]
        docs = [{"units": [{
            "status": unit["status"], "label": unit["label"],
            "branches": [{"key": "a", "snapshot": unit["snapshot"]}, {"key": "b", "snapshot": unit["snapshot"]}],
        }]}]
        agent = Agent()
        ticks = iter(range(10))
        rows = zero_shot_rows(agent, ROOT, docs, clock=lambda: next(ticks))
        self.assertEqual(len(agent.states), 3)
        self.assertEqual(agent.states[0]["profile"], "balanced")
        self.assertEqual([row["key"] for row in rows], ["a", "b"])
        self.assertEqual([row["kind"] for row in rows], ["disagree", "disagree"])
        self.assertEqual(rows[0]["forward_us"], 1_000_000)


class LabelFileTests(unittest.TestCase):
    def copy(self, change):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        doc = json.loads(INTEL[0].read_text())
        shutil.copy(DATA / doc["records"], Path(tmp.name) / doc["records"])
        change(doc)
        path = Path(tmp.name) / INTEL[0].name
        path.write_text(json.dumps(doc))
        return path

    def test_training_labels_are_not_development_or_calibration_labels(self):
        for split in ("development", "calibration"):
            with self.subTest(split), self.assertRaises(SplitError):
                load_labels(ROOT, INTEL[0], split)
        with self.assertRaises(FitError):
            load_labels(ROOT, INTEL[0], "final_test")

    def test_edited_or_mismatched_labels_are_refused(self):
        def records_sha(doc):
            doc["records_sha256"] = "0" * 64

        def label(doc):
            doc["units"][0]["label"] = ["throughput"]

        def sealed(doc):
            doc["split"] = "final_test"

        for change, error in ((records_sha, FitError), (label, FitError), (sealed, SplitError)):
            with self.subTest(change.__name__), self.assertRaises(error):
                load_labels(ROOT, self.copy(change), "training")

    def test_a_fit_reads_one_host_and_each_unit_once(self):
        def other_host(doc):
            doc["host_cpu"] = "AMD Ryzen 5 1600 Six-Core Processor"

        with self.assertRaises(FitError):
            run(ROOT, [INTEL[1], self.copy(other_host)], [], None)
        with self.assertRaises(FitError):
            run(ROOT, [INTEL[1], INTEL[1]], [], None)


class IntelTrainingFitTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.report = run(ROOT, INTEL, [], None)

    def test_the_threshold_separates_every_intel_training_decision(self):
        params = self.report["fitted"]["pending-threshold"]
        self.assertEqual((params["cut"], params["below"], params["above"]), (14.0, "latency", "balanced"))
        self.assertEqual(params["gap"], [2, 26])
        self.assertEqual(self.report["decisions"]["training"], 288)
        training = self.report["scores"]["pending-threshold"]["training"]
        self.assertEqual((training["hits"], training["infeasible"]), (288, 0))

    def test_the_heuristic_is_infeasible_in_every_overload_decision(self):
        training = self.report["scores"]["heuristic-v0"]["training"]
        self.assertEqual((training["hits"], training["infeasible"]), (144, 144))

    def test_no_selection_without_development_labels(self):
        self.assertIsNone(self.report["selected"])
        self.assertIsNone(self.report["scores"]["laya-zero-shot"]["development"])


class FreezeAndCalibrateTests(unittest.TestCase):
    """Uses the Intel training fit with a stand-in selection; no calibration label exists yet."""

    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        cls.dir = Path(cls.tmp.name)
        report = run(ROOT, INTEL[:2], [], None)
        report["scores"]["pending-threshold"]["development"] = report["scores"]["pending-threshold"]["training"]
        cls.report = report

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def write(self, name, doc):
        path = self.dir / name
        path.write_text(json.dumps(doc))
        return path

    def stand_in_calibration(self):
        doc = json.loads(INTEL[2].read_text())
        shutil.copy(DATA / doc["records"], self.dir / doc["records"])
        doc["split"] = "calibration"
        return self.write("labels-calibration-stand-in.json", doc)

    def test_nothing_is_frozen_without_a_development_selection(self):
        with self.assertRaises(FitError):
            freeze(ROOT, self.write("unselected.json", self.report))
        laya = self.report | {"selected": "laya-zero-shot"}
        with self.assertRaises(FitError):
            freeze(ROOT, self.write("laya.json", laya))
        edited = self.report | {"selected": "pending-threshold", "fit_sha256": "0" * 64}
        with self.assertRaises(FitError):
            freeze(ROOT, self.write("edited.json", edited))

    def test_a_frozen_threshold_is_scored_once_on_its_own_host(self):
        path = self.write("fit.json", self.report | {"selected": "pending-threshold"})
        candidate = freeze(ROOT, path)
        self.assertEqual(candidate["kind"], "threshold")
        self.assertEqual(candidate["params"], self.report["fitted"]["pending-threshold"])
        self.assertEqual(candidate["development"]["hits"], 192)
        frozen = self.write("candidate.json", candidate)
        calibration = self.stand_in_calibration()
        out = calibrate(ROOT, frozen, [calibration])
        self.assertEqual((out["score"]["decisions"], out["score"]["hits"], out["score"]["infeasible"]), (96, 96, 0))
        self.assertEqual((out["units"], out["units_all_hit"]), (8, 8))
        self.assertAlmostEqual(out["units_all_hit_wilson95"][0], 0.6756, places=4)
        other = self.write("other-host.json", candidate | {"host_cpu": "AMD Ryzen 5 1600 Six-Core Processor"})
        with self.assertRaises(FitError):
            calibrate(ROOT, other, [calibration])
        with self.assertRaises(SplitError):
            calibrate(ROOT, frozen, [INTEL[2]])

    def test_the_wilson_interval_needs_units(self):
        self.assertIsNone(wilson(0, 0))
        low, high = wilson(4, 8)
        self.assertLess(low, 0.5)
        self.assertGreater(high, 0.5)


if __name__ == "__main__":
    unittest.main()

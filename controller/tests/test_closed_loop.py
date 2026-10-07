import argparse
import copy
import json
import tempfile
import unittest
from pathlib import Path

from aik_controller import closed_loop
from aik_controller.branches import (
    BranchError,
    check_record,
    comparability_flags,
    label_unit,
    parse_boot,
    snapshot,
)
from aik_controller.closed_loop import ClosedLoopError, check_decision, guest_args, score_unit
from aik_controller.jobs import load_family

ROOT = Path(__file__).resolve().parents[2]
FAMILY = load_family(ROOT / "configs" / "workloads" / "jobs-v2.json")
OBJECTIVE = json.loads((ROOT / "configs" / "objective-v2.json").read_text())
CANDIDATE_PATH = ROOT / "configs" / "candidate-v0-i7-10750h.json"
CANDIDATE = json.loads(CANDIDATE_PATH.read_text())
INTEL = "Intel(R) Core(TM) i7-10750H CPU @ 2.60GHz"

# A real Intel KVM boot of the candidate branch on development unit s12-u65/151.
BOOT = f"""\
FERRUM_START policy-guest
FERRUM_BRANCH_BEGIN family=jobs-v2 scenario=s12-u65 seed=151 profile=candidate jobs=451 schedule_fnv64=0xbcce088c4a318853 spins_per_ms=41154 measured_spins_per_ms=41154
FERRUM_BRANCH_STATE window_us=1000000 decision_late_us=33 latency_queue=20 latency_runnable=3 latency_service_us=500244 latency_wait_samples=41 latency_max_wait_us=576461 latency_completions=41 batch_queue=3 batch_runnable=3 batch_service_us=499728 batch_completions=972 maintenance_service_us=0 system_service_us=0
FERRUM_BRANCH_DECISION controller=pending-threshold cut=14 below=latency above=balanced pending=20 choice=balanced decide_us=26
FERRUM_BRANCH_OUTCOME scenario=s12-u65 seed=151 profile=candidate offered=265 completed=220 completion_bp=8301 p50_us=1486997 p90_us=censored p95_us=censored p99_us=censored batch_units=4766 latency_service_us=2497388 batch_service_us=2497011 batch_spins_per_ms=39274
FERRUM_BRANCH_OK
raw_exit=3 accel=kvm host_cpu={INTEL} kvm_module=kvm_intel
"""


def forced(profile, p99, batch, completion=10_000, pending=10):
    lines = [line for line in BOOT.splitlines() if not line.startswith("FERRUM_BRANCH_DECISION")]
    out = parse_boot("\n".join(lines).replace("profile=candidate", f"profile={profile}"))
    out["state"]["latency_queue"] = pending
    out["outcome"].update(p99_us=p99, batch_service_us=batch, completion_bp=completion)
    return out


def chosen(p99, batch, completion=10_000, pending=10, repeat=0):
    out = copy.deepcopy(parse_boot(BOOT))
    choice = "balanced" if pending >= 14 else "latency"
    out["state"]["latency_queue"] = pending
    out["decision"].update(pending=pending, choice=choice)
    out["outcome"].update(p99_us=p99, batch_service_us=batch, completion_bp=completion)
    out.update(repeat=repeat, attempt=0)
    return out


def labelled(rows):
    """rows: profile -> list of (p99, batch). The unit as fit.load_labels returns it."""
    records = [forced(profile, *values) for profile, repeats in rows.items() for values in repeats]
    unit = label_unit(records, OBJECTIVE)
    unit["branches"] = [{"snapshot": snapshot(r)} for r in records]
    return unit


UNIT = {
    "balanced": [(40_000, 3_000_000)] * 3,
    "latency": [(20_000, 2_990_000), (21_000, 2_990_000), (22_000, 2_990_000)],
    "throughput": [(60_000, 3_100_000)] * 3,
    "reclaim": [(45_000, 2_000_000)] * 3,
}


class ParseTests(unittest.TestCase):
    def test_a_candidate_boot_keeps_its_decision(self):
        record = parse_boot(BOOT)
        self.assertEqual(record["profile"], "candidate")
        self.assertEqual(record["decision"]["choice"], "balanced")
        self.assertEqual(record["decision"]["pending"], record["state"]["latency_queue"])
        self.assertEqual(record["decision"]["decide_us"], 26)
        self.assertEqual(comparability_flags(record, FAMILY, OBJECTIVE), [])

    def test_labels_refuse_a_candidate_record(self):
        with self.assertRaises(BranchError):
            check_record(parse_boot(BOOT), FAMILY, OBJECTIVE)

    def test_a_decision_must_match_the_announced_choice(self):
        lines = [line for line in BOOT.splitlines() if not line.startswith("FERRUM_BRANCH_DECISION")]
        with self.assertRaises(BranchError):
            parse_boot("\n".join(lines))
        with self.assertRaises(BranchError):
            parse_boot(BOOT.replace("profile=candidate", "profile=balanced"))


class DecisionTests(unittest.TestCase):
    def test_guest_args_carry_the_frozen_threshold(self):
        self.assertEqual(
            guest_args(CANDIDATE),
            "--candidate=pending-threshold --cut=14.0 --below=latency --above=balanced",
        )
        office = {"kind": "threshold", "params": {"cut": 14.5, "below": "latency", "above": "reclaim"}}
        self.assertEqual(
            guest_args(office),
            "--candidate=pending-threshold --cut=14.5 --below=latency --above=reclaim",
        )
        with self.assertRaises(ClosedLoopError):
            guest_args({"kind": "logistic", "params": {}})

    def test_the_guest_choice_matches_the_offline_rule(self):
        self.assertEqual(check_decision(parse_boot(BOOT) | {"repeat": 0, "attempt": 0}, CANDIDATE), "balanced")
        self.assertEqual(check_decision(chosen(20_000, 3_000_000, pending=13), CANDIDATE), "latency")

    def test_any_disagreement_stops_the_run(self):
        base = chosen(20_000, 3_000_000, pending=13)
        wrong = copy.deepcopy(base)
        wrong["decision"]["choice"] = "balanced"
        other_cut = copy.deepcopy(base)
        other_cut["decision"]["cut"] = "14.5"
        other_state = copy.deepcopy(base)
        other_state["state"]["latency_queue"] = 15
        not_candidate = forced("latency", 20_000, 3_000_000) | {"repeat": 0, "attempt": 0}
        for record in (wrong, other_cut, other_state, not_candidate):
            with self.assertRaises(ClosedLoopError):
                check_decision(record, CANDIDATE)


class ScoreTests(unittest.TestCase):
    def setUp(self):
        self.unit = labelled(UNIT)
        self.assertEqual(self.unit["label"], ["latency"])

    def test_repeats_inside_the_best_range_hit(self):
        rows = [chosen(21_500, 2_990_000, repeat=r) for r in range(3)]
        row = score_unit(self.unit, rows, OBJECTIVE, CANDIDATE["params"])
        self.assertEqual(row["closed_loop"], "hit")
        self.assertEqual(row["choices"], {"latency": 3})
        self.assertEqual(row["choices_in_label"], 3)
        self.assertEqual(row["offline_choices"], {"latency": 12})

    def test_repeats_outside_the_best_range_miss(self):
        rows = [chosen(30_000, 2_990_000, repeat=r) for r in range(3)]
        self.assertEqual(score_unit(self.unit, rows, OBJECTIVE, CANDIDATE["params"])["closed_loop"], "miss")

    def test_feasibility_is_against_the_units_own_balanced_branches(self):
        rows = [chosen(20_000, 2_000_000, repeat=r) for r in range(3)]
        row = score_unit(self.unit, rows, OBJECTIVE, CANDIDATE["params"])
        self.assertEqual(row["closed_loop"], "infeasible")
        self.assertEqual(row["summary"]["batch_retained_bp"], 6667)

    def test_a_censored_best_is_reached_on_completion(self):
        unit = labelled({
            "balanced": [(None, 3_000_000)] * 3,
            "latency": [(None, 2_990_000)] * 3,
            "throughput": [(None, 3_100_000)] * 3,
            "reclaim": [(None, 2_000_000)] * 3,
        })
        rows = [chosen(None, 3_000_000, pending=20, repeat=r) for r in range(3)]
        row = score_unit(unit, rows, OBJECTIVE, CANDIDATE["params"])
        self.assertEqual(row["closed_loop"], "hit")
        self.assertIsNone(row["summary"]["p99_median"])

    def test_fewer_repeats_than_the_objective_are_incomplete(self):
        rows = [chosen(21_500, 2_990_000, repeat=r) for r in range(2)]
        self.assertEqual(score_unit(self.unit, rows, OBJECTIVE, CANDIDATE["params"])["closed_loop"], "incomplete")


def _args(out, **extra):
    base = dict(
        candidate=str(CANDIDATE_PATH),
        split="development",
        scenario=["s12-u65"],
        seed=[151],
        repeats=2,
        retries=0,
        timeout_s=40,
        sealed_evaluation=False,
        calibration_report=None,
        fill=False,
        out=str(out),
    )
    return argparse.Namespace(**(base | extra))


class CollectTests(unittest.TestCase):
    def test_collect_writes_checked_candidate_records(self):
        calls = []

        def boot(root, family, unit, profile, timeout_s, serial, choice):
            calls.append((unit["scenario"], unit["seed"], profile, choice))
            return BOOT

        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "closed.jsonl"
            closed_loop.collect(_args(out), ROOT, boot=boot, committed=lambda root, path: True, this_host=lambda: INTEL)
            entries = [json.loads(line) for line in out.read_text().splitlines()]
            self.assertEqual(len(entries), 2)
            self.assertEqual({e["choice"] for e in entries}, {"balanced"})
            self.assertEqual([e["flags"] for e in entries], [[], []])
            self.assertEqual(calls[0][3], guest_args(CANDIDATE))
            with self.assertRaises(ClosedLoopError):
                closed_loop.collect(_args(out), ROOT, boot=boot, committed=lambda root, path: True, this_host=lambda: INTEL)
            closed_loop.collect(_args(out, fill=True), ROOT, boot=boot, committed=lambda root, path: True, this_host=lambda: INTEL)
            self.assertEqual(len(out.read_text().splitlines()), 2)

    def test_a_disagreeing_guest_is_written_down_and_stops_the_run(self):
        def boot(*args):
            return BOOT.replace("choice=balanced", "choice=latency")

        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "closed.jsonl"
            with self.assertRaises(ClosedLoopError):
                closed_loop.collect(_args(out), ROOT, boot=boot, committed=lambda root, path: True, this_host=lambda: INTEL)
            entries = [json.loads(line) for line in out.read_text().splitlines()]
            self.assertEqual(len(entries), 1)
            self.assertIn("offline rule chooses balanced", entries[0]["error"])

    def test_guards_before_any_boot(self):
        def boot(*args):
            raise AssertionError("booted")

        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "closed.jsonl"
            cases = [
                (_args(out), lambda root, path: False, INTEL),
                (_args(out), lambda root, path: True, "AMD Ryzen 5 1600 Six-Core Processor"),
                (_args(out, split="final_test", scenario=None, seed=None), lambda root, path: True, INTEL),
                (
                    _args(out, split="final_test", scenario=None, seed=None, sealed_evaluation=True),
                    lambda root, path: True,
                    INTEL,
                ),
            ]
            for args, committed, host in cases:
                with self.assertRaises(ClosedLoopError):
                    closed_loop.collect(args, ROOT, boot=boot, committed=committed, this_host=lambda: host)
            self.assertFalse(out.exists())

    def test_a_sealed_run_needs_a_calibration_of_this_candidate(self):
        def boot(*args):
            raise AssertionError("booted")

        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "closed.jsonl"
            report = Path(tmp) / "calibration.json"
            report.write_text(json.dumps({"candidate_sha256": "0" * 64}))
            args = _args(out, split="final_test", scenario=None, seed=None, sealed_evaluation=True, calibration_report=str(report))
            with self.assertRaises(ClosedLoopError):
                closed_loop.collect(args, ROOT, boot=boot, committed=lambda root, path: True, this_host=lambda: INTEL)
            self.assertFalse(out.exists())


class ReportTests(unittest.TestCase):
    LABELS = ROOT / "data" / "jobs-v2" / "labels-development-seed151-i7-10750h.json"

    def _write(self, tmp, records):
        sha = closed_loop._sha256(CANDIDATE_PATH)
        path = Path(tmp) / "closed.jsonl"
        lines = []
        for record in records:
            entry = record | {
                "split": "development",
                "candidate_sha256": sha,
                "manifest_sha256": CANDIDATE["manifest_sha256"],
                "flags": [],
            }
            lines.append(json.dumps(entry))
        path.write_text("\n".join(lines) + "\n")
        return path

    def test_the_report_scores_each_unit_beside_the_offline_score(self):
        with tempfile.TemporaryDirectory() as tmp:
            records = [parse_boot(BOOT) | {"repeat": r, "attempt": 0} for r in range(3)]
            report = closed_loop.score(ROOT, CANDIDATE_PATH, [self.LABELS], [self._write(tmp, records)])
        self.assertEqual(report["units"], 8)
        self.assertEqual(report["units_complete"], 1)
        self.assertEqual(report["offline"]["hits"], report["offline"]["decisions"])
        row = next(r for r in report["rows"] if (r["scenario"], r["seed"]) == ("s12-u65", 151))
        self.assertEqual(row["choices"], {"balanced": 3})
        self.assertNotEqual(row["closed_loop"], "incomplete")

    def test_records_of_an_unlabelled_unit_are_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            stray = parse_boot(BOOT) | {"repeat": 0, "attempt": 0, "seed": 999}
            with self.assertRaises(ClosedLoopError):
                closed_loop.score(ROOT, CANDIDATE_PATH, [self.LABELS], [self._write(tmp, [stray])])


if __name__ == "__main__":
    unittest.main()

import copy
import json
import unittest
from pathlib import Path

from aik_controller.branches import (
    BATCH_UNIT_US,
    BranchError,
    check_record,
    label_unit,
    missing,
    parse_boot,
    plan,
    prefix_drift_bp,
    snapshot,
)
from aik_controller.features import encode, load_edges
from aik_controller.jobs import load_family

ROOT = Path(__file__).resolve().parents[2]
FAMILY = load_family(ROOT / "configs" / "workloads" / "jobs-v1.json")
OBJECTIVE = json.loads((ROOT / "configs" / "objective-v1.json").read_text())
FAMILY_V2 = load_family(ROOT / "configs" / "workloads" / "jobs-v2.json")
OBJECTIVE_V2 = json.loads((ROOT / "configs" / "objective-v2.json").read_text())

BOOT = """\
FERRUM_START policy-guest
FERRUM_BRANCH_BEGIN family=jobs-v1 scenario=s4-u35 seed=101 profile=latency jobs=851 schedule_fnv64=0xd58e8e4af254388f spins_per_ms=39723 measured_spins_per_ms=39723
FERRUM_BRANCH_STATE window_us=1000000 decision_late_us=29 latency_queue=2 latency_runnable=2 latency_service_us=357100 latency_wait_samples=88 latency_max_wait_us=73142 latency_completions=88 batch_queue=3 batch_runnable=3 batch_service_us=642876 batch_completions=1242 maintenance_service_us=0 system_service_us=0
FERRUM_BRANCH_OUTCOME scenario=s4-u35 seed=101 profile=latency offered=494 completed=494 completion_bp=10000 p50_us=12122 p90_us=25213 p95_us=27664 p99_us=34112 batch_units=6000 latency_service_us=1986914 batch_service_us=3009178 batch_spins_per_ms=39600
FERRUM_BRANCH_OK
raw_exit=3 accel=kvm host_cpu=Intel(R) Core(TM) i7-10750H CPU @ 2.60GHz kvm_module=kvm_intel
"""


def record(profile, p99, batch, completion=10_000, pending=2, speed=39_600):
    base = parse_boot(BOOT)
    out = copy.deepcopy(base)
    out["profile"] = profile
    out["state"]["latency_queue"] = pending
    out["outcome"].update(
        p99_us=p99, batch_service_us=batch, completion_bp=completion, batch_spins_per_ms=speed
    )
    return out


def unit(rows):
    """rows: profile -> list of (p99, batch[, completion[, pending]])."""
    out = []
    for profile, repeats in rows.items():
        for values in repeats:
            out.append(record(profile, *values))
    return out


class ParseTests(unittest.TestCase):
    def test_a_clean_boot_becomes_a_record(self):
        parsed = parse_boot(BOOT)
        self.assertEqual(parsed["profile"], "latency")
        self.assertEqual(parsed["state"]["latency_queue"], 2)
        self.assertEqual(parsed["outcome"]["p99_us"], 34112)
        self.assertEqual(parsed["host"]["kvm_module"], "kvm_intel")
        self.assertEqual(parsed["host"]["host_cpu"], "Intel(R) Core(TM) i7-10750H CPU @ 2.60GHz")
        self.assertEqual(check_record(parsed, FAMILY, OBJECTIVE), [])

    def test_censored_percentiles_are_none(self):
        text = BOOT.replace("p99_us=34112", "p99_us=censored")
        self.assertIsNone(parse_boot(text)["outcome"]["p99_us"])

    def test_partial_failed_or_unclean_boots_are_refused(self):
        for broken in (
            BOOT.replace("FERRUM_BRANCH_OK\n", ""),
            BOOT.replace("raw_exit=3", "raw_exit=1"),
            BOOT.replace("FERRUM_BRANCH_OK", "FERRUM_BRANCH_FAIL workers did not register"),
            BOOT.replace("scenario=s4-u35 seed=101 profile=latency offered", "scenario=s4-u35 seed=101 profile=balanced offered"),
        ):
            with self.assertRaises(BranchError):
                parse_boot(broken)

    def test_a_different_schedule_is_refused_and_a_drifting_boot_is_flagged(self):
        other = parse_boot(BOOT.replace("0xd58e8e4af254388f", "0x0000000000000001"))
        with self.assertRaises(BranchError):
            check_record(other, FAMILY, OBJECTIVE)
        slow = record("latency", 34_000, 3_000_000, speed=37_000)
        self.assertEqual(check_record(slow, FAMILY, OBJECTIVE), ["disturbed"])

    def test_prefix_speed_comes_from_batch_units_against_batch_time(self):
        self.assertEqual(BATCH_UNIT_US, FAMILY["batch_unit_us"])
        # 1242 units of 500 us in 642876 us of batch CPU ran 3.4% slow.
        self.assertEqual(prefix_drift_bp(parse_boot(BOOT)), 341)

    def test_objective_v2_also_refuses_a_prefix_that_ran_off_speed(self):
        v2 = parse_boot(BOOT.replace("family=jobs-v1", "family=jobs-v2"))
        self.assertEqual(check_record(v2, FAMILY_V2, OBJECTIVE_V2), [])
        v2["state"]["batch_completions"] = 1400
        self.assertEqual(prefix_drift_bp(v2), 888)
        self.assertEqual(check_record(v2, FAMILY_V2, OBJECTIVE_V2), ["disturbed"])
        v1 = parse_boot(BOOT)
        v1["state"]["batch_completions"] = 1400
        self.assertEqual(check_record(v1, FAMILY, OBJECTIVE), [])
        with self.assertRaises(BranchError):
            check_record(v1, FAMILY_V2, OBJECTIVE_V2)

    def test_the_pre_decision_snapshot_encodes(self):
        snap = snapshot(parse_boot(BOOT))
        self.assertEqual(snap["current_profile"], "balanced")
        self.assertEqual(snap["groups"][0]["queue_len"], 2)
        self.assertEqual(snap["groups"][1]["cpu_service_us"], 642876)
        encode(snap, load_edges(ROOT / "configs" / "features-v0.json"))


class LabelTests(unittest.TestCase):
    def test_latency_wins_when_it_keeps_batch_and_cuts_p99_beyond_the_repeats(self):
        records = unit({
            "balanced": [(119_000, 3_019_000), (130_000, 2_949_000), (115_000, 3_026_000)],
            "reclaim": [(124_000, 2_934_000), (115_000, 3_027_000), (128_000, 2_990_000)],
            "latency": [(34_000, 3_009_000), (35_000, 2_990_000), (36_000, 3_000_000)],
            "throughput": [(None, 3_700_000, 7_500), (None, 3_720_000, 7_600), (None, 3_690_000, 7_400)],
        })
        out = label_unit(records, OBJECTIVE)
        self.assertEqual(out["status"], "labelled")
        self.assertEqual(out["label"], ["latency"])
        self.assertFalse(out["profiles"]["throughput"]["feasible"])
        self.assertTrue(out["profiles"]["reclaim"]["feasible"])

    def test_overlapping_repeats_are_a_tie_and_equal_weights_tie_each_other(self):
        records = unit({
            "balanced": [(120_000, 3_000_000), (125_000, 3_000_000), (130_000, 3_000_000)],
            "reclaim": [(118_000, 3_000_000), (126_000, 3_000_000), (140_000, 3_000_000)],
            "latency": [(124_000, 3_000_000), (131_000, 3_000_000), (150_000, 3_000_000)],
            "throughput": [(None, 3_700_000, 7_500)] * 3,
        })
        out = label_unit(records, OBJECTIVE)
        self.assertEqual(out["status"], "tie")
        self.assertEqual(out["label"], ["balanced", "latency", "reclaim"])

    def test_a_profile_that_takes_batch_cpu_below_the_retention_bar_is_infeasible(self):
        records = unit({
            "balanced": [(None, 2_500_000, 9_100)] * 3,
            "reclaim": [(None, 2_500_000, 9_050), (None, 2_500_000, 9_150), (None, 2_500_000, 9_100)],
            "latency": [(60_000, 2_250_000)] * 3,
            "throughput": [(None, 3_700_000, 7_000)] * 3,
        })
        out = label_unit(records, OBJECTIVE)
        self.assertFalse(out["profiles"]["latency"]["feasible"])
        self.assertEqual(out["profiles"]["latency"]["batch_retained_bp"], 9_000)
        self.assertTrue(out["primary_censored"])
        self.assertEqual(out["label"], ["balanced", "reclaim"])

    def test_a_missing_repeat_leaves_the_unit_incomplete(self):
        records = unit({
            "balanced": [(120_000, 3_000_000)] * 3,
            "reclaim": [(120_000, 3_000_000)] * 3,
            "latency": [(35_000, 3_000_000)] * 2,
            "throughput": [(None, 3_700_000, 7_500)] * 3,
        })
        self.assertEqual(label_unit(records, OBJECTIVE)["status"], "incomplete")

    def test_a_prefix_that_diverges_is_kept_out_of_supervised_labels(self):
        records = unit({
            "balanced": [(120_000, 3_000_000, 10_000, 2)] * 3,
            "reclaim": [(120_000, 3_000_000, 10_000, 2)] * 3,
            "latency": [(35_000, 3_000_000, 10_000, 9)] * 3,
            "throughput": [(None, 3_700_000, 7_500, 2)] * 3,
        })
        out = label_unit(records, OBJECTIVE)
        self.assertEqual(out["status"], "diverged")
        self.assertEqual(out["pending_spread"], 7)


class PlanTests(unittest.TestCase):
    def test_every_branch_is_planned_once_per_repeat_in_a_shuffled_order(self):
        units = [{"scenario": "s4-u15", "seed": 101}, {"scenario": "s4-u55", "seed": 101}]
        steps = plan(units, OBJECTIVE["candidates"], 3, order_seed=1)
        self.assertEqual(len(steps), 2 * 4 * 3)
        for repeat in range(3):
            for item in units:
                profiles = [p for u, p, r in steps if u is item and r == repeat]
                self.assertEqual(sorted(profiles), sorted(OBJECTIVE["candidates"]))
        orders = {tuple(p for u, p, r in steps if u is units[0] and r == repeat) for repeat in range(3)}
        self.assertGreater(len(orders), 1)
        self.assertEqual(steps, plan(units, OBJECTIVE["candidates"], 3, order_seed=1))

    def test_fill_reruns_only_branches_without_a_usable_record(self):
        units = [{"scenario": "s4-u15", "seed": 101}]
        steps = plan(units, OBJECTIVE["candidates"], 2, order_seed=1)
        done = [
            {"scenario": "s4-u15", "seed": 101, "profile": profile, "repeat": 0, "flags": []}
            for profile in OBJECTIVE["candidates"]
        ]
        done[0]["flags"] = ["disturbed"]
        done.append({"scenario": "s4-u15", "seed": 101, "profile": "latency", "repeat": 1, "error": "raw exit 124"})
        left = missing(steps, done)
        self.assertEqual(len(left), 1 + 4)
        self.assertIn((units[0], OBJECTIVE["candidates"][0], 0), left)


if __name__ == "__main__":
    unittest.main()

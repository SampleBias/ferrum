import unittest
from pathlib import Path

from aik_controller.interval import (
    PUBLISHED_BUDGET_US,
    IntervalError,
    judge,
    load_declared,
    select_budget,
)
from aik_controller.laya_timing import RECORDED_WARM_LATER_US, RECORDED_WARM_US, envelope

ROOT = Path(__file__).resolve().parents[2]


class IntervalTests(unittest.TestCase):
    def test_published_budget_stands_when_inference_leaves_transport_room(self):
        self.assertIsNone(select_budget(100_000))
        decision = judge(100_000, 200_000)
        self.assertEqual(decision["experiment"], "published")
        self.assertEqual(decision["budget_us"], PUBLISHED_BUDGET_US)
        self.assertTrue(decision["fits_published"])
        self.assertFalse(decision["replaces_guest_deadline"])

    def test_two_seconds_is_the_smallest_budget_past_750ms(self):
        self.assertEqual(select_budget(750_001), 2_000_000)
        self.assertEqual(select_budget(1_000_000), 2_000_000)

    def test_five_seconds_is_used_only_when_two_seconds_lack_headroom(self):
        self.assertEqual(select_budget(1_000_001), 5_000_000)
        self.assertEqual(judge(1_500_000, 100_000)["experiment"], "acceptance-5s-v0")

    def test_no_documented_budget_is_invented_past_five_seconds(self):
        with self.assertRaises(IntervalError):
            select_budget(4_000_001)

    def test_intel_record_selects_two_seconds_and_still_needs_warmup(self):
        p99 = envelope(list(RECORDED_WARM_US))["p99_us"]
        decision = judge(p99, 3_842_048)
        recorded = load_declared(ROOT / "configs" / "acceptance-2s-v0.json")
        self.assertEqual(decision["experiment"], recorded["id"])
        self.assertEqual(decision["budget_us"], recorded["budget_us"])
        self.assertEqual(decision["headroom_us"], 1_116_737)
        self.assertFalse(decision["fits_published"])
        self.assertFalse(decision["cold_fits_budget"])
        self.assertFalse(decision["replaces_guest_deadline"])
        self.assertFalse(recorded["replaces_guest_deadline"])

    def test_later_intel_envelope_selects_five_seconds(self):
        summary = envelope(list(RECORDED_WARM_LATER_US))
        self.assertEqual(summary["n"], 19)
        self.assertEqual(summary["p99_us"], 1_059_871)
        self.assertEqual(summary["p50_us"], 812_293)
        decision = judge(summary["p99_us"], 796_929)
        recorded = load_declared(ROOT / "configs" / "acceptance-5s-v0.json")
        self.assertEqual(decision["experiment"], recorded["id"])
        self.assertEqual(decision["budget_us"], recorded["budget_us"])
        self.assertEqual(decision["headroom_us"], 3_940_129)
        self.assertTrue(decision["cold_fits_budget"])
        self.assertFalse(decision["fits_published"])
        self.assertFalse(decision["replaces_guest_deadline"])
        self.assertFalse(recorded["replaces_guest_deadline"])


if __name__ == "__main__":
    unittest.main()

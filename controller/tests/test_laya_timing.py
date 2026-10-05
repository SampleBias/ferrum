import unittest

from aik_controller.laya_offline import QUESTION_ID, resource_question
from aik_controller.laya_timing import (
    ACCEPTANCE_BUDGET_US,
    RECORDED_WARM_US,
    envelope,
    measure,
    percentile_us,
)

ROOT_CASES = 19


class StepClock:
    """Each read returns the current time, then advances by the next delta."""

    def __init__(self, deltas):
        self.now = 0.0
        self.deltas = list(deltas)

    def __call__(self):
        current = self.now
        self.now += self.deltas.pop(0)
        return current


class FakeAgent:
    def __init__(self, script):
        self.script = list(script)
        self.seen = []

    def predict(self, state, questions):
        self.seen.append((state, questions))
        return self.script.pop(0)


def answer(choice):
    return {
        "answers": {
            QUESTION_ID: {
                "choice": choice,
                "answer_confidence": 0.4,
                "probabilities": {choice: 0.7},
            }
        },
        "usage": {"input_tokens": 300, "state_tokens": 40, "state_tokens_dropped": 0},
    }


class TimingTests(unittest.TestCase):
    def test_nearest_rank_percentile(self):
        self.assertEqual(percentile_us([40, 10, 30, 20], 0.50), 20)
        self.assertEqual(percentile_us([10, 20, 30], 0.99), 30)
        self.assertEqual(percentile_us([5], 1), 5)

    def test_envelope_uses_the_acceptance_budget(self):
        under = envelope([100_000, 200_000, 740_000])
        self.assertTrue(under["fits_budget"])
        self.assertEqual(under["budget_us"], ACCEPTANCE_BUDGET_US)
        self.assertEqual(under["max_us"], 740_000)
        over = envelope([100_000, 800_000])
        self.assertFalse(over["fits_budget"])
        self.assertEqual(over["p99_us"], 800_000)
        with self.assertRaises(ValueError):
            envelope([])

    def test_measure_discards_the_cold_forward(self):
        # One cold forward, then one warm forward per recorded window.
        durations = [1.5, 0.0] + [0.1, 0.0] * ROOT_CASES
        agent = FakeAgent([answer("latency")] * (ROOT_CASES + 1))
        from pathlib import Path

        root = Path(__file__).resolve().parents[2]
        report = measure(agent, root, clock=StepClock(durations))
        self.assertEqual(report["cold_us"], 1_500_000)
        self.assertEqual(report["warm"]["n"], ROOT_CASES)
        self.assertEqual(report["warm"]["min_us"], 100_000)
        self.assertEqual(report["warm"]["max_us"], 100_000)
        self.assertTrue(report["warm"]["fits_budget"])
        self.assertEqual(len(agent.seen), ROOT_CASES + 1)
        self.assertEqual(agent.seen[0][1], resource_question())
        self.assertIn("profile", agent.seen[0][0])
        self.assertEqual(report["cases"][0]["forward_us"], 100_000)
        self.assertNotIn("stage", report)

    def test_recorded_cpu_forwards_miss_the_published_budget(self):
        summary = envelope(list(RECORDED_WARM_US))
        self.assertEqual(summary["n"], 19)
        self.assertEqual(summary["min_us"], 754_893)
        self.assertEqual(summary["p50_us"], 777_387)
        self.assertEqual(summary["p99_us"], 883_263)
        self.assertGreater(summary["min_us"], ACCEPTANCE_BUDGET_US)
        self.assertFalse(summary["fits_budget"])


if __name__ == "__main__":
    unittest.main()

import tempfile
import unittest
from pathlib import Path

from aik_controller.features import encode, load_edges
from aik_controller.laya_offline import QUESTION_ID, cases
from aik_controller.shadow import (
    ShadowError,
    ShadowWorker,
    prepare,
    send_heuristic_then_shadow,
    shadow_trace_line,
)

ROOT = Path(__file__).resolve().parents[2]


class FakeAgent:
    def __init__(self, script):
        self.script = list(script)
        self.calls = 0
        self.device = "cpu"

    def predict(self, state, questions):
        self.calls += 1
        self.seen = (state, questions)
        return self.script.pop(0)


def answer(choice, truncated=False):
    return {
        "answers": {
            QUESTION_ID: {
                "choice": choice,
                "answer_confidence": 0.41,
                "probabilities": {choice: 0.41},
            }
        },
        "usage": {
            "input_tokens": 316,
            "state_tokens": 40,
            "state_tokens_dropped": 1 if truncated else 0,
            "truncated": truncated,
        },
    }


def features():
    edges = load_edges(ROOT / "configs" / "features-v0.json")
    return encode(cases(ROOT)[0]["snapshot"], edges)


class StepClock:
    def __init__(self, deltas):
        self.now = 0.0
        self.deltas = list(deltas)

    def __call__(self):
        current = self.now
        self.now += self.deltas.pop(0)
        return current


class ShadowTests(unittest.TestCase):
    def test_trace_line_counts_kinds_without_claiming_a_stage(self):
        notes = [
            {"kind": "agree", "staged": False},
            {"kind": "disagree", "staged": False},
            {"kind": "abstain", "staged": False},
        ]
        self.assertEqual(
            shadow_trace_line(notes),
            "SHADOW_TRACE rounds=3 agree=1 disagree=1 abstain=1 staged=false",
        )

    def test_a_staged_note_fails_the_trace(self):
        notes = [{"kind": "agree", "staged": True}]
        self.assertIn("staged=true", shadow_trace_line(notes))

    def test_proposal_is_sent_before_the_model_runs(self):
        agent = FakeAgent([answer("throughput"), answer("reclaim")])
        worker = ShadowWorker(agent, clock=StepClock([0.0, 0.0, 0.2, 0.0]))
        worker.warmup(features())
        events = []

        def write_proposal(profile):
            events.append(("proposal", profile, agent.calls))

        note = send_heuristic_then_shadow(write_proposal, worker, features(), "latency")
        self.assertEqual(events, [("proposal", "latency", 1)])
        self.assertEqual(note["choice"], "reclaim")
        self.assertEqual(note["kind"], "disagree")
        self.assertEqual(note["heuristic"], "latency")
        self.assertFalse(note["staged"])
        self.assertEqual(note["forward_us"], 200_000)
        self.assertIn("profile", agent.seen[0])
        self.assertEqual(agent.seen[1], resource_question())

    def test_structural_failures_abstain_without_staging(self):
        worker = ShadowWorker(FakeAgent([answer("latency", truncated=True)]))
        worker.warm = True
        note = worker.score({"profile": "balanced"}, "latency")
        self.assertEqual(note["kind"], "abstain")
        self.assertEqual(note["structural"], "truncated")
        self.assertFalse(note["staged"])
        missing = ShadowWorker(FakeAgent([{"answers": {}, "usage": {}}]))
        missing.warm = True
        self.assertEqual(missing.score({}, "balanced")["structural"], "missing")

    def test_a_busy_worker_abstains_instead_of_overlapping(self):
        agent = FakeAgent([answer("latency")])
        worker = ShadowWorker(agent)
        worker.warm = True
        worker.busy = True
        note = worker.score({}, "latency")
        self.assertEqual(note["structural"], "model_busy")
        self.assertEqual(agent.calls, 0)
        self.assertFalse(note["staged"])

    def test_score_before_warmup_is_refused(self):
        worker = ShadowWorker(FakeAgent([]))
        with self.assertRaises(ShadowError):
            worker.score({}, "balanced")

    def test_prepare_refuses_an_absent_checkpoint(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(SystemExit):
                prepare(ROOT, Path(tmp))


def resource_question():
    from aik_controller.laya_offline import resource_question as question

    return question()


if __name__ == "__main__":
    unittest.main()

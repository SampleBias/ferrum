import tempfile
import threading
import unittest
from pathlib import Path

from aik_controller.features import encode, load_edges
from aik_controller.laya_offline import QUESTION_ID, cases
from aik_controller.shadow import (
    ShadowError,
    ShadowWorker,
    decide_live,
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


class ManualClock:
    def __init__(self) -> None:
        self.now = 0.0

    def __call__(self) -> float:
        return self.now


class GateAgent:
    """The first predict waits. Later predicts return immediately."""

    def __init__(self, script) -> None:
        self.script = list(script)
        self.device = "cpu"
        self.calls = 0
        self.started = threading.Event()
        self.release = threading.Event()
        self._lock = threading.Lock()

    def predict(self, state, questions):
        with self._lock:
            self.calls += 1
            call = self.calls
            result = self.script.pop(0)
        if call == 1:
            self.started.set()
            self.release.wait()
        return result


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

    def test_a_newer_request_displaces_the_queued_one(self):
        agent = GateAgent([answer("latency"), answer("reclaim")])
        worker = ShadowWorker(agent)
        worker.warm = True
        notes = {}

        def run(name, state, window=None):
            notes[name] = worker.score(state, "balanced", window)

        first = threading.Thread(target=run, args=("first", {"n": 1}), daemon=True)
        second = threading.Thread(target=run, args=("second", {"n": 2}, 750_000), daemon=True)
        third = threading.Thread(target=run, args=("third", {"n": 3}, 750_000), daemon=True)
        first.start()
        self.assertTrue(agent.started.wait(2))
        second.start()
        self._wait_until(lambda: worker._queued is not None)
        third.start()
        self._wait_until(lambda: "second" in notes)
        self.assertEqual(notes["second"]["structural"], "model_busy")
        self.assertEqual(agent.calls, 1)
        self.assertFalse(notes["second"]["staged"])
        agent.release.set()
        first.join(2)
        third.join(2)
        self.assertFalse(first.is_alive() or third.is_alive())
        self.assertEqual(notes["first"]["choice"], "latency")
        self.assertEqual(notes["third"]["choice"], "reclaim")
        self.assertEqual(notes["third"]["queue_wait_us"] >= 0, True)
        self.assertEqual(agent.calls, 2)

    def test_a_queued_request_past_its_window_does_not_start(self):
        clock = ManualClock()
        agent = GateAgent([answer("latency")])
        worker = ShadowWorker(agent, clock=clock)
        worker.warm = True
        notes = {}
        first = threading.Thread(
            target=lambda: notes.update(first=worker.score({"n": 1}, "balanced")),
            daemon=True,
        )
        second = threading.Thread(
            target=lambda: notes.update(second=worker.score({"n": 2}, "throughput", 750_000)),
            daemon=True,
        )
        first.start()
        self.assertTrue(agent.started.wait(2))
        second.start()
        self._wait_until(lambda: worker._queued is not None)
        clock.now = 1.0
        agent.release.set()
        first.join(2)
        second.join(2)
        self.assertFalse(first.is_alive() or second.is_alive())
        self.assertEqual(notes["second"]["structural"], "expired")
        self.assertEqual(notes["second"]["queue_wait_us"], 1_000_000)
        self.assertEqual(decide_live(notes["second"])["reason"], "expired")
        self.assertEqual(agent.calls, 1)

    def test_a_queued_request_inside_its_window_still_runs(self):
        clock = ManualClock()
        agent = GateAgent([answer("latency"), answer("reclaim")])
        worker = ShadowWorker(agent, clock=clock)
        worker.warm = True
        notes = {}
        first = threading.Thread(
            target=lambda: notes.update(first=worker.score({"n": 1}, "balanced", 750_000)),
            daemon=True,
        )
        second = threading.Thread(
            target=lambda: notes.update(second=worker.score({"n": 2}, "throughput", 750_000)),
            daemon=True,
        )
        first.start()
        self.assertTrue(agent.started.wait(2))
        second.start()
        self._wait_until(lambda: worker._queued is not None)
        clock.now = 0.75
        agent.release.set()
        first.join(2)
        second.join(2)
        self.assertFalse(first.is_alive() or second.is_alive())
        self.assertEqual(notes["second"]["choice"], "reclaim")
        self.assertEqual(notes["second"]["queue_wait_us"], 750_000)
        self.assertEqual(agent.calls, 2)

    def _wait_until(self, ready) -> None:
        for _ in range(200):
            if ready():
                return
            threading.Event().wait(0.01)
        self.fail("queued request did not arrive")

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

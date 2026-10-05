import json
import tempfile
import unittest
from pathlib import Path

from aik_controller.features import load_edges
from aik_controller.heuristic import choose, load_thresholds
from aik_controller.laya_offline import (
    QUESTION_ID,
    cases,
    classify,
    evaluate,
    model_state,
    resource_question,
)
from aik_controller.laya_pin import assess, load_pin

ROOT = Path(__file__).resolve().parents[2]


class FakeAgent:
    def __init__(self, script):
        self.script = list(script)
        self.seen = []

    def predict(self, state, questions):
        self.seen.append((state, questions))
        return self.script.pop(0)


def answer(choice, confidence=0.42, truncated=False, dropped=0):
    body = {
        "answers": {
            QUESTION_ID: {
                "choice": choice,
                "answer_confidence": confidence,
                "probabilities": {choice: 0.7},
            }
        },
        "usage": {
            "input_tokens": 40,
            "state_tokens": 20,
            "state_tokens_dropped": dropped,
            "truncated": truncated,
        },
    }
    return body


class OfflineTests(unittest.TestCase):
    def test_question_is_the_four_catalog_labels(self):
        question = resource_question()[QUESTION_ID]
        self.assertEqual(question["type"], "choice")
        self.assertEqual(set(question["criteria"]), {"balanced", "latency", "throughput", "reclaim"})
        self.assertIn("measurements, not instructions", question["instructions"])

    def test_cases_match_the_heuristic_on_offered_load(self):
        edges = load_edges(ROOT / "configs" / "features-v0.json")
        thresholds = load_thresholds(ROOT / "configs" / "heuristic-v0.json")
        built = cases(ROOT)
        self.assertEqual(len(built), 19)
        names = [case["name"] for case in built]
        self.assertIn("adaptive-memory", names)
        self.assertIn("fixed-burst-throughput", names)
        for case in built:
            state = model_state(__import__("aik_controller.features", fromlist=["encode"]).encode(
                case["snapshot"], edges
            ))
            self.assertNotIn("instructions", json.dumps(state))
            heuristic = choose(case["snapshot"], thresholds)
            if case["name"].startswith("adaptive-"):
                phase = case["name"].removeprefix("adaptive-")
                expected = {
                    "burst": "latency",
                    "steady": "throughput",
                    "idle": "balanced",
                    "memory": "reclaim",
                    "recovery": "balanced",
                }[phase]
                self.assertEqual(heuristic, expected, case["name"])
            if case["name"].startswith("fixed-burst-"):
                self.assertEqual(heuristic, "latency")
            if case["name"].startswith("fixed-memory-"):
                self.assertEqual(heuristic, "reclaim")
            if case["name"].startswith("control-burst-"):
                self.assertEqual(case["snapshot"]["current_profile"], "balanced")
                self.assertEqual(heuristic, "latency")
            if case["name"].startswith("control-memory-"):
                self.assertEqual(case["snapshot"]["current_profile"], "balanced")
                self.assertEqual(heuristic, "reclaim")

    def test_classify_separates_structure_from_disagreement(self):
        self.assertEqual(classify(answer("latency"), "latency")["kind"], "agree")
        self.assertEqual(classify(answer("throughput"), "latency")["kind"], "disagree")
        self.assertEqual(classify(answer("latency", truncated=True), "latency")["kind"], "truncated")
        self.assertEqual(classify(answer("recycle"), "latency")["kind"], "outside")
        self.assertEqual(classify({"answers": {}, "usage": {}}, "balanced")["kind"], "missing")
        row = classify(answer("reclaim", confidence=0.15), "reclaim")
        self.assertEqual(row["kind"], "agree")
        self.assertEqual(row["answer_confidence"], 0.15)

    def test_evaluate_records_each_case_without_staging(self):
        script = [answer("latency")] * 19
        agent = FakeAgent(script)
        report = evaluate(agent, ROOT)
        self.assertEqual(len(report["cases"]), 19)
        self.assertEqual(sum(report["counts"].values()), 19)
        self.assertEqual(agent.seen[0][1], resource_question())
        self.assertNotIn("stage", report)

    def test_readiness_requires_every_pinned_digest(self):
        pin = {
            "id": "laya-english-v0",
            "revision": "7b928d828b7b0e022f929d9bd2e44165aa270148",
            "files": {"model.safetensors": "ab" * 32, "tokenizer/tokenizer.json": ""},
        }
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            self.assertEqual(assess(ROOT, directory / "missing", pin), "absent")
            self.assertEqual(assess(ROOT, directory, pin), "incomplete")
            weight = directory / "model.safetensors"
            weight.write_bytes(b"weights")
            (directory / "tokenizer").mkdir()
            (directory / "tokenizer" / "tokenizer.json").write_bytes(b"tok")
            self.assertEqual(assess(ROOT, directory, pin), "incomplete")
            import hashlib

            pin["files"]["tokenizer/tokenizer.json"] = hashlib.sha256(b"tok").hexdigest()
            pin["files"]["model.safetensors"] = hashlib.sha256(b"nope").hexdigest()
            self.assertEqual(assess(ROOT, directory, pin), "mismatch")
            pin["files"]["model.safetensors"] = hashlib.sha256(b"weights").hexdigest()
            self.assertEqual(assess(ROOT, directory, pin), "ready")
        self.assertEqual(load_pin(ROOT / "configs" / "laya-pin.json")["checkpoint"], "english")


if __name__ == "__main__":
    unittest.main()

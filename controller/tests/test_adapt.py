import json
import unittest
from pathlib import Path

from aik_controller.adapt import AdaptError, checkpoint_dir, development_score, load_declaration, rows
from aik_controller.fit import FitError
from aik_controller.splits import SplitError
from aik_controller.features import PROFILES
from aik_controller.laya_offline import QUESTION_ID

ROOT = Path(__file__).resolve().parents[2]
DECL = json.loads((ROOT / "configs" / "adapt-v0-i7-10750h.json").read_text())
TRAINING = [ROOT / rel for rel in DECL["reads"]]


class DeclarationTests(unittest.TestCase):
    def test_the_declaration_fits_on_training_only(self):
        doc = load_declaration(ROOT / "configs" / "adapt-v0-i7-10750h.json")
        self.assertEqual(doc["fit_reads"], ["training"])
        self.assertTrue(doc["freeze_encoder"])
        self.assertEqual(doc["loss"], "soft-ce")
        self.assertNotIn("final_test", json.dumps(doc["reads"]))
        out = checkpoint_dir(ROOT, doc)
        self.assertEqual(out.name, "adapt-v0-i7-10750h")
        with self.assertRaises(AdaptError):
            checkpoint_dir(ROOT, doc | {"output": doc["base"]})


class RowTests(unittest.TestCase):
    def test_each_scored_training_decision_is_one_row(self):
        built = rows(ROOT, DECL, TRAINING)
        self.assertEqual(len(built), 288)
        for row in built:
            weights = row["gold"][QUESTION_ID]["probabilities"]
            self.assertEqual(list(weights), list(PROFILES))
            self.assertAlmostEqual(sum(weights.values()), 1.0)
            self.assertEqual(row["questions"][QUESTION_ID]["type"], "choice")
            self.assertIn("profile", row["state"])
        under = [row for row in built if row["state"]["latency_queue_len"] < 14]
        self.assertTrue(under)
        self.assertTrue(all(row["gold"][QUESTION_ID]["probabilities"]["latency"] == 1.0 for row in under))

    def test_a_file_outside_the_declaration_is_refused(self):
        other = [TRAINING[0], TRAINING[1], ROOT / "data" / "jobs-v2" / "labels-development-seed151-i7-10750h.json"]
        with self.assertRaises(AdaptError):
            rows(ROOT, DECL, other)

    def test_selection_refuses_a_training_file(self):
        with self.assertRaises((AdaptError, FitError, SplitError)):
            development_score(ROOT, DECL, TRAINING[0])


if __name__ == "__main__":
    unittest.main()

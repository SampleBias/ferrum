import json
import tempfile
import unittest
from pathlib import Path

from aik_controller.splits import (
    SplitError,
    check_fit_labels,
    check_screening,
    check_selection_labels,
    fit,
    fit_units,
    load_branched,
    load_manifest,
    manifest_sha256,
    units,
)

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "configs" / "splits-v0.json"
V1 = ROOT / "configs" / "splits-v1.json"
V2 = ROOT / "configs" / "splits-v2.json"


class SplitTests(unittest.TestCase):
    def test_development_windows_match_the_manifest_and_are_not_a_fit_set(self):
        doc = check_screening(ROOT, MANIFEST)
        self.assertEqual(len(doc["development"]["cases"]), 19)
        self.assertEqual(doc["development"]["role"], "screening")
        self.assertFalse(doc["fit_allowed"])
        self.assertEqual(doc["training"], [])
        self.assertEqual(doc["calibration"], [])
        self.assertEqual(doc["final_test"], [])
        with self.assertRaises(SplitError):
            fit(ROOT, MANIFEST)

    def test_a_manifest_that_allows_a_fit_is_refused(self):
        doc = json.loads(MANIFEST.read_text())
        doc["fit_allowed"] = True
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "splits.json"
            path.write_text(json.dumps(doc))
            with self.assertRaises(SplitError):
                load_manifest(path)

    def test_a_final_test_inside_this_manifest_is_refused(self):
        doc = json.loads(MANIFEST.read_text())
        doc["final_test"] = ["other-scenario"]
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "splits.json"
            path.write_text(json.dumps(doc))
            with self.assertRaises(SplitError):
                load_manifest(path)


SIZES = ("training", "development", "calibration", "final_test", "out_of_distribution")


class SplitV1Tests(unittest.TestCase):
    def test_the_pilot_manifest_still_matches_its_family_objective_and_schedules(self):
        doc = load_branched(ROOT, V1)
        self.assertEqual((doc["family"], doc["objective"]), ("jobs-v1", "objective-v1"))
        self.assertEqual({split: len(doc[split]) for split in SIZES}, {
            "training": 18, "development": 6, "calibration": 12,
            "final_test": 10, "out_of_distribution": 4,
        })
        self.assertEqual(
            manifest_sha256(V1), "89ce507aad82b70755a74ab640a5d4034306c89e21dbd30190616023f7f05a1a"
        )


class SplitV2Tests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.doc = load_branched(ROOT)

    def tampered(self, change):
        doc = json.loads(V2.read_text())
        change(doc)
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        path = Path(tmp.name) / "splits-v2.json"
        path.write_text(json.dumps(doc))
        return path

    def test_the_frozen_manifest_matches_its_family_objective_and_schedules(self):
        self.assertEqual((self.doc["id"], self.doc["family"]), ("splits-v2", "jobs-v2"))
        self.assertEqual({split: len(self.doc[split]) for split in SIZES}, {
            "training": 24, "development": 8, "calibration": 16,
            "final_test": 16, "out_of_distribution": 6,
        })
        trained = {unit["scenario"] for unit in self.doc["training"]}
        self.assertEqual(trained, {
            "s4-u15", "s4-u35", "s4-u65", "s4-u80", "s12-u15", "s12-u35", "s12-u65", "s12-u80",
        })
        unseen = set(self.doc["unseen_in_final_test"])
        self.assertEqual(unseen, {"s4-u45", "s12-u45", "s4-u72", "s12-u72"})

    def test_sealed_splits_open_only_for_a_sealed_evaluation(self):
        for split in ("final_test", "out_of_distribution"):
            with self.assertRaises(SplitError):
                units(self.doc, split)
            self.assertTrue(units(self.doc, split, sealed_evaluation=True))
        self.assertEqual(fit_units(ROOT), self.doc["training"])

    def test_a_manifest_must_pair_with_its_own_family_and_objective(self):
        def other_family(doc):
            doc["family"] = "jobs-v1"

        def other_objective(doc):
            doc["objective"] = "objective-v1"

        for change in (other_family, other_objective):
            with self.subTest(change.__name__), self.assertRaises(SplitError):
                load_branched(ROOT, self.tampered(change))

    def test_leaks_and_edits_are_refused(self):
        def shared_seed(doc):
            doc["calibration"][0]["seed"] = doc["training"][0]["seed"]
            doc["calibration"][0]["scenario"] = "s4-u65"

        def ood_in_training(doc):
            doc["training"][0] = dict(doc["out_of_distribution"][0], seed=999)

        def wrong_digest(doc):
            doc["training"][0]["schedule_fnv64"] = "0x0000000000000000"

        def edited_family(doc):
            doc["family_sha256"] = "0" * 64

        def open_final_test(doc):
            doc["sealed"] = ["out_of_distribution"]

        def unseen_trained(doc):
            doc["unseen_in_final_test"] = ["s4-u35"]

        for change in (shared_seed, ood_in_training, wrong_digest, edited_family, open_final_test, unseen_trained):
            with self.subTest(change.__name__), self.assertRaises(SplitError):
                load_branched(ROOT, self.tampered(change))

    def test_a_fit_reads_training_labels_from_this_manifest_only(self):
        sha = manifest_sha256(V2)
        check_fit_labels({"split": "training", "manifest_sha256": sha}, ROOT)
        for labels in (
            {"split": "training", "manifest_sha256": manifest_sha256(V1)},
            {"split": "final_test", "manifest_sha256": sha},
            {"split": "calibration", "manifest_sha256": sha},
        ):
            with self.assertRaises(SplitError):
                check_fit_labels(labels, ROOT)

    def test_a_selection_reads_development_labels_from_this_manifest_only(self):
        sha = manifest_sha256(V2)
        check_selection_labels({"split": "development", "manifest_sha256": sha}, ROOT)
        for labels in (
            {"split": "development", "manifest_sha256": manifest_sha256(V1)},
            {"split": "training", "manifest_sha256": sha},
            {"split": "calibration", "manifest_sha256": sha},
            {"split": "out_of_distribution", "manifest_sha256": sha},
        ):
            with self.assertRaises(SplitError):
                check_selection_labels(labels, ROOT)


if __name__ == "__main__":
    unittest.main()

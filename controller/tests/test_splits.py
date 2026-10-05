import json
import tempfile
import unittest
from pathlib import Path

from aik_controller.splits import SplitError, check_screening, fit, load_manifest

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "configs" / "splits-v0.json"


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


if __name__ == "__main__":
    unittest.main()

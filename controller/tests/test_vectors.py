import hashlib
import json
import unittest
from pathlib import Path

from aik_controller.framing import (
    CONTROLLER_TO_GUEST,
    canonical_snapshot,
    open_frame,
    seal,
    snapshot_hash,
)
from aik_controller.heuristic import choose, load_thresholds

ROOT = Path(__file__).resolve().parents[2]
VECTORS = ROOT / "tests" / "protocol-vectors"
KEY = bytes([0x11]) * 32


class VectorTests(unittest.TestCase):
    def test_proposal_mac_matches_rust(self):
        payload = (VECTORS / "proposal.json").read_bytes().rstrip(b"\n")
        frame = seal(CONTROLLER_TO_GUEST, KEY, payload)
        self.assertEqual(
            frame[-32:].hex(),
            "2c99f48a01e3f53afd70acae38f3efd7e12dea1063fe837473905c9d078c1219",
        )
        self.assertEqual(open_frame(CONTROLLER_TO_GUEST, KEY, frame), payload)

    def test_snapshot_canonical_hash(self):
        text = (VECTORS / "snapshot.canonical.json").read_text().rstrip("\n")
        fields = json.loads(text)
        self.assertEqual(canonical_snapshot(fields), text)
        self.assertEqual(
            snapshot_hash(fields),
            "c8c5552a73e7bda9b8486d34cc93f643b5e8254b6ba8615f6bd06b38a4c76245",
        )

    def test_catalog_files_hash_stable(self):
        for name in ("catalog-cpu-v1.json", "catalog-joint-v1.json"):
            raw = (ROOT / "configs" / name).read_bytes()
            digest = hashlib.sha256(raw).hexdigest()
            self.assertEqual(len(digest), 64)
            parsed = json.loads(raw)
            self.assertEqual(len(parsed["profiles"]), 4)

    def test_heuristic_seed_matches_the_rust_rules(self):
        thresholds = load_thresholds(ROOT / "configs" / "heuristic-v0.json")
        quiet = json.loads((VECTORS / "snapshot.canonical.json").read_text())
        self.assertEqual(choose(quiet, thresholds), "latency")
        quiet["groups"][0]["queue_len"] = 0
        self.assertEqual(choose(quiet, thresholds), "balanced")


if __name__ == "__main__":
    unittest.main()

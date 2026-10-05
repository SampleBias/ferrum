import json
import socket
import struct
import threading
import unittest
from pathlib import Path

from aik_controller.framing import (
    CONTROLLER_TO_GUEST,
    GUEST_TO_CONTROLLER,
    open_frame,
    seal,
    snapshot_hash,
)
from aik_controller.server import main
from aik_controller.shadow import acceptance_window_us, decide_live, live_trace_line, withhold_expired

ROOT = Path(__file__).resolve().parents[2]
KEY = bytes([0x11]) * 32


def note(**overrides):
    row = {
        "choice": "reclaim",
        "kind": "disagree",
        "structural": None,
        "heuristic": "balanced",
        "answer_confidence": 0.11,
        "forward_us": 900_000,
        "queue_wait_us": 0,
        "input_tokens": 310,
        "staged": False,
    }
    row.update(overrides)
    return row


class LiveDecisionTests(unittest.TestCase):
    def test_a_low_confidence_choice_is_still_proposed(self):
        decision = decide_live(note(answer_confidence=0.11))
        self.assertEqual(
            decision,
            {
                "kind": "proposal",
                "profile": "reclaim",
                "reason_code": "model_choice",
                "answer_confidence_bp": 1100,
            },
        )

    def test_a_structural_failure_is_an_abstain(self):
        decision = decide_live(
            note(choice=None, kind="abstain", structural="truncated", answer_confidence=None)
        )
        self.assertEqual(decision, {"kind": "abstain", "reason": "truncated_input"})
        self.assertEqual(
            decide_live(note(choice=None, kind="abstain", structural="model_busy"))["reason"],
            "model_busy",
        )
        self.assertEqual(
            decide_live(note(choice=None, kind="abstain", structural="backend"))["reason"],
            "backend_error",
        )

    def test_live_and_shadow_are_different_sessions(self):
        self.assertEqual(main(["--live", "--shadow", "--once"]), 2)

    def test_a_forward_inside_the_window_is_still_proposed(self):
        decision = withhold_expired(decide_live(note(forward_us=750_000)), note(forward_us=750_000), 750_000)
        self.assertEqual(decision["kind"], "proposal")

    def test_a_forward_past_the_window_is_not_a_proposal(self):
        decision = withhold_expired(decide_live(note(forward_us=750_001)), note(forward_us=750_001), 750_000)
        self.assertEqual(decision, {"kind": "abstain", "reason": "expired"})
        structural = decide_live(
            note(choice=None, kind="abstain", structural="truncated", forward_us=9_000_000)
        )
        self.assertEqual(
            withhold_expired(structural, note(forward_us=9_000_000), 750_000)["reason"],
            "truncated_input",
        )
        self.assertEqual(
            withhold_expired(decide_live(note(forward_us=900_000)), note(forward_us=900_000), None)[
                "kind"
            ],
            "proposal",
        )

    def test_the_trace_counts_the_frame_that_was_sent(self):
        self.assertEqual(
            live_trace_line([{"kind": "disagree", "sent": "abstain"}, {"kind": "agree", "sent": "proposal"}]),
            "LIVE_TRACE rounds=2 proposed=1 abstain=1",
        )

    def test_the_model_choice_is_the_proposal(self):
        message = self.roundtrip(note(forward_us=100_000))
        self.assertEqual(message["kind"], "proposal")
        self.assertEqual(message["profile"], "reclaim")
        self.assertEqual(message["reason_code"], "model_choice")
        self.assertEqual(message["answer_confidence_bp"], 1100)

    def test_a_slow_forward_is_an_expired_abstain(self):
        snapshot = json.loads(
            (ROOT / "tests" / "protocol-vectors" / "snapshot.canonical.json").read_text()
        )
        window = acceptance_window_us(snapshot)
        self.assertEqual(window, 750_000)
        message = self.roundtrip(note(forward_us=900_000), answer=False)
        self.assertEqual(message["kind"], "abstain")
        self.assertEqual(message["reason"], "expired")
        self.assertNotIn("profile", message)

    def test_an_abstain_does_not_name_a_profile(self):
        message = self.roundtrip(
            note(choice=None, kind="abstain", structural="truncated", answer_confidence=None),
            answer=False,
        )
        self.assertEqual(message["kind"], "abstain")
        self.assertEqual(message["reason"], "truncated_input")
        self.assertNotIn("profile", message)

    def roundtrip(self, scored: dict, answer: bool = True) -> dict:
        from aik_controller.features import load_edges
        from aik_controller.heuristic import load_thresholds
        from aik_controller.server import catalog_hash, exchange_round, load_key

        snapshot = json.loads((ROOT / "tests" / "protocol-vectors" / "snapshot.canonical.json").read_text())
        snapshot["snapshot_hash"] = snapshot_hash(
            {key: value for key, value in snapshot.items() if key != "snapshot_hash"}
        )
        catalog_path = ROOT / "configs" / "catalog-cpu-v1.json"
        catalog_doc = json.loads(catalog_path.read_text())
        identity = json.loads((ROOT / "configs" / "lab-identity.json").read_text())
        server, client = socket.socketpair()
        server.settimeout(5)
        client.settimeout(5)
        seen = {}

        def guest():
            client.sendall(seal(GUEST_TO_CONTROLLER, KEY, json.dumps(snapshot).encode()))
            seen["message"] = read_controller(client)
            if answer:
                client.sendall(
                    seal(
                        GUEST_TO_CONTROLLER,
                        KEY,
                        json.dumps(
                            {
                                "protocol": 1,
                                "kind": "reject",
                                "boot_id": snapshot["boot_id"],
                                "session_id": snapshot["session_id"],
                                "request_seq": snapshot["request_seq"],
                                "reason": "late",
                            }
                        ).encode(),
                    )
                )

        thread = threading.Thread(target=guest)
        thread.start()
        try:
            status = exchange_round(
                server,
                load_key(ROOT / "configs" / "lab-psk.hex"),
                {"id": catalog_doc["catalog_id"], "hash": catalog_hash(catalog_path)},
                load_thresholds(ROOT / "configs" / "heuristic-v0.json"),
                identity,
                {"boot_id": snapshot["boot_id"]},
                snapshot["session_id"],
                load_edges(ROOT / "configs" / "features-v0.json"),
                __import__("aik_controller.features", fromlist=["FeatureHistory"]).FeatureHistory(),
                live=Script(scored),
            )
        finally:
            thread.join()
            server.close()
            client.close()
        self.assertEqual(status, 0)
        return seen["message"]


class Script:
    def __init__(self, scored: dict) -> None:
        self.scored = scored

    def score(self, state: dict, heuristic: str, window_us: int | None = None) -> dict:
        self.state = state
        self.heuristic = heuristic
        self.window_us = window_us
        return self.scored


def read_controller(conn: socket.socket) -> dict:
    header = _exact(conn, 4)
    length = struct.unpack(">I", header)[0]
    payload = _exact(conn, length)
    tag = _exact(conn, 32)
    body = open_frame(CONTROLLER_TO_GUEST, KEY, header + payload + tag)
    return json.loads(body)


def _exact(conn: socket.socket, size: int) -> bytes:
    buf = bytearray()
    while len(buf) < size:
        chunk = conn.recv(size - len(buf))
        if not chunk:
            raise AssertionError("truncated")
        buf.extend(chunk)
    return bytes(buf)


if __name__ == "__main__":
    unittest.main()

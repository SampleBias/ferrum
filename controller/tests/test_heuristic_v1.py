import contextlib
import copy
import hashlib
import io
import json
import socket
import struct
import threading
import unittest
from pathlib import Path

from aik_controller import fit, splits, tune
from aik_controller.explain import explain_line
from aik_controller.features import FeatureHistory, load_edges
from aik_controller.framing import CONTROLLER_TO_GUEST, GUEST_TO_CONTROLLER, open_frame, seal, snapshot_hash
from aik_controller.heuristic import (
    NORMAL,
    OVERLOAD,
    Session,
    choose,
    evaluate_v1,
    load,
    load_thresholds,
    load_v1,
)

ROOT = Path(__file__).resolve().parents[2]
DATA = ROOT / "data" / "jobs-v2"
HOSTS = ("r5-1600", "i7-10750h")


def jobs_window(latency_us: int, batch_us: int, pending: int, wait_us: int, current: str = "balanced") -> dict:
    snap = json.loads((ROOT / "tests" / "policy-golden" / "heuristic-v1.json").read_text())["cases"][0]["snapshot"]
    snap = copy.deepcopy(snap)
    snap["current_profile"] = current
    snap["window_us"] = 1_000_000
    latency, batch, maintenance, system = snap["groups"]
    latency.update(queue_len=pending, runnable=min(pending, 3), cpu_service_us=latency_us, max_wait_us=wait_us, wait_samples=10)
    batch.update(queue_len=3, runnable=3, cpu_service_us=batch_us)
    maintenance.update(queue_len=0, runnable=0, cpu_service_us=0)
    system.update(cpu_service_us=0)
    return snap


class HeuristicV1Tests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.config = load_v1(ROOT)

    def test_golden_vectors_agree_with_rust(self):
        doc = json.loads((ROOT / "tests" / "policy-golden" / "heuristic-v1.json").read_text())
        self.assertEqual(doc["stall_wait_us"], self.config["stall_wait_us"])
        for case in doc["cases"]:
            decision = evaluate_v1(case["snapshot"], self.config, case["regime"])
            expect = case["expect"]
            got = {
                "profile": decision["profile"],
                "rule": decision["rule"],
                "regime": decision["regime"],
                "latency_load_bp": decision["signals"]["latency_load_bp"],
                "batch_retained_bp": decision["signals"]["batch_retained_bp"],
            }
            self.assertEqual(got, expect, case["name"])

    def test_heuristic_v0_is_unchanged(self):
        raw = (ROOT / "configs" / "heuristic-v0.json").read_bytes()
        for host in HOSTS:
            candidate = json.loads((ROOT / "configs" / f"candidate-v0-{host}.json").read_text())
            report = json.loads((DATA / f"fit-v0-{host}.json").read_text())
            self.assertEqual(report["heuristic_sha256"], hashlib.sha256(raw).hexdigest())
            self.assertEqual(candidate["fit"], "fit-v0")
        thresholds = load_thresholds(ROOT / "configs" / "heuristic-v0.json")
        labels = json.loads((DATA / "labels-training-seed101-r5-1600.json").read_text())
        for unit in labels["units"]:
            self.assertEqual(choose(unit["snapshot"], thresholds), unit["heuristic"])

    def test_the_stall_cut_reproduces_from_training_labels_only(self):
        derived = tune.check(ROOT, self.config)
        self.assertEqual(derived["reads"], "training")
        self.assertEqual(derived["stall_wait_us"], 303_756)
        self.assertLess(derived["under_capacity_max_wait_us"], self.config["stall_wait_us"])
        self.assertGreater(derived["overload_min_wait_us"], self.config["stall_wait_us"])
        self.assertTrue(all("training" in item["labels"] for item in derived["labels"]))

    def test_the_derivation_refuses_other_splits(self):
        for name in ("labels-development-seed151-r5-1600.json", "labels-final_test-seed301-r5-1600.json"):
            with self.assertRaises(splits.SplitError):
                tune.derive(ROOT, [DATA / name], self.config)

    def test_never_infeasible_on_the_design_splits(self):
        for split in ("training", "development"):
            for host in HOSTS:
                docs = [fit.load_labels(ROOT, path, split) for path in sorted(DATA.glob(f"labels-{split}-seed*-{host}.json"))]
                scored, _ = fit.decisions(docs, fit.load_config(ROOT))
                result = fit.score([(unit, evaluate_v1(b["snapshot"], self.config)["profile"], False) for unit, b in scored])
                self.assertEqual(result["infeasible"], 0, f"{split} {host}")
                v0 = fit.score([(unit, choose(b["snapshot"], self.config), False) for unit, b in scored])
                self.assertGreater(v0["infeasible"], 0)
                self.assertGreaterEqual(result["hits"], v0["hits"])

    def test_a_session_does_not_flip_inside_the_hysteresis_band(self):
        session = Session(self.config)
        loads = [350, 500, 470, 440, 470, 500, 430, 380, 470, 500]
        waits = [40, 600, 30, 20, 30, 30, 20, 20, 30, 700]
        trace = []
        for load_ms, wait_ms in zip(loads, waits):
            snap = jobs_window(load_ms * 1000, (1000 - load_ms) * 1000, 3 if wait_ms < 300 else 40, wait_ms * 1000)
            trace.append((session.decide(snap)["profile"], session.regime))
        self.assertEqual(
            trace,
            [
                ("latency", NORMAL),
                ("balanced", OVERLOAD),
                ("balanced", OVERLOAD),
                ("balanced", OVERLOAD),
                ("balanced", OVERLOAD),
                ("balanced", OVERLOAD),
                ("balanced", OVERLOAD),
                ("latency", NORMAL),
                ("latency", NORMAL),
                ("balanced", OVERLOAD),
            ],
        )

    def test_a_boosted_class_that_cuts_batch_returns_to_the_reference(self):
        snap = jobs_window(750_000, 250_000, 30, 100_000, current="latency")
        decision = evaluate_v1(snap, self.config)
        self.assertEqual((decision["profile"], decision["rule"], decision["regime"]), ("balanced", "batch_floor", OVERLOAD))

    def test_load_names_either_heuristic(self):
        self.assertEqual(Session(load(ROOT, "heuristic-v0")).id, "heuristic-v0")
        self.assertEqual(Session(load(ROOT, "heuristic-v1")).id, "heuristic-v1")
        self.assertEqual(Session(load(ROOT, "heuristic-v2")).id, "heuristic-v2")
        with self.assertRaises(ValueError):
            load_v1(ROOT, "heuristic-v0")

    def test_a_short_queue_needs_fourteen_jobs_before_v2_calls_it_stalled(self):
        v2 = load_v1(ROOT, "heuristic-v2")
        derived = tune.check_backlog(ROOT, v2)
        self.assertEqual(derived["backlog_min"], 14)
        self.assertEqual(derived["under_capacity_max_queue"], 2)
        self.assertEqual(derived["overload_min_queue"], 26)
        short = jobs_window(500_000, 500_000, 3, 400_000)
        long = jobs_window(500_000, 500_000, 26, 500_000)
        self.assertEqual(evaluate_v1(short, self.config)["profile"], "balanced")
        self.assertEqual(evaluate_v1(short, v2)["profile"], "latency")
        self.assertEqual(evaluate_v1(long, v2)["profile"], "balanced")
        for split in ("training", "development"):
            for host in HOSTS:
                docs = [fit.load_labels(ROOT, path, split) for path in sorted(DATA.glob(f"labels-{split}-seed*-{host}.json"))]
                scored, _ = fit.decisions(docs, fit.load_config(ROOT))
                result = fit.score([(unit, evaluate_v1(b["snapshot"], v2)["profile"], False) for unit, b in scored])
                self.assertEqual(result["infeasible"], 0, f"{split} {host}")


class ServerSessionTests(unittest.TestCase):
    """The live server keeps one heuristic-v1 regime per connection."""

    # Twenty latency jobs have waited 400 ms in every window, so v0 boosts
    # latency each time. Latency load falls 10000 -> 9000 -> 8000.
    WINDOWS = [(500_000, 500_000), (450_000, 540_000), (400_000, 590_000)]

    def exchange(self, session: Session, windows: list[tuple[int, int]]) -> tuple[list[str], str]:
        from aik_controller.server import catalog_hash, exchange_round, load_key

        key = load_key(ROOT / "configs" / "lab-psk.hex")
        base = json.loads((ROOT / "tests" / "protocol-vectors" / "snapshot.canonical.json").read_text())
        catalog_path = ROOT / "configs" / "catalog-cpu-v1.json"
        catalog = {"id": "cpu-v1", "hash": catalog_hash(catalog_path)}
        identity = json.loads((ROOT / "configs" / "lab-identity.json").read_text())
        edges = load_edges(ROOT / "configs" / "features-v0.json")
        history = FeatureHistory()
        server, client = socket.socketpair()
        server.settimeout(5)
        client.settimeout(5)
        chosen = []

        def guest():
            for seq, (latency_us, batch_us) in enumerate(windows, start=1):
                snap = copy.deepcopy(base)
                snap["request_seq"] = seq
                snap["groups"][0].update(
                    queue_len=20, runnable=3, cpu_service_us=latency_us, max_wait_us=400_000, wait_samples=10
                )
                snap["groups"][1].update(queue_len=3, runnable=3, cpu_service_us=batch_us)
                snap["snapshot_hash"] = snapshot_hash(snap)
                client.sendall(seal(GUEST_TO_CONTROLLER, key, json.dumps(snap).encode()))
                chosen.append(_controller_frame(client, key)["profile"])
                report = {
                    "protocol": 1,
                    "kind": "reject",
                    "boot_id": snap["boot_id"],
                    "session_id": snap["session_id"],
                    "request_seq": seq,
                    "reason": "late",
                }
                client.sendall(seal(GUEST_TO_CONTROLLER, key, json.dumps(report).encode()))

        thread = threading.Thread(target=guest)
        thread.start()
        out = io.StringIO()
        try:
            with contextlib.redirect_stdout(out):
                for _ in windows:
                    status = exchange_round(
                        server, key, catalog, session, identity, {"boot_id": base["boot_id"]},
                        base["session_id"], edges, history,
                    )
                    self.assertEqual(status, 0)
        finally:
            thread.join()
            server.close()
            client.close()
        return chosen, out.getvalue()

    def test_the_regime_carries_between_rounds_of_one_connection_only(self):
        config = load_v1(ROOT)
        chosen, log = self.exchange(Session(config), self.WINDOWS)
        self.assertEqual(chosen, ["balanced", "balanced", "latency"])
        self.assertIn("controller=heuristic-v1 choice=balanced rule=latency_overload regime=overload", log)
        self.assertIn("controller=heuristic-v1 choice=balanced rule=overload_hold regime=overload", log)
        self.assertIn("controller=heuristic-v1 choice=latency rule=latency_queue_or_wait_high regime=normal", log)
        fresh, _ = self.exchange(Session(config), self.WINDOWS[1:2])
        self.assertEqual(fresh, ["latency"])

    def test_heuristic_v0_still_boosts_every_round(self):
        chosen, log = self.exchange(Session(load(ROOT, "heuristic-v0")), self.WINDOWS)
        self.assertEqual(chosen, ["latency"] * 3)
        self.assertIn("heuristic=latency(latency_queue_or_wait_high)", log)

    def test_the_explain_line_names_v0_unchanged(self):
        snap = jobs_window(500_000, 500_000, 20, 400_000)
        v0 = load_thresholds(ROOT / "configs" / "heuristic-v0.json")
        self.assertEqual(
            explain_line(snap, v0), f"EXPLAIN seq={snap.get('request_seq', 0)} heuristic=latency(latency_queue_or_wait_high)"
        )


def _controller_frame(conn: socket.socket, key: bytes) -> dict:
    def exact(size: int) -> bytes:
        buf = bytearray()
        while len(buf) < size:
            chunk = conn.recv(size - len(buf))
            if not chunk:
                raise ConnectionError("closed")
            buf.extend(chunk)
        return bytes(buf)

    header = exact(4)
    rest = exact(struct.unpack(">I", header)[0] + 32)
    return json.loads(open_frame(CONTROLLER_TO_GUEST, key, header + rest))


if __name__ == "__main__":
    unittest.main()

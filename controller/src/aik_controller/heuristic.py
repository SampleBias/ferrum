"""Untuned development heuristic. Thresholds come from configs/heuristic-v0.json."""

import json
from pathlib import Path


def load_thresholds(path: Path) -> dict:
    return json.loads(path.read_text())


def choose(snapshot: dict, thresholds: dict) -> str:
    groups = {group["class"]: group for group in snapshot["groups"]}
    latency = groups["latency"]
    batch = groups["batch"]
    pressure = snapshot["pressure"]
    cap = pressure["managed_cap_bytes"]
    ratio = 0 if cap == 0 else pressure["managed_used_bytes"] * 10_000 // cap
    if (
        pressure["emergency"]
        or ratio >= thresholds["memory_pressure_bp"]
        or pressure["evictable_backlog_bytes"] >= thresholds["evictable_backlog_bytes"]
    ):
        return "reclaim"
    wait_high = (
        latency["wait_samples"] > 0
        and latency["max_wait_us"] >= thresholds["latency_wait_us_high"]
    )
    if latency["queue_len"] >= thresholds["latency_queue_high"] or wait_high:
        return "latency"
    if batch["queue_len"] >= thresholds["batch_queue_high"] and latency["queue_len"] == 0:
        return "throughput"
    return "balanced"

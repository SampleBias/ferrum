"""Versioned snapshot features for a later model.

The bins describe the counters. They do not name a profile. Laya is not called.
"""

import json
from pathlib import Path

CLASSES = ("latency", "batch", "maintenance", "system")
PROFILES = ("balanced", "latency", "throughput", "reclaim")
WORKLOADS = ("latency", "batch", "maintenance")


class FeatureError(ValueError):
    pass


def load_edges(path: Path) -> dict:
    edges = json.loads(path.read_text())
    if edges.get("id") != "features-v0" or edges.get("schema") != 1:
        raise FeatureError("feature schema is not features-v0")
    return edges


def laya_status(root: Path) -> str:
    """Whether the pinned English checkpoint is on disk. This does not download it."""
    from aik_controller.laya_pin import assess

    return assess(root)


def encode(
    snapshot: dict,
    edges: dict,
    previous_queue: dict | None = None,
    previous_ema: dict | None = None,
) -> dict:
    """Turn one snapshot into the model state. Free text on the snapshot is dropped."""
    groups = _groups(snapshot)
    profile = snapshot.get("current_profile")
    if profile not in PROFILES:
        raise FeatureError("current profile is not a catalog label")
    window = snapshot.get("window_us")
    if not isinstance(window, int) or window < 0:
        raise FeatureError("window is not a duration")
    pressure = _pressure(snapshot, edges)
    encoded_groups = {}
    for name in CLASSES:
        encoded_groups[name] = _group(
            groups[name],
            edges,
            window,
            None if previous_queue is None else previous_queue.get(name),
            None if previous_ema is None else previous_ema.get(name),
        )
    return {
        "feature_id": edges["id"],
        "schema": edges["schema"],
        "current_profile": profile,
        "window_us": window,
        "window_valid": window > 0,
        "groups": encoded_groups,
        "pressure": pressure,
    }


class FeatureHistory:
    """Current window plus an integer exponential average of each queue."""

    def __init__(self) -> None:
        self.previous_queue: dict | None = None
        self.queue_ema: dict | None = None

    def observe(self, snapshot: dict, edges: dict) -> dict:
        state = encode(snapshot, edges, self.previous_queue, self.queue_ema)
        queues = {name: state["groups"][name]["queue_len"] for name in CLASSES}
        alpha = edges["trend_alpha_bp"]
        if self.queue_ema is None:
            self.queue_ema = dict(queues)
        else:
            self.queue_ema = {
                name: (self.queue_ema[name] * (10_000 - alpha) + queues[name] * alpha) // 10_000
                for name in CLASSES
            }
        self.previous_queue = queues
        return state


def feature_line(state: dict) -> str:
    groups = state["groups"]
    queues = "/".join(groups[name]["queue_bin"] for name in WORKLOADS)
    trends = "/".join(groups[name]["queue_trend"] for name in WORKLOADS)
    pressure = state["pressure"]
    return (
        f"FEATURES schema={state['schema']} profile={state['current_profile']} "
        f"q={queues} trend={trends} pressure={pressure['pressure_bin']} "
        f"backlog={pressure['backlog_bin']} emergency={pressure['emergency']} "
        f"window_us={state['window_us']}"
    )


def _groups(snapshot: dict) -> dict:
    found = {}
    for group in snapshot.get("groups", []):
        name = group.get("class")
        if name not in CLASSES:
            raise FeatureError("class is not a catalog enum")
        if name in found:
            raise FeatureError("duplicate class")
        found[name] = group
    if set(found) != set(CLASSES):
        raise FeatureError("snapshot is missing a class")
    return found


def _group(group: dict, edges: dict, window: int, previous: int | None, previous_ema: int | None) -> dict:
    queue = group["queue_len"]
    if not isinstance(queue, int) or queue < 0:
        raise FeatureError("queue is not a count")
    runnable = group["runnable"]
    if not isinstance(runnable, int) or runnable < 0:
        raise FeatureError("runnable is not a count")
    service = group["cpu_service_us"]
    if not isinstance(service, int) or service < 0:
        raise FeatureError("service is not a duration")
    wait_samples = group["wait_samples"]
    max_wait = group["max_wait_us"]
    if not isinstance(wait_samples, int) or wait_samples < 0:
        raise FeatureError("wait sample count is not a count")
    if not isinstance(max_wait, int) or max_wait < 0:
        raise FeatureError("wait is not a duration")
    if wait_samples == 0 and max_wait != 0:
        raise FeatureError("wait without samples")
    if window == 0:
        share = None
        service_bin = "no_window"
    else:
        share = service * 10_000 // window
        service_bin = _service_bin(share, edges)
    alpha = edges["trend_alpha_bp"]
    updated_ema = queue if previous_ema is None else (previous_ema * (10_000 - alpha) + queue * alpha) // 10_000
    return {
        "queue_len": queue,
        "queue_bin": _queue_bin(queue, edges),
        "queue_trend": _trend(queue, previous),
        "queue_ema": updated_ema,
        "queue_ema_trend": _trend(updated_ema, previous_ema),
        "runnable": runnable,
        "service_share_bp": share,
        "service_bin": service_bin,
        "wait": _wait_bin(wait_samples, max_wait, edges),
    }


def _pressure(snapshot: dict, edges: dict) -> dict:
    pressure = snapshot["pressure"]
    used = pressure["managed_used_bytes"]
    cap = pressure["managed_cap_bytes"]
    backlog = pressure["evictable_backlog_bytes"]
    if not isinstance(used, int) or used < 0 or not isinstance(cap, int) or cap < 0:
        raise FeatureError("managed bytes are not a size")
    if not isinstance(backlog, int) or backlog < 0:
        raise FeatureError("backlog is not a size")
    if cap == 0:
        ratio = 0
        pressure_bin = "no_cap"
    else:
        ratio = used * 10_000 // cap
        if ratio >= edges["pressure_high_bp"]:
            pressure_bin = "high"
        elif ratio >= edges["pressure_medium_bp"]:
            pressure_bin = "medium"
        else:
            pressure_bin = "low"
    if backlog == 0:
        backlog_bin = "none"
    elif backlog >= edges["evictable_backlog_bytes"]:
        backlog_bin = "high"
    else:
        backlog_bin = "present"
    emergency = pressure["emergency"]
    if not isinstance(emergency, bool):
        raise FeatureError("emergency is not a flag")
    return {
        "managed_pressure_bp": ratio,
        "pressure_bin": pressure_bin,
        "evictable_backlog_bytes": backlog,
        "backlog_bin": backlog_bin,
        "emergency": "set" if emergency else "clear",
    }


def _queue_bin(queue: int, edges: dict) -> str:
    if queue <= 0:
        return "empty"
    if queue >= edges["queue_high_min"]:
        return "high"
    return "present"


def _service_bin(share: int, edges: dict) -> str:
    if share >= edges["service_dominant_bp"]:
        return "dominant"
    if share >= edges["service_high_bp"]:
        return "high"
    if share >= edges["service_mid_bp"]:
        return "mid"
    return "low"


def _wait_bin(samples: int, max_wait: int, edges: dict) -> str:
    if samples == 0:
        return "no_samples"
    if max_wait >= edges["wait_us_high"]:
        return "above"
    return "within"


def _trend(current: int, previous: int | None) -> str:
    if previous is None:
        return "no_history"
    if current > previous:
        return "rising"
    if current < previous:
        return "falling"
    return "flat"

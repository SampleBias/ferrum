"""Supervisory heuristics.

heuristic-v0 (configs/heuristic-v0.json) is the untuned development seed. It is
frozen: candidate files and evaluation reports name its hash.

heuristic-v1 (configs/heuristic-v1.json) keeps v0's rules for memory, latency
pressure and batch demand, and adds an overload regime in front of the latency
rule. Overload means latency already uses its whole reference share and its
backlog has stopped draining, or a boosted latency class is taking batch below
the objective's floor. Boosting latency then only moves CPU from batch, so v1
holds the reference profile until latency demand falls well under its share.
"""

import json
from pathlib import Path

CLASSES = ("latency", "batch", "maintenance")
NORMAL = "normal"
OVERLOAD = "overload"
REGIMES = (NORMAL, OVERLOAD)


def load_thresholds(path: Path) -> dict:
    return json.loads(path.read_text())


def choose(snapshot: dict, thresholds: dict) -> str:
    return _base_rule(snapshot, thresholds)[0]


def _memory_hot(snapshot: dict, thresholds: dict) -> bool:
    pressure = snapshot["pressure"]
    cap = pressure["managed_cap_bytes"]
    ratio = 0 if cap == 0 else pressure["managed_used_bytes"] * 10_000 // cap
    return (
        pressure["emergency"]
        or ratio >= thresholds["memory_pressure_bp"]
        or pressure["evictable_backlog_bytes"] >= thresholds["evictable_backlog_bytes"]
    )


def _base_rule(snapshot: dict, thresholds: dict) -> tuple[str, str]:
    """heuristic-v0: the profile and the rule that chose it."""
    groups = {group["class"]: group for group in snapshot["groups"]}
    latency = groups["latency"]
    batch = groups["batch"]
    if _memory_hot(snapshot, thresholds):
        return "reclaim", "memory_pressure_or_emergency"
    wait_high = (
        latency["wait_samples"] > 0
        and latency["max_wait_us"] >= thresholds["latency_wait_us_high"]
    )
    if latency["queue_len"] >= thresholds["latency_queue_high"] or wait_high:
        return "latency", "latency_queue_or_wait_high"
    if batch["queue_len"] >= thresholds["batch_queue_high"] and latency["queue_len"] == 0:
        return "throughput", "sustained_batch_demand"
    return "balanced", "default_balanced_fallback"


def load_v1(root: Path, name: str = "heuristic-v1") -> dict:
    """A v1 config with its catalog's CPU weights attached under `weights`."""
    if name not in ("heuristic-v1", "heuristic-v2"):
        raise ValueError(f"unknown heuristic {name}")
    config = json.loads((root / "configs" / f"{name}.json").read_text())
    if config.get("id") != name:
        raise ValueError(f"configs/{name}.json does not declare {name}")
    if name == "heuristic-v2" and "backlog_min" not in config:
        raise ValueError("heuristic-v2 requires backlog_min")
    catalog = json.loads((root / "configs" / f"catalog-{config['catalog']}.json").read_text())
    weights = {
        profile["id"]: tuple(profile["cpu_weights"][name] for name in CLASSES)
        for profile in catalog["profiles"]
    }
    if config["reference_profile"] not in weights:
        raise ValueError(f"{config['reference_profile']} is not a {config['catalog']} profile")
    return {**config, "weights": weights}


def _active(group: dict) -> bool:
    return group["runnable"] > 0 or group["queue_len"] > 0 or group["cpu_service_us"] > 0


def signals_v1(snapshot: dict, config: dict) -> dict:
    """Integer measurements v1 decides on. Every ratio is in basis points.

    A class's share under a profile is its weight over the summed weights of
    the classes that wanted CPU in the window. Latency always counts. Load is
    measured service over that share of the window, so 10000 means the class
    received exactly the share the profile gives it.
    """
    groups = {group["class"]: group for group in snapshot["groups"]}
    weights = config["weights"]
    reference = weights[config["reference_profile"]]
    current = weights[snapshot.get("current_profile", config["reference_profile"])]
    window = max(snapshot["window_us"], 1)
    active = [name == "latency" or _active(groups[name]) for name in CLASSES]
    ref_sum = sum(weight for weight, on in zip(reference, active) if on)
    cur_sum = sum(weight for weight, on in zip(current, active) if on)
    latency = groups["latency"]
    batch = groups["batch"]
    service = latency["cpu_service_us"]
    busy = sum(group["cpu_service_us"] for group in snapshot["groups"])
    return {
        "latency_load_bp": service * ref_sum * 10_000 // (window * reference[0]),
        "latency_current_bp": service * cur_sum * 10_000 // (window * current[0]),
        "batch_active": active[1],
        "batch_retained_bp": (
            batch["cpu_service_us"] * ref_sum * 10_000 // (window * reference[1]) if active[1] else None
        ),
        "busy_bp": busy * 10_000 // window,
        "latency_below_reference": current[0] * ref_sum < reference[0] * cur_sum,
        "stalled": latency["queue_len"] >= config.get("backlog_min", 1)
        and latency["max_wait_us"] >= config["stall_wait_us"],
    }


def evaluate_v1(snapshot: dict, config: dict, regime: str = NORMAL) -> dict:
    """heuristic-v1's profile, the rule that chose it, and the regime it leaves.

    `regime` is what the previous round of the same session returned. A
    one-shot decision starts from `normal`.
    """
    if regime not in REGIMES:
        raise ValueError(f"unknown regime {regime}")
    reference = config["reference_profile"]
    sig = signals_v1(snapshot, config)

    def out(profile: str, rule: str, left: str) -> dict:
        return {"profile": profile, "rule": rule, "regime": left, "signals": sig}

    if _memory_hot(snapshot, config):
        return out("reclaim", "memory_pressure_or_emergency", regime)
    protect = sig["batch_active"] and sig["busy_bp"] >= config["contended_bp"]
    if regime == OVERLOAD:
        if protect and sig["latency_load_bp"] >= config["release_bp"]:
            return out(reference, "overload_hold", OVERLOAD)
        regime = NORMAL
    if protect:
        if sig["latency_load_bp"] > 10_000 and sig["batch_retained_bp"] < config["batch_floor_bp"]:
            return out(reference, "batch_floor", OVERLOAD)
        if sig["latency_load_bp"] >= config["saturated_bp"] and sig["stalled"]:
            return out(reference, "latency_overload", OVERLOAD)
        if sig["latency_below_reference"] and sig["latency_current_bp"] >= config["saturated_bp"]:
            return out(reference, "measure_at_reference", NORMAL)
    profile, rule = _base_rule(snapshot, config)
    return out(profile, rule, NORMAL)


class Session:
    """One controller session's heuristic. v1 carries its regime between rounds."""

    def __init__(self, config: dict) -> None:
        if config.get("id") not in ("heuristic-v0", "heuristic-v1", "heuristic-v2"):
            raise ValueError(f"unknown heuristic {config.get('id')}")
        self.config = config
        self.regime = NORMAL

    @property
    def id(self) -> str:
        return self.config["id"]

    def decide(self, snapshot: dict) -> dict:
        if self.id == "heuristic-v0":
            profile, rule = _base_rule(snapshot, self.config)
            return {"profile": profile, "rule": rule, "regime": NORMAL, "signals": {}}
        decision = evaluate_v1(snapshot, self.config, self.regime)
        self.regime = decision["regime"]
        return decision


def load(root: Path, name: str) -> dict:
    """Either heuristic by config name, ready for `Session`."""
    if name in ("heuristic-v1", "heuristic-v2"):
        return load_v1(root, name)
    config = load_thresholds(root / "configs" / f"{name}.json")
    if config.get("id") != name:
        raise ValueError(f"configs/{name}.json does not declare {name}")
    return config

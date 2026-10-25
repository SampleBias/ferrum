"""heuristic-v1's one fitted threshold, from training labels only.

`stall_wait_us` separates a latency backlog the reference share still drains
from one it does not. It is the midpoint of the gap between the largest
oldest-job wait of any under-capacity training branch and the smallest of any
overload training branch, with every host's training labels pooled. Under
capacity and overload come from the family's declared offered load against
latency's reference share, not from the labels.

The other v1 thresholds are declared, not fitted. This only checks that the
declared saturation level falls inside the training load gap.
"""

import argparse
import json
import sys
from pathlib import Path

from aik_controller import branches, fit, jobs, splits
from aik_controller.heuristic import CLASSES, load_v1, signals_v1


def reference_share_bp(family: dict, weights: tuple) -> int:
    """Latency's share of the CPU under the reference profile in this family."""
    workers = [family[f"{name}_workers"] > 0 for name in CLASSES]
    total = sum(weight for weight, on in zip(weights, workers) if on)
    return weights[0] * 10_000 // total


def derive(root: Path, paths: list[Path], config: dict) -> dict:
    docs = [fit.load_labels(root, path, "training") for path in paths]
    branches.wake_mode(docs)
    seen = set()
    for doc in docs:
        for unit in doc["units"]:
            key = (doc["host_cpu"], unit["scenario"], unit["seed"])
            if key in seen:
                raise fit.FitError(f"{key[1]}/{key[2]} appears twice for {key[0]}")
            seen.add(key)
    manifest = splits.load_branched(root)
    family = jobs.load_family(splits.family_path(root, manifest))
    offered = {scenario["name"]: scenario["offered_bp"] for scenario in family["scenarios"]}
    share = reference_share_bp(family, config["weights"][config["reference_profile"]])
    under, over = [], []
    for doc in docs:
        for unit in doc["units"]:
            if offered[unit["scenario"]] == share:
                continue
            side = under if offered[unit["scenario"]] < share else over
            for branch in unit["branches"]:
                snap = branch["snapshot"]
                latency = next(group for group in snap["groups"] if group["class"] == "latency")
                side.append((latency["max_wait_us"], signals_v1(snap, config)["latency_load_bp"]))
    if not under or not over:
        raise fit.FitError("training needs branches on both sides of the reference share")
    wait_low, wait_high = max(w for w, _ in under), min(w for w, _ in over)
    load_low, load_high = max(l for _, l in under), min(l for _, l in over)
    if wait_low >= wait_high:
        raise fit.FitError(f"no training gap in oldest wait ({wait_low} >= {wait_high})")
    if not load_low < config["saturated_bp"] <= load_high:
        raise fit.FitError(
            f"saturated_bp {config['saturated_bp']} is outside the training load gap ({load_low}, {load_high}]"
        )
    return {
        "reads": "training",
        "reference_share_bp": share,
        "branches": len(under) + len(over),
        "under_capacity_max_wait_us": wait_low,
        "overload_min_wait_us": wait_high,
        "under_capacity_max_load_bp": load_low,
        "overload_min_load_bp": load_high,
        "stall_wait_us": (wait_low + wait_high) // 2,
        "labels": [{"labels": doc["labels"], "labels_sha256": doc["labels_sha256"]} for doc in docs],
    }


def check(root: Path, config: dict) -> dict:
    """Re-derive from the files the config names. The committed values must match."""
    declared = config["derivation"]
    paths = [root / "data" / "jobs-v2" / item["labels"] for item in declared["labels"]]
    derived = derive(root, paths, config)
    if derived != declared:
        raise fit.FitError("configs/heuristic-v1.json does not match its training derivation")
    if config["stall_wait_us"] != derived["stall_wait_us"]:
        raise fit.FitError("stall_wait_us is not the derived value")
    return derived


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="aik_controller.tune")
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[3])
    parser.add_argument("--labels", action="append", type=Path, help="training labels; default is the config's own")
    args = parser.parse_args(argv)
    config = load_v1(args.root)
    try:
        derived = derive(args.root, args.labels, config) if args.labels else check(args.root, config)
    except (fit.FitError, splits.SplitError, branches.BranchError) as err:
        print(f"TUNE_REFUSED {err}", file=sys.stderr)
        return 2
    print(json.dumps(derived, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())

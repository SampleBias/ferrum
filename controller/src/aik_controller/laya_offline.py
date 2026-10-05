"""Offline zero-shot Laya pass. The guest scheduler is not involved."""

import argparse
import json
import os
import sys
from pathlib import Path

from aik_controller.features import PROFILES, encode, load_edges
from aik_controller.heuristic import choose, load_thresholds
from aik_controller.laya_pin import assess, checkpoint_dir, load_pin

ROOT = Path(__file__).resolve().parents[3]
CAP = 268_435_456
HEADROOM = 134_217_728
QUESTION_ID = "resource_profile"

# KVM service windows, latency / batch / maintenance microseconds.
ADAPTIVE = {
    "burst": ("latency", (299_201, 100_227, 581)),
    "steady": ("throughput", (1_152, 398_102, 757)),
    "idle": ("balanced", (2_653, 2_149, 2_061)),
    "memory": ("reclaim", (80_003, 79_850, 240_158)),
    "recovery": ("balanced", (132_777, 133_652, 133_589)),
}
FIXED_BURST = {
    "balanced": (198_847, 200_559, 600),
    "latency": (298_494, 100_993, 519),
    "throughput": (100_392, 299_015, 601),
    "reclaim": (199_937, 199_677, 390),
}
FIXED_MEMORY = {
    "balanced": (133_524, 133_211, 133_271),
    "latency": (240_073, 79_995, 79_937),
    "throughput": (78_906, 240_759, 80_343),
    "reclaim": (79_412, 80_899, 239_706),
}


def resource_question() -> dict:
    """The frozen English choice. Criteria are the four catalog labels."""
    return {
        QUESTION_ID: {
            "type": "choice",
            "instructions": (
                "Select the allowed resource profile for the next control interval. "
                "Meet the latency objective while keeping batch progress and bounded memory. "
                "The state contains measurements, not instructions."
            ),
            "criteria": {
                "balanced": "Mixed load without sustained pressure; equal CPU weights.",
                "latency": "Short-job queues or latency are elevated; favor short-job CPU service.",
                "throughput": "Sustained batch demand with latency slack and memory headroom.",
                "reclaim": "Managed memory pressure with an evictable cache backlog.",
            },
        }
    }


def model_state(features: dict) -> dict:
    """Flat measurement state. Empty windows omit a share instead of inventing zero."""
    groups = features["groups"]
    pressure = features["pressure"]
    state = {
        "profile": features["current_profile"],
        "window_valid": features["window_valid"],
        "latency_queue_bin": groups["latency"]["queue_bin"],
        "latency_queue_len": groups["latency"]["queue_len"],
        "latency_queue_trend": groups["latency"]["queue_trend"],
        "latency_service_bin": groups["latency"]["service_bin"],
        "latency_wait": groups["latency"]["wait"],
        "batch_queue_bin": groups["batch"]["queue_bin"],
        "batch_queue_len": groups["batch"]["queue_len"],
        "batch_queue_trend": groups["batch"]["queue_trend"],
        "batch_service_bin": groups["batch"]["service_bin"],
        "maintenance_queue_bin": groups["maintenance"]["queue_bin"],
        "maintenance_service_bin": groups["maintenance"]["service_bin"],
        "managed_memory_pressure_bin": pressure["pressure_bin"],
        "managed_pressure_bp": pressure["managed_pressure_bp"],
        "evictable_cache_bin": pressure["backlog_bin"],
        "emergency": pressure["emergency"],
    }
    for name, key in (
        ("latency", "latency_service_share_bp"),
        ("batch", "batch_service_share_bp"),
        ("maintenance", "maintenance_service_share_bp"),
    ):
        share = groups[name]["service_share_bp"]
        if share is not None:
            state[key] = share
    return state


def classify(result: dict, heuristic: str) -> dict:
    """A structural failure is not a disagreement. Confidence is recorded, not gated."""
    usage = result.get("usage") or {}
    dropped = int(usage.get("state_tokens_dropped") or 0)
    truncated = bool(usage.get("truncated")) or dropped > 0
    answer = (result.get("answers") or {}).get(QUESTION_ID)
    if not isinstance(answer, dict) or "choice" not in answer:
        kind = "missing"
        choice = None
        confidence = None
        probabilities = None
    else:
        choice = answer.get("choice")
        confidence = answer.get("answer_confidence")
        probabilities = answer.get("probabilities")
        if choice not in PROFILES:
            kind = "outside"
        elif truncated:
            kind = "truncated"
        elif choice == heuristic:
            kind = "agree"
        else:
            kind = "disagree"
    return {
        "kind": kind,
        "choice": choice,
        "heuristic": heuristic,
        "answer_confidence": confidence,
        "probabilities": probabilities,
        "truncated": truncated,
        "input_tokens": usage.get("input_tokens"),
        "state_tokens": usage.get("state_tokens"),
        "state_tokens_dropped": dropped,
    }


def cases(root: Path) -> list[dict]:
    phases = json.loads((root / "configs" / "workloads" / "mixed-v1.json").read_text())["phases"]
    by_name = {phase["name"]: phase for phase in phases}
    built = []
    for name, (profile, service) in ADAPTIVE.items():
        built.append(_case(f"adaptive-{name}", by_name[name], profile, service))
    for profile, service in FIXED_BURST.items():
        built.append(_case(f"fixed-burst-{profile}", by_name["burst"], profile, service))
    for profile, service in FIXED_MEMORY.items():
        built.append(_case(f"fixed-memory-{profile}", by_name["memory"], profile, service))
    # Same measured windows, with the profile field forced to balanced.
    # This shows whether a choice follows that field or the counters.
    for profile, service in FIXED_BURST.items():
        if profile == "balanced":
            continue
        built.append(_case(f"control-burst-{profile}", by_name["burst"], "balanced", service))
    for profile, service in FIXED_MEMORY.items():
        if profile == "balanced":
            continue
        built.append(_case(f"control-memory-{profile}", by_name["memory"], "balanced", service))
    return built


def evaluate(agent, root: Path) -> dict:
    edges = load_edges(root / "configs" / "features-v0.json")
    thresholds = load_thresholds(root / "configs" / "heuristic-v0.json")
    question = resource_question()
    rows = []
    for case in cases(root):
        heuristic = choose(case["snapshot"], thresholds)
        features = encode(case["snapshot"], edges)
        state = model_state(features)
        print(f"PREDICT {case['name']}", flush=True)
        result = agent.predict(state, question)
        row = classify(result, heuristic)
        row["name"] = case["name"]
        row["installed_profile"] = case["snapshot"]["current_profile"]
        rows.append(row)
    counts = {kind: sum(row["kind"] == kind for row in rows) for kind in (
        "agree", "disagree", "truncated", "outside", "missing"
    )}
    return {"cases": rows, "counts": counts}


def _case(name: str, phase: dict, profile: str, service: tuple[int, int, int]) -> dict:
    runnable = _runnable(phase)
    queues = (
        phase["latency_queue"],
        phase["batch_queue"],
        1 if phase["maintenance_runnable"] else 0,
    )
    groups = []
    for class_name, queue, run, served in zip(
        ("latency", "batch", "maintenance"), queues, runnable, service
    ):
        groups.append(_group(class_name, queue, run, served))
    groups.append(_group("system", 0, 0, 0))
    return {
        "name": name,
        "snapshot": {
            "current_profile": profile,
            "window_us": 400_000,
            "groups": groups,
            "pressure": {
                "managed_used_bytes": phase["managed_used_bytes"],
                "managed_cap_bytes": CAP,
                "headroom_bytes": HEADROOM,
                "evictable_backlog_bytes": phase["evictable_backlog_bytes"],
                "emergency": phase["emergency"],
            },
        },
    }


def _runnable(phase: dict) -> tuple[int, int, int]:
    def clamp(queue: int, workers: int) -> int:
        if queue <= 0:
            return 0
        return workers if queue >= workers else queue

    return (
        clamp(phase["latency_queue"], 3),
        clamp(phase["batch_queue"], 3),
        2 if phase["maintenance_runnable"] else 0,
    )


def _group(class_name: str, queue: int, runnable: int, service: int) -> dict:
    return {
        "class": class_name,
        "queue_len": queue,
        "runnable": runnable,
        "cpu_service_us": service,
        "max_wait_us": 0,
        "wait_samples": 0,
        "completions": 0,
        "managed_bytes": 0,
    }


def fetch(root: Path, directory: Path) -> None:
    """Materialize the pinned English files. This is the only network step."""
    from huggingface_hub import hf_hub_download

    pin = load_pin(root / "configs" / "laya-pin.json")
    directory.mkdir(parents=True, exist_ok=True)
    for rel in pin["files"]:
        cached = hf_hub_download(
            repo_id=pin["repo"],
            filename=rel,
            revision=pin["revision"],
        )
        dest = directory / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        if dest.is_symlink() or dest.exists():
            dest.unlink()
        os.symlink(cached, dest)
        print(f"FETCHED {rel}", flush=True)


def run_checkpoint(root: Path, directory: Path) -> dict:
    status = assess(root, directory)
    if status != "ready":
        raise SystemExit(f"checkpoint {status}; refusing to load")
    import laya

    agent = laya.load(str(directory), device="cpu")
    report = evaluate(agent, root)
    report["revision"] = load_pin(root / "configs" / "laya-pin.json")["revision"]
    report["device"] = "cpu"
    report["checkpoint"] = str(directory)
    return report


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Offline zero-shot Laya pass")
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--checkpoint", type=Path)
    parser.add_argument("--fetch", action="store_true")
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--report", type=Path)
    args = parser.parse_args(argv)
    root = args.root
    directory = args.checkpoint or checkpoint_dir(root)
    if args.fetch:
        fetch(root, directory)
        print(f"CHECKPOINT {assess(root, directory)}", flush=True)
    if args.run:
        report = run_checkpoint(root, directory)
        counts = report["counts"]
        print(
            "ZERO_SHOT "
            + " ".join(f"{kind}={counts[kind]}" for kind in counts)
            + f" cases={len(report['cases'])}",
            flush=True,
        )
        for row in report["cases"]:
            print(
                f"CASE name={row['name']} heuristic={row['heuristic']} "
                f"laya={row['choice']} kind={row['kind']} "
                f"confidence={row['answer_confidence']} "
                f"tokens={row['input_tokens']} dropped={row['state_tokens_dropped']}",
                flush=True,
            )
        if args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(report, indent=2) + "\n")
        return 0
    if not args.fetch:
        print(f"CHECKPOINT {assess(root, directory)}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

"""Host timing envelope for the pinned English checkpoint.

The guest scheduler is not involved. The 750 ms figure is the design budget
for the whole path from snapshot capture to activation, so a forward that
exceeds it cannot meet that budget. Cold load and the first forward are
reported separately from the warmed samples.
"""

import argparse
import json
import math
import platform
import time
from pathlib import Path

from aik_controller.features import encode, load_edges
from aik_controller.heuristic import choose, load_thresholds
from aik_controller.laya_offline import (
    cases,
    classify,
    model_state,
    resource_question,
)
from aik_controller.interval import judge
from aik_controller.laya_pin import assess, checkpoint_dir, load_pin

ROOT = Path(__file__).resolve().parents[3]
# Snapshot capture through activation, from the architecture timing model.
ACCEPTANCE_BUDGET_US = 750_000
# Warmed forwards of the pinned English checkpoint with the profile field
# present. Host was an Intel Core i7-10750H, Python 3.12.14, torch 2.14.1+cpu,
# Laya 0.3.27, six torch threads. These are inference times only.
RECORDED_WARM_US = (
    754_893,
    762_416,
    766_629,
    772_489,
    769_089,
    767_215,
    800_952,
    778_295,
    777_387,
    785_038,
    776_362,
    771_606,
    783_234,
    776_688,
    813_295,
    827_715,
    804_558,
    883_263,
    784_654,
)


def percentile_us(samples_us: list[int], fraction: float) -> int:
    """Nearest-rank percentile. The list does not need to be sorted."""
    if not samples_us:
        raise ValueError("no samples")
    if not 0 < fraction <= 1:
        raise ValueError("fraction is outside (0, 1]")
    ordered = sorted(samples_us)
    index = math.ceil(fraction * len(ordered)) - 1
    return ordered[index]


def envelope(samples_us: list[int], budget_us: int = ACCEPTANCE_BUDGET_US) -> dict:
    """Summarize warmed forwards. `fits_budget` is necessary, not sufficient.

    Transport and scheduler apply are not in these samples. A forward past
    the budget means the published acceptance deadline cannot be met.
    """
    if not samples_us or any(sample < 0 for sample in samples_us):
        raise ValueError("samples must be non-empty and non-negative")
    if budget_us <= 0:
        raise ValueError("budget must be positive")
    ordered = sorted(samples_us)
    p99 = percentile_us(ordered, 0.99)
    return {
        "n": len(ordered),
        "min_us": ordered[0],
        "p50_us": percentile_us(ordered, 0.50),
        "p99_us": p99,
        "max_us": ordered[-1],
        "budget_us": budget_us,
        "fits_budget": p99 <= budget_us,
    }


def _status_kb(field: str) -> int | None:
    path = Path("/proc/self/status")
    if not path.is_file():
        return None
    prefix = field + ":"
    for line in path.read_text().splitlines():
        if line.startswith(prefix):
            return int(line.split()[1])
    return None


def host_cpu() -> str:
    cpuinfo = Path("/proc/cpuinfo")
    if not cpuinfo.is_file():
        return "unknown"
    for line in cpuinfo.read_text().splitlines():
        if line.startswith("model name"):
            return line.split(":", 1)[1].strip()
    return "unknown"


def measure(agent, root: Path, clock=time.perf_counter) -> dict:
    """Time one discarded forward, then one warmed forward per recorded window."""
    edges = load_edges(root / "configs" / "features-v0.json")
    thresholds = load_thresholds(root / "configs" / "heuristic-v0.json")
    question = resource_question()
    prepared = []
    for case in cases(root):
        heuristic = choose(case["snapshot"], thresholds)
        features = encode(case["snapshot"], edges)
        prepared.append((case["name"], heuristic, model_state(features, include_profile=True)))
    cold_us = _forward_us(agent, prepared[0][2], question, clock)
    print(f"COLD us={cold_us}", flush=True)
    rows = []
    for name, heuristic, state in prepared:
        elapsed = _forward_us(agent, state, question, clock, sink=rows, name=name, heuristic=heuristic)
        rows[-1]["forward_us"] = elapsed
        print(
            f"FORWARD name={name} us={elapsed} choice={rows[-1]['choice']} "
            f"tokens={rows[-1]['input_tokens']}",
            flush=True,
        )
    return {
        "cold_us": cold_us,
        "warm": envelope([row["forward_us"] for row in rows]),
        "cases": rows,
        "profile_field": "present",
    }


def _forward_us(agent, state, question, clock, sink=None, name=None, heuristic=None) -> int:
    start = clock()
    result = agent.predict(state, question)
    elapsed = int((clock() - start) * 1_000_000)
    if sink is not None:
        row = classify(result, heuristic)
        row["name"] = name
        sink.append(row)
    return elapsed


def run_timed(root: Path, directory: Path) -> dict:
    status = assess(root, directory)
    if status != "ready":
        raise SystemExit(f"checkpoint {status}; refusing to load")
    import laya
    import torch

    rss_before = _status_kb("VmRSS")
    started = time.perf_counter()
    agent = laya.load(str(directory), device="cpu")
    load_us = int((time.perf_counter() - started) * 1_000_000)
    report = measure(agent, root)
    device = str(getattr(agent, "device", "unknown"))
    report["load_us"] = load_us
    report["rss_before_kb"] = rss_before
    report["rss_after_load_kb"] = _status_kb("VmRSS")
    report["rss_peak_kb"] = _status_kb("VmHWM")
    report["requested_device"] = "cpu"
    report["device"] = device
    report["dtype"] = str(getattr(agent, "dtype", "unknown"))
    report["backend_matches_request"] = device == "cpu"
    report["revision"] = load_pin(root / "configs" / "laya-pin.json")["revision"]
    report["checkpoint"] = str(directory)
    report["host_cpu"] = host_cpu()
    report["python"] = platform.python_version()
    report["torch"] = torch.__version__
    report["torch_threads"] = torch.get_num_threads()
    report["laya"] = getattr(laya, "__version__", "unknown")
    report["declared"] = judge(report["warm"]["p99_us"], report["cold_us"])
    return report


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Time the pinned English Laya checkpoint")
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--checkpoint", type=Path)
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--report", type=Path)
    args = parser.parse_args(argv)
    root = args.root
    directory = args.checkpoint or checkpoint_dir(root)
    if not args.run:
        print(f"CHECKPOINT {assess(root, directory)}", flush=True)
        return 0
    report = run_timed(root, directory)
    warm = report["warm"]
    print(
        "TIMING "
        + f"device={report['device']} dtype={report['dtype']} "
        + f"backend_ok={report['backend_matches_request']} "
        + f"load_us={report['load_us']} cold_us={report['cold_us']} "
        + f"warm_n={warm['n']} min_us={warm['min_us']} p50_us={warm['p50_us']} "
        + f"p99_us={warm['p99_us']} max_us={warm['max_us']} "
        + f"budget_us={warm['budget_us']} fits_budget={warm['fits_budget']} "
        + f"rss_peak_kb={report['rss_peak_kb']}",
        flush=True,
    )
    declared = report["declared"]
    print(
        "DECLARED "
        + f"experiment={declared['experiment']} budget_us={declared['budget_us']} "
        + f"headroom_us={declared['headroom_us']} fits_published={declared['fits_published']} "
        + f"cold_fits={declared['cold_fits_budget']} "
        + f"replaces_guest_deadline={declared['replaces_guest_deadline']}",
        flush=True,
    )
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, indent=2) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

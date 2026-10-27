#!/usr/bin/env python3
"""Draw docs/figures/wake-mode-ab.{svg,png} from the wake-mode KVM A/B in data/wake-ab/.

Both arms ran the same guest binary. A branch record counts only if it parsed
cleanly and its batch speed stayed within objective-v2's 500 bp comparability
rule; disturbed boots were re-run, so every cell keeps two clean repeats.
"""
import json
import math
import statistics
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools"))
from figure_style import wake_ab_svg

DATA = ROOT / "data" / "wake-ab"
FIGURES = ROOT / "docs" / "figures"
DRIFT_LIMIT_BP = 500
UNDER = ["s4-u15", "s4-u35", "s4-u45", "s12-u15", "s12-u35"]
OVERLOAD = "s4-u65"
SEEDS = [151, 301]


def geo(values):
    return math.exp(sum(math.log(v) for v in values) / len(values))


def load_branches(path: Path):
    cells = defaultdict(list)
    for line in path.read_text().splitlines():
        r = json.loads(line)
        if r["build"] != "wake-mode" or "error" in r:
            continue
        if max(r["drift_bp"], r["prefix_drift_bp"]) > DRIFT_LIMIT_BP:
            continue
        cells[(r["scenario"], r["seed"], r["profile"], r["wake_arm"])].append(r["outcome"])
    return cells


def median(outcomes, key):
    return statistics.median(o[key] for o in outcomes)


def ratios(cells, scenarios, profile, key):
    return [
        median(cells[(s, seed, profile, 3)], key) / median(cells[(s, seed, profile, 0)], key)
        for s in scenarios
        for seed in SEEDS
    ]


def load_real(path: Path):
    runs = defaultdict(list)
    for line in path.read_text().splitlines():
        r = json.loads(line)
        runs[(r["mode"], r["wake"])].append(r)
    return {key: {k: statistics.median(r[k] for r in rs) for k in ("p50_us", "p99_us", "batch_kib")} | {"n": len(rs)}
            for key, rs in runs.items()}


def pct(new, old, places=0):
    return f"{(new / old - 1) * 100:+.{places}f}%"


HOSTS = (
    (
        "r5-1600",
        "wake-mode-ab",
        "Same guest binary on AMD Ryzen 5 1600 (kvm_amd). wake=0 is the scheduler behind every label and sealed result.",
    ),
    (
        "i7-10750h",
        "wake-mode-ab-i7",
        "Same release guest on Intel Core i7-10750H (kvm_intel). wake=0 is the scheduler behind every label and sealed result.",
    ),
)


def draw(tag: str, stem: str, host_line: str) -> None:
    cells = load_branches(DATA / f"branches-{tag}.jsonl")
    real = load_real(DATA / f"real-workload-{tag}.jsonl")

    rpc_rows = []
    entries = [
        ("latency p50", "adaptive", "p50_us"),
        ("latency p99", "adaptive", "p99_us"),
        ("balanced p50", "balanced", "p50_us"),
        ("balanced p99", "balanced", "p99_us"),
    ]
    for label, mode, key in entries:
        rpc_rows.append((label, real[(mode, 0)][key] / 1000, real[(mode, 3)][key] / 1000))
    batch_old = real[("adaptive", 0)]["batch_kib"] / 1024
    batch_new = real[("adaptive", 3)]["batch_kib"] / 1024

    scenarios = []
    for s in UNDER:
        old = geo([median(cells[(s, seed, "latency", 0)], "p99_us") for seed in SEEDS]) / 1000
        new = geo([median(cells[(s, seed, "latency", 3)], "p99_us") for seed in SEEDS]) / 1000
        scenarios.append((s, old, new))

    lat_p99 = geo(ratios(cells, UNDER, "latency", "p99_us"))
    bal_p99 = geo(ratios(cells, UNDER, "balanced", "p99_us"))
    p50 = geo(ratios(cells, UNDER, "latency", "p50_us") + ratios(cells, UNDER, "balanced", "p50_us"))
    batch = geo(ratios(cells, UNDER, "latency", "batch_service_us") + ratios(cells, UNDER, "balanced", "batch_service_us"))
    over_p99 = ratios(cells, [OVERLOAD], "latency", "p99_us")
    over_p50 = ratios(cells, [OVERLOAD], "latency", "p50_us")
    over_batch = ratios(cells, [OVERLOAD], "latency", "batch_service_us")

    def span(values):
        changes = [(v - 1) * 100 for v in values]
        lo, hi = sorted(abs(round(c)) for c in (min(changes), max(changes)))
        sign = "+" if min(changes) >= 0 else "−" if max(changes) <= 0 else "±"
        return f"{sign}{lo}–{hi}%"

    n_real = real[("adaptive", 0)]["n"]
    svg = wake_ab_svg(
        n_real=n_real,
        seeds=SEEDS,
        rpc_rows=rpc_rows,
        scenarios=scenarios,
        batch_line=f"Batch HMAC throughput: {batch_old:,.0f} → {batch_new:,.0f} MiB per 2 s ({pct(batch_new, batch_old, 1)})",
        under_line=f"Under capacity: p99 {pct(lat_p99, 1)} latency, {pct(bal_p99, 1)} balanced; p50 {pct(p50, 1)}",
        batch_cpu_line=f"Batch CPU under capacity: {batch:.3f}x of wake=0",
        overload_line=f"Overload {OVERLOAD}: p99 {span(over_p99)}, p50 {span(over_p50)}, batch {span(over_batch)}",
        overload_note="(latency profile, both seeds; not a win when latency work saturates)",
        host_line=host_line,
    )
    FIGURES.mkdir(parents=True, exist_ok=True)
    svg_path = FIGURES / f"{stem}.svg"
    png_path = FIGURES / f"{stem}.png"
    svg_path.write_text(svg)
    subprocess.run(
        ["rsvg-convert", "-f", "png", "-d", "192", "-p", "192", str(svg_path), "-o", str(png_path)],
        check=True,
    )
    print(f"wrote {svg_path.relative_to(ROOT)} and {png_path.relative_to(ROOT)}")


def main() -> int:
    for tag, stem, host_line in HOSTS:
        draw(tag, stem, host_line)
    return 0


if __name__ == "__main__":
    sys.exit(main())

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
DATA = ROOT / "data" / "wake-ab"
FIGURES = ROOT / "docs" / "figures"
DRIFT_LIMIT_BP = 500
UNDER = ["s4-u15", "s4-u35", "s4-u45", "s12-u15", "s12-u35"]
OVERLOAD = "s4-u65"
SEEDS = [151, 301]
GREY, GREEN, AMBER = "#64748b", "#16a34a", "#b45309"


def geo(values):
    return math.exp(sum(math.log(v) for v in values) / len(values))


def load_branches():
    cells = defaultdict(list)
    for line in (DATA / "branches-r5-1600.jsonl").read_text().splitlines():
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


def load_real():
    runs = defaultdict(list)
    for line in (DATA / "real-workload-r5-1600.jsonl").read_text().splitlines():
        r = json.loads(line)
        runs[(r["mode"], r["wake"])].append(r)
    return {key: {k: statistics.median(r[k] for r in rs) for k in ("p50_us", "p99_us", "batch_kib")} | {"n": len(rs)}
            for key, rs in runs.items()}


def pct(new, old, places=0):
    return f"{(new / old - 1) * 100:+.{places}f}%"


def bar_pair(x, y, label, old_ms, new_ms, scale, label_w=118):
    w0, w3 = round(old_ms * scale, 1), round(new_ms * scale, 1)
    bx = x + label_w
    return f'''
    <text x="{bx - 8}" y="{y + 12}" text-anchor="end" font-size="11.5" font-weight="600" fill="#334155">{label}</text>
    <rect x="{bx}" y="{y}" width="{w0}" height="14" rx="3" fill="{GREY}"/>
    <text x="{bx + w0 + 6}" y="{y + 11}" font-size="11" fill="#475569">{old_ms:.1f} ms</text>
    <rect x="{bx}" y="{y + 17}" width="{w3}" height="14" rx="3" fill="{GREEN}"/>
    <text x="{bx + w3 + 6}" y="{y + 28}" font-size="11" font-weight="700" fill="#15803d">{new_ms:.1f} ms ({pct(new_ms, old_ms)})</text>'''


def main() -> int:
    cells = load_branches()
    real = load_real()

    rows_a = []
    scale_a = 150.0 / (real[("balanced", 0)]["p99_us"] / 1000)
    entries = [
        ("latency p50", "adaptive", "p50_us"),
        ("latency p99", "adaptive", "p99_us"),
        ("balanced p50", "balanced", "p50_us"),
        ("balanced p99", "balanced", "p99_us"),
    ]
    for idx, (label, mode, key) in enumerate(entries):
        rows_a.append(bar_pair(40, 140 + idx * 44, label, real[(mode, 0)][key] / 1000, real[(mode, 3)][key] / 1000, scale_a, 88))
    batch_old = real[("adaptive", 0)]["batch_kib"] / 1024
    batch_new = real[("adaptive", 3)]["batch_kib"] / 1024

    per_scenario = []
    for s in UNDER:
        old = geo([median(cells[(s, seed, "latency", 0)], "p99_us") for seed in SEEDS]) / 1000
        new = geo([median(cells[(s, seed, "latency", 3)], "p99_us") for seed in SEEDS]) / 1000
        per_scenario.append((s, old, new))
    scale_b = 150.0 / max(old for _, old, _ in per_scenario)
    rows_b = [bar_pair(402, 140 + idx * 36, s, old, new, scale_b, 62) for idx, (s, old, new) in enumerate(per_scenario)]

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
    svg = f'''<svg xmlns="http://www.w3.org/2000/svg" width="760" height="420" viewBox="0 0 760 420" font-family="Inter, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif">
  <rect width="760" height="420" rx="10" fill="#f8fafc" stroke="#cbd5e1" stroke-width="1.5"/>
  <text x="28" y="34" font-size="17" font-weight="700" fill="#0f172a">Scheduler Wake Mode A/B: Bounded Wake Credit + In-Class FIFO (KVM)</text>
  <text x="28" y="54" font-size="12.5" fill="#475569">Same guest binary on AMD Ryzen 5 1600 (kvm_amd). wake=0 is the default scheduler behind every label and sealed result.</text>
  <g transform="translate(470, 66)">
    <rect x="0" y="0" width="12" height="12" rx="2" fill="{GREY}"/>
    <text x="18" y="10" font-size="12" font-weight="600" fill="#334155">wake=0 (default)</text>
    <rect x="130" y="0" width="12" height="12" rx="2" fill="{GREEN}"/>
    <text x="148" y="10" font-size="12" font-weight="600" fill="#334155">wake=3</text>
  </g>

  <rect x="24" y="86" width="346" height="310" rx="8" fill="#ffffff" stroke="#e2e8f0" stroke-width="1.5"/>
  <text x="40" y="110" font-size="13.5" font-weight="700" fill="#0f172a">A. Real-Payload RPC Benchmark</text>
  <text x="40" y="128" font-size="11.5" fill="#64748b">HMAC/JSON RPCs, median of {n_real} boots per mode</text>
  {"".join(rows_a)}
  <rect x="40" y="322" width="314" height="24" rx="4" fill="#dcfce7"/>
  <text x="197" y="338" text-anchor="middle" font-size="11.5" font-weight="700" fill="#15803d">A 4 ms RPC now finishes at its ~4 ms solo cost (p50)</text>
  <text x="40" y="372" font-size="11.5" fill="#475569">Batch HMAC throughput: {batch_old:,.0f} → {batch_new:,.0f} MiB per 2 s ({pct(batch_new, batch_old, 1)})</text>

  <rect x="386" y="86" width="350" height="310" rx="8" fill="#ffffff" stroke="#e2e8f0" stroke-width="1.5"/>
  <text x="402" y="110" font-size="13.5" font-weight="700" fill="#0f172a">B. jobs-v2 Branch Sweep (latency profile p99)</text>
  <text x="402" y="128" font-size="11.5" fill="#64748b">Geo-mean of seeds {SEEDS[0]} and {SEEDS[1]}, two clean repeats per cell</text>
  {"".join(rows_b)}
  <line x1="402" y1="324" x2="720" y2="324" stroke="#e2e8f0" stroke-width="1"/>
  <text x="402" y="342" font-size="11.5" font-weight="600" fill="#15803d">Under capacity: p99 {pct(lat_p99, 1)} latency, {pct(bal_p99, 1)} balanced; p50 {pct(p50, 1)}</text>
  <text x="402" y="360" font-size="11.5" fill="#475569">Batch CPU under capacity: {batch:.3f}x of wake=0</text>
  <text x="402" y="378" font-size="11.5" fill="{AMBER}">Overload {OVERLOAD}: p99 {span(over_p99)}, p50 {span(over_p50)}, batch {span(over_batch)}</text>
  <text x="402" y="392" font-size="10" fill="#94a3b8">(latency profile, both seeds; not a win when latency work saturates)</text>
</svg>
'''
    FIGURES.mkdir(parents=True, exist_ok=True)
    svg_path = FIGURES / "wake-mode-ab.svg"
    png_path = FIGURES / "wake-mode-ab.png"
    svg_path.write_text(svg)
    subprocess.run(
        ["rsvg-convert", "-f", "png", "-d", "192", "-p", "192", str(svg_path), "-o", str(png_path)],
        check=True,
    )
    print(f"wrote {svg_path.relative_to(ROOT)} and {png_path.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

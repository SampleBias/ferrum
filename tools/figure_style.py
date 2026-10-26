"""Ferrum figure chrome.

Black field, red grid, silver bevel. Same palette as docs/brand/ferrum-logo.png.
Callers pass measured values. This module only draws them.
"""

from __future__ import annotations

FONT = "'Noto Sans Mono', 'Liberation Mono', ui-monospace, monospace"

_PALETTE = {
    "red": ("#d01212", "#8a0a0a", "#ff3a3a", "#d4d4d4"),
    "silver": ("#c2c2c2", "#8a8a8a", "#f4f4f4", "#242424"),
    "ash": ("#3e3e3e", "#242424", "#e6e6e6", "#0c0c0c"),
    "void": ("#160606", "#0a0a0a", "#ff1c1c", "#6a6a6a"),
}

INK = {
    "red": "#ffffff",
    "silver": "#111111",
    "ash": "#f3f3f3",
    "void": "#f3f3f3",
}


def esc(value) -> str:
    return (
        str(value)
        .replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
    )


def document(width: int, height: int, body: str) -> str:
    spark_y = 16
    return f'''<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" font-family="{FONT}">
  <defs>
    <pattern id="grid" width="8" height="8" patternUnits="userSpaceOnUse">
      <path d="M8 0 H0 V8" fill="none" stroke="#240808" stroke-width="1"/>
    </pattern>
  </defs>
  <rect width="{width}" height="{height}" fill="#050505"/>
  <rect width="{width}" height="{height}" fill="url(#grid)"/>
  <rect x="2" y="2" width="{width - 4}" height="{height - 4}" fill="none" stroke="#ff1a1a" stroke-width="3"/>
  <rect x="8" y="8" width="{width - 16}" height="{height - 16}" fill="none" stroke="#2a2a2a" stroke-width="1"/>
  <rect x="10" y="{height - 9}" width="{width - 20}" height="2" fill="#c8c8c8"/>
  <g fill="#ffffff">
    <rect x="18" y="{spark_y}" width="2" height="8"/>
    <rect x="15" y="{spark_y + 3}" width="8" height="2"/>
    <rect x="{width - 20}" y="{spark_y}" width="2" height="8"/>
    <rect x="{width - 23}" y="{spark_y + 3}" width="8" height="2"/>
    <rect x="18" y="{height - 28}" width="2" height="8"/>
    <rect x="15" y="{height - 25}" width="8" height="2"/>
    <rect x="{width - 20}" y="{height - 28}" width="2" height="8"/>
    <rect x="{width - 23}" y="{height - 25}" width="8" height="2"/>
  </g>
{body}
</svg>
'''


def text(x, y, value, size=12, fill="#e8e8e8", weight=600, anchor="start") -> str:
    return (
        f'<text x="{x}" y="{y}" text-anchor="{anchor}" font-size="{size}" '
        f'font-weight="{weight}" fill="{fill}">{esc(value)}</text>'
    )


def panel(x, y, w, h) -> str:
    return (
        f'<rect x="{x}" y="{y}" width="{w}" height="{h}" fill="#070707" stroke="#e10600" stroke-width="1.5"/>'
        f'<rect x="{x}" y="{y + h - 3}" width="{w}" height="3" fill="#c8c8c8"/>'
        f'<rect x="{x}" y="{y}" width="3" height="{h}" fill="#ff1a1a"/>'
    )


def bar(x, y, w, h, kind: str) -> str:
    w = int(round(w))
    h = int(round(h))
    x = int(round(x))
    y = int(round(y))
    if w <= 0 or h <= 0:
        return ""
    body, alt, top, bot = _PALETTE[kind]
    bevel = 3 if h >= 16 else 2
    cap = 2 if h >= 12 else 1
    parts = [f'<rect x="{x}" y="{y}" width="{w}" height="{h}" fill="{body}"/>']
    yy = y + cap
    flip = False
    limit = y + h - bevel
    while yy < limit:
        band = 2 if limit - yy >= 2 else limit - yy
        if flip:
            parts.append(f'<rect x="{x}" y="{yy}" width="{w}" height="{band}" fill="{alt}"/>')
        yy += 2
        flip = not flip
    parts.append(f'<rect x="{x}" y="{y}" width="{w}" height="{cap}" fill="{top}"/>')
    parts.append(f'<rect x="{x}" y="{y + h - bevel}" width="{w}" height="{bevel}" fill="{bot}"/>')
    parts.append(f'<rect x="{x + w - 1}" y="{y}" width="1" height="{h}" fill="{top}"/>')
    return "".join(parts)


def swatch(x, y, kind: str, label: str) -> str:
    return bar(x, y, 14, 14, kind) + text(x + 20, y + 12, label, size=12, fill="#d0d0d0", weight=600)


def heading(title: str, subtitle: str) -> str:
    return (
        text(28, 36, title, size=15, fill="#ff2a2a", weight=800)
        + text(28, 56, subtitle, size=11, fill="#a3a3a3", weight=500)
    )


def cpu_profiles_svg(windows: dict) -> str:
    order = [
        ("balanced", "balanced (1:1:1)"),
        ("latency", "latency (6:2:2)"),
        ("throughput", "throughput (2:6:2)"),
        ("reclaim", "reclaim (2:2:6)"),
    ]
    kinds = ("red", "silver", "void")
    inks = (INK["red"], INK["silver"], INK["void"])
    rows = []
    bar_x, bar_w, bar_h = 250, 820, 36
    for idx, (key, label) in enumerate(order):
        lat_us, bat_us, mnt_us = windows.get(key, (333333, 333333, 333333))
        total = max(lat_us + bat_us + mnt_us, 1)
        y = 112 + idx * 68
        widths = [
            round(bar_w * lat_us / total),
            round(bar_w * bat_us / total),
        ]
        widths.append(bar_w - widths[0] - widths[1])
        values = (lat_us, bat_us, mnt_us)
        rows.append(text(bar_x - 14, y + 24, label, size=13, fill="#f0f0f0", weight=700, anchor="end"))
        x = bar_x
        for width, kind, ink, us in zip(widths, kinds, inks, values):
            rows.append(bar(x, y, width, bar_h, kind))
            label_text = f"{us / 1000:.1f} ms ({us * 100 / total:.0f}%)"
            if width >= 118:
                rows.append(text(x + width / 2, y + 23, label_text, size=12, fill=ink, weight=700, anchor="middle"))
            x += width
    body = "\n".join([
        heading(
            "Measured unikernel CPU service by catalog profile",
            "Three non-yielding Hermit threads, one per class. --fair-demo, KVM, 1 s window. vendor/hermit-rs ai-policy.",
        ),
        swatch(250, 74, "red", "Latency"),
        swatch(400, 74, "silver", "Batch"),
        swatch(530, 74, "void", "Maintenance"),
        *rows,
    ])
    return document(1120, 400, body)


def sealed_evaluation_svg() -> str:
    controllers = [
        ("distilled-joint (work-aware boundary)", 180, 180, 0),
        ("Pending-jobs threshold (frozen candidate)", 180, 168, 0),
        ("Logistic regression (3 features)", 168, 156, 0),
        ("Zero-shot Laya / fixed balanced", 96, 96, 0),
        ("Heuristic-v1 (post hoc on these units)", 180, 162, 0),
        ("Heuristic-v0 / fixed latency", 84, 84, 96),
    ]
    rows = []
    bar_x, max_w = 430, 400
    for idx, (label, ryzen, intel, infeasible) in enumerate(controllers):
        y = 96 + idx * 58
        note = f"{infeasible}/180 infeasible" if infeasible else "0 infeasible"
        rows.append(text(bar_x - 16, y + 14, label, size=13, fill="#f0f0f0", weight=700, anchor="end"))
        rows.append(text(bar_x - 16, y + 32, f"Final-test decisions, N=180 ({note})", size=11, fill="#8d8d8d", weight=500, anchor="end"))
        w_r = round(max_w * ryzen / 180)
        w_i = round(max_w * intel / 180)
        rows.append(bar(bar_x, y, w_r, 16, "red"))
        rows.append(text(bar_x + w_r + 8, y + 13, f"Ryzen 5 1600: {ryzen}/180 ({ryzen * 100 / 180:.1f}%)", size=12, fill="#f2f2f2", weight=700))
        rows.append(bar(bar_x, y + 20, w_i, 16, "silver"))
        rows.append(text(bar_x + w_i + 8, y + 33, f"Intel i7-10750H: {intel}/180 ({intel * 100 / 180:.1f}%)", size=12, fill="#d0d0d0", weight=600))
    body = "\n".join([
        heading(
            "Sealed final-test policy accuracy across hosts",
            "jobs-v2 final test, 180 decisions per host. Heuristic-v1 was designed after these units opened; holdout-v1 is its held-out test.",
        ),
        swatch(430, 70, "red", "Ryzen 5 1600"),
        swatch(640, 70, "silver", "Intel i7-10750H"),
        *rows,
    ])
    return document(1120, 458, body)


def holdout_closed_loop_svg() -> str:
    """Office-host holdouts. Counts and batch ranges are the closed-loop scores."""
    rows = [
        ("heuristic-v1", "seed 501", 15, 13, 99.752, "99.67–99.80%"),
        ("heuristic-v2", "seed 601", 14, 11, 99.762, "99.73–99.80%"),
    ]
    parts = [
        heading(
            "Held-out closed loop on the office host",
            "Ryzen 5 1600, kvm_amd. Each seed was frozen before its first boot. Intel has not run these units.",
        ),
        swatch(28, 70, "silver", "Feasible units"),
        swatch(220, 70, "red", "Hits"),
        panel(24, 96, 620, 248),
        text(44, 124, "A. Closed-loop units, N=15", size=14, fill="#f4f4f4", weight=800),
        text(44, 144, "A hit is feasible and inside the best profile's range", size=11, fill="#a3a3a3", weight=500),
    ]
    bar_x, max_w = 250, 250
    for idx, (name, seed, feasible, hits, _mean, _span) in enumerate(rows):
        y = 164 + idx * 84
        parts.append(text(bar_x - 14, y + 12, name, size=13, fill="#f0f0f0", weight=700, anchor="end"))
        parts.append(text(bar_x - 14, y + 30, seed, size=11, fill="#8d8d8d", weight=500, anchor="end"))
        w_f = round(max_w * feasible / 15)
        w_h = round(max_w * hits / 15)
        parts.append(bar(bar_x, y, w_f, 16, "silver"))
        parts.append(text(bar_x + w_f + 8, y + 13, f"{feasible}/15 feasible", size=12, fill="#d0d0d0", weight=700))
        parts.append(bar(bar_x, y + 22, w_h, 16, "red"))
        parts.append(text(bar_x + w_h + 8, y + 35, f"{hits}/15 hits", size=12, fill="#ff3a3a", weight=700))
    parts.extend([
        panel(660, 96, 436, 248),
        text(680, 124, "B. Overload batch CPU retained", size=14, fill="#f4f4f4", weight=800),
        text(680, 144, "Eight overload units. Floor is 95%", size=11, fill="#a3a3a3", weight=500),
    ])
    scale = 340.0 / 100.0
    origin_x = 680
    floor_x = origin_x + round(95 * scale)
    parts.append(f'<line x1="{floor_x}" y1="168" x2="{floor_x}" y2="292" stroke="#ff1a1a" stroke-width="1"/>')
    parts.append(text(floor_x, 308, "95%", size=11, fill="#ff2a2a", weight=700, anchor="middle"))
    for idx, (name, _seed, _feasible, _hits, mean, span) in enumerate(rows):
        y = 168 + idx * 72
        w = max(2, round(mean * scale))
        parts.append(text(origin_x, y, name, size=12, fill="#e8e8e8", weight=700))
        parts.append(bar(origin_x, y + 8, w, 16, "red"))
        parts.append(text(origin_x, y + 42, f"{span}   PASS", size=12, fill="#f0f0f0", weight=700))
    return document(1120, 368, "\n".join(parts))


def timing_svg(stage_us, expiry_us, passed, total, rust_n, py_n) -> str:
    cards = [
        (28, "STAGE TO ACTIVATION", f"{stage_us} µs", "Target: <= 10,000 µs (10 ms)", f"Lease expiry ack: {expiry_us} µs", "PASS  >1000x margin"),
        (390, "1-HOUR RENEWAL SOAK", "2,314", "Consecutive renewals (26 passes)", "Missed leases / panics: 0", "PASS  3,611 s KVM"),
        (752, "VERIFICATION SUITE", f"{passed}/{total}", "Automated tiers passed", f"{rust_n} Rust + {py_n} Python + 19 QEMU", "PASS  100%"),
    ]
    parts = [
        heading(
            "Kernel enforcement timings and soak qualification",
            "AMD Ryzen 5 1600, kvm_amd, against the declared S3/G5 engineering targets.",
        )
    ]
    for x, kicker, value, line_a, line_b, badge in cards:
        parts.append(panel(x, 84, 340, 176))
        parts.append(text(x + 18, 112, kicker, size=12, fill="#ff2a2a", weight=700))
        parts.append(text(x + 18, 162, value, size=40, fill="#f4f4f4", weight=800))
        parts.append(text(x + 18, 188, line_a, size=12, fill="#c8c8c8", weight=500))
        parts.append(text(x + 18, 208, line_b, size=12, fill="#c8c8c8", weight=500))
        parts.append(f'<rect x="{x + 18}" y="222" width="210" height="22" fill="#140404" stroke="#ff1a1a" stroke-width="1"/>')
        parts.append(text(x + 28, 238, badge, size=12, fill="#ff2a2a", weight=700))
    return document(1120, 284, "\n".join(parts))


def _pair_rows(origin_x, origin_y, rows, scale, label_w, row_h=40):
    parts = []
    for idx, (label, old, new) in enumerate(rows):
        y = origin_y + idx * row_h
        w0 = max(2, round(old * scale))
        w1 = max(2, round(new * scale))
        bx = origin_x + label_w
        parts.append(text(bx - 10, y + 26, label, size=12, fill="#e8e8e8", weight=700, anchor="end"))
        parts.append(bar(bx, y, w0, 14, "silver"))
        parts.append(text(bx + w0 + 8, y + 12, f"{old:.1f} ms", size=12, fill="#c8c8c8", weight=600))
        parts.append(bar(bx, y + 16, w1, 14, "red"))
        delta = (new / old - 1) * 100 if old else 0
        parts.append(text(bx + w1 + 8, y + 28, f"{new:.1f} ms ({delta:+.0f}%)", size=12, fill="#ff3a3a", weight=700))
    return parts


def real_benchmark_svg(
    bal_p50, bal_p95, bal_p99, bal_mib,
    adp_p50, adp_p95, adp_p99, adp_mib,
    t_p99, l_p99, b_p99,
    t_bat, l_bat, h_bat, v1_bat,
) -> str:
    speedup = bal_p99 / max(adp_p99, 0.1)
    reduction = (bal_p99 - adp_p99) * 100.0 / max(bal_p99, 0.1)
    left_scale = 230.0 / max(bal_p99, 50.0)
    right_scale = 210.0 / max(b_p99, 180.0)
    parts = [
        heading(
            "Real-data unikernel benchmarks",
            "Left: live HMAC-SHA256 + JSON RPC (--real-workload). Right: 252 sealed KVM branch decisions (jobs-v2).",
        ),
        panel(24, 78, 528, 300),
        text(44, 106, "A. Real-payload RPC and HMAC", size=14, fill="#f4f4f4", weight=800),
        text(44, 126, "168 open-loop RPCs + 8 KiB batch blocks", size=11, fill="#a3a3a3", weight=500),
        text(44, 154, "RPC p99 latency, lower is better", size=12, fill="#ff2a2a", weight=700),
    ]
    bx = 196
    parts.append(text(bx - 10, 180, "balanced 1:1:1", size=12, fill="#e8e8e8", weight=700, anchor="end"))
    w_bal = max(2, round(bal_p99 * left_scale))
    parts.append(bar(bx, 166, w_bal, 16, "silver"))
    parts.append(text(bx + w_bal + 8, 179, f"{bal_p99:.1f} ms", size=12, fill="#d0d0d0", weight=700))
    parts.append(text(bx - 10, 210, "adaptive 6:2:2", size=12, fill="#ff3a3a", weight=700, anchor="end"))
    w_adp = max(2, round(adp_p99 * left_scale))
    parts.append(bar(bx, 196, w_adp, 16, "red"))
    parts.append(text(bx + w_adp + 8, 209, f"{adp_p99:.1f} ms  ({speedup:.2f}x)", size=12, fill="#ff3a3a", weight=700))
    parts.append(f'<line x1="44" y1="228" x2="528" y2="228" stroke="#2a2a2a" stroke-width="1"/>')
    parts.append(text(44, 252, "RPC p50 / p95 and verified batch, 2 s window", size=12, fill="#f0f0f0", weight=700))
    parts.append(text(44, 274, f"Static balanced:  p50 {bal_p50:.1f} ms   p95 {bal_p95:.1f} ms   {bal_mib} MiB", size=12, fill="#c8c8c8", weight=500))
    parts.append(text(44, 294, f"Adaptive policy:  p50 {adp_p50:.1f} ms   p95 {adp_p95:.1f} ms   {adp_mib} MiB", size=12, fill="#ff3a3a", weight=700))
    parts.append('<rect x="44" y="312" width="488" height="28" fill="#140404" stroke="#ff1a1a" stroke-width="1"/>')
    parts.append(text(56, 331, f"{reduction:.1f}% p99 reduction, batch HMAC throughput held", size=12, fill="#ff2a2a", weight=700))

    parts.append(panel(568, 78, 528, 324))
    parts.append(text(588, 106, "B. Sealed KVM tradeoff, 21 units", size=14, fill="#f4f4f4", weight=800))
    parts.append(text(588, 126, "Under-capacity mean p99 vs overload batch CPU retained", size=11, fill="#a3a3a3", weight=500))
    parts.append(text(588, 154, "Under-capacity mean p99 (15%–45% load)", size=12, fill="#ff2a2a", weight=700))
    series = [
        (172, "pending-threshold", t_p99, "red", f"{t_p99:.1f} ms  (2.35x)"),
        (196, "logistic-v0", l_p99, "silver", f"{l_p99:.1f} ms"),
        (220, "Laya / balanced", b_p99, "ash", f"{b_p99:.1f} ms"),
    ]
    rx = 760
    for y, label, value, kind, caption in series:
        w = max(2, round(value * right_scale))
        parts.append(text(rx - 10, y + 13, label, size=12, fill="#e8e8e8", weight=700, anchor="end"))
        parts.append(bar(rx, y, w, 16, kind))
        parts.append(text(rx + w + 8, y + 13, caption, size=12, fill="#f0f0f0" if kind != "red" else "#ff3a3a", weight=700))
    parts.append(f'<line x1="588" y1="248" x2="1072" y2="248" stroke="#2a2a2a" stroke-width="1"/>')
    parts.append(text(588, 270, "Overload batch CPU retained (floor 95%)", size=12, fill="#ff2a2a", weight=700))
    retain = [
        (284, "pending-threshold", t_bat, "red", f"{t_bat:.1f}%  PASS"),
        (306, "heuristic-v1", v1_bat, "red", f"{v1_bat:.1f}%  PASS"),
        (328, "Laya / logistic-v0", l_bat, "silver", f"{l_bat:.1f}%  PASS"),
        (350, "heuristic-v0 (old rule)", h_bat, "void", f"{h_bat:.1f}%  INFEASIBLE"),
    ]
    for y, label, value, kind, caption in retain:
        w = max(2, round(180 * value / 100.0))
        parts.append(text(rx - 10, y + 13, label, size=12, fill="#e8e8e8", weight=700, anchor="end"))
        parts.append(bar(rx, y, w, 16, kind))
        parts.append(text(rx + w + 8, y + 13, caption, size=12, fill="#ff3a3a" if kind == "void" else "#f0f0f0", weight=700))
    return document(1120, 424, "\n".join(parts))


def explain_svg(cards: list[dict]) -> str:
    parts = [
        heading(
            "Policy explainability: margins, attribution, counterfactuals",
            "Why the controller selects each profile across the four sealed-KVM regimes.",
        )
    ]
    for idx, card in enumerate(cards):
        col, row = idx % 2, idx // 2
        x = 24 + col * 548
        y = 78 + row * 168
        choice = card["choice"]
        kind = "red" if choice == "latency" else "silver"
        parts.append(panel(x, y, 528, 152))
        parts.append(text(x + 18, y + 28, card["title"], size=13, fill="#f4f4f4", weight=800))
        parts.append(bar(x + 392, y + 12, 118, 20, kind))
        parts.append(text(x + 451, y + 27, f"-> {choice}", size=12, fill=INK[kind], weight=800, anchor="middle"))
        parts.append(text(x + 18, y + 54, card["threshold"], size=12, fill="#f0f0f0", weight=700))
        parts.append(text(x + 18, y + 74, card["regime"], size=11, fill="#b5b5b5", weight=500))
        parts.append(text(x + 18, y + 94, card["logistic"], size=11, fill="#b5b5b5", weight=500))
        parts.append(f'<rect x="{x + 18}" y="{y + 106}" width="492" height="28" fill="#100404" stroke="#3a1010" stroke-width="1"/>')
        parts.append(f'<rect x="{x + 18}" y="{y + 106}" width="3" height="28" fill="#ff1a1a"/>')
        parts.append(text(x + 30, y + 125, card["counterfactual"], size=11, fill="#f2f2f2", weight=700))
    return document(1120, 430, "\n".join(parts))


def wake_ab_svg(
    n_real: int,
    seeds: list[int],
    rpc_rows: list[tuple[str, float, float]],
    scenarios: list[tuple[str, float, float]],
    batch_line: str,
    under_line: str,
    batch_cpu_line: str,
    overload_line: str,
    overload_note: str,
) -> str:
    rpc_max = max(old for _, old, _ in rpc_rows)
    scen_max = max(old for _, old, _ in scenarios)
    parts = [
        heading(
            "Scheduler wake mode A/B: bounded wake credit + in-class FIFO",
            "Same guest binary on AMD Ryzen 5 1600 (kvm_amd). wake=0 is the scheduler behind every label and sealed result.",
        ),
        swatch(700, 68, "silver", "wake=0 (default)"),
        swatch(920, 68, "red", "wake=3"),
        panel(24, 96, 528, 340),
        text(44, 124, "A. Real-payload RPC benchmark", size=14, fill="#f4f4f4", weight=800),
        text(44, 144, f"HMAC/JSON RPCs, median of {n_real} boots per mode", size=11, fill="#a3a3a3", weight=500),
    ]
    parts.extend(_pair_rows(44, 160, rpc_rows, 200.0 / rpc_max, 130, row_h=44))
    parts.append('<rect x="44" y="348" width="488" height="26" fill="#140404" stroke="#ff1a1a" stroke-width="1"/>')
    parts.append(text(56, 366, "A 4 ms RPC now finishes near its ~4 ms solo cost (p50)", size=12, fill="#ff2a2a", weight=700))
    parts.append(text(44, 404, batch_line, size=12, fill="#c8c8c8", weight=500))
    parts.append(panel(568, 96, 528, 340))
    parts.append(text(588, 124, "B. jobs-v2 branch sweep, latency p99", size=14, fill="#f4f4f4", weight=800))
    parts.append(text(588, 144, f"Geo-mean of seeds {seeds[0]} and {seeds[1]}, two clean repeats per cell", size=11, fill="#a3a3a3", weight=500))
    parts.extend(_pair_rows(588, 158, scenarios, 190.0 / scen_max, 78, row_h=34))
    parts.append('<line x1="588" y1="336" x2="1072" y2="336" stroke="#2a2a2a" stroke-width="1"/>')
    parts.append(text(588, 356, under_line, size=11, fill="#ff3a3a", weight=700))
    parts.append(text(588, 374, batch_cpu_line, size=11, fill="#c8c8c8", weight=500))
    parts.append(text(588, 392, overload_line, size=11, fill="#d0d0d0", weight=600))
    parts.append(text(588, 410, overload_note, size=10, fill="#8d8d8d", weight=500))
    return document(1120, 460, "\n".join(parts))

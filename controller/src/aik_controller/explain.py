"""Explainability and counterfactual analysis for Ferrum policy controllers.

Provides:
1. Rule-chain attribution for `heuristic-v0` (`explain_heuristic`).
2. Boundary margin and regime attribution for `pending-threshold` (`explain_threshold`).
3. Standardized feature z-scores and log-odds contributions for `logistic-v0` (`explain_logistic`).
4. Real-data counterfactual outcome comparison on branched KVM trials (`explain_counterfactual`).
5. Single-line `EXPLAIN` formatting for live `aik_controller.server` sessions (`explain_line`).
"""

import argparse
import json
from pathlib import Path

from aik_controller import fit
from aik_controller.heuristic import choose, load_thresholds


def _group(snapshot: dict, name: str) -> dict:
    return next(group for group in snapshot["groups"] if group["class"] == name)


def explain_heuristic(snapshot: dict, thresholds: dict) -> dict:
    """Trace which rule in `heuristic-v0` fired and the margin to each threshold."""
    latency = _group(snapshot, "latency")
    batch = _group(snapshot, "batch")
    pressure = snapshot["pressure"]
    cap = pressure["managed_cap_bytes"]
    pressure_bp = 0 if cap == 0 else pressure["managed_used_bytes"] * 10_000 // cap
    backlog = pressure["evictable_backlog_bytes"]
    wait_high = (
        latency["wait_samples"] > 0
        and latency["max_wait_us"] >= thresholds["latency_wait_us_high"]
    )
    queue_high = latency["queue_len"] >= thresholds["latency_queue_high"]
    batch_high = batch["queue_len"] >= thresholds["batch_queue_high"] and latency["queue_len"] == 0

    if (
        pressure["emergency"]
        or pressure_bp >= thresholds["memory_pressure_bp"]
        or backlog >= thresholds["evictable_backlog_bytes"]
    ):
        rule = "memory_pressure_or_emergency"
        choice = "reclaim"
        rationale = (
            f"emergency={pressure['emergency']}, pressure_bp={pressure_bp}>={thresholds['memory_pressure_bp']} "
            f"or backlog={backlog}>={thresholds['evictable_backlog_bytes']} -> reclaim (2:2:6)"
        )
    elif queue_high or wait_high:
        rule = "latency_queue_or_wait_high"
        choice = "latency"
        rationale = (
            f"latency_queue={latency['queue_len']} (th={thresholds['latency_queue_high']}), "
            f"max_wait_us={latency['max_wait_us']} (th={thresholds['latency_wait_us_high']}) -> latency (6:2:2)"
        )
    elif batch_high:
        rule = "sustained_batch_demand"
        choice = "throughput"
        rationale = (
            f"batch_queue={batch['queue_len']}>={thresholds['batch_queue_high']} with latency_queue=0 -> throughput (2:6:2)"
        )
    else:
        rule = "default_balanced_fallback"
        choice = "balanced"
        rationale = (
            f"no pressure/queue threshold crossed (lat_q={latency['queue_len']}, bat_q={batch['queue_len']}) -> balanced (1:1:1)"
        )

    assert choice == choose(snapshot, thresholds)
    return {
        "controller": "heuristic-v0",
        "choice": choice,
        "triggered_rule": rule,
        "signals": {
            "emergency": bool(pressure["emergency"]),
            "memory_pressure_bp": pressure_bp,
            "memory_pressure_threshold_bp": thresholds["memory_pressure_bp"],
            "evictable_backlog_bytes": backlog,
            "evictable_backlog_threshold_bytes": thresholds["evictable_backlog_bytes"],
            "latency_queue_len": latency["queue_len"],
            "latency_queue_threshold": thresholds["latency_queue_high"],
            "latency_max_wait_us": latency["max_wait_us"],
            "latency_wait_threshold_us": thresholds["latency_wait_us_high"],
            "batch_queue_len": batch["queue_len"],
            "batch_queue_threshold": thresholds["batch_queue_high"],
        },
        "rationale": rationale,
    }


def explain_threshold(snapshot: dict, params: dict) -> dict:
    """Explain a `pending-threshold` decision by distance to the cut and load regime."""
    latency = _group(snapshot, "latency")
    q_len = int(latency["queue_len"])
    cut = float(params["cut"])
    margin = q_len - cut
    choice = fit.threshold_choice(params, snapshot)
    regime = "overload_shedding" if q_len >= cut else "under_capacity"
    share = latency["cpu_service_us"] / max(snapshot.get("window_us", 1), 1)
    if regime == "under_capacity":
        rationale = (
            f"pending={q_len} < cut={cut} (margin={margin:+.1f} jobs, regime={regime}, lat_share={share:.2f}) "
            f"-> {choice} (cuts tail latency while batch retains >=95% CPU)"
        )
    else:
        rationale = (
            f"pending={q_len} >= cut={cut} (margin={margin:+.1f} jobs, regime={regime}, lat_share={share:.2f}) "
            f"-> {choice} (preserves >=95% batch CPU floor under overload)"
        )
    return {
        "controller": "pending-threshold",
        "choice": choice,
        "pending": q_len,
        "cut": cut,
        "margin_jobs": round(margin, 2),
        "regime": regime,
        "latency_share": round(share, 4),
        "max_wait_us": int(latency["max_wait_us"]),
        "rationale": rationale,
    }


def explain_distilled(snapshot: dict, cut: float = 14.5) -> dict:
    """Explain a `distilled-joint` (`DISTILLED_JOINT_V1`) multi-regime decision."""
    latency = _group(snapshot, "latency")
    batch = _group(snapshot, "batch")
    maint = _group(snapshot, "maintenance")
    pressure = snapshot["pressure"]
    cap = int(pressure.get("managed_cap_bytes", 0))
    used = int(pressure.get("managed_used_bytes", 0))
    headroom = int(pressure.get("headroom_bytes", 134_217_728))
    backlog = int(pressure.get("evictable_backlog_bytes", 0))
    emergency = bool(pressure.get("emergency", False))
    pressure_bp = 0 if cap == 0 else (used * 10_000) // cap

    q_len = int(latency["queue_len"])
    b_len = int(batch["queue_len"])
    wait_us = int(latency["max_wait_us"])
    wait_samples = int(latency["wait_samples"])
    margin = q_len - cut

    if emergency or pressure_bp >= 8_000 or backlog >= 33_554_432 or (cap > 0 and headroom < 67_108_864):
        choice = "reclaim"
        regime = "overload_or_reclaim"
        conf_bp = 9_900 if emergency else 9_400
        rationale = (
            f"memory_pressure={pressure_bp}bp, backlog={backlog // (1024*1024)}MiB, emergency={emergency} "
            f"-> reclaim (2:2:6, 32MiB cache, 4/2 admission)"
        )
    elif q_len >= cut:
        choice = "reclaim"
        regime = "overload_or_reclaim"
        excess_x2 = int(round((q_len - cut) * 2))
        conf_bp = min(7_800 + excess_x2 * 150, 9_800)
        rationale = (
            f"pending={q_len} >= cut={cut} (margin={margin:+.1f} jobs) "
            f"-> reclaim (protects >=95% batch confirmation specificity under overload)"
        )
    elif q_len >= 4 or (wait_samples > 0 and wait_us >= 5_000) or (q_len > 0 and maint["runnable"] == 0 and b_len == 0):
        choice = "latency"
        regime = "plume_trigger_capture"
        slack_x2 = int(round((cut - q_len) * 2))
        conf_bp = min(7_600 + slack_x2 * 75, 9_700)
        rationale = (
            f"pending={q_len} < cut={cut} (wait={wait_us/1000:.1f}ms) "
            f"-> latency (6:2:2, 16/4 admission; maximizes plume capture sensitivity)"
        )
    elif b_len >= 4 and q_len == 0:
        choice = "throughput"
        regime = "deep_confirmation"
        conf_bp = min(8_200 + b_len * 120, 9_600)
        rationale = (
            f"batch_queue={b_len}>=4 with latency_queue=0 "
            f"-> throughput (2:6:2, 96MiB cache, 8/16 admission; accelerates confirmation)"
        )
    else:
        choice = "balanced"
        regime = "clean_air_patrol"
        conf_bp = 9_100
        rationale = (
            f"quiet patrol (lat_q={q_len}, bat_q={b_len}) "
            f"-> balanced (1:1:1, 64MiB cache, tickless sleep coalescing)"
        )

    return {
        "controller": "distilled-joint",
        "choice": choice,
        "regime": regime,
        "confidence_bp": conf_bp,
        "pending": q_len,
        "cut": cut,
        "margin_jobs": round(margin, 2),
        "rationale": rationale,
    }


def explain_logistic(snapshot: dict, params: dict) -> dict:
    """Decompose `logistic-v0` into standardized z-scores and per-feature log-odds contributions."""
    names = params["features"]
    raw_values = {name: fit.FEATURES[name](snapshot) for name in names}
    z_scores = {
        name: (raw_values[name] - params["mean"][j]) / params["std"][j]
        for j, name in enumerate(names)
    }
    probs = fit.logistic_probabilities(params, snapshot)
    choice = fit.logistic_choice(params, snapshot)
    profiles = params["profiles"]

    per_profile = {}
    for c, prof in enumerate(profiles):
        contribs = {
            name: round(params["weights"][c][j] * z_scores[name], 4)
            for j, name in enumerate(names)
        }
        logit = round(params["bias"][c] + sum(contribs.values()), 4)
        per_profile[prof] = {
            "probability": round(probs[c], 4),
            "logit": logit,
            "bias": round(params["bias"][c], 4),
            "contributions": contribs,
        }

    chosen_contribs = per_profile[choice]["contributions"]
    dominant_feature = max(chosen_contribs, key=lambda k: abs(chosen_contribs[k]))
    return {
        "controller": "logistic-v0",
        "choice": choice,
        "probability": per_profile[choice]["probability"],
        "dominant_feature": dominant_feature,
        "raw_features": {k: round(v, 4) for k, v in raw_values.items()},
        "z_scores": {k: round(v, 4) for k, v in z_scores.items()},
        "profiles": per_profile,
    }


def explain_counterfactual(unit: dict, choice: str) -> dict:
    """Compare the chosen profile's real KVM outcome against all counterfactual profiles in `unit`."""
    profiles = unit["profiles"]
    chosen_stats = profiles[choice]
    balanced_p99 = profiles["balanced"]["p99_median"]
    chosen_p99 = chosen_stats["p99_median"]

    p99_speedup_vs_balanced = None
    if balanced_p99 is not None and chosen_p99 is not None and chosen_p99 > 0:
        p99_speedup_vs_balanced = round(balanced_p99 / chosen_p99, 2)

    alternatives = {}
    for prof, stats in profiles.items():
        alternatives[prof] = {
            "p99_median_us": stats["p99_median"],
            "batch_retained_bp": stats["batch_retained_bp"],
            "completion_median_bp": stats["completion_median"],
            "feasible": stats["feasible"],
            "in_label": prof in unit["label"],
        }

    return {
        "scenario": unit["scenario"],
        "seed": unit["seed"],
        "choice": choice,
        "label": list(unit["label"]),
        "hit": choice in unit["label"],
        "feasible": bool(chosen_stats["feasible"]),
        "p99_median_us": chosen_p99,
        "batch_retained_bp": chosen_stats["batch_retained_bp"],
        "completion_median_bp": chosen_stats["completion_median"],
        "p99_speedup_vs_balanced": p99_speedup_vs_balanced,
        "counterfactuals": alternatives,
    }


def explain_line(
    snapshot: dict,
    thresholds: dict,
    candidate_params: dict | None = None,
    logistic_params: dict | None = None,
) -> str:
    """Single-line structured explanation for `aik_controller.server` output."""
    h = explain_heuristic(snapshot, thresholds)
    parts = [
        f"EXPLAIN seq={snapshot.get('request_seq', 0)}",
        f"heuristic={h['choice']}({h['triggered_rule']})",
    ]
    if candidate_params is not None:
        t = explain_threshold(snapshot, candidate_params)
        parts.append(
            f"threshold={t['choice']}(pending={t['pending']},cut={t['cut']},margin={t['margin_jobs']:+.1f},regime={t['regime']})"
        )
    if logistic_params is not None:
        lg = explain_logistic(snapshot, logistic_params)
        parts.append(
            f"logistic={lg['choice']}(p={lg['probability']:.3f},top={lg['dominant_feature']})"
        )
    return " ".join(parts)


def summarize_real_data_benchmarks(root: Path, host_tag: str = "r5-1600") -> dict:
    """Summarize real recorded KVM benchmarks and explainability case studies on `host_tag`."""
    eval_doc = json.loads((root / "data" / "jobs-v2" / f"evaluation-v0-{host_tag}.json").read_text())
    fit_doc = json.loads((root / "data" / "jobs-v2" / f"fit-v0-{host_tag}.json").read_text())
    candidate_doc = json.loads((root / "configs" / f"candidate-v0-{host_tag}.json").read_text())
    thresholds = load_thresholds(root / "configs" / "heuristic-v0.json")
    cand_params = candidate_doc["params"]
    log_params = fit_doc["fitted"]["logistic-v0"]

    label_files = [
        root / "data" / "jobs-v2" / item["labels"]
        for item in eval_doc["final_test"] + eval_doc["out_of_distribution"]
    ]

    units = []
    for lf in label_files:
        doc = json.loads(lf.read_text())
        for u in doc["units"]:
            if u.get("status", "labelled") == "diverged":
                continue
            units.append((doc["split"], u))

    # Aggregate real outcome metrics per controller across the 21 sealed units (15 final_test + 6 OOD)
    controller_names = [
        "distilled-joint",
        "pending-threshold",
        "logistic-v0",
        "fixed-balanced",
        "heuristic-v0",
    ]
    agg = {
        name: {
            "units": 0,
            "hits": 0,
            "feasible_units": 0,
            "under_capacity_p99_ms": [],
            "overload_batch_retained_pct": [],
            "overload_completion_pct": [],
        }
        for name in controller_names
    }

    case_studies = []
    scenario_rows = []

    for split_name, unit in units:
        snap = unit["snapshot"]
        d_exp = explain_distilled(snap, cand_params["cut"])
        t_exp = explain_threshold(snap, cand_params)
        l_exp = explain_logistic(snap, log_params)
        h_exp = explain_heuristic(snap, thresholds)

        picks = {
            "distilled-joint": d_exp["choice"],
            "pending-threshold": t_exp["choice"],
            "logistic-v0": l_exp["choice"],
            "fixed-balanced": "balanced",
            "heuristic-v0": h_exp["choice"],
        }

        under_cap = t_exp["pending"] < cand_params["cut"]
        for cname, choice in picks.items():
            cf = explain_counterfactual(unit, choice)
            st = agg[cname]
            st["units"] += 1
            st["hits"] += int(cf["hit"])
            st["feasible_units"] += int(cf["feasible"])
            if under_cap and cf["p99_median_us"] is not None:
                st["under_capacity_p99_ms"].append(cf["p99_median_us"] / 1000.0)
            if not under_cap:
                st["overload_batch_retained_pct"].append(cf["batch_retained_bp"] / 100.0)
                st["overload_completion_pct"].append(cf["completion_median_bp"] / 100.0)

        cf_cand = explain_counterfactual(unit, t_exp["choice"])
        cf_bal = explain_counterfactual(unit, "balanced")
        cf_heur = explain_counterfactual(unit, h_exp["choice"])
        scenario_rows.append({
            "split": split_name,
            "scenario": unit["scenario"],
            "seed": unit["seed"],
            "pending": t_exp["pending"],
            "max_wait_ms": round(t_exp["max_wait_us"] / 1000.0, 2),
            "latency_share": t_exp["latency_share"],
            "threshold_choice": t_exp["choice"],
            "logistic_choice": l_exp["choice"],
            "logistic_prob": l_exp["probability"],
            "heuristic_choice": h_exp["choice"],
            "label": unit["label"],
            "candidate_p99_ms": None if cf_cand["p99_median_us"] is None else round(cf_cand["p99_median_us"] / 1000.0, 2),
            "balanced_p99_ms": None if cf_bal["p99_median_us"] is None else round(cf_bal["p99_median_us"] / 1000.0, 2),
            "candidate_batch_pct": round(cf_cand["batch_retained_bp"] / 100.0, 2),
            "heuristic_batch_pct": round(cf_heur["batch_retained_bp"] / 100.0, 2),
        })

        # Pick 4 representative case studies:
        # 1) s12-u35/301 (under-capacity seen load)
        # 2) s12-u45/312 (unseen 45% load near boundary)
        # 3) s12-u72/311 (unseen 72% overload)
        # 4) s32-u65/401 (OOD 32ms jobs at 65% load — pre-registered weak point)
        if (unit["scenario"], unit["seed"]) in {
            ("s12-u35", 301),
            ("s12-u45", 312),
            ("s12-u72", 311),
            ("s32-u65", 401),
        }:
            case_studies.append({
                "split": split_name,
                "scenario": unit["scenario"],
                "seed": unit["seed"],
                "threshold": t_exp,
                "logistic": l_exp,
                "heuristic": h_exp,
                "counterfactual": cf_cand,
            })

    summary_controllers = {}
    for cname, st in agg.items():
        u_p99 = st["under_capacity_p99_ms"]
        o_bat = st["overload_batch_retained_pct"]
        o_cmp = st["overload_completion_pct"]
        summary_controllers[cname] = {
            "units": st["units"],
            "hits": st["hits"],
            "hit_rate_pct": round(st["hits"] * 100.0 / max(st["units"], 1), 1),
            "feasible_units": st["feasible_units"],
            "feasible_rate_pct": round(st["feasible_units"] * 100.0 / max(st["units"], 1), 1),
            "mean_under_capacity_p99_ms": round(sum(u_p99) / len(u_p99), 2) if u_p99 else None,
            "mean_overload_batch_retained_pct": round(sum(o_bat) / len(o_bat), 2) if o_bat else None,
            "mean_overload_completion_pct": round(sum(o_cmp) / len(o_cmp), 2) if o_cmp else None,
        }

    return {
        "host_tag": host_tag,
        "host_cpu": eval_doc["host_cpu"],
        "controllers": summary_controllers,
        "case_studies": case_studies,
        "scenario_rows": scenario_rows,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Explain Ferrum policy decisions and real-data counterfactuals")
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[3])
    parser.add_argument("--host", default="r5-1600", choices=("r5-1600", "i7-10750h"))
    parser.add_argument("--json", action="store_true", help="Print full JSON report")
    args = parser.parse_args(argv)

    report = summarize_real_data_benchmarks(args.root, args.host)
    if args.json:
        print(json.dumps(report, indent=2))
        return 0

    print(f"=== Real-Data Sealed KVM Benchmark & Explainability ({report['host_cpu']}) ===")
    for cname, stats in report["controllers"].items():
        print(
            f"  {cname:<20} hits={stats['hits']}/{stats['units']} ({stats['hit_rate_pct']}%) "
            f"feasible={stats['feasible_units']}/{stats['units']} ({stats['feasible_rate_pct']}%) "
            f"under_cap_p99={stats['mean_under_capacity_p99_ms']}ms "
            f"overload_batch={stats['mean_overload_batch_retained_pct']}%"
        )
    print("\n=== Representative Explainability Case Studies ===")
    for cs in report["case_studies"]:
        t = cs["threshold"]
        lg = cs["logistic"]
        cf = cs["counterfactual"]
        print(f"* {cs['scenario']}/{cs['seed']} ({cs['split']}): label={cf['label']}")
        print(f"    Threshold : {t['rationale']}")
        print(
            f"    Logistic  : choice={lg['choice']} (P={lg['probability']:.3f}, dominant={lg['dominant_feature']}) "
            f"contribs={lg['profiles'][lg['choice']]['contributions']}"
        )
        print(
            f"    Outcome   : p99={cf['p99_median_us']} us, batch_retained={cf['batch_retained_bp']/100:.2f}%, "
            f"speedup_vs_balanced={cf['p99_speedup_vs_balanced']}x"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

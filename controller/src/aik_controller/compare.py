"""Every controller on the same recorded decisions, and where each cuts batch.

Each usable branch of a scored unit is one decision: its own pre-decision
state, scored against the unit's measured profile summaries under
objective-v2. A decision "cuts too much" when the profile it picks kept less
than objective-v2's batch_retain_bp of balanced's batch CPU.

The splits carry different weight for heuristic-v1. Training set its one fitted
value. Development, calibration and both sealed splits were inspected while v1
was designed, so they are development evidence for v1. Only units collected
after v1 was frozen (a holdout manifest) are held out for it.

Laya appears only where a recorded report exists: the zero-shot run on each
host's development labels, and adapt-v0 on the i7-10750h development labels.
An abstention counts as balanced, as in fit-v0.
"""

import argparse
import json
import sys
from pathlib import Path

from aik_controller import fit, holdout, jobs, splits
from aik_controller.explain import explain_distilled
from aik_controller.heuristic import choose, evaluate_v1, load_thresholds, load_v1

HOSTS = ("r5-1600", "i7-10750h")
SPLITS = ("training", "development", "calibration", "final_test", "out_of_distribution")
EVIDENCE = {
    "training": "fit",
    "development": "design",
    "calibration": "design",
    "final_test": "design",
    "out_of_distribution": "design",
    "holdout": "held_out",
}
CONTROLLERS = (
    "fixed-balanced",
    "heuristic-v0",
    "heuristic-v1",
    "pending-threshold",
    "logistic-v0",
    "distilled-joint",
    "laya-zero-shot",
    "laya-adapt-v0",
)
POST_HOC = {"distilled-joint": "added after the sealed splits opened; every split is development evidence for it"}


def _laya(path: Path) -> dict[str, tuple[str, bool]]:
    report = json.loads(path.read_text())
    rows = report["rows"] if isinstance(report["rows"], list) else report["choices"]
    return {
        row["key"]: (fit.FALLBACK, True) if row["kind"] in fit.ABSTAIN_KINDS else (row["choice"], False)
        for row in rows
    }


def rules(root: Path, host: str) -> dict:
    """Snapshot rules for one host. Laya is looked up by branch key instead."""
    v0 = load_thresholds(root / "configs" / "heuristic-v0.json")
    v1 = load_v1(root)
    candidate = json.loads((root / "configs" / f"candidate-v0-{host}.json").read_text())
    report = json.loads((root / "data" / "jobs-v2" / f"fit-v0-{host}.json").read_text())
    threshold, logistic = candidate["params"], report["fitted"]["logistic-v0"]
    return {
        "fixed-balanced": lambda snap: "balanced",
        "heuristic-v0": lambda snap: choose(snap, v0),
        "heuristic-v1": lambda snap: evaluate_v1(snap, v1)["profile"],
        "pending-threshold": lambda snap: fit.threshold_choice(threshold, snap),
        "logistic-v0": lambda snap: fit.logistic_choice(logistic, snap),
        "distilled-joint": lambda snap: explain_distilled(snap, threshold["cut"])["choice"],
    }


def laya_reports(root: Path) -> dict[tuple[str, str, str], dict]:
    data = root / "data" / "jobs-v2"
    out = {}
    for host in HOSTS:
        path = data / f"zero-shot-development-seed151-{host}.json"
        if path.exists():
            out[("laya-zero-shot", host, "development")] = _laya(path)
        path = data / f"adapt-v0-development-{host}.json"
        if path.exists():
            out[("laya-adapt-v0", host, "development")] = _laya(path)
    return out


def _docs(root: Path, split: str, host: str) -> list[dict]:
    data = root / "data" / "jobs-v2"
    if split == "holdout":
        return [holdout.load_labels(root, path) for path in sorted(data.glob(f"labels-holdout-v1-{host}.json"))]
    return [fit.load_labels(root, path, split) for path in sorted(data.glob(f"labels-{split}-seed*-{host}.json"))]


def decisions(root: Path, splits_read: tuple[str, ...] = SPLITS) -> list[dict]:
    manifest = splits.load_branched(root)
    family = jobs.load_family(splits.family_path(root, manifest))
    objective = json.loads(splits.objective_path(root, manifest).read_text())
    offered = {scenario["name"]: scenario["offered_bp"] for scenario in family["scenarios"]}
    config = fit.load_config(root)
    laya = laya_reports(root)
    out = []
    for host in HOSTS:
        host_rules = rules(root, host)
        for split in splits_read:
            for doc in _docs(root, split, host):
                scored, _ = fit.decisions([doc], config)
                for unit, branch in scored:
                    picks = {cid: (rule(branch["snapshot"]), False) for cid, rule in host_rules.items()}
                    for (cid, laya_host, laya_split), rows in laya.items():
                        if (laya_host, laya_split) == (host, split):
                            if branch["key"] not in rows:
                                raise fit.FitError(f"{cid} has no row for {host} {branch['key']}")
                            picks[cid] = rows[branch["key"]]
                    latency = next(g for g in branch["snapshot"]["groups"] if g["class"] == "latency")
                    for cid, (profile, abstained) in picks.items():
                        summary = unit["profiles"][profile]
                        out.append({
                            "controller": cid,
                            "host": host,
                            "split": split,
                            "scenario": unit["scenario"],
                            "seed": unit["seed"],
                            "key": branch["key"],
                            "overload": offered[unit["scenario"]] > 5000,
                            "pending": latency["queue_len"],
                            "max_wait_us": latency["max_wait_us"],
                            "label": unit["label"],
                            "choice": profile,
                            "abstained": abstained,
                            "hit": profile in unit["label"],
                            "feasible": bool(summary["feasible"]),
                            "batch_retained_bp": summary["batch_retained_bp"],
                            "cuts_batch": summary["batch_retained_bp"] < objective["batch_retain_bp"],
                        })
    return out


def _summary(rows: list[dict]) -> dict:
    over = [row["batch_retained_bp"] for row in rows if row["overload"]]
    under = [row for row in rows if not row["overload"]]
    return {
        "decisions": len(rows),
        "hits": sum(row["hit"] for row in rows),
        "infeasible": sum(not row["feasible"] for row in rows),
        "cuts_batch": sum(row["cuts_batch"] for row in rows),
        "abstain": sum(row["abstained"] for row in rows),
        "min_batch_retained_bp": min(row["batch_retained_bp"] for row in rows),
        "overload_mean_batch_retained_bp": round(sum(over) / len(over)) if over else None,
        "under_capacity_hits": sum(row["hit"] for row in under),
        "under_capacity_decisions": len(under),
    }


def report(rows: list[dict]) -> dict:
    table = {}
    for cid in CONTROLLERS:
        mine = [row for row in rows if row["controller"] == cid]
        if not mine:
            continue
        by_split = {}
        for split in dict.fromkeys(row["split"] for row in mine):
            these = [row for row in mine if row["split"] == split]
            by_split[split] = {
                "evidence": EVIDENCE[split],
                "pooled": _summary(these),
                **{host: _summary([row for row in these if row["host"] == host]) for host in HOSTS if any(row["host"] == host for row in these)},
            }
        table[cid] = by_split
    cuts: dict[tuple, dict] = {}
    for row in rows:
        if not row["cuts_batch"]:
            continue
        key = (row["controller"], row["host"], row["split"], row["scenario"], row["seed"], row["choice"])
        entry = cuts.setdefault(key, {
            "controller": row["controller"],
            "host": row["host"],
            "split": row["split"],
            "unit": f"{row['scenario']}/{row['seed']}",
            "choice": row["choice"],
            "label": row["label"],
            "batch_retained_bp": row["batch_retained_bp"],
            "decisions": 0,
            "pending": [],
            "max_wait_ms": [],
        })
        entry["decisions"] += 1
        entry["pending"].append(row["pending"])
        entry["max_wait_ms"].append(round(row["max_wait_us"] / 1000))
    side = {}
    for row in rows:
        if row["split"] != "development" or row["controller"] not in ("heuristic-v0", "heuristic-v1", "laya-zero-shot", "laya-adapt-v0"):
            continue
        unit = side.setdefault((row["host"], row["scenario"], row["seed"]), {
            "host": row["host"],
            "unit": f"{row['scenario']}/{row['seed']}",
            "label": row["label"],
            "overload": row["overload"],
        })
        choices = unit.setdefault(row["controller"], {})
        choices[row["choice"]] = choices.get(row["choice"], 0) + 1
    return {
        "objective": "objective-v2",
        "evidence": EVIDENCE,
        "post_hoc": POST_HOC,
        "controllers": table,
        "cuts_too_much": sorted(cuts.values(), key=lambda e: (CONTROLLERS.index(e["controller"]), e["host"], e["split"], e["unit"])),
        "laya_side_by_side": [side[key] for key in sorted(side)],
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="aik_controller.compare")
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[3])
    parser.add_argument("--holdout", action="store_true", help="also score holdout-v1 labels")
    parser.add_argument("--out", type=Path)
    args = parser.parse_args(argv)
    read = SPLITS + (("holdout",) if args.holdout else ())
    try:
        result = report(decisions(args.root, read))
    except (fit.FitError, splits.SplitError, holdout.HoldoutError) as err:
        print(f"COMPARE_REFUSED {err}", file=sys.stderr)
        return 2
    if args.out:
        args.out.write_text(json.dumps(result, indent=1, sort_keys=True) + "\n")
    for cid, by_split in result["controllers"].items():
        for split, entry in by_split.items():
            s = entry["pooled"]
            print(
                f"{cid:18s} {split:20s} {entry['evidence']:8s} hits={s['hits']}/{s['decisions']} "
                f"infeasible={s['infeasible']} cuts_batch={s['cuts_batch']} min_batch={s['min_batch_retained_bp'] / 100:.1f}% "
                f"overload_batch={'-' if s['overload_mean_batch_retained_bp'] is None else f'{s['overload_mean_batch_retained_bp'] / 100:.1f}%'} "
                f"under_cap_hits={s['under_capacity_hits']}/{s['under_capacity_decisions']}"
            )
    return 0


if __name__ == "__main__":
    sys.exit(main())

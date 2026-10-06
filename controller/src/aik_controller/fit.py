"""Fit and select a CPU-profile controller on branched labels, under fit-v0.

A fit reads the training labels of one host. Selection reads the development
labels of that same host. Each usable branch that a unit's label reads is one
decision: that branch's own pre-decision state, scored against the unit's
label set and its measured profile summaries. Every branch of a unit runs its
prefix under balanced, so the branches are repeated draws of the state the
controller would see at that decision point.

`zero-shot` runs the pinned Laya checkpoint on those states. It needs the Laya
environment. `run` does not import Laya; it reads the zero-shot report.
`freeze` writes the selected candidate, and `calibrate` scores that frozen
candidate once on the calibration labels of the same host. `evaluate` reads
the final test and the out-of-distribution subset together, once, for a
committed and calibrated candidate. Nothing else here reads them.
"""

import argparse
import hashlib
import json
import math
import platform
import subprocess
import sys
import time
from pathlib import Path

from aik_controller import branches, jobs, splits
from aik_controller.features import PROFILES, encode, load_edges
from aik_controller.heuristic import choose, load_thresholds

FIT_ID = "fit-v0"
KINDS = ("fixed", "heuristic", "threshold", "logistic", "laya")
ABSTAIN_KINDS = ("truncated", "outside", "missing")
FALLBACK = "balanced"


class FitError(ValueError):
    pass


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load_config(root: Path) -> dict:
    doc = json.loads((root / "configs" / f"{FIT_ID}.json").read_text())
    if doc.get("id") != FIT_ID or doc.get("manifest") != splits.CURRENT:
        raise FitError(f"configs/{FIT_ID}.json is not {FIT_ID} over {splits.CURRENT}")
    if doc.get("fit_reads") != ["training"] or doc.get("selection_reads") != ["development"]:
        raise FitError("a fit reads training and selection reads development")
    ids = [candidate["id"] for candidate in doc["candidates"]]
    if len(set(ids)) != len(ids):
        raise FitError("candidate ids repeat")
    for candidate in doc["candidates"]:
        if candidate.get("kind") not in KINDS:
            raise FitError(f"candidate {candidate['id']} has an unknown kind")
        if candidate["kind"] == "fixed" and candidate.get("profile") not in PROFILES:
            raise FitError(f"candidate {candidate['id']} is not a catalog profile")
    return doc


# Pre-decision features. Only the snapshot the guest sends enters a candidate.

def _latency(snapshot: dict) -> dict:
    return next(group for group in snapshot["groups"] if group["class"] == "latency")


def pending(snapshot: dict) -> int:
    return _latency(snapshot)["queue_len"]


FEATURES = {
    "log1p_pending": lambda snap: math.log1p(pending(snap)),
    "log1p_wait_ms": lambda snap: math.log1p(_latency(snap)["max_wait_us"] / 1000),
    "latency_share": lambda snap: _latency(snap)["cpu_service_us"] / max(snap["window_us"], 1),
}


def _first_best(counts: dict[str, int]) -> str:
    best = max(counts.values())
    return next(profile for profile in PROFILES if counts[profile] == best)


def _side(examples: list[tuple[int, list[str]]]) -> tuple[str, int]:
    """The profile in the most label sets of these examples, and its hit count."""
    counts = {profile: sum(profile in label for _, label in examples) for profile in PROFILES}
    profile = _first_best(counts)
    return profile, counts[profile]


def fit_threshold(examples: list[tuple[int, list[str]]]) -> dict:
    """One cut on pending jobs. `examples` are (pending, label set) pairs."""
    values = sorted({value for value, _ in examples})
    if len(values) < 2:
        raise FitError("a threshold needs at least two distinct training values")
    best = None
    for low, high in zip(values, values[1:]):
        cut = (low + high) / 2
        below_profile, below_hits = _side([e for e in examples if e[0] < cut])
        above_profile, above_hits = _side([e for e in examples if e[0] >= cut])
        key = (below_hits + above_hits, high - low, -cut)
        if best is None or key > best[0]:
            best = (key, {
                "cut": cut,
                "below": below_profile,
                "above": above_profile,
                "gap": [low, high],
                "training_hits": below_hits + above_hits,
                "examples": len(examples),
            })
    return best[1]


def threshold_choice(params: dict, snapshot: dict) -> str:
    return params["above"] if pending(snapshot) >= params["cut"] else params["below"]


def _softmax(logits: list[float]) -> list[float]:
    top = max(logits)
    exps = [math.exp(value - top) for value in logits]
    total = sum(exps)
    return [value / total for value in exps]


def _targets(label: list[str]) -> list[float]:
    return [1 / len(label) if profile in label else 0.0 for profile in PROFILES]


def fit_logistic(rows: list[list[float]], labels: list[list[str]], spec: dict) -> dict:
    """Multinomial logistic regression with soft targets and full-batch gradient descent."""
    n, width = len(rows), len(rows[0])
    mean = [sum(row[j] for row in rows) / n for j in range(width)]
    std = []
    for j in range(width):
        deviation = math.sqrt(sum((row[j] - mean[j]) ** 2 for row in rows) / n)
        std.append(deviation if deviation > 0 else 1.0)
    xs = [[(row[j] - mean[j]) / std[j] for j in range(width)] for row in rows]
    ys = [_targets(label) for label in labels]
    k = len(PROFILES)
    weights = [[0.0] * width for _ in range(k)]
    bias = [0.0] * k
    rate, l2 = spec["learning_rate"], spec["l2"]
    for _ in range(spec["steps"]):
        grad_w = [[0.0] * width for _ in range(k)]
        grad_b = [0.0] * k
        for x, y in zip(xs, ys):
            probs = _softmax([bias[c] + sum(w * v for w, v in zip(weights[c], x)) for c in range(k)])
            for c in range(k):
                error = probs[c] - y[c]
                grad_b[c] += error
                for j in range(width):
                    grad_w[c][j] += error * x[j]
        for c in range(k):
            bias[c] -= rate * grad_b[c] / n
            for j in range(width):
                weights[c][j] -= rate * (grad_w[c][j] / n + l2 * weights[c][j])
    loss = 0.0
    for x, y in zip(xs, ys):
        probs = _softmax([bias[c] + sum(w * v for w, v in zip(weights[c], x)) for c in range(k)])
        loss -= sum(t * math.log(max(p, 1e-300)) for t, p in zip(y, probs) if t > 0)
    loss = loss / n + l2 / 2 * sum(w * w for row in weights for w in row)
    return {
        "features": list(spec["features"]),
        "profiles": list(PROFILES),
        "mean": mean,
        "std": std,
        "weights": weights,
        "bias": bias,
        "training_loss": loss,
        "examples": n,
    }


def logistic_probabilities(params: dict, snapshot: dict) -> list[float]:
    x = [
        (FEATURES[name](snapshot) - params["mean"][j]) / params["std"][j]
        for j, name in enumerate(params["features"])
    ]
    return _softmax([
        params["bias"][c] + sum(w * v for w, v in zip(params["weights"][c], x))
        for c in range(len(params["profiles"]))
    ])


def logistic_choice(params: dict, snapshot: dict) -> str:
    probs = logistic_probabilities(params, snapshot)
    best = max(probs)
    return next(profile for profile, p in zip(params["profiles"], probs) if p == best)


# Labels and the branches they read.

def _branch_key(record: dict) -> str:
    return f"{record['scenario']}/{record['seed']}/{record['profile']}/r{record['repeat']}/a{record['attempt']}"


LABEL_CHECKS = {
    "training": splits.check_fit_labels,
    "development": splits.check_selection_labels,
    "calibration": splits.check_calibration_labels,
    "final_test": lambda doc, root: splits.check_sealed_labels(doc, root, "final_test"),
    "out_of_distribution": lambda doc, root: splits.check_sealed_labels(doc, root, "out_of_distribution"),
}


def load_labels(root: Path, path: Path, split: str) -> dict:
    """One labels file with the branches its units read. The file must reproduce from its records."""
    doc = json.loads(path.read_text())
    if split not in LABEL_CHECKS:
        raise FitError(f"{split} is not a split of {splits.CURRENT}")
    LABEL_CHECKS[split](doc, root)
    records_path = path.parent / doc["records"]
    if not records_path.is_file() or _sha256(records_path) != doc["records_sha256"]:
        raise FitError(f"{records_path.name} does not match {path.name}")
    records = [json.loads(line) for line in records_path.read_text().splitlines() if line.strip()]
    manifest = splits.load_branched(root)
    family = jobs.load_family(splits.family_path(root, manifest))
    objective = json.loads(splits.objective_path(root, manifest).read_text())
    groups = branches.usable_units(records, family, objective)
    units = []
    for unit in doc["units"]:
        group = groups.get((unit["scenario"], unit["seed"]))
        if group is None:
            raise FitError(f"{unit['scenario']}/{unit['seed']} has no usable records")
        again = branches.label_unit(group, objective)
        if (again["status"], again["label"]) != (unit["status"], unit["label"]):
            raise FitError(f"{unit['scenario']}/{unit['seed']} does not reproduce its label")
        kept = [record for rows in branches.kept_branches(group, objective).values() for record in rows]
        units.append({
            "scenario": unit["scenario"],
            "seed": unit["seed"],
            "status": unit["status"],
            "label": unit["label"],
            "best": unit.get("best"),
            "profiles": unit["profiles"],
            "branches": [{"key": _branch_key(r), "snapshot": branches.snapshot(r)} for r in kept],
        })
    return {
        "labels": path.name,
        "labels_sha256": _sha256(path),
        "records": records_path.name,
        "records_sha256": doc["records_sha256"],
        "split": split,
        "host_cpu": doc["host_cpu"],
        "kvm_module": doc["kvm_module"],
        "units": units,
    }


def _gather(root: Path, paths: list[Path], split: str) -> list[dict]:
    docs = [load_labels(root, path, split) for path in paths]
    seen = set()
    for doc in docs:
        for unit in doc["units"]:
            key = (unit["scenario"], unit["seed"])
            if key in seen:
                raise FitError(f"unit {key[0]}/{key[1]} appears in more than one {split} file")
            seen.add(key)
    return docs


def _files(docs: list[dict]) -> list[dict]:
    return [
        {key: doc[key] for key in ("labels", "labels_sha256", "records", "records_sha256")} | {"units": len(doc["units"])}
        for doc in docs
    ]


def _host(docs: list[dict]) -> tuple[str, str]:
    hosts = {(doc["host_cpu"], doc["kvm_module"]) for doc in docs}
    if len(hosts) != 1:
        raise FitError(f"labels mix hosts {sorted(hosts)}; fit and select one host at a time")
    return hosts.pop()


def decisions(docs: list[dict], config: dict) -> tuple[list[tuple[dict, dict]], list[dict]]:
    """(unit, branch) pairs from scored units, and the units left unscored."""
    scored, left = [], []
    for doc in docs:
        for unit in doc["units"]:
            if unit["status"] in config["scored_status"]:
                scored.extend((unit, branch) for branch in unit["branches"])
            else:
                left.append({"scenario": unit["scenario"], "seed": unit["seed"], "status": unit["status"]})
    return scored, left


def rule(kind: str, params: dict, thresholds: dict):
    """The offline decision rule of a fixed, heuristic, threshold, or logistic candidate."""
    if kind == "fixed":
        return lambda snap: params["profile"]
    if kind == "heuristic":
        return lambda snap: choose(snap, thresholds)
    if kind == "threshold":
        return lambda snap: threshold_choice(params, snap)
    if kind == "logistic":
        return lambda snap: logistic_choice(params, snap)
    raise FitError(f"a {kind} candidate has no offline rule")


# Scoring and selection.

def score(choices: list[tuple[dict, str, bool]]) -> dict:
    """`choices` are (unit, profile, abstained) per decision."""
    units: dict[tuple[str, int], dict] = {}
    for unit, profile, abstained in choices:
        key = (unit["scenario"], unit["seed"])
        row = units.setdefault(key, {
            "scenario": unit["scenario"],
            "seed": unit["seed"],
            "label": unit["label"],
            "decisions": 0,
            "hits": 0,
            "infeasible": 0,
            "abstain": 0,
            "choices": {},
            "p99_regret_us": [],
        })
        summary = unit["profiles"][profile]
        row["decisions"] += 1
        row["hits"] += profile in unit["label"]
        row["infeasible"] += not summary["feasible"]
        row["abstain"] += abstained
        row["choices"][profile] = row["choices"].get(profile, 0) + 1
        best = unit["profiles"][unit["best"]]["p99_median"]
        if summary["p99_median"] is not None and best is not None:
            row["p99_regret_us"].append(summary["p99_median"] - best)
    rows = [units[key] for key in sorted(units)]
    for row in rows:
        regrets = sorted(row.pop("p99_regret_us"))
        row["p99_regret_max_us"] = regrets[-1] if regrets else None
    total = {name: sum(row[name] for row in rows) for name in ("decisions", "hits", "infeasible", "abstain")}
    total["misses"] = total["decisions"] - total["hits"]
    return {**total, "units": rows}


def select(config: dict, development: dict[str, dict]) -> str:
    """Fewest infeasible, then fewest misses. Ties go to the earlier, simpler candidate."""
    order = [candidate["id"] for candidate in config["candidates"]]
    missing = [cid for cid in order if development.get(cid) is None]
    if missing:
        raise FitError(f"selection needs every declared candidate; not scored: {', '.join(missing)}")
    return min(order, key=lambda cid: (development[cid]["infeasible"], development[cid]["misses"], order.index(cid)))


def _zero_shot_choices(report: dict, development: list[dict]) -> dict[str, tuple[str, bool]]:
    shas = sorted(doc["labels_sha256"] for doc in development)
    if sorted(report.get("labels_sha256", [])) != shas:
        raise FitError("the zero-shot report was not run on these development labels")
    if report.get("device") != "cpu" or report.get("dtype") != "torch.float32":
        raise FitError("the zero-shot report is not the qualified cpu float32 backend")
    out = {}
    for row in report["rows"]:
        if row["kind"] in ABSTAIN_KINDS:
            out[row["key"]] = (FALLBACK, True)
        else:
            out[row["key"]] = (row["choice"], False)
    return out


def run(root: Path, training: list[Path], development: list[Path], zero_shot: Path | None) -> dict:
    config = load_config(root)
    train_docs = _gather(root, training, "training")
    dev_docs = _gather(root, development, "development") if development else []
    host_cpu, kvm_module = _host(train_docs + dev_docs)
    train, train_left = decisions(train_docs, config)
    if not train:
        raise FitError("no scored training unit")
    thresholds = load_thresholds(root / "configs" / "heuristic-v0.json")
    fitted: dict[str, dict] = {}
    rules = {}
    for candidate in config["candidates"]:
        cid, kind = candidate["id"], candidate["kind"]
        if kind == "fixed":
            rules[cid] = rule(kind, {"profile": candidate["profile"]}, thresholds)
        elif kind == "heuristic":
            rules[cid] = rule(kind, {}, thresholds)
        elif kind == "threshold":
            fitted[cid] = fit_threshold([(pending(b["snapshot"]), u["label"]) for u, b in train])
            rules[cid] = rule(kind, fitted[cid], thresholds)
        elif kind == "logistic":
            fitted[cid] = fit_logistic(
                [[FEATURES[name](b["snapshot"]) for name in candidate["features"]] for _, b in train],
                [u["label"] for u, _ in train],
                candidate,
            )
            rules[cid] = rule(kind, fitted[cid], thresholds)
    laya_choices = None
    zero_shot_meta = None
    if zero_shot is not None:
        report = json.loads(zero_shot.read_text())
        laya_choices = _zero_shot_choices(report, dev_docs)
        zero_shot_meta = {
            key: report.get(key)
            for key in ("revision", "device", "dtype", "torch", "laya", "python", "host_cpu", "torch_threads")
        }
        zero_shot_meta["report"] = zero_shot.name
        zero_shot_meta["report_sha256"] = _sha256(zero_shot)
    dev, dev_left = decisions(dev_docs, config)
    scores = {}
    for candidate in config["candidates"]:
        cid = candidate["id"]
        entry = {"training": None, "development": None}
        if candidate["kind"] == "laya":
            if laya_choices is not None:
                rows = []
                for unit, branch in dev:
                    if branch["key"] not in laya_choices:
                        raise FitError(f"the zero-shot report has no row for {branch['key']}")
                    profile, abstained = laya_choices[branch["key"]]
                    rows.append((unit, profile, abstained))
                entry["development"] = score(rows)
        else:
            choice = rules[cid]
            entry["training"] = score([(u, choice(b["snapshot"]), False) for u, b in train])
            if dev:
                entry["development"] = score([(u, choice(b["snapshot"]), False) for u, b in dev])
        scores[cid] = entry
    selected = select(config, {cid: s["development"] for cid, s in scores.items()}) if dev else None
    manifest_path = splits.branched_path(root)
    manifest = splits.load_branched(root, manifest_path)
    return {
        "fit": FIT_ID,
        "fit_sha256": _sha256(root / "configs" / f"{FIT_ID}.json"),
        "heuristic_sha256": _sha256(root / "configs" / "heuristic-v0.json"),
        "manifest": manifest["id"],
        "manifest_sha256": splits.manifest_sha256(manifest_path),
        "objective": manifest["objective"],
        "objective_sha256": manifest["objective_sha256"],
        "host_cpu": host_cpu,
        "kvm_module": kvm_module,
        "training": _files(train_docs),
        "development": _files(dev_docs) if dev_docs else None,
        "decisions": {"training": len(train), "development": len(dev)},
        "unscored": {"training": train_left, "development": dev_left},
        "fitted": fitted,
        "zero_shot": zero_shot_meta,
        "scores": scores,
        "selected": selected,
    }


CANDIDATE_ID = "candidate-v0"


def freeze(root: Path, report_path: Path) -> dict:
    """Freeze the candidate a fit report selected on development. Nothing here reads calibration."""
    report = json.loads(report_path.read_text())
    config = load_config(root)
    if report.get("fit") != FIT_ID or report.get("fit_sha256") != _sha256(root / "configs" / f"{FIT_ID}.json"):
        raise FitError(f"the report was not produced under the current {FIT_ID}")
    if report.get("manifest_sha256") != splits.manifest_sha256(splits.branched_path(root)):
        raise FitError("the report was produced under a different split manifest")
    selected = report.get("selected")
    if selected is None:
        raise FitError("the report selected no candidate; select on development first")
    spec = next(candidate for candidate in config["candidates"] if candidate["id"] == selected)
    kind = spec["kind"]
    if kind == "laya":
        raise FitError(f"{FIT_ID} does not freeze a Laya candidate; its calibration needs a declared calibrator")
    if kind == "fixed":
        params = {"profile": spec["profile"]}
    elif kind == "heuristic":
        params = {"config": spec["config"], "sha256": report["heuristic_sha256"]}
    else:
        params = report["fitted"][selected]
    development = report["scores"][selected]["development"]
    return {
        "id": CANDIDATE_ID,
        "selected": selected,
        "kind": kind,
        "params": params,
        "fit": FIT_ID,
        "fit_sha256": report["fit_sha256"],
        "fit_report": report_path.name,
        "fit_report_sha256": _sha256(report_path),
        "manifest": report["manifest"],
        "manifest_sha256": report["manifest_sha256"],
        "objective": report["objective"],
        "objective_sha256": report["objective_sha256"],
        "host_cpu": report["host_cpu"],
        "kvm_module": report["kvm_module"],
        "development": {key: development[key] for key in ("decisions", "hits", "misses", "infeasible", "abstain")},
        "note": "Frozen from the development selection before any calibration label was read.",
    }


def wilson(k: int, n: int, z: float = 1.96) -> list[float] | None:
    """Wilson score interval for k of n independent units."""
    if n == 0:
        return None
    p = k / n
    denom = 1 + z * z / n
    centre = (p + z * z / (2 * n)) / denom
    half = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / denom
    return [max(0.0, centre - half), min(1.0, centre + half)]


def _candidate(root: Path, path: Path) -> dict:
    candidate = json.loads(path.read_text())
    if candidate.get("id") != CANDIDATE_ID:
        raise FitError(f"{path.name} is not a {CANDIDATE_ID} file")
    if candidate.get("manifest_sha256") != splits.manifest_sha256(splits.branched_path(root)):
        raise FitError("the candidate was frozen under a different split manifest")
    if candidate["kind"] == "heuristic" and candidate["params"]["sha256"] != _sha256(root / "configs" / "heuristic-v0.json"):
        raise FitError("heuristic-v0 changed after the candidate was frozen")
    return candidate


def _units(result: dict) -> dict:
    """Unit-level counts with Wilson intervals. Branches of one unit are not independent."""
    units = len(result["units"])
    all_hit = sum(row["hits"] == row["decisions"] for row in result["units"])
    feasible = sum(row["infeasible"] == 0 for row in result["units"])
    return {
        "units": units,
        "units_all_hit": all_hit,
        "units_all_hit_wilson95": wilson(all_hit, units),
        "units_feasible": feasible,
        "units_feasible_wilson95": wilson(feasible, units),
    }


def calibrate(root: Path, candidate_path: Path, paths: list[Path]) -> dict:
    """Score a frozen candidate once on calibration labels from its own host.

    A deterministic candidate has no confidence to rescale, so this is its
    error estimate on independent units before the sealed test opens.
    """
    candidate = _candidate(root, candidate_path)
    config = load_config(root)
    docs = _gather(root, paths, "calibration")
    if _host(docs) != (candidate["host_cpu"], candidate["kvm_module"]):
        raise FitError("calibration labels come from a different host than the candidate's fit")
    thresholds = load_thresholds(root / "configs" / "heuristic-v0.json")
    choice = rule(candidate["kind"], candidate["params"], thresholds)
    scored, left = decisions(docs, config)
    if not scored:
        raise FitError("no scored calibration unit")
    result = score([(unit, choice(branch["snapshot"]), False) for unit, branch in scored])
    return {
        "candidate": candidate["id"],
        "selected": candidate["selected"],
        "candidate_file": candidate_path.name,
        "candidate_sha256": _sha256(candidate_path),
        "host_cpu": candidate["host_cpu"],
        "kvm_module": candidate["kvm_module"],
        "calibration": _files(docs),
        "unscored": left,
        "score": result,
        **_units(result),
    }


SEALED_GROUPS = ("final_test", "final_test_seen", "final_test_unseen", "out_of_distribution")


def git_committed(root: Path, path: Path) -> bool:
    """True when `path` is tracked and matches HEAD."""
    try:
        rel = str(path.resolve().relative_to(root.resolve()))
    except ValueError:
        return False
    tracked = subprocess.run(["git", "-C", str(root), "ls-files", "--error-unmatch", "--", rel], capture_output=True)
    clean = subprocess.run(["git", "-C", str(root), "diff", "--quiet", "HEAD", "--", rel], capture_output=True)
    return tracked.returncode == 0 and clean.returncode == 0


def _complete(root: Path, docs: list[dict], split: str) -> None:
    """Every unit the manifest plans for `split`, and no other."""
    planned = {(u["scenario"], u["seed"]) for u in splits.units(splits.load_branched(root), split, sealed_evaluation=True)}
    present = {(u["scenario"], u["seed"]) for doc in docs for u in doc["units"]}
    if present != planned:
        missing = ", ".join(f"{s}/{n}" for s, n in sorted(planned - present)) or "none"
        extra = ", ".join(f"{s}/{n}" for s, n in sorted(present - planned)) or "none"
        raise FitError(f"{split} labels must cover the split exactly; missing {missing}; extra {extra}")


def sealed_scores(rules: dict, docs: dict[str, list[dict]], config: dict, unseen: list[str]) -> tuple[dict, list[dict]]:
    """Every rule on every scored sealed decision, by group, with unit-level counts."""
    final, final_left = decisions(docs["final_test"], config)
    ood, ood_left = decisions(docs["out_of_distribution"], config)
    groups = {
        "final_test": final,
        "final_test_seen": [(u, b) for u, b in final if u["scenario"] not in unseen],
        "final_test_unseen": [(u, b) for u, b in final if u["scenario"] in unseen],
        "out_of_distribution": ood,
    }
    scores = {}
    for cid, choice in rules.items():
        scores[cid] = {}
        for group, scored in groups.items():
            if not scored:
                scores[cid][group] = None
                continue
            result = score([(unit, choice(branch["snapshot"]), False) for unit, branch in scored])
            scores[cid][group] = {**result, "unit_level": _units(result)}
    left = [dict(row, split="final_test") for row in final_left] + [dict(row, split="out_of_distribution") for row in ood_left]
    return scores, left


def evaluate(
    root: Path,
    candidate_path: Path,
    calibration_path: Path,
    fit_report_path: Path,
    final_test: list[Path],
    out_of_distribution: list[Path],
    committed=git_committed,
) -> dict:
    """Score a committed, calibrated candidate once on both sealed splits of its own host.

    The fixed profiles and heuristic-v0 are scored on the same decisions. So is
    every other fitted candidate in the fit report. Those are reported and not
    selectable: a change made after reading this needs a new untouched test.
    """
    candidate = _candidate(root, candidate_path)
    for path in (candidate_path, calibration_path):
        if not committed(root, path):
            raise FitError(f"{path.name} must be committed before a sealed split is read")
    calibration = json.loads(calibration_path.read_text())
    if calibration.get("candidate_sha256") != _sha256(candidate_path):
        raise FitError("the calibration report does not score this candidate")
    if _sha256(fit_report_path) != candidate["fit_report_sha256"]:
        raise FitError("the fit report is not the one this candidate was frozen from")
    report = json.loads(fit_report_path.read_text())
    config = load_config(root)
    manifest = splits.load_branched(root)
    docs = {
        "final_test": _gather(root, final_test, "final_test"),
        "out_of_distribution": _gather(root, out_of_distribution, "out_of_distribution"),
    }
    for split, split_docs in docs.items():
        _complete(root, split_docs, split)
    if _host(docs["final_test"] + docs["out_of_distribution"]) != (candidate["host_cpu"], candidate["kvm_module"]):
        raise FitError("sealed labels come from a different host than the candidate's fit")
    thresholds = load_thresholds(root / "configs" / "heuristic-v0.json")
    rules = {CANDIDATE_ID: rule(candidate["kind"], candidate["params"], thresholds)}
    comparators = []
    for spec in config["candidates"]:
        cid, kind = spec["id"], spec["kind"]
        if kind == "fixed":
            rules[cid] = rule(kind, {"profile": spec["profile"]}, thresholds)
        elif kind == "heuristic":
            rules[cid] = rule(kind, {}, thresholds)
        elif cid in report["fitted"] and cid != candidate["selected"]:
            rules[cid] = rule(kind, report["fitted"][cid], thresholds)
        else:
            continue
        comparators.append(cid)
    scores, left = sealed_scores(rules, docs, config, manifest["unseen_in_final_test"])
    return {
        "candidate": candidate["id"],
        "selected": candidate["selected"],
        "candidate_file": candidate_path.name,
        "candidate_sha256": _sha256(candidate_path),
        "calibration_report": calibration_path.name,
        "calibration_sha256": _sha256(calibration_path),
        "fit_report": fit_report_path.name,
        "fit_report_sha256": candidate["fit_report_sha256"],
        "manifest": manifest["id"],
        "manifest_sha256": candidate["manifest_sha256"],
        "host_cpu": candidate["host_cpu"],
        "kvm_module": candidate["kvm_module"],
        "unseen_in_final_test": manifest["unseen_in_final_test"],
        "final_test": _files(docs["final_test"]),
        "out_of_distribution": _files(docs["out_of_distribution"]),
        "comparators": comparators,
        "unscored": left,
        "scores": scores,
    }


def _line(cid: str, split: str, entry: dict | None) -> str:
    if entry is None:
        return f"SCORE id={cid} split={split} not_scored"
    return (
        f"SCORE id={cid} split={split} decisions={entry['decisions']} hits={entry['hits']} "
        f"misses={entry['misses']} infeasible={entry['infeasible']} abstain={entry['abstain']}"
    )


def zero_shot_rows(agent, root: Path, docs: list[dict], clock=time.perf_counter) -> list[dict]:
    """One discarded warmup forward, then one forward per branch of every unit."""
    from aik_controller.laya_offline import classify, model_state, resource_question

    edges = load_edges(root / "configs" / "features-v0.json")
    thresholds = load_thresholds(root / "configs" / "heuristic-v0.json")
    question = resource_question()
    every = [(unit, branch) for doc in docs for unit in doc["units"] for branch in unit["branches"]]
    agent.predict(model_state(encode(every[0][1]["snapshot"], edges), include_profile=True), question)
    rows = []
    for unit, branch in every:
        snap = branch["snapshot"]
        state = model_state(encode(snap, edges), include_profile=True)
        started = clock()
        result = agent.predict(state, question)
        elapsed = int((clock() - started) * 1_000_000)
        row = classify(result, choose(snap, thresholds))
        row.update(key=branch["key"], status=unit["status"], label=unit["label"], forward_us=elapsed)
        rows.append(row)
        print(
            f"ZERO_SHOT key={row['key']} laya={row['choice']} kind={row['kind']} "
            f"label={','.join(unit['label'])} confidence={row['answer_confidence']} us={elapsed}",
            flush=True,
        )
    return rows


def run_zero_shot(root: Path, development: list[Path], out: Path) -> dict:
    """Score every development decision with the pinned checkpoint. Nothing is staged."""
    from aik_controller.laya_pin import assess, checkpoint_dir, load_pin
    from aik_controller.laya_timing import host_cpu

    config = load_config(root)
    docs = _gather(root, development, "development")
    _host(docs)
    directory = checkpoint_dir(root)
    status = assess(root, directory)
    if status != "ready":
        raise FitError(f"checkpoint {status}; refusing to load")
    import laya
    import torch

    agent = laya.load(str(directory), device="cpu")
    device, dtype = str(getattr(agent, "device", "unknown")), str(getattr(agent, "dtype", "unknown"))
    if device != "cpu" or dtype != "torch.float32":
        raise FitError(f"backend is {device} {dtype}, not the qualified cpu torch.float32")
    rows = zero_shot_rows(agent, root, docs)
    report = {
        "fit": config["id"],
        "labels": [doc["labels"] for doc in docs],
        "labels_sha256": [doc["labels_sha256"] for doc in docs],
        "revision": load_pin(root / "configs" / "laya-pin.json")["revision"],
        "device": device,
        "dtype": dtype,
        "torch": torch.__version__,
        "torch_threads": torch.get_num_threads(),
        "laya": getattr(laya, "__version__", "unknown"),
        "python": platform.python_version(),
        "host_cpu": host_cpu(),
        "profile_field": "present",
        "rows": rows,
    }
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(report, indent=1, sort_keys=True) + "\n")
    return report


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="aik_controller.fit")
    sub = parser.add_subparsers(dest="command", required=True)
    fit = sub.add_parser("run")
    fit.add_argument("--training", action="append", required=True, type=Path)
    fit.add_argument("--development", action="append", default=[], type=Path)
    fit.add_argument("--zero-shot", type=Path)
    fit.add_argument("--out", required=True, type=Path)
    shot = sub.add_parser("zero-shot")
    shot.add_argument("--development", action="append", required=True, type=Path)
    shot.add_argument("--out", required=True, type=Path)
    hold = sub.add_parser("freeze")
    hold.add_argument("--report", required=True, type=Path)
    hold.add_argument("--out", required=True, type=Path)
    cal = sub.add_parser("calibrate")
    cal.add_argument("--candidate", required=True, type=Path)
    cal.add_argument("--calibration", action="append", required=True, type=Path)
    cal.add_argument("--out", required=True, type=Path)
    seal = sub.add_parser("evaluate")
    seal.add_argument("--candidate", required=True, type=Path)
    seal.add_argument("--calibration-report", required=True, type=Path)
    seal.add_argument("--fit-report", required=True, type=Path)
    seal.add_argument("--final-test", action="append", required=True, type=Path)
    seal.add_argument("--out-of-distribution", action="append", required=True, type=Path)
    seal.add_argument("--out", required=True, type=Path)
    args = parser.parse_args(argv)
    root = Path(__file__).resolve().parents[3]
    try:
        if args.command == "evaluate":
            if args.out.exists():
                raise FitError(f"{args.out} already exists; a sealed evaluation runs once")
            doc = evaluate(
                root, args.candidate, args.calibration_report, args.fit_report,
                args.final_test, args.out_of_distribution,
            )
            args.out.parent.mkdir(parents=True, exist_ok=True)
            args.out.write_text(json.dumps(doc, indent=1, sort_keys=True) + "\n")
            for cid, groups in doc["scores"].items():
                for group in SEALED_GROUPS:
                    print(_line(cid, group, groups[group]))
            for group in ("final_test", "out_of_distribution"):
                entry = doc["scores"][CANDIDATE_ID][group]
                for row in entry["units"] if entry else []:
                    print(
                        f"UNIT group={group} {row['scenario']}/{row['seed']} label={','.join(row['label'])} "
                        f"hits={row['hits']}/{row['decisions']} infeasible={row['infeasible']} "
                        f"choices={json.dumps(row['choices'], sort_keys=True)}"
                    )
            for unit in doc["unscored"]:
                print(f"UNSCORED split={unit['split']} unit={unit['scenario']}/{unit['seed']} status={unit['status']}")
            for group in SEALED_GROUPS:
                entry = doc["scores"][CANDIDATE_ID][group]
                if entry:
                    level = entry["unit_level"]
                    print(
                        f"EVALUATION group={group} units={level['units']} all_hit={level['units_all_hit']} "
                        f"wilson95={level['units_all_hit_wilson95']} feasible={level['units_feasible']} "
                        f"wilson95={level['units_feasible_wilson95']}"
                    )
            print(f"EVALUATION_SHA256 {_sha256(args.out)}")
            return 0
        if args.command == "freeze":
            if args.out.exists():
                raise FitError(f"{args.out} already exists; a frozen candidate is not overwritten")
            doc = freeze(root, args.report)
            args.out.parent.mkdir(parents=True, exist_ok=True)
            args.out.write_text(json.dumps(doc, indent=1, sort_keys=True) + "\n")
            print(f"FROZEN id={doc['id']} selected={doc['selected']} kind={doc['kind']} params={json.dumps(doc['params'], sort_keys=True)}")
            print(f"CANDIDATE_SHA256 {_sha256(args.out)}")
            return 0
        if args.command == "calibrate":
            doc = calibrate(root, args.candidate, args.calibration)
            args.out.parent.mkdir(parents=True, exist_ok=True)
            args.out.write_text(json.dumps(doc, indent=1, sort_keys=True) + "\n")
            print(_line(doc["selected"], "calibration", doc["score"]))
            for row in doc["score"]["units"]:
                print(
                    f"UNIT {row['scenario']}/{row['seed']} label={','.join(row['label'])} "
                    f"hits={row['hits']}/{row['decisions']} infeasible={row['infeasible']} "
                    f"choices={json.dumps(row['choices'], sort_keys=True)}"
                )
            for unit in doc["unscored"]:
                print(f"UNSCORED split=calibration unit={unit['scenario']}/{unit['seed']} status={unit['status']}")
            print(
                f"CALIBRATION units={doc['units']} all_hit={doc['units_all_hit']} "
                f"wilson95={doc['units_all_hit_wilson95']} feasible={doc['units_feasible']} "
                f"wilson95={doc['units_feasible_wilson95']}"
            )
            print(f"CALIBRATION_SHA256 {_sha256(args.out)}")
            return 0
        if args.command == "zero-shot":
            report = run_zero_shot(root, args.development, args.out)
            kinds = {}
            for row in report["rows"]:
                kinds[row["kind"]] = kinds.get(row["kind"], 0) + 1
            print("ZERO_SHOT_DONE " + " ".join(f"{k}={v}" for k, v in sorted(kinds.items())), flush=True)
            return 0
        if args.zero_shot is not None and not args.development:
            raise FitError("a zero-shot report scores development decisions; pass --development")
        report = run(root, args.training, args.development, args.zero_shot)
    except (FitError, splits.SplitError, branches.BranchError) as err:
        print(f"FIT_REFUSED {err}", file=sys.stderr)
        return 2
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(report, indent=1, sort_keys=True) + "\n")
    for cid, params in report["fitted"].items():
        if "cut" in params:
            print(
                f"FIT id={cid} cut={params['cut']} below={params['below']} above={params['above']} "
                f"gap={params['gap'][0]}..{params['gap'][1]} training_hits={params['training_hits']}/{params['examples']}"
            )
        else:
            print(f"FIT id={cid} loss={params['training_loss']:.6f} examples={params['examples']}")
    for cid, entry in report["scores"].items():
        print(_line(cid, "training", entry["training"]))
        print(_line(cid, "development", entry["development"]))
    for split, left in report["unscored"].items():
        for unit in left:
            print(f"UNSCORED split={split} unit={unit['scenario']}/{unit['seed']} status={unit['status']}")
    print(f"SELECTED {report['selected'] or 'pending development labels'}")
    print(f"FIT_SHA256 {_sha256(args.out)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

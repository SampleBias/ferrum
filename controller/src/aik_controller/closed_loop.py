"""Closed-loop branches: the frozen candidate chooses the horizon's profile.

A forced branch measures one profile. A closed-loop branch runs the same prefix
under balanced. At the decision point the guest applies the frozen candidate to
its own live pre-decision state and forces the profile it chose. Each record
carries that decision. The harness recomputes the choice from the recorded state
with the rule that fit scored offline and stops on any difference, so the guest
and the scorer cannot disagree silently.

`collect` boots closed-loop branches for one split on the candidate's own host,
into a records file of their own; `branches label` refuses those records. A
sealed split also needs --sealed-evaluation and a committed calibration report
for this candidate. `score` places each unit's closed-loop repeats beside the
same unit's forced branches under the frozen objective. The controller hits a
unit when its own repeats are feasible against balanced and reach the best
profile's range on the ranking metric.
"""

import argparse
import hashlib
import json
import statistics
import sys
from collections import Counter
from pathlib import Path

from aik_controller import branches, fit, splits

CONTROLLERS = {"threshold": "pending-threshold"}


class ClosedLoopError(ValueError):
    pass


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _key(record: dict) -> str:
    return f"{record['scenario']}/{record['seed']}/r{record['repeat']}/a{record['attempt']}"


def host_cpu() -> str:
    for line in Path("/proc/cpuinfo").read_text().splitlines():
        if line.startswith("model name"):
            return line.split(":", 1)[1].strip()
    return "unknown"


def guest_args(candidate: dict) -> str:
    """The guest arguments that carry a frozen candidate in place of --profile."""
    kind = candidate["kind"]
    if kind not in CONTROLLERS:
        raise ClosedLoopError(f"a {kind} candidate has no guest rule; the guest runs {', '.join(CONTROLLERS)}")
    params = candidate["params"]
    return (
        f"--candidate={CONTROLLERS[kind]} --cut={float(params['cut'])!r} "
        f"--below={params['below']} --above={params['above']}"
    )


def check_decision(record: dict, candidate: dict) -> str:
    """The guest's choice, after checking it against the offline rule on the recorded state."""
    decision = record.get("decision")
    if record.get("profile") != branches.CANDIDATE_PROFILE or decision is None:
        raise ClosedLoopError(f"{_key(record)} is not a candidate branch")
    params = candidate["params"]
    ran = (decision["controller"], float(decision["cut"]), decision["below"], decision["above"])
    if ran != (CONTROLLERS[candidate["kind"]], float(params["cut"]), params["below"], params["above"]):
        raise ClosedLoopError(f"{_key(record)} ran a different candidate")
    if decision["pending"] != record["state"]["latency_queue"]:
        raise ClosedLoopError(f"{_key(record)} decided on a state other than the one it printed")
    offline = fit.threshold_choice(params, branches.snapshot(record))
    if decision["choice"] != offline:
        raise ClosedLoopError(f"{_key(record)} chose {decision['choice']}; the offline rule chooses {offline}")
    return decision["choice"]


def _host(candidate: dict) -> tuple[str, str]:
    return candidate["host_cpu"], candidate["kvm_module"]


def _ready(
    root: Path,
    candidate_path: Path,
    split: str,
    calibration_path: Path | None,
    committed,
) -> None:
    """A closed loop reads a committed candidate. A sealed one also needs its committed calibration."""
    if not committed(root, candidate_path):
        raise ClosedLoopError(f"{candidate_path.name} must be committed before its closed loop runs")
    if split not in splits.SEALED:
        return
    if calibration_path is None:
        raise ClosedLoopError("a sealed closed loop needs --calibration-report")
    if not committed(root, calibration_path):
        raise ClosedLoopError(f"{calibration_path.name} must be committed before a sealed split is booted")
    if json.loads(calibration_path.read_text()).get("candidate_sha256") != _sha256(candidate_path):
        raise ClosedLoopError("the calibration report does not score this candidate")


def plan(units: list[dict], repeats: int) -> list[tuple[dict, int]]:
    """Repeat-major, so host drift spreads across units."""
    return [(unit, repeat) for repeat in range(repeats) for unit in units]


def _slot(record: dict) -> tuple:
    return (record["scenario"], record["seed"], record["repeat"])


def collect(
    args: argparse.Namespace,
    root: Path,
    boot=branches.boot,
    committed=fit.git_committed,
    this_host=host_cpu,
) -> int:
    candidate_path = Path(args.candidate)
    candidate = fit._candidate(root, candidate_path)
    choice = guest_args(candidate)
    _ready(root, candidate_path, args.split, args.calibration_report and Path(args.calibration_report), committed)
    if this_host() != candidate["host_cpu"]:
        raise ClosedLoopError(f"this host is not {candidate['host_cpu']}, which froze the candidate")
    doc, family, _, objective, manifest_sha = branches._frozen(root, candidate["manifest"])
    selected = splits.units(doc, args.split, sealed_evaluation=args.sealed_evaluation)
    if args.scenario:
        selected = [unit for unit in selected if unit["scenario"] in args.scenario]
    if args.seed:
        selected = [unit for unit in selected if unit["seed"] in args.seed]
    if not selected:
        raise ClosedLoopError("no units selected")
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    logs = root / "artifacts" / "branches" / family["id"] / "closed-loop"
    repeats = args.repeats or objective["repeats"]
    steps = plan(selected, repeats)
    earlier = branches._read_records(out)
    if earlier and not args.fill:
        raise ClosedLoopError(f"{out} already has records; pass --fill to add only missing branches")
    if {record.get("candidate_sha256") for record in earlier} - {_sha256(candidate_path)}:
        raise ClosedLoopError(f"{out} holds branches of a different candidate")
    have = {_slot(record) for record in earlier if not record.get("error") and not record.get("flags")}
    steps = [(unit, repeat) for unit, repeat in steps if (unit["scenario"], unit["seed"], repeat) not in have]
    tried: dict[tuple, int] = {}
    for record in earlier:
        tried[_slot(record)] = max(tried.get(_slot(record), -1), record.get("attempt", 0))
    print(f"CLOSED_LOOP_PLAN units={len(selected)} boots={len(steps)} split={args.split}", flush=True)
    for index, (unit, repeat) in enumerate(steps):
        first = tried.get((unit["scenario"], unit["seed"], repeat), -1) + 1
        for attempt in range(first, first + args.retries + 1):
            name = f"{unit['scenario']}-{unit['seed']}-candidate-r{repeat}-a{attempt}"
            text = boot(root, family["id"], unit, branches.CANDIDATE_PROFILE, args.timeout_s, logs / f"{name}.log", choice)
            entry = {
                "split": args.split,
                "manifest": doc["id"],
                "manifest_sha256": manifest_sha,
                "candidate": candidate_path.name,
                "candidate_sha256": _sha256(candidate_path),
                "repeat": repeat,
                "attempt": attempt,
            }
            stop = None
            try:
                record = branches.parse_boot(text)
                entry.update(record)
                entry["flags"] = branches.comparability_flags(record, family, objective)
                if (record["host"]["host_cpu"], record["host"]["kvm_module"]) != _host(candidate):
                    stop = ClosedLoopError(f"{_key(entry)} ran on a host other than the candidate's")
                else:
                    entry["choice"] = check_decision(entry, candidate)
            except branches.BranchError as err:
                entry.update({
                    "scenario": unit["scenario"],
                    "seed": unit["seed"],
                    "profile": branches.CANDIDATE_PROFILE,
                    "error": str(err),
                })
            except ClosedLoopError as err:
                stop = err
            if stop is not None:
                entry["error"] = str(stop)
            with out.open("a") as handle:
                handle.write(json.dumps(entry, sort_keys=True) + "\n")
            if stop is not None:
                raise stop
            outcome = entry.get("outcome", {})
            print(
                f"CLOSED_LOOP {index + 1}/{len(steps)} {name} choice={entry.get('choice')} "
                f"pending={entry.get('state', {}).get('latency_queue')} "
                f"decide_us={entry.get('decision', {}).get('decide_us')} p99_us={outcome.get('p99_us')} "
                f"completion_bp={outcome.get('completion_bp')} batch_service_us={outcome.get('batch_service_us')} "
                f"flags={','.join(entry.get('flags', [])) or entry.get('error', 'ok')}",
                flush=True,
            )
            if not entry.get("error") and not entry.get("flags"):
                break
    return 0


def _restore(summary: dict) -> dict:
    """A label file's profile summary, with a censored p99 back at infinity for ranking."""
    return {key: (branches.INF if value is None and key.startswith("p99") else value) for key, value in summary.items()}


def score_unit(unit: dict, records: list[dict], objective: dict, params: dict) -> dict:
    """One unit's closed-loop repeats against its own forced branches under the objective."""
    kept = records[: objective["repeats"]]
    row = {
        "scenario": unit["scenario"],
        "seed": unit["seed"],
        "status": unit["status"],
        "label": unit["label"],
        "best": unit["best"],
        "choices": dict(Counter(record["decision"]["choice"] for record in kept)),
        "choices_in_label": sum(record["decision"]["choice"] in unit["label"] for record in kept),
        "offline_choices": dict(Counter(fit.threshold_choice(params, b["snapshot"]) for b in unit["branches"])),
        "pending": [record["state"]["latency_queue"] for record in kept],
        "decide_us": [record["decision"]["decide_us"] for record in kept],
    }
    if len(kept) < objective["repeats"]:
        return {**row, "closed_loop": "incomplete"}
    summary = branches._summary(kept)
    ref = _restore(unit["profiles"][objective["reference_profile"]])
    best = _restore(unit["profiles"][unit["best"]])
    batch_ok = summary["batch_service_median"] * 10_000 >= ref["batch_service_median"] * objective["batch_retain_bp"]
    done_ok = summary["completion_median"] * 10_000 >= ref["completion_median"] * objective["completion_retain_bp"]
    reaches = branches._rank(summary) <= branches._rank(best) or branches._ties(best, summary)
    summary["batch_retained_bp"] = round(summary["batch_service_median"] * 10_000 / max(ref["batch_service_median"], 1))
    summary["feasible"] = batch_ok and done_ok
    verdict = "hit" if summary["feasible"] and reaches else ("infeasible" if not summary["feasible"] else "miss")
    return {**row, "closed_loop": verdict, "summary": branches._jsonable(summary)}


def score(root: Path, candidate_path: Path, labels: list[Path], records_paths: list[Path]) -> dict:
    """Every scored unit's closed-loop verdict, beside the same candidate's offline score."""
    candidate = fit._candidate(root, candidate_path)
    guest_args(candidate)
    records = [record for path in records_paths for record in branches._read_records(path)]
    if not records:
        raise ClosedLoopError("no closed-loop records")
    split = {record.get("split") for record in records}
    if len(split) != 1:
        raise ClosedLoopError(f"records mix splits {sorted(map(str, split))}")
    split = split.pop()
    if {record.get("candidate_sha256") for record in records} != {_sha256(candidate_path)}:
        raise ClosedLoopError("records were collected for a different candidate")
    if {record.get("manifest_sha256") for record in records} != {candidate["manifest_sha256"]}:
        raise ClosedLoopError("records were collected under a different split manifest")
    docs = fit._gather(root, labels, split)
    if fit._host(docs) != _host(candidate):
        raise ClosedLoopError("labels come from a different host than the candidate's fit")
    usable = [record for record in records if not record.get("error") and not record.get("flags")]
    grouped: dict[tuple[str, int], list[dict]] = {}
    for record in sorted(usable, key=lambda r: (r["repeat"], r["attempt"])):
        if (record["host"]["host_cpu"], record["host"]["kvm_module"]) != _host(candidate):
            raise ClosedLoopError(f"{_key(record)} ran on a host other than the candidate's")
        check_decision(record, candidate)
        grouped.setdefault((record["scenario"], record["seed"]), []).append(record)
    units = {(unit["scenario"], unit["seed"]): unit for doc in docs for unit in doc["units"]}
    stray = sorted(set(grouped) - set(units))
    if stray:
        raise ClosedLoopError(f"closed-loop records have no labels: {', '.join(f'{s}/{n}' for s, n in stray)}")
    _, _, _, objective, _ = branches._frozen(root, candidate["manifest"])
    config = fit.load_config(root)
    rows, left = [], []
    for key in sorted(units):
        unit = units[key]
        if unit["status"] not in config["scored_status"]:
            left.append({"scenario": key[0], "seed": key[1], "status": unit["status"]})
            continue
        rows.append(score_unit(unit, grouped.get(key, []), objective, candidate["params"]))
    complete = [row for row in rows if row["closed_loop"] != "incomplete"]
    hits = sum(row["closed_loop"] == "hit" for row in complete)
    feasible = sum(row["closed_loop"] != "infeasible" for row in complete)
    scored, _ = fit.decisions(docs, config)
    offline = fit.score([(unit, fit.threshold_choice(candidate["params"], b["snapshot"]), False) for unit, b in scored])
    decide = [record["decision"]["decide_us"] for row in rows for record in grouped.get((row["scenario"], row["seed"]), [])]
    return {
        "candidate": candidate["id"],
        "selected": candidate["selected"],
        "candidate_file": candidate_path.name,
        "candidate_sha256": _sha256(candidate_path),
        "host_cpu": candidate["host_cpu"],
        "kvm_module": candidate["kvm_module"],
        "split": split,
        "labels": [{key: doc[key] for key in ("labels", "labels_sha256")} for doc in docs],
        "records": [{"records": path.name, "records_sha256": _sha256(path)} for path in records_paths],
        "attempts": len(records),
        "refused": len(records) - len(usable),
        "units": len(rows),
        "units_complete": len(complete),
        "units_hit": hits,
        "units_hit_wilson95": fit.wilson(hits, len(complete)),
        "units_feasible": feasible,
        "units_feasible_wilson95": fit.wilson(feasible, len(complete)),
        "decide_us": {"min": min(decide), "median": statistics.median(decide), "max": max(decide)} if decide else None,
        "offline": {key: offline[key] for key in ("decisions", "hits", "misses", "infeasible", "abstain")},
        "unscored": left,
        "rows": rows,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="aik_controller.closed_loop")
    sub = parser.add_subparsers(dest="command", required=True)
    run = sub.add_parser("collect")
    run.add_argument("--candidate", required=True)
    run.add_argument("--split", required=True)
    run.add_argument("--scenario", action="append")
    run.add_argument("--seed", action="append", type=int)
    run.add_argument("--repeats", type=int)
    run.add_argument("--retries", type=int, default=2)
    run.add_argument("--timeout-s", type=int, default=40)
    run.add_argument("--sealed-evaluation", action="store_true")
    run.add_argument("--calibration-report")
    run.add_argument("--fill", action="store_true")
    run.add_argument("--out", required=True)
    rate = sub.add_parser("score")
    rate.add_argument("--candidate", required=True, type=Path)
    rate.add_argument("--labels", action="append", required=True, type=Path)
    rate.add_argument("--records", action="append", required=True, type=Path)
    rate.add_argument("--out", required=True, type=Path)
    args = parser.parse_args(argv)
    root = Path(__file__).resolve().parents[3]
    try:
        if args.command == "collect":
            return collect(args, root)
        if args.out.exists():
            raise ClosedLoopError(f"{args.out} exists; a closed-loop score is written once")
        report = score(root, args.candidate, args.labels, args.records)
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(report, indent=1, sort_keys=True) + "\n")
    except (ClosedLoopError, fit.FitError, branches.BranchError, splits.SplitError) as err:
        print(f"CLOSED_LOOP_REFUSED {err}", file=sys.stderr)
        return 2
    for row in report["rows"]:
        print(
            f"UNIT {row['scenario']}/{row['seed']} label={','.join(row['label'])} closed_loop={row['closed_loop']} "
            f"choices={json.dumps(row['choices'], sort_keys=True)} offline={json.dumps(row['offline_choices'], sort_keys=True)} "
            f"pending={row['pending']}"
        )
    print(
        f"CLOSED_LOOP split={report['split']} units={report['units_complete']}/{report['units']} "
        f"hit={report['units_hit']} wilson95={report['units_hit_wilson95']} feasible={report['units_feasible']} "
        f"decide_us={report['decide_us']} offline_hits={report['offline']['hits']}/{report['offline']['decisions']}"
    )
    print(f"CLOSED_LOOP_SHA256 {_sha256(args.out)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

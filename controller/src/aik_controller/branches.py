"""Branched trials and their labels, under a frozen branched manifest.

The manifest names the job family and the objective (splits-v2: jobs-v2 and
objective-v2). splits-v1 still labels the jobs-v1 pilot records.

`collect` boots one fresh guest per (unit, profile, repeat). Every boot runs the
unit's prefix under balanced, then forces its profile for the horizon. Profile
order is shuffled per unit and repeat so host drift does not line up with a
profile. Each attempt, including a refused one, is appended to the records file.
`--fill` boots only the planned branches that still lack a usable record.

`label` turns records into one label per unit. Labels come from measured
outcomes under the frozen objective. Only the pre-decision state is kept as
model input.
"""

import argparse
import hashlib
import json
import math
import os
import random
import re
import statistics
import subprocess
import sys
from pathlib import Path

from aik_controller import jobs, splits
from aik_controller.heuristic import choose, load_thresholds

LOADER = "artifacts/loader/hermit-loader-x86_64-v0.5.6"
GUEST = "target/x86_64-unknown-hermit/debug/policy-guest"
BATCH_UNIT_US = 500
CANDIDATE_PROFILE = "candidate"
INF = math.inf


class BranchError(ValueError):
    pass


def _fields(text: str) -> dict:
    out = {}
    for key, value in re.findall(r"(\w+)=(\S+)", text):
        if value == "censored":
            out[key] = None
        elif re.fullmatch(r"\d+", value):
            out[key] = int(value)
        else:
            out[key] = value
    return out


def parse_boot(text: str) -> dict:
    """One boot's serial and harness output, as a record. Refuse anything partial."""
    lines: dict[str, str] = {}
    host = None
    for raw in text.splitlines():
        line = raw.strip()
        match = re.match(r"(FERRUM_BRANCH_[A-Z]+)\b ?(.*)", line)
        if match:
            lines.setdefault(match.group(1), match.group(2))
        elif line.startswith("raw_exit="):
            host = line
    if "FERRUM_BRANCH_FAIL" in lines:
        raise BranchError(f"guest failed: {lines['FERRUM_BRANCH_FAIL']}")
    if host is None:
        raise BranchError("no raw_exit line")
    host_fields = {
        "raw_exit": int(re.search(r"raw_exit=(\d+)", host).group(1)),
        "accel": re.search(r"accel=(\S+)", host).group(1),
        "host_cpu": re.search(r"host_cpu=(.*) kvm_module=", host).group(1),
        "kvm_module": re.search(r"kvm_module=(\S+)", host).group(1),
    }
    if host_fields["raw_exit"] != 3:
        raise BranchError(f"raw exit {host_fields['raw_exit']} is not a clean shutdown")
    for name in ("BEGIN", "STATE", "OUTCOME", "OK"):
        if f"FERRUM_BRANCH_{name}" not in lines:
            raise BranchError(f"FERRUM_BRANCH_{name} is missing")
    begin = _fields(lines["FERRUM_BRANCH_BEGIN"])
    outcome = _fields(lines["FERRUM_BRANCH_OUTCOME"])
    for key in ("scenario", "seed", "profile"):
        if outcome.get(key) != begin.get(key):
            raise BranchError(f"outcome {key} does not match the boot request")
    decided = "FERRUM_BRANCH_DECISION" in lines
    if decided != (begin["profile"] == CANDIDATE_PROFILE):
        raise BranchError("a candidate branch carries one decision line and a forced branch none")
    extra = {"decision": _fields(lines["FERRUM_BRANCH_DECISION"])} if decided else {}
    return {
        "family": begin["family"],
        "scenario": begin["scenario"],
        "seed": begin["seed"],
        "profile": begin["profile"],
        "jobs": begin["jobs"],
        "schedule_fnv64": begin["schedule_fnv64"],
        "spins_per_ms": begin["spins_per_ms"],
        "measured_spins_per_ms": begin["measured_spins_per_ms"],
        "state": _fields(lines["FERRUM_BRANCH_STATE"]),
        "outcome": {key: value for key, value in outcome.items() if key not in ("scenario", "seed", "profile")},
        "host": host_fields,
        **extra,
    }


def drift_bp(record: dict) -> int:
    """In-trial batch speed against the boot's own sizing rate, in basis points."""
    sized = record["spins_per_ms"]
    return abs(record["outcome"]["batch_spins_per_ms"] - sized) * 10_000 // sized


def prefix_drift_bp(record: dict) -> int:
    """The same comparison over the pre-decision window. A batch unit is
    BATCH_UNIT_US at the sizing rate, so units times that against batch CPU time
    is the speed ratio."""
    state = record["state"]
    work_us = state["batch_completions"] * BATCH_UNIT_US
    return abs(work_us * 10_000 // max(state["batch_service_us"], 1) - 10_000)


def check_record(record: dict, family: dict, objective: dict) -> list[str]:
    """Flags for one forced branch. A candidate branch is refused here, so a label never reads one."""
    if record["profile"] not in objective["candidates"]:
        raise BranchError("record is outside the candidate set")
    return comparability_flags(record, family, objective)


def comparability_flags(record: dict, family: dict, objective: dict) -> list[str]:
    """Flags for any branch of a unit. A schedule that is not the unit's own is refused."""
    expected = format(jobs.digest(jobs.schedule(family, record["scenario"], record["seed"])), "#018x")
    if record["schedule_fnv64"] != expected:
        raise BranchError(f"{record['scenario']}/{record['seed']} ran a different schedule")
    if record["family"] != family["id"]:
        raise BranchError("record is outside the family")
    rule = objective["comparability"]
    flags = []
    if drift_bp(record) > rule["trial_speed_drift_bp"]:
        flags.append("disturbed")
    elif "prefix_speed_drift_bp" in rule and prefix_drift_bp(record) > rule["prefix_speed_drift_bp"]:
        flags.append("disturbed")
    return flags


def snapshot(record: dict) -> dict:
    """The pre-decision state in the shape features.encode and the heuristic read."""
    state = record["state"]

    def group(name: str, prefix: str | None) -> dict:
        value = (lambda key: state.get(f"{prefix}_{key}", 0)) if prefix else (lambda key: 0)
        return {
            "class": name,
            "queue_len": value("queue"),
            "runnable": value("runnable"),
            "cpu_service_us": value("service_us"),
            "max_wait_us": value("max_wait_us"),
            "wait_samples": value("wait_samples"),
            "completions": value("completions"),
            "managed_bytes": 0,
        }

    groups = [group("latency", "latency"), group("batch", "batch"), group("maintenance", None), group("system", None)]
    groups[2]["cpu_service_us"] = state.get("maintenance_service_us", 0)
    groups[3]["cpu_service_us"] = state.get("system_service_us", 0)
    return {
        "current_profile": "balanced",
        "window_us": state["window_us"],
        "groups": groups,
        "pressure": {
            "managed_used_bytes": 0,
            "managed_cap_bytes": 256 * 1024 * 1024,
            "headroom_bytes": 128 * 1024 * 1024,
            "evictable_backlog_bytes": 0,
            "emergency": False,
        },
    }


def _p99(record: dict) -> float:
    value = record["outcome"]["p99_us"]
    return INF if value is None else float(value)


def _summary(records: list[dict]) -> dict:
    p99 = [_p99(record) for record in records]
    completion = [record["outcome"]["completion_bp"] for record in records]
    batch = [record["outcome"]["batch_service_us"] for record in records]
    units = [record["outcome"]["batch_units"] for record in records]
    return {
        "repeats": len(records),
        "p99_median": statistics.median(p99),
        "p99_min": min(p99),
        "p99_max": max(p99),
        "completion_median": statistics.median(completion),
        "completion_min": min(completion),
        "completion_max": max(completion),
        "batch_service_median": statistics.median(batch),
        "batch_units_median": statistics.median(units),
    }


def _rank(summary: dict) -> tuple:
    return (summary["p99_median"], -summary["completion_median"], -summary["batch_service_median"])


def _ties(best: dict, other: dict) -> bool:
    if best["p99_median"] == INF:
        return other["completion_max"] >= best["completion_min"]
    return other["p99_min"] <= best["p99_max"]


def _jsonable(summary: dict) -> dict:
    return {key: (None if value == INF else value) for key, value in summary.items()}


def kept_branches(records: list[dict], objective: dict) -> dict[str, list[dict]]:
    """The branches a label reads: the first `repeats` usable records of each candidate."""
    repeats = objective["repeats"]
    return {
        profile: [r for r in records if r["profile"] == profile][:repeats]
        for profile in objective["candidates"]
    }


def label_unit(records: list[dict], objective: dict, thresholds: dict | None = None) -> dict:
    """The objective's label for one unit. Records must be the usable branches of that unit."""
    candidates = objective["candidates"]
    repeats = objective["repeats"]
    reference = objective["reference_profile"]
    by_profile = kept_branches(records, objective)
    first = records[0]
    unit = {"scenario": first["scenario"], "seed": first["seed"]}
    if any(len(by_profile[profile]) < repeats for profile in candidates):
        return {**unit, "status": "incomplete", "label": [], "profiles": {}}
    kept = [record for profile in candidates for record in by_profile[profile]]
    pending = [record["state"]["latency_queue"] for record in kept]
    rule = objective["comparability"]
    spread = max(pending) - min(pending)
    allowed = max(rule["pending_spread_jobs"], statistics.median(pending) * rule["pending_spread_bp"] / 10_000)
    summaries = {profile: _summary(by_profile[profile]) for profile in candidates}
    ref = summaries[reference]
    feasible = []
    for profile, summary in summaries.items():
        batch_ok = summary["batch_service_median"] * 10_000 >= ref["batch_service_median"] * objective["batch_retain_bp"]
        done_ok = summary["completion_median"] * 10_000 >= ref["completion_median"] * objective["completion_retain_bp"]
        summary["feasible"] = batch_ok and done_ok
        summary["batch_retained_bp"] = round(summary["batch_service_median"] * 10_000 / max(ref["batch_service_median"], 1))
        if summary["feasible"]:
            feasible.append(profile)
    best = min(feasible, key=lambda profile: _rank(summaries[profile]))
    label = sorted(
        (profile for profile in feasible if profile == best or _ties(summaries[best], summaries[profile])),
        key=candidates.index,
    )
    status = "diverged" if spread > allowed else ("tie" if len(label) > 1 else "labelled")
    out = {
        **unit,
        "status": status,
        "label": label,
        "best": best,
        "primary_censored": summaries[best]["p99_median"] == INF,
        "pending_spread": spread,
        "prefix_drift_bp_max": max(prefix_drift_bp(record) for record in kept),
        "profiles": {profile: _jsonable(summary) for profile, summary in summaries.items()},
        "snapshot": snapshot(by_profile[reference][0]),
    }
    if thresholds is not None:
        out["heuristic"] = choose(out["snapshot"], thresholds)
    return out


def usable_units(records: list[dict], family: dict, objective: dict) -> dict[tuple[str, int], list[dict]]:
    """Usable records grouped by unit, in unit order. A refused attempt is left out."""
    usable: dict[tuple[str, int], list[dict]] = {}
    for record in records:
        if record.get("error") or record.get("flags"):
            continue
        check_record(record, family, objective)
        usable.setdefault((record["scenario"], record["seed"]), []).append(record)
    return dict(sorted(usable.items()))


def label_records(records: list[dict], family: dict, objective: dict, thresholds: dict | None = None) -> list[dict]:
    return [label_unit(group, objective, thresholds) for group in usable_units(records, family, objective).values()]


def plan(units: list[dict], candidates: list[str], repeats: int, order_seed: int) -> list[tuple[dict, str, int]]:
    """Repeat-major order: every unit's branches for repeat 0, then repeat 1, and so on."""
    out = []
    for repeat in range(repeats):
        for unit in units:
            order = list(candidates)
            random.Random(f"{order_seed}:{unit['scenario']}:{unit['seed']}:{repeat}").shuffle(order)
            out.extend((unit, profile, repeat) for profile in order)
    return out


def _slot(record: dict) -> tuple:
    return (record["scenario"], record["seed"], record["profile"], record["repeat"])


def missing(steps: list[tuple[dict, str, int]], records: list[dict]) -> list[tuple[dict, str, int]]:
    """Planned branches that still lack a usable record."""
    have = {_slot(record) for record in records if not record.get("error") and not record.get("flags")}
    return [step for step in steps if (step[0]["scenario"], step[0]["seed"], step[1], step[2]) not in have]


def _read_records(path: Path) -> list[dict]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def boot(
    root: Path, family: str, unit: dict, profile: str, timeout_s: int, serial: Path, choice: str | None = None
) -> str:
    """One fresh guest. `choice` replaces `--profile=` with a candidate's own arguments."""
    append = (
        f"-- --branch --family={family} --scenario={unit['scenario']} "
        f"--seed={unit['seed']} {choice or f'--profile={profile}'}"
    )
    env = dict(
        os.environ,
        AIK_ACCEL="kvm",
        AIK_TIMEOUT_S=str(timeout_s),
        AIK_LOADER=str(root / LOADER),
        AIK_GUEST=str(root / GUEST),
        AIK_APPEND=append,
        AIK_SERIAL=str(serial),
    )
    done = subprocess.run(
        [str(root / "tools" / "run-qemu.sh")], env=env, capture_output=True, text=True, check=False
    )
    text = serial.read_text(errors="replace") if serial.exists() else ""
    return text + "\n" + done.stdout + done.stderr


def _frozen(root: Path, name: str) -> tuple[dict, dict, Path, dict, str]:
    """Manifest, family, objective path, objective, and manifest hash, all checked."""
    path = splits.branched_path(root, name)
    doc = splits.load_branched(root, path)
    family = jobs.load_family(splits.family_path(root, doc))
    objective_file = splits.objective_path(root, doc)
    objective = json.loads(objective_file.read_text())
    return doc, family, objective_file, objective, splits.manifest_sha256(path)


def collect(args: argparse.Namespace, root: Path) -> int:
    doc, family, _, objective, manifest_sha = _frozen(root, args.manifest)
    selected = splits.units(doc, args.split, sealed_evaluation=args.sealed_evaluation)
    if args.scenario:
        selected = [unit for unit in selected if unit["scenario"] in args.scenario]
    if args.seed:
        selected = [unit for unit in selected if unit["seed"] in args.seed]
    if not selected:
        raise BranchError("no units selected")
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    logs = root / "artifacts" / "branches" / family["id"]
    steps = plan(selected, objective["candidates"], args.repeats, args.order_seed)
    earlier = _read_records(out)
    if earlier and not args.fill:
        raise BranchError(f"{out} already has records; pass --fill to add only missing branches")
    if args.fill:
        steps = missing(steps, earlier)
    tried: dict[tuple, int] = {}
    for record in earlier:
        tried[_slot(record)] = max(tried.get(_slot(record), -1), record.get("attempt", 0))
    print(f"BRANCH_PLAN units={len(selected)} boots={len(steps)} split={args.split}", flush=True)
    for index, (unit, profile, repeat) in enumerate(steps):
        first = tried.get((unit["scenario"], unit["seed"], profile, repeat), -1) + 1
        for attempt in range(first, first + args.retries + 1):
            name = f"{unit['scenario']}-{unit['seed']}-{profile}-r{repeat}-a{attempt}"
            text = boot(root, family["id"], unit, profile, args.timeout_s, logs / f"{name}.log")
            entry = {
                "split": args.split,
                "manifest": doc["id"],
                "manifest_sha256": manifest_sha,
                "repeat": repeat,
                "attempt": attempt,
            }
            try:
                record = parse_boot(text)
                entry.update(record)
                entry["flags"] = check_record(record, family, objective)
            except BranchError as err:
                entry.update({"scenario": unit["scenario"], "seed": unit["seed"], "profile": profile, "error": str(err)})
            with out.open("a") as handle:
                handle.write(json.dumps(entry, sort_keys=True) + "\n")
            outcome = entry.get("outcome", {})
            print(
                f"BRANCH {index + 1}/{len(steps)} {name} "
                f"p99_us={outcome.get('p99_us')} completion_bp={outcome.get('completion_bp')} "
                f"batch_service_us={outcome.get('batch_service_us')} "
                f"flags={','.join(entry.get('flags', [])) or entry.get('error', 'ok')}",
                flush=True,
            )
            if not entry.get("error") and not entry.get("flags"):
                break
    return 0


def label(args: argparse.Namespace, root: Path) -> int:
    records_path = Path(args.records)
    raw = records_path.read_bytes()
    records = [json.loads(line) for line in raw.decode().splitlines() if line.strip()]
    named = {record.get("manifest") for record in records}
    if named == {None}:
        if args.manifest is None:
            raise BranchError("records do not name their manifest; pass --manifest")
        name = args.manifest
    elif len(named) == 1 and None not in named:
        name = named.pop()
        if args.manifest not in (None, name):
            raise BranchError(f"records name {name}, not {args.manifest}")
    else:
        raise BranchError(f"records mix manifests {sorted(map(str, named))}")
    doc, family, objective_file, objective, manifest_sha = _frozen(root, name)
    split = {record.get("split") for record in records}
    if len(split) != 1:
        raise BranchError(f"records mix splits {sorted(map(str, split))}")
    if {record.get("manifest_sha256") for record in records} != {manifest_sha}:
        raise BranchError("records were collected under a different split manifest")
    hosts = {record["host"]["host_cpu"] for record in records if "host" in record}
    if len(hosts) != 1:
        raise BranchError(f"records mix hosts {sorted(hosts)}; label each host separately")
    thresholds = load_thresholds(root / "configs" / "heuristic-v0.json")
    units = label_records(records, family, objective, thresholds)
    host = next(record["host"] for record in records if "host" in record)
    doc = {
        "family": family["id"],
        "objective": objective["id"],
        "objective_sha256": splits.manifest_sha256(objective_file),
        "manifest": doc["id"],
        "manifest_sha256": manifest_sha,
        "split": split.pop(),
        "host_cpu": host["host_cpu"],
        "kvm_module": host["kvm_module"],
        "records": records_path.name,
        "records_sha256": hashlib.sha256(raw).hexdigest(),
        "attempts": len(records),
        "refused": sum(1 for record in records if record.get("error") or record.get("flags")),
        "units": units,
    }
    Path(args.out).write_text(json.dumps(doc, indent=1, sort_keys=True) + "\n")
    for unit in units:
        print(
            f"LABEL {unit['scenario']}/{unit['seed']} status={unit['status']} "
            f"label={','.join(unit['label'])} heuristic={unit.get('heuristic')} "
            + " ".join(
                f"{name}:p99={_ms(s.get('p99_median'))}/batch={s.get('batch_retained_bp')}/done={s.get('completion_median')}"
                for name, s in unit["profiles"].items()
            )
        )
    print(f"LABELS_SHA256 {hashlib.sha256(Path(args.out).read_bytes()).hexdigest()}")
    return 0


def _ms(value) -> str:
    return "censored" if value is None else f"{value / 1000:.1f}ms"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="aik_controller.branches")
    sub = parser.add_subparsers(dest="command", required=True)
    run = sub.add_parser("collect")
    run.add_argument("--manifest", default=splits.CURRENT, choices=sorted(splits.BRANCHED))
    run.add_argument("--split", default="training")
    run.add_argument("--scenario", action="append")
    run.add_argument("--seed", action="append", type=int)
    run.add_argument("--repeats", type=int, default=3)
    run.add_argument("--retries", type=int, default=1)
    run.add_argument("--order-seed", type=int, default=1)
    run.add_argument("--timeout-s", type=int, default=40)
    run.add_argument("--sealed-evaluation", action="store_true")
    run.add_argument("--fill", action="store_true")
    run.add_argument("--out", required=True)
    mark = sub.add_parser("label")
    mark.add_argument("--manifest", choices=sorted(splits.BRANCHED))
    mark.add_argument("--records", required=True)
    mark.add_argument("--out", required=True)
    args = parser.parse_args(argv)
    root = Path(__file__).resolve().parents[3]
    try:
        return collect(args, root) if args.command == "collect" else label(args, root)
    except (BranchError, splits.SplitError) as err:
        print(f"BRANCH_REFUSED {err}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())

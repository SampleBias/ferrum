#!/usr/bin/env python3
"""Collect the wake-mode KVM A/B the way the Ryzen records were collected.

Same guest binary for both arms. Arm 0 is the default scheduler (no --wake
flag). Arm 3 passes --wake=3. A branch counts only when it parses and both
speed gates stay inside objective-v2's 500 bp rule. Disturbed boots are kept
and re-run until every cell has two clean repeats. The real-payload arm is six
clean boots per wake setting; each boot prints the balanced phase and the
adaptive phase.

Cells, copied from tools/plot-wake-ab.py:
  scenarios s4-u15, s4-u35, s4-u45, s12-u15, s12-u35, and overload s4-u65
  seeds 151 and 301
  profiles balanced and latency
  wake arms 0 and 3
  two repeats
"""

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "controller" / "src"))

from aik_controller import branches, jobs  # noqa: E402

UNDER = ["s4-u15", "s4-u35", "s4-u45", "s12-u15", "s12-u35"]
OVERLOAD = "s4-u65"
SCENARIOS = UNDER + [OVERLOAD]
SEEDS = [151, 301]
PROFILES = ["balanced", "latency"]
ARMS = [0, 3]
REPEATS = 2
REAL_BOOTS = 6
DRIFT_LIMIT_BP = 500
MAX_ATTEMPTS = 8
BUILD = "wake-mode"
FAMILY = ROOT / "configs" / "workloads" / "jobs-v2.json"
OBJECTIVE = ROOT / "configs" / "objective-v2.json"
LOADER = ROOT / "artifacts" / "loader" / "hermit-loader-x86_64-v0.5.6"
# Release guest: the Ryzen A/B's HMAC throughput is the optimized binary.
# Branch spins are sized from each boot's own calibration, and both arms
# share this one file.
GUEST = ROOT / "target" / "x86_64-unknown-hermit" / "release" / "policy-guest"
DATA = ROOT / "data" / "wake-ab"


def host_tag() -> str:
    model = ""
    for line in Path("/proc/cpuinfo").read_text().splitlines():
        if line.startswith("model name"):
            model = line.split(":", 1)[1].strip()
            break
    tags = {
        "Intel(R) Core(TM) i7-10750H CPU @ 2.60GHz": "i7-10750h",
        "AMD Ryzen 5 1600 Six-Core Processor": "r5-1600",
    }
    if model not in tags:
        raise SystemExit(f"no host tag for {model!r}")
    return tags[model]


def read_jsonl(path: Path) -> list[dict]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def append(path: Path, record: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a") as handle:
        handle.write(json.dumps(record, sort_keys=True) + "\n")
        handle.flush()


def clean_branch(record: dict) -> bool:
    if record.get("build") != BUILD or "error" in record or "outcome" not in record:
        return False
    return max(record["drift_bp"], record["prefix_drift_bp"]) <= DRIFT_LIMIT_BP


def boot_real(wake: int, serial: Path) -> str:
    append_arg = "-- --real-workload" + (f" --wake={wake}" if wake else "")
    env = dict(
        **{k: v for k, v in __import__("os").environ.items()},
        AIK_ACCEL="kvm",
        AIK_TIMEOUT_S="40",
        AIK_LOADER=str(LOADER),
        AIK_GUEST=str(GUEST),
        AIK_APPEND=append_arg,
        AIK_SERIAL=str(serial),
    )
    done = subprocess.run(
        [str(ROOT / "tools" / "run-qemu.sh")],
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    text = serial.read_text(errors="replace") if serial.exists() else ""
    return text + "\n" + done.stdout + done.stderr


def parse_real(text: str, repeat: int) -> list[dict]:
    host = None
    workloads = []
    failed = None
    for raw in text.splitlines():
        line = raw.strip()
        if line.startswith("FERRUM_REAL_WORKLOAD "):
            workloads.append(branches._fields(line.split(" ", 1)[1]))
        elif line.startswith("FERRUM_REAL_WORKLOAD_FAIL"):
            failed = line
        elif line.startswith("raw_exit="):
            host = line
    if failed:
        raise branches.BranchError(failed)
    if host is None:
        raise branches.BranchError("no raw_exit line")
    raw_exit = int(re.search(r"raw_exit=(\d+)", host).group(1))
    if raw_exit != 3:
        raise branches.BranchError(f"raw exit {raw_exit} is not a clean shutdown")
    if len(workloads) != 2:
        raise branches.BranchError(f"expected two workload lines, saw {len(workloads)}")
    host_fields = {
        "raw_exit": raw_exit,
        "accel": re.search(r"accel=(\S+)", host).group(1),
        "host_cpu": re.search(r"host_cpu=(.*) kvm_module=", host).group(1),
        "kvm_module": re.search(r"kvm_module=(\S+)", host).group(1),
    }
    records = []
    for fields in workloads:
        if fields.get("wake", 0) not in (0, 3):
            raise branches.BranchError(f"unexpected wake {fields.get('wake')}")
        records.append({"host": host_fields, "repeat": repeat, **fields})
    return records


def collect_real(path: Path, logs: Path) -> None:
    have = read_jsonl(path)
    seen: dict[tuple, set[str]] = {}
    for record in have:
        if "error" in record:
            continue
        seen.setdefault((record["wake"], record["repeat"]), set()).add(record["mode"])
    done = {wake: {repeat for (w, repeat), modes in seen.items() if w == wake and modes >= {"balanced", "adaptive"}} for wake in ARMS}
    for wake in ARMS:
        for repeat in range(REAL_BOOTS):
            if repeat in done[wake]:
                print(f"REAL wake={wake} repeat={repeat} already recorded", flush=True)
                continue
            last = "no attempt"
            for attempt in range(MAX_ATTEMPTS):
                text = boot_real(wake, logs / f"real-w{wake}-r{repeat}-a{attempt}.log")
                try:
                    records = parse_real(text, repeat)
                except branches.BranchError as err:
                    last = str(err)
                    print(f"REAL wake={wake} repeat={repeat} attempt={attempt} {last}", flush=True)
                    continue
                if [record["wake"] for record in records] != [wake, wake]:
                    last = f"guest wake {[record['wake'] for record in records]}"
                    print(f"REAL wake={wake} repeat={repeat} attempt={attempt} {last}", flush=True)
                    continue
                for record in records:
                    append(path, record)
                print(
                    f"REAL wake={wake} repeat={repeat} "
                    + " ".join(
                        f"{record['mode']}:p99_us={record['p99_us']}:batch_kib={record['batch_kib']}"
                        for record in records
                    ),
                    flush=True,
                )
                break
            else:
                raise SystemExit(f"REAL wake={wake} repeat={repeat} failed after {MAX_ATTEMPTS}: {last}")


def collect_branches(path: Path, logs: Path, family: dict, objective: dict) -> None:
    have = read_jsonl(path)
    clean: dict[tuple, int] = {}
    for record in have:
        if clean_branch(record):
            key = (record["scenario"], record["seed"], record["profile"], record["wake_arm"])
            clean[key] = clean.get(key, 0) + 1
    for repeat in range(REPEATS):
        for scenario in SCENARIOS:
            for seed in SEEDS:
                unit = {"scenario": scenario, "seed": seed}
                for profile in PROFILES:
                    for wake in ARMS:
                        key = (scenario, seed, profile, wake)
                        if clean.get(key, 0) > repeat:
                            print(f"BRANCH {scenario}/{seed} {profile} wake={wake} r{repeat} already clean", flush=True)
                            continue
                        last = "no attempt"
                        for attempt in range(MAX_ATTEMPTS):
                            name = f"{scenario}-{seed}-{profile}-w{wake}-r{repeat}-a{attempt}"
                            text = branches.boot(
                                ROOT, "jobs-v2", unit, profile, 40, logs / f"{name}.log", wake=wake
                            )
                            try:
                                record = branches.parse_boot(text)
                                if record.get("wake", 0) != wake:
                                    raise branches.BranchError(
                                        f"guest ran wake mode {record.get('wake', 0)}, not {wake}"
                                    )
                                if (record["scenario"], record["seed"], record["profile"]) != (
                                    scenario,
                                    seed,
                                    profile,
                                ):
                                    raise branches.BranchError("boot answered a different branch")
                                flags = branches.check_record(record, family, objective)
                                entry = {
                                    "build": BUILD,
                                    "wake_arm": wake,
                                    "repeat": repeat,
                                    "drift_bp": branches.drift_bp(record),
                                    "prefix_drift_bp": branches.prefix_drift_bp(record),
                                    **record,
                                }
                                if flags:
                                    entry_note = ",".join(flags)
                                else:
                                    entry_note = "ok"
                            except branches.BranchError as err:
                                append(
                                    path,
                                    {
                                        "build": BUILD,
                                        "error": str(err),
                                        "profile": profile,
                                        "repeat": repeat,
                                        "scenario": scenario,
                                        "seed": seed,
                                        "wake_arm": wake,
                                    },
                                )
                                last = str(err)
                                print(f"BRANCH {name} error={last}", flush=True)
                                continue
                            append(path, entry)
                            print(
                                f"BRANCH {name} p99_us={entry['outcome'].get('p99_us')} "
                                f"drift_bp={entry['drift_bp']} prefix_drift_bp={entry['prefix_drift_bp']} "
                                f"flags={entry_note}",
                                flush=True,
                            )
                            if not flags:
                                clean[key] = clean.get(key, 0) + 1
                                break
                            last = entry_note
                        else:
                            raise SystemExit(f"BRANCH {key} repeat={repeat} not clean after {MAX_ATTEMPTS}: {last}")


def main() -> int:
    branches.GUEST = "target/x86_64-unknown-hermit/release/policy-guest"
    if not GUEST.is_file() or not LOADER.is_file():
        raise SystemExit("debug policy-guest or the pinned loader is missing")
    tag = host_tag()
    family = jobs.load_family(FAMILY)
    objective = json.loads(OBJECTIVE.read_text())
    if objective["comparability"]["trial_speed_drift_bp"] != DRIFT_LIMIT_BP:
        raise SystemExit("objective drift gate is not 500 bp")
    logs = ROOT / "artifacts" / "wake-ab" / tag
    logs.mkdir(parents=True, exist_ok=True)
    print(f"WAKE_AB host={tag}", flush=True)
    collect_real(DATA / f"real-workload-{tag}.jsonl", logs)
    collect_branches(DATA / f"branches-{tag}.jsonl", logs, family, objective)
    print("WAKE_AB_OK", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())

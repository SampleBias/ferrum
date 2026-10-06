"""jobs-v1 arrival schedules, built the same way as crates/workloads/src/jobs.rs.

The harness checks each guest's printed schedule digest against this builder,
so a record that ran a different schedule is refused.
"""

import json
from functools import lru_cache
from pathlib import Path

MASK = (1 << 64) - 1


def load_family(path: Path) -> dict:
    return json.loads(path.read_text())


def scenario(family: dict, name: str) -> dict:
    for entry in family["scenarios"]:
        if entry["name"] == name:
            return entry
    raise KeyError(f"{name} is not a {family['id']} scenario")


def fnv1a64(data: bytes) -> int:
    value = 0xCBF29CE484222325
    for byte in data:
        value ^= byte
        value = (value * 0x100000001B3) & MASK
    return value


def _splitmix(state: int):
    while True:
        state = (state + 0x9E3779B97F4A7C15) & MASK
        z = state
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & MASK
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & MASK
        yield z ^ (z >> 31)


def schedule(family: dict, name: str, seed: int) -> list[int]:
    entry = scenario(family, name)
    span = family["prefix_us"] + family["horizon_us"] + family["drain_us"]
    return list(_schedule(name, seed, entry["rate_per_s"], family["tick_us"], span))


@lru_cache(maxsize=256)
def _schedule(name: str, seed: int, rate_per_s: int, tick: int, span: int) -> tuple[int, ...]:
    threshold = ((rate_per_s * tick) << 64) // 1_000_000
    draws = _splitmix((seed ^ fnv1a64(name.encode())) & MASK)
    return tuple(index * tick for index in range(span // tick) if next(draws) < threshold)


def digest(arrivals: list[int]) -> int:
    return fnv1a64(b"".join(arrival.to_bytes(8, "little") for arrival in arrivals))

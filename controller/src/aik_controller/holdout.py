"""Holdout manifests: fresh units for a controller frozen after the sealed splits opened.

splits-v2's final test and out-of-distribution subset were opened for
candidate-v0. A controller designed later has seen them, so they are
development evidence for it. A holdout manifest names new seeds over the same
family and objective, disjoint from every branched manifest, and pins the
controller files it tests by hash. It is committed before any of its units is
booted, and each unit is collected and labelled once.
"""

import json
from pathlib import Path

from aik_controller import jobs, splits

HOLDOUTS = {
    "holdout-v1": ("jobs-v2", "objective-v2"),
    "holdout-v2": ("jobs-v2", "objective-v2"),
}
SPLIT = "holdout"


class HoldoutError(ValueError):
    pass


def path(root: Path, name: str) -> Path:
    if name not in HOLDOUTS:
        raise HoldoutError(f"{name} is not a holdout manifest")
    return root / "configs" / f"{name}.json"


def _branched_seeds(root: Path) -> dict[int, str]:
    seeds = {}
    for name in splits.BRANCHED:
        doc = json.loads(splits.branched_path(root, name).read_text())
        for split in splits.SPLITS:
            for entry in doc.get(split, []):
                seeds[entry["seed"]] = f"{name} {split}"
    return seeds


def load(root: Path, name: str = "holdout-v1") -> dict:
    """A holdout manifest, once its family, objective, schedules and controllers still match."""
    file = path(root, name)
    doc = json.loads(file.read_text())
    if doc.get("id") != name or (doc.get("family"), doc.get("objective")) != HOLDOUTS[name]:
        raise HoldoutError(f"{file.name} does not declare {name} over its own family and objective")
    if doc.get("frozen_before_labels") is not True:
        raise HoldoutError(f"{name} must be frozen before labels")
    if splits.manifest_sha256(splits.family_path(root, doc)) != doc.get("family_sha256"):
        raise HoldoutError(f"{doc['family']} hash does not match {name}")
    if splits.manifest_sha256(splits.objective_path(root, doc)) != doc.get("objective_sha256"):
        raise HoldoutError(f"{doc['objective']} hash does not match {name}")
    controllers = doc.get("controllers")
    if not isinstance(controllers, list) or not controllers:
        raise HoldoutError(f"{name} names no controller")
    for entry in controllers:
        config = root / "configs" / entry["config"]
        if not config.is_file() or splits.manifest_sha256(config) != entry.get("sha256"):
            raise HoldoutError(f"{entry['config']} changed after {name} was frozen")
    family = jobs.load_family(splits.family_path(root, doc))
    names = {scenario["name"] for scenario in family["scenarios"]}
    used = _branched_seeds(root)
    for other, _family in HOLDOUTS.items():
        if other == name:
            continue
        other_path = root / "configs" / f"{other}.json"
        if not other_path.is_file():
            continue
        for entry in json.loads(other_path.read_text()).get(SPLIT, []):
            used.setdefault(entry["seed"], f"{other}")
    units = doc.get(SPLIT)
    if not isinstance(units, list) or not units:
        raise HoldoutError(f"{name} has no units")
    seen = set()
    for entry in units:
        scenario, seed = entry.get("scenario"), entry.get("seed")
        if scenario not in names or not isinstance(seed, int):
            raise HoldoutError(f"unit {entry} is not a {doc['family']} unit")
        if seed in used:
            raise HoldoutError(f"seed {seed} is already in {used[seed]}")
        if (scenario, seed) in seen:
            raise HoldoutError(f"unit {scenario}/{seed} appears twice")
        expected = format(jobs.digest(jobs.schedule(family, scenario, seed)), "#018x")
        if entry.get("schedule_fnv64") != expected:
            raise HoldoutError(f"unit {scenario}/{seed} schedule digest does not match")
        seen.add((scenario, seed))
    return doc


def require_committed(root: Path, name: str, committed=None) -> None:
    """A holdout unit boots only once its manifest and the controllers it pins are committed."""
    if committed is None:
        from aik_controller.fit import git_committed as committed
    doc = load(root, name)
    for file in [path(root, name)] + [root / "configs" / entry["config"] for entry in doc["controllers"]]:
        if not committed(root, file):
            raise HoldoutError(f"{file.name} must be committed before {name} is booted")


def units(doc: dict, split: str) -> list[dict]:
    if split != SPLIT:
        raise HoldoutError(f"{doc['id']} has one split, {SPLIT}")
    return list(doc[SPLIT])


def controller_sha256(doc: dict, config_name: str) -> str:
    for entry in doc["controllers"]:
        if entry["config"] == config_name:
            return entry["sha256"]
    raise HoldoutError(f"{doc['id']} does not test {config_name}")


def load_labels(root: Path, labels_path: Path, name: str = "holdout-v1") -> dict:
    """Holdout labels in the shape fit.load_labels returns, reproduced from their records."""
    from aik_controller import fit

    doc = json.loads(labels_path.read_text())
    manifest = load(root, name)
    if doc.get("manifest") != name or doc.get("manifest_sha256") != splits.manifest_sha256(path(root, name)):
        raise HoldoutError(f"{labels_path.name} was not measured under {name}")
    if doc.get("split") != SPLIT:
        raise HoldoutError(f"{labels_path.name} is not {SPLIT} labels")
    family = jobs.load_family(splits.family_path(root, manifest))
    objective = json.loads(splits.objective_path(root, manifest).read_text())
    return fit.reproduce(labels_path, doc, SPLIT, family, objective)

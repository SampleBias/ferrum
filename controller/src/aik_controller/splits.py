"""Split manifests.

splits-v0 holds the mixed-v1 windows. They are one scenario family and stay
screening evidence; a fit on them is refused.

splits-v1 and splits-v2 hold branched-label units, each frozen before any of its
labels was measured. A unit is one (scenario, seed) pair, and every branch of it
stays in its split. A fit reads training only. The final test and the
out-of-distribution subset open only for a sealed evaluation of a frozen
candidate. splits-v2 is current; splits-v1 holds the jobs-v1 pilot.
"""

import hashlib
import json
from pathlib import Path

from aik_controller import jobs
from aik_controller.laya_offline import cases


class SplitError(ValueError):
    pass


def load_manifest(path: Path) -> dict:
    """Refuse a file that would allow a fit on this development set."""
    doc = json.loads(path.read_text())
    if doc.get("id") != "splits-v0":
        raise SplitError("manifest is not splits-v0")
    if doc.get("fit_allowed") is not False:
        raise SplitError("splits-v0 does not allow a fit")
    for key in ("training", "calibration", "final_test", "out_of_distribution"):
        if doc.get(key) != []:
            raise SplitError(f"splits-v0 {key} set is not empty")
    development = doc.get("development")
    if not isinstance(development, dict):
        raise SplitError("development split is missing")
    if development.get("scenario") != "mixed-v1" or development.get("role") != "screening":
        raise SplitError("development split is not the mixed-v1 screening set")
    if not isinstance(development.get("cases"), list) or not development["cases"]:
        raise SplitError("development cases are missing")
    return doc


def check_screening(root: Path, manifest: Path | None = None) -> dict:
    """The manifest, once it matches the workload file and the screening cases."""
    path = manifest if manifest is not None else root / "configs" / "splits-v0.json"
    doc = load_manifest(path)
    workload = root / "configs" / "workloads" / "mixed-v1.json"
    digest = hashlib.sha256(workload.read_bytes()).hexdigest()
    if digest != doc["development"]["workload_sha256"]:
        raise SplitError("mixed-v1 hash does not match the split manifest")
    names = [case["name"] for case in cases(root)]
    if names != doc["development"]["cases"]:
        raise SplitError("screening cases do not match the split manifest")
    return doc


def fit(root: Path, manifest: Path | None = None) -> None:
    """Refuse to train. These windows are not a training set."""
    check_screening(root, manifest)
    raise SplitError(
        "refusing to fit: splits-v0 has no training, calibration, or final-test set"
    )


SPLITS = ("training", "development", "calibration", "final_test", "out_of_distribution")
SEALED = ("final_test", "out_of_distribution")
# Each branched manifest names exactly one family and one objective.
BRANCHED = {
    "splits-v1": ("jobs-v1", "objective-v1"),
    "splits-v2": ("jobs-v2", "objective-v2"),
}
CURRENT = "splits-v2"


def branched_path(root: Path, name: str = CURRENT) -> Path:
    if name not in BRANCHED:
        raise SplitError(f"{name} is not a branched-label manifest")
    return root / "configs" / f"{name}.json"


def family_path(root: Path, doc: dict) -> Path:
    return root / "configs" / "workloads" / f"{doc['family']}.json"


def objective_path(root: Path, doc: dict) -> Path:
    return root / "configs" / f"{doc['objective']}.json"


def manifest_sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load_branched(root: Path, manifest: Path | None = None) -> dict:
    """A branched manifest, once it matches its frozen family, objective, and schedules."""
    path = manifest if manifest is not None else branched_path(root)
    doc = json.loads(path.read_text())
    name = doc.get("id")
    if name not in BRANCHED or (doc.get("family"), doc.get("objective")) != BRANCHED[name]:
        raise SplitError(f"{path.name} is not a known branched manifest with its own family and objective")
    if doc.get("frozen_before_labels") is not True:
        raise SplitError(f"{name} must be frozen before labels")
    if doc.get("fit_reads") != ["training"]:
        raise SplitError("a fit reads the training split only")
    if tuple(doc.get("sealed", ())) != SEALED:
        raise SplitError("final_test and out_of_distribution must stay sealed")
    if manifest_sha256(family_path(root, doc)) != doc.get("family_sha256"):
        raise SplitError(f"{doc['family']} hash does not match the split manifest")
    if manifest_sha256(objective_path(root, doc)) != doc.get("objective_sha256"):
        raise SplitError(f"{doc['objective']} hash does not match the split manifest")
    family = jobs.load_family(family_path(root, doc))
    names = {entry["name"] for entry in family["scenarios"]}
    seen_units: set[tuple[str, int]] = set()
    seeds: dict[int, str] = {}
    scenarios: dict[str, set[str]] = {}
    for split in SPLITS:
        entries = doc.get(split)
        if not isinstance(entries, list) or not entries:
            raise SplitError(f"{name} {split} is empty")
        for entry in entries:
            scenario, seed = entry.get("scenario"), entry.get("seed")
            if scenario not in names or not isinstance(seed, int):
                raise SplitError(f"{split} unit {entry} is not a {doc['family']} unit")
            if (scenario, seed) in seen_units:
                raise SplitError(f"unit {scenario}/{seed} appears twice")
            if seeds.setdefault(seed, split) != split:
                raise SplitError(f"seed {seed} is shared by {seeds[seed]} and {split}")
            expected = format(jobs.digest(jobs.schedule(family, scenario, seed)), "#018x")
            if entry.get("schedule_fnv64") != expected:
                raise SplitError(f"unit {scenario}/{seed} schedule digest does not match")
            seen_units.add((scenario, seed))
            scenarios.setdefault(scenario, set()).add(split)
    for scenario, used in scenarios.items():
        if "out_of_distribution" in used and used != {"out_of_distribution"}:
            raise SplitError(f"out-of-distribution scenario {scenario} appears in {sorted(used)}")
    for scenario in doc.get("unseen_in_final_test", []):
        if scenarios.get(scenario) != {"final_test"}:
            raise SplitError(f"unseen scenario {scenario} is outside the final test or also elsewhere")
    if not doc.get("unseen_in_final_test"):
        raise SplitError("the final test declares no unseen scenario")
    return doc


def units(doc: dict, split: str, sealed_evaluation: bool = False) -> list[dict]:
    """The units of one split. A sealed split needs an explicit sealed evaluation."""
    if split not in SPLITS:
        raise SplitError(f"unknown split {split}")
    if split in SEALED and not sealed_evaluation:
        raise SplitError(f"{split} is sealed until a frozen candidate is evaluated")
    return list(doc[split])


def fit_units(root: Path, manifest: Path | None = None) -> list[dict]:
    """What a fit may read: training units of a manifest that still matches."""
    return units(load_branched(root, manifest), "training")


def check_fit_labels(labels: dict, root: Path, manifest: Path | None = None) -> None:
    """A fit reads training labels measured under this exact manifest."""
    path = manifest if manifest is not None else branched_path(root)
    load_branched(root, path)
    if labels.get("manifest_sha256") != manifest_sha256(path):
        raise SplitError("labels were measured under a different split manifest")
    if labels.get("split") in SEALED:
        raise SplitError(f"{labels.get('split')} labels cannot feed a fit")
    if labels.get("split") != "training":
        raise SplitError("a fit reads training labels only")


def check_selection_labels(labels: dict, root: Path, manifest: Path | None = None) -> None:
    """Model selection reads development labels measured under this exact manifest."""
    path = manifest if manifest is not None else branched_path(root)
    load_branched(root, path)
    if labels.get("manifest_sha256") != manifest_sha256(path):
        raise SplitError("labels were measured under a different split manifest")
    if labels.get("split") in SEALED:
        raise SplitError(f"{labels.get('split')} labels stay sealed until a frozen candidate is evaluated")
    if labels.get("split") != "development":
        raise SplitError("model selection reads development labels only")


def check_calibration_labels(labels: dict, root: Path, manifest: Path | None = None) -> None:
    """A frozen candidate is calibrated on calibration labels from this exact manifest."""
    path = manifest if manifest is not None else branched_path(root)
    load_branched(root, path)
    if labels.get("manifest_sha256") != manifest_sha256(path):
        raise SplitError("labels were measured under a different split manifest")
    if labels.get("split") != "calibration":
        raise SplitError("calibration reads calibration labels only")


def check_sealed_labels(labels: dict, root: Path, split: str, manifest: Path | None = None) -> None:
    """A sealed evaluation reads final-test or out-of-distribution labels from this exact manifest."""
    path = manifest if manifest is not None else branched_path(root)
    load_branched(root, path)
    if labels.get("manifest_sha256") != manifest_sha256(path):
        raise SplitError("labels were measured under a different split manifest")
    if split not in SEALED or labels.get("split") != split:
        raise SplitError(f"a sealed evaluation of {split} reads {split} labels only")

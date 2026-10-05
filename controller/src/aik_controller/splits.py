"""Development split for the recorded windows.

The mixed-v1 windows are one scenario family. They are screening evidence.
A fit has to wait for a separate final-test family collected before training.
"""

import hashlib
import json
from pathlib import Path

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

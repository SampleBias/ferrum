"""Local Laya checkpoint pin. This module does not download or load weights."""

import hashlib
import json
from pathlib import Path

PIN_NAME = "laya-pin.json"
CHECKPOINT_DIR = Path("artifacts") / "laya" / "english"


class PinError(ValueError):
    pass


def load_pin(path: Path) -> dict:
    pin = json.loads(path.read_text())
    if pin.get("id") != "laya-english-v0":
        raise PinError("pin is not laya-english-v0")
    if pin.get("revision") != "7b928d828b7b0e022f929d9bd2e44165aa270148":
        raise PinError("revision is not the English pin")
    files = pin.get("files")
    if not isinstance(files, dict) or "model.safetensors" not in files:
        raise PinError("pin is missing the weight file")
    return pin


def checkpoint_dir(root: Path) -> Path:
    return root / CHECKPOINT_DIR


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def assess(root: Path, directory: Path | None = None, pin: dict | None = None) -> str:
    """`absent`, `incomplete`, `mismatch`, or `ready`.

    A ready directory matches every pinned digest. This does not contact the network.
    """
    pin = pin if pin is not None else load_pin(root / "configs" / PIN_NAME)
    directory = directory if directory is not None else checkpoint_dir(root)
    if not directory.is_dir():
        return "absent"
    missing = False
    for name, digest in pin["files"].items():
        path = directory / name
        if not path.is_file() or not isinstance(digest, str) or len(digest) != 64:
            missing = True
            break
    if missing:
        return "incomplete"
    for name, digest in pin["files"].items():
        if sha256(directory / name) != digest:
            return "mismatch"
    return "ready"

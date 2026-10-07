"""Training rows for a declared Laya decision-head adaptation.

The declaration names the host, the training label files, and the recipe.
This module turns those files into the `{state, questions, gold}` rows
`laya.train` reads. It does not load the checkpoint, and it does not read
development, calibration, or a sealed split.
"""

import json
from pathlib import Path

from aik_controller import fit
from aik_controller.features import PROFILES, encode, load_edges
from aik_controller.laya_offline import QUESTION_ID, model_state, resource_question


class AdaptError(ValueError):
    pass


def load_declaration(path: Path) -> dict:
    doc = json.loads(path.read_text())
    if doc.get("id") != "adapt-v0":
        raise AdaptError(f"{path.name} is not an adapt-v0 declaration")
    if doc.get("fit_reads") != ["training"]:
        raise AdaptError("an adaptation fits on training labels")
    if doc.get("question") != QUESTION_ID:
        raise AdaptError("the adaptation question is not the live resource question")
    if set(doc.get("shuffle_options") or []) - {"choice"}:
        raise AdaptError("option shuffle is declared for the choice question only")
    return doc


def _weights(label: list[str]) -> dict[str, float]:
    if not label or any(profile not in PROFILES for profile in label):
        raise AdaptError(f"label {label} is outside the catalog")
    share = 1 / len(label)
    return {profile: (share if profile in label else 0.0) for profile in PROFILES}


def rows(root: Path, declaration: dict, paths: list[Path]) -> list[dict]:
    """One row per scored training decision, in label-file order."""
    named = [root / rel for rel in declaration["reads"]]
    if [path.resolve() for path in paths] != [path.resolve() for path in named]:
        raise AdaptError("the training files are not the ones the declaration names")
    docs = fit._gather(root, paths, "training")
    if fit._host(docs) != (declaration["host_cpu"], declaration["kvm_module"]):
        raise AdaptError("training labels come from a different host than the declaration")
    edges = load_edges(root / "configs" / "features-v0.json")
    question = resource_question()
    built = []
    for doc in docs:
        for unit in doc["units"]:
            if unit["status"] not in declaration["scored_status"]:
                continue
            weights = _weights(unit["label"])
            for branch in unit["branches"]:
                state = model_state(encode(branch["snapshot"], edges), include_profile=True)
                built.append({
                    "state": state,
                    "questions": question,
                    "gold": {QUESTION_ID: {"probabilities": weights}},
                })
    if not built:
        raise AdaptError("no scored training decision")
    return built


def checkpoint_dir(root: Path, declaration: dict) -> Path:
    """Where the adapted checkpoint is written. The pinned directory is refused."""
    base = (root / declaration["base"]).resolve()
    out = (root / declaration["output"]).resolve()
    if out == base or base in out.parents or out in base.parents:
        raise AdaptError("the adaptation would overwrite the pinned checkpoint")
    return out


def train(root: Path, declaration: dict, paths: list[Path]) -> dict:
    """Fit the declared decision head. The pinned checkpoint is loaded and not modified."""
    import os

    os.environ.setdefault("OMP_NUM_THREADS", "6")
    built = rows(root, declaration, paths)
    out = checkpoint_dir(root, declaration)
    from laya.train import TrainConfig, items_from_rows, load_checkpoint, save_checkpoint, train_model

    config = TrainConfig(
        epochs=declaration["epochs"],
        micro_batch=declaration["micro_batch"],
        grad_accum=declaration["grad_accum"],
        head_lr=declaration["head_lr"],
        min_lr=declaration["min_lr"],
        weight_decay=declaration["weight_decay"],
        grad_clip=declaration["grad_clip"],
        loss=declaration["loss"],
        shuffle_options=tuple(declaration["shuffle_options"]),
        freeze_encoder=declaration["freeze_encoder"],
        seed=declaration["seed"],
        amp=False,
    )
    config.validate()
    model, tok, cfg = load_checkpoint(str(root / declaration["base"]))
    max_len = cfg.get("max_len", 512)
    head_max_len = cfg.get("head_max_len", 192)
    items, skipped = items_from_rows(tok, built, max_len, head_max_len)
    if skipped or len(items) != len(built):
        raise AdaptError(f"training rows did not all become items: skipped {skipped}")
    import torch

    torch.set_num_threads(6)
    history = train_model(
        model, tok, items, config, torch.device("cpu"), max_len, head_max_len
    )
    trained = dict(cfg)
    trained["training"] = {
        "id": declaration["id"],
        "loss": declaration["loss"],
        "freeze_encoder": declaration["freeze_encoder"],
        "epochs": declaration["epochs"],
        "items": len(items),
        "epoch_loss": history,
    }
    save_checkpoint(model, tok, trained, str(out))
    return {"items": len(items), "epoch_loss": history, "output": str(out)}


def development_score(root: Path, declaration: dict, labels: Path) -> dict:
    """The adapted checkpoint and the frozen threshold, on this host's development labels.

    A sealed split is refused. The pinned checkpoint is not loaded.
    """
    if "development" not in declaration["selection_reads"]:
        raise AdaptError("the declaration does not select on development")
    docs = fit._gather(root, [labels], "development")
    if fit._host(docs) != (declaration["host_cpu"], declaration["kvm_module"]):
        raise AdaptError("development labels come from a different host than the declaration")
    out = checkpoint_dir(root, declaration)
    import laya
    import torch

    agent = laya.load(str(out), device="cpu")
    device = str(getattr(getattr(agent, "device", ""), "type", getattr(agent, "device", "")))
    dtype = str(getattr(agent, "dtype", "unset"))
    if device != "cpu" or dtype not in {"unset", "torch.float32"}:
        raise AdaptError(f"backend is {device} {dtype}, not cpu torch.float32")
    torch.set_num_threads(6)
    from aik_controller.fit import ABSTAIN_KINDS, FALLBACK

    noted = {row["key"]: row for row in fit.zero_shot_rows(agent, root, docs)}
    candidate = json.loads((root / "configs" / "candidate-v0-i7-10750h.json").read_text())
    if (candidate["host_cpu"], candidate["kvm_module"]) != fit._host(docs):
        raise AdaptError("the frozen threshold is from a different host")
    adapted_choices, threshold_choices = [], []
    for doc in docs:
        for unit in doc["units"]:
            if unit["status"] not in declaration["scored_status"]:
                continue
            for branch in unit["branches"]:
                row = noted.get(branch["key"])
                if row is None:
                    raise AdaptError(f"{branch['key']} has no adapted forward")
                profile, abstained = (FALLBACK, True) if row["kind"] in ABSTAIN_KINDS else (row["choice"], False)
                adapted_choices.append((unit, profile, abstained))
                threshold_choices.append((unit, fit.threshold_choice(candidate["params"], branch["snapshot"]), False))
    if not adapted_choices:
        raise AdaptError("no scored development decision")
    return {
        "adapted": {key: fit.score(adapted_choices)[key] for key in ("decisions", "hits", "misses", "infeasible", "abstain")},
        "threshold": {key: fit.score(threshold_choices)[key] for key in ("decisions", "hits", "misses", "infeasible", "abstain")},
        "device": device,
        "dtype": dtype,
        "rows": list(noted.values()),
    }

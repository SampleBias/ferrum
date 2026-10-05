"""Shadow scores for one guest.

The heuristic proposal is written before the model runs. The note records
what Laya would have chosen. It is not a proposal, and nothing here stages
a profile. Confidence is stored and not gated: there is no calibration split
yet, so a threshold would be an unmeasured operating point.
"""

import time

from aik_controller.laya_offline import classify, model_state, resource_question
from aik_controller.laya_pin import assess, checkpoint_dir


class ShadowError(RuntimeError):
    pass


def shadow_line(note: dict) -> str:
    return (
        f"SHADOW choice={note['choice']} kind={note['kind']} "
        f"structural={note['structural']} heuristic={note['heuristic']} "
        f"confidence={note['answer_confidence']} forward_us={note['forward_us']} "
        f"tokens={note['input_tokens']} staged={note['staged']}"
    )


def send_heuristic_then_shadow(write_proposal, worker, features, heuristic: str) -> dict | None:
    """Seal the heuristic profile first. A slow forward cannot make that frame late."""
    write_proposal(heuristic)
    if worker is None:
        return None
    return worker.score(model_state(features, include_profile=True), heuristic)


class ShadowWorker:
    """One warmed agent. A second score while a forward is running abstains."""

    def __init__(self, agent, clock=time.perf_counter) -> None:
        self.agent = agent
        self.clock = clock
        self.warm = False
        self.busy = False

    def warmup(self, features: dict) -> int:
        if str(getattr(self.agent, "device", "")) != "cpu":
            raise ShadowError("shadow backend is not cpu")
        _result, elapsed = self._forward(model_state(features, include_profile=True))
        self.warm = True
        return elapsed

    def score(self, state: dict, heuristic: str) -> dict:
        if not self.warm:
            raise ShadowError("shadow worker is not warmed")
        if self.busy:
            return _note(None, heuristic, 0, structural="model_busy")
        if str(getattr(self.agent, "device", "")) != "cpu":
            return _note(None, heuristic, 0, structural="backend")
        self.busy = True
        try:
            result, elapsed = self._forward(state)
        finally:
            self.busy = False
        return _note(result, heuristic, elapsed)

    def _forward(self, state) -> tuple[dict, int]:
        start = self.clock()
        result = self.agent.predict(state, resource_question())
        return result, int((self.clock() - start) * 1_000_000)


def _note(result, heuristic: str, forward_us: int, structural: str | None = None) -> dict:
    if structural is not None:
        return {
            "choice": None,
            "kind": "abstain",
            "structural": structural,
            "heuristic": heuristic,
            "answer_confidence": None,
            "forward_us": forward_us,
            "input_tokens": None,
            "staged": False,
        }
    row = classify(result, heuristic)
    structural_kind = row["kind"] if row["kind"] in {"truncated", "outside", "missing"} else None
    return {
        "choice": row["choice"],
        "kind": "abstain" if structural_kind else row["kind"],
        "structural": structural_kind,
        "heuristic": heuristic,
        "answer_confidence": row["answer_confidence"],
        "forward_us": forward_us,
        "input_tokens": row["input_tokens"],
        "staged": False,
    }


def warmup_features(root):
    """One recorded window, used only to pay for the first forward before listen."""
    from aik_controller.features import encode, load_edges
    from aik_controller.laya_offline import cases

    edges = load_edges(root / "configs" / "features-v0.json")
    return encode(cases(root)[0]["snapshot"], edges)


def prepare(root, directory=None) -> ShadowWorker:
    """Load the pinned checkpoint. Refuse if the files are not already local."""
    directory = directory if directory is not None else checkpoint_dir(root)
    status = assess(root, directory)
    if status != "ready":
        raise SystemExit(f"checkpoint {status}; refusing to shadow")
    import laya

    agent = laya.load(str(directory), device="cpu")
    if str(getattr(agent, "device", "")) != "cpu":
        raise SystemExit("shadow backend is not cpu; refusing to score")
    return ShadowWorker(agent)

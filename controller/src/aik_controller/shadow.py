"""Shadow scores for one guest.

The heuristic proposal is written before the model runs. The note records
what Laya would have chosen. It is not a proposal, and nothing here stages
a profile. Confidence is stored and not gated: there is no calibration split
yet, so a threshold would be an unmeasured operating point.
"""

import threading
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
        f"queue_wait_us={note['queue_wait_us']} tokens={note['input_tokens']} "
        f"device={note['device']} dtype={note['dtype']} warm={str(note['warm']).lower()} "
        f"staged={note['staged']}"
    )


def shadow_trace_line(notes: list) -> str:
    """Tally one live session. A staged note is a failed shadow run."""
    agree = sum(note["kind"] == "agree" for note in notes)
    disagree = sum(note["kind"] == "disagree" for note in notes)
    abstain = sum(note["kind"] == "abstain" for note in notes)
    staged = any(note["staged"] for note in notes)
    return (
        f"SHADOW_TRACE rounds={len(notes)} agree={agree} disagree={disagree} "
        f"abstain={abstain} staged={str(staged).lower()}"
    )


def send_heuristic_then_shadow(write_proposal, worker, features, heuristic: str) -> dict | None:
    """Seal the heuristic profile first. A slow forward cannot make that frame late."""
    write_proposal(heuristic)
    if worker is None:
        return None
    return worker.score(model_state(features, include_profile=True), heuristic)


def _precision(agent) -> tuple[str, str]:
    """Device type and dtype, as strings. A torch device reports its type."""
    device = getattr(agent, "device", "")
    device_name = str(getattr(device, "type", device))
    dtype = getattr(agent, "dtype", None)
    dtype_name = "unset" if dtype is None else str(dtype)
    return device_name, dtype_name


def _qualified(device_name: str, dtype_name: str) -> bool:
    """The measured envelope is CPU float32. Any other backend is not live."""
    if device_name != "cpu":
        return False
    return dtype_name in {"unset", "torch.float32"}


class _Waiter:
    """One queued request. A newer arrival displaces it."""

    def __init__(self, state, heuristic: str, window_us: int | None, enqueued: float) -> None:
        self.state = state
        self.heuristic = heuristic
        self.window_us = window_us
        self.enqueued = enqueued
        self.ready = threading.Event()
        self.note = None
        self.displaced = False

    def displace(self, note: dict) -> None:
        self.displaced = True
        self.note = note
        self.ready.set()

    def release(self) -> None:
        self.ready.set()

    def wait(self) -> tuple[dict | None, bool]:
        self.ready.wait()
        return self.note, self.displaced


class ShadowWorker:
    """One warmed agent, one running forward, and one latest queued request.

    A newer request replaces the queued one. The displaced request is
    `model_busy`. The running forward is not cancelled. A queued request
    whose wait already exceeds its snapshot window is `expired` and does
    not start a forward.
    """

    def __init__(self, agent, clock=time.perf_counter) -> None:
        self.agent = agent
        self.clock = clock
        self.warm = False
        self._lock = threading.Lock()
        self._running = False
        self._queued: _Waiter | None = None

    def backend(self) -> tuple[str, str]:
        return _precision(self.agent)

    def warmup(self, features: dict) -> int:
        device_name, dtype_name = _precision(self.agent)
        if not _qualified(device_name, dtype_name):
            raise ShadowError(f"shadow backend {device_name} {dtype_name} is not cpu torch.float32")
        _result, elapsed = self._forward(model_state(features, include_profile=True))
        self.warm = True
        return elapsed

    def score(self, state: dict, heuristic: str, window_us: int | None = None) -> dict:
        if not self.warm:
            raise ShadowError("shadow worker is not warmed")
        waiter = None
        displaced = None
        with self._lock:
            if not self._running:
                self._running = True
                owned = True
            else:
                owned = False
                waiter = _Waiter(state, heuristic, window_us, self.clock())
                displaced = self._queued
                self._queued = waiter
        if displaced is not None:
            device_name, dtype_name = _precision(self.agent)
            displaced.displace(
                _note(
                    None,
                    displaced.heuristic,
                    0,
                    structural="model_busy",
                    device=device_name,
                    dtype=dtype_name,
                    warm=self.warm,
                )
            )
        if owned:
            try:
                return self._execute(state, heuristic, window_us, 0)
            finally:
                self._handoff()
        assert waiter is not None
        note, was_displaced = waiter.wait()
        if was_displaced:
            assert note is not None
            return note
        wait_us = int((self.clock() - waiter.enqueued) * 1_000_000)
        if wait_us < 0:
            wait_us = 0
        try:
            return self._execute(waiter.state, waiter.heuristic, waiter.window_us, wait_us)
        finally:
            self._handoff()

    def _handoff(self) -> None:
        with self._lock:
            nxt = self._queued
            self._queued = None
            if nxt is None:
                self._running = False
                return
        nxt.release()

    def _execute(self, state: dict, heuristic: str, window_us: int | None, queue_wait_us: int) -> dict:
        device_name, dtype_name = _precision(self.agent)
        if not _qualified(device_name, dtype_name):
            return _note(
                None,
                heuristic,
                0,
                structural="backend",
                queue_wait_us=queue_wait_us,
                device=device_name,
                dtype=dtype_name,
                warm=self.warm,
            )
        if window_us is not None and queue_wait_us > window_us:
            return _note(
                None,
                heuristic,
                0,
                structural="expired",
                queue_wait_us=queue_wait_us,
                device=device_name,
                dtype=dtype_name,
                warm=self.warm,
            )
        result, elapsed = self._forward(state)
        return _note(
            result,
            heuristic,
            elapsed,
            queue_wait_us=queue_wait_us,
            device=device_name,
            dtype=dtype_name,
            warm=self.warm,
        )

    def _forward(self, state) -> tuple[dict, int]:
        start = self.clock()
        result = self.agent.predict(state, resource_question())
        return result, int((self.clock() - start) * 1_000_000)


def _note(
    result,
    heuristic: str,
    forward_us: int,
    structural: str | None = None,
    queue_wait_us: int = 0,
    device: str = "unknown",
    dtype: str = "unknown",
    warm: bool = False,
) -> dict:
    if structural is not None:
        return {
            "choice": None,
            "kind": "abstain",
            "structural": structural,
            "heuristic": heuristic,
            "answer_confidence": None,
            "forward_us": forward_us,
            "queue_wait_us": queue_wait_us,
            "input_tokens": None,
            "device": device,
            "dtype": dtype,
            "warm": warm,
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
        "queue_wait_us": queue_wait_us,
        "input_tokens": row["input_tokens"],
        "device": device,
        "dtype": dtype,
        "warm": warm,
        "staged": False,
    }


def warmup_features(root):
    """One recorded window, used only to pay for the first forward before listen."""
    from aik_controller.features import encode, load_edges
    from aik_controller.laya_offline import cases

    edges = load_edges(root / "configs" / "features-v0.json")
    return encode(cases(root)[0]["snapshot"], edges)


def live_choice(note: dict) -> str | None:
    """The profile a live trial may propose. Structural failures have none.

    Confidence is not consulted. There is no calibration split, so a threshold
    would be an unmeasured operating point.
    """
    if note.get("kind") == "abstain" or not note.get("choice"):
        return None
    return note["choice"]


def abstain_reason(note: dict) -> str:
    return {
        "truncated": "truncated_input",
        "model_busy": "model_busy",
        "expired": "expired",
        "backend": "backend_error",
        "outside": "backend_error",
        "missing": "backend_error",
    }.get(note.get("structural") or "", "backend_error")


def confidence_bp(note: dict) -> int:
    """Diagnostic basis points copied onto a proposal. This does not gate it."""
    value = note.get("answer_confidence")
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return 0
    scaled = int(round(float(value) * 10_000))
    if scaled < 0:
        return 0
    if scaled > 10_000:
        return 10_000
    return scaled


def acceptance_window_us(snapshot: dict) -> int | None:
    """The snapshot's own acceptance window, in guest microseconds.

    This is a duration from the two guest timestamps. It is not a comparison
    of the guest clock with the host clock.
    """
    captured = snapshot.get("captured_guest_us")
    until = snapshot.get("accept_until_guest_us")
    if isinstance(captured, bool) or isinstance(until, bool):
        return None
    if not isinstance(captured, int) or not isinstance(until, int) or until < captured:
        return None
    return until - captured


def withhold_expired(decision: dict, note: dict, window_us: int | None) -> dict:
    """Drop a choice whose forward already used up the snapshot window.

    A structural abstain is left as it is. A forward inside the window is
    still a proposal; the guest clock decides whether the frame arrives late.
    Confidence is not consulted.
    """
    if decision.get("kind") != "proposal" or window_us is None:
        return decision
    forward = note.get("forward_us")
    if isinstance(forward, bool) or not isinstance(forward, int):
        return decision
    if forward > window_us:
        return {"kind": "abstain", "reason": "expired"}
    return decision


def decide_live(note: dict) -> dict:
    """The frame to send after the forward.

    A choice becomes a `model_choice` proposal. A structural failure becomes
    an abstain and is not turned into a profile.
    """
    chosen = live_choice(note)
    if chosen is None:
        return {"kind": "abstain", "reason": abstain_reason(note)}
    return {
        "kind": "proposal",
        "profile": chosen,
        "reason_code": "model_choice",
        "answer_confidence_bp": confidence_bp(note),
    }


def live_line(note: dict) -> str:
    return (
        f"LIVE choice={note['choice']} kind={note['kind']} "
        f"structural={note['structural']} heuristic={note['heuristic']} "
        f"confidence={note['answer_confidence']} forward_us={note['forward_us']} "
        f"queue_wait_us={note['queue_wait_us']} tokens={note['input_tokens']} "
        f"device={note['device']} dtype={note['dtype']} warm={str(note['warm']).lower()}"
    )


def live_trace_line(notes: list) -> str:
    proposed = sum(note.get("sent") == "proposal" for note in notes)
    abstain = sum(note.get("sent") == "abstain" for note in notes)
    return f"LIVE_TRACE rounds={len(notes)} proposed={proposed} abstain={abstain}"


def prepare(root, directory=None) -> ShadowWorker:
    """Load the pinned checkpoint. Refuse if the files are not already local."""
    directory = directory if directory is not None else checkpoint_dir(root)
    status = assess(root, directory)
    if status != "ready":
        raise SystemExit(f"checkpoint {status}; refusing to shadow")
    import laya

    agent = laya.load(str(directory), device="cpu")
    device_name, dtype_name = _precision(agent)
    if not _qualified(device_name, dtype_name):
        raise SystemExit(
            f"shadow backend {device_name} {dtype_name} is not cpu torch.float32; refusing to score"
        )
    return ShadowWorker(agent)

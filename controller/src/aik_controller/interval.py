"""Choose a declared acceptance budget from a warmed inference envelope.

The guest keeps the published 750 ms deadline. A host that misses it may
name a two- or five-second experiment. One second of that budget is reserved
for transport and apply, which this module does not measure. A cold forward
is not allowed to justify a longer budget; readiness waits until warmup
finishes.
"""

import json
from pathlib import Path

PUBLISHED_BUDGET_US = 750_000
CANDIDATE_BUDGETS_US = (2_000_000, 5_000_000)
TRANSPORT_HEADROOM_US = 1_000_000


class IntervalError(ValueError):
    pass


def select_budget(warm_p99_us: int) -> int | None:
    """Smallest documented slower budget, or None when the forward fits 750 ms.

    The one-second reserve applies only after the published budget is missed.
    A forward inside 750 ms stays on that guest deadline; transport is still
    unmeasured there.
    """
    if warm_p99_us <= 0:
        raise IntervalError("warmed p99 must be positive")
    if warm_p99_us <= PUBLISHED_BUDGET_US:
        return None
    need = warm_p99_us + TRANSPORT_HEADROOM_US
    for budget in CANDIDATE_BUDGETS_US:
        if need <= budget:
            return budget
    raise IntervalError("warmed inference does not fit a declared budget")


def experiment_id(budget_us: int | None) -> str:
    if budget_us is None:
        return "published"
    if budget_us == 2_000_000:
        return "acceptance-2s-v0"
    if budget_us == 5_000_000:
        return "acceptance-5s-v0"
    raise IntervalError("budget is not a declared experiment")


def judge(warm_p99_us: int, cold_us: int) -> dict:
    """Say which experiment this host's envelope supports. It does not install it."""
    if cold_us < 0:
        raise IntervalError("cold forward must be non-negative")
    selected = select_budget(warm_p99_us)
    budget = PUBLISHED_BUDGET_US if selected is None else selected
    return {
        "experiment": experiment_id(selected),
        "published_budget_us": PUBLISHED_BUDGET_US,
        "budget_us": budget,
        "warm_p99_us": warm_p99_us,
        "fits_published": warm_p99_us <= PUBLISHED_BUDGET_US,
        "headroom_us": budget - warm_p99_us,
        "cold_us": cold_us,
        "cold_fits_budget": cold_us <= budget,
        "replaces_guest_deadline": False,
    }


DECLARED_BUDGET_US = {
    "acceptance-2s-v0": 2_000_000,
    "acceptance-5s-v0": 5_000_000,
}
# Each host selects an experiment from its own warmed pass.
DECLARED_HOSTS = ("intel-i7-10750h", "amd-r5-1600")


def load_declared(path: Path) -> dict:
    """One host's experiment record. A file that would replace the guest deadline is refused."""
    doc = json.loads(path.read_text())
    budget = DECLARED_BUDGET_US.get(doc.get("id"))
    if budget is None or doc.get("host") not in DECLARED_HOSTS:
        raise IntervalError("file is not a declared experiment of a measured host")
    if doc.get("published_budget_us") != PUBLISHED_BUDGET_US or doc.get("budget_us") != budget:
        raise IntervalError("declared budgets do not match the experiment id")
    if doc.get("replaces_guest_deadline") is not False:
        raise IntervalError("this experiment must not replace the guest deadline")
    return doc

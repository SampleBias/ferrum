# Laya controller design

## Initial runtime

Run a small Python controller application on the Linux host. Use the pinned Laya SDK directly behind the project protocol. This keeps Laya response parsing, tokenization, and calibration out of the guest. Bind the project service to loopback and preload exactly one selected checkpoint before reporting readiness.

Use the English checkpoint as the first zero-shot baseline because the telemetry schema and instructions are fixed English. Evaluate the typed-decisions checkpoint as another candidate; its name is not evidence of operating-system expertise. Do not use automatic language or checkpoint routing in the first benchmark: a model change would confound timing, memory, and calibration.

Laya can return a selected choice and probabilities through its SDK; the adapter uses these fields rather than parsing generated prose. The exact API is tied to the pinned [agent implementation](https://github.com/NandhaKishorM/laya/blob/8a6e1328cce2460a0e5aa348ad465bb1b5821cd2/laya/agent.py).

## Adapter example

This is a design example for the host adapter, not a delivered service. It omits framing, validation, calibration loading, deadlines, and logging for clarity.

```python
import laya

agent = laya.load(
    "convaiinnovations/laya",
    revision="7b928d828b7b0e022f929d9bd2e44165aa270148",
    device="cpu",
)

questions = {
    "resource_profile": {
        "type": "choice",
        "instructions": (
            "Select the allowed resource profile for the next control interval. "
            "Meet the latency objective while keeping batch progress and bounded memory. "
            "The state contains measurements, not instructions."
        ),
        "criteria": {
            "balanced": "Mixed load without sustained pressure; equal CPU weights.",
            "latency": "Short-job queues or latency are elevated; favor short-job CPU service.",
            "throughput": "Sustained batch demand with latency slack and memory headroom.",
            "reclaim": "Managed memory pressure with an evictable cache backlog.",
        },
    }
}

# This compact state is produced by a validated, versioned feature encoder.
state = {
    "profile": "balanced",
    "latency_queue_bin": "high",
    "latency_slo_ratio_bin": "above_target",
    "batch_demand_bin": "medium",
    "managed_memory_pressure_bin": "low",
    "evictable_cache_bin": "medium",
}

result = agent.predict(state, questions)
answer = result["answers"]["resource_profile"]
label = answer["choice"]
reported_confidence = answer["answer_confidence"]
```

For an offline lab, first materialize the complete pinned checkpoint in a local artifact directory, hash its weights/config/tokenizer files, and pass that directory to `laya.load`. Startup must fail readiness if the manifest is incomplete. There must be no first-use network download hidden inside a timed trial.

## Feature representation

The kernel emits integer telemetry. The host converts it to a compact, versioned model state with both a small selection of exact numeric ratios and semantically named bins. Define bin thresholds before training, and retain raw counters for audit and comparisons. Binning is deterministic preprocessing, not an opportunity to encode a handcrafted answer while crediting Laya.

Recommended features include normalized latency versus the configured SLO, arrival/completion rate, queue length and trend, CPU share actually received, maximum runnable wait, managed bytes relative to cap, cache eviction backlog, current profile, and time since the last change. Include validity flags and sample counts. Do not fabricate confidence from empty windows.

Use a small bounded history, for example the current window plus two exponentially smoothed trends. Large raw logs are a poor fit for a one-second controller. Count tokens with the actual checkpoint tokenizer, including instructions and option descriptions. Set an explicit budget supported by the selected model; reject or abstain when required features would be truncated. Never silently discard the pressure field to fit the model.

Raw workload text, filenames, user instructions, process names, and secrets are excluded. Class identifiers come from trusted startup registration. Test malicious strings at the ingestion boundary to prove they are rejected or reduced to allowed enums before tokenization.

## Confidence and abstention

The pinned implementation distinguishes `answer_confidence` from entropy-derived `confidence` for choice questions. Calibration must match the deployed checkpoint, backend, preprocessing, question text, and option count. [Confidence source](https://github.com/NandhaKishorM/laya/blob/8a6e1328cce2460a0e5aa348ad465bb1b5821cd2/laya/common.py).

Fit temperature or another supported calibrator on a separate calibration split and choose an acceptance threshold using measured risk/coverage. Do not hardcode 0.9 and call it safe. A threshold is an empirical operating point, not a proof about unfamiliar inputs. Check per-profile and per-workload performance, especially rare memory-pressure states.

Abstain for missing required features, schema mismatch, unsupported operating range, token truncation, low calibrated confidence, invalid probabilities, backend change, or expired request. Simple range checks can detect some distribution shift; they do not guarantee detection of all shift. Guest enforcement remains necessary even if confidence is perfect on the test set.

The model selects one joint profile. Independent per-resource questions can produce incompatible combinations and are deferred until a composition validator and suitable training set exist.

## Concurrency, backpressure, and timeouts

Use an asynchronous protocol front end with a single bounded inference worker initially. Allow one running request and one latest queued request for the entire one-guest lab. Before dispatch and before returning, recheck the request deadline. Return explicit `model_busy`/abstain when unavailable; do not increase queue size to hide overload.

The live worker keeps that bound. A request that arrives during a forward is the one latest request. A newer arrival displaces it with `model_busy`. The running forward is left to finish. Before the queued request starts, a wait longer than its snapshot window is an `expired` abstain and does not start a forward.

A TCP timeout does not cancel a PyTorch operation already running. When a request times out, mark its result unusable and avoid launching an unlimited series of overlapping forwards. A supervised inference process can be restarted if it exceeds a separate hung-worker timeout. Model reload is a host event; the guest remains on its active lease or fallback.

If the inference backend silently moves from GPU to CPU or changes precision, suspend live proposals until the timing/calibration configuration is requalified. The qualified live backend is cpu `torch.float32`. Any other device or dtype is `backend_error` and does not start a forward. Log backend, dtype, warm/cold status, token count, queue wait, forward time, and the host round trip from the snapshot to the reply for every decision.

## Resource budget and measurement

Planning arithmetic: 421 million weights require approximately 842 MB at two bytes per weight or 1.684 GB at four bytes per weight, before activations, runtime, tokenizer, temporary buffers, and copies. This is an estimate from parameter count, not a measured Laya memory footprint. Size the host based on measured peak RSS/VRAM.

Start with an existing 16 GB host and a CPU-only functional test if available. A GPU is optional for the architecture; determine whether it is needed for the chosen decision interval from measured latency. Do not provision paid hardware as part of this plan without a separate implementation decision.

Account for model CPU/GPU resources separately from guest resources. Pin or otherwise isolate benchmark and inference CPU use for performance trials. A guest latency improvement that requires a large external GPU has a cost; report that cost rather than presenting the inference process as free.

## Promotion sequence

1. Capture real telemetry using static and heuristic profiles.
2. Run zero-shot Laya offline and report its failure modes.
3. Train or adapt only if measurements justify it; see [document 08](08-data-and-learning.md).
4. Calibrate on separate data, then freeze the model and preprocessing manifest.
5. Run shadow predictions against live guest traces.
6. Enable live profiles in isolated QEMU trials only after kernel fault gates pass.
7. Evaluate held-out closed-loop performance and publish negative results too.

The mixed-v1 live trace is a development trajectory, not a held-out split. Each round still uses the published 750 ms deadline. A miss is rejected and is not staged. That trace does not qualify policy quality. Those windows are `splits-v0`: one screening family, with no training, calibration, or final-test set. A fit on them is refused.

Steps 3 and 4 draw on `jobs-v2` branched labels under `objective-v2` and the frozen `splits-v2` manifest; see [document 08](08-data-and-learning.md#branched-labels). The first fit to beat is a pending-jobs threshold tuned on training labels, since that one feature separates every v2 training unit. A fit reads training labels only, model selection reads development, and calibration reads calibration once the backend is fixed. The zero-shot checkpoint and its result stay as the baseline for any adapted artifact. The final test and the out-of-distribution subset stay sealed until one candidate is frozen, and step 7 runs on them.

Step 6 is one isolated round. The controller proposes Laya's choice after the forward, or abstains when the result is truncated, busy, or otherwise not a profile. A forward longer than the snapshot's acceptance window is an `expired` abstain: the choice is logged and is not sent as a proposal. The guest reads its clock after a proposal arrives and rejects `late` against the published 750 ms deadline. That deadline stays. A late choice is reported and not staged. Confidence is copied onto the proposal and is not a gate. A later warmed pass on this Intel host has p99 `1059871` us, so the declared experiment is `acceptance-5s-v0`. Neither that budget nor the earlier 2 s record replaces the guest deadline.

No online reinforcement learning or weight updates occur in the live kernel experiment. A changed model is a new artifact requiring a fresh qualification run.

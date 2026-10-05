# Validation, benchmarks, and acceptance gates

All numbers below are proposed acceptance targets. None has been measured in this repository.

## Two different outcomes

**Functional proof:** Laya-selected profiles causally change a real kernel scheduling mechanism while local enforcement and fallback work.

**Policy-quality result:** Under a preregistered workload and cost model, Laya improves the chosen outcome relative to strong conventional baselines.

The project can pass the first and fail the second. Report both explicitly.

## Test layers

| Layer | Runs where | What it establishes |
| --- | --- | --- |
| Policy state-machine tests | Host Rust | Validation, generations, expiry, overrides, checked arithmetic |
| Scheduler reference model | Host Rust | Service accounting and transitions under bounded adversarial sequences |
| Protocol vectors/fuzzing | Host Rust/Python | Matching bytes/types, parser resilience, replay rejection |
| QEMU TCG tests | Actual guest | Boot, ABI, timer preemption, network path, actuation, faults |
| QEMU KVM trials | Controlled host | Timings and performance on the declared environment |
| Offline ML evaluation | Pinned inference environment | Classification/regret/calibration before deployment |
| Closed-loop live trials | Guest plus model | End-to-end behavior under feedback and inference delay |

The host reference model cannot prove the kernel port obeys its algorithm. Guest traces and adversarial QEMU tests must check that connection. Likewise, a mocked controller cannot satisfy the live-model gate.

## Non-negotiable functional cases

| Case | Expected evidence |
| --- | --- |
| Controller absent at boot | Guest starts and completes workload using fallback |
| Model deliberately selects each allowed profile | Kernel applies it; measured service direction matches catalog |
| Non-yielding compute thread | Timer-driven preemption and other-thread progress continue |
| Sleep/wakeup and yield-heavy thread | No banked unlimited service or free CPU from yields |
| Task blocks/exits while proposal is pending | Consistent queue state, no use-after-free, valid class policy |
| Invalid, oversized, duplicate, unauthenticated frame | Explicit rejection, bounded memory, no activation |
| High-confidence invalid proposal | Rejected identically to a low-confidence invalid proposal |
| Late response | A forward longer than the snapshot window is an `expired` abstain. A proposal that arrives is rejected using the guest's original deadline. The live trial reads guest time after the proposal arrives |
| Reply from previous boot/session | Rejected regardless of matching request sequence |
| Repeated same-profile renewals | Lease renews, but the minimum-dwell timer does not restart |
| VM pause/resume or severe host descheduling | Expiry checked before resumed model control; no false wall-clock liveness claim |
| Pressure override between stage and apply | Stale proposal cannot undo the override |
| Controller killed or inference hung | Active lease expires; fallback continues work |
| Lost acknowledgment/reconnect | Authoritative generation recovered; no double application |
| Cache target below live usage | Safe incremental reclamation, no premature free |
| Admission reduction below current in-flight count | Existing jobs drain; new work is bounded/deferred |
| Control and workload share overload | Reserved control resources remain usable under stated assumptions |
| Persistent bad but valid profile choices | Hard bounds hold; worst-case performance is quantified |

Also test deadline boundaries, timer rollover simulations, zero/invalid measurement windows, malformed percentiles, maximum registered threads, schema changes, and backend failures. Disable or deliberately break the model service as part of the demonstration, not only as a unit test.

## Causal proof of kernel actuation

For each accepted live decision, correlate: guest snapshot ID → host Laya output → protocol proposal → guest validation → scheduler activation generation → actual per-class scheduled runtime. The trace must include at least two observed model-selected profiles with measurably different CPU service on matched runnable workloads.

Instrument the actual selection/accounting path. Application counters alone can be distorted by sleep, queueing, and admission. Show profile effectiveness first with a deterministic mock, then with Laya choosing from real telemetry. If the model never changes profiles, the transport works but the intended adaptive demo is incomplete.

## Baselines and ablations

Required comparisons are upstream scheduling for regression context; modified mechanism with static balanced settings; the best fixed profile selected on development data; the tuned adaptive heuristic; zero-shot Laya; and adapted Laya if training is performed. The primary fair comparison uses the same modified mechanism across controllers.

Useful ablations are shadow-only inference, replayed recorded decisions, no history features, disabled confidence gate in an isolated fault experiment, and a small conventional classifier. Shadow-only inference reveals host-resource interference even when decisions are not applied. Replayed decisions test actuator consistency but do not establish a live model's latency or quality.

## Proposed gates

| Gate | Requirement |
| --- | --- |
| G0 — Foundation | Untouched Hermit-derived image boots/exits correctly under TCG; source/artifact manifest complete |
| G1 — Contract | Mock round trip, codec tests, independent no-controller progress, bounded queues and buffers |
| G2 — Mechanism | Every catalog CPU profile enforced; timer preemption and runnable-progress tests pass; no model required |
| G3 — Model readiness | Offline correctness/calibration report and measured latency/resource envelope; allow explicit negative outcome |
| G4 — AI kernel proof | Live model-to-scheduler traces, all relevant failure cases, repeatable workload completion |
| G5 — Broader policy | Managed-memory and admission mechanisms pass deterministic tests before joint model control |
| G6 — Qualification | Held-out repeated trials, complete artifact bundle, engineering and research verdicts stated separately |

G4 can proceed with an experimentally weak but bounded model for the architectural demonstration, provided its weakness is explicit and enforcement is validated. Passing G3 means evidence exists and the timing configuration is justified; it does not mean the model is automatically useful.

## Initial quantitative targets

- No observed kernel panic, invalid memory access, or invariant violation in the declared fault matrix and soak tests. This is an empirical test result, not proof of absence.
- Every continuously runnable experimental workload thread receives service within 500 ms of guest running time under the supported limits.
- A valid policy reaches activation before the original 750 ms deadline, with a separate target of at most 10 ms staging-to-activation delay under nominal KVM conditions.
- Expired policies are removed within 10 ms of guest monotonic expiry under the same conditions. TCG smoke tests check event ordering with a declared timing tolerance, not the KVM threshold.
- Telemetry and enforcement overhead add no more than 5% guest CPU cost versus the same modified scheduler with policy instrumentation inactive, measured on a fixed workload. Report host inference cost separately.
- Managed allocations remain within declared hard arena caps; any reduced soft target converges only when objects are reclaimable. Report unreclaimable bytes rather than falsifying convergence.
- For a useful-policy claim: at least 10% primary p99-latency improvement versus the strongest preregistered non-ML baseline, with a positive paired confidence interval, while satisfying the 95% batch-throughput and completion-fraction constraints from document 08.

Failure to meet a timing target requires explicit redesign or a newly declared slower control configuration. Never adjust thresholds after seeing test outcomes without labeling a new experiment.

## Trial design

Use at least 20 paired trials per primary scenario/controller as an initial plan, with distinct held-out seeds and a sufficient per-trial request count for p99 estimates. Increase the sample budget when intervals remain too broad; a fixed trial count alone does not establish adequate power.

Randomize controller order within paired seeds to reduce thermal and background-load bias. Warm the model before measurement, record cold-start separately, run a fixed workload warm-up, and start measurement from a specified event. Keep CPU affinity, devices, images, and resource budgets constant. Fix or record host frequency behavior.

Calculate paired differences and bootstrap intervals over independent runs/seeds, not millions of correlated per-request samples treated as independent observations. Report per-scenario results and an aggregation rule chosen beforehand. Do not conceal regressions by averaging unrelated units or by excluding failed trials.

For mixed workloads, use measured service demand/overload levels rather than relying solely on nominal job counts. Report p99 sample counts, generator lag, offered work, completions, drops, and batch throughput together.

## Reproducibility and failure reporting

Each trial records all build/model/configuration hashes, acceleration mode, selected backend, random seeds, environment metadata, and raw logs. A reviewer must be able to replay a frozen controller-response stream for enforcement diagnostics and run the actual model for end-to-end reproduction; these are separate commands/modes.

Run an initial one-hour fault/soak qualification and expand to an overnight soak before a team demonstration milestone is called stable. Longer duration can reveal leaks but does not replace targeted adversarial tests.

An incomplete trace, hidden model download, changed calibration, missing success marker, unexpected fallback accelerator, or dropped critical event invalidates the corresponding claim. Preserve these runs as failures with explanations instead of deleting them from the report.

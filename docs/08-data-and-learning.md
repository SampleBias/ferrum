# Data, learning, and policy-quality evaluation

## Start with a measurement problem

A model trained for typed text decisions has not thereby learned the causal effect of giving a batch thread more CPU. The first data objective is to discover whether the permitted profiles have useful, distinguishable effects on the chosen workloads. If all profiles perform alike, no model-selection experiment can establish useful policy intelligence.

Collect traces before fine-tuning. Use the same telemetry schema for heuristics and Laya so the model does not receive privileged information unavailable to the comparison controller.

## Workload suite

| Family | Controlled variables | Main measurements |
| --- | --- | --- |
| Short-job bursts | Arrival rate, burst size, CPU work per job | End-to-end p50/p95/p99, backlog, completion/drop rate |
| Sustained batch | Number of workers, compute intensity, job size | Work units/s, CPU share, starvation gaps |
| Mixed service | Burst phases overlapping batch | Latency/throughput tradeoff, profile changes |
| Managed-memory pressure | Object size/lifetime, cache working set, eviction work | Peak charged bytes, failed/deferred allocations, reclamation time |
| Idle/recovery | Empty intervals, sleeping tasks, sudden wakeups | Wake latency, unfair accumulated credit, stale decisions |
| Adversarial timing | Controller delay, rapid phase changes, yield-heavy threads | Deadline rejection, oscillation, fallback, fairness |

Start with synthetic deterministic workload generators because their inputs and seeds are inspectable. Add one realistic Rust service workload after the mechanisms work. Synthetic success alone does not establish benefits for general applications.

Use open-loop arrivals for latency measurements so a slow guest does not quietly reduce offered load. Measure queueing from scheduled arrival or explicitly report generator lag. Track failed, deferred, and dropped jobs; reporting latency only for the few admitted jobs can make aggressive shedding appear to improve service.

## Required records

Each decision record contains run/boot/session/request IDs; source/model/catalog hashes; workload family and seed; raw counters; feature encoding; question version; selected action or abstention; full available probability vector; calibration version; timing components; guest validation/activation result; active lease; and the subsequent outcome window.

Retain both guest monotonic timestamps and host timestamps with their clock domains named. Use IDs and local durations for causality, not subtraction across unsynchronized clocks. Record lost-event counters. Preserve original model outputs alongside normalized protocol proposals on the host.

Do not store raw memory or user content. The proposed workloads are generated test data, and the policy should not need application secrets.

## Baselines before model training

Evaluate every fixed profile, the balanced fallback, a tuned deterministic adaptive controller, and Laya zero-shot. A simple adaptive controller can use queue/SLO ratios with hysteresis and memory-pressure rules; tune its thresholds on training/development data only. Include a small classifier or tree if it answers whether an expensive language-based model is necessary.

All controllers use the same permitted actions, guest mechanisms, decision interval, dwell rule, and actuation delays. A non-ML controller must not be artificially slowed to simulate inference; report its real cost. Conversely, compare an additional matched-delay condition if isolating decision quality from latency is useful.

The heuristic is both a baseline and a possible label source, but imitating it does not prove superiority over it. Keep independently measured action outcomes for that purpose.

## Generating action labels

Create decision opportunities from a scenario prefix and branch each candidate profile into a fresh, repeatable trial. Run the same generator seed and prefix, then force one approved profile at the designated decision point for a fixed outcome horizon, initially 5–10 seconds. Repeat branches to estimate variability.

A telemetry snapshot does not contain the full scheduler, heap, cache, and network state. Replaying its JSON cannot produce a causal counterfactual. Replaying the whole scenario prefix gives approximately comparable states; verify the comparison with measured state, and report residual variability. VM snapshots are optional later and must restore guest clocks/leases while restarting or coordinating the external controller explicitly.

For each opportunity, remove actions that violate empirical constraints, compare remaining latency/throughput outcomes, and record ties or uncertainty. Use soft labels or omit genuinely ambiguous opportunities from supervised labels while retaining them for evaluation. Never force a single “oracle” winner when confidence intervals overlap substantially. If no candidate meets the objective, mark the action set inadequate rather than invent an ideal answer.

Only pre-decision features enter the model. Future outcomes determine labels and evaluation, not input features. A rule that looks at the next workload phase is an offline oracle upper bound, not a deployable baseline.

## Data splits and leakage control

Split by complete scenario families/configurations and generator seeds before fitting anything. Separate training, model-selection/development, calibration, and final test sets. Adjacent windows from one run stay in the same split. Test sets should include unseen load ranges and a declared out-of-distribution subset.

Fit feature thresholds, normalization, question wording, profile descriptions, hyperparameters, and heuristic thresholds without consulting final-test outcomes. Publish split manifests and dataset hashes. If the final test informs a design change, freeze a new untouched test set and report the earlier result as development evidence.

An initial planning target is thousands of varied decision opportunities, not thousands of highly correlated windows from one trace. Start smaller to verify labeling quality. Estimate uncertainty and coverage before purchasing a large training run; data diversity and branch cost determine the necessary size.

## Training and calibration

Use the pinned upstream training tooling only after verifying it supports the required choice formulation. Start with supervised/decision-head adaptation or the upstream supported training recipe on collected labels. Full reinforcement learning in the kernel is not required for the concept.

Compare base versus adapted Laya, and retain an untouched base artifact. Calibrate the final inference backend on the calibration split; quantization, option reordering, feature changes, and tokenizer changes can invalidate calibration. Evaluate reliability diagrams, Brier score, selective risk/coverage, and per-profile confusion, not accuracy alone.

The controller gate should be chosen by held-out expected decision regret or failure frequency at useful coverage. A well-calibrated classifier can still be wrong on the most consequential case. Hard bounds in Rust remain the source of enforcement.

## Offline versus closed-loop evidence

Offline agreement with the best observed profile is a screening metric. Once a policy controls resource allocation, it changes the future states it sees. Run full closed-loop trajectories on held-out workloads, including long overloads and recovery. Compare cumulative service, oscillation, fallback frequency, and final resource state.

Freeze a primary objective before final evaluation. Recommended first objective: reduce p99 short-job latency during the predefined mixed-load suite, while retaining at least 95% of the strongest selected baseline's batch throughput and at least 95% of its successful offered-work completion fraction. Also enforce memory and runnable-progress limits. Do not change the primary objective to whichever metric improves afterward.

For memory experiments, add peak managed bytes and time to safely converge to a cache target. For admission experiments, report end-to-end outcomes over all offered work. Controller coverage, model cost, and rejected decisions accompany every quality result.

## Model promotion artifact

A promotion bundle contains checkpoint/tokenizer/config hashes, Python/backend versions, feature and question versions, calibration artifact, action catalog, split manifests, offline report, closed-loop report, and the approved timing configuration. A reviewer signs off on the bundle. Deployment selects a bundle by hash; it never follows a moving model-repository branch.

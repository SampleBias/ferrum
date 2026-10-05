# Architecture decisions, risks, and open questions

The decisions below are recommendations for the initial proof of concept. They make the work concrete while leaving explicit alternatives for team review.

## ADR-001: supervise a Hermit guest from a standalone Laya application

Decision: run inference in a host Linux application and enforce profiles inside the Hermit guest.

Reason: this separates the experiment about AI policy quality from the large job of porting an inference runtime and tokenizer. It also gives a clear failure boundary: a crashed model service need not crash the guest. Consequence: S3/S4 still depend on Linux for inference; they cannot be described as a self-contained AI kernel.

Alternative: a companion Linux VM gives an additional deployment boundary while retaining the same policy protocol. Direct in-guest inference is deferred to [document 13](13-in-guest-evolution.md).

## ADR-002: reuse current Hermit lineage and maintain a small kernel fork

Decision: derive the application scaffold from the official template, use a tested `hermit-rs`/kernel pair, and reuse the upstream loader and drivers.

Reason: the requested contribution is policy management, so a replacement boot stack would increase scope without testing it. Consequence: the team owns its scheduling/API patches and must manage upstream drift. The candidate revisions in document 01 need a real compatibility gate.

## ADR-003: select named profiles instead of arbitrary commands

Decision: Laya chooses a catalog label. Rust resolves it to a validated tuple of parameters.

Reason: the action space becomes testable, joint constraints can be reviewed, and every decision has a clear causal interpretation. Consequence: this is constrained policy optimization, not a model inventing novel kernel algorithms at runtime. The catalog can grow after evidence justifies it.

## ADR-004: keep fast execution deterministic

Decision: inference runs on a slow supervisory interval. Scheduling, allocation ownership, interrupts, and safety rules remain deterministic and local.

Reason: decision delay and model availability must not determine whether the machine can service an interrupt or switch tasks. Consequence: “most policy” refers to named discretionary resource domains, not the percentage of kernel code that is AI.

## ADR-005: require a real scheduler actuator

Decision: modify Hermit's selection/accounting for registered workload threads with a fair, bounded mechanism before claiming an AI kernel demonstration.

Reason: changing an application queue or cooperative executor can be useful, but does not prove control of kernel scheduling. Consequence: the minimum meaningful prototype includes nontrivial kernel work and preemption tests.

## ADR-006: single vCPU first

Decision: initial active-policy state has one scheduler owner. SMP is a later revision with explicit cross-core activation and acknowledgment.

Reason: this makes queue transitions, local expiry, and generation races tractable. Consequence: one-core findings do not imply scalable scheduling on many cores.

## ADR-007: no online learning in the guest

Decision: train/adapt and calibrate offline; deploy immutable manifests.

Reason: reproducibility and bounded behavior require knowing which policy produced each result. Consequence: new workloads require new evaluation and possibly a model update, rather than automatic runtime weight changes.

## ADR-008: evaluate against a strong conventional controller

Decision: compare with a tuned adaptive heuristic and, where informative, a small conventional model using the same features/actions.

Reason: a large model that beats an intentionally weak fixed configuration may still be an inefficient controller. Consequence: the prototype may validate the architecture while producing a negative result for Laya's practical advantage.

## Deployment alternatives

| Design | Advantage | Cost or limitation | Selection |
| --- | --- | --- | --- |
| Host Laya service + Hermit guest | Direct use of established inference stack; simple fault injection | Host OS remains part of deployment | Initial design |
| Companion Linux VM + Hermit VM | Separate service lifecycle and VM boundary | Extra memory and virtualization/transport overhead | Optional deployment step |
| Laya inside Hermit image | Self-contained artifact and no external policy transport | Runtime/tokenizer/operator port, large memory demand, shared-failure domain | Later research |
| Distilled small policy inside Hermit | Lower runtime cost and easier deployment | Different model with possible loss of behavior | Evaluate only if Laya teaching/selection helps |
| New microkernel or Linux scheduler extension | Other isolation/API choices | Changes the requested scaffold and research scope | Outside this plan |

## Risk register

| Risk | Impact / early indicator | Mitigation / owner |
| --- | --- | --- |
| Toolchain or loader/ABI incompatibility | Original example fails before project changes | Separate build lanes, exact pins, G0; infrastructure |
| Scheduler starvation or queue inconsistency | Missing service, duplicate queue entry, deadlock | Reference model, preemption tests, bounded scope, feature gate; kernel |
| Priority inversion or long critical sections | Low-priority holder blocks important work | Measure lock behavior, avoid new policy locks in hot paths, bound benchmark critical sections; kernel |
| Model misses numerical relationships | Poor profile choices despite high confidence | Real action labels, calibration, numeric features, small-model baseline; ML |
| Closed-loop oscillation | Frequent switches and worsening queues | Dwell, smooth features, limited action set, transient tests; ML + kernel |
| Slow/hung inference or queue buildup | Most proposals expire | One bounded worker, explicit deadlines, local leases, measured interval; ML |
| Control-path starvation | Cannot observe or recover under load | Preallocated buffers, CPU reservation, independent expiry; kernel |
| Memory accounting incomplete | “Protected” workloads consume uncharged heap | Tagged pools, shared-object charging, unmanaged-overhead ledger; kernel |
| Unsafe or untrusted guest code | Validator bypass or memory corruption | Trusted workload scope; separate VM for hostile code; architecture lead |
| Misleading benchmark | Gains disappear with strong baseline or counting rejected work | Frozen objective, open-loop load, paired trials, independent review; test lead |
| Inference cost exceeds value | Large host/GPU cost for a small service gain | Report total resources, compare tiny controller, define acceptable tradeoff; project lead |
| Upstream source/model drift | Reproductions change silently | Artifact manifests and hashes; no moving refs in trials; infrastructure |
| Observability changes behavior | Serial logs dominate timing | Buffered counters, bounded off-path export, overhead ablation; test lead |

Risks are not automatic reasons to stop. Each has a concrete test or a decision gate. Failure of an invariant, unlike poor model quality, blocks live application immediately.

## Open questions and working defaults

| Question | Working default | When an answer is needed |
| --- | --- | --- |
| Which Laya repository is intended? | `NandhaKishorM/laya`, upstream of discovered fork | Before implementation pins are finalized |
| Is an external controller acceptable for the concept? | Yes for S3/S4; self-contained deployment is S5 | Architecture kickoff |
| What does “most policy” mean to stakeholders? | CPU, managed-memory/cache targets, and admission in this prototype | Before publishing project claims |
| What host hardware is available? | TCG-capable Linux x86-64 first; KVM host for timing | G0/environment setup |
| Is a GPU available or affordable? | Start with CPU measurement; no purchase assumed | After inference timing spike |
| Which real service should follow synthetic workloads? | Select one after deterministic mechanisms work | WP4 |
| Is self-contained inference a mandatory first deliverable? | No; optional separate port | Kickoff, because it changes critical path substantially |
| Which metric determines policy value? | Mixed-load p99 latency with throughput/completion constraints | Before final data split and qualification |

If a default changes, update the objective, roadmap, and acceptance claims together. An altered platform or requirement is a scope change, not something to hide in implementation details.

## Statements to avoid in presentations

Do not say that a typed response cannot be wrong, that Rust eliminates all kernel faults, that QEMU/TCG proves production latency, or that the model replaced every operating-system decision. Do not claim process isolation inside the ordinary unikernel, instantaneous cache reclamation, or a working in-guest PyTorch port.

Use the measured result: which policy domains were delegated, which mechanisms enforced them, what the model cost, how failures behaved, and which workloads improved or regressed.

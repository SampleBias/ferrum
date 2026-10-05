# Project brief

## The request in engineering terms

We want to investigate whether an AI model can manage a substantial portion of a small operating system's **resource policy**, while a Rust kernel implements the mechanisms that make those decisions safe and executable. Use the RustyHermit/Hermit lineage as the foundation, use Laya for typed decisions, and make every stage reproducible in QEMU.

An accurate short description is:

> An AI-managed Rust unikernel: Laya selects bounded resource policies from current system telemetry, and a modified Hermit kernel enforces those policies with deterministic scheduling, resource limits, and autonomous fallback.

The AI decides which allowed resource allocation best serves an operator-defined objective. Rust decides whether the proposal is valid, how to enact it, and how execution continues if there is no valid proposal. The operator defines the objective and non-negotiable limits. Model inference does not become an interrupt handler, memory allocator, or source of executable kernel code.

## Why a paired application is the first design

A Hermit unikernel links an application with an operating-system library into a bootable image. That supplies the hardware-facing Rust foundation. Laya's existing software ecosystem is much easier to operate in a conventional host environment, so the initial system has two deployable artifacts: a guest image and a controller application. See the [upstream evidence](01-upstream-and-feasibility.md).

The guest contains the policy enforcement code, telemetry bridge, and workloads. The Linux controller contains Laya, its tokenizer, calibration, and inference dependencies. They exchange bounded messages through virtio networking. QEMU exercises the actual guest kernel and its virtual devices.

This is a complete proof of external AI management of a unikernel. It is not yet a self-contained AI operating system: Linux still hosts the inference runtime. [Document 13](13-in-guest-evolution.md) defines the additional proof needed for that claim.

## Questions the prototype must answer

1. Can the proposed Rust mechanisms maintain progress under any syntactically valid model output?
2. Can Laya interpret bounded kernel telemetry well enough to choose useful policies?
3. Is the end-to-end decision delay short enough for the workload's changing phases?
4. Does Laya offer value over static settings and an inexpensive deterministic adaptive controller?
5. Can the experiment be reproduced from pinned source, model, data, and environment artifacts?

The first, third, and fifth are engineering questions. The second and fourth are research questions. A successful integration must not be presented as evidence that the research questions have positive answers.

## Scope and evidence levels

| Level | Deliverable | Claim allowed |
| --- | --- | --- |
| S0 | Original Hermit-derived image boots under QEMU | The foundation works |
| S1 | Typed mock decisions traverse the bridge and change application behavior | Transport and integration work |
| S2 | Deterministic profiles change actual kernel scheduling | The kernel exposes enforceable policy mechanisms |
| S3 | Live Laya chooses those profiles; kernel failure tests pass | AI manages one real kernel policy domain |
| S4 | CPU, managed-memory, and admission policies are jointly selected and evaluated | AI manages the prototype's three declared policy domains |
| S5, optional | Inference executes inside the guest without a host service | A self-contained AI-managed unikernel works |

S3 is the minimum kernel proof. S4 is the target for the broader concept. S5 is a separate porting project with a stop/go gate.

## Initial workload and platform

Use x86-64, one vCPU, a 512 MiB guest, serial output, and virtio-net. Start with three workload classes: short latency-sensitive jobs, sustained batch computation, and cache/reclamation work. Give each class real Hermit threads; keep the initial workload to at most eight runnable workload threads plus bounded system/control threads. Static class membership is established by trusted guest startup code.

Use a reproducible mixture of bursts, steady load, idle periods, and memory pressure. Each phase lasts tens of seconds so a one-second supervisory decision can matter. Include adverse and rapidly changing phases to reveal where the approach fails. Do not evaluate only workloads engineered to match the model's vocabulary.

Out of scope initially: arbitrary Linux binaries, multiple mutually distrustful applications in one address space, a new device-driver stack, filesystem correctness policy, swapping arbitrary live pages, bare-metal GPU support, SMP policy updates, hard real-time guarantees, and online changes to model weights.

## Success criteria

Engineering success requires a bootable guest, traceable model-to-kernel effects, continued workload progress during controller loss, rejection of invalid/stale outputs, and reproducibility from the recorded artifact set. No benchmark improvement is necessary to establish that limited functional claim.

Research success additionally requires a preregistered held-out benchmark showing a useful quality/cost tradeoff against the strongest selected non-ML baseline. The initial target is a 10% improvement in the primary workload metric with a positive paired confidence interval, while satisfying throughput, fairness, memory, and overhead constraints. These are proposed project gates, not measured performance.

If Laya fails the research gate, retain the measurements and say so. The system can still demonstrate the architecture; the team should then decide whether domain training, distillation, or a conventional controller is justified.

## Ownership

The kernel lead owns enforceability and progress. The ML lead owns model behavior and calibration. The infrastructure lead owns builds and QEMU reproducibility. An independent reviewer owns the acceptance evidence and checks that application effects are not mislabeled as kernel effects. One person may hold multiple roles; these are responsibility boundaries, not a staffing mandate.

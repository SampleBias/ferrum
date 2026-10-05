# Ferrum

> Build a Hermit-based Rust unikernel whose resource-management policies are selected by Laya, a System 1 decision model. Rust retains hardware control, correctness rules, bounded execution, and fallback policies. Demonstrate the system in QEMU with measurable kernel effects and continued operation when the model is unavailable.

This repository currently contains a development plan, not a working kernel implementation. The plan was researched on **2026-10-05**. Commands, interfaces, budgets, and acceptance thresholds described as proposed must be implemented or validated before they are treated as working infrastructure.

The recommended first architecture is **one Hermit guest plus a standalone Linux application running Laya**. The guest combines the kernel, a small policy bridge, and benchmark workloads in one unikernel image. The external application makes decisions asynchronously. An enforced, expiring policy inside the guest governs ordinary execution without waiting for inference.

“Type one” is interpreted as **System 1 inference**, not a Type 1 hypervisor. “Laya” is assumed to mean [NandhaKishorM/laya](https://github.com/NandhaKishorM/laya), the upstream of the initially discovered Dancing-coin/l-aya fork. RustyHermit's current project lineage is [Hermit](https://github.com/hermit-os). These assumptions can be changed without discarding the policy/mechanism design.

## Reading order

| Document | Purpose |
| --- | --- |
| [00 — Project brief](docs/00-project-brief.md) | Communicate the concept, scope, and what constitutes proof |
| [01 — Upstream and feasibility](docs/01-upstream-and-feasibility.md) | Verified sources, candidate revisions, compatibility risks |
| [02 — System architecture](docs/02-system-architecture.md) | Placement, trust boundaries, boot, timing, failure recovery |
| [03 — Kernel mechanisms and policies](docs/03-kernel-mechanisms-and-policies.md) | Real scheduler changes, resource accounting, policy catalog |
| [04 — Policy protocol](docs/04-policy-protocol.md) | Messages, freshness, validation, application, acknowledgments |
| [05 — Laya controller](docs/05-laya-controller.md) | Model adapter, typed questions, confidence, resource budgets |
| [06 — Repository and scaffold](docs/06-repository-and-scaffold.md) | Hermit derivation, proposed crates, build contract, patch order |
| [07 — QEMU lab](docs/07-qemu-lab.md) | Host setup, TCG/KVM, networking, debugging, reproducible runs |
| [08 — Data and learning](docs/08-data-and-learning.md) | Telemetry, labels, calibration, experiments, model promotion |
| [09 — Validation and benchmarks](docs/09-validation-and-benchmarks.md) | Functional proof, fault injection, baselines, performance gates |
| [10 — Delivery roadmap](docs/10-delivery-roadmap.md) | Dependencies, estimates, owners, acceptance criteria |
| [11 — Decisions, risks, questions](docs/11-decisions-risks-and-questions.md) | Architecture decisions and unresolved implementation risks |
| [12 — Demo and review](docs/12-demo-and-review.md) | Demonstration script and team sign-off checklist |
| [13 — In-guest evolution](docs/13-in-guest-evolution.md) | Path toward a self-contained image and its additional costs |

For an architecture review, read 00, 02, 03, and 11. Kernel engineers should then read 04, 06, and 07; ML engineers should read 05, 08, and 09. The delivery lead can turn the work packages in 10 into tickets.

## Recommended first commitment

Fund the scaffold and deterministic scheduler experiment first, then the live Laya integration. The minimum meaningful demo is a model-selected policy that changes **Hermit's selection of runnable threads**, with a trace showing the choice, acceptance, activation, and resulting CPU service. Application-level job routing alone is an earlier integration milestone.

The expanded proof of concept delegates three defined policy domains: CPU allocation, managed-memory/cache targets, and workload admission. “Most policy” means these named discretionary choices within the prototype. It does not claim that an AI has replaced most responsibilities of a general-purpose operating system.

Host policy crates now implement the catalog, lease state machine, scheduler reference model, and authenticated frames. `cargo test --workspace` exercises those on Linux. QEMU 11.1.1 is installed and `tools/run-qemu.sh` selects TCG or KVM explicitly. This machine is an AMD Ryzen 5 1600 with AMD-V; `kvm_amd` is loaded and `/dev/kvm` is accessible.

The untouched template `da0826ec` boots under both accelerators with Rust 1.94.0, `rust-std-hermit` 1.94.0, and loader release v0.5.6. Serial contains `Hello, world!` and QEMU’s raw `isa-debug-exit` status is 3. Loader v0.5.7 panics in that kernel while mapping the SMP trampoline at `0x8000`. The boot record is `bootstrap/lane-a/g0-manifest.txt`.

`apps/policy-guest` speaks the authenticated protocol to `python3 -m aik_controller.server`. Under both TCG and KVM the guest at `10.0.2.15` received the heuristic `latency` profile, printed `FERRUM_APPLIED profile=latency generation=2`, installed that profile's `cpu-v1` weights, and measured one second of three non-yielding threads. KVM service was `599063 / 200570 / 200376` microseconds, TCG was `598884 / 200691 / 200515`, then `FERRUM_SCHED_OK` and raw status 3. No model weights have been downloaded, and no performance result is claimed.

The same guest, built against `vendor/hermit-rs` with `ai-policy`, runs `--fair-demo`: three non-yielding threads, one per workload class, through all four `cpu-v1` weight tuples. KVM one-second windows, in latency/batch/maintenance microseconds, were balanced `332920 / 334189 / 332904`, latency `600614 / 199655 / 199737`, throughput `201605 / 596842 / 201561`, and reclaim `200138 / 201527 / 598340`, then `FERRUM_SCHED_OK` and raw status 3. TCG was balanced `333288 / 332713 / 334177`, latency `601343 / 197803 / 200105`, throughput `201643 / 598509 / 199968`, and reclaim `200154 / 201151 / 598966`. Those counters are guest-timer microseconds. Virtual runtime is kept across the weight changes. `--fair-progress` registers eight threads (three latency, three batch, two maintenance) under latency weights `6:2:2`. In the following 500 ms every thread's service increased. KVM deltas by class were `1:100877,1:100855,1:98780,2:32255,2:32277,2:34281,3:50246,3:50435`, then `FERRUM_PROGRESS_OK` and raw status 3. TCG was `1:101261,1:101198,2:35161,2:32054,2:32237,3:49059,3:50190,1:99094`.

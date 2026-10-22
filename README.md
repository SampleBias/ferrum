# Ferrum

Ferrum is a [Hermit](https://github.com/hermit-os)-based Rust unikernel whose resource-management policies are selected by an external controller (heuristic, threshold classifier, or [Laya](https://github.com/NandhaKishorM/laya) System 1 inference) while the Rust kernel retains hardware control, safety invariants, lease expiration, and autonomous fallback.

## How It Works

The system runs as two paired artifacts over QEMU `virtio-net`:

1. **Hermit Unikernel Guest (`apps/policy-guest` + `vendor/hermit-rs`):**
   - Implements an opt-in hierarchical fair-share scheduler (`ai-policy` feature) across three workload classes (`latency`, `batch`, `maintenance`) plus a reserved `system` budget.
   - Exports bounded telemetry snapshots and accepts authenticated, lease-bounded policy proposals (`balanced`, `latency`, `throughput`, `reclaim`).
   - Enforces minimum dwell times, generation checks, acceptance deadlines, emergency overrides, and automatic fallback to `balanced` if a lease expires or the controller disappears.
2. **Host Policy Controller (`controller/`):**
   - Speaks the authenticated frame protocol (`HMAC-SHA256`), encodes telemetry features, and replies with a catalog profile or explicit abstention.
   - Supports deterministic heuristics, frozen threshold/logistic candidates, shadow scoring, and live Laya inference.

## Repository Layout

| Path | Description |
| --- | --- |
| `vendor/hermit-rs/` | Patched Hermit 0.13.0 kernel and `hermit` crate with the `ai-policy` scheduler and syscalls |
| `crates/policy-types/` | `no_std` policy identifiers, catalog types, and telemetry structures |
| `crates/policy-core/` | Policy lease engine, scheduler reference model, resource ledgers, and cache entry table |
| `crates/policy-wire/` | Authenticated wire framing and strict JSON codec |
| `crates/hermit-policy-abi/` | Narrow C ABI definitions between the guest bridge and kernel |
| `crates/workloads/` | Synthetic phase definitions (`mixed-v1`) and seeded open-loop job schedules (`jobs-v1`, `jobs-v2`) |
| `apps/policy-guest/` | Unikernel guest binary and session driver for functional, fault, branch, and soak modes |
| `controller/` | Python controller server, Laya adapter, branched label tooling, and evaluation suite |
| `configs/` | Frozen catalogs, feature bins, split manifests, and calibrated host candidates |
| `tools/` | Helper scripts for environment checks, building the guest, and running QEMU |

## Quick Start

### 1. Run Host & Controller Tests

```bash
cargo test --workspace
PYTHONPATH=controller/src python3 -m unittest discover -s controller/tests
```

### 2. Build the Unikernel Guest

Requires Rust `1.94.0` (with `rust-std` for `x86_64-unknown-hermit`) and `nightly-2026-02-01` for the kernel build:

```bash
./tools/build-policy-guest.sh --release
```

### 3. Boot in QEMU

Run a standalone guest check (for example, deterministic scheduler profiles or G5 arena/admission convergence):

```bash
AIK_ACCEL=kvm \
AIK_LOADER=artifacts/loader/hermit-loader-x86_64-v0.5.6 \
AIK_GUEST=target/x86_64-unknown-hermit/release/policy-guest \
AIK_APPEND="-- --fair-demo" \
./tools/run-qemu.sh
```

Run a connected session with the host controller:

```bash
# Terminal 1: start the controller
PYTHONPATH=controller/src python3 -m aik_controller.server --once --bind 127.0.0.1:7777

# Terminal 2: boot the guest with virtio-net enabled
AIK_ACCEL=kvm AIK_NET=1 \
AIK_LOADER=artifacts/loader/hermit-loader-x86_64-v0.5.6 \
AIK_GUEST=target/x86_64-unknown-hermit/release/policy-guest \
./tools/run-qemu.sh
```

Note: Hermit exits QEMU via `isa-debug-exit`, where raw exit status `3` indicates success (`0` in the guest).

## Status Summary

- **Engineering (S3 CPU + G5 deterministic ledgers):** Complete and verified under TCG and KVM (`kvm_intel` and `kvm_amd`). Stage-to-activation and lease-expiry delays meet the 10 ms target while the vCPU runs, invalid/late/stale proposals are rejected, controller loss falls back cleanly to `balanced`, and the 1-hour renewal soak completes without missed renewals (26 passes, 2,314 rounds).
- **Research (`jobs-v2` sealed evaluation):** A single pre-decision feature (pending jobs) separates under-capacity from overload states. The frozen pending-jobs threshold hits 100% of final-test and out-of-distribution decisions on Ryzen (`14/15` and `6/6` units in closed loop) and `168/180` final-test plus `72/72` out-of-distribution decisions on Intel (`15/15` and `5/6` units in closed loop), outperforming both `heuristic-v0` and zero-shot/adapted Laya.

Full acceptance measurements and gate verdicts are recorded in [docs/14-s3-acceptance.md](docs/14-s3-acceptance.md).

## Documentation

| Document | Purpose |
| --- | --- |
| [00 — Project brief](docs/00-project-brief.md) | Concept, scope, and success criteria |
| [01 — Upstream and feasibility](docs/01-upstream-and-feasibility.md) | Pinned upstream sources and compatibility notes |
| [02 — System architecture](docs/02-system-architecture.md) | Trust boundaries, boot flow, timing, and recovery |
| [03 — Kernel mechanisms and policies](docs/03-kernel-mechanisms-and-policies.md) | Fair-share scheduler, resource accounting, and policy catalog |
| [04 — Policy protocol](docs/04-policy-protocol.md) | Wire messages, freshness, validation, and acknowledgments |
| [05 — Laya controller](docs/05-laya-controller.md) | Model adapter, typed questions, and timing budgets |
| [06 — Repository and scaffold](docs/06-repository-and-scaffold.md) | Crate responsibilities and build contract |
| [07 — QEMU lab](docs/07-qemu-lab.md) | TCG/KVM setup, networking, and reproducible runs |
| [08 — Data and learning](docs/08-data-and-learning.md) | Branched labels, splits, candidate fitting, and sealed evaluation |
| [09 — Validation and benchmarks](docs/09-validation-and-benchmarks.md) | Functional proof, fault matrix, and quantitative targets |
| [10 — Delivery roadmap](docs/10-delivery-roadmap.md) | Work packages and stop/go gates |
| [11 — Decisions, risks, questions](docs/11-decisions-risks-and-questions.md) | Architecture decision records and risk register |
| [12 — Demo and review](docs/12-demo-and-review.md) | Demonstration sequence and review checklists |
| [13 — In-guest evolution](docs/13-in-guest-evolution.md) | Requirements for self-contained in-guest inference |
| [14 — S3 acceptance record](docs/14-s3-acceptance.md) | Recorded S3 gates, sealed evaluations, pause behavior, and soak |

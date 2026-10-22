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

### 4. Run the Full Verification Suite, Real-Data Benchmark & Explainability CLI

To execute all four test tiers (Rust workspace tests, Python controller tests, standalone QEMU mechanism boots, connected protocol/fault boots, real-payload RPC/HMAC benchmark, closed-loop branch, and live soak) and refresh [docs/15-test-results.md](docs/15-test-results.md) and its figures in `docs/figures/`:

```bash
./tools/run-test-report.sh
```

To inspect per-decision feature attributions, boundary margins, and counterfactual outcomes on the sealed KVM dataset:

```bash
PYTHONPATH=controller/src python3 -m aik_controller.explain --host r5-1600
PYTHONPATH=controller/src python3 -m aik_controller.explain --host i7-10750h
```

Note: Hermit exits QEMU via `isa-debug-exit`, where raw exit status `3` indicates success (`0` in the guest).

## Performance, Benchmarks & Explainability

### 1. Kernel Enforcement Timings & Soak Qualification

![Kernel Enforcement Timings and Soak Qualification](docs/figures/timing-and-soak.png)

- **Engineering (S3 CPU + G5 deterministic ledgers):** Verified under TCG and KVM (`kvm_intel` and `kvm_amd`). Stage-to-activation (`3 µs`) and lease-expiry (`5 µs`) meet the `10,000 µs` (`10 ms`) target with `>1000x` margin while the vCPU runs. Invalid/late/stale proposals are rejected, controller loss falls back cleanly to `balanced`, and the 1-hour renewal soak completes without missed renewals (`26` passes, `2,314` rounds).

### 2. Real-Data Unikernel Benchmarks (`--real-workload` & Sealed KVM Dataset)

![Real-Data Unikernel Benchmarks](docs/figures/real-data-benchmark.png)

- **Live Real-Payload RPC & HMAC Benchmark (`--real-workload`):** Runs `168` open-loop RPC requests (`HMAC-SHA256` signing + verification + strict JSON schema decoding) alongside continuous `8 KiB` `HMAC-SHA256` batch workers inside the Hermit unikernel. Adaptive policy actuation (`balanced 1:1:1` $\to$ `latency 6:2:2`) cuts RPC **p99 latency from `45.5 ms` to `20.7 ms` (`2.20x` faster, `-54.5%`)** while retaining **`100.5%` batch throughput** (`1,034 MiB` vs `1,029 MiB` verified per `2 s` window).
- **Sealed KVM Evaluation (`jobs-v2`, `252` decisions across `21` units):** The frozen `pending-threshold` candidate hits **`100%` of final-test (`180/180`) and out-of-distribution (`72/72`) decisions** on Ryzen (`168/180` and `72/72` on Intel), achieving **`71.9 ms` mean under-capacity p99** (`2.35x` faster than `balanced`/Laya at `169.0 ms`) and **`100.0%` overload batch CPU retention** (whereas `heuristic-v0` drops batch CPU to `51.1%`, violating the `95%` floor).

### 3. Policy Explainability & Counterfactual Attribution

![Policy Explainability and Feature Attribution](docs/figures/explainability-attribution.png)

- Every controller decision emits an explicit attribution record (`FERRUM_EXPLAIN` in the guest and `EXPLAIN seq=...` in `aik_controller.server`) reporting the triggered rule, signed queue margin (`pending - cut`), standardized feature log-odds ($\beta z$), and measured counterfactual regret.

Full acceptance measurements, visual charts, and captured kernel traces are recorded in [docs/14-s3-acceptance.md](docs/14-s3-acceptance.md) and [docs/15-test-results.md](docs/15-test-results.md).

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
| [15 — Test guide & verification results](docs/15-test-results.md) | Automated test runner guide, visual charts, scorecard, and captured kernel traces |

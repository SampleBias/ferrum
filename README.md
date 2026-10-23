# Ferrum

Ferrum is a [Hermit](https://github.com/hermit-os)-based Rust unikernel whose resource-management policies are selected by an external controller (heuristic, threshold classifier, or [Laya](https://github.com/NandhaKishorM/laya) System 1 inference) while the Rust kernel retains hardware control, safety invariants, lease expiration, and autonomous fallback.

## How It Works

The system runs as two paired artifacts over QEMU `virtio-net`:

1. **Hermit Unikernel Guest (`apps/policy-guest` + `vendor/hermit-rs`):**
   - Implements an opt-in hierarchical fair-share scheduler (`ai-policy` feature) across three workload classes (`latency`, `batch`, `maintenance`) plus a reserved `system` budget, with tickless idle and single-runnable quantum coalescing for constrained devices.
   - Exposes a narrow C ABI (`crates/hermit-policy-abi`: `AbiStageJointIn`, `AbiJointStatus`) that atomically actuates CPU weights, managed cache soft targets, and per-class admission limits (`joint-v1`).
   - Enforces minimum dwell times, generation checks, acceptance deadlines, emergency hysteresis overrides, and automatic fallback to `balanced` if a lease expires or the controller disappears.
2. **Host Policy Controllers (`controller/` & `apps/policy-controller/`):**
   - Speak the authenticated frame protocol (`HMAC-SHA256`), encode telemetry features (`features-v0`, `features-v1`), and reply with a catalog profile or explicit abstention.
   - Support deterministic heuristics, frozen threshold/logistic candidates, the work-aware `distilled-joint` (`DISTILLED_JOINT_V1`) multi-regime policy, shadow scoring, and live Laya inference.
3. **Isolated Embedded Deployments (`deployments/`):**
   - Domain-specific freestanding applications (such as `deployments/air-biosensor`) link against `hermit-policy-abi` and `policy-core` without coupling domain logic into the base kernel or core crates.

## Repository Layout

| Path | Description |
| --- | --- |
| `vendor/hermit-rs/` | Patched Hermit 0.13.0 kernel and `hermit` crate with the `ai-policy` scheduler, tickless coalescing, and joint ABI syscalls |
| `crates/policy-types/` | `no_std` policy identifiers, catalog types, and telemetry structures |
| `crates/policy-core/` | Policy lease engine, scheduler reference model, resource ledgers, cache entry table, and `no_std` distilled/threshold policies |
| `crates/policy-wire/` | Authenticated wire framing and strict JSON codec |
| `crates/hermit-policy-abi/` | Narrow C ABI definitions (`AbiStageJointIn`, `AbiJointStatus`, `AbiSnapshot`) between freestanding apps and the kernel |
| `crates/workloads/` | Generic OS phase definitions (`mixed-v1`) and seeded open-loop job schedules (`jobs-v1`, `jobs-v2`) |
| `apps/policy-guest/` | Unikernel guest binary and session driver for functional, fault, joint-ABI, branch, and soak modes |
| `apps/policy-controller/` | Standalone Rust host controller supporting `pending-threshold`, `distilled-joint`, and `heuristic-v0` across `cpu-v1` and `joint-v1` |
| `deployments/air-biosensor/` | Isolated embedded unikernel deployment for an Air Biological Sampling Sensor on top of `hermit-policy-abi` |
| `controller/` | Python controller server, Laya adapter (`adapt-v0`, `adapt-v1`), branched label tooling, and explainability/evaluation suite |
| `configs/` | Frozen catalogs, feature schemas (`features-v0`, `features-v1`), split manifests, and calibrated host candidates |
| `tools/` | Helper scripts for environment checks, building the base guest or embedded deployments, and running QEMU |

## Quick Start

### 1. Run Host & Controller Tests

```bash
cargo test --workspace
PYTHONPATH=controller/src python3 -m unittest discover -s controller/tests
```

### 2. Build the Unikernel Guest (or Embedded Deployments)

Requires Rust `1.94.0` (with `rust-std` for `x86_64-unknown-hermit`) and `nightly-2026-02-01` for the kernel build:

```bash
# Base generic policy-guest unikernel
./tools/build-policy-guest.sh --release

# Standalone Air Biological Sampling Sensor embedded deployment
./tools/build-air-biosensor.sh --release
```

### 3. Boot in QEMU

Run a standalone guest check (for example, deterministic scheduler profiles, S4 joint ABI & tickless coalescing, or an embedded deployment):

```bash
# Base scheduler fair-share check
AIK_ACCEL=kvm \
AIK_LOADER=artifacts/loader/hermit-loader-x86_64-v0.5.6 \
AIK_GUEST=target/x86_64-unknown-hermit/release/policy-guest \
AIK_APPEND="-- --fair-demo" \
./tools/run-qemu.sh

# Base S4 JointV1 ABI & tickless idle coalescing check
AIK_ACCEL=kvm \
AIK_LOADER=artifacts/loader/hermit-loader-x86_64-v0.5.6 \
AIK_GUEST=target/x86_64-unknown-hermit/release/policy-guest \
AIK_APPEND="-- --joint-abi" \
./tools/run-qemu.sh

# Isolated Air Biological Sampling Sensor embedded deployment
AIK_ACCEL=kvm \
AIK_LOADER=artifacts/loader/hermit-loader-x86_64-v0.5.6 \
AIK_GUEST=target/x86_64-unknown-hermit/release/air-biosensor \
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

- **Engineering (S3 CPU + S4 Joint ABI + Tickless Coalescing):** Verified under TCG and KVM (`kvm_intel` and `kvm_amd`). Stage-to-activation (`3 µs`) and lease-expiry (`4–5 µs`) meet the `10,000 µs` (`10 ms`) target with `>1000x` margin while the vCPU runs. Tickless idle coalescing (`--joint-abi`) achieves `98.8%` hardware sleep residency during low-duty patrol (`10` timer wakeups vs `110` coalesced wakeups). Invalid/late/stale proposals are rejected, controller loss falls back cleanly to `balanced`, and the 1-hour renewal soak completes without missed renewals (`26` passes, `2,314` rounds).

### 2. Real-Data Unikernel Benchmarks (`--real-workload` & Sealed KVM Dataset)

![Real-Data Unikernel Benchmarks](docs/figures/real-data-benchmark.png)

- **Live Real-Payload RPC & HMAC Benchmark (`--real-workload`):** Runs `168` open-loop RPC requests (`HMAC-SHA256` signing + verification + strict JSON schema decoding) alongside continuous `8 KiB` `HMAC-SHA256` batch workers inside the Hermit unikernel. Adaptive policy actuation (`balanced 1:1:1` $\to$ `latency 6:2:2`) cuts RPC **p99 latency from `46.9 ms` to `18.2 ms` (`2.58x` faster, `-61.3%`)** while retaining **`101.4%` batch throughput** (`1,036 MiB` verified per `2 s` window).
- **Sealed KVM Evaluation (`jobs-v2`, `252` decisions across `21` units per host):**
  - **`distilled-joint` (Work-Aware Boundary):** Combines `queue_len` (`14.5`) with `max_wait_us < 350 ms` transient spike absorption, hitting **`21 / 21 (100%)` of sealed units (`180/180` final-test and `72/72` OOD decisions) with `0` infeasible decisions on both Ryzen 5 1600 and Intel Core i7-10750H**.
  - **`pending-threshold` (`cut=14.5` / `14.0`):** Hits `21 / 21 (100%)` on Ryzen and `20 / 21 (95.2%)` on Intel (`168/180` final-test, `72/72` OOD), achieving **`71.9 ms` mean under-capacity p99** (`2.35x` faster than `balanced`/Laya at `169.0 ms`) and **`100.0%` overload batch CPU retention** (whereas `heuristic-v0` drops batch CPU to `51.1%`, violating the `95%` floor).

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

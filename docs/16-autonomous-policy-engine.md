# Autonomous Policy Engine v1

## Architecture audit

The reusable policy implementation is already in Rust:

| Component | Existing responsibility | Offline milestone use |
| --- | --- | --- |
| `crates/policy-types` | Fixed-size telemetry and profile types; `no_std` | Observation and named-profile types |
| `crates/policy-core/src/heuristic.rs` | `heuristic-v1`, integer-only signals, overload regime, and session state; `no_std` outside tests | Same `HeuristicSession` used by the host controller |
| `crates/policy-core/build.rs` | Compiles the catalog and heuristic JSON into constants | Keeps the guest thresholds and catalog in step with the checked-in configs |
| `apps/policy-controller` and `controller/` | Optional host controller and experimental Laya/Python integration | Remain available for external runs |
| `crates/hermit-policy-abi` | Stable joint-profile staging and status layouts | Existing `sys_policy_stage_joint` and `sys_policy_read_joint` boundary |
| `deployments/air-biosensor` | Sensor phase definitions and simulated application | `--autonomous` runs the local phase loop |

The heuristic configuration is `configs/heuristic-v1.json`. Its base rules choose `reclaim` for emergency or memory pressure (at least 8,000 bp managed-memory use, or 32 MiB evictable backlog), `latency` for a latency queue of at least 4 or a sampled wait of at least 5 ms, `throughput` for a batch queue of at least 4 with no latency queue, and `balanced` otherwise. The v1 overload regime adds the contended and saturated thresholds (9,500 bp), a 9,500 bp batch floor, a 303,756 µs stall threshold, and an 8,500 bp release threshold. Its state persists between decisions in `HeuristicSession`.

The same fixed-point implementation is used by `apps/policy-controller`; the guest does not load a model or parse JSON at runtime. `tests/policy-golden/heuristic-v1.json` is already consumed by the policy-core test and checks the Rust decision and signal values against the shared golden cases.

## Offline runtime

`apps/policy-guest --autonomous` runs the existing `mixed-v1` trace with the local `HeuristicSession` and joint-profile ABI, without a controller connection. The air-biosensor `--autonomous` mode below is the sensor-facing demonstrator with capture, confirmation, recovery, and pressure phases.

The new `air-biosensor --autonomous` path has no controller argument and does not open a socket. It runs patrol, capture, confirmation, recovery, and a pressure-reclaim phase. It periodically reads the Hermit CPU-service counters, combines their deltas with the sensor simulator's queue, wait, and memory-pressure values, evaluates the shared v1 session, and submits the selected profile using the existing joint ABI. The kernel status and acknowledgment determine whether the change was accepted. Rejected or unconfirmed changes are logged and the current kernel policy remains authoritative.

Ordinary changes require the same candidate in two consecutive one-second samples. `reclaim` is submitted immediately when v1 selects it. The guest also observes the two-second dwell interval before attempting a different profile. Hermit independently checks generation, its 750 ms acceptance window, the dwell interval, and the three-second lease; lease expiry installs `balanced`. If telemetry is unavailable or stale, the guest skips that proposal. It does not extend a profile lease using missing samples, so repeated telemetry failure eventually returns the kernel to `balanced`.

The current kernel ABI does **not** expose one complete `AbiSnapshot` read operation. It exposes cumulative per-class CPU-service counters and joint-policy status. Queue length, wait age, and memory pressure in this demonstrator are therefore simulated from the declared sensor phase. CPU service and active policy status are measured in the guest. This keeps the change above the kernel while making the telemetry boundary explicit. It is not yet a general-purpose live queue or allocator telemetry pipeline.

The five-phase run is finite by default. `--cycles=0` repeats it continuously. A full one-hour soak, failure-injection campaign, and local-versus-controller performance comparison require QEMU and an appropriate host lane; no results are claimed here until those runs are captured.

## Build and run without networking

Host-side decision trace (this prints choices but cannot call the Hermit kernel):

```bash
cargo run -p air-biosensor -- --autonomous
```

Build the freestanding image with the repository's pinned guest toolchain, then boot it in QEMU with networking explicitly disabled. `AIK_NET=0` keeps the NIC off even if the shell has a different default.

```bash
./tools/build-air-biosensor.sh --release

AIK_ACCEL=tcg \
AIK_NET=0 \
AIK_TIMEOUT_S=60 \
AIK_LOADER=artifacts/loader/hermit-loader-x86_64-v0.5.6 \
AIK_GUEST=target/x86_64-unknown-hermit/release/air-biosensor \
AIK_APPEND="-- --autonomous --cycles=1" \
./tools/run-qemu.sh
```

Use `--cycles=0` for continuous control and set `AIK_TIMEOUT_S` to the desired external run limit. Use `AIK_ACCEL=kvm` only when `/dev/kvm` is accessible; the script refuses to silently replace KVM with TCG.

## Optional external controller

The existing `air-biosensor` mode remains available. A `--controller=ADDRESS` argument selects the authenticated host session; omit that argument to use the in-guest policy. For the controller run, start the existing server and enable QEMU user networking as documented in the main README. The autonomous mode rejects `--controller=` so an offline run cannot quietly turn into a connected experiment. Laya and the Python integration remain experimental controller-side components.

## Verification and benchmark plan

Run the focused regression checks and host compilation with:

```bash
cargo test -p policy-core -p air-biosensor
cargo check -p policy-guest -p air-biosensor
```

The existing golden-vector test verifies heuristic-v1 decision parity. The biosensor tests verify the local phase sequence. The pinned Hermit target build now succeeds for both `policy-guest` and `air-biosensor`; a QEMU boot is still required to confirm the actual freestanding ABI call path.

For comparable results, use the same QEMU version, guest CPU, vCPU count, memory, workload seed, phase durations, and host power state for three modes: fixed `balanced`, the external controller, and `--autonomous`. Record policy evaluation and activation time, RPC p50/p95/p99, CPU service, idle residency, timer wakes, throughput, image/guest memory, and switch count. The autonomous demo currently logs capture-policy activation delay; it does not yet measure full event processing latency. Do not attribute an end-to-end latency change to controller removal without measuring the workload and scheduler effects. Keep host-controller records distinct from local mode records.

## Current host and known limits

The development host used for this change reports an Intel Core i7-6700. This is a different processor from the existing i7-10750H result set. The pinned `nightly-2026-02-01` toolchain with `rust-src` is installed, and both release guests compile for Hermit. The current environment has no `qemu-system-x86_64`, no `/dev/kvm`, and no staged loader at `artifacts/loader/hermit-loader-x86_64-v0.5.6`, so neither a guest boot nor runtime ABI behavior has been verified here. Installing QEMU through the system package manager was blocked because sudo requires an interactive password. The Rust target is Tier 3 and does not ship a precompiled standard library; the build scripts use the pinned nightly with `-Z build-std=std,panic_abort`. The Core i7 model does not change heuristic decisions; it matters for QEMU acceleration and benchmark reproducibility. Do not compare future measurements directly with the i7-10750H data without recording the distinct CPU and QEMU lane.

Still required for the full sprint definition of done: a supported kernel telemetry snapshot or equivalent live queue/wait/memory collection, offline QEMU boot, controller-loss and fault-injection runs, a one-hour soak, and captured comparative benchmark results. The optional external workflow remains intact.

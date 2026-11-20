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

The controller sleeps until the next one-second sample or the phase deadline. Each 2.5-second phase has room for two full samples. The remaining half-second belongs to that phase and produces no proposal. A CPU-service delta requires two adjacent successful counter reads. After a missed read, the first successful read establishes a fresh baseline.

Inactive workers park until a phase change or shutdown. Active workers block on a timer between bursts of work. Hermit's `sys_usleep` busy-waits for delays under 10 ms, which previously charged the one-millisecond worker waits as CPU service and could trigger the overload rule during capture.

Ordinary changes require the same candidate in two consecutive valid samples within the phase. A skipped sample clears that pending candidate. `reclaim` bypasses the two-sample filter. The guest also observes the two-second dwell interval before attempting a different profile. Hermit independently checks generation, its 750 ms acceptance window, the dwell interval, and the three-second lease. Repeated telemetry failure lets the lease expire and returns the kernel to `balanced`.

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

The golden-vector test checks heuristic-v1 decision parity. The biosensor tests cover the expected host phase sequence, sampling deadlines, missing counter reads, counter resets, and stale intervals.

For comparable results, use the same QEMU version, guest CPU, vCPU count, memory, workload seed, phase durations, and host power state for three modes: fixed `balanced`, the external controller, and `--autonomous`. Record policy evaluation and activation time, RPC p50/p95/p99, CPU service, idle residency, timer wakes, throughput, image/guest memory, and switch count. The autonomous demo currently logs capture-policy activation delay; it does not yet measure full event processing latency. Do not attribute an end-to-end latency change to controller removal without measuring the workload and scheduler effects. Keep host-controller records distinct from local mode records.

## Current host and known limits

The autonomous air-biosensor release guest booted on an Intel Core i7-6700 under QEMU 11.1.2 TCG with one vCPU, 512 MiB RAM, networking disabled, and the v0.5.6 Hermit loader. The build used the pinned `nightly-2026-02-01` toolchain with `-Z build-std=std,panic_abort`. KVM was unavailable on this host.

Three pairs of one-cycle boots compared the `b8102bf` guest with the updated sampler and workers. The run order alternated between pairs. Every boot completed all five phases and returned raw QEMU status 3.

| Per boot | `b8102bf` | Updated |
| --- | ---: | ---: |
| Median elapsed time, startup through exit | 15.32 s | 12.86 s |
| Median host CPU time, user + system | 10.49 s | 7.76 s |
| Median guest timer interrupts | 5,947 | 7,512 |
| Boots that applied the expected `latency` profile during capture | 0/3 | 2/3 |

The shorter cycle includes the correction to the five 2.5-second phase deadlines. Blocking short worker waits also trades CPU time for more timer interrupts. Battery energy was unmeasured. On the third updated boot the batch-floor rule kept `balanced` on the first capture sample, leaving only one `latency` sample before the phase ended. The other two boots applied `latency` about two seconds into capture. The simulator's 35 ms capture deadline and physical detection accuracy require separate sensor measurements.

Live queue, wait, and memory measurements, controller-loss and fault-injection runs, a one-hour soak, and a comparison against fixed and external control are still open.

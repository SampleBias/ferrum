# Kernel mechanisms and delegated policies

This document specifies proposed changes. The named policy modules and fair-share mechanism do not already exist in upstream Hermit.

## Policy versus mechanism

| Domain | Laya may choose | Rust always decides/enforces | First milestone |
| --- | --- | --- | --- |
| CPU | Approved relative service profile for workload classes | Eligible tasks, context switches, timer handling, minimum progress, system reservation | S3 |
| Managed memory | Approved cache target and reclamation preference | Ownership, arena caps, allocation validity, safe destruction, emergency thresholds | S4 |
| Admission | Approved bounded in-flight job limits | Counter correctness, hard caps, backpressure, completion accounting | S4 |
| Network/I/O | Later: workload-side batching or quota profile | Device correctness, DMA, descriptor ownership, control-channel progress | Extension |
| Security | No authority in this prototype | Access checks, executable mappings, peer authentication, catalog allowlist | Always deterministic |
| Hardware | No authority | Boot, interrupt state, page tables, drivers, CPU register configuration | Always deterministic |

Delegation is at the timescale where the model can contribute. Rust still performs every context switch and individual allocation locally. A slow model can choose a profile that controls millions of subsequent operations without being consulted for each operation.

## CPU actuator: actual thread scheduling

Use three workload groups: `latency`, `batch`, and `maintenance`. A fourth group, `system`, covers the bridge and essential ordinary kernel support tasks. Interrupt execution remains outside these workload groups and must be measured separately where possible.

The first working baseline leaves the original scheduler untouched. The next patch adds **an opt-in hierarchical fair scheduler for the registered experimental workload threads**. All benchmark modes then use that same mechanism, including static and heuristic baselines. Preserve an upstream-only build as a regression comparison.

The inspected upstream ready queues prioritize runnable tasks; there is no demonstrated fair-share guarantee merely from choosing a bounded numeric priority. See [the scheduler](https://github.com/hermit-os/kernel/blob/7f7dcf70a7739f00c2e998ec7c317d65bba38ee2/src/scheduler/mod.rs). The new mechanism must integrate into the path that decides whether to continue the current task, not only the path used when a task exits or blocks.

### Proposed selection algorithm

1. Account elapsed execution time at every timer-driven preemption, yield, block, and exit. Charge each interval once using the weight in force during that interval, before activating a new profile. Use checked integer/fixed-point arithmetic and define counter rollover handling.
2. Service due system work from a reserved server: initially up to 5 ms of scheduled CPU per 100 ms accounting interval. Select runnable system work before workload work while that interval's reservation remains, and rotate fairly within the system group. Set its next preemption to the smaller of the ordinary quantum and remaining budget. Unused reservation is available to workloads. If system work exceeds its budget, defer nonessential work and record overload; essential interrupt/device handling is measured and can cause the run to fail its assumptions.
3. Among runnable workload groups, choose the group with the smallest normalized virtual runtime. Charge actual elapsed scheduled time as `delta_runtime * scale / weight`, with a nonzero minimum increment and adequate integer precision.
4. Within the selected group, rotate eligible threads with round-robin selection, bounded by the current quantum. A voluntary early yield still incurs its elapsed charge.
5. On waking a sleeping group or adding a thread, clamp its starting virtual runtime to the active minimum so sleeping cannot bank unlimited service credit. Preserve service history across weight changes; a change must not reset debt or grant a free burst.
6. If no workload is runnable, service bounded pending system work or idle normally. Recheck a workload wakeup at the next eligible preemption point.

A code review must specify exact tie-breaking, per-thread queue membership, minimum virtual runtime updates, and borrow/clamp arithmetic before implementation. A host-side model of these operations must test blocking, wakeup, yield-heavy tasks, changes, and overflow. Timer-driven preemption must be demonstrated with a thread that never voluntarily yields.

Keep scheduling metadata in preallocated task/group records. Do not allocate, perform inference, serialize JSON, or do network I/O in the new selection path. Existing upstream operations in that path still require a timing audit; adding a bounded policy lookup does not retroactively make the entire scheduler constant-time. Audit background executor polling inside the scheduling path explicitly and keep new bridge/inference work out of it if it runs with interrupts disabled.

Distinguish scheduled elapsed time from true physical CPU consumption. A TSC/monotonic delta may include host preemption, VM exits, or guest interrupt work. Measure these effects where supported, document any attribution approximation, and use host vCPU statistics to cross-check KVM trials. Do not label TCG elapsed-time accounting as hardware CPU-time measurement.

### Scheduling limits

The initial catalog has three workload groups and at most eight workload threads total. Registration is trusted and frozen after startup. Unknown threads go to a predefined system/default classification; they do not acquire model-controlled privileges from a supplied string.

With every workload group continuously runnable, a `6:2:2` profile requests 60%, 20%, and 20% of the workload CPU service after system work. These are long-window targets, not a promise that each 2 ms slice has those proportions. With sleeping groups, redistribute their unused service among runnable groups. Record measured share over windows of at least one second.

The initial validation target is that every continuously runnable workload thread receives CPU within 500 ms of guest running time under the declared thread limit and nominal system load. Measure it under the least favorable profile. Do not assert a hard bound without accounting for interrupt latency, critical sections, and the full scheduler. A failed progress target blocks model activation.

### Patch locations and ownership

| Location in paired kernel | Proposed work |
| --- | --- |
| `src/scheduler/mod.rs` | Scheduler-owned activation, group selection, accounting hooks, fallback checks |
| `src/scheduler/task/mod.rs` | Stable task generation, class membership, runnable queue transitions |
| `src/scheduler/timer_interrupts.rs` and x86 timer integration | Arrange quantum and policy-expiry wakeups without breaking existing timers |
| `src/syscalls/` | Narrow bridge ABI for registration, snapshots, staging, and status |
| New `src/policy/` | Catalog, validation, lease state, counters, emergency override |

Do not mutate arbitrary task priorities through the currently inspected `set_priority` path to emulate this algorithm. Add a well-defined owner-mediated interface that handles running, ready, blocked, and exited tasks consistently.

## Initial joint policy catalog

Catalog `cpu-v1` implements only the CPU column. Catalog `joint-v1` adds the remaining columns. Never accept a catalog hash with more capability than the guest advertised.

| Profile | CPU weights: latency/batch/maintenance | Cache soft target | In-flight limits: latency/batch | Intent |
| --- | --- | --- | --- | --- |
| `balanced` | `1:1:1` | 64 MiB | `16 / 8` | Stable mixed service and fallback |
| `latency` | `6:2:2` | 64 MiB | `16 / 4` | Protect short jobs during bursts |
| `throughput` | `2:6:2` | 96 MiB | `8 / 16` | Favor sustained batch work |
| `reclaim` | `2:2:6` | 32 MiB | `4 / 2` | Give safe reclamation work more service |

These values are design seeds. Benchmark and freeze them before labeling training data. Laya chooses a complete named profile, so it cannot accidentally combine individually valid knobs into an untested resource configuration. New profiles require a new catalog version/hash and deterministic validation.

Changing a profile installs desired state atomically. CPU weights can take effect at the next scheduling boundary. Memory targets and in-flight limits may need time to converge; telemetry must distinguish desired values from actual values. “Activated” means the desired tuple became authoritative, not that memory immediately reached its target.

## Memory and reclamation actuator

The guest's proposed 512 MiB budget is: up to 128 MiB for measured image/kernel/stacks/control/network needs, 256 MiB of managed workload arenas, and 128 MiB headroom. This is a planning envelope to verify during G0/G1, not an upstream Hermit memory-footprint claim. Reject a configuration whose measured fixed requirements do not fit.

Split managed arenas into latency buffers capped at 64 MiB, batch buffers capped at 64 MiB, and an explicitly evictable cache capped at 128 MiB. Admission limits and maximum per-job allocation must fit these caps; begin with at most 1 MiB charged per admitted job. The model cannot change hard arena caps or take memory reserved for control.

All allocations counted by this experiment go through a tagged pool interface. Shared objects carry an explicit owner/pool charge; frees reverse exactly one recorded charge. Allocations through other libraries are tracked as unmanaged overhead and are not silently represented as protected per-workload memory. Thread identity alone does not establish ownership of shared allocations.

Shrinking a cache target schedules an eviction worker that only removes entries whose ownership/lifetime permits removal. Never free buffers still in use, steal arbitrary mapped pages, or claim a smaller soft target reclaimed bytes. Reclaim at most a configured batch per service opportunity; record backlog, actual bytes, and time to target.

On admission pressure, reject or defer new jobs before allocation. Rust `try_reserve` or an explicitly fallible arena interface is appropriate where supported; do not depend on recovering from an arbitrary global allocator abort. If existing live buffers exceed a reduced limit, let them finish, stop new admissions, and report convergence pending.

For S4, begin with a deterministic emergency trigger when a managed pool reaches 90% of its hard cap or measured unreserved guest headroom falls below 64 MiB. The immediate allocation/admission path still enforces hard caps synchronously. At the next policy boundary, set an emergency override with the `reclaim` desired profile, increment generation, and reject model proposals. Clear it only after every managed pool is below 70% and headroom exceeds 96 MiB continuously for three seconds. If live objects cannot drain, remain degraded and report that fact. Freeze or retune these initial thresholds before qualification; the model cannot edit them. The CPU-only stage has no claim to this memory actuator.

No general paging, swap, or process memory isolation is promised. This subsystem proves AI selection of **managed resource targets** inside the unikernel. A global kernel allocator policy would need a separate design and evaluation.

## Admission actuator

Use bounded queues and in-flight counters around trusted workload submission. Limits apply to new work; reduce capacity by draining existing jobs, without dropping references to active work. Bound queued bytes as well as queued job counts. Reserve a small independent control path that workload admission cannot close.

Admission is an application/runtime mechanism linked into the unikernel, while CPU selection is a kernel mechanism. Report these separately. An S4 result must not imply that every delegated domain is implemented in the same kernel source file.

## Immutable invariants

The model cannot disable preemption, alter trusted class membership, change hard memory caps, extend a lease, change the profile catalog, or turn off emergency handling. Every allowed profile must preserve positive workload service and bounded resource consumption on the supported workload set.

Low confidence means abstention and eventual fallback. High confidence is never permission to break a limit. A model that always chooses the least useful allowed profile should harm performance only within the tested policy envelope; test that case directly.

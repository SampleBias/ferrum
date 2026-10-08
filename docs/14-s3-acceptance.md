# S3 acceptance record

Recorded 2026-10-08 on the office AMD Ryzen 5 1600 (`kvm_amd`), QEMU 11.1.1, Rust 1.94.0, loader `artifacts/loader/hermit-loader-x86_64-v0.5.6`. The tree is `b3cb1ee` plus the measurement guest that prints `FERRUM_STAGE_DELAY` and `FERRUM_EXPIRY_DELAY` and repeats `mixed-v1` under `--soak`. This record does not reopen `jobs-v2`.

## Level

**S3, CPU only.** The architectural claim is that a named profile changes Hermit's scheduler, invalid and late proposals are rejected, and the guest continues when the controller is gone. The research claim is the one already sealed: the pending-jobs threshold is the candidate, and zero-shot Laya does not beat it.

G5 and G6 are not claimed. Managed memory and admission are not actuators in the guest yet. In-guest inference is out of scope.

## Gates

| Gate | Verdict | Evidence |
| --- | --- | --- |
| G0 Foundation | Pass | Untouched template boots under TCG and KVM. Manifest in `bootstrap/lane-a/g0-manifest.txt`. |
| G1 Contract | Pass | Authenticated round trip, codec tests, and a boot with nothing listening (`FERRUM_NO_CONTROLLER`, raw status 3). |
| G2 Mechanism | Pass | All four `cpu-v1` profiles change measured service. Preemption, yield, queue-exit, dwell, and progress boots are in the README. |
| G3 Model readiness | Pass, negative for Laya | Both hosts declare `acceptance-5s`. Zero-shot Laya chose `balanced` on every development state. The office candidate is cut 14.5, frozen before calibration, then 16 of 16 calibration units. |
| G4 Live CPU proof | Pass, with the limitation below | One live Laya boot on each host installed `balanced` inside the 5 s window. The fault matrix below was run with the heuristic controller. Laya has not selected a second profile. |
| G5 Memory and admission | Not claimed | Host ledger and entry table exist. The guest does not drive them. |
| G6 Qualification | Not claimed | The CPU sealed evaluation is done. There is no joint package and no final handoff. |

## Research verdict

On this Ryzen the frozen cut of 14.5 pending jobs hit 180 of 180 final-test decisions on 15 labelled units and 72 of 72 out-of-distribution decisions. Closed loop hit 14 of 15 final-test units and 6 of 6 out of distribution. Zero-shot Laya matched fixed `balanced` and missed every even-split unit. The 10% gain over the strongest non-ML baseline is not met. That result stands.

Live Laya on this host, 2026-10-07, installed `balanced` (the boot profile) 3,956,383 µs after capture. The forward was inside the 5 s window and past the 750 ms heuristic deadline. The trace shows the model can stage a profile. It does not show the model moving CPU service off `balanced`.

## Measurements on this host, 2026-10-08

The heuristic controller proposed `latency`. The guest staged it, measured one second, then waited out the three-second lease. QEMU raw status was 3. `kvm_amd` was the accelerator. KVM was not rewritten to TCG.

### Staging and expiry while the vCPU is running

| | Guest time |
| --- | --- |
| Stage to activation | 23 µs (`stage_us=2029819`, `activate_us=2029842`) |
| Lease end to expiry ack | 26 µs (`lease_until=5029828`, `expired_us=5029854`) |

Both are inside the 10 ms target. The latency window was `599272 / 200484 / 200136` µs. After expiry the balanced window was `333400 / 333377 / 333236`. The controller had already closed. The guest printed `FERRUM_CONTROLLER_LOST closed`, then `FERRUM_SCHED_OK`.

### QEMU stop for 5.05 s

`stop` was sent on the monitor when `FERRUM_ACTIVATED` appeared, held for 5.050 s of host time, then `cont`. Guest monotonic time is `rdtsc` divided by the measured frequency (`get_timer_ticks`). It kept advancing while the vCPU was stopped.

The lease was 3 s from activation (`guest_us=2220835`, `lease_until=5220821`). It ended during the pause. The expiry ack was stamped on the first scheduler entry after `cont`, at `expired_us=7300700`, which is 2,079,879 µs after `lease_until`. That misses the 10 ms target by the amount the pause outlasted the lease. The service window after resume was balanced `333828 / 333755 / 332436`, so the resumed guest was on the fallback, not on the profile whose lease had ended while it was stopped.

Host time from the activated line to the expiry line was 9.167 s, of which 5.050 s was the stop. The running vCPU still met the 10 ms target on the boot above. A stopped vCPU cannot run the expiry path until it is continued.

### Renewal soak

`--soak` repeats the `mixed-v1` phases until the guest limit, renewing the three-second lease. A 60 s run (`--soak-s=60`) finished one pass at raw status 3: `FERRUM_SOAK_OK passes=1 rounds=28`, burst then steady, generation 29.

The one-hour limit (`FERRUM_SOAK limit_us=3600000000`) did not finish. It completed 16 passes and 992 rounds, then failed at guest time about 2,218 s (37 minutes) with raw status 1: `FERRUM_SOAK_FAIL scheduler ack kind lease_expired reason none`. The last applied lease was generation 995, `latency`, `guest_us=2218790026`, `lease_until=2221789982`. The controller had already sent the next proposal. The scheduler's acknowledgment was `lease_expired` rather than `applied`, so the renewal sample and the round trip did not fit in the one-second slack left on that lease. There was no panic. The 10 ms expiry path is not what failed. The soak's renew loop lost the race once after 992 successes.

## What this record does not close

The two-profile Laya trace is a limitation of G4, stated here, not a missing boot to go hunt. VM pause does not freeze guest time, so a lease that ends during `stop` is applied late, on `cont`. The one-hour soak failed once, as recorded above, because a renewal missed its lease. Memory, admission, and the S4 handoff remain the next build. A renew slack that leaves room for the 400 ms sample is the fix for that soak miss, and it is separate from those actuators.

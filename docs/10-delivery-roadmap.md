# Delivery roadmap and work packages

## Planning envelope

Assume a kernel/Rust engineer, an ML engineer, and an infrastructure/test engineer, with architecture review shared among them. For a team familiar with low-level Rust, budget approximately **12 weeks plus 2–4 weeks contingency for S4 qualification**. The minimum S3 demonstration may be reachable around weeks 6–8 if upstream compatibility and scheduler work proceed well. These are estimates, not delivery promises.

A single engineer should schedule the work serially and allow substantially more time. In-guest inference is excluded from this estimate. The critical path is boot compatibility → deterministic scheduler correctness → live actuation → held-out qualification, not model downloading.

## Dependencies

```mermaid
flowchart TD
    A[WP0: upstream boot and artifact pins] --> B[WP1: telemetry, ABI, protocol]
    A --> C[WP2: deterministic scheduling]
    B --> D[WP3: mock service and failures]
    C --> E[WP4: dataset and controller baselines]
    B --> E
    E --> F[WP5: Laya offline and calibration]
    C --> G[WP6: live CPU policy proof]
    D --> G
    F --> G
    G --> H[WP7: managed memory and admission]
    H --> I[WP8: final evaluation and demo]
    I --> J[Optional: in-guest inference study]
```

Telemetry and model tooling can develop alongside scheduler work, but causal action labels need working deterministic actuators. Training on invented telemetry while the real metrics remain undefined is not a substitute.

## Work packages

| ID / suggested window | Owner | Deliverable and acceptance |
| --- | --- | --- |
| WP0 / week 1 | Infrastructure + kernel | Pin source/toolchains/loader; boot untouched template in TCG; decode success/failure; record G0 manifest |
| WP1 / weeks 2–3 | Kernel | Class registration, counters, versioned ABI, bounded snapshots, baseline resource reservations; ordinary guest progress independent of controller |
| WP2 / weeks 2–5 | Kernel | Host scheduler model, timer/accounting integration, opt-in fair scheduler, immutable CPU catalog, expiry and fallback; pass G2 |
| WP3 / weeks 2–4 | Infrastructure | Authenticated bounded protocol, mock server, activation traces, timeout/reconnect/fault harness; pass G1 and relevant failure tests |
| WP4 / weeks 3–6 | ML + infrastructure | Fixed-profile sweeps, tuned heuristic, workload splits, branch labeling prototype, explicit experiment objectives |
| WP5 / weeks 4–6 | ML | Pinned Laya adapter; zero-shot report; timing profile; calibration; optional adaptation only if justified; G3 evidence |
| WP6 / weeks 6–8 | Entire team | Shadow then active Laya CPU decisions; actual scheduler effects; controller-kill recovery; S3/G4 demonstration |
| WP7 / weeks 8–10 | Kernel + ML | Tagged fallible pools, safe eviction, bounded admission, joint catalog, deterministic tests, new labels/calibration; G5 |
| WP8 / weeks 10–12 | Test lead + reviewers | Held-out paired runs, fault/soak qualification, cost report, replay package, S4 verdict, demo; G6 |

Weeks overlap because owners can work on independent components. If WP2 slips, don't convert an application-only demonstration into an S3 claim to preserve the date.

## Ticket-sized implementation breakdown

### WP0: reproducible foundation

- Inventory host capabilities and package/toolchain gaps; produce a doctor report.
- Reproduce template lane A without policy code.
- Reproduce candidate development lane B and verify its nested kernel/loader toolchains.
- Build successful and deliberately failing smoke images; test exit-code decoding and timeout cleanup.
- Capture a clean source/artifact manifest and freeze a known-working QEMU machine/CPU configuration.

Exit evidence: serial logs, checksums, commands, compiler versions, accelerator mode, expected exit statuses. Stop model integration if the original baseline cannot boot.

### WP1–WP3: mechanisms and contract

- Define fixed-capacity structures and unit conventions; establish ABI layout fixtures.
- Implement frozen class registration, task generation IDs, and telemetry counters.
- Implement scheduler reference model and adversarial service-accounting tests.
- Integrate queue transitions and timer preemption with Hermit; preserve a feature-disabled baseline.
- Implement profile catalog, staging mailbox, local expiry, emergency generation rules, and acknowledgment ring.
- Implement protocol framing, authentication, cross-language fixtures, and parser rejection tests.
- Implement mock controller and guest transport worker without locks held during I/O.
- Add process supervision, event correlation, controller absence/kill/hang tests, and artifact capture.

Exit evidence: all deterministic profiles affect actual scheduling; no inference needed for progress; failure matrix passes under TCG, with timing checks separately on KVM.

### WP4–WP6: Laya evaluation and activation

- Define the open-loop workload generator and split manifests.
- Measure fixed-profile effects and the deterministic adaptive baseline.
- Collect bounded raw telemetry and verify token budgets for the selected checkpoint.
- Implement pinned SDK loading, warm-up, backend reporting, bounded inference queue, and abstention.
- Evaluate zero-shot choices, train if justified, and calibrate on independent data.
- Run shadow mode and inspect disagreement with measured outcomes.
- Activate live CPU-only catalog in fresh QEMU trials; correlate all decision and kernel events.
- Demonstrate controller loss and recovery with continued kernel service.

Exit evidence: live S3 causal trace, model-quality report including weaknesses, qualified end-to-end deadline, and reproducible artifact bundle.

The first item has its implemented form in `jobs-v2`, `objective-v2`, and `splits-v2` ([document 08](08-data-and-learning.md#branched-labels)). The full training split, seeds 101–103, is labelled on both hosts. The first version, `jobs-v1`, stays as development evidence. The remaining WP4–WP6 order is:

1. Collect development and calibration labels on the Intel host, and every split on the office host if it is to carry labels. A unit's branches stay on one host. Budget about 1.45 boots per branch.
2. Fit the deterministic baseline, a pending-jobs threshold, and a small classifier on training labels, and select on development. In jobs-v2 one feature separates every training unit, so a learned model or an adapted Laya has to beat the tuned threshold, most likely on the unseen 45% and 72% loads or out of distribution. Keep the zero-shot checkpoint.
3. Freeze one candidate, then calibrate on the calibration split.
4. Open the sealed final test and the out-of-distribution subset once, for the held-out closed loop.
5. Decide the guest deadline separately. The 750 ms deadline stays until that explicit decision.

Progress by host. On the office AMD Ryzen 5 1600, steps 1–3 have run. The fit-v0 selection is the pending-jobs threshold, frozen as `configs/candidate-v0-r5-1600.json` before any calibration label existed ([document 08](08-data-and-learning.md#branched-labels)). On calibration it hit all 16 units, with a 95% Wilson interval of 0.806–1.0. The office host is ready for step 4's offline half. On the Intel i7-10750H, steps 1–3 have also run. Its candidate is frozen from its own development labels and is never copied from the office host: `configs/candidate-v0-i7-10750h.json`, cut 14 pending jobs, `latency` below and `balanced` at or above, committed before any Intel calibration label. On calibration it hit all 16 units, with a 95% Wilson interval of 0.806–1.0. Both hosts are ready for step 4's offline half. The Intel sequence was, from the repository root:

```bash
tools/collect-labels.sh development 151            # about 140 boots
cd controller
PYTHONPATH=src <laya-python> -m aik_controller.fit zero-shot \
  --development ../data/jobs-v2/labels-development-seed151-i7-10750h.json \
  --out ../data/jobs-v2/zero-shot-development-seed151-i7-10750h.json
PYTHONPATH=src python3 -m aik_controller.fit run \
  --training ../data/jobs-v2/labels-training-seed101-i7-10750h.json \
  --training ../data/jobs-v2/labels-training-seed102-i7-10750h.json \
  --training ../data/jobs-v2/labels-training-seed103-i7-10750h.json \
  --development ../data/jobs-v2/labels-development-seed151-i7-10750h.json \
  --zero-shot ../data/jobs-v2/zero-shot-development-seed151-i7-10750h.json \
  --out ../data/jobs-v2/fit-v0-i7-10750h.json
PYTHONPATH=src python3 -m aik_controller.fit freeze \
  --report ../data/jobs-v2/fit-v0-i7-10750h.json --out ../configs/candidate-v0-i7-10750h.json
# Commit the candidate and its evidence before any calibration boot.
cd .. && tools/collect-labels.sh calibration 201 202   # about 280 boots
cd controller && PYTHONPATH=src python3 -m aik_controller.fit calibrate \
  --candidate ../configs/candidate-v0-i7-10750h.json \
  --calibration ../data/jobs-v2/labels-calibration-seed201-i7-10750h.json \
  --calibration ../data/jobs-v2/labels-calibration-seed202-i7-10750h.json \
  --out ../data/jobs-v2/calibration-v0-i7-10750h.json
```

Step 4 has its offline half. `branches collect --split final_test --sealed-evaluation` and the same for `out_of_distribution` boot the sealed units. `collect-labels.sh` refuses them on purpose. `fit evaluate` then reads both sealed splits together and requires the following:

- The candidate and its calibration report are tracked by git and unchanged from HEAD.
- The calibration report scores that candidate.
- The fit report is the one the candidate was frozen from.
- Each split's labels cover exactly the units the manifest plans for it.
- The labels come from the candidate's host.

It refuses to overwrite its output. It scores the candidate, the fixed profiles, heuristic-v0, and the fit report's other fitted candidates on the same decisions. It reports the seen final-test configurations, the unseen 45% and 72% loads, and the out-of-distribution subset separately, with Wilson intervals over units. The other fitted candidates are reported and cannot be selected.

Step 4's closed-loop half is built and has run on development units on the Intel host. In a closed-loop branch the guest applies the frozen candidate to its own live pre-decision state and forces the chosen profile for the horizon. `closed_loop collect` checks every live choice against the offline rule, and `closed_loop score` sets each unit's closed-loop repeats beside its forced branches under objective-v2. A sealed split also needs `--sealed-evaluation` and the candidate's committed calibration report. On Intel development seed 151, all 39 live decisions matched the offline rule, in 23–55 µs. The candidate hit 7 of 8 units, with a 95% Wilson interval of 0.529–0.978. The miss is s12-u80/151, where the candidate chose balanced and those repeats completed 10 basis points under the 95% floor set by the forced balanced branches from 90 minutes earlier ([document 08](08-data-and-learning.md#branched-labels)). The office host runs the same development check with its own candidate before any sealed closed-loop boot there:

```bash
cd controller
PYTHONPATH=src python3 -m aik_controller.closed_loop collect --candidate ../configs/candidate-v0-i7-10750h.json \
  --split development --seed 151 --out ../data/jobs-v2/closed-loop-development-seed151-i7-10750h.jsonl
# --fill boots only slots still missing a usable record.
PYTHONPATH=src python3 -m aik_controller.closed_loop score --candidate ../configs/candidate-v0-i7-10750h.json \
  --labels ../data/jobs-v2/labels-development-seed151-i7-10750h.json \
  --records ../data/jobs-v2/closed-loop-development-seed151-i7-10750h.jsonl \
  --out ../data/jobs-v2/closed-loop-development-seed151-i7-10750h.json
```

### WP7–WP8: broader policy and final handoff

- Add tagged arenas and safe cache eviction with ownership tests.
- Add admission accounting, queued-byte limits, and drain-on-limit-reduction behavior.
- Freeze a joint catalog; regenerate training/calibration data for that action space.
- Validate desired versus actual resource convergence.
- Run held-out trials, per-scenario reports, cost accounting, and soak/fault qualification.
- Package demo instructions, exact artifacts, trace viewer/report, and reviewer sign-off.

Exit evidence: engineering verdict and research verdict separately stated, with all limitations attached to the relevant claims.

## Stop/go decisions

| Trigger | Required response |
| --- | --- |
| No compatible upstream build after a time-boxed 3–5 day investigation | Choose another explicitly pinned upstream pair and document the compatibility issue |
| Scheduler cannot preserve progress under allowed profiles | Continue deterministic mechanism work; do not activate Laya |
| Laya exceeds decision deadline | Measure a slower declared control interval or a justified alternate backend; reconsider workload timescale |
| Profiles are not meaningfully distinguishable | Improve the actuator/workload experiment before training |
| Zero-shot Laya is poor | Evaluate targeted adaptation and a simple classifier; preserve zero-shot result |
| Laya loses to the tuned heuristic after adaptation | Report negative policy-quality result; decide whether to keep it only as an architecture demonstration |
| In-guest port dominates remaining effort | Keep S4 as the deliverable and schedule the port as a separate project |

## Review checkpoints

At the end of weeks 1, 5, 8, and 12, review tangible artifacts: a boot log; a scheduler service trace and fault report; a live AI actuation/recovery trace; and a held-out evaluation package. Reviewers should be able to reproduce each checkpoint without relying on an engineer narrating what happened.

The implementation branch should never label a milestone complete merely because all its source files exist. Each work package exits on the evidence above.

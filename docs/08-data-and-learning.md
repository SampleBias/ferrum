# Data, learning, and policy-quality evaluation

## Start with a measurement problem

A model trained for typed text decisions has not thereby learned the causal effect of giving a batch thread more CPU. The first data objective is to discover whether the permitted profiles have useful, distinguishable effects on the chosen workloads. If all profiles perform alike, no model-selection experiment can establish useful policy intelligence.

Collect traces before fine-tuning. Use the same telemetry schema for heuristics and Laya so the model does not receive privileged information unavailable to the comparison controller.

## Workload suite

| Family | Controlled variables | Main measurements |
| --- | --- | --- |
| Short-job bursts | Arrival rate, burst size, CPU work per job | End-to-end p50/p95/p99, backlog, completion/drop rate |
| Sustained batch | Number of workers, compute intensity, job size | Work units/s, CPU share, starvation gaps |
| Mixed service | Burst phases overlapping batch | Latency/throughput tradeoff, profile changes |
| Managed-memory pressure | Object size/lifetime, cache working set, eviction work | Peak charged bytes, failed/deferred allocations, reclamation time |
| Idle/recovery | Empty intervals, sleeping tasks, sudden wakeups | Wake latency, unfair accumulated credit, stale decisions |
| Adversarial timing | Controller delay, rapid phase changes, yield-heavy threads | Deadline rejection, oscillation, fallback, fairness |

Start with synthetic deterministic workload generators because their inputs and seeds are inspectable. Add one realistic Rust service workload after the mechanisms work. Synthetic success alone does not establish benefits for general applications.

Use open-loop arrivals for latency measurements so a slow guest does not quietly reduce offered load. Measure queueing from scheduled arrival or explicitly report generator lag. Track failed, deferred, and dropped jobs; reporting latency only for the few admitted jobs can make aggressive shedding appear to improve service.

## Required records

Each decision record contains run/boot/session/request IDs; source/model/catalog hashes; workload family and seed; raw counters; feature encoding; question version; selected action or abstention; full available probability vector; calibration version; timing components; guest validation/activation result; active lease; and the subsequent outcome window.

Retain both guest monotonic timestamps and host timestamps with their clock domains named. Use IDs and local durations for causality, not subtraction across unsynchronized clocks. Record lost-event counters. Preserve original model outputs alongside normalized protocol proposals on the host.

Do not store raw memory or user content. The proposed workloads are generated test data, and the policy should not need application secrets.

## Baselines before model training

Evaluate every fixed profile, the balanced fallback, a tuned deterministic adaptive controller, and Laya zero-shot. A simple adaptive controller can use queue/SLO ratios with hysteresis and memory-pressure rules; tune its thresholds on training/development data only. Include a small classifier or tree if it answers whether an expensive language-based model is necessary.

All controllers use the same permitted actions, guest mechanisms, decision interval, dwell rule, and actuation delays. A non-ML controller must not be artificially slowed to simulate inference; report its real cost. Conversely, compare an additional matched-delay condition if isolating decision quality from latency is useful.

The heuristic is both a baseline and a possible label source, but imitating it does not prove superiority over it. Keep independently measured action outcomes for that purpose.

## Generating action labels

Create decision opportunities from a scenario prefix and branch each candidate profile into a fresh, repeatable trial. Run the same generator seed and prefix, then force one approved profile at the designated decision point for a fixed outcome horizon, initially 5–10 seconds. Repeat branches to estimate variability.

A telemetry snapshot does not contain the full scheduler, heap, cache, and network state. Replaying its JSON cannot produce a causal counterfactual. Replaying the whole scenario prefix gives approximately comparable states; verify the comparison with measured state, and report residual variability. VM snapshots are optional later and must restore guest clocks/leases while restarting or coordinating the external controller explicitly.

For each opportunity, remove actions that violate empirical constraints, compare remaining latency/throughput outcomes, and record ties or uncertainty. Use soft labels or omit genuinely ambiguous opportunities from supervised labels while retaining them for evaluation. Never force a single “oracle” winner when confidence intervals overlap substantially. If no candidate meets the objective, mark the action set inadequate rather than invent an ideal answer.

Only pre-decision features enter the model. Future outcomes determine labels and evaluation, not input features. A rule that looks at the next workload phase is an offline oracle upper bound, not a deployable baseline.

## Branched labels

This section is the implemented form of the procedure above. Code: `crates/workloads/src/jobs.rs`, `apps/policy-guest/src/branch.rs`, `controller/src/aik_controller/branches.py`. The current frozen inputs are `configs/workloads/jobs-v2.json`, `configs/objective-v2.json`, and `configs/splits-v2.json`. The first version, `jobs-v1` with `objective-v1` and `splits-v1`, stays frozen with its pilot records as development evidence; its pilot set the changes that version 2 makes.

**Why the mixed-v1 windows cannot produce labels.** Those windows measure group CPU service, and the scheduler sets group service from the weights of whichever profile is installed. A label drawn from service share restates the forced profile. A label needs an outcome of work: how long jobs waited and how much batch work finished.

**The job family.** Latency work arrives on a seeded open-loop schedule, so a slow guest still receives the full offered load. Each job is a fixed amount of guest CPU time, and its latency runs from the scheduled arrival to completion. Three batch workers stay runnable and count 500 µs units. Maintenance is off. Arrivals are Bernoulli draws on a 100 µs grid from SplitMix64, built with integers only, so the Rust guest and the Python harness produce the same schedule bit for bit. Every record carries its FNV-1a schedule digest, and the harness refuses a record whose digest differs from its unit.

The scheduler shapes the scenario table. The quantum is 2 ms, and a woken class is clamped to the minimum virtual runtime of the runnable classes, so it runs first. A job of one quantum or less finishes in its first slice under any profile. The profiles separate only when latency work stays runnable across several slices, so jobs are 4, 12, and 32 ms. Under balanced and reclaim, latency and batch split 1:1; latency takes 3:1 and throughput 1:3. Offered latency load runs from 15% to 75% of one CPU in jobs-v1 and from 15% to 95% in jobs-v2, which holds no load between 45% and 65% for the reason the v1 pilot gives below. Below an even split the latency profile should cut tail latency at no batch cost, because the scheduler is work-conserving and batch receives whatever the jobs leave. Above it, the latency profile serves the jobs by taking CPU that batch would have had. Reclaim gives latency and batch the same 1:1 split as balanced in this family, so every unit carries an equal-weight A/A pair that measures noise directly.

`sys_usleep` under 10 ms busy-waits inside the caller's class, which would charge an idle job worker as latency demand. Job workers block with a millisecond timeout instead. A job can start up to 1 ms late, and that sets the latency floor.

**One branch.** Each branch is a fresh boot of `policy-guest --branch --family=… --scenario=… --seed=… --profile=…`. The guest calibrates its spin rate, runs 3 s under balanced, and prints `FERRUM_BRANCH_STATE`, the pre-decision snapshot over the last second (pending jobs, latency and batch service, oldest wait, completions). It then forces the profile with `sys_policy_set_weights` and measures jobs that arrive in the next 5 s. A 1 s drain lets late arrivals finish; arrivals continue meanwhile. A job still unfinished at the end of the drain is incomplete. A percentile whose rank lands on an incomplete job is censored.

**Host speed is the main noise source, and it shaped two choices.** On this Intel laptop, host speed differs by about 5% from one boot to the next. With work sized in fixed spins, two equal-weight branches differed by 11% in batch units, which is larger than the retention margin. An unwarmed 20 ms calibration missed the in-trial speed by several percent. Each boot now warms up for 300 ms and takes the median of five runs of at least 100 ms. That predicts in-trial speed within about 1.5%, and work is sized in guest CPU time from it. The outcome line reports in-trial batch speed. A branch more than 5% away from its own calibration is `disturbed`: the harness keeps the record and reruns the branch. Batch is scored as guest CPU time over the horizon, which carried about 1.5% boot-to-boot spread where units carried about 5%. Batch time measures work in this family because latency demand is open-loop: it falls only when a profile takes CPU that batch would otherwise have used. objective-v2 applies the same 5% test to the prefix window, where speed is batch units times 500 µs over batch CPU time, because the prefix sets the backlog that the forced profile inherits.

**The label rule (objective-v1 and objective-v2).** Each unit gets three repeats of every candidate. Profile order is shuffled per unit and repeat, and repeats are interleaved across units, so host drift does not line up with a profile.

1. A candidate is feasible when its median batch CPU is at least 95% of the balanced branch's median, and its median completion is at least 95% of balanced's. Balanced is the fallback profile, so it is always feasible.
2. The best feasible candidate has the lowest median p99. A censored p99 ranks after every uncensored one, and censored candidates rank by completion, then batch CPU.
3. Every feasible candidate whose repeat range overlaps the best's range ties with it. The label is that set. With more than one member the unit is a `tie`, and a soft label spreads weight over the set.
4. The branches of a unit must share a prefix. The schedule digest is identical by construction. Pending jobs at the decision point may spread by at most 4 jobs or a share of the median: 25% in objective-v1 and 50% in objective-v2, which is the spread a 5% speed band admits at 65% load. A unit outside that is `diverged`: it is kept for evaluation and left out of supervised labels.

The recommended closed-loop objective below keeps 95% of the strongest baseline's batch throughput, and that comparison is between whole controllers. Inside one decision opportunity, the strongest batch branch is always throughput, so measuring against it would let the latency profile qualify only when latency and batch never contend. The per-opportunity rule therefore measures against the fallback profile.

**Splits (splits-v2).** A unit is one (scenario, seed) pair, and every branch of a unit stays in its split. Seeds are disjoint across splits. Training has eight configurations by three seeds: 4 ms and 12 ms jobs at 15%, 35%, 65%, and 80% load, so half of training sits under an even split and half over it. Development has one more seed each, and calibration has two more. The final test holds four seen configurations on new seeds, plus unseen loads on each side of the boundary: 45% under it and 72% between the trained overload loads. The out-of-distribution subset holds a 95% load and the 32 ms job size at 35% and 65%. splits-v1 had six training configurations, an unseen 45% load, and a 75% load and 32 ms jobs out of distribution; it stays frozen with the v1 pilot. The manifest stores the family and objective hashes and each unit's schedule digest, and the loader recomputes all of them. `branches collect` refuses the final test and the out-of-distribution subset without `--sealed-evaluation`, which belongs to the evaluation of a frozen candidate. A fit reads training labels whose manifest hash matches the current manifest (`splits.check_fit_labels`). Labels are recorded per host and never pooled across hosts.

```bash
cd controller
# --manifest defaults to splits-v2.
PYTHONPATH=src python3 -m aik_controller.branches collect --split training --seed 101 \
  --retries 2 --out ../data/jobs-v2/training-seed101-<host>.jsonl
# Boot only the branches that still lack a usable record, for example after disturbed retries.
PYTHONPATH=src python3 -m aik_controller.branches collect --split training --seed 101 \
  --retries 2 --fill --out ../data/jobs-v2/training-seed101-<host>.jsonl
PYTHONPATH=src python3 -m aik_controller.branches label \
  --records ../data/jobs-v2/training-seed101-<host>.jsonl \
  --out ../data/jobs-v2/labels-training-seed101-<host>.json
```

Each record names its manifest and manifest hash, and `label` refuses a set that mixes manifests, splits, or hosts. The jobs-v1 records predate that field, so they label with `--manifest splits-v1`, which reproduces the committed v1 labels byte for byte.

**Pilot on jobs-v1 (Intel i7-10750H, `kvm_intel`, training seed 101 only).** 72 branches took 84 boots. Twelve were refused as disturbed, kept in the records, and rerun. Five of the six units labelled `latency` with repeat ranges well clear of the equal-weight pair. For example, at 4 ms and 35% load latency p99 was 34–38 ms against 111–127 ms for balanced and reclaim, at 99% of balanced's batch CPU. At 35% load throughput completed 73–92% of the jobs and was infeasible; at 15% it completed every job with the worst p99. At 4 ms and 55% load the measured answer flips. Latency kept 74% of batch CPU and was infeasible, balanced and reclaim completed 86–95% of the jobs with p99 censored, and the label was {balanced, reclaim}. The heuristic chose `latency` in all six units, so the overload unit is the only one where a measured label disagrees with it.

That unit is `diverged`. Pending jobs at the decision point ranged from 9 to 94 across its usable branches. Its offered load is 5 points over an even split, so the backlog grows with that excess, and a 1% change in host speed moves the backlog by 10–20%. The branch with 9 pending ran 12.9% fast during the prefix while its horizon speed was normal, which the horizon-only speed gate does not see. The labels file reports `prefix_drift_bp_max` per unit, and objective-v2 gates each branch on that speed.

The pilot answered the handoff's check: the labels are measured, repeatable, and separated far beyond equal-weight noise. It also showed three changes to make before collecting more labels, and version 2 makes them as new frozen files. The v1 records and labels stay as development evidence.

**What version 2 changes.**

1. Overload loads sit at least 15 points over an even split: 65% and 80% in training, 72% in the final test, and 95% out of distribution. At 65% a 5% speed error moves the backlog by about 22%. The 45% loads stay, since a load under capacity builds no backlog.
2. The disturbance gate covers the prefix window as well as the horizon, and the pending tolerance widens to 50% of the median to match the backlog a 5% speed band admits.
3. Half of the training configurations sit over an even split, so both measured answers carry equal training weight.

**Pilot on jobs-v2 (Intel i7-10750H, `kvm_intel`, training seed 101 only).** 96 branches took 135 boots in about 25 minutes. 39 boots were refused as disturbed and rerun: 28 by the horizon gate and 11 by the new prefix gate. Every unit received a label, and none diverged.

- The four units under an even split labelled `latency`. At 4 ms and 35% load, latency p99 was 33–40 ms against 125–140 ms for balanced and 113–141 ms for reclaim, at 102% of balanced's batch CPU. At 12 ms and 35% it was 46–52 ms against 99–117 ms for balanced and reclaim.
- The four overload units tied {balanced, reclaim}. Latency kept 50–55% of balanced's batch CPU, which is infeasible. In return it finished every job with p99 of 0.5–1.04 s, except at 4 ms and 80%, where it finished 90–95%. Balanced and reclaim finished 52–82% of the jobs with p99 censored, and throughput finished 13–33%.
- Prefixes were comparable: pending jobs at the decision point spread by at most 22 around medians from 34 to 230.
- The heuristic chose `latency` in every unit, so it disagrees with the measured label in all four overload units.

Refusals rose from 14% of boots in the v1 pilot to 29%. In-trial speed ran a median of about 1.1% under the boot's calibration in both sessions, and its 10th percentile fell from −4.6% to −6.8%, so this session's host was noisier. Budget about 1.4 boots per branch on this host.

**Training seeds 102 and 103 (same host).** Both seeds gave the same labels as seed 101: `latency` at 15% and 35% load, and a {balanced, reclaim} tie at 65% and 80%. All 16 units received a label, and none diverged. Seed 102 took 130 boots with 34 refused, and seed 103 took 154 boots with 58 refused. The full training split is 288 branches in 419 boots, about 1.45 boots per branch and 75 minutes of boots. Latency's margin moves with the seed while its sign holds. At 12 ms and 35% load, latency p99 was 72–98 ms against balanced 139–140 ms on seed 102, and 85–134 ms against 170–183 ms on seed 103. At 65% and 80%, latency kept 50–62% of balanced's batch CPU on both seeds, well under the 95% floor. The largest pending spread in any unit was 29 jobs.

**What the v2 labels mean for the next step.** In this family the measured answer depends on one quantity: whether offered latency load exceeds an even split. The pre-decision state carries it directly. Across the usable branches of all three training seeds, pending jobs at the decision point were 0–2 under an even split and 26–274 over it. The oldest wait was 16–193 ms under it and 0.41–1.47 s over it. A one-feature threshold on pending jobs separates every training unit with a wide margin. Heuristic-v0 chooses `latency` once the oldest wait reaches 5 ms, and every unit's oldest wait is at least 16 ms, so it chooses `latency` everywhere. This sets a precise bar for the classifier step in document 10. The deterministic baseline is a pending-jobs threshold tuned on training labels, and a learned model or an adapted Laya earns its place by beating that threshold on the unseen 45% and 72% loads or out of distribution. Under overload, objective-v2's 95% batch-retention rule prefers finishing 52–82% of the jobs to halving batch work. That preference belongs to the frozen objective, and a later objective version can revisit it explicitly.

**Training split on the office host (AMD Ryzen 5 1600, `kvm_amd`).** Seeds 101–103 ran with the same guest, family, objective, and manifest. 288 branches took 303 boots, 15 refused as disturbed and none failed, about 1.05 boots per branch against 1.45 on the Intel laptop. Every unit received a label and none diverged. The labels match the Intel host in 23 of 24 units: `latency` at 15% and 35% load and {balanced, reclaim} at 65% and 80%. The exception is s12-u80/103, labelled `reclaim` alone. Both p99s were censored, so the rule ranked by completion. Balanced finished 63.3–63.6% of the jobs and reclaim 64.3–65.2%, so the repeat ranges missed each other by 0.7 points. Those two profiles give latency and batch the same 1:1 split in this family, so that is the equal-weight pair separating by noise. Across the 12 office overload units reclaim's median completion was higher in 8, which a sign test does not distinguish from chance (p ≈ 0.39). Pending jobs at the decision point were 0–2 under an even split and 27–276 over it, and the oldest wait was 16–170 ms under it and 0.44–1.31 s over it.

**Fit and selection (fit-v0).** Code: `controller/src/aik_controller/fit.py`. The declaration `configs/fit-v0.json` (sha256 `61c8ea11ac8e609ba86836a76390149bc4208aa9a6e899b2443cf9340be741d4`) was committed before any development label was measured on either host. A decision is one usable branch that a unit's label reads: that branch's own pre-decision state, scored against the unit's label set and its measured profile summaries. Every branch runs the same balanced prefix, so a unit's twelve branches are twelve draws of the state a controller would see there. A choice in the label set is a hit. A choice that fails the 95% batch or completion floor is infeasible. Diverged units are listed and not scored. Before scoring, each labels file must reproduce its labels from its records. The candidates, simplest first, are:

1. The four fixed profiles.
2. Heuristic-v0.
3. The pending-jobs threshold. Each side of the cut takes the profile found in the most training label sets on that side.
4. A logistic regression on log pending jobs, log oldest wait, and latency service share, with soft targets spread over the label set.
5. Zero-shot Laya, given the live state with its profile field.

Selection takes the fewest infeasible development decisions, then the fewest misses. A remaining tie goes to the earlier candidate, so a learned model or Laya is selected only when it strictly beats every simpler candidate. `freeze` writes `candidate-v0` for that host. `calibrate` then scores the frozen candidate once on that host's calibration labels, with a Wilson interval over units. A threshold has no confidence to rescale, so for it calibration is an error estimate on independent units before the sealed splits open. `fit` refuses labels that name more than one host.

```bash
cd controller
PYTHONPATH=src python3 -m aik_controller.fit run \
  --training ../data/jobs-v2/labels-training-seed101-<host>.json \
  --training ../data/jobs-v2/labels-training-seed102-<host>.json \
  --training ../data/jobs-v2/labels-training-seed103-<host>.json \
  --development ../data/jobs-v2/labels-development-seed151-<host>.json \
  --zero-shot ../data/jobs-v2/zero-shot-development-seed151-<host>.json \
  --out ../data/jobs-v2/fit-v0-<host>.json
# The zero-shot report needs the Laya environment and runs before the fit.
PYTHONPATH=src <laya-python> -m aik_controller.fit zero-shot \
  --development ../data/jobs-v2/labels-development-seed151-<host>.json \
  --out ../data/jobs-v2/zero-shot-development-seed151-<host>.json
PYTHONPATH=src python3 -m aik_controller.fit freeze \
  --report ../data/jobs-v2/fit-v0-<host>.json --out ../configs/candidate-v0-<host>.json
PYTHONPATH=src python3 -m aik_controller.fit calibrate --candidate ../configs/candidate-v0-<host>.json \
  --calibration ../data/jobs-v2/labels-calibration-seed201-<host>.json \
  --calibration ../data/jobs-v2/labels-calibration-seed202-<host>.json \
  --out ../data/jobs-v2/calibration-v0-<host>.json
# Once, after the candidate and calibration report are committed and both sealed splits are labelled.
PYTHONPATH=src python3 -m aik_controller.fit evaluate --candidate ../configs/candidate-v0-<host>.json \
  --calibration-report ../data/jobs-v2/calibration-v0-<host>.json --fit-report ../data/jobs-v2/fit-v0-<host>.json \
  --final-test ../data/jobs-v2/labels-final_test-seed301-<host>.json \
  --final-test ../data/jobs-v2/labels-final_test-seed302-<host>.json \
  --out-of-distribution ../data/jobs-v2/labels-out_of_distribution-seed401-<host>.json \
  --out-of-distribution ../data/jobs-v2/labels-out_of_distribution-seed402-<host>.json \
  --out ../data/jobs-v2/evaluation-v0-<host>.json
```

**Training fits.** Without development labels `fit` reports the fits and selects nothing. On the Intel training seeds the cut is 14 pending jobs, between observed values 2 and 26: `latency` below and `balanced` at or above. It hits all 288 decisions. On the office training seeds the cut is 14.5, between 2 and 27, with `latency` below and `reclaim` above, also 288 of 288. The office upper side is `reclaim` because reclaim is in all twelve office overload label sets and balanced is in eleven. In jobs-v2 that choice gives the same split as balanced. It would not with maintenance work, and fit-v0 is not changed after the fact to prefer the fallback. On both hosts the logistic regression also hits all 288, heuristic-v0 hits 144 and is infeasible in the other 144, and fixed latency matches heuristic-v0.

**Development and selection on the office host.** Development seed 151 took 96 boots for 96 branches, with none refused or failed. Records are `data/jobs-v2/development-seed151-r5-1600.jsonl` (sha256 `ac304b4849ce660af2c324fb70df549128d6e05c56d0ef40143a451e217183f8`) and labels are `data/jobs-v2/labels-development-seed151-r5-1600.json` (sha256 `24fd2deccddd21ebee3ca0574de2947c86913ff744249059684bba3ef3ec8ff2`). The four units under an even split labelled `latency`, the four over it tied {balanced, reclaim}, and none diverged. Pending jobs at the decision point were 0–3 under an even split and 17–291 over it. The low end is a 12 ms, 65% branch, 2.5 jobs above the office cut. The oldest wait kept its margin: 32–85 ms under and 0.49–1.51 s over. Zero-shot Laya ran on the pinned checkpoint, CPU float32, six torch threads. It chose `balanced` for all 96 states, with `answer_confidence` 0.340–0.443, 312–314 input tokens, nothing truncated, and forwards of 1.73–2.47 s. The report is `data/jobs-v2/zero-shot-development-seed151-r5-1600.json` (sha256 `469a01da7423a5a480fd922effa35fc0de02d2f9097ee7c12b20181baac90cd9`). Its `kind` field compares Laya with heuristic-v0, which chose `latency` everywhere, so all 96 rows read `disagree`. The development scores, out of 96 decisions, were:

| Candidate | Hits | Infeasible |
| --- | --- | --- |
| fixed balanced, fixed reclaim, zero-shot Laya | 48 | 0 |
| fixed latency, heuristic-v0 | 48 | 48 |
| fixed throughput | 0 | 72 |
| pending-jobs threshold, logistic regression | 96 | 0 |

Every miss by balanced, reclaim, or Laya is a unit under an even split, where latency's p99 was lower while it kept at least 99% of balanced's batch work. Every infeasible latency choice is an overload unit. The threshold and the logistic regression tie, so the declared order selects the threshold. The fit report is `data/jobs-v2/fit-v0-r5-1600.json` (sha256 `d179fc4745b96dbd38bb3028b7c85c389b1c7956a84d9389a850a3378c5842fb`). `configs/candidate-v0-r5-1600.json` (sha256 `3f1304fcefc46384dfc735d526982ccf546a90cb3ee39d3cc41d5015abfc6e89`) freezes cut 14.5, `latency` below and `reclaim` at or above. It was committed in 3bff0a4 at 09:39:07 local time. The first calibration label file was written after that.

**Calibration on the office host.** Calibration seeds 201 and 202 were collected after the freeze. The first calibration label file was written at 09:56:52, about 18 minutes after the candidate commit. 192 branches took 198 boots, with 6 refused as disturbed and none failed. Records are `data/jobs-v2/calibration-seed201-r5-1600.jsonl` (sha256 `8206ea3ad94c05407ac6dce442e5e26de824c884495caa74f31d5de367bbed11`) and `data/jobs-v2/calibration-seed202-r5-1600.jsonl` (sha256 `f9c3a5a0e029186b6c51d4712d453430f8d33a83ab12fd63b772854e68fc8933`). Labels are `labels-calibration-seed201-r5-1600.json` (sha256 `1bbf3cb1358e8db47578cb33fbac1f8490296b11f2219913d76775e4e78fe265`) and `labels-calibration-seed202-r5-1600.json` (sha256 `38cacd6fe792cd8480b9d6ec42c911e5d8eaf1ca36b3fb6dec8be79db7c50552`). Every unit labelled as in training: `latency` under an even split and {balanced, reclaim} over it, and none diverged. Scored once, candidate-v0 hit all 192 decisions and none were infeasible. It chose `latency` in the eight units under an even split and `reclaim` in the eight over it. That is 16 of 16 units, with a 95% Wilson interval over units of 0.806–1.0. The lower bound reflects how few units there are, not a miss. The report is `data/jobs-v2/calibration-v0-r5-1600.json` (sha256 `17fe74ca4efc5ade41cb242a19ed36011a549d27d579f163db0c13aad619bcaf`). Pending jobs were 0–6 under an even split and 44–263 over it, and the latency share was 0.08–0.36 under and 0.499–0.501 over. Calibration repeats the training configurations on new seeds. It estimates the error on independent units of configurations already seen. It says nothing about the unseen loads or the 32 ms job size.

**What the frozen threshold is expected to miss.** This was written before any sealed unit ran. Pending jobs count jobs, not queued work. Over an even split the backlog after the 3 s prefix is about (load − 50%) × 3 s of work, and pending jobs are that divided by job size. The office medians were 0.85–1.3 times that estimate, for example 32 at 12 ms and 65% against 37.5. The unseen 45% and 72% loads sit far from the cut. The out-of-distribution 32 ms jobs at 65% load predict about 14 pending, with a training-scaled spread of roughly 6 to 20, which straddles the cut. A branch under it gets `latency`, which is infeasible in every overload unit measured so far. The oldest wait and the latency share do not depend on job size. Over an even split the latency share was 0.499–0.501 in every training state, against 0.09–0.42 under it. The logistic regression weights all three features. Its standardized weights against `latency` are −1.35 for pending, −1.15 for oldest wait, and −0.81 for share. It prefers `reclaim` for any pending count from 6 to 20 when the share is 0.5 and the oldest wait is 0.49–1.5 s. candidate-v0 is not changed for this. The final test scores it as frozen. A later fit version that prefers job-size-free features would be new work. Its evidence would come from splits that have not yet been opened.

## Data splits and leakage control

Split by complete scenario families/configurations and generator seeds before fitting anything. Separate training, model-selection/development, calibration, and final test sets. Adjacent windows from one run stay in the same split. Test sets should include unseen load ranges and a declared out-of-distribution subset.

Fit feature thresholds, normalization, question wording, profile descriptions, hyperparameters, and heuristic thresholds without consulting final-test outcomes. Publish split manifests and dataset hashes. If the final test informs a design change, freeze a new untouched test set and report the earlier result as development evidence.

An initial planning target is thousands of varied decision opportunities, not thousands of highly correlated windows from one trace. Start smaller to verify labeling quality. Estimate uncertainty and coverage before purchasing a large training run; data diversity and branch cost determine the necessary size.

## Training and calibration

Use the pinned upstream training tooling only after verifying it supports the required choice formulation. Start with supervised/decision-head adaptation or the upstream supported training recipe on collected labels. Full reinforcement learning in the kernel is not required for the concept.

Compare base versus adapted Laya, and retain an untouched base artifact. Calibrate the final inference backend on the calibration split; quantization, option reordering, feature changes, and tokenizer changes can invalidate calibration. Evaluate reliability diagrams, Brier score, selective risk/coverage, and per-profile confusion, not accuracy alone.

The controller gate should be chosen by held-out expected decision regret or failure frequency at useful coverage. A well-calibrated classifier can still be wrong on the most consequential case. Hard bounds in Rust remain the source of enforcement.

## Offline versus closed-loop evidence

Offline agreement with the best observed profile is a screening metric. Once a policy controls resource allocation, it changes the future states it sees. Run full closed-loop trajectories on held-out workloads, including long overloads and recovery. Compare cumulative service, oscillation, fallback frequency, and final resource state.

Freeze a primary objective before final evaluation. Recommended first objective: reduce p99 short-job latency during the predefined mixed-load suite, while retaining at least 95% of the strongest selected baseline's batch throughput and at least 95% of its successful offered-work completion fraction. Also enforce memory and runnable-progress limits. Do not change the primary objective to whichever metric improves afterward. This objective compares whole controllers. Per-opportunity labels use `objective-v2`, which measures retention against the balanced branch of the same opportunity.

For memory experiments, add peak managed bytes and time to safely converge to a cache target. For admission experiments, report end-to-end outcomes over all offered work. Controller coverage, model cost, and rejected decisions accompany every quality result.

## Model promotion artifact

A promotion bundle contains checkpoint/tokenizer/config hashes, Python/backend versions, feature and question versions, calibration artifact, action catalog, split manifests, offline report, closed-loop report, and the approved timing configuration. A reviewer signs off on the bundle. Deployment selects a bundle by hash; it never follows a moving model-repository branch.

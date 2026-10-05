# Demonstration and team review

## Intended demonstration

Show a bootable Rust unikernel whose actual CPU service allocation changes in response to live Laya decisions, then show that the guest continues when inference disappears. At S4, also show safe memory-target convergence and admission changes. Keep the live and mock demonstrations visibly distinct.

## Preparation

Use a frozen artifact bundle with the selected guest/loader/controller/model/configuration hashes. Preload weights, complete model warm-up, verify the selected accelerator/backend, and archive the environment manifest. Prepare baseline, mock, shadow, and live runs with distinct names.

The display can be a terminal report or generated plot. It should show offered/completed work, p99 latency, batch throughput, actual CPU service by class, current desired profile, guest generation, lease remaining, managed bytes, admission limits, fallback state, and decision latency. Do not infer activation solely from the controller's output.

## Demonstration sequence

1. **Boot with no controller.** Show loader/kernel identities, the balanced profile, and completed work. This establishes independent execution.
2. **Prove the actuator with a mock.** Force each allowed CPU profile with all three groups runnable. Show actual scheduled-service changes and progress for every thread. Label this run as deterministic mechanism validation.
3. **Run the conventional baseline.** Replay the declared mixed-load scenario and record its outcome metrics.
4. **Run Laya in shadow.** Show input summaries, selected profiles, confidences, and hypothetical timing without claiming those profiles affected execution.
5. **Run live Laya on a fresh guest.** Start the same workload family/seed under its declared trial conditions. Correlate the model decision, guest acceptance, activation generation, and measured CPU service.
6. **Introduce a latency burst and later sustained batch demand.** Let the model decide. If it makes an unhelpful choice or never changes policy, show that result; do not replace it with a scripted answer while calling it live inference.
7. **At S4, introduce managed-memory pressure.** Show desired versus actual cache bytes, safe eviction backlog, and admission draining. Verify hard pool caps remain unchanged.
8. **Stop the controller process.** Observe the remaining lease, independent expiration, fallback generation, and continued workload progress. Attribute any VM restart as a failure/recovery event rather than uninterrupted service.
9. **Inject stale and invalid messages through the test controller.** Show explicit rejection and no unauthorized profile change. Again label the injection run as such.
10. **Present the report.** Compare repeated held-out trials, costs, failures, and limits. A live demonstration illustrates the system; it does not replace the statistical evaluation.

## Required causal trace

The report should contain a compact example with real values in this shape:

```text
snapshot request=42 base_generation=7 profile=balanced
inference request=42 choice=latency model=<hash> duration=<measured>
validated request=42 catalog=<hash> age=<measured>
applied request=42 generation=8 profile=latency lease_until=<guest time>
service window=<interval> latency=<runtime> batch=<runtime> maintenance=<runtime>
fallback generation=9 reason=lease_expired
```

The lines above are an expected format, not collected output. Preserve original records with identifiers and clock domains so a reviewer can verify the narrative.

## Architecture review checklist

- [ ] Every claimed AI-managed domain has a named actuator and measured effect.
- [ ] At least one actuator changes the actual Hermit scheduler.
- [ ] The model never enters interrupt, context-switch, allocator-correctness, or boot paths.
- [ ] Every catalog profile preserves immutable limits under the supported workload assumptions.
- [ ] Local expiry works when the bridge/model is unavailable.
- [ ] Joint desired state activates atomically, with convergence tracked separately.
- [ ] The application/kernel ABI is explicit and its unsafe boundary reviewed.
- [ ] The ordinary guest is described as one trust domain.
- [ ] CPU reservation, memory reservation, and transport bounds are concrete and tested.
- [ ] No procedure silently expands the action set or inference deadline.

## ML and experiment review checklist

- [ ] Code, weights, tokenizer, questions, preprocessing, and calibration are pinned.
- [ ] Real end-to-end latency and model resource use were measured.
- [ ] Training, development, calibration, and test scenarios are separated.
- [ ] Labels come from defensible outcome comparisons; future information is excluded from inputs.
- [ ] Static and strong adaptive non-ML baselines use the same mechanisms and actions.
- [ ] Full trajectories test closed-loop behavior and distribution shift.
- [ ] Failed/deferred work, generator lag, and inference costs appear in results.
- [ ] Confidence gates are calibrated and do not substitute for kernel enforcement.
- [ ] Negative findings and per-scenario regressions are retained.

## Reproduction and handoff checklist

- [ ] A clean machine can follow the recorded build and QEMU instructions.
- [ ] TCG and KVM results are separately labeled.
- [ ] Success/failure guest exit codes and timeout cleanup are verified.
- [ ] The bundle includes raw traces, hashes, seeds, workload definitions, and reports.
- [ ] A response-replay mode exists for enforcement debugging and is distinct from live-model mode.
- [ ] An independent engineer reproduced boot, live activation, and controller-loss recovery.
- [ ] Owners and known limitations are assigned for every remaining issue.

## Final acceptance record

Write one record stating the achieved level (S0–S5), passed gates, failed gates, observed violations, hardware, policy domains, model artifact, and research verdict. The architectural claim may be accepted at S3 even if Laya fails to beat the heuristic, provided that limitation is explicit. S4 additionally requires the broader deterministic actuators and their joint-control evidence.

A release or presentation should link this acceptance record and the reproduction bundle. Do not call the planning files themselves a completed kernel.

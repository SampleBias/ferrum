# Path to a self-contained AI unikernel

## What changes after the paired prototype

The external-controller design tests the central idea without requiring a new inference platform. If the requirement later becomes “one bootable image contains both Laya and the Rust kernel,” the project must solve runtime portability and resource isolation as well as policy quality.

The model still runs as ordinary guest work. Moving it into the image does not justify putting it in an interrupt handler or blocking the scheduler on inference. Keep the same bounded proposal interface, catalog, lease semantics, fallback, and comparison baselines.

## Candidate paths

| Path | Porting work | What the resulting claim means |
| --- | --- | --- |
| Companion Linux VM | Package controller and transport; qualify new latency/resources | Paired VM system; inference still uses Linux |
| Full Laya in a Rust/native guest runtime | Encoder + typed head + tokenizer + loading + operator/backend support | Full-model inference inside Hermit if parity passes |
| Exported Laya with a native inference engine | Export fidelity, engine platform adaptation, tokenizer, allocator/thread support | Equivalent exported model if numerical/decision parity passes |
| Small student policy trained from Laya/action data | Distillation, small runtime, separate evaluation | A learned controller influenced by Laya; not the original Laya checkpoint |

Do not assume a Rust API makes an engine freestanding. A Rust binding to a C/C++ runtime may still require a platform port. A mostly Rust implementation may depend on file mapping, threading, target intrinsics, dynamic libraries, or OS facilities absent from the selected Hermit configuration.

## Full-model portability spike

Time-box the first study to approximately two weeks after S4; this is discovery, not a delivery estimate for the port. Produce an operator/dependency inventory and one reproducible feasibility result before committing to a larger implementation.

1. Freeze the exact selected checkpoint and produce a small golden corpus of tokenized inputs, raw logits/probabilities, chosen labels, and calibrated outputs on the host.
2. Inventory the complete model graph, including the decision head and type/marker handling, not just the base encoder. Record shapes, dtypes, masking, attention, normalization, and calibration operations.
3. Evaluate the upstream ONNX export path on Linux first and measure decision parity on the golden corpus and held-out policy data. Export availability is established upstream, but Hermit support is not. [Pinned export script](https://github.com/NandhaKishorM/laya/blob/8a6e1328cce2460a0e5aa348ad465bb1b5821cd2/scripts/export_onnx.py).
4. Choose a CPU inference engine only after its required operations and target dependencies are audited. Compare a Rust-native path and an adapted native engine; do not decide from language branding alone.
5. Compile a tiny operator test for Hermit, then a tokenizer test, then one full frozen inference. Test the actual executable in QEMU.
6. Measure memory high-water mark, boot/model-load time, inference p99, code size, and interference with workload progress.

If any required operator, tokenizer behavior, threading primitive, or allocation path is missing, document the specific work instead of hiding it behind a “load model” placeholder.

## Loading and memory

Plan a larger guest for a full hundreds-of-millions-parameter model; 4–8 GiB is a starting investigation range, not a validated requirement. Budget weights, activations, tokenizer tables, runtime scratch space, application arenas, stacks, device buffers, and fallback reservations separately. Measure peak allocation during loading, which can exceed steady-state memory.

Choose a tested read-only model delivery path: embedded asset, an explicitly supported initramfs/filesystem path, or a supported block device. Do not overwrite the loader's use of QEMU `-initrd` for the guest ELF by casually putting model weights in the same argument. Additional artifacts require an agreed loader/filesystem mechanism.

Preallocate predictable inference workspaces where feasible. Model allocation failure must disable inference and preserve guest fallback; if the runtime uses infallible allocations that can abort the whole image, that is a material limitation to resolve before calling the design fault-tolerant.

## Scheduling and failure containment

Give inference its own bounded workload class after the initial scheduler supports it; adding a fourth class changes the catalog, fairness accounting, and evaluation. It cannot use the protected system reservation as unlimited compute. Prevent inference from holding kernel locks or monopolizing execution while interrupts are disabled.

A CPU-hungry forward pass can often be preempted as ordinary task work, but an allocator failure, panic, unsafe runtime bug, or malformed weight parser can still crash a shared-address-space unikernel. Reproduce the external design's failure tests and report the reduced isolation honestly. A companion VM may remain the preferable deployment if fault containment is a priority.

## Quantization and distillation

Treat quantization as a new model/backend artifact with fresh parity, calibration, and closed-loop evaluation. Smaller weights do not guarantee that the runtime's total memory or worst-case latency fits. Reject conversions that preserve a few demo labels but degrade policy regret or rare pressure cases.

Distillation may be more practical than carrying the full model. Train a compact classifier or other bounded controller from measured action outcomes and, if useful, Laya labels. Compare it with the simple non-ML baseline and full Laya. If the student wins, describe the result as an in-kernel learned policy derived from the earlier research, not full Laya running inside the guest.

## S5 acceptance criteria

- The image boots and runs its declared workload with host inference/network access disabled.
- A recorded local inference chooses a profile and the kernel activates it through the same validated boundary.
- The complete encoder/head/tokenizer/calibration behavior passes a predefined parity and policy-quality budget.
- Model memory, inference CPU service, and loading behavior fit declared limits.
- Deliberate local inference failure preserves fallback to the extent claimed; whole-guest crash cases are explicitly reported.
- QEMU artifacts and the experiment manifest reproduce the result independently.

Only after these pass should the team describe the system as a self-contained AI-managed Rust unikernel. This milestone is optional for proving the initial paired architecture and requires its own schedule and resourcing decision.

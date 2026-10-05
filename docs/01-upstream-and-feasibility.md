# Upstream evidence and feasibility

Researched 2026-10-05. Links to commits identify the inspected code, not a compatibility certification. Implementation gate G0 must produce a successfully built and booted combination.

## Hermit foundation

Use the current [Hermit organization](https://github.com/hermit-os): `kernel` supplies the operating-system library, `hermit-rs` supplies application integration, and `loader` boots images on platforms including QEMU. Preserve RustyHermit lineage through these components rather than copying an unrelated kernel tutorial. The official [application template](https://github.com/hermit-os/hermit-rs-template) is the scaffold starting point.

| Component | Inspected revision | Use |
| --- | --- | --- |
| `hermit-os/hermit-rs-template` | `da0826ec435a6cebc7133ebb0d1f3d8bc92fffdb` | Application shape; template currently references `hermit-0.13.0` |
| `hermit-os/hermit-rs` | `3e7dfe5048448108a3c028643e22c8358657961e` | Candidate development integration; kernel submodule below |
| Kernel paired with that integration | `7f7dcf70a7739f00c2e998ec7c317d65bba38ee2` | Candidate fork base and source inspection reference |
| `hermit-os/kernel` main observed separately | `4fccbbace86ce16c3a38b77ba83d6759d872e3ae` | Inventory only; do not substitute automatically |
| `hermit-os/loader` | `3ec52a9c585e66c0a8c7d5841293b3be28bae462` | Candidate Multiboot loader source |

Sources: [template tree](https://github.com/hermit-os/hermit-rs-template/tree/da0826ec435a6cebc7133ebb0d1f3d8bc92fffdb), [application integration tree](https://github.com/hermit-os/hermit-rs/tree/3e7dfe5048448108a3c028643e22c8358657961e), [paired kernel](https://github.com/hermit-os/kernel/tree/7f7dcf70a7739f00c2e998ec7c317d65bba38ee2), [loader](https://github.com/hermit-os/loader/tree/3ec52a9c585e66c0a8c7d5841293b3be28bae462).

There are two build lanes to keep distinct. The template declares Rust `1.94.0` and an installable Hermit standard library. The inspected development integration declares `nightly-2026-08-01`; its paired kernel declares `nightly-2026-09-01`. Do not combine these into one imaginary universal toolchain. First boot the untouched template with its documented components, then validate the development pair independently. Sources: [template toolchain](https://github.com/hermit-os/hermit-rs-template/blob/da0826ec435a6cebc7133ebb0d1f3d8bc92fffdb/rust-toolchain.toml), [integration toolchain](https://github.com/hermit-os/hermit-rs/blob/3e7dfe5048448108a3c028643e22c8358657961e/rust-toolchain.toml), [kernel toolchain](https://github.com/hermit-os/kernel/blob/7f7dcf70a7739f00c2e998ec7c317d65bba38ee2/rust-toolchain.toml).

The integration build script accepts `HERMIT_MANIFEST_DIR` to select kernel source and forwards feature flags. This is a concrete way to link a maintained kernel fork. Kernel modifications are therefore necessary but a replacement bootloader and driver stack are not. [Inspected build script](https://github.com/hermit-os/hermit-rs/blob/3e7dfe5048448108a3c028643e22c8358657961e/hermit/build.rs).

## Kernel observations that affect the design

The paired source has a per-core scheduler, ready queues indexed by priority, and paths that select equal-or-higher-priority work. Its priority update path is not a ready-made fair-share policy API: the remote-core case warns, and the local path assumes a suitable ready task when it is not updating the running task. Our design therefore introduces group scheduling and transactional policy application rather than issuing arbitrary priority changes. [Scheduler source](https://github.com/hermit-os/kernel/blob/7f7dcf70a7739f00c2e998ec7c317d65bba38ee2/src/scheduler/mod.rs), [task queues](https://github.com/hermit-os/kernel/blob/7f7dcf70a7739f00c2e998ec7c317d65bba38ee2/src/scheduler/task/mod.rs).

The ordinary unikernel is treated as one trust domain. The inspected kernel marks its `common-os` multiple-address-space feature incomplete; that feature is not a basis for claiming process isolation in this plan. Enable explicit loader/network features for the prototype and audit the resulting dependency graph. [Kernel feature definitions](https://github.com/hermit-os/kernel/blob/7f7dcf70a7739f00c2e998ec7c317d65bba38ee2/Cargo.toml).

## Laya identity and capabilities

Use [NandhaKishorM/laya](https://github.com/NandhaKishorM/laya) as the assumed upstream. The discovered [Dancing-coin/l-aya](https://github.com/Dancing-coin/l-aya) is a fork. Candidate SDK source: `8a6e1328cce2460a0e5aa348ad465bb1b5821cd2`. Candidate weights repository: `convaiinnovations/laya`, revision `7b928d828b7b0e022f929d9bd2e44165aa270148`. Pin code and weights separately.

Laya supplies typed `choice`, `score`, and `noul` outputs. Its published model family includes English, multilingual, and typed-decisions variants. The published English configuration is much larger than a small hand-coded controller; the upstream card lists 421M parameters. No inspected material establishes kernel-policy competence. [Model card](https://huggingface.co/convaiinnovations/laya/tree/7b928d828b7b0e022f929d9bd2e44165aa270148).

For choice responses, use the selected label and `answer_confidence`; the legacy `confidence` field has different semantics. The source explicitly ties reliable confidence interpretation to held-out calibration. Treat raw confidence as an uncertain model output. [Agent output code](https://github.com/NandhaKishorM/laya/blob/8a6e1328cce2460a0e5aa348ad465bb1b5821cd2/laya/agent.py), [confidence calculations](https://github.com/NandhaKishorM/laya/blob/8a6e1328cce2460a0e5aa348ad465bb1b5821cd2/laya/common.py).

Upstream exposes an ONNX export path. That is relevant to a later portability experiment, but it is not evidence that ONNX Runtime, the tokenizer, or every required operator runs on Hermit. [Export implementation](https://github.com/NandhaKishorM/laya/blob/8a6e1328cce2460a0e5aa348ad465bb1b5821cd2/scripts/export_onnx.py).

## What the evidence supports

| Statement | Status |
| --- | --- |
| Hermit is an appropriate Rust unikernel starting point | Supported by upstream architecture and template |
| Hermit has a documented QEMU loader path | Supported by loader documentation |
| A fork can add resource-policy mechanisms | Architectural proposal supported by available source |
| Laya can select a typed label from a catalog | Supported by SDK |
| Laya can select good OS policies from telemetry | Unproven; must evaluate |
| Published GPU latency will hold end to end on this host | Unproven; must measure |
| A Python/PyTorch application can simply be linked into Hermit | Not established; not assumed |
| Rust alone guarantees liveness, safe DMA, or useful AI decisions | False assumption; requires separate mechanisms and tests |

## Lockfile and provenance requirements

The team must generate a run manifest containing full source SHAs, dirty patch hashes, both Rust toolchains, all Cargo lockfiles, selected features, standard-library artifact checksum, loader checksum, QEMU version/machine/CPU, Python dependency lock, model/tokenizer/config hashes, calibration hash, policy catalog hash, and workload seed.

Confirm licenses from each pinned repository and model artifact before redistribution; preserve required notices. The inspected projects advertise permissive licenses, but a license label is not a completed dependency audit. Store source and weight provenance separately.

The untouched template pin has since booted with Rust 1.94.0 and loader release v0.5.6. That release binary is not the loader source pin above. The development integration, paired kernel, and loader source pin remain unbooted. No model weights have been installed.

# Repository layout and Hermit scaffold

## Current versus proposed contents

The delivered repository contains Markdown planning documents only. The tree below is the **implementation target**. Paths, crate names, and scripts in this document are proposed interfaces; they do not already exist.

```text
ai-as-a-kernel/
  README.md
  docs/
  Cargo.toml                         Rust workspace
  Cargo.lock
  rust-toolchain.toml                 Application toolchain pin
  vendor/
    hermit-rs/                       Pinned integration source
      kernel/                       Its pinned kernel submodule, with our patches
    loader/                          Pinned upstream loader
  crates/
    policy-types/                    no_std fixed-size types and catalog identifiers
    policy-core/                     no_std validation/lease/catalog logic
    policy-wire/                     Bounded codec; shared host test fixtures
    hermit-policy-abi/               Narrow FFI declarations and safe guest wrappers
    workloads/                      Portable benchmark logic
  apps/
    policy-guest/                    Hermit-linked bridge and workloads
    host-workload/                   Optional external load generator
  controller/
    pyproject.toml
    dependency lockfile
    src/aik_controller/             Server, Laya adapter, feature encoder
    tests/
  configs/
    catalog-cpu-v1.json
    catalog-joint-v1.json
    workloads/
    experiments/
  tools/
    doctor.sh
    build-guest.sh
    build-loader.sh
    run-qemu.sh
    run-matrix.py
    collect-manifest.py
  tests/
    protocol-vectors/
    policy-model/
    qemu/
  artifacts/                        Ignored outputs, manifests, logs, reports
```

Maintain one canonical source for catalog values. Generate or verify Rust constants and controller metadata against its content hash. Do not hand-maintain two disagreeing policy tables.

## Upstream strategy

Start by reproducing the official template in an isolated bootstrap directory. Keep its origin and licenses. Then use the inspected `hermit-rs` development pair as the candidate for kernel work, or deliberately choose a tested release pair if development revisions fail G0. Record the chosen pair; do not independently update kernel HEAD.

Recommended ownership is an application repository plus a small maintained kernel fork. The `vendor/hermit-rs/kernel` submodule should reference a commit in that fork when patches are committed. A submodule checkout with unrecorded local edits is insufficient for a team handoff. Record both parent and nested submodule revisions and any dirty patch hash.

Keep changes as reviewable patches: counters; group metadata; deterministic scheduling; mailbox/leases; transport; model activation; managed memory/admission. Feature-gate experimental scheduling so the upstream baseline remains available.

## Build lanes

### Lane A: prove the official template

The [template instructions](https://github.com/hermit-os/hermit-rs-template/tree/da0826ec435a6cebc7133ebb0d1f3d8bc92fffdb) use the matching Hermit standard-library distribution for their declared stable compiler. Acquire the exact `rust-std-hermit` artifact, record its checksum, and install it into that matching rustup toolchain following the artifact instructions. A moving `stable` alias is not an adequate version pin. [Standard-library distribution](https://github.com/hermit-os/rust-std-hermit).

Build the untouched example, pair it with a pinned Multiboot loader, and capture serial output plus exit status. If the target artifact or dependency graph is unavailable, document the failure and move to a supported pinned lane; do not assume `rustup target add x86_64-unknown-hermit` supplies everything.

### Lane B: develop the modified kernel

For the inspected candidate, use application nightly `nightly-2026-08-01` with `rust-src`, while allowing the kernel's build machinery to use its declared `nightly-2026-09-01`. The integration script intentionally invokes a separate kernel build. [Build script](https://github.com/hermit-os/hermit-rs/blob/3e7dfe5048448108a3c028643e22c8358657961e/hermit/build.rs).

Proposed application manifest fragment:

```toml
[target.'cfg(target_os = "hermit")'.dependencies]
hermit = { path = "../../vendor/hermit-rs/hermit", default-features = false, features = [
  "loader", "acpi", "pci", "fsgsbase", "kernel-stack", "tcp", "virtio-net"
] }
```

Retain the template's conditional linkage in the application entry point:

```rust
#[cfg(target_os = "hermit")]
use hermit as _;
```

Add a project `ai-policy` feature to the wrapper and kernel when policy patches land; the wrapper forwards features, so both sides must recognize it. Include `alloc-stats` only after verifying its logging overhead and applicability. Leave SMP and DHCP disabled in the first static-address, one-vCPU experiment.

Candidate build command **after the workspace and compatible lockfiles have been created**:

```bash
HERMIT_MANIFEST_DIR="$PWD/vendor/hermit-rs/kernel" \
HERMIT_IP=10.0.2.15 \
HERMIT_GATEWAY=10.0.2.2 \
HERMIT_MASK=255.255.255.0 \
cargo +nightly-2026-08-01 build \
  --locked -Zbuild-std=std,panic_abort \
  --target x86_64-unknown-hermit --release -p policy-guest
```

This command is a G0 validation candidate, not a command executed during planning. The team must test the chosen nightly/standard-library/ABI pairing. The normal `--locked` argument on the outer build does not prove every nested Cargo invocation is locked: audit the kernel `xtask` invocations, commit their locks, and enforce locked/offline dependency resolution in the reproducibility wrapper after dependencies are fetched.

Do not copy a prebuilt `libhermit.a` into the app and lose source provenance. Make the linked guest report the kernel fork SHA and catalog hash so a stale or accidentally unmodified build is detectable.

## Kernel/application ABI

The application bridge must not reach into private scheduler data structures. Add a small C-compatible interface with explicit version/size fields, fixed-width integers, bounded arrays, and result codes. Proposed calls:

| Call | Context | Result |
| --- | --- | --- |
| Register current workload class | Trusted startup thread before registration closes | Class registration or explicit refusal |
| Read telemetry snapshot | Normal thread | Bounded copy with generation/time metadata |
| Stage decoded proposal | Bridge thread | Staged or enumerated rejection |
| Read active policy/status | Normal thread | Authoritative profile, generation, lease, override |
| Drain acknowledgment records | Bridge thread | Bounded batch plus dropped-record count |

Avoid Rust references, `Vec`, `String`, trait objects, or unwinding across the ABI. Validate lengths and pointers in the wrapper/kernel according to the actual trust model; same-address-space bounds checking does not create process isolation. Keep unsafe blocks small and documented.

Decide how shared `no_std` crates participate in Hermit's separate kernel build and symbol mangling. Host/application compilation and kernel compilation may instantiate distinct copies of types/logic; never depend on sharing a Rust global by symbol accident. Export only the explicit ABI. Verify symbol presence and layouts with a tiny integration fixture before building the whole controller.

## Tooling contracts

`doctor.sh` reports required commands, compiler versions, available QEMU accelerators, artifact checksums, and KVM access without changing system settings. Missing QEMU is an actionable setup result, not a reason to silently choose another hypervisor.

`build-guest.sh` uses exact profiles and feature sets, checks the selected kernel path, and writes a manifest. `run-qemu.sh` selects an explicit TCG or KVM lane and refuses an invalid combination. It must capture exit status correctly, kill only its own timed-out process, and retain logs. KVM benchmark mode must not silently fall back to TCG.

`run-matrix.py` starts the controller, waits for model readiness when required, launches a fresh guest for each trial, applies timeouts, and checks guest-produced assertions. Mock, shadow, and live modes have visibly different run IDs. No script downloads changing dependencies during a measured trial.

## CI layers

Run portable policy logic and protocol tests on an ordinary Linux runner. Run QEMU/TCG boot and fault smoke tests on a runner that has QEMU. Run KVM timing and live-model qualification on a controlled machine with recorded hardware. A missing GPU may skip a GPU lane, but it must never turn a required CPU/QEMU functional gate into a green pass.

Each pull request should state its effect on the policy/mechanism boundary and include the smallest relevant evidence. Changes to a catalog, telemetry schema, model preprocessing, or checkpoint trigger appropriate requalification even if the Rust code is unchanged.

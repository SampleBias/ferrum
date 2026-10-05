# QEMU development and test environment

## Status and host requirements

This host is an AMD Ryzen 5 1600 with AMD-V. QEMU 11.1.1 is installed, `kvm_amd` is loaded, and `/dev/kvm` is accessible (`crw-rw-rw-`). The untouched template has booted; the record is `bootstrap/lane-a/g0-manifest.txt`. The rest of this page remains the lab runbook.

Use a Linux x86-64 development host with enough RAM for the guest, compiler, and separate model service. Begin with TCG emulation for functional tests. Use KVM on a machine with accessible virtualization support for performance evaluation. The host may run all components, but isolate inference and QEMU CPU resources when comparing latency.

Required software: QEMU system x86 with user-network support, Rust/rustup and the selected components, Git, the pinned Hermit loader, Python and a locked environment, and ordinary build/link tools required by those pins. GDB and ELF inspection tools support debugging. Record package versions; don't replace the machine's default Rust toolchain globally just for this project.

Illustrative package installation choices, to be executed by the team on a suitable host:

```bash
# Debian/Ubuntu-style host; verify package names for the chosen release.
sudo apt-get install qemu-system-x86 qemu-utils gdb git build-essential pkg-config python3-venv

# Arch-style host; choose this alternative only on that distribution.
sudo pacman -S --needed qemu-system-x86 qemu-img gdb git base-devel python
```

Package installation is environment setup, not evidence that Hermit can boot. On KVM hosts, verify access through the site's normal group/device permissions; do not make `/dev/kvm` world-writable. TCG remains the explicit functional lane if KVM is unavailable.

## Build and identify artifacts

Complete [the build-lane procedure](06-repository-and-scaffold.md) first. Build the loader in its pinned checkout with its own declared toolchain:

```bash
cargo xtask build --target x86_64-multiboot --release
```

The upstream loader documents this build target and places output under `target/release`. Verify the exact artifact name at the selected pin and store its absolute path and hash in the manifest. The application is the Hermit-linked ELF, not a Linux ELF. [Loader build and boot documentation](https://github.com/hermit-os/loader/blob/3ec52a9c585e66c0a8c7d5841293b3be28bae462/README.md).

Before running, inspect both files with `file`/`readelf`, verify SHA-256 hashes, and compare the source IDs the guest will print. Do not assume the loader and application are interchangeable arguments: QEMU receives the loader through `-kernel` and the guest ELF through `-initrd`.

## TCG boot smoke test

Set the two variables to real absolute paths in the implementation workspace. The checks intentionally stop if the artifacts have not yet been built.

```bash
: "${AIK_LOADER:?Set to the verified Hermit Multiboot loader path}"
: "${AIK_GUEST:?Set to the built Hermit application ELF path}"
test -f "$AIK_LOADER"
test -f "$AIK_GUEST"

qemu-system-x86_64 \
  -machine pc -accel tcg \
  -cpu qemu64,apic,fsgsbase,fxsr,rdrand,rdtscp,xsave,xsaveopt \
  -smp 1 -m 512M \
  -display none -serial stdio -monitor none \
  -no-reboot \
  -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
  -kernel "$AIK_LOADER" \
  -initrd "$AIK_GUEST"
```

This derives from the official template's CPU/boot shape, with an explicit accelerator and larger experiment memory. Qualify the CPU feature set against the installed QEMU build. Freeze a versioned `pc-i440fx-*` machine type from that QEMU's `-machine help` once G0 passes; `pc` is only the initial discovery alias. [Template boot example](https://github.com/hermit-os/hermit-rs-template/blob/da0826ec435a6cebc7133ebb0d1f3d8bc92fffdb/README.md).

On this host, loader release v0.5.7 (`hermit-loader-x86_64-multiboot`) panics Hermit 0.13.0 with `ParentEntryHugePage` while mapping the 4 KiB SMP trampoline at `0x8000`. Release v0.5.6 (`hermit-loader-x86_64`) leaves that page as a 4 KiB mapping and reaches `Hello, world!` with raw exit status 3 under both TCG and KVM at 512 MiB. Do not substitute the newer multiboot asset until the kernel can split that huge page.

The G0 guest must print a start marker, its build identities, and an explicit test-complete marker, then exit. Capture serial output and host exit status. At the inspected paired kernel revision, successful shutdown writes a value that makes `isa-debug-exit` return host status **3**; failure returns **1**. Verify this with success/failure smoke images at the final pin. Do not hardcode the common unrelated OS tutorial status of 33 or expect a successful guest to exit QEMU with 0. [Hermit shutdown implementation](https://github.com/hermit-os/kernel/blob/7f7dcf70a7739f00c2e998ec7c317d65bba38ee2/src/arch/x86_64/kernel/processor.rs).

The harness must fail on a timeout, panic, missing completion marker, incompatible source ID, or unexpected status. Avoid `set -e` discarding the intended nonzero success status before it can be interpreted. Retain the raw status even if the harness normalizes it for CI.

## Guest/controller networking

Compile the guest with static IPv4 `10.0.2.15`, mask `255.255.255.0`, gateway `10.0.2.2`, and the `tcp`/`virtio-net`/PCI features selected in the scaffold. Omit DHCP in this lane so static settings are unambiguous. Start the project controller on host loopback port 7777 before the connectivity test.

Add these QEMU arguments to the boot command:

```bash
-netdev user,id=net0,net=10.0.2.0/24,host=10.0.2.2,ipv6=off \
-device virtio-net-pci,netdev=net0,disable-legacy=on
```

The guest connects outward to `10.0.2.2:7777`. `127.0.0.1` inside the guest refers to the guest. QEMU documents the default guest network and host address; test host-loopback reachability on the chosen QEMU/libslirp build. `hostfwd` is for incoming host-to-guest connections, not this guest-initiated controller connection. [QEMU networking options](https://www.qemu.org/docs/master/system/invocation.html#network-options).

If an external host load generator drives a guest service listening on port 8080, add a loopback-only forward to the same `-netdev` option:

```text
hostfwd=tcp:127.0.0.1:18080-10.0.2.15:8080
```

Do not set `restrict=on` and expect unrestricted access to the host controller: QEMU documents that it blocks that traffic unless an explicit forwarding path is configured. For a later isolated setup, validate `guestfwd` or a dedicated host-only TAP network and document its exact firewall policy. The initial user network needs no TAP privileges, but it also is not a proof of total guest network isolation.

First test a bounded echo/mock service, then the real protocol, then Laya. An inference error must not be confused with an unconfigured NIC. If containers are used, publish the controller port on host loopback and test through the same path; do not assume container-local `127.0.0.1` equals host loopback.

## Proposed guest options

The future guest CLI should accept these application arguments, separated from kernel arguments by `--`:

```text
-append "-- --mode=shadow --controller=10.0.2.2:7777 --boot-id=<fresh-128-bit-hex> --workload=mixed-v1 --seed=42 --duration-s=120"
```

These are proposed application flags, not existing Hermit kernel flags. The harness creates a new boot ID on every launch. The pre-shared key is provisioned separately, never passed here. Use distinct modes `baseline`, `heuristic`, `mock`, `shadow`, and `live` in logs and manifests.

## KVM performance lane

Use the same guest, memory size, virtual devices, and workload definitions, replacing the TCG accelerator/CPU arguments with:

```bash
-accel kvm -cpu host
```

Confirm KVM actually initialized. Record host CPU model, topology, kernel, QEMU version, CPU affinity, governor/turbo policy, inference device, and concurrent host load. If measuring with invariant TSC, validate host support and use the loader's documented benchmarking configuration, then keep it fixed across comparisons. Never use `-cpu host` as the TCG configuration.

TCG demonstrates functionality and supports fault reproduction. It does not substantiate native performance or hardware interrupt latency. Do not combine TCG and KVM samples into one confidence interval.

## Debugging

Add `-S -gdb tcp:127.0.0.1:1234` to a local diagnostic run, retaining serial output. Use `rust-gdb`, connect with `target remote 127.0.0.1:1234`, and load application symbols with the actual relocation/load address printed by the loader. The loader documents `symbol-file -o <START> <IMAGE_PATH>` for this purpose. [Loader debugging guidance](https://github.com/hermit-os/loader/blob/3ec52a9c585e66c0a8c7d5841293b3be28bae462/README.md), [QEMU GDB documentation](https://www.qemu.org/docs/master/system/gdb.html).

Set breakpoints at boot, policy activation, expiry, and scheduler selection. Keep debug ports on loopback. Debug builds and instruction tracing are diagnostic conditions, not performance samples.

## Failure and experiment artifacts

Every trial directory must contain a manifest, serial log, controller log, decisions, guest activation/rejection records, workload measurements, host measurements, raw exit status, and a pass/fail explanation. A missing decision log or dropped critical trace record invalidates a causal proof even when the guest exits successfully.

Fault cases include no controller at boot, controller killed mid-run, delayed responses, malformed frames, saturated workload pools, a non-yielding worker, and stale replies after reconnect. The harness needs ownership of its own VM/controller process IDs to stop them on timeout without affecting unrelated processes.

## First-day checklist

- [ ] Install/verify QEMU and selected compiler components on the development host.
- [ ] Boot untouched upstream-derived hello world in TCG.
- [ ] Confirm success/failure QEMU exit decoding and timeout behavior.
- [ ] Print exact image/kernel/loader identities in recorded artifacts.
- [ ] Reach the host mock controller through virtio-net.
- [ ] Run the guest with no controller and observe independent progress.
- [ ] On a suitable host, reproduce the baseline in KVM as a separate lane.

Do not begin model-quality claims before these checks pass.

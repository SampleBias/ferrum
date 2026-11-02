#!/usr/bin/env bash
# Policy guest for the booted Hermit 0.13.0 tree in vendor/hermit-rs.
# Hermit is a Tier 3 target without a distributed rust-std component. The
# pinned nightly builds std for the guest and the kernel's own toolchain file
# selects the same nightly for its kernel build.
set -eu

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
# QEMU user-net is 10.0.2.0/24 with the host at 10.0.2.2. Hermit's unset
# address is 10.0.5.3, and a guest built that way cannot reach the controller.
export HERMIT_IP=10.0.2.15
export HERMIT_GATEWAY=10.0.2.2
export HERMIT_MASK=255.255.255.0
export CARGO_TARGET_DIR="$root/target"
exec cargo +nightly-2026-02-01 -Z build-std=std,panic_abort build -p policy-guest --target x86_64-unknown-hermit "$@"

#!/usr/bin/env bash
# Policy guest for the booted Hermit 0.13.0 tree in vendor/hermit-rs.
# Rust 1.94.0 builds the guest. The kernel's own toolchain file selects
# nightly-2026-02-01. The ai-policy feature is enabled by the guest manifest.
set -eu

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
# QEMU user-net is 10.0.2.0/24 with the host at 10.0.2.2. Hermit's unset
# address is 10.0.5.3, and a guest built that way cannot reach the controller.
export HERMIT_IP=10.0.2.15
export HERMIT_GATEWAY=10.0.2.2
export HERMIT_MASK=255.255.255.0
exec cargo +1.94.0 build -p policy-guest --target x86_64-unknown-hermit "$@"

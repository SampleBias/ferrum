#!/usr/bin/env bash
# Build the standalone Air Biological Sampling Sensor embedded unikernel image
# (`deployments/air-biosensor`) on top of vendor/hermit-rs and hermit-policy-abi.
set -eu

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
export HERMIT_IP=10.0.2.15
export HERMIT_GATEWAY=10.0.2.2
export HERMIT_MASK=255.255.255.0
export CARGO_TARGET_DIR="$root/target"
exec cargo +nightly-2026-02-01 -Z build-std=std,panic_abort build -p air-biosensor --target x86_64-unknown-hermit "$@"

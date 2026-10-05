#!/usr/bin/env bash
# Policy guest for the booted Hermit 0.13.0 tree in vendor/hermit-rs.
# Rust 1.94.0 builds the guest. The kernel's own toolchain file selects
# nightly-2026-02-01. The ai-policy feature is enabled by the guest manifest.
set -eu

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
exec cargo +1.94.0 build -p policy-guest --target x86_64-unknown-hermit "$@"

#!/usr/bin/env bash
# Lane A policy guest. Uses the published hermit-0.13.0 crate and Rust 1.94.0.
# The kernel fork in vendor/ is still required for scheduler changes; this
# image only carries the protocol bridge.
set -eu

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
exec cargo +1.94.0 build -p policy-guest --target x86_64-unknown-hermit "$@"

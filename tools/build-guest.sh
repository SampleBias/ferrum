#!/usr/bin/env bash
# Lane B guest build. The kernel fork is not vendored yet, so this refuses
# to link a prebuilt libhermit.a or to pretend a host binary is the guest.
set -u

root="$(cd "$(dirname "$0")/.." && pwd)"
kernel="${HERMIT_MANIFEST_DIR:-$root/vendor/hermit-rs/kernel}"

if [[ ! -f "$kernel/Cargo.toml" ]]; then
  echo "kernel manifest not found at ${kernel}" >&2
  echo "G0 still needs a pinned Hermit checkout. This script will not download one during a measured run." >&2
  exit 2
fi

echo "guest build is not wired until the pinned kernel fork is present" >&2
exit 2

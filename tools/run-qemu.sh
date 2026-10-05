#!/usr/bin/env bash
# Boot a Hermit loader and guest under an explicit accelerator.
# A successful Hermit shutdown is raw QEMU status 3, not 0.
# KVM is never rewritten to TCG.
set -u

accel="${AIK_ACCEL:-tcg}"
timeout_s="${AIK_TIMEOUT_S:-30}"
serial_path="${AIK_SERIAL:-}"

if [[ "$accel" != "tcg" && "$accel" != "kvm" ]]; then
  echo "refusing accelerator '${accel}'; use tcg or kvm" >&2
  exit 2
fi

if ! command -v qemu-system-x86_64 >/dev/null 2>&1; then
  echo "qemu-system-x86_64 is not installed" >&2
  exit 2
fi

if [[ "$accel" == "kvm" ]]; then
  if [[ ! -r /dev/kvm || ! -w /dev/kvm ]]; then
    echo "KVM was requested but /dev/kvm is not accessible; not falling back to TCG" >&2
    exit 2
  fi
  cpu_arg=(-cpu host)
  accel_arg=(-accel kvm)
else
  cpu_arg=(-cpu qemu64,apic,fsgsbase,fxsr,rdrand,rdtscp,xsave,xsaveopt)
  accel_arg=(-accel tcg)
fi

if [[ -z "${AIK_LOADER:-}" || -z "${AIK_GUEST:-}" ]]; then
  echo "Set AIK_LOADER and AIK_GUEST to built artifact paths before booting" >&2
  exit 2
fi
if [[ ! -f "$AIK_LOADER" || ! -f "$AIK_GUEST" ]]; then
  echo "loader or guest file is missing" >&2
  exit 2
fi

serial_arg=(-serial stdio)
if [[ -n "$serial_path" ]]; then
  mkdir -p "$(dirname "$serial_path")"
  serial_arg=(-serial "file:${serial_path}")
fi

net_arg=()
if [[ "${AIK_NET:-0}" == "1" ]]; then
  net_arg=(
    -nic none
    -netdev user,id=net0,net=10.0.2.0/24,host=10.0.2.2,ipv6=off
    -device virtio-net-pci,netdev=net0,disable-legacy=on
  )
fi

append_arg=()
if [[ -n "${AIK_APPEND:-}" ]]; then
  append_arg=(-append "$AIK_APPEND")
fi

# timeout sends SIGTERM to its direct child. Do not wrap QEMU in another shell.
timeout --foreground "$timeout_s" qemu-system-x86_64 \
  -machine pc "${accel_arg[@]}" \
  "${cpu_arg[@]}" \
  -smp 1 -m 512M \
  -display none "${serial_arg[@]}" -monitor none \
  -no-reboot \
  -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
  "${net_arg[@]}" \
  "${append_arg[@]}" \
  -kernel "$AIK_LOADER" \
  -initrd "$AIK_GUEST"
raw_status=$?

echo "raw_exit=${raw_status} accel=${accel}"
if [[ "$raw_status" -eq 124 ]]; then
  echo "qemu timed out after ${timeout_s}s" >&2
fi
exit "$raw_status"

#!/usr/bin/env bash
# Report the host lane. This script does not change system settings.
set -u

require_qemu=0
require_kvm=0
probe_tcg=0
for arg in "$@"; do
  case "$arg" in
    --require-qemu) require_qemu=1 ;;
    --require-kvm) require_kvm=1 ;;
    --probe-tcg) probe_tcg=1 ;;
    *)
      echo "unknown argument: $arg" >&2
      exit 2
      ;;
  esac
done

status=0
report() {
  local name="$1"
  local state="$2"
  local detail="$3"
  printf '%-22s %-8s %s\n' "$name" "$state" "$detail"
  if [[ "$state" == "missing" || "$state" == "fail" ]]; then
    status=1
  fi
}

if command -v rustc >/dev/null 2>&1; then
  report rustc ok "$(rustc --version)"
else
  report rustc missing "install a Rust toolchain before host tests"
fi

if command -v cargo >/dev/null 2>&1; then
  report cargo ok "$(cargo --version)"
else
  report cargo missing "install cargo"
fi

if command -v python3 >/dev/null 2>&1; then
  report python3 ok "$(python3 --version)"
else
  report python3 missing "install Python 3"
fi

if command -v qemu-system-x86_64 >/dev/null 2>&1; then
  report qemu-system-x86_64 ok "$(qemu-system-x86_64 --version | head -n 1)"
  machine="$(qemu-system-x86_64 -machine help 2>/dev/null | awk '/^pc / { print $1; exit }')"
  report qemu-machine ok "${machine:-unknown} (discovery alias; freeze pc-i440fx-* after G0)"
else
  report qemu-system-x86_64 missing "install qemu-system-x86; do not substitute another hypervisor"
fi

if [[ -r /proc/cpuinfo ]]; then
  model="$(awk -F: '/model name/ { gsub(/^ +/, "", $2); print $2; exit }' /proc/cpuinfo)"
  report cpu ok "${model:-unknown}"
fi

if [[ -e /dev/kvm ]]; then
  mode="$(stat -c '%A' /dev/kvm 2>/dev/null || echo unknown)"
  if [[ -r /dev/kvm && -w /dev/kvm ]]; then
    report kvm ok "/dev/kvm ${mode}"
  else
    report kvm fail "/dev/kvm ${mode} is not readable and writable by this user"
  fi
else
  report kvm absent "no /dev/kvm; TCG remains the functional lane"
fi

if [[ -r /proc/modules ]] && grep -q '^kvm ' /proc/modules; then
  mods="$(awk '/^kvm/ { printf "%s ", $1 }' /proc/modules)"
  report kvm-module ok "${mods}"
else
  report kvm-module absent "kvm module is not loaded"
fi

if [[ "$probe_tcg" -eq 1 ]]; then
  if ! command -v qemu-system-x86_64 >/dev/null 2>&1; then
    report tcg-probe fail "qemu is not installed"
  else
    tmp="$(mktemp)"
    timeout --foreground 2 qemu-system-x86_64 \
      -machine pc -accel tcg \
      -cpu qemu64,apic,fsgsbase,fxsr,rdrand,rdtscp,xsave,xsaveopt \
      -smp 1 -m 64M \
      -display none -serial none -monitor none -no-reboot \
      >"$tmp" 2>&1
    probe_status=$?
    if [[ "$probe_status" -eq 124 ]]; then
      report tcg-probe ok "CPU feature set accepted; process ran until the probe timeout"
    else
      report tcg-probe fail "qemu exited ${probe_status} before the probe timeout"
      sed -n '1,20p' "$tmp" >&2
    fi
    rm -f "$tmp"
  fi
fi

if [[ "$require_qemu" -eq 1 && ! -x "$(command -v qemu-system-x86_64 || true)" ]]; then
  status=1
fi
if [[ "$require_kvm" -eq 1 && ! -r /dev/kvm ]]; then
  status=1
fi

exit "$status"

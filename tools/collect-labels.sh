#!/usr/bin/env bash
# Collect and label branched trials for one split of the current manifest on this host.
#   tools/collect-labels.sh <split> <seed> [<seed> ...]
# Records and labels are named for the host that ran them, so a unit's branches
# stay on one host. Sealed splits are refused by the collector.
set -u

root="$(cd "$(dirname "$0")/.." && pwd)"
split="${1:?usage: collect-labels.sh <split> <seed> [<seed> ...]}"
shift
if [[ $# -eq 0 ]]; then
  echo "name at least one seed of the ${split} split" >&2
  exit 2
fi

host="${AIK_HOST_TAG:-}"
if [[ -z "$host" ]]; then
  model="$(awk -F: '/model name/ { gsub(/^ +/, "", $2); print $2; exit }' /proc/cpuinfo)"
  case "$model" in
    "Intel(R) Core(TM) i7-10750H CPU @ 2.60GHz") host=i7-10750h ;;
    "AMD Ryzen 5 1600 Six-Core Processor") host=r5-1600 ;;
    *)
      echo "no host tag for '${model}'; set AIK_HOST_TAG" >&2
      exit 2
      ;;
  esac
fi

cd "$root/controller"
for seed in "$@"; do
  out="../data/jobs-v2/${split}-seed${seed}-${host}.jsonl"
  if [[ ! -s "$out" ]]; then
    PYTHONPATH=src python3 -m aik_controller.branches collect \
      --split "$split" --seed "$seed" --retries 2 --out "$out" || exit $?
  fi
  # Each pass boots only the planned branches that still lack a usable record.
  for pass in 1 2 3; do
    PYTHONPATH=src python3 -m aik_controller.branches collect \
      --split "$split" --seed "$seed" --retries 2 --fill --out "$out" || exit $?
  done
  PYTHONPATH=src python3 -m aik_controller.branches label --records "$out" \
    --out "../data/jobs-v2/labels-${split}-seed${seed}-${host}.json" || exit $?
done

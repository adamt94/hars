#!/usr/bin/env bash
# Run scripts/perf.sh in a Linux container (the budgets are defined on Linux).
#
#   scripts/perf-docker.sh [warmup_s] [quiet_s]
#
# Builds a release binary inside the container; the cargo cache and target
# dir live in named volumes so reruns are incremental. perf.sh's options are
# passed through: PERF_GATE, PERF_MET, PERF_DETAIL, the MAX_* budgets, and
# PERF_CONFIG=DIR (DIR is mounted read-only; perf.sh copies it).
set -euo pipefail
# Resolved before the cd below, so a relative path means the caller's.
if [ -n "${PERF_CONFIG:-}" ]; then
  [ -d "$PERF_CONFIG" ] || { echo "PERF_CONFIG=$PERF_CONFIG is not a directory" >&2; exit 1; }
  PERF_CONFIG=$(cd "$PERF_CONFIG" && pwd)
fi
cd "$(dirname "$0")/.."

args=(
  -v "$PWD":/src:ro
  -v hars-perf-cargo:/usr/local/cargo/registry
  -v hars-perf-target:/target
  -e CARGO_TARGET_DIR=/target
)
for var in PERF_GATE PERF_MET PERF_DETAIL \
  MAX_BINARY_MIB MAX_START_MS MAX_RSS_MIB MAX_CPU_PCT MAX_WAKEUPS; do
  if [ -n "${!var:-}" ]; then
    args+=(-e "$var=${!var}")
  fi
done
if [ -n "${PERF_CONFIG:-}" ]; then
  args+=(-v "$PERF_CONFIG":/perf-config:ro -e PERF_CONFIG=/perf-config)
fi

exec docker run --rm "${args[@]}" -w /src rust:1 \
  bash -c 'cargo build --release --locked -q && exec scripts/perf.sh /target/release/hars "$1" "$2"' \
  perf-docker "${1:-15}" "${2:-60}"

#!/usr/bin/env bash
# Run scripts/perf.sh in a Linux container (the budgets are defined on Linux).
#
#   scripts/perf-docker.sh [warmup_s] [quiet_s]
#
# Builds a release binary inside the container; the cargo cache and target
# dir live in named volumes so reruns are incremental.
set -euo pipefail
cd "$(dirname "$0")/.."
exec docker run --rm \
  -v "$PWD":/src:ro \
  -v hars-perf-cargo:/usr/local/cargo/registry \
  -v hars-perf-target:/target \
  -e CARGO_TARGET_DIR=/target \
  -e PERF_MET="${PERF_MET:-0}" \
  -e PERF_DETAIL="${PERF_DETAIL:-0}" \
  -e PERF_GATE="${PERF_GATE:-0}" \
  -w /src \
  rust:1 \
  bash -c "cargo build --release --locked -q && scripts/perf.sh /target/release/hars ${1:-15} ${2:-60}"

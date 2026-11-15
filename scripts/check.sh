#!/usr/bin/env bash
# Run what CI's `check` job runs (.github/workflows/ci.yml), in the same
# order, and stop at the first failure.
#
#   scripts/check.sh
#
# CI's `docker` job (image build, smoke test, resource budgets) is not run
# here: see CONTRIBUTING.md for running it locally.
set -euo pipefail
cd "$(dirname "$0")/.."
export RUSTFLAGS="${RUSTFLAGS:--D warnings}"
step() { printf '\n==> %s\n' "$*"; "$@"; }
step cargo fmt --all --check
step cargo clippy --all-targets --locked
step cargo test --workspace --locked
step python3 -m unittest discover -s scripts
printf '\nAll checks passed.\n'

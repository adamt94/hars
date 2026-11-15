# AGENTS.md

hars (homeassistant-rust) is a Rust rebuild of Home Assistant's core. **The
#1 goal is minimal resource use** (RAM, CPU, allocations, idle wakeups,
binary size) while behaving exactly like the pinned Home Assistant release.

Read before changing code: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
(crates, runtime, where code goes), [docs/PERF.md](docs/PERF.md) (budgets,
rules, evidence), [CONTRIBUTING.md](CONTRIBUTING.md) (tests, PRs). Pick work
from [docs/ROADMAP.md](docs/ROADMAP.md).

## Commands

- All CI checks: `scripts/check.sh` (fmt check, clippy `-D warnings`
  `--locked`, workspace tests `--locked`, Python script tests)
- Format: `cargo fmt --all`
- One crate's tests: `cargo test -p ha-integrations met`
- Run: `HARS_CONFIG=./config cargo run --release` (binds `0.0.0.0:8123`;
  `HARS_BIND` overrides); first `... -- auth add USER PASS`
- Resource budgets: `PERF_GATE=1 scripts/perf.sh target/release/hars`
  (Linux) or `PERF_GATE=1 scripts/perf-docker.sh` (macOS)

## Rules

- Match Home Assistant 2026.6.1's API/JSON shapes, files and behaviour.
  Check HA's source; don't guess.
- Expected values that must match HA come from vectors made by running HA's
  code. A new vector set commits its script in `scripts/vectors/`; existing
  sets have none yet ([its README](scripts/vectors/README.md) lists them).
- Every behaviour change ships with a test. Use the simulated clock; no real
  network.
- Follow the hot-path rules in [PERF.md](docs/PERF.md#rules): `Arc` sharing,
  no `String` clones (`SmolStr`/`EntityId`), no blocking I/O on async
  threads, no polling. Performance claims need numbers.
- Put code where [ARCHITECTURE.md](docs/ARCHITECTURE.md#where-new-code-goes)
  says. Crates depend only downwards (`scripts/test_crate_layers.py`); add a
  crate only when that section's rule allows it.
- `unsafe` is denied workspace-wide; an exception needs a reason in review.
- When a change finishes a roadmap item, tick its box in the same change.
  Update the docs the change affects (CONTRIBUTING.md lists which).
- Never commit tokens, keys, personal data or real coordinates.

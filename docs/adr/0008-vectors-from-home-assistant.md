# 0008: Expected values come from running Home Assistant's code

Status: accepted (2026-11-15)

## Context

Matching Home Assistant means matching its maths, rounding, ordering and
error messages exactly. Expected values written by hand from reading HA's
source encode the reader's misunderstandings. Values produced by scripts
that weren't kept can't be remade when the pinned version moves.

## Decision

- Where hars must give HA's answer, tests compare with **vectors**: real
  inputs and what HA's own code at the pinned version produced for them,
  saved in the module's `vectors/` folder.
- The script that ran HA's code is committed in `scripts/vectors/`, checks
  the pinned versions before it runs, and gives the same output on the same
  input. The rules are in
  [scripts/vectors/README.md](../../scripts/vectors/README.md).

## Consequences

- A disagreement with HA is a failing test, not a bug report months later.
- Moving the pinned version is mechanical: rerun the scripts and read the
  diff.
- Vector sets made before this rule have no script yet; the roadmap tracks
  writing them.

# 0005: Integrations are compiled in, in one crate

Status: accepted (2026-11-13)

## Context

Home Assistant loads integrations as Python packages at run time. hars has
no interpreter. The options are compile-time inclusion, Cargo features to
choose them, WASM plugins, or out-of-process plugins. Each integration added
so far costs little binary size (most of the growth was the HTTPS client,
shared by all).

## Decision

Integrations are Rust modules in `ha-integrations`, compiled in
unconditionally and started from config entries. There are no Cargo
features yet. An integration moves to its own crate, behind a feature, when
it brings heavy dependencies of its own (see [PERF.md](../PERF.md#rules) for
the threshold).

## Consequences

- One build, one binary, and integrations share the HTTP client and the
  entity components.
- Every user carries every integration's code. That is acceptable while the
  binary is far under budget, and is checked in CI.
- Third-party integrations are not possible. A plugin model is an open
  question for after the MVP.

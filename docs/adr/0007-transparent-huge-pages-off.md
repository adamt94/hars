# 0007: Turn transparent huge pages off by re-exec

Status: accepted (2026-11-13)

## Context

On Linux hosts with transparent huge pages set to `always`, the kernel backs
mimalloc's arena with 2 MiB pages: 8 MiB of a 13.2 MiB idle RSS. The first
huge pages are faulted before `main` runs, so setting the flag in `main` only
got to 7.6 MiB. A build-time mimalloc define was fragile: a plain `CFLAGS` or
`cargo install --git` silently dropped it.

## Decision

At start, `hars` sets `PR_SET_THP_DISABLE` and re-execs itself once through
`/proc/self/exe`. The flag is inherited, so the new image allocates nothing
with huge pages. The pid is kept, so it is safe as a container's PID 1.
`MIMALLOC_ALLOW_THP=1` keeps huge pages on.

## Consequences

- Idle RSS went from 13.2 to 4.1 MiB at the time, with no slower start.
- Start-up runs `main` twice on Linux. `hars` must not spawn children, which
  would inherit the flag.

# Performance

Every milestone records its numbers here. The MVP gates are in
[`ROADMAP.md`](ROADMAP.md#resource-budgets-mvp-gates); a regression past a gate
blocks the milestone.

## How to measure

### Process budgets: `scripts/perf.sh`

Linux only (it reads `/proc`). On macOS, `scripts/perf-docker.sh` builds and
runs it in a `rust:1` container.

```sh
scripts/perf.sh target/release/hars 300 1800   # the MVP gate: 5-min warm-up, 30-min quiet
scripts/perf-docker.sh 10 60                   # quick check from macOS
```

It starts the binary, times how long until `GET /api/` answers, posts 200
state writes as warm-up traffic, waits out the warm-up, then measures over the
quiet window:

| Column | Source |
|---|---|
| binary | size of the stripped release binary |
| start→/api/ | process spawn until `/api/` first answers (2 ms polling, so ±2 ms). Since auth, that answer is a `401`; the warm-up then logs in |
| idle RSS / PSS | `Rss` and `Pss` in `/proc/<pid>/smaps_rollup` (exact, unlike `VmRSS`) |
| idle CPU | `utime + stime` delta over the quiet window, % of one core |
| wakeups/s | voluntary context switches summed over all threads: a proxy, since each block is followed by a wakeup. Preemptions are excluded. It warns if threads came or went during the window |

The script refuses to start if something already answers on the port, and
fails if `/api/` does not answer within 10 s.

### Hot paths: `cargo bench -p ha-core`

`criterion` benches in `crates/ha-core/benches/core.rs`:

- `state_write/{changed,unchanged}/{no_attributes,light_attributes}`: one
  `Hass::set_state`. Building the attribute map is excluded.
- `fan_out/feeds/N`: one changed write delivered to `N` lossless feeds and
  one bus subscriber, all drained.

## Results

### M0 (core), 2026-11-13

No integrations yet, so this is the floor the inventory builds on, not a gate
result. Docker on an Apple-silicon Mac (linux/aarch64, 2 vCPUs).

| binary | start→/api/ | idle RSS | idle PSS | idle CPU | wakeups/s | threads | run |
|---|---|---|---|---|---|---|---|
| 2.02 MiB | 8 ms | 13.2 MiB | 12.4 MiB | 0.000 % | 0.00 | 3 | 5s warm-up, 30s quiet |

Idle is truly idle: no CPU time and no voluntary context switches over the quiet window.

**RSS finding:** 8 MiB of the 13.2 MiB is a single mimalloc arena backed by
transparent huge pages (the container's kernel has THP set to `always`, and
`AnonHugePages` is 8192 kB). With `MIMALLOC_ALLOW_THP=0`, the same binary sits at
3.7 MiB (`AnonHugePages` 0). mimalloc's `no_thp` Cargo feature does not help,
because it only skips the `madvise` call.

### THP off by default, 2026-11-13

`hars` now sets `PR_SET_THP_DISABLE` (via `rustix`) and re-execs itself once
at start-up, so the new image allocates nothing with THP on. Setting the flag
without re-exec only got to 7.6 MiB, because mimalloc faults 4 MiB of huge
pages before `main` runs. A build-time mimalloc define was also tried and
rejected: a plain `CFLAGS` in the environment, or `cargo install --git`,
silently drops it. The pid is kept across the exec, and
`MIMALLOC_ALLOW_THP=1` keeps THP on. Same container, built with
`CFLAGS=-O2`:

| binary | start→/api/ | idle RSS | idle PSS | idle CPU | wakeups/s | threads | run |
|---|---|---|---|---|---|---|---|
| 2.02 MiB | 7 ms | 4.1 MiB | 3.1 MiB | 0.000 % | 0.00 | 3 | 5s warm-up, 30s quiet |

Idle RSS went from 13.2 to 4.1 MiB (−69 %), and start-up did not get slower.

### Auth, 2026-11-14

`/api/*` now needs a token, so `perf.sh` creates a login (`hars auth add`) and
the warm-up logs in first, as the frontend does: bcrypt at cost 12 on a
blocking thread, then a `.storage/auth` write. The warm-up must be longer
than tokio's 10 s blocking-thread keep-alive, or that thread exits during the
quiet window and the wakeup count is wrong (`perf.sh` warns).

| binary | start→/api/ | idle RSS | idle PSS | idle CPU | wakeups/s | threads | run |
|---|---|---|---|---|---|---|---|
| 2.70 MiB | 10 ms | 5.2 MiB | 4.3 MiB | 0.000 % | 0.00 | 3 | 15s warm-up, 30s quiet |

Where the extra 1.1 MiB comes from, measured on the same binary (RSS /
anonymous, 15 s after each step):

| after | RSS | anonymous |
|---|---|---|
| start, no login | 4.5 MiB | 1.0 MiB |
| 20 rejected `/api/` requests | 4.7 MiB | 1.0 MiB |
| 1 login | 5.1 MiB | 1.2 MiB |
| 6 logins | 5.2 MiB | 1.3 MiB |

The baseline gained 0.5 MiB, mostly the larger binary's mapped pages. The
first login adds 0.4 MiB: mimalloc keeps the heap the blocking thread used for
bcrypt. Each further login costs about 20 KiB (a refresh token in memory).

### WebSocket API, 2026-11-14

Same harness as above (no WebSocket client connected). An open connection
costs one timer: the 55 s heartbeat ping.

| binary | start→/api/ | idle RSS | idle PSS | idle CPU | wakeups/s | threads | run |
|---|---|---|---|---|---|---|---|
| 3.08 MiB | 9 ms | 5.5 MiB | 4.6 MiB | 0.000 % | 0.00 | 3 | 15s warm-up, 30s quiet |

### Registries, dashboards, zones/people and `sun`, 2026-11-14

Fresh install (no imported data), so `sun` runs at the default location.
Its position refreshes every 2 to 20 minutes depending on the sun's phase,
as in HA, each a single timer; none fell in this 60 s window.

| binary | start→/api/ | idle RSS | idle PSS | idle CPU | wakeups/s | threads | run |
|---|---|---|---|---|---|---|---|
| 3.70 MiB | 10 ms | 6.7 MiB | 5.8 MiB | 0.000 % | 0.00 | 3 | 15s warm-up, 60s quiet |

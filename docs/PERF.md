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
| start→/api/ | process spawn until the first `200` from `/api/` (2 ms polling, so ±2 ms) |
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
because it only skips the `madvise` call. Fixing the default is tracked in
the roadmap.

Benches on the same Mac, natively (`cargo bench -p ha-core`, median):

| Bench | Time |
|---|---|
| `state_write/changed/no_attributes` | 234 ns |
| `state_write/unchanged/no_attributes` | 50 ns |
| `state_write/changed/light_attributes` | 503 ns |
| `state_write/unchanged/light_attributes` | 403 ns |
| `fan_out/feeds/0` | 275 ns |
| `fan_out/feeds/1` | 292 ns |
| `fan_out/feeds/4` | 357 ns |
| `fan_out/feeds/16` | 540 ns |

The timings include freeing what the write leaves behind: the replaced state
for a changed write, and the caller's map for an unchanged one. An unchanged
write with 8 attributes still costs 80 % of a changed one, because comparing
the maps and freeing the caller's map dominate. Each extra lossless feed adds
~15 ns.

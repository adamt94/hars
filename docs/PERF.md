# Performance guide

Using as little memory, CPU, wakeups and disk as possible is hars's main
goal. This guide is the one place for the budgets, the rules that keep hars
inside them, how to measure, and what a pull request must show. Measured
results over time are in [perf/RESULTS.md](perf/RESULTS.md).

## Budgets

| Metric | Budget | Checked in CI |
|---|---|---|
| Stripped release binary | **≤ 15 MiB** (the frontend is served from disk, not embedded) | yes |
| Cold start to serving `/api/` | **≤ 1 s** | yes |
| Idle RSS | **≤ 20 MiB** with the full reference install and one dashboard open | on a fresh install |
| Idle CPU | **≤ 0.1 %** of one core | on a fresh install |
| Wakeups when quiet | **≤ 1 /s** on average | on a fresh install |
| Recorder disk writes when quiet | batched, at most one fsync per 5 s | no (by design: [ADR 0003](adr/0003-recorder-segment-log.md)) |

The MVP is measured on Linux with the [reference
install](adr/0009-mvp-reference-install.md) loaded, after a 5-minute warm-up
and a 30-minute quiet run.

**CI gate.** CI's `docker` job copies the binary out of the image it built
and runs `PERF_GATE=1 scripts/perf.sh ./hars-bin 15 30`. That measures a
fresh install (one login, 200 state writes, no integrations other than
`sun`) and fails the job if any number is over the budgets above, which are
the defaults in `scripts/perf.sh`. The table row is printed in the job's log
on every pull request, so the binary size and idle RSS can be compared with
`main`'s.

## Rules

Each rule says why. Hot paths are the state write, event fan-out, WebSocket
serialisation and filtering, the recorder's intake, and anything run per
state change.

1. **Share, don't copy.** Pass `Arc<State>`, `Arc<Event>` and other shared
   data by `Arc`. *Why:* one `state_changed` reaches the state machine, the
   recorder, automations and every WebSocket client; sharing keeps one state
   and event payload instead of deep-copying them for each consumer.
2. **No `String` clones on hot paths.** Use `EntityId` and `SmolStr` (inline
   up to 23 bytes) and borrow `&str`. *Why:* almost every id and state value
   fits inline, so a write allocates nothing for them; a `String` allocates
   every time.
3. **Count allocations per event.** A changed write allocates a shared
   `State` and `Event`; channel queues and consumers can allocate too. Avoid
   per-consumer payload copies; a matching entity filter allocates nothing
   (it keeps byte offsets). Check a change with the `state_write`, `fan_out` and
   `entity_filter` benches. *Why:* allocation is the main CPU cost at this
   size, and freed memory is not always returned to the OS.
4. **Never poll.** Sleep until the next deadline
   (`tokio::time::sleep_until(hass.clock().instant() + wait)`, as `sun` does),
   or wait on a socket or channel. A device that must be
   polled is polled at Home Assistant's interval, no faster. *Why:* every
   timer wake costs CPU and keeps a small device's CPU out of its deep sleep
   states. Between scheduled work, hars should have no wakeups. Sun updates,
   restore dumps, heartbeats and device polls still wake at their deadlines.
5. **Bound what can grow.** Queues to clients are bounded (the bus, 4096
   events; a WebSocket outbox, 4096 messages). Lossless feeds are unbounded,
   so only internal consumers that keep up may use them. *Why:* a slow
   client must not be able to grow hars's memory.
6. **No blocking I/O on async threads.** File reads and writes, bcrypt and
   other slow work go on `spawn_blocking`. *Why:* a blocked worker stalls
   every task scheduled on it. Note that a blocking thread lingers for 10 s
   after its last job, so measure idle after that. The synchronous frontend
   load and config-directory creation at startup are existing exceptions
   ([startup sequence](ARCHITECTURE.md#start-up)).
7. **Hold nothing while idle.** If something is used once an hour, create it
   then and drop it after (`met` makes its HTTPS client per fetch). *Why:*
   idle RSS is the budget that matters most on small devices.
8. **Every dependency pays for its bytes.** Use `default-features = false`
   and enable only what is used. Report the binary size change of any new
   dependency. *Why:* binary pages that run become RSS (adding the HTTPS
   client grew the binary 1.44 MiB and idle RSS 0.7 MiB, even with `met`
   off), and the binary is the install on a small device.
9. **Large static data stays out of the heap.** Data served as it is
   (translations, icons) is compiled in as ready-made JSON pieces and joined
   per request, never parsed into structures at start. Very large data (the
   frontend) is served from disk. *Why:* compiled-in bytes are file-backed
   pages the kernel can drop; parsed copies are heap that stays.
10. **Feature-gate heavy integrations.** Integrations are compiled in, with
    no Cargo features today ([ADR 0005](adr/0005-integrations-compiled-in.md)).
    An integration that adds more than 512 KiB to the release binary, or
    brings a dependency tree nothing else uses (an MQTT or Zigbee stack),
    goes in its own crate behind a Cargo feature, on by default. *Why:*
    users who don't use it shouldn't carry its pages.
11. **Numbers for every performance claim.** "Faster" or "smaller" in a
    comment, commit or review needs a measurement from the commands below.

## How to measure

### Process numbers: `scripts/perf.sh`

Linux only (it reads `/proc`). On macOS, `scripts/perf-docker.sh` builds a
release binary in a `rust:1` container and runs it there.

```sh
scripts/perf.sh target/release/hars              # 15 s warm-up, 60 s quiet
scripts/perf.sh target/release/hars 300 1800     # the MVP run
scripts/perf-docker.sh 15 60                     # from macOS
PERF_GATE=1 scripts/perf-docker.sh 15 30         # what CI checks
```

It starts the binary with a throwaway config and one login, times how long
until `/api/` answers, logs in and posts 200 state writes, waits out the
warm-up, then measures over the quiet window. It prints one Markdown row:

| Column | Source |
|---|---|
| binary | size of the binary file |
| start→/api/ | process spawn until `/api/` first answers (a `401`). Polled every 2 ms; curl and process start add a few ms |
| idle RSS / PSS | `Rss` and `Pss` from `/proc/<pid>/smaps_rollup` |
| idle CPU | `utime + stime` over the quiet window, % of one core |
| wakeups/s | voluntary context switches of all threads, per second. It warns if threads started or exited during the window |
| threads | threads at the end |

Options, as environment variables:

- `PERF_GATE=1`: exit 3 if a number is over budget. `MAX_BINARY_MIB`,
  `MAX_START_MS`, `MAX_RSS_MIB`, `MAX_CPU_PCT` and `MAX_WAKEUPS` override
  the budgets, for example to check the gate trips. Comparisons use raw
  values; only the printed table is rounded. The boundary checks in
  `scripts/test_perf_report.py` exercise the same `perf-report.awk` used
  by the harness and CI.
- `PERF_MET=1`: add a `met` entry for London, so a real forecast is fetched
  and held.
- `PERF_CONFIG=DIR`: start from a copy of `DIR` instead of an empty config,
  for example an imported install, so its integrations run. This is how the
  full reference-install figure is taken (with a browser left on a
  dashboard).
- `PERF_DETAIL=1`: print the file-backed and anonymous parts of RSS.

The warm-up must be longer than 10 s (the login's blocking thread), or the
wakeup figure is wrong.

### Hot paths: `cargo bench -p ha-core --bench core`

`criterion` benches in `crates/ha-core/benches/core.rs`:

- `state_write/{changed,unchanged}/{no_attributes,light_attributes}`: one
  `Hass::set_state`. Building the attribute map is excluded.
- `fan_out/feeds/N`: one changed write delivered to `N` lossless feeds and
  one bus subscriber, all drained.
- `entity_filter/…`: `subscribe_entities` filters (globs, domains).

To compare a branch with `main` on the same machine:

```sh
git switch main && cargo bench -p ha-core --bench core -- --save-baseline main
git switch -   && cargo bench -p ha-core --bench core -- --baseline main
```

Criterion prints the change and whether it is significant. Add a bench to
`core.rs` (or a new `benches/` file) when you add a hot path.

### Binary size

`ls -l target/release/hars` after `cargo build --release`, on `main` and on
the branch. CI's resource step prints the size of the Docker image's binary
(static musl) for every pull request.

## What a pull request must show

| If the change… | Show in the description |
|---|---|
| does anything | CI green. The resource step is part of it |
| adds or upgrades a dependency, or adds compiled-in data | release binary size before and after, in bytes |
| touches a hot path (rule list above) | `criterion` before and after with `--baseline main`, for the benches it affects (add one if none covers it) |
| adds an integration, a background task, a timer, a socket or a cache | a `scripts/perf.sh` row, warm-up ≥ 15 s and quiet ≥ 60 s, with the feature running (`PERF_CONFIG` or `PERF_MET`), and a new entry in [perf/RESULTS.md](perf/RESULTS.md) |
| claims to be faster or smaller | before and after numbers from the commands above |

"None needed" with a reason is fine for changes that can't affect any of
these (docs, tests only).

## When numbers get worse

- **Over a budget:** CI fails and the pull request does not merge. A budget
  only changes through a new [ADR](adr/README.md) the maintainers agree to.
- **Worse but within budget** (a bench slower with criterion calling it a
  regression, the binary over 256 KiB larger, idle RSS over 0.5 MiB higher,
  or any idle wakeup): the description says how much and why it is worth
  it, and [perf/RESULTS.md](perf/RESULTS.md) records it. The reviewer may
  ask for it to be trimmed first.
- **Found after merge:** open an issue labelled `perf` with the numbers. It
  is fixed or reverted before the milestone it belongs to is closed.

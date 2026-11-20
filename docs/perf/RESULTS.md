# Performance results

Measured numbers, newest last, so a change can be compared with what came
before. How to measure, and the budgets, are in [../PERF.md](../PERF.md).
Process rows come from `scripts/perf.sh` (Linux, or `scripts/perf-docker.sh`
from macOS); bench rows from `cargo bench -p ha-core --bench core`. Add an
entry when a pull request changes the numbers; say what was measured, where,
and what changed.

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

### `met`, 2026-11-14

`PERF_MET=1 scripts/perf-docker.sh 15 60`: a home in London with HA's
onboarding `met` entry, so the run includes a real fetch from api.met.no
(60 KB of JSON, 88 time steps) and holds its forecast.

| binary | start→/api/ | idle RSS | idle PSS | idle CPU | wakeups/s | threads | run |
|---|---|---|---|---|---|---|---|
| 5.14 MiB | 13 ms | 8.7 MiB | 7.6 MiB | 0.000 % | 0.00 | 3 | 15s warm-up, 60s quiet, `met` running |
| 5.14 MiB | 11 ms | 7.4 MiB | 6.5 MiB | 0.000 % | 0.00 | 3 | 15s warm-up, 60s quiet, no `met` entry |

The binary grew 1.44 MiB (3,884,416 → 5,393,024 bytes), nearly all of it
the HTTPS client: rustls with ring, and Mozilla's roots compiled in. A
client that only did HTTP would not reach api.met.no. That size costs 0.7
MiB of RSS even when `met` doesn't run (6.7 → 7.4 MiB), from mapped pages.

Running `met` adds 1.3 MiB, split between 0.6 MiB of file-backed pages
(the TLS code it touches) and 0.6 MiB of anonymous memory (`PERF_DETAIL=1`).
What was tried:

- Loading the system's CA file (`rustls-native-certs`) instead: anonymous
  memory was 0.5 MiB higher (2668 vs 2184 kB), and it is blocking file
  I/O. Before that I/O moved off the async threads, it also put 31 ms onto
  start-up, because the client was built before the server bound its port.
  Compiled-in roots are what HA does too: it ships `certifi`. The Docker
  image now carries no CA bundle.
- Making the client for each fetch and dropping it afterwards: an idle
  `hars` holds no TLS state or socket between the hourly fetches.

Refreshes are one timer every 55–64 minutes, as HA, so none falls in the
window.

### Translations and icons, 2026-11-14

| binary | start→/api/ | idle RSS | idle PSS | idle CPU | wakeups/s | threads | run |
|---|---|---|---|---|---|---|---|
| 5.46 MiB | 11 ms | 7.5 MiB | 6.5 MiB | 0.000 % | 0.00 | 3 | 15s warm-up, 60s quiet |

The binary grew 320 KiB (5,393,024 → 5,720,704 bytes), mostly the 214 KB
of English strings and icons compiled in. Idle RSS barely moved (7.4 → 7.5
MiB): the strings are file-backed pages, read only while a request is
answered. Nothing is parsed or cached; an answer is the stored JSON
pieces joined.

### Entity filter, 2026-11-14

`subscribe_entities` filters (`ha_core::entityfilter`) run once per state
change per filtered subscription. The glob walk keeps byte offsets into the
entity id, so a match allocates nothing. `cargo bench -p ha-core -- entity_filter`
on an Apple M4:

| case | time |
|---|---|
| glob `sensor.phone_*_level` vs `sensor.phone_battery_level` | 76.6 ns |
| the iOS app's `include.domains [zone, person]`, rejecting a sensor | 18.2 ns |
| case 4, globs on both sides | 143 ns |

### `mobile_app` webhook, 2026-11-14

Release `hars` (aarch64-darwin), main vs the webhook branch: 6,337,120 →
6,419,920 bytes, **+80.9 KiB**. That is over the 64 KiB the feature's
design allowed for the whole webhook: `crypto_secretbox` is about 16.5 KB,
and the rest is the pipeline and commands. It is a candidate for trimming
once sensors and location land. Seal + open of a 2 KiB payload costs 12.1 µs
on an Apple M4 (a standalone bench written for the design, not in the
repository).

### Lossless recorded-event feed, 2026-11-15

Every `fire` now takes the bus's lossless-subscriber mutex (held across the
broadcast send, so the recorder's feed and the bus agree on order).
`cargo bench -p ha-core -- "state_write|fan_out"` on an Apple M4, main vs the
branch, with no lossless subscriber: every case within ±2 % (criterion: no
change for `state_write/changed/*` and `fan_out/*`; 106.3 vs 105.2 ns for
an unchanged write, which fires nothing).

| case | main | branch |
|---|---|---|
| `state_write/changed/no_attributes` | 247.4 ns | 243.7 ns |
| `state_write/changed/light_attributes` | 507.8 ns | 499.5 ns |
| `fan_out/feeds/16` | 550.4 ns | 553.2 ns |

### Docker image binary, CI gate, 2026-11-15

The binary from the Docker image (static musl, linux/aarch64, after the
recorder, history, logbook and shadow mode), run the way CI's resource step
runs it: `PERF_GATE=1 scripts/perf.sh ./hars-bin 15 30` in a `rust:1`
container on an Apple-silicon Mac. A fresh install: `sun` only.

| binary | start→/api/ | idle RSS | idle PSS | idle CPU | wakeups/s | threads | run |
|---|---|---|---|---|---|---|---|
| 7.65 MiB | 21 ms | 7.2 MiB | 7.2 MiB | 0.000 % | 0.00 | 4 | 15s warm-up, 30s quiet |

The fourth thread is the recorder's writer. Earlier rows are glibc builds
from `scripts/perf-docker.sh`, so their binary sizes are not directly
comparable with this one. Within every budget.

### Onboarding in the browser, 2026-11-15

The onboarding views, `config/core/update` and `config/core/detect`,
`analytics`, `config_entries/subscribe`, the generated country, currency,
language and default-area tables, and `zone.home`, `sun` and `met` following
the home (a `watch` channel: no wakeups until the settings change). A fresh
install now saves `sun`'s config entry at its first start, as HA does.
`PERF_GATE=1 scripts/perf.sh /target/release/hars 10 30` in a `rust:1`
container on an Apple-silicon Mac (glibc builds, as `scripts/perf-docker.sh`
makes them), `main` (7b365dd) and the branch each built in its own target
volume, three runs each:

| build | binary | start→/api/ | idle RSS | idle PSS | idle CPU | wakeups/s | threads |
|---|---|---|---|---|---|---|---|
| main | 8,279,296 B (7.90 MiB) | 10–31 ms | 9.4–9.6 MiB | 8.4–8.6 MiB | 0.000 % | 0.00 | 4 |
| branch | 8,476,032 B (8.08 MiB) | 9–29 ms | 9.8–10.0 MiB | 8.8–9.0 MiB | 0.000 % | 0.00 | 4 |

The binary grows 196,736 B (192 KiB): the onboarding and settings commands
and about 8 KiB of generated tables. Idle RSS is 0.3–0.4 MiB higher, all of
it file-backed: with `PERF_DETAIL=1`, `Pss_File` is 6,685 → 7,020 kB and
`Anonymous` 2,088 → 2,052 kB, so it is the larger binary's pages, which the
kernel can drop, not heap. Start-up time is noise between runs. Idle stays
idle. Within every budget.

### `render_template` and minijinja, 2026-11-15

The first slice of Jinja templates (`ha_core::template` on minijinja 3.1.0
with the `vendor/minijinja` fuel patch, features `builtins`, `macros`,
`loop_controls`, `adjacent_loop_items`, `fuel`, `preserve_order`) and the
WebSocket `render_template` command, merged with onboarding.
`PERF_GATE=1 PERF_DETAIL=1 scripts/perf-docker.sh 10 30` (glibc build in
`rust:1`, Docker on an Apple-silicon Mac, 2 vCPUs), two runs. A fresh
install, `sun` only; no template is rendered, so the environments are
never built. `main` is main with onboarding, from the entry above.

| build | binary | start→/api/ | idle RSS | idle PSS | idle CPU | wakeups/s | threads |
|---|---|---|---|---|---|---|---|
| main | 8,476,032 B (8.08 MiB) | 9–29 ms | 9.8–10.0 MiB | 8.8–9.0 MiB | 0.000 % | 0.00 | 4 |
| branch | 9,197,760 B (8.77 MiB) | 11–13 ms | 9.8–10.0 MiB | 8.9–9.0 MiB | 0.000 % | 0.00–0.17 | 4 |

The binary grows 721,728 bytes. Idle RSS is within main's range; the
binary's file-backed pages are `Pss_File` 6,968–6,975 kB (main 7,020 kB),
anonymous memory 2,184–2,344 kB. The first run's 0.17 wakeups/s (5 in 30
s) didn't recur in the second (0.00); a `render_template` subscription
holds no task, timer or listener, because templates can't read states or
the clock yet, and nothing renders while idle.

Before main moved, the same command against 7b365dd (8,279,296 B, 9.6 MiB)
measured the first version at 8,869,888 B (+590,592) and, after the review
fixes below, 9,001,024 B (+721,728), both at 9.9 MiB idle RSS.

How the size was trimmed (macOS release builds against 7b365dd, same
toolchain and profile):

| build | bytes | vs 7b365dd |
|---|---|---|
| 7b365dd | 7,363,584 | |
| minijinja at the profile's `opt-level = 3` | 8,142,112 | +778,528 |
| minijinja at `opt-level = "z"` (kept) | 7,813,776 | +450,192 |
| the same with minijinja's default features | 7,863,664 | +500,080 |

`[profile.release.package.minijinja] opt-level = "z"` saves 328,336 bytes;
leaving out `debug`, `multi_template` and `std_collections` saves 49,888.
Rendering stays cheap at `"z"`: the default dashboard's
`## Welcome {{ user }}` takes 5.8 µs to compile and render and 1.1 µs to
render again (4.3 µs and 1.1 µs at `opt-level = 3`; Apple M4, release,
20,000 runs). Templates render once per subscription, never per state
write, so size wins. The growth is over the 256 KiB note threshold: it
buys Jinja itself, which the frontend uses for markdown cards and the
default dashboard's heading, and which automations and template entities
will need. Within every budget.

#### Review round: fuel, Python-semantics filters, render threads

The same command and container after the review fixes: minijinja's `fuel`
and `preserve_order` features, the deep-size fuel patch
(`vendor/minijinja`), Jinja's filters and tests with Python's rules
(`template/jinja.rs`), and renders on blocking threads behind two permits.

| build | binary | start→/api/ | idle RSS | idle PSS | idle CPU | wakeups/s | threads | run |
|---|---|---|---|---|---|---|---|---|
| branch, first version | 8.46 MiB (8,869,888 B) | 10 ms | 9.9 MiB | 9.0 MiB | 0.000 % | 0.00 | 4 | 10s warm-up, 30s quiet |
| branch, after review | 8.58 MiB (9,001,024 B) | 11 ms | 9.9 MiB | 9.0 MiB | 0.000 % | 0.00 | 4 | 10s warm-up, 30s quiet |

+131,136 bytes for the fixes (macOS: 7,813,776 → 7,929,824, +116,048);
721,728 bytes over 7b365dd in all, the same as over main with onboarding. Idle RSS is unchanged: file-backed PSS
6,973 kB, anonymous 2,272 kB. A render now costs 1.2 µs cached and 6.2 µs
with compiling (was 1.1 and 5.8), and a template that runs to its whole
budget stops after 27 ms (Apple M4, release).

### Push notifications to the companion app, 2026-11-15

`mobile_app`'s notify services, the local push channel and cloud push
(`ha_integrations::mobile_app::notify`). Nothing new runs while nothing is
sent: a pending confirmation is one sleeping task (aborted when the app
confirms, 10 s at most), and the HTTPS client is made per push. Measured
with `PERF_GATE=1 PERF_CONFIG=<dir> scripts/perf-docker.sh 15 60`, the dir
holding one push-capable `mobile_app` registration (the services, the notify
entity's restore and the notifier are set up), Docker on an Apple-silicon Mac
(linux/aarch64):

| build | binary | start→/api/ | idle RSS | idle PSS | idle CPU | wakeups/s | threads | run |
|---|---|---|---|---|---|---|---|---|
| branch, one push registration | 8.90 MiB | 10 ms | 9.9 MiB | 8.9 MiB | 0.000 % | 0.00 | 4 | 15s warm-up, 60s quiet |

No new dependency. Release binary on macOS arm64: 8,145,072 → 8,227,968
bytes (+82,896) over `main` at 1306935.

### Template functions: states, type casts, maths, time, 2026-11-15

HA's state, type cast, maths and time functions in `ha_core::template`
(Python's `datetime` model, number rules, `RenderInfo`) and two hooks in
`vendor/minijinja` (operators on objects, missing-attribute messages). No
dependency added. macOS release builds (Apple M4, same toolchain and
profile), `cargo build --release -p ha-server`:

| build | bytes |
|---|---|
| main (1306935) | 8,145,072 |
| branch | 8,410,064 (+264,992) |

The growth is just over the 256 KiB note threshold: it is the functions
dashboards use most (`states()`, `is_state`, `now()`, `float`, `round`,
`as_timestamp`, `relative_time`). Nothing runs while idle: a render reads
the state machine only while it runs (one `Arc` per state it iterates,
dropped at the end), and `render_template` still holds no task, timer or
listener after its reply, so idle RSS, CPU and wakeups are unaffected.
Hot paths (state writes, fan-out) are untouched.

### Templates in automations, 2026-11-16

Templates in YAML automations (`ha-automation`'s `tpl`, `cond`, `track`,
`listen`; `ha_core::template::{run, vars}` and the limited environment). A
`template` trigger is one task that listens only to the entities, domains
or states its last render read and sleeps otherwise; reading the clock
adds HA's minute tick. Renders run on the shared render threads, started
from tokio's blocking pool (so one pool thread lingers 10 s after a
render). Measured with `PERF_GATE=1 PERF_CONFIG=<dir> scripts/perf-docker.sh
15 60`, Docker on an Apple-silicon Mac (linux/aarch64), the dir holding a
`configuration.yaml` with the automations named:

| build | binary | start→/api/ | idle RSS | idle PSS | idle CPU | wakeups/s | threads | run |
|---|---|---|---|---|---|---|---|---|
| branch, a template trigger on a sensor (template condition, templated notify) | 9.58 MiB | 10 ms | 11.4 MiB | 10.5 MiB | 0.000 % | 0.00 | 4 | 15s warm-up, 60s quiet |
| branch, that and a template trigger reading `now()` | 9.58 MiB | 16 ms | 11.1 MiB | 10.2 MiB | 0.000 % | 0.07 | 4 | 15s warm-up, 60s quiet |

The second row's wakeups are the `now()` template re-rendering once a
minute at second 0, as HA's `async_track_template_result` does: each tick
starts a render thread from the blocking pool, whose thread then exits
after its 10 s keep-alive. A trigger that reads only states wakes only when
one of them changes (the test `a_template_trigger_sleeps_until_what_it_reads_changes`
counts deliveries).

No dependency added (`smol_str`, already in the tree, is now a direct
dependency of `ha-automation`). macOS release builds (Apple M4, same
toolchain and profile), `cargo build --release -p ha-server`:

| build | bytes |
|---|---|
| main (4c78179) | 8,542,560 |
| branch | 8,857,216 (+314,656) |

The growth is over the 256 KiB note threshold. It is the automation engine
for templates: the template trigger's tracker (rate limits, listeners by
domain and for every state, the minute tick), HA's condition errors,
`render_complex`, script variables and the limited environment. It is only
paid in pages that run when an automation uses templates; an automation
without templates takes the same path as before and makes no variables.

### Reference install, hars and HA, 2026-11-16

The reference install, on the host it runs on: Docker Desktop on an
Apple-silicon Mac (linux/aarch64), bridge network, `TZ` set, the full
inventory (Hue over its event stream, LIFX and SwitchBot Cloud polled, met,
sun, the iPhone's registration, 4 automations). HA is
`ghcr.io/home-assistant/home-assistant:stable` (2026.6.1); `hars` is the
static musl image built from fd9ba06 under `--memory 150m`.

| | start-up | RSS | CPU | wakeups/s | threads |
|---|---|---|---|---|---|
| HA | 5.85 s | 203 MiB (427 MiB peak) | 0.199 % | 3.4 | 38 |
| `hars`, shadow mode | | 12.1 MiB | 0.017 % | 0.64 | 4 |
| `hars`, in charge | 0.39 s | 10.7 MiB | 0.023 % | 0.75 | 4 |

- **Start-up**: container start (`docker inspect` `StartedAt`) to HA's
  `homeassistant_started` event in its database, and to `hars`'s
  `listening` log line, by which point its automations are set up; the
  devices connect after.
- **RSS, CPU, wakeups**: five quiet minutes each, from `/proc` of the main
  process (CPU from `utime`+`stime`, wakeups as voluntary and involuntary
  context switches summed over its threads). HA and `hars` in shadow mode were
  measured side by side on 2026-11-15, 15 minutes after `hars` started; `hars`
  in charge on 2026-11-16, 5 minutes after the cutover. HA's peak is
  `VmHWM`, reached during its start-up.

Against HA on the same host and devices: about 1/19th of the memory, 1/9th of
the CPU, a fifth of the wakeups and 1/15th of the start-up time. The wakeups
are the device traffic: the Hue event stream and the LIFX and SwitchBot
polls, which HA does too. Within the [budgets](../PERF.md#budgets).

### Reference install again, 2026-11-20

The same install, now on `hars` alone: the image built from fc6592a
(static musl, linux/aarch64) under `--memory 150m` on the same host, with
the same devices and automations plus what has merged since 2026-11-16
(templates in automations, push notifications, the automation editor and
the Settings panel). HA's row is the 2026-11-15 measurement above: HA has
been stopped since the cutover, so it was not run again.

| | start-up | RSS | CPU | wakeups/s | threads |
|---|---|---|---|---|---|
| HA (2026-11-15) | 5.85 s | 203 MiB (427 MiB peak) | 0.199 % | 3.4 | 38 |
| `hars`, fc6592a | 0.59 s | 11.1 MiB (12.5 MiB peak) | 0.015 % | 0.65 | 12 |

- **RSS, CPU, wakeups**: `/proc` of the live process, read from a second
  container in its pid namespace, 11.5 hours after it started. RSS is the
  higher of two `smaps_rollup` readings five minutes apart (11.1 and 10.4
  MiB); the peak is `VmHWM`. CPU and wakeups are the averages over the
  whole 11.5 hours (`utime`+`stime` 6.02 s; 26,814 voluntary and
  involuntary context switches over all threads), which include its
  start-up and a day of normal use. The five quiet minutes alone measured
  0.010 % and 0.54 wakeups/s, but 3 clock ticks is too few to quote.
- **Start-up**: container start (`docker inspect` `StartedAt`) to the
  `listening` log line, on a copy of the live data in shadow mode with the
  network up, three runs: 0.53, 0.59 and 0.63 s. It was 0.39 s on
  2026-11-16. `hars` waits for each integration's first answer before it
  listens, as HA waits for set-up: when the host started before its
  network on 2026-11-19, the same build took 15.3 s, because the Hue
  bridge and the LIFX bulb did not answer until their time-outs.
- **Threads**: 12, up from 4. They are not yet explained; idle CPU and
  wakeups did not rise with them.

Against HA on the same host and devices: about 1/18th of the memory, 1/13th
of the CPU, a fifth of the wakeups and a tenth of the start-up time. Within
the [budgets](../PERF.md#budgets).

### Device and area edits, 2026-11-20

The Add integration dialog's last step: a device's name, area and labels,
a new area, the entity ids that follow from a name, and SwitchBot Cloud's
running entities taking a new name or id. All of it runs when a user
saves a change and at no other time: no timer, no task, nothing that
wakes. What stays in memory is one weak reference per running entry that
follows the registry (an empty, unallocated list on an install with
none), and per SwitchBot entity the names its id is made from (two short
strings) behind a lock that only a state write or a rename takes. A state
write of such an entity now copies its id and friendly name out of that
lock, where it copied the id and built the name's JSON value before: the
same number of allocations.

Release binary (macOS arm64, stripped, `cargo build --release -p
ha-server`): main (49c08fc) 9,469,328 bytes, branch 9,518,944
(+49,616).

The same change stops the registry from parsing every deleted entity row
for each entity it removes (the row for the same unique id is now looked
for in the rows' text first, and the list is changed in place instead of
copied per removal). Removing an entry's 1,000 entities from a registry of
8,000 (`Registry::clear_config_entry`, in memory, an Apple-silicon Mac,
one run each):

| | debug build | release build |
|---|---|---|
| before | 8.10 s | 634 ms |
| after | 0.74 s | 30 ms |

Then 200 single removals (`Registry::remove_entity`, 1,000 deleted rows
kept): 3.65 s to 0.34 s in a debug build, 280 ms to 28 ms in a release
build.

### Config flows, 2026-11-20

Adding an integration from the UI: the flow engine and SwitchBot Cloud's
flow, HA's flow views and four WebSocket commands, the registry rows a
set-up makes, removing an entry. Nothing in it runs or wakes while no flow
is open: no timer and no task. No memory is held either once every flow
was answered or its dialog closed (the list of flows in progress is then
empty and unallocated; a subscriber's listener exists only while its
WebSocket subscription does). A flow whose client vanished without
deleting it stays in memory (its id, step and form schema, a few hundred
bytes) until the next flow starts an hour or more later; at most 16 are
kept. A fresh install, as CI's gate
measures it (`scripts/perf-docker.sh 15 60`, Docker on an Apple-silicon
Mac, linux/aarch64), each row one run:

| | binary | start→/api/ | idle RSS | anonymous | idle CPU | wakeups/s | threads |
|---|---|---|---|---|---|---|---|
| main (064778f) | 9.84 MiB | 10 ms | 8.6 MiB | 2.80 MiB | 0.000 % | 0.00 | 12 |
| main (064778f) | 9.84 MiB | 8 ms | 9.8 MiB | 2.81 MiB | 0.000 % | 0.00 | 12 |
| branch | 9.96 MiB | 16 ms | 9.3 MiB | 2.82 MiB | 0.000 % | 0.00 | 12 |
| branch | 9.96 MiB | 7 ms | 11.6 MiB | 2.94 MiB | 0.000 % | 0.00 | 12 |

Idle CPU and wakeups are unchanged. The RSS of a run is the anonymous
memory plus however much of the binary the kernel happens to keep mapped
(`Pss_File` moved between 5.6 and 8.1 MiB from run to run of one binary),
so compare the anonymous column: within 0.14 MiB, and three runs of main
one commit earlier (385d917) spanned 2.88 to 3.00 MiB by themselves.

Release binary (macOS arm64, stripped, `cargo build --release -p
ha-server`):

| | bytes |
|---|---|
| main (064778f) | 9,122,224 |
| branch | 9,238,176 (+115,952) |

The growth is the six views, the flow manager, the set-up's registry rows
and stopping an entry. A first version grew 297,840 bytes:
`ha_integrations::start` had become generic over its registry argument, so
it was compiled into the binary crate and took a third copy of the HTTPS
client's futures with it (hyper's `send_request` alone went from 36,568 to
54,068 bytes). Found by comparing symbol sizes of two `--profile profiling`
builds; `start` now takes a plain `ha_registry::Lookup`. Two checkouts must
not share one `CARGO_TARGET_DIR` for such a comparison: cargo names a
workspace member's artifacts the same in both and can reuse the other
tree's.

# Add an integration

This walks through adding a Home Assistant integration to hars, using `met`
(the weather forecast from api.met.no) as the worked example. `met` is small
and touches every part: a config entry, an HTTP fetch, setup retries, an
entity with registry-chosen id and name, a shared entity component
(`weather`), a WebSocket command, vectors and a performance entry. Read
[ARCHITECTURE.md](../ARCHITECTURE.md) first for the runtime model.

Paths are relative to the repository root. `<domain>` is HA's domain for the
integration (`met`, `hue`).

## 0. Before you start

- **Is it in scope?** Check the [roadmap](../ROADMAP.md). Integrations
  outside the MVP inventory wait, unless a maintainer agrees in an issue.
- **Read HA's code at the pinned version** (2026.6.1):
  `homeassistant/components/<domain>/` and the library it uses, at the
  version HA 2026.6.1 pins in its `manifest.json`. hars reproduces what HA
  does, including schedules, retries, rounding and error messages.
- **Decide what it needs.** Which entity domains does it create? If a domain
  has services (`light`, `switch`, `climate` …) it uses that domain's shared
  component; if the component doesn't exist yet, add it first
  ([guide](add-an-entity-component.md)). Domains without services (`sensor`,
  `binary_sensor`) are just states.

## 1. Create the module

Make `crates/ha-integrations/src/<domain>/mod.rs` and register it in
`crates/ha-integrations/src/lib.rs` (`pub mod <domain>;`).

`met`'s layout:

```
crates/ha-integrations/src/met/
  mod.rs        DOMAIN, Location, entity(), fetch(), run(), schedule()
  forecast.rs   pyMetno's parsing and HA's forecast maths
  tests.rs      #[cfg(test)] mod tests; in mod.rs
  vectors/      recorded API bodies and what HA produced from them
```

The module starts with a doc comment naming the HA files it mirrors, and
exports `pub const DOMAIN: &str = "<domain>";`.

## 2. Write `run`

Each config entry gets one task running your `run` function until the
runtime stops. `met::run` (in `met/mod.rs`) takes:

```rust
pub async fn run(
    hass: Hass,
    registries: Arc<Registries>,
    weather: Arc<Weather>,          // the shared weather component
    user_agent: &'static str,
    base_url: String,               // injectable, so tests use a local server
    entry: Map<String, Value>,      // the core.config_entries item
    ready: ha_core::SetupGuard,
)
```

What it must do:

1. **Read the entry.** Settings are in `entry["data"]` and
   `entry["options"]`, exactly as HA stored them.
2. **Choose entity ids and names from the registry.** An imported install
   already has a registry row for each entity; use its `entity_id`, name
   and options, and skip the entity if the row is disabled. For new entities,
   build the id the way HA does. `met::entity()` does this by hand;
   `naming::Names` (`crates/ha-integrations/src/naming.rs`) does it for
   integrations with devices (`lifx`, `switchbot_cloud`).
3. **Signal setup once.** Call `ready.done()` after the first successful
   write *or* the first failure. `hars` waits for every integration's first
   attempt (at most 300 s) before firing `homeassistant_start`, so never
   wait for a device to come back before calling it. Dropping the guard also
   counts.
4. **Retry setup as HA does.** On failure, wait with
   `hass.setup_retry_wait(backoff)` on HA's ladder (`met` uses
   `(5 << tries.min(7)).min(600)` seconds plus jitter). Before start it
   waits for `homeassistant_started` instead, as HA does.
5. **Write states** with `hass.set_state(entity_id, state, attributes)`.
   Writes that change nothing reuse the stored state and emit no event,
   so don't diff yourself.
6. **Sleep, don't poll.** After a write, wait for the next push, socket
   message or deadline. `met` sleeps 55–64 minutes between fetches, as HA
   does. Use `hass.clock()` and `tokio::time::sleep_until` so tests can run
   on simulated time.

If the integration restores state across restarts (scenes, buttons), read
`hass.restore().last_state(&id)` at setup and call
`hass.restore().register(id, extra)`. If HA leaves some attributes out of
history, call `hass.set_unrecorded(id, &[...])` (see `sun/mod.rs`).

## 3. Talk to the device or service

- **HTTP(S):** use `ha_http::Client` (`crates/ha-http/src/lib.rs`): `get`,
  `put_json`, `request`, and `stream` for server-sent events. Set
  `hars`'s `USER_AGENT`. If you fetch rarely, make the client per fetch, as
  `met::fetch_once` does, so nothing is held while idle.
- **UDP/TCP:** use tokio sockets directly (`lifx/conn.rs` is the example).
- **Make the address injectable** (`base_url`, a host, a port) so tests can
  point it at a local server.
- No blocking calls in async code. CPU-heavy or blocking work goes on
  `spawn_blocking`.

## 4. Start it from config entries

In `ha_integrations::start` (`crates/ha-integrations/src/lib.rs`):

1. add `<domain>::DOMAIN` to `SUPPORTED`. That makes `hars import` report the
   integration's entries as supported;
2. add a block that spawns `run` for each enabled entry, with a fresh
   `hass.setup_guard()` each, and adds the domain and its entity domains to
   `running` (that list becomes `components` in `/api/config`, which the
   frontend reads):

```rust
let met: Vec<Map<String, Value>> = enabled(met::DOMAIN).cloned().collect();
if !met.is_empty() {
    for entry in met {
        tokio::spawn(met::run(hass.clone(), registries.clone(), weather.clone(),
            USER_AGENT, met::URL.to_owned(), entry, hass.setup_guard()));
    }
    weather.register_services(hass, registries.clone());
    running.extend(["met", "weather"]);
}
```

If your integration provides entities of a domain with a shared component,
take it from `components` (`components.lights()`, `components.switches()` …)
and add each entity with `.add(Arc::new(...))`. Touch the component in
`start` even before entities exist, so its services are registered at start
as in HA (see the `switchbot_cloud` block).

## 5. Integration-specific services and API

- **Services the integration owns** (`hue.activate_scene`): register them
  once, with a `Schema`, through `hass.services().register_with_schema`.
- **WebSocket commands for its domain** (`weather/subscribe_forecast`) go in
  `crates/ha-api/src/ws/<domain>.rs`. If the command needs data from the
  integration, `ha-server` hands it over through an `ApiConfig` field
  (`ApiConfig::weather`). See
  [add-an-api-command.md](add-an-api-command.md).

## 6. Translations, icons and manifests

The frontend asks for each running integration's strings, icons and
manifest. Add the domain to `DOMAINS` in `scripts/gen-i18n.py` and
`scripts/gen-manifests.py`, then regenerate from HA's wheel:

```sh
# HA 2026.6.1 needs Python 3.14.2+; --python-version lets an older pip fetch it.
pip download homeassistant==2026.6.1 --no-deps --only-binary=:all: \
    --python-version 3.14.2 -d /tmp/ha-wheel
python3 scripts/gen-i18n.py /tmp/ha-wheel/homeassistant-2026.6.1-py3-none-any.whl \
    > crates/ha-api/src/i18n_data.rs
python3 scripts/gen-manifests.py /tmp/ha-wheel/homeassistant-2026.6.1-py3-none-any.whl
cargo fmt --all
scripts/check.sh
```

`gen-i18n.py` writes translations and icons to stdout; the redirection
updates `crates/ha-api/src/i18n_data.rs`. `gen-manifests.py` takes the same
wheel argument and writes `crates/ha-api/src/manifests_data.rs` itself,
including `default_config`'s dependencies as well as `DOMAINS`. Run these
commands from the repository root. Don't edit the generated tables by hand.
Note the binary size change (rule 9 in
[PERF.md](../PERF.md#rules)).

## 7. Tests and vectors

Put tests in `<domain>/tests.rs`. `met/tests.rs` shows the three kinds:

- **Vectors** (`state_attributes_and_forecasts_match_ha`): recorded API
  bodies in `vectors/`, and the states, attributes and forecasts HA 2026.6.1
  and pyMetno produced from them. The test loads them with `include_str!`
  and compares field by field. `met`'s vectors predate the rule that keeps
  the script, so there is no `met.py` to copy: start from the skeleton in
  [scripts/vectors/README.md](../../scripts/vectors/README.md), save it as
  `scripts/vectors/<domain>.py`, commit it with the vectors it writes, and
  name it in the comment at the top of your `tests.rs`. (`met/tests.rs`
  only names the versions that made its vectors, on its `Vector` struct.)
- **Schedule and failure** (`refreshes_on_has_schedule_and_goes_unavailable_on_failure`):
  `#[tokio::test(start_paused = true)]`, a `SimulatedClock`, and a closure in
  place of the network. It checks the refresh interval, the setup retry
  ladder and the unavailable state, in simulated hours.
- **The wire** (`fetch_sends_if_modified_since_and_keeps_the_forecast_on_304`):
  a local `TcpListener` serving canned HTTP, checking the exact request.

Sanitise every recorded input: no tokens or keys, ids replaced, coordinates
rounded. Run `cargo test -p ha-integrations <domain>` while working and
`scripts/check.sh` before you push.

## 8. Measure it

Integrations add code, timers and sockets, so the pull request needs numbers
([PERF.md](../PERF.md#what-a-pull-request-must-show)):

1. Release binary size on `main` and on your branch.
2. A `scripts/perf.sh` row with the integration running: make a config dir
   with a `core.config_entries` entry for it (see how `PERF_MET=1` writes
   one in `scripts/perf.sh`), then
   `PERF_CONFIG=that-dir scripts/perf.sh target/release/hars 15 60`
   (on macOS, `PERF_CONFIG=that-dir scripts/perf-docker.sh 15 60`).
   Idle wakeups should stay at 0 between its scheduled work.
3. If it changes a hot path (state processing or per-change fan-out), include
   Criterion before/after results too; add a bench if none covers it.
4. An entry in [perf/RESULTS.md](../perf/RESULTS.md) like the `met` one:
   what grew, why, and what was tried.

## 9. Finish

- Tick the integration's items in the [roadmap](../ROADMAP.md), listing
  deliberate differences from HA.
- Add the domain to "Works today" in the [README](../../README.md).
- Open the pull request with the template filled in.

## Checklist

- [ ] `crates/ha-integrations/src/<domain>/` with `DOMAIN` and `run`
- [ ] `pub mod`, `SUPPORTED` and a `start` block in `lib.rs`
- [ ] Ids and names from the registry; disabled entities skipped
- [ ] `ready.done()` after the first write or failure; HA's retry ladder
- [ ] No polling faster than HA; no blocking calls on async threads
- [ ] Domain added to `gen-i18n.py` and `gen-manifests.py`, tables regenerated
- [ ] Tests: vectors from `scripts/vectors/<domain>.py`, simulated-time
      schedule, wire format
- [ ] Binary size, a `perf.sh` row and a RESULTS entry
- [ ] Roadmap and README updated

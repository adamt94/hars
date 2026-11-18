# Architecture

How hars is built, how it runs, and where new code goes. Performance rules
and budgets are in [PERF.md](PERF.md); decisions and their reasons are in
[adr/](adr/README.md).

## Principles

1. **Small and idle is the feature.** Every design choice is judged by
   memory, CPU, allocations, wakeups and binary size, measured
   ([PERF.md](PERF.md)).
2. **Behave like Home Assistant where a client can see it.** The REST and
   WebSocket APIs, the `.storage` files and the observable behaviour match a
   pinned Home Assistant release, so the official frontend, the companion
   apps and an existing install work unchanged
   ([ADR 0001](adr/0001-match-a-pinned-home-assistant.md)).
3. **One static binary.** No Python and nothing installed at run time.
   Integrations are compiled in
   ([ADR 0005](adr/0005-integrations-compiled-in.md)).
4. **Idle means idle.** Nothing polls when nothing happens. Timers sleep
   until their deadline; nothing wakes just to check for work.

## Crates

Ten crates in `crates/`. `ha-server` builds the only binary, `hars`.

| Crate | Job | Home Assistant equivalent |
|---|---|---|
| `ha-core` | `Hass`, the state machine, the event bus and lossless feeds, services and schemas, lifecycle stages, `.storage` files, restore state, the clock, YAML loading and writing, `slugify`, shadow mode's gate, Jinja templates (on `minijinja`) | `core.py`, `helpers/storage.py`, `helpers/restore_state.py`, `util/yaml`, `helpers/template` |
| `ha-auth` | Users, credentials, login flows, refresh and access tokens, permissions | `auth/` |
| `ha-registry` | Entity, device, area, floor and label registries; target expansion (device/area/floor/label to entities) | `helpers/*_registry.py` |
| `ha-http` | The one outbound HTTP(S) client: HTTP/1.1, rustls with compiled-in roots, timeouts, server-sent events | `aiohttp` client session |
| `ha-recorder` | History: an append-only segment log, its intake, retention, and the history and logbook queries | `components/recorder`, `history`, `logbook` |
| `ha-components` | Built-in `zone` and `person`; new entity ids for them (with `ha_core::slugify`, re-exported here) | `components/zone`, `components/person` |
| `ha-integrations` | Config entries; the shared entity components (`light`, `switch`, `scene`, `button`, `climate`, `weather`); every integration (`sun`, `met`, `hue`, `lifx`, `switchbot_cloud`, `mobile_app`); the config flows onboarding starts (`met`'s) | `config_entries.py`, `components/<domain>`, `components/onboarding/views.py` |
| `ha-automation` | YAML automations: triggers, conditions, actions, modes | `components/automation`, `helpers/script.py` |
| `ha-api` | Everything served over HTTP: REST, WebSocket, auth views, the companion app's webhook, the frontend, brands | `components/api`, `websocket_api`, `http`, `frontend`, `auth` views |
| `ha-server` | The `hars` binary: start-up wiring, signals, the allocator, the `auth add` and `import` commands, shadow mode's log | `__main__.py`, `bootstrap.py` |

### Dependency direction

A crate depends only on crates in lower layers.
`scripts/test_crate_layers.py` checks this in CI.

```
layer 5   ha-server
              │ (everything below except ha-http)
layer 4   ha-api
              │ (everything below)
layer 3   ha-integrations ─────────────────── ha-automation
            (core, registry, components, http)   (core, registry, components)
layer 2   ha-components (core, registry)
layer 1   ha-auth   ha-registry   ha-http   ha-recorder
layer 0   ha-core   (no workspace dependencies)
```

- `ha-core` knows nothing about integrations, HTTP or storage layouts other
  than `.storage`.
- Integrations never import `ha-api`. What the API needs from an
  integration (the weather forecasts, the companion app's registrations) is
  handed to `ha-api` by `ha-server` through `ApiConfig`.
- Only `ha-server` wires things together and reads configuration from the
  environment.

## Runtime model

### Start-up

`main` (`crates/ha-server/src/main.rs`) does this, in order:

1. On Linux, turns off transparent huge pages and re-execs itself once, so
   the allocator never gets 2 MiB pages
   ([ADR 0007](adr/0007-transparent-huge-pages-off.md)).
2. With arguments, runs an offline command (`auth add`, `import`) and exits.
   Otherwise starts a multi-thread tokio runtime.
3. If `HARS_FRONTEND` is set, loads the frontend's `index.html` synchronously
   through `api_config()` before loading core config or starting the recorder.
   Creating the config directory is also synchronous. These are current
   exceptions to the no-blocking-I/O rule, not patterns for new code.
   Loads `.storage/core.config` and `configuration.yaml` on `spawn_blocking`.
   Each top-level `configuration.yaml` key hars ignores logs a warning
   (`ha-server`'s `config_yaml.rs`): hars reads only `recorder:` and
   `automation:`.
4. Starts the **recorder** first, so every state written from here on is
   history, then shadow mode's log if `HARS_SHADOW=1`.
5. Loads the registries, the restore state, `zone` and `person`, and the
   config entries (on a fresh install, saving `sun`'s, as HA's first start
   imports it).
6. **Starts the integrations** (`ha_integrations::start`) and the companion
   app's registrations, then sets up the YAML automations.
7. Loads the frontend's settings, Labs, the analytics preferences and
   auth, works out the installation type, binds the HTTP port and starts
   serving. Until onboarding is done, the onboarding views are served too.
8. Waits until every integration has made its first setup attempt (at most
   300 s, HA's `STAGE_2_TIMEOUT`), then fires `homeassistant_start`, writes
   registered entities that nothing provides as `unavailable`, fires
   `homeassistant_started` and attaches the automations' triggers.

### `Hass`, states and events

- **`Hass`** is a cheap-to-clone handle (`Arc` inside) passed to everything.
  It holds the clock, the state machine, the event bus, the service
  registry, the lifecycle state, the core config and the restore state.
  The core config sits in a `watch` channel: what follows the home
  (`zone.home`, `sun`, a `met` entry that tracks the home) sleeps on
  `hass.watch_config()` and wakes only when `config/core/update` changes
  it; everything else reads `hass.config()` when it needs it.
- **`EntityId`** is `domain.object_id`, validated once at the edge and
  stored as a `SmolStr` (inline up to 23 bytes, so it almost never
  allocates).
- **`State`** is a snapshot shared as `Arc<State>`, immutable except for
  the atomic `last_reported` timestamp (its JSON value freezes at first
  serialisation, matching HA's cache). The state machine, the
  `state_changed` event and every API response share one allocation.
- **`StateMachine`** is a `RwLock<HashMap<EntityId, Arc<State>>>`. A write
  that changes nothing returns the existing `Arc`, moves only
  `last_reported` and fires no event.

There are three ways to hear about changes:

| Feed | Delivery | Used by |
|---|---|---|
| `hass.bus().subscribe()` | Bounded `broadcast` (4096 events) of `Arc<Event>`. A slow subscriber lags and must resync from the state machine | WebSocket clients |
| `hass.subscribe_state_changes()` | Unbounded and lossless, `state_changed` only, with a snapshot taken atomically with the subscription | automations, `person`, `subscribe_entities`, history streams |
| `hass.bus().subscribe_lossless(filter)` | Unbounded and lossless, any event a filter picks | the recorder, shadow mode's log |

Writers send to the lossless feeds and the bus while holding the state lock,
so every consumer sees the same order
([ADR 0002](adr/0002-lossy-bus-lossless-feeds.md)).

### Services

`hass.services()` is HA's service registry. A handler is registered with
`register_with_schema(domain, service, schema, supports_response, handler)`,
where the handler is an async closure taking a `ServiceCall`. Registering
the same service again replaces the handler, which is why the shared entity
components register their domain's services once
([below](#where-new-code-goes)).

`hass.call_service` looks the service up, checks the response mode, merges
`target` into the data, validates it against the `Schema` (unknown keys
rejected, as voluptuous), fires `call_service`, then runs the handler. In
shadow mode the handler of a domain that acts on devices is skipped after
the event fires ([SHADOW.md](SHADOW.md)). `entity_id` targets are resolved by
`ServiceCall::entity_ids`; device, area, floor and label targets by
`ha_registry::Registries::target_entities`.

### Config entries and integration setup

Integrations start from HA's `core.config_entries`, imported as is. On a
fresh install with no config-entries file, `sun`'s entry is saved and `sun`
starts, as HA's default configuration does. Onboarding's core config step
adds `met`'s entry and starts it while running
(`ha_integrations::onboarding`). `ha_integrations::start` matches enabled entries to integrations by
domain and spawns one task per entry. Each integration's `run` function:

1. gets a `SetupGuard` (`hass.setup_guard()`) and calls `done()` after its
   first write or its first failure, so start-up never waits on a device
   that is down;
2. retries a failed setup on HA's ladder (5 s doubling to 10 min) with
   `hass.setup_retry_wait`, which before `homeassistant_started` waits for
   start instead, as HA does;
3. writes its entities with `hass.set_state`, taking ids and names from the
   entity registry, and then sleeps until the next push, poll or timer.

Integrations have no stop hook: their tasks end with the runtime. Anything
that must reach disk at stop registers with `hass.on_final_write`.

### Storage and restore

- **`.storage` files** (`ha_core::storage::Store`) use HA's envelope
  (`version`, `minor_version`, `key`, `data`) and HA's file names, so the
  files move between HA and hars. A write goes to a temp file, is synced,
  renamed over the old one, and the directory synced. Secrets are written
  mode 0600. `Store` is blocking: use `spawn_blocking` from async code,
  including during start-up; offline CLI commands can call it directly.
- **Delayed saves** (`DelayedSave`) are HA's `async_delay_save`: a change
  schedules a write after a quiet period (registries 10 s, or 180 s while
  starting; config entries 1 s; `person` 10 s), and `flush()` writes at
  stop.
- **Restore state** (`core.restore_state`) is loaded at start, before any
  entity that restores itself, and dumped at start, every 15 minutes and at
  stop, with HA's 7-day expiry. An entity opts in with
  `hass.restore().register(entity_id, extra_data)`.

### Recorder

`ha-recorder` keeps history in an append-only segment log under
`<config>/.hars/recorder/`, not in SQLite
([ADR 0003](adr/0003-recorder-segment-log.md)). Its intake reads the bus's
lossless feed (states, `call_service`, lifecycle events,
`automation_triggered`), turns events into rows as HA's recorder would, and
hands them to one writer thread. The writer batches rows for up to 5 s, one
`write()` per batch, and syncs a segment when it closes. Retention is 10
days and 64 MiB by default, purged nightly at 04:12. The WebSocket history
and logbook commands read the store on `spawn_blocking`; live streams follow
the intake's rows.

### Automations

`ha-automation` reads `automation:` from `configuration.yaml` at start.
Automations whose config hars can't run are `unavailable`; the import report
says why. Triggers attach at `homeassistant_started`
and listen on the lossless state feed, by entity, by domain or to every
change. Time triggers sleep until their wall time, and check the wall clock
again when they wake.

Templates in automations compile once, when the config loads (on a render
thread: compiling recurses as deep as a template nests). They render on the
render threads `ha_core::template::run` keeps for every template, the
WebSocket's `render_template` too: a thread per job with
`template::STACK`, at most two at a time, started from tokio's blocking pool
and never on the async runtime. What a render logs is written back on the
runtime. When a change needs a render before the next change can be judged
(a template trigger, a templated condition or variable, the run's first
templated step), the listener takes a guard and the state feed waits for
it, so changes are handled in order, as in HA's event loop. A `template`
trigger is one task that listens only to what its last render read and
sleeps otherwise (a minute tick while it reads the clock).

`automation.reload` reads `configuration.yaml` again (the loader
`ha-server` hands over, off the runtime) and compares each automation with
its entry by name and raw config, as HA does: an unchanged one is left
alone, with its triggers and runs; a changed one is removed (its triggers
and runs stop, its last state goes to the restore cache, its entity shows
the registry's `unavailable` placeholder) and made again, restoring on/off
and `last_triggered` from that cache. Reloads run one at a time. An
automation with an `id` has an entity registry row, made when it is first
seen, which is where its entity id comes from.

The editor's saves (`ha-api`'s `automation.rs`) rewrite `automations.yaml`
whole, as HA does: the file is loaded, the posted entry put in place of
the one with its id (or at the end), and the list written with
`ha_core::yaml::dump` to a temporary file renamed over the old one. Then
`automation.reload` runs for that id. A delete removes the entry, then the
automation's registry row and entity.

### Shadow mode

With `HARS_SHADOW=1`, `call_service` still validates and fires the event,
but only domains in `ha_core::shadow::RUNS` (automation, homeassistant,
frontend and other internal ones) run their handlers. Held-back calls and
`automation_triggered` events go to `<config>/.hars/shadow.jsonl` for
`scripts/shadow-diff.py` ([SHADOW.md](SHADOW.md)).

### Shutdown

On SIGTERM or SIGINT, `main`:

1. stops the automations and fires `homeassistant_stop`;
2. stops accepting HTTP and gives open requests up to 5 s;
3. saves auth, dumps the restore state, flushes the registries, config
   entries and `person`;
4. fires `homeassistant_final_write` and waits for its handlers (the
   recorder drains its queue and syncs; shadow mode's log is written), at
   most 60 s;
5. fires `homeassistant_close` (handlers get at most 30 s) and exits 0.

### Time

`ha_core::Clock` is either the system clock or a `SimulatedClock` that
follows tokio's pausable time. Entity timestamps, sun maths and automations
use `hass.clock()`; timers use tokio's pausable monotonic time. Their tests
run days of simulated time in milliseconds.

## API surface

All of it is in `ha-api`; `ha_api::router` builds it.

| Surface | Where | Notes |
|---|---|---|
| REST `/api/…` | `rest.rs`, `automation.rs`, `lib.rs` (`/api/config`), `mobile_app::registration_routes` (`/api/mobile_app/registrations`) | Behind `auth::require_auth`; each route's permission is in `auth::required_permission` |
| Auth `/auth/…` | `auth.rs` | Login flow, token, revoke; HA's `.storage/auth*` files. `/auth/token` and `/auth/revoke` allow CORS from any origin (`cors.rs`, aiohttp_cors as HA sets it up) |
| WebSocket `/api/websocket` | `ws/` | Authenticates in the protocol; `ws/commands.rs::dispatch` matches the command type, then asks each domain's module (`ws/weather.rs`, `ws/history.rs`, …) |
| Companion app webhook `/api/webhook/{id}` | `mobile_app/` | The webhook id is the credential; encryption is required for registrations with `supports_encryption` enabled |
| Companion app push channel (WS `mobile_app/push_notification_*`) | `ws/mobile_app.rs` | The commands only; the channels, the confirm timers and cloud push (through `ha-http`) are `ha_integrations::mobile_app::notify`'s, reached through `ApiConfig.mobile_app` |
| Frontend `/`, `/frontend_latest/…` | `frontend.rs` | Served from `HARS_FRONTEND` on disk with precompressed files ([ADR 0004](adr/0004-frontend-from-disk.md)) |
| Onboarding `/api/onboarding/…` | `onboarding.rs` | HA's views, registered only while a step is left. The status, installation type, `users` and `integration/wait` views are anonymous; the other steps need a `Bearer` token |
| Brands `/api/brands/…` | `brands.rs` | Proxied from HA's CDN with HA's disk cache |
| Map tiles `/hars/map_tiles/…` | `map_tiles.rs` | Not HA's: OpenStreetMap tiles relayed for the frontend's maps, which the image build points here ([ADR 0010](adr/0010-map-tiles-from-osm.md)) |

How to add a command or a view:
[guides/add-an-api-command.md](guides/add-an-api-command.md).

## Home Assistant compatibility

hars matches one pinned release of each thing it talks to. Moving a pin is a
deliberate change: remake each vector set with its generator at the new
release (the existing sets need their generators written first; see below),
refresh every compatibility table in the
[vector and table inventory](../scripts/vectors/README.md), review its inline
HA/Python expectations, run `scripts/check.sh`, then run the browser and
companion-app checks. Missing vector generators and table extractors must
first reproduce the current pin before being updated to the new one.

| Thing | Pinned version |
|---|---|
| Home Assistant core (behaviour, `.storage` shapes, import) | **2026.6.1** |
| Frontend served by hars | **`home-assistant-frontend` 20260527.4** (what 2026.6.1 ships; `Dockerfile` checks its sha256), with its map tile layer pointed at OpenStreetMap ([ADR 0010](adr/0010-map-tiles-from-osm.md)) |
| iOS companion app | **2026.9.0** on iOS 27 |
| Test time zone and units | Europe/London (for its DST transitions), metric |

How it is kept:

- **Vectors.** Where hars must give HA's answer, tests compare with inputs
  run through HA's own code at the pinned version. A new vector set commits
  the script that made it in `scripts/vectors/` (`onboarding.py`,
  `render_template.py`, `mobile_app_notify.py`, `template_functions.py`,
  `automation_templates.py`).
  The sets made before that rule have no script yet;
  they are listed as a backlog in
  [scripts/vectors/README.md](../scripts/vectors/README.md#vector-sets-made-before-this-rule)
  ([ADR 0008](adr/0008-vectors-from-home-assistant.md)).
- **Generated tables.** Translations, icons and manifests are generated from
  HA's wheel by `scripts/gen-i18n.py` and `scripts/gen-manifests.py`;
  the countries, currencies and languages `config/core/update` accepts and
  onboarding's default area names by `scripts/gen-onboarding.py`. Sensor
  units, statistic units and the Labs feature list are also compiled in;
  the [inventory](../scripts/vectors/README.md#generated-compatibility-tables)
  records their sources and missing extractors, plus Python's case-fold table.
- **Same files.** hars reads and writes HA's `.storage` files, so
  `hars import` copies them byte for byte after checking them with hars's own
  loaders.
- **Deliberate differences** are written in the module's doc comment and in
  the [roadmap](ROADMAP.md).
- **One patched dependency.** `vendor/minijinja` is minijinja 3.1.0 with a
  change to its fuel accounting, so a template's budget covers everything
  it makes, and hooks for objects: answering operators (Python's
  `datetime` arithmetic), paying for what they copy (a state's
  attributes), and wording a missing attribute's message
  ([`vendor/minijinja/HARS.md`](../vendor/minijinja/HARS.md));
  `[patch.crates-io]` in `Cargo.toml` points at it.

## Where new code goes

| You are adding | It goes in | Notes |
|---|---|---|
| An integration (a config entry domain) | `crates/ha-integrations/src/<domain>/` | Started from `ha_integrations::start`, listed in `SUPPORTED`. [Guide](guides/add-an-integration.md) |
| An entity platform for an existing domain (a new kind of light) | The integration's module, implementing that component's trait (`light::Light`, `switch::Switch`, …) | Add it with `components.lights().add(...)` |
| A new entity domain with services (`cover`, `fan`) | `crates/ha-integrations/src/<domain>.rs`, plus an accessor on `components::Components` | [Guide](guides/add-an-entity-component.md) |
| A domain with no services (`sensor`, `binary_sensor`) | Nothing shared: the integration writes states | |
| A built-in that isn't a config entry (`zone`, `person`, a helper like `input_boolean`) | `ha-components` | |
| A WebSocket command | `crates/ha-api/src/ws/<domain>.rs`, chained in `ws/commands.rs` | [Guide](guides/add-an-api-command.md) |
| A REST view | `crates/ha-api/src/<area>.rs`, merged in `ha_api::router` | Add its permission to `auth::required_permission` |
| Core behaviour every crate needs (event types, lifecycle, storage) | `ha-core` | Keep it free of integration knowledge |
| A template function, filter or test (`states`, `as_timestamp`) | `crates/ha-core/src/template/` | Templates live in `ha-core` because the API, automations and template entities all render them; they are not a crate of their own because every build carries them (see below) |
| New YAML automation syntax | `crates/ha-automation/src/config.rs` and `lib.rs` | |
| Wiring, environment variables, CLI commands | `ha-server` | |

**A new crate** is justified only when a module has its own heavy
dependencies that most builds should not carry (a protocol stack such as
MQTT or Zigbee), or has grown past the point where its crate can be
understood on its own. Add it to a layer in `scripts/test_crate_layers.py`
and to the diagram above in the same pull request.

## Open design questions

- **Feature-gating integrations.** All integrations are compiled in today
  and there are no Cargo features. [PERF.md](PERF.md#rules) says when that
  changes. Third-party integrations (WASM or out-of-process) are not
  planned before the MVP.
- **State map.** One `RwLock` today. Sharded or lock-free maps (`papaya`,
  `scc`) are worth a benchmark once a realistic many-writer workload exists.
- **Attribute storage.** `serde_json::Map` today; a compact typed
  representation would cut allocations per write.
- **One serialised state per change.** Each WebSocket connection serialises
  the state itself; sharing one `Arc<str>` per change is on the roadmap.
- **Runtime.** A multi-thread tokio runtime today. A `current_thread`
  runtime may suit single-core devices; it needs measuring.
- **Integration stop hooks.** Integrations end with the runtime. If one needs
  to say goodbye to a device, it should use `on_final_write` or a new hook.

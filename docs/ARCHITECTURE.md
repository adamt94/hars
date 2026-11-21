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
| `ha-http` | The one outbound HTTP(S) client: HTTP/1.1, rustls with compiled-in roots, timeouts, redirects as aiohttp follows them (no `Authorization`, `Cookie` or `Proxy-Authorization` to another origin), server-sent events | `aiohttp` client session |
| `ha-recorder` | History: an append-only segment log, its intake, retention, and the history and logbook queries | `components/recorder`, `history`, `logbook` |
| `ha-components` | Built-in `zone` and `person`; new entity ids for them (with `ha_core::slugify`, re-exported here) | `components/zone`, `components/person` |
| `ha-integrations` | Config entries; the shared entity components (`light`, `switch`, `scene`, `button`, `climate`, `weather`); every integration (`sun`, `met`, `hue`, `lifx`, `switchbot_cloud`, `mobile_app`); config flows (`flow`: the engine, and `switchbot_cloud`'s, `hue`'s and `lifx`'s); the config flows onboarding starts (`met`'s) | `config_entries.py`, `data_entry_flow.py`, `components/<domain>`, `components/onboarding/views.py` |
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
that must reach disk at stop registers with `hass.on_final_write`. The
exceptions are `switchbot_cloud` and `hue`, whose entries' tasks
`flow::Manager` keeps a handle to and aborts (and waits for) when the
entry is removed, after which nothing of the entry writes a state; a
`mobile_app` entry's registration is stopped the way a re-pair stops it.

**Config flows** (`ha_integrations::flow`) add an entry while `hars` runs.
An integration registers a `flow::Handler` in `flow::HANDLERS`; each flow
is one `flow::Flow` value the handler makes, and its fields are what the
flow remembers between steps. A step returns a `flow::Step`: a form (its
fields serialised as HA's `voluptuous_serialize` does, and the answer
checked against them), an abort, or the entry to create. `flow::Manager`
keeps the flows in progress (an empty, unallocated list while no dialog is
open; a flow leaves when it is answered or deleted, with no timer; one
nobody came back to is dropped when another starts an hour later, and 16
are kept at most), and on
`CreateEntry` adds the entry (`ConfigEntries::add_tracked`), starts its
integration and waits for the first setup attempt before answering, as
HA's `async_add` does. The integration's `run` reports where its setup is
through a `config_entries::Status` (`setup_in_progress`, `loaded`,
`setup_retry` with HA's reason), and `ConfigEntries` tells
`config_entries/subscribe` each `added`, `updated` and `removed`.

An entry set up at runtime needs registry rows nobody imported, so
`switchbot_cloud`'s and `hue`'s set-ups make their devices' and entities'
rows with `Registry::get_or_create_device` and `get_or_create_entity`, as
HA's platforms do on every setup (`hue/register.rs` for a bridge: the
bridge, each device, each room and zone as a service device with HA's
suggested areas, a row per entity), then name their entities from the
registry as before. A row that exists is changed only where HA's values
differ, so an imported install's rows come out as they went in. `Components` holds an `ha_registry::Lookup`: the live registry in
`hars` (a fixed set in tests), so the entity services resolve device and
area targets against rows made after start-up. What `hars` calls outside
(`ha_integrations::Endpoints`: the SwitchBot API's base URL, Hue's
discovery service, and a stand-in for every Hue bridge) is a parameter,
so tests and a debug build's browser run point it at 127.0.0.1.

**Registry changes that reach running entities.** HA's entities each
listen for their registry row and their device to change. In `hars` an
entity takes its id and friendly name from the registry when its entry is
set up, so whoever changes a device's name or an entity's id
(`ha-api`'s `ws/device_registry.rs` and `ws/entity_registry.rs`) makes the
change inside `Components::registry_edit`, which then has each entry that
registered as a `components::Follower` take its entities' ids and names
again: a new name is one more state write, a new id removes the old state
and puts the entity under the new id in its component, with what it had
(or, as HA's entity added again does, what was kept for a restore under
that id). The edit and the entities' move happen under the lock that
entries are set up and removed under, so neither a set-up nor a removal
sees rows the entities have not caught up with, and an entity writes its
state under the lock its id is changed under, so no write lands on an id
it has just left. A Hue state is made before the lock is taken, so it is
made again under the lock if the bridge took the registries again in
between, and an entity moved to another id writes nothing more.
`components::follows` lists the integrations that do this (today
`switchbot_cloud`, `hue` and `lifx`); the registry refuses a change the others'
entities would not follow (`ha_registry::DeviceRefusal`,
`RenameRefusal`), and one whose store it would not write
(`Registry::unwritable`: a version refusal, known before the change; a
write that fails later is logged, as for every store). No task or timer
of its own is involved (the registries' saves are delayed as before): a
follower is a reference kept from an entry's set-up until the entry is
removed, in a list that is empty on an install with no such entry.

One rule keeps a Hue bridge's entities and renames apart: every entity
the bridge exposes to services comes to be in one place,
`hue::Bridge::expose`. Under one hold of the set-up lock (then the
bridge's write guard), `expose` gives the entity its id, puts it in the
bridge's list and its component, and writes its first state. Set-up, an
event-stream addition, each addition in a reconnect's snapshot, and a
room that releases the scenes waiting for it all go through it. A
registry edit holds the same lock until the running entities have
followed it. So a new entity never takes an id a rename is giving or
one another entry's row has (ids are checked against the live
registry), and a rename that comes after finds the entity in its list,
in its component and with a state (its id is taken). A resource the
bridge reports again as another type, while an entity of the same
domain runs for its id, makes no second entity, as HA's platform
refuses a unique id that is taken. A reconnect's snapshot is taken one
resource at a time, in its order, each followed by its writes, as the
events it stands for: an addition makes only what that resource lets
be made. What existing entities write after a change waits for no
lock: a reconnect's updates don't hold up a set-up or a registry edit.

### Storage and restore

- **`.storage` files** (`ha_core::storage::Store`) use HA's envelope
  (`version`, `minor_version`, `key`, `data`) and HA's file names, so the
  files move between HA and hars. A write goes to a temp file, is synced,
  renamed over the old one, and the directory synced. Secrets are written
  mode 0600. `Store` is blocking: use `spawn_blocking` from async code,
  including during start-up; offline CLI commands can call it directly.
  As in HA, a file of a newer major version is an error; one of a newer
  minor version loads, and the next save writes hars's own minor version
  (the registries and config entries never write over another minor
  version: [what that means for imports](#which-home-assistant-releases-can-be-imported)).
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
| The frontend's Settings panel | `ws/entity_registry.rs`, `ws/device_registry.rs`, `automation.rs`, `ws/automation.rs` | Registered for admins (`ws/commands.rs::panels`). hars answers what the automation pages ask for; the roadmap lists the pages that still have gaps |
| Companion app webhook `/api/webhook/{id}` | `mobile_app/` | The webhook id is the credential; encryption is required for registrations with `supports_encryption` enabled |
| Companion app push channel (WS `mobile_app/push_notification_*`) | `ws/mobile_app.rs` | The commands only; the channels, the confirm timers and cloud push (through `ha-http`) are `ha_integrations::mobile_app::notify`'s, reached through `ApiConfig.mobile_app` |
| Frontend `/`, `/frontend_latest/…` | `frontend.rs` | Served from `HARS_FRONTEND` on disk with precompressed files ([ADR 0004](adr/0004-frontend-from-disk.md)) |
| Config flows `/api/config/config_entries/…` | `config_entries.rs`, `ws/config_entries.rs`, `ws/manifest.rs` | HA's flow views (start, show, answer, delete), `flow_handlers` and the entry's delete, over `ha_integrations::flow::Manager` (`ApiConfig.flows`); WS `config_entries/get`, `subscribe`, `flow/progress` and `integration/descriptions`. Only the integrations `hars` has a flow for are offered |
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
| Home Assistant core (behaviour, `.storage` shapes) | **2026.6.1** |
| Home Assistant installs `hars import` takes over | **2026.6.x to 2026.11.x** ([below](#which-home-assistant-releases-can-be-imported)) |
| Frontend served by hars | **`home-assistant-frontend` 20260527.4** (what 2026.6.1 ships; `Dockerfile` checks its sha256), with its map tile layer pointed at OpenStreetMap ([ADR 0010](adr/0010-map-tiles-from-osm.md)) |
| iOS companion app | **2026.9.0** on iOS 27 |
| Test time zone and units | Europe/London (for its DST transitions), metric |

How it is kept:

- **Vectors.** Where hars must give HA's answer, tests compare with inputs
  run through HA's own code at the pinned version. A new vector set commits
  the script that made it in `scripts/vectors/` (`onboarding.py`,
  `render_template.py`, `mobile_app_notify.py`, `template_functions.py`,
  `automation_templates.py`, `config_flow.py`, …).
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
  loaders. A file that 2026.8 to 2026.11 write in a newer format is put back
  into 2026.6.1's first. Which releases' files it takes is
  [below](#which-home-assistant-releases-can-be-imported).
- **Deliberate differences** are written in the module's doc comment and in
  the [roadmap](ROADMAP.md).
- **One patched dependency.** `vendor/minijinja` is minijinja 3.1.0 with a
  change to its fuel accounting, so a template's budget covers everything
  it makes, and hooks for objects: answering operators (Python's
  `datetime` arithmetic), paying for what they copy (a state's
  attributes), and wording a missing attribute's message
  ([`vendor/minijinja/HARS.md`](../vendor/minijinja/HARS.md));
  `[patch.crates-io]` in `Cargo.toml` points at it.

### Which Home Assistant releases can be imported

The pin says what hars *behaves like*. What `hars import` *accepts* is
wider: the release lines it is tested with are 2026.6 to 2026.11. hars at
run time reads and writes 2026.6.1's formats only, so the import works in
two steps on a staged copy, and both go by each store's version, not by
the release number.

**First, conversion** (`crates/ha-server/src/import/convert.rs`). The
stores and config entries that 2026.8 to 2026.11 write in a newer form are
written again in 2026.6.1's: `core.device_registry` 3.2 and 3.4 to 1.12,
`core.entity_registry` 1.23 and 1.24 to 1.22, `core.restore_state` rows
without `entity_registry_id`, and `lifx` and `met` entries 2.1 to 1.1.
Home Assistant has no code that goes back a version, so each rule is the
reverse of the migration that release runs, or the answer that release
itself gives to code written for the old form (`BaseDeviceEntry`'s
deprecated `config_entries`, `async_get_legacy_friendly_name`). Where
neither exists, the data stays on the row as a field hars doesn't use and
writes back (a split device's `composite_device_id`, a child device's
`parent_device_id`), or the import stops. Anything left out is listed in
the report. A version with no rule (a later minor version included) is
refused, as before.

**Then, the checks.** The rule starts from Home Assistant's own for a
downgrade (what would 2026.6.1 do with these files?) and is stricter where
hars could not keep the data safe.

- A store with a newer **major** storage version stops the import.
  Home Assistant refuses such a file too (`helpers/storage.py` raises
  `UnsupportedStorageVersionError`); for the stores its bootstrap loads
  (the registries, the config entries, the restore state) it then starts
  in recovery mode. So does a config entry of a newer **version** for an
  integration hars runs (`ConfigEntry.async_migrate` answers
  `migration_error`; hars refuses a disabled one as well). The whole
  import is refused, with the store and both versions named, and none of
  the destination's files is changed.
- A newer **minor** storage version is compatible by HA's rule: HA loads
  the data as it is and saves it at its own minor version. hars takes one
  only where its own reader and writer make that safe, store by store
  (`Minor` in `crates/ha-server/src/import.rs`):
  - most stores: read, and written at hars's minor version the next time
    they change, with what hars's writer for that store keeps (`mobile_app`:
    the deleted ids only, as 2026.6.1; the frontend's settings and the
    dashboards: every key);
  - the entity, device and area registries and `core.config_entries`: only
    the exact minor version hars writes. hars never writes these over a
    file of another minor version, so every later change would be lost at
    the next restart. A newer one is refused, and an older one too, which
    is stricter than HA (it migrates an older file and saves it when it
    starts, so a directory 2026.6 or 2026.7 has run on is at these
    versions, and one a later release has run on is at versions the
    conversion takes);
  - the floor, label and category registries, which hars only reads, and
    `person`: not a newer one (2026.6.1's own `person` migration fails on
    one).
- Each store is also decoded with the decoder the server uses at start, so
  a file the server would start without (and then overwrite) stops the
  import. Frontend settings that don't decode are the exception: they are
  left out and listed in the report.
- Releases older than 2026.6 are refused: update Home Assistant first.

| Home Assistant | `hars import` | What that release changed in what hars reads |
|---|---|---|
| before 2026.6 | refused: update to a release from 2026.6 to 2026.11 first | older store formats, which hars doesn't migrate |
| **2026.6.x** | **imports** (tested: 2026.6.1, 2026.6.4) | nothing (2026.6.4 gives Hue's grouped lights a `translation_key`) |
| **2026.7.x** | **imports** (tested: 2026.7.0, 2026.7.4) | `mobile_app`'s store is 1.2 (Live Activity push tokens); `switchbot_cloud` entries can hold a `cloudhook_url`; `sun` removes its long-gone `solar_rising` sensor's registry row; the Labs preview for the new triggers and conditions leaves `core.labs`. No other store hars reads changes version or shape |
| **2026.8.x** | **imports, converted** (tested: 2026.8.3) | `core.device_registry` is major version 3 (3.2): a device belongs to one config entry, and a device two integrations shared is split in two with new ids. Converted to 1.12; the parts stay apart, with the new ids. `core.entity_registry` is 1.23 (`settings`): left out |
| **2026.9.x** | **imports, converted** (tested: 2026.9.4) | and: `core.device_registry` 3.4 adds `child_devices`, which become ordinary device rows |
| **2026.10.x** | **imports, converted** (tested: 2026.10.4) | and: `lifx` entries are version 2 (`serial` in `data`, the serial without colons in the unique id, the device identifier and the entities' unique ids): converted to version 1. `hue` entries are 1.2 (Zigbee addresses leave the devices' `mac` connections): left at 1.2, which 2026.6.1 runs too |
| **2026.11.x** | **imports, converted** (tested: 2026.11.2) | and: `core.entity_registry` is 1.24 (a user's entity name is stored without the device's name, `compat_name` keeps the old one): names stored in full again. `met` entries are version 2 (`location` nested, `name` dropped, the device named after the place): converted to version 1. Restore-state rows gain `entity_registry_id`: found by entity id again |
| after 2026.11 | by store: refused if any store or entry has a version with no rule; otherwise imported and flagged untested | not examined |

What the conversion leaves different from the 2026.6.1 install the newer
release started from is listed, with the reason for each, in
`a_converted_install_is_the_2026_6_1_install_again`
(`crates/ha-server/tests/import.rs`). In short: split devices stay split
(new ids, one config entry each, a link through one moved to its own
part); a child device has no manufacturer, model or `via_device_id`
(2026.9 dropped them); Hue devices keep the connections 2026.10 left; the
`lifx` entry from before one entry per bulb stays removed; a `lifx`
entity's `previous_unique_id` holds what 2026.10's migration recorded; a
name 1.24 stripped keeps the capital it gained; and an alias 1.24 wrote
out stays written out.

Two things 2026.8 and later do at run time have no 2026.6.1 form, and
are reported, not converted: an action on a split device's old id reaches
all its parts there, and an action on a device reaches its child devices'
entities (`helpers/target.py`). The report names the automations, YAML
files and dashboards that use such an id (`split_devices`,
`parent_devices`, an automation's `warning`). And 2026.8 finds a device
by identifier *and config entry*, where hars finds the first row with the
identifier: the conversion lists an integration's own row first. It
refuses a registry with two rows of the integration's own for one
identifier, or two rows that each hold a copy of the other's. Other
leftovers: a copy whose original was removed (it is in `deleted_devices`)
is refused, because 2026.8 would bring the original back and hars would
attach the integration to the copy; a copy whose original is gone
altogether is found as the integration's device, and said in the report;
and a removed device comes back for whoever registers its identifier, as
in 2026.6.1.

The source is told from the destination by device and inode, not by path
(one directory can be mounted twice), and a name is told from another by
asking the destination's file system (staged files are made with
create-new; the plan checks each name against the directory's own
listing), because only it knows which names it takes for one file.

Unchanged from 2026.6.1 through 2026.11.2: `auth`,
`auth_provider.homeassistant`, `onboarding`, `core.config`,
`core.config_entries` (the store itself), the area, floor, label and
category registries, `core.labs`, `person`, `zone`, the dashboards'
stores, the frontend's settings, and the YAML loader (`util/yaml`).

The evidence is in the tests: `crates/ha-server/tests/vectors/import/` holds
one synthetic install as each of 2026.6.1, 2026.6.4, 2026.7.0, 2026.7.4,
2026.8.3, 2026.9.4, 2026.10.4 and 2026.11.2 leaves it on disk, made by
running each release's own storage code
(`scripts/vectors/import_releases.py`), and `downgrade.json` there is what
Home Assistant 2026.6.1's base bootstrap (registries, config entries,
restore state; no integration is set up) did with each one: it loads the
first four and refuses the last four, naming `core.device_registry`, the
first store it can't read. hars copies the first four byte for byte and
converts the last four; the conversion's test is the round trip back to
the 2026.6.1 install. The same script's `loads` mode runs 2026.6.1's
bootstrap on a directory hars converted (a check made by hand, not a
committed vector): it loads each of the four and writes the entity
registry back as hars wrote it, and the device registry too apart from the
fields 2026.6.1 doesn't know. The list of tested releases is `TESTED_HA`
in `crates/ha-server/src/import.rs`.

The decision is made per store, not from `.HA_VERSION`: a directory that
says it is from a later release and whose stores and entries all pass is
imported, with `ha_version_tested: false` in the report and a warning
(`ha_version_tested` is about the release line, not each patch release).
After an import, hars still behaves like 2026.6.1 and serves 2026.6's
frontend, whichever release the files came from. What the user should
expect from each release is in
[INSTALL.md](INSTALL.md#which-home-assistant-releases).

## Where new code goes

| You are adding | It goes in | Notes |
|---|---|---|
| An integration (a config entry domain) | `crates/ha-integrations/src/<domain>/` | Started from `ha_integrations::start`, listed in `SUPPORTED`. [Guide](guides/add-an-integration.md) |
| A config flow for an integration (adding it from the UI) | `crates/ha-integrations/src/<domain>/config_flow.rs`: a `flow::Handler` listed in `flow::HANDLERS`, started by `flow::Manager::setup_entry` | [Guide](guides/add-an-integration.md#4b-a-config-flow) |
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
- **Integration stop hooks.** Integrations end with the runtime, except
  that a `switchbot_cloud` or `hue` entry's task is aborted when the entry
  is removed. If
  one needs to say goodbye to a device, it should use `on_final_write` or a
  new hook; removing or reloading the other integrations' entries without a
  restart needs one too.

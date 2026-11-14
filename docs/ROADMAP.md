# Roadmap

## North star: the MVP

**MVP = `hars` replaces the maintainer's running Home Assistant with no visible
loss of function**: the same dashboards (rendered by the unchanged official HA
frontend), the same automations firing at the same moments, the same devices
controllable, and the iPhone companion app still logged in and reporting, all
within the resource budgets below.

Anything not in the inventory is **post-MVP**. When in doubt, cut it.

### Compatibility targets (pinned)

| Thing | Version |
|---|---|
| Home Assistant core (behaviour + `.storage` shapes) | **2026.6.1** |
| Frontend bundle served by `hars` | **`home-assistant-frontend` 20260527.4** (the version 2026.6.1 ships) |
| iOS companion app | **2026.9.0**, iOS 27 |
| Time zone / units | Europe/London (DST transitions matter), metric |

Upgrading any of these needs browser and iPhone regression runs first.

### Resource budgets (MVP gates)

Measured on Linux (container) with the full inventory loaded, after a 5-min
warm-up and a 30-min quiet run. The harness lands in M0 and every milestone
records its numbers in `docs/PERF.md`. A regression past a gate blocks the
milestone.

| Metric | MVP gate |
|---|---|
| Idle RSS (full inventory, 1 dashboard open) | **≤ 20 MiB** |
| Idle CPU | **≤ 0.1 %** of one core |
| Process wakeups when quiet | **≤ 1 /s** average |
| Stripped release binary | **≤ 15 MiB** (frontend served from disk, not embedded) |
| Cold start to serving `/api/` | **≤ 1 s** |
| Recorder disk writes when quiet | batched; no more than 1 fsync per 5 s |

---

## MVP scope inventory

Taken from the live install on 2026-11-13. This is the acceptance list. Entity
ids are kept as they are, and the importer carries them over.

### Integrations

| Integration | Entities | Transport | Endpoint/credential |
|---|---|---|---|
| `hue` | 8 lights (3 filament bulbs, Hue Go, colour lamp, room `grouped_light`s), 4 scenes, motion + illuminance + temperature + battery, motion/light-sensor enable switches, 1 bridge `behavior_instance` switch | Local HTTPS CLIP v2 + SSE (push) | host + app key imported |
| `lifx` | Color 1000 kitchen light + identify/restart buttons | LAN UDP :56700 | host imported |
| `switchbot_cloud` | Bot (`button.towel_switch` + battery), DIY IR air conditioner (`climate` + `switch`, assumed state), Hub 2 temp/humidity | Cloud HTTPS v1.1, **polling-only** (600 s, as HA) | token + secret imported |
| `met` | `weather.forecast_home` | HTTPS api.met.no | home location |
| `sun` | `sun.sun` + 6 `sensor.sun_next_*` | Local calculation | home location |
| `mobile_app` | iPhone `device_tracker.phone`, ~15 sensors, 3 kiosk binary sensors | App → `hars` webhooks, **encrypted** (`supports_encryption`) | registration imported |
| `person` / `zone` | `person.*`, `zone.home` | Built-in | imported |

### Dashboards ("widgets")

- **Overview**: the auto-generated default dashboard (HA returns
  `config_not_found` and the frontend generates it)
- **Bedroom** (`/bedroom-dashboard`): `sections` view → `grid` sections →
  `tile` cards (`light-brightness` feature, `toggle` / `more-info` actions) and
  `button` cards (`call-service` → `scene.turn_on`)
- **Map** (`/map`): the `map` strategy, unchanged (person + tracker, zone radius)
- **More-info** for every inventory entity: light brightness/colour/temperature
  controls, climate modes/setpoint, sensor history graph, weather forecast

### Automations (YAML, no templates)

- Triggers: `state` (`to`, `for`), `numeric_state` (`below`), `time` (`at`),
  `sun.sun` state, trigger `id`
- Conditions: `state`, `numeric_state`, `time` (`after` inclusive / `before`
  exclusive, windows crossing midnight), `trigger`
- Actions: service call (`target.entity_id` single/list, `data`),
  `choose`/`default`, `wait_for_trigger` (with `for`)
- Modes: `single`, `restart`
- Light data: `brightness_pct`, `color_temp_kelvin`, `transition`

### Explicitly out of MVP

- `notify.phone` / push notifications: never called in the 10 days of recorded history
- Remote access: none today (no external URL, no Nabu Casa); LAN only
- SwitchBot webhooks: these need an external URL
- Onboarding defaults that are installed but unused: `shopping_list`/`todo`,
  `google_translate` TTS, `radio_browser`, `go2rtc`, `backup`, `analytics`, Assist
- Blueprints, scripts/scenes YAML (both empty), Jinja templates
- Config flows for *adding* integrations, the UI automation editor, dashboard
  editing beyond `lovelace/config/save`, long-term statistics, the add-on store, HACS
- Importing HA's history database: `hars` history starts fresh at cutover

---

## Test oracle

HA's own recorder DB on the live install holds 10 days of real data: about
20.7k state rows, 1,128 `automation_triggered` and 1,387 `call_service` events.
This is the reference for "behaves like HA":

1. **Automation replay**: feed recorded state changes into `hars` with a
   simulated clock and compare the service calls it issues (order, timing within
   1 s, non-firings) with HA's recorded `call_service` events.
2. **Frontend traces**: capture sanitised WebSocket traces from the live HA for
   a cold load, each dashboard, every more-info dialog and control, reconnect,
   and app backgrounding. Classify each command as required, supported-empty or
   expected-error, then replay against `hars`.
3. **Entity fixtures**: sanitised attribute snapshots of every inventory
   entity (capability flags, colour/temperature limits, units, device classes).

Fixtures are sanitised (no tokens, coordinates rounded) before they are committed.

---

## Milestones

### M0: Core (done)
- [x] Cargo workspace, CI, perf-tuned release profile
- [x] `EntityId`, `State`, `StateMachine`, `EventBus`, `Hass`
- [x] REST: `/api/`, `/api/states`, `/api/states/{entity_id}`
- [x] Injectable clock (wall + monotonic) used by states, timers and sun maths,
      so tests run on simulated time
- [x] Service registry: `call(domain, service, data, target)`, `call_service`
      event, `supports_response`, `entity_id` target resolution; REST
      `GET /api/services` + `POST /api/services/{domain}/{service}`
- [x] Per-service data schemas, checked before `call_service` fires (`Invalid` → 400)
- [x] `Context` (id, parent_id, user_id) on states, events and service calls
- [x] Lossless, ordered state-change delivery for internal consumers
      (automations, recorder). The lossy broadcast bus is only for API
      subscribers, which resync from a snapshot when they lag
- [x] Perf harness (`docs/PERF.md`): RSS/PSS, CPU time, wakeups, binary size;
      `criterion` benches for state writes and event fan-out
- [x] Turn off transparent huge pages for mimalloc by default (8 MiB of
      13 MiB idle RSS on a THP=`always` Linux host; see `docs/PERF.md`)
- [x] Graceful shutdown on SIGTERM + lifecycle events
      (`homeassistant_start`/`started`/`stop`)

### M1: Import, registries, config & restore
- [ ] Versioned, idempotent **importer** that reads a consistent copy of an HA
      2026.6.1 config dir and writes a machine-readable report of anything
      unsupported or unresolved. It imports:
  - [ ] Entity/device/area registries (unique-id mapping, disabled/hidden flags)
  - [ ] `core.config` (location, elevation, units, time zone), `person`, zones
  - [ ] Lovelace dashboards and the dashboards list (`url_path` identity)
  - [ ] Auth: users, bcrypt hashes, **refresh tokens with ids, client ids and
        `jwt_key`s**, so the iPhone and browser sessions survive
  - [ ] Config entries: Hue host/key, LIFX host, SwitchBot token/secret,
        mobile_app registration (webhook id, secret, push data)
- [ ] `configuration.yaml`: `!include`, `!secret`, `!include_dir_merge_named`;
      unknown keys log a warning, while unsupported **automation** syntax fails validation
- [ ] Restore state on restart: automation on/off + `last_triggered`, IR climate
      target temperature/mode/fan (`assumed_state`). Pending waits are not restored.

### M2: Auth + HTTP/WebSocket API for the official frontend
- [x] Bootstrap HTML + static frontend 20260527.4 from disk (precompressed
      `.br`/`.gz`, HA's 31-day `public` cache headers; HA sends no
      `immutable`), `/manifest.json`, `/auth/authorize`. The image ships the
      wheel's `hass_frontend` minus source maps (234 MB, ES5 build kept);
      the binary grew 2.08 → 2.33 MiB
- [x] Auth: `/auth/providers`, `/auth/login_flow` + `/auth/login_flow/{flow_id}`,
      `/auth/token` (`authorization_code` + `refresh_token` + revoke, as
      urlencoded **or multipart**: the frontend sends `FormData`), client id /
      redirect validation (no outbound `<link rel=redirect_uri>` fetch), HS256
      JWT access tokens, `Bearer` on every `/api/*`. State lives in HA's own
      `.storage/{auth, auth_provider.homeassistant, onboarding}` files.
      `hars auth add` creates the owner until onboarding UI lands; binary
      2.33 → 2.64 MiB
- [ ] Onboarding UI: `/api/onboarding/users` and the `core_config`,
      `analytics` and `integration` steps (needs WS); a fresh install
      redirects to `/onboarding.html` until then
- [ ] CORS for `/auth/token` and `/auth/revoke` (HA allows any origin)
- [ ] REST `/api/config` (truthful `components` list, version, units, location)
- [ ] WS core: `auth`, `ping`, `supported_features` (**coalescing only once it
      is negotiated**), `subscribe_events`/`unsubscribe_events`, `get_states`,
      `get_config`, `get_services`, `call_service`, `get_panels`,
      `auth/current_user`
- [ ] WS `subscribe_entities`: compressed `a`/`c`/`r` diffs, attribute
      removals, context; subscribe and snapshot taken atomically; a lagged
      client gets a fresh snapshot
- [ ] WS frontend: `frontend/get_themes`, `frontend/get_translations`,
      `frontend/subscribe_user_data`, `frontend/subscribe_system_data`,
      `frontend/set_user_data`, `manifest/list`, `brands/access_token`
- [ ] WS registries: `config/entity_registry/list_for_display`,
      `config/{device,area,floor,label}_registry/list`, and their `*_updated` events
- [ ] WS lovelace: `lovelace/info`, `lovelace/dashboards/list`,
      `lovelace/config` (`config_not_found` for Overview),
      `lovelace/config/save`, `lovelace/resources`
- [ ] One serialised `Arc<str>` per state change, shared by all connections
- [ ] Acceptance: replaying the frontend trace passes against `hars` with
      fixture entities (protocol smoke test; real data comes in M4/M5)

### M3: Automations
- [ ] `automation.*` entities, `trigger`/`turn_on`/`turn_off`/`toggle`/`reload`
- [ ] Inventory triggers/conditions/actions/modes with HA 2026.6.1 semantics:
  - [ ] `numeric_state` fires on crossing only; covers threshold equality,
        already-below at startup, and recovery from `unknown`/`unavailable`
  - [ ] `state` `to`/`for`: attribute-only updates do not retrigger or reset
        `for`; leaving and re-entering restarts the duration
  - [ ] `wait_for_trigger` subscribes at the moment it starts, so an
        already-matching state does not complete it
  - [ ] `restart` cancels the running instance only after the new run passes
        its conditions; `single` drops overlapping runs with a warning
  - [ ] `time` windows across midnight, both London DST transitions, clock jumps
- [ ] Timers via `tokio::time::sleep_until` (no custom wheel until benchmarks
      demand one); nothing polls
- [ ] Acceptance: the automation replay oracle matches HA on all 4 automations
      over 10 days of recorded data

### M4: Integrations (each behind a Cargo feature, numbers recorded when added)
- [ ] `sun` (solar elevation/azimuth + `sensor.sun_next_*`, HA-matched maths),
      `person`, `zone.home`
- [ ] `met`: identifying User-Agent, ≤ 4-decimal coordinates, honour
      `Expires`/`If-Modified-Since`, jitter, backoff, serve the stale cache on
      failure; `weather/subscribe_forecast`; attribution
- [ ] `hue` v2: full snapshot on connect, then SSE partial-resource merge;
      reconcile after reconnect; bounded 429/503 retry; mDNS rediscovery if the
      imported host moves; link-button re-pair only if the key is rejected.
      Lights (on/off, brightness, mirek/kelvin, xy, transition), grouped lights,
      scenes, motion/illuminance/temperature/battery, enable switches, the
      `behavior_instance` switch
- [ ] `lifx`: imported host + broadcast rediscovery, source/sequence matching,
      bounded retries, offline → `unavailable`, re-read state after a
      transition (the ack is not the final state); on/off/brightness/kelvin/HSBK;
      identify/restart buttons
- [ ] `switchbot_cloud`: poll every 600 s (~430 req/day of the 10k/day quota,
      leaving headroom while HA and `hars` run side by side); Bot press is never
      auto-retried; IR climate + switch with assumed state; Hub 2 sensors; Bot battery
- [ ] Acceptance: every inventory entity matches its HA fixture attributes, and
      every frontend control (light colour/temp, climate mode/setpoint) works

### M5: iPhone companion app
- [ ] Imported registration works unchanged: `/api/webhook/{webhook_id}` with
      **encryption** (libsodium secretbox, `no_legacy_encryption`), encrypted
      response envelopes
- [ ] Webhook commands: `register_sensor`, `update_sensor_states`,
      `update_location`, `get_config` (entity enabled-state sync), `get_zones`,
      `update_registration`, `call_service`, `fire_event`
- [ ] `/api/mobile_app/registrations` for fresh registrations (re-pair fallback)
- [ ] `device_tracker.phone` → `person` → Map dashboard updates live
- [ ] Acceptance: the existing app session keeps working after cutover without
      logging in again; sensors and location update over LAN; dashboards and
      controls work in the app

### M6: History
- [ ] Recorder: SQLite (WAL), bounded writer queue, max batch delay 5 s,
      10-day **and** byte-size retention, purge + checkpoint, sane behaviour on a full disk
- [ ] Stop waits for the final-write flush: the lifecycle stages
      (`homeassistant_final_write`) need a completion barrier, as HA's
      `async_stop` waits for each stage's handlers; today they only fire events
- [ ] Crash test: kill -9 loses at most one batch window and never corrupts the DB
- [ ] WS `history/stream`, `logbook/event_stream`,
      `sensor/numeric_device_classes`, `recorder/get_statistics_metadata` and
      `recorder/statistics_during_period` (empty statistics, so the frontend
      falls back to history), `recorder/info`
- [ ] Acceptance: more-info history graphs and the Map trail render

### M7: Cutover
- [ ] **Shadow mode**: HA stays authoritative; `hars` runs alongside it with
      service dispatch suppressed and logs the calls it *would* make, and a
      diff tool compares them with HA's `call_service` events. The phone stays
      on HA. One week clean.
- [x] Packaging: static musl binary in a `FROM scratch` image with CA roots,
      tzdata, a persistent volume and SIGTERM handling
- [ ] Promote: stop HA, run the importer on a fresh copy, start `hars` on :8123.
      Rollback = start the untouched HA snapshot again.
- [ ] Publish the RSS / CPU / startup / wakeup numbers against the HA instance
      being replaced

## Post-MVP
- Push notifications (`notify.phone` via the relay, local push channel)
- Remote access, SwitchBot webhooks
- Jinja templates (`minijinja`), scripts, scenes YAML, blueprints
- Config flows for adding integrations; UI automation/script editor
- Long-term statistics
- MQTT, ESPHome native API, zigbee2mqtt; `input_*` / `template` helpers, `todo`, TTS
- Synthetic-scale HA vs `hars` benchmark harness

## Decision log
- **2026-11-13**: MVP scoped to the live install. Two Sol reviews (frontend/API/app;
  integrations/automation/perf) were folded in. Key calls: pin HA 2026.6.1 +
  frontend 20260527.4; HA's recorder DB is the test oracle; automations get
  lossless delivery; import auth sessions; mobile encryption is required;
  notify, remote access and history import are deferred; cutover is via a
  shadow-mode week; budget is ≤ 20 MiB idle RSS.

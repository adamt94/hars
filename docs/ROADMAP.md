# Roadmap

What hars is building towards, in milestones. Contributors pick work from
the unticked items; a pull request that finishes one ticks its box. Why the
plan looks like this is in [ADR 0009](adr/0009-mvp-reference-install.md).

## The MVP

**The MVP is a Home Assistant 2026.6.1 install running on hars with no
visible loss of function**: the same dashboards (rendered by the unchanged
official frontend), the same automations firing at the same moments, the
same devices controllable, and the iOS companion app still logged in and
reporting, all within the [resource budgets](PERF.md#budgets).

The install it is measured against is a real one, called **the reference
install** below. Its inventory is the acceptance list. Anything not in it is
**post-MVP**; when in doubt, cut it.

A few acceptance checks need the reference install's own data: its devices,
its phone and its recorded history (the `HARS_AUTOMATION_REPLAY` run, the
checks against the real bridge, the companion-app runs and the shadow-mode
week). That data is personal and stays out of the repository, so the
maintainer runs those checks. If your change needs one, say so in the pull
request.

The pinned versions hars matches are in
[ARCHITECTURE.md](ARCHITECTURE.md#home-assistant-compatibility). Moving one
needs the browser and companion-app regression runs first.

---

## MVP scope inventory

The reference install's inventory, taken on 2026-11-13. Entity ids are kept
as they are, and the importer carries them over.

### Integrations

| Integration | Entities | Transport | Endpoint/credential |
|---|---|---|---|
| `hue` | 8 lights (filament bulbs, a Hue Go, a colour lamp, room `grouped_light`s), 4 scenes, motion + illuminance + temperature + battery sensors, motion/light-sensor enable switches, 1 bridge `behavior_instance` switch | Local HTTPS CLIP v2 + SSE (push) | host + app key imported |
| `lifx` | 1 colour bulb + identify/restart buttons | LAN UDP :56700 | host imported |
| `switchbot_cloud` | Bot (a `button` + battery), DIY IR air conditioner (`climate` + `switch`, assumed state), Hub 2 temperature/humidity | Cloud HTTPS v1.1, **polling-only** (600 s, as HA) | token + secret imported |
| `met` | `weather.forecast_home` | HTTPS api.met.no | home location |
| `sun` | `sun.sun` + 6 `sensor.sun_next_*` | Local calculation | home location |
| `mobile_app` | an iPhone: a `device_tracker`, 15 sensors (+1 disabled), 2 kiosk binary sensors (+1 disabled camera sensor), a `notify` service | App → `hars` webhooks, **encrypted** (`supports_encryption`) | registration imported |
| `person` / `zone` | `person.*`, `zone.home` | Built-in | imported |

### Dashboards

- **Overview**: the auto-generated default dashboard (HA returns
  `config_not_found` and the frontend generates it)
- **A sections dashboard**: `sections` view → `grid` sections → `tile`
  cards (`light-brightness` feature, `toggle` / `more-info` actions) and
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

- Push notifications through `notify`: never called in the reference
  install's 10 days of recorded history
- Remote access (the reference install is LAN only)
- SwitchBot webhooks: these need an external URL
- Onboarding defaults that are installed but unused: `shopping_list`/`todo`,
  `google_translate` TTS, `radio_browser`, `go2rtc`, `backup`, `analytics`, Assist
- Blueprints, scripts/scenes YAML, Jinja templates
- Config flows for *adding* integrations, the UI automation editor, dashboard
  editing beyond `lovelace/config/save`, long-term statistics, the add-on store, HACS
- Importing HA's history database: `hars` history starts fresh at cutover

---

## Test oracle

HA's own recorder database on the reference install holds 10 days of real
data: about 20.7k state rows, 1,128 `automation_triggered` and 1,387
`call_service` events. This is the reference for "behaves like HA":

1. **Automation replay**: feed recorded state changes into `hars` with a
   simulated clock and compare the service calls it issues (order, timing within
   1 s, non-firings) with HA's recorded `call_service` events.
2. **Frontend traces**: capture sanitised WebSocket traces from the reference HA for
   a cold load, each dashboard, every more-info dialog and control, reconnect,
   and app backgrounding. Classify each command as required, supported-empty or
   expected-error, then replay against `hars`.
3. **Entity fixtures**: sanitised attribute snapshots of every inventory
   entity (capability flags, colour/temperature limits, units, device classes).

4. **Vectors**: inputs run through HA's own code, with what HA produced saved
   next to the Rust module that must reproduce it. A new vector set commits
   the script that ran HA in `scripts/vectors/` so it can be remade (see its
   README); the sets made before that rule are the backlog item below.

Fixtures are sanitised (no tokens, coordinates rounded) before they are committed.

- [ ] Generators for the vector sets made before scripts were kept (the
      table in `scripts/vectors/README.md`), each rerun to confirm it
      reproduces what is committed. Needed before the pinned HA version moves.

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
- [x] CI gate on the resource budgets (`PERF_GATE=1 scripts/perf.sh` on the
      image's binary) and on the crates' dependency direction
- [x] Turn off transparent huge pages for mimalloc by default (8 MiB of
      13 MiB idle RSS on a THP=`always` Linux host; see
      [ADR 0007](adr/0007-transparent-huge-pages-off.md))
- [x] Graceful shutdown on SIGTERM + lifecycle events
      (`homeassistant_start`/`started`/`stop`)

### M1: Import, registries, config & restore
- [x] Versioned, idempotent **importer** (`hars import`) that reads a consistent copy of an HA
      2026.6.1 config dir and writes a machine-readable report of anything
      unsupported or unresolved (`import-report.json`). Stores `hars` reads
      are validated with its own loaders and copied byte for byte; a re-run
      that would replace state `hars` changed needs `--force`. It imports:
  - [x] Entity/device/area/floor/label registries (HA's files as they are:
        unique ids, disabled/hidden flags, names), read by `ha-registry`
  - [x] `core.config` (location, elevation, units, time zone)
  - [x] `person`, zones (`.storage/person`, `.storage/zone`, `zone.home` from
        `core.config`); people restore their last state, then follow their
        device trackers from `homeassistant_start`; zones count them. HA's
        Vincenty `distance` and `async_in_zones`; 20 `location.json` steps match
  - [ ] Full Unicode transliteration in `slugify` (CJK, Cyrillic) for new
        entity ids; imported ones come from the registry
  - [x] Lovelace dashboards and the dashboards list (`url_path` identity),
        resources; storage mode (YAML dashboards come with `configuration.yaml`)
  - [x] Auth: users, bcrypt hashes, **refresh tokens with ids, client ids and
        `jwt_key`s**, so the iPhone and browser sessions survive (tested: an
        imported HA refresh token refreshes against `hars`); frontend user
        and system settings
  - [x] Config entries: `core.config_entries` imported as is (0600); the
        report marks each entry's integration as supported or not. Hue host/key,
        LIFX host, SwitchBot token/secret and mobile_app registration are used
        by the implemented integrations in M4/M5
- [x] `configuration.yaml` loaded as HA loads it: `!include`, the
      `!include_dir_*` tags, `!secret`, `!env_var`; unsupported
      **automation** syntax makes that automation `unavailable`
- [ ] Unknown top-level `configuration.yaml` keys log a warning
- [x] Restore state: `core.restore_state` (imported) loaded at start and
      dumped as HA dumps it (at start, every 15 min, at stop; 7-day expiry);
      entities register as restorable (hue scenes restore their last
      activation); registered entities nobody provides are `unavailable`
      with `restored: true`, as HA writes them at start
- [x] Restore for automations (on/off, `last_triggered`; M3) and the IR
      climate (target temperature/mode/fan, `assumed_state`; M4). Pending
      waits are not restored.

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
      JWT access tokens, `Bearer` on protected REST routes. WebSocket uses
      protocol auth; webhooks use secret ids; onboarding status is anonymous
      (see [security policy](../SECURITY.md)). State lives in HA's own
      `.storage/{auth, auth_provider.homeassistant, onboarding}` files.
      `hars auth add` creates the owner until onboarding UI lands; binary
      2.33 → 2.64 MiB
- [ ] Onboarding UI: `/api/onboarding/users` and the `core_config`,
      `analytics` and `integration` steps (needs WS); a fresh install
      redirects to `/onboarding.html` until then
- [ ] CORS for `/auth/token` and `/auth/revoke` (HA allows any origin)
- [ ] Permissions beyond the built-in groups: custom group policies (users
      with only custom groups are refused for now), per-entity filtering, and
      admin-only services once services exist that need it
- [ ] REST bodies parsed as JSON whatever the `Content-Type`, as HA does
      (today a missing `application/json` is a 415)
- [x] REST `/api/config` (truthful `components` list, version, units, location),
      from HA's `.storage/core.config` (1.4)
- [x] WS core: `auth`, `ping`, `supported_features` (**coalescing only once it
      is negotiated**), `subscribe_events`/`unsubscribe_events`, `get_states`,
      `get_config`, `get_services`, `call_service`, `get_panels`,
      `auth/current_user`; HA's id rules and error codes, the 55 s heartbeat,
      4096-message cut-off, and close on token revocation. Binary 2.70 → 3.08 MiB
- [x] WS `subscribe_entities`: compressed `a`/`c`/`r` diffs, attribute
      removals, context; subscribe and snapshot taken atomically. A client
      that falls behind is disconnected, as in HA, and gets a fresh snapshot
      when the frontend reconnects
- [x] WS frontend: `frontend/{get,set,subscribe}_{user,system}_data` in HA's
      `.storage/frontend.*` files, `frontend/get_themes`,
      `frontend/get_translations` and `frontend/get_icons` (bundled English
      translations and icons, described below), `persistent_notification/subscribe`,
      `repairs/list_issues`; the frontend's cold load gets every answer it
      waits for (test replays it)
- [x] Bundle translations and icons (`frontend/get_translations`,
      `frontend/get_icons`): HA 2026.6.1's generated English strings and
      `icons.json` for the 37 integrations `hars` runs or will run
      (`scripts/gen-i18n.py` from HA's wheel), compiled in as ready-made JSON
      pieces, so an answer is joined, not parsed. Other languages get
      English, HA's fallback
- [x] Labs: `labs/list`, `labs/subscribe`, `labs/update` (admin) with HA's
      generated preview features, `.storage/core.labs` (imported), and
      `labs_updated`; `system_log.write`/`clear` (the frontend reports its
      errors there; they go to the log)
- [x] Brands: `brands/access_token` (two live tokens, rotated every 30
      min) and `/api/brands/{integration,hardware}/…` proxied from HA's CDN
      with HA's disk cache (`.cache/brands`, 404s cached, 30-day background
      refresh, the placeholder image)
- [x] `manifest/list`, `manifest/get`: HA's manifests (generated from the
      wheel by `scripts/gen-manifests.py`) for what hars runs, `default_config`
      and common config-entry domains
- [x] `system_log/list` (admin): the last 50 warnings and errors from a
      `tracing` layer, deduplicated by logger and call site as HA's
      `DedupStore` (count, up to 5 messages, newest first); `clear` empties it
- [ ] Other languages
- [x] WS registries: `config/entity_registry/list_for_display` (HA's display
      keys, legacy device-name prefix stripping),
      `config/{device,area,floor,label}_registry/list`
- [x] Registry `*_updated` events, once registries change at runtime:
      `entity_registry_updated` (create/update/remove) and
      `device_registry_updated` (create/update), with HA's delayed saves (10 s
      after the last change, 180 s before start)
- [x] WS lovelace: `lovelace/info`, `lovelace/dashboards/list`,
      `lovelace/config` (`config_not_found` for Overview),
      `lovelace/config/save` + `/delete` (admin, fire `lovelace_updated`),
      `lovelace/resources`; each dashboard is a panel and its URL loads the app
- [ ] `lovelace/dashboards/create|update|delete`, resource editing
- [ ] One serialised `Arc<str>` per state change, shared by all connections
- [ ] Acceptance: replaying the frontend trace passes against `hars` with
      fixture entities (protocol smoke test; real data comes in M4/M5)

### M3: Automations
- [x] Startup as HA's bootstrap: `homeassistant_start` waits for every
      integration's first setup attempt (its first writes, or a failure it
      retries in the background; at most 300 s), registered entities nobody
      provides are written `unavailable` at `start`, then `started`, so
      automations attach to real states
- [x] `automation.*` entities (registry ids, HA's attributes, restore with
      `initial_state`, invalid configs `unavailable`), `trigger`/`turn_on`/
      `turn_off`/`toggle`; loaded from `configuration.yaml` (`ha-automation`)
- [x] `hars import` copies the YAML (`configuration.yaml`, every file its
      `!include*` tags reach, `secrets.yaml` only if `!secret` is used,
      owner-only), validates it with hars's loader and reports each
      automation as supported or `unavailable` with the reason
- [x] The editor's read side: `GET /api/config/automation/config/<id>`
      (admin; `automations.yaml` read with HA's loader, the entry in file key
      order, `Resource not found`) and WS `automation/config` (the raw entry);
      saving and deleting answer that hars doesn't support it yet
- [ ] `automation.reload`, editor saves, traces
- [x] Inventory triggers/conditions/actions/modes with HA 2026.6.1 semantics,
      matched against HA's own engine on 36 scenario vectors:
  - [x] `numeric_state` fires on crossing only; covers threshold equality,
        already-below at startup, and recovery from `unknown`/`unavailable`
  - [x] `state` `to`/`for`: attribute-only updates do not retrigger or reset
        `for`; leaving and re-entering restarts the duration
  - [x] `wait_for_trigger` subscribes at the moment it starts, so an
        already-matching state does not complete it
  - [x] `restart` cancels the running instance only after the new run passes
        its conditions; `single` drops overlapping runs with a warning
  - [x] `time` windows across midnight, both London DST transitions, clock jumps
- [x] Timers via `tokio::time::sleep_until` (no custom wheel until benchmarks
      demand one); nothing polls
- [x] Acceptance: the automation replay oracle matches HA on all 4 automations
      over 10 days of recorded data (1,345 triggers, 1,627 light calls; the
      vector is the reference install's motion timeline, which is personal
      data, so it's kept out of the repo: `HARS_AUTOMATION_REPLAY` runs it)

### M4: Integrations (binary and RSS cost recorded in `docs/perf/RESULTS.md` as each lands)
- [x] `sun` (solar elevation/azimuth + `sensor.sun_next_*`, HA-matched maths:
      a port of astral 2.2 that matches HA to the microsecond on 231 event
      times), `person`, `zone.home`; started from `core.config_entries`
      (imported) or by default on a fresh install
- [x] `met`: a port of pyMetno 0.13.0 and HA's `met`/`weather` code that
      matches HA value for value (Python rounding, summation order, int/float
      kinds, wall-clock DST slots, days split in the process's zone as
      pyMetno does) on 4 recorded scenarios; `weather.forecast_home` with HA's
      attributes and unit options; `weather/subscribe_forecast`,
      `weather/convertible_units`, `weather.get_forecasts`. HA's schedule
      (55–64 min + jitter, setup retries 5 s→10 min, unavailable on failure
      with the last forecast kept for readers). Deliberate differences: the
      public api.met.no host with `hars`'s own User-Agent, and
      `If-Modified-Since` (a `304` recomputes from the kept body, which is what
      HA gets). `Expires` isn't used to fetch more often: HA's cadence is
      observable. Not yet: options-flow edits, removing legacy `*-hourly`
      registry entries, the restored `unavailable` state during setup retries
- [x] The `light` entity component, shared by `hue` and `lifx`: HA's
      `util/color` maths and `light.turn_on`/`turn_off`/`toggle` processing
      (schema with exclusive groups and clamps, profiles including
      `light_profiles.csv` defaults, colour names, brightness steps, every
      colour-space conversion, filtering by capability), capability and state
      attributes; matched against HA's own code on 348 mireks, 4536 kelvins,
      600 xy points and 48 service calls
- [x] `hue` v2, lights: full snapshot, then the event stream merged as
      aiohue merges it (absent and `null` keep, objects merge, lists
      replace); HA's writes per item in HA's order (no coalescing); lights
      and room/zone groups with HA's state, attributes and group
      carry-overs; `light.*` services as the exact aiohue request bodies,
      429/503 retried 25 times, HA's error messages; HA's availability
      (zigbee connectivity, `ignore_availability`); reconnect at `2n` s and,
      unlike HA, a full reconcile after every reconnect, removals included;
      TCP keepalive instead of aiohue's geofence-renaming keepalive. Matched
      against HA's own code on the real bridge: every light entity, 14
      event-stream scenarios, 50 service calls
- [x] `hue`: sensors (temperature, illuminance and grouped illuminance,
      battery, zigbee connectivity), binary sensors (motion, camera and
      grouped motion, contact, tamper, entertainment), the enable and
      behaviour-instance switches, scenes and smart scenes, through new
      generic `switch` and `scene` components; `scene.turn_on`,
      `hue.activate_scene`, `hue.hue_activate_scene`; HA's fan-out order
      across platforms. Matched against HA's code on the real bridge: all 28
      entities (19 with a state), 14 event-stream scenarios, 59 service calls
- [x] Shared entity components (one per domain, services registered on
      first use, so platforms don't replace each other's handlers); `button`
      (press time written first, restore) and `climate` (HA's ten services,
      schemas, required features and `service_not_supported`, mode and range
      validation, `display_temp`); `service_validation_error`, translated
      `home_assistant_error` and `unknown_error` replies
- [ ] `hue`: button/rotary/bell events and `hue_event`, the motion-aware
      sensor; mDNS
      rediscovery if the imported host moves; user-initiated re-pairing if
      the key is rejected
- [x] `lifx`: one UDP socket per bulb, aiolifx's framing and matching
      (sequence mod 128, source, expected type), HA's retry layer (5 tries,
      a new message each; 3 s apart for commands, 18 s for polls; a late
      reply to any try counts), a `LightGet` every 10 s, unavailable after a
      poll's 90 s of silence; HA's `set_state` (cached-power branches,
      waveform-optional colour, 0.3 s settle, the debounced and
      end-of-transition re-reads); identify/restart buttons; broadcast
      rediscovery that reconnects. Matched against HA's own code: 27 light
      service vectors byte for byte, the real bulb's replies, buttons,
      timeouts. Checked against the real bulb with Get messages only
- [ ] `lifx`: the `lifx.*` services (effects, `set_state`, themes), the
      RSSI sensor, persisting a rediscovered host
- [x] `switchbot_cloud`: the library's signing, envelope and error classes;
      HA's setup classification and retry ladder; Hub 2 and Bot polled every
      600 s (HA stops polling the Bot once its webhook is set; `hars` never
      touches the webhook endpoints, so it keeps polling: ~290 req/day); the
      Bot's button (press never retried, time written first) and battery; the
      IR climate (`assumed_state`, `setAll` maths, restore) and its switch
      (`unknown` until commanded; HA doesn't restore it); HA's float state
      format. Matched against HA's own code on the real GET responses: all 6
      entities, the 40-call service sequence, command errors, polling,
      setup failures, restore cases; checked once against the real API
- [ ] `switchbot_cloud`: Bots in switch mode and the other device types
- [ ] Acceptance: every inventory entity matches its HA fixture attributes, and
      every frontend control (light colour/temp, climate mode/setpoint) works

### M5: iPhone companion app
- [x] Imported registration works unchanged: `/api/webhook/{webhook_id}` with
      **encryption** (libsodium secretbox, `no_legacy_encryption`), encrypted
      response envelopes; HA's 29 transport cases match
- [x] Webhook commands: `register_sensor`, `update_sensor_states`,
      `update_location`, `get_config` (entity enabled-state sync), `get_zones`,
      `update_registration`, `call_service`, `fire_event`. Sensors and binary
      sensors restore as HA's `RestoreSensor` (17 of `startup.json`); HA's
      `sensors.json` (updates, registrations, re-registrations,
      disable/enable) and all 20 `location.json` tracker steps match
  - [x] persist entry changes (`update_registration`, `no_legacy_encryption`,
        `enable_encryption`) to `core.config_entries` (1 s after, HA's layout,
        mode 0600), and `update_registration`'s device update
- [x] `/api/mobile_app/registrations` for fresh registrations (re-pair fallback):
      HA's flow (tracker row, the user's person, a re-pair replaces the old
      entry and saves its webhook id as deleted); plus the push-channel WS stubs
- [x] WS `subscribe_entities` `include`/`exclude` filters (HA's `entityfilter`
      cases 1-6, `fnmatch` globs, `cv.entity_ids`): the app subscribes to
      zones and persons only
- [ ] The phone's `device_tracker` → `person` → Map dashboard updates live
      (tracker → person → zone done and vectored; the live Map check is left
      for cutover)
- [ ] Acceptance: the existing app session keeps working after cutover without
      logging in again; sensors and location update over LAN; dashboards and
      controls work in the app

### M6: History
- [x] Recorder: an append-only segment log (`ha-recorder`, decision D7,
      [ADR 0003](adr/0003-recorder-segment-log.md)) in
      place of SQLite: bounded writer queue, max batch delay 5 s, 10-day
      **and** byte-size retention (nightly purge at 04:12), sane behaviour on
      a full disk
- [x] Recording: states with HA's recorded attributes and the logbook's
      `old_state_id` rule, the logbook's events, write-time `context_*`
      fields (D2–D4); started before anything writes states, and drained at
      final write
- [x] Stop waits for the final-write flush: `finish_stop` awaits the
      `homeassistant_final_write` (60 s) and `homeassistant_close` (30 s)
      handlers, as HA's `async_stop` does
- [x] A lossless feed for the recorder: state changes and the logbook's event
      types in the bus's order (`EventBus::subscribe_lossless`); HA's
      unrecorded-attribute sets by domain, plus `sun.sun`'s own
- [x] Crash test: `kill -9` of a writer (a child process, killed after six
      different numbers of commits and at a jittered moment) loses nothing
      that was committed, and what survives is the rows in order with no gap
      and no corrupt row (`crates/ha-recorder/tests/crash.rs`); how a torn
      tail is cut is the store's recovery tests. A process kill, not power
      loss: the store `fdatasync`s when a segment closes
- [x] WS `sensor/numeric_device_classes` and
      `sensor/device_class_convertible_units` (HA 2026.6.1's tables)
- [x] WS `recorder/info`, and the statistics calls answering empty
      (`recorder/get_statistics_metadata`, `recorder/list_statistic_ids`,
      `recorder/statistics_during_period`), so the frontend falls back to
      history (D1)
- [x] WS `history/history_during_period` and `history/stream`: HA's
      `get_significant_states` over the segment store (start-time state
      stamped at the start, significant-only and `minimal_response` rules,
      `{}` when the attributes are left out, removals as a `null` state);
      the stream sends the past, re-reads what the writer hadn't committed,
      then follows the live feed in 350 ms groups
- [x] WS `logbook/get_events` and `logbook/event_stream`: HA's rows (states,
      `automation_triggered`, start/stop; `context_*` from the write-time
      fields, D4) over the segment store, with the entity, device and
      `context_id` selections, continuous sensors and counters left out, and
      HA's checks and error codes. The stream sends the past (an unfiltered
      window over 25 h in two parts, the recent day first, with HA's
      `partial` flags), the closing message, then follows live in 350 ms
      groups: the recorder's intake hands the streams the rows it works out
      (same `context_*` fields as the stored row, one pass however many
      clients follow, nothing worked out while none does). Left out: the
      `logbook:` include/exclude YAML filters; a live row's `icon` is read
      from the recorded attributes, not the state's
- [ ] Acceptance: more-info history graphs and the Map trail render

### M7: Cutover
- [x] **Shadow mode** (`HARS_SHADOW=1`, [`docs/SHADOW.md`](SHADOW.md)): HA
      stays authoritative; `hars` runs alongside it with service handlers
      that act on devices held back (the `call_service` event still fires),
      and logs the calls it *would* make and its `automation_triggered`
      events; `scripts/shadow-diff.py` compares them with HA's recorder
      database (service, targets, data within 1 s; triggers per automation)
- [ ] Run shadow mode for a week beside the reference install: no
      differences. The phone stays on HA.
- [x] Packaging: static musl binary in a `FROM scratch` image with tzdata
      (CA roots are compiled into the binary), a persistent volume and SIGTERM
      handling
- [x] `compose.yaml` (host network, `./config`, restart policy) and a
      workflow that publishes linux/amd64 and linux/arm64 images to ghcr.io
      (`.github/workflows/publish.yml`)
- [ ] Onboarding without the command line (the M2 onboarding UI), so
      installing is `docker compose up -d` and a browser
- [ ] Promote: stop HA, run the importer on a fresh copy, start `hars` on :8123.
      Rollback = start the untouched HA snapshot again.
- [ ] Publish the RSS / CPU / startup / wakeup numbers against the HA instance
      being replaced

## Post-MVP
- Push notifications (`notify.<device>` via the relay, local push channel)
- Remote access, SwitchBot webhooks
- Jinja templates (`minijinja`), scripts, scenes YAML, blueprints
- Config flows for adding integrations; UI automation/script editor
- Long-term statistics
- MQTT, ESPHome native API, zigbee2mqtt; `input_*` / `template` helpers, `todo`, TTS
- Synthetic-scale HA vs `hars` benchmark harness

## Decisions

Decisions and their reasons are recorded as [ADRs](adr/README.md). The two
made while planning the MVP:

- **2026-11-13**: the MVP is scoped to the reference install, pins HA
  2026.6.1 and frontend 20260527.4, uses HA's recorder database as the test
  oracle, gives automations lossless delivery, imports auth sessions,
  requires app encryption, defers notifications, remote access and history
  import, cuts over after a shadow-mode week, and budgets ≤ 20 MiB idle RSS
  ([ADR 0009](adr/0009-mvp-reference-install.md),
  [0001](adr/0001-match-a-pinned-home-assistant.md),
  [0002](adr/0002-lossy-bus-lossless-feeds.md)).
- **2026-11-15**: history (M6) is an append-only segment log, not SQLite and
  never HA's database, capped at 64 MiB on top of 10 days, with the
  deliberate deviations D1 to D9
  ([ADR 0003](adr/0003-recorder-segment-log.md)).

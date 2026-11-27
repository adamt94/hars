# Vector generators

How hars's tests are checked against Home Assistant. Why:
[ADR 0008](../../docs/adr/0008-vectors-from-home-assistant.md). Where this
fits in testing: [CONTRIBUTING.md](../../CONTRIBUTING.md#tests).

A *vector* is a test case whose expected answer came from running Home
Assistant's own code: real input (a bridge's JSON, a bulb's reply, a service
call) and what HA 2026.6.1 did with it (states, attributes, the request it
sent, the error it raised). The Rust tests feed the same input to `hars` and
fail on any difference. They live next to the code they test, in
`crates/<crate>/src/<module>/vectors/` (some older sets use a single
`vectors.json` file).

The rule: a new vector set commits the script that ran HA's code to make
it, here, as `scripts/vectors/<module>.py`. Without it, vectors can't be
remade when the pinned HA version moves, when an input is added, or when
someone asks how an expected value was produced. The sets made before this
rule have no script yet; they are listed
[at the end](#vector-sets-made-before-this-rule) as a backlog.

| Generator | Writes | HA code it runs |
|---|---|---|
| `onboarding.py` | `ha-api/src/onboarding/vectors/` | HA 2026.6.1's core with `http`, `auth`, `websocket_api`, `person`, `sun`, `analytics`, `onboarding` and `config`'s `config/core/*` and `config_entries/*` commands, booted in a temporary directory: every call the onboarding pages make, the states and the `.storage` files left (aiohasupervisor 0.4.3, PyMetno 0.13.0) |
| `render_template.py` | `ha-core/src/template/vectors/` (renders, `_parse_result`, the environment's names, and Jinja's filters and tests over values of every kind) and `ha-api/src/ws/vectors/render_template.json` (whole `render_template` conversations) | HA 2026.6.1's `helpers/template` and `websocket_api` `handle_render_template` on Jinja 3.1.6, on a fresh `HomeAssistant` per case |
| `template_functions.py` | `ha-core/src/template/vectors/functions.json` (HA's state, type cast, maths and time functions, lenient and strict, with the `listeners` each render reports) and `ha-api/src/ws/vectors/render_template_states.json` (`render_template` conversations reading states and the clock), from the home in `ha-core/src/template/vectors/home.json` (an input: states, time zone, display precision, `now`) | HA 2026.6.1's `helpers/template` (Jinja 3.1.6, ciso8601 2.3.3) and `handle_render_template`, on a fresh `HomeAssistant` holding the home for each render, in Europe/London with the clock stopped at the 2026 spring-forward |
| `mobile_app_notify.py` | `ha-api/src/mobile_app/vectors/notify.json` (15 scenarios, 126 steps: service calls, the app's push channel commands, closed connections and 10 s passing, with every WebSocket message, relay request body, result, notify state and log line; and 20 `log_rate_limits` lines) | HA 2026.6.1's core with `http`, `websocket_api` and `mobile_app` (its `notify.py`, `push_notification.py`, `websocket_api.py`) and `notify`, on aiohttp 3.13.5, with a local server as the push relay |
| `http.py` | `ha-api/src/cors/vectors/cors.json` (preflights and requests, with and without an `Origin`, to `/auth/token` and `/auth/revoke`: status, body and `Access-Control-*` headers) and `ha-api/src/rest/vectors/post_state.json` (`POST /api/states/<id>` with each kind of body and `Content-Type`, and the state after each step) | HA 2026.6.1's `http` (aiohttp 3.13.5, aiohttp_cors 0.8.1), `auth` and `api`, booted in a temporary directory with an admin and a non-admin user |
| `automation_templates.py` | `ha-automation/src/vectors/templates.json` (19 scenarios: each one's states, entity registry entries, automations and steps, and the service calls with their rendered data, `automation_triggered` events, automation states and WARNING/ERROR log lines HA produced) | HA 2026.6.1's `automation` with `helpers/script.py`, `condition.py`, `trigger.py`, `service.py`, `event.py`, the `homeassistant` integration's `state`, `numeric_state` and `time` triggers and the `template` trigger, on Jinja 3.1.6 and voluptuous 0.15.2, in Europe/London, on a virtual-time event loop |
| `automation_reload.py` | `ha-automation/src/vectors/reload.json` (26 scenarios: the automations at start, the steps that rewrite `automations.yaml`, call `automation.reload` and the other `automation.*` services, write states and pass time, and what HA did in order: each automation's `state_changed` with its attributes, the `entity_registry_updated`, `automation_reloaded` and `automation_triggered` events, the actions' service calls, what a call raised and the ERROR log lines; then the automations' registry rows and states) | HA 2026.6.1's `automation` (its `reload` service and `ReloadServiceHelper`), the entity component's `async_prepare_reload`, the entity registry and `RestoreEntity`'s restore state, from a `configuration.yaml` that includes `automations.yaml`, in Europe/London, on a virtual-time event loop |
| `yaml_dump.py` | `ha-core/src/yaml/vectors/dump.json` (930 values in the typed form of `yaml/vectors.json`, each with the text HA writes for it: every kind of string as a mapping's value, a list's item and a key, numbers, dates and nesting; and 18 documents loaded with HA's loader and written back, as an editor save does to `automations.yaml`) | HA 2026.6.1's `homeassistant.util.yaml.dump` and `parse_yaml`: annotatedyaml 1.0.2 on PyYAML 6.0.3's `CSafeDumper` and `CSafeLoader` (libyaml 0.2.5) |
| `automation_editor.py` | `ha-api/src/automation/vectors/editor.json` (19 scenarios: `automations.yaml` at start and, for each `GET`, `POST` and `DELETE` to `/api/config/automation/config/<id>`, the reply's status, content type and body, the file afterwards, and the automations' `state_changed`, `entity_registry_updated` and `automation_reloaded` events that follow; then the registry rows and states) | HA 2026.6.1's core with `http`, `auth`, `api`, `config` (`EditAutomationConfigView`) and `automation`, booted in a temporary directory with an admin and a non-admin user, over real HTTP (aiohttp 3.13.5, annotatedyaml 1.0.2) |
| `entity_registry_ws.py` | `ha-api/src/ws/vectors/entity_registry.json` (the entity, device and category registries as stored at start, then 44 messages from an admin and a user: `config/entity_registry/list`, `get`, `get_entries` and `update` of the area, labels and categories, and `config/category_registry/list`, each with its reply, the `entity_registry_updated` events and the rows as stored afterwards) | HA 2026.6.1's core with `http`, `auth`, `websocket_api` and `config` (`entity_registry.py`, `category_registry.py`), booted in a temporary directory, over a real WebSocket (aiohttp 3.13.5) |
| `import_releases.py` | `ha-server/tests/vectors/import/` (one synthetic install as 2026.6.1, 2026.6.4, 2026.7.0, 2026.7.4, 2026.8.3, 2026.9.4, 2026.10.4 and 2026.11.2 leave it on disk: every `.storage` file but `http`'s (it holds the run's port) and the YAML; and `downgrade.json`: whether Home Assistant 2026.6.1's `bootstrap.async_load_base_functionality` loads each of those directories, with the error it logs when it doesn't) and `ha-integrations/src/mobile_app/vectors/store_downgrade.json` (2026.7's `mobile_app` store after 2026.6.1 loaded it, and after its next change) | **Each release's own code**, one virtualenv per release: its registries, `Store`s and their migrations (`bootstrap.async_load_base_functionality`), `auth`, `config_entries` (`ConfigEntry.async_migrate`, so `hue`'s, `lifx`'s and `met`'s `async_migrate_entry`), `core_config`, `person`, `zone`, `lovelace`, `frontend` storage, `restore_state` and `mobile_app`'s store. Only `http` (a free loopback port), `person`, `zone` and `labs` are set up; nothing reaches the network. The registry calls `lifx`, `hue` and `met` make at setup are made by the script with the arguments each release's source gives (`register_as_setup_does`), since the integrations themselves need their devices. `downgrade.json` is the result of 2026.6.1's base bootstrap alone |
| `config_flow.py` | `ha-api/src/config_entries/vectors/flow.json` (13 scenarios of adding SwitchBot Cloud from the UI: each HTTP request to the flow views and each WebSocket message, with the reply, the events `config_entries/subscribe` and `config_entries/flow/subscribe` sent after it, and the entries stored at the end), `ha-integrations/src/switchbot_cloud/vectors/fresh_setup.json` (the device and entity registry rows and the states of the entry the first scenario adds), `two_accounts.json` (the same after a second account lists the same devices), `retry.json` (the entry's changes when a setup that wasn't ready is retried, after 6.5 s of real time), `disabled_account.json` (the entries and registry rows after an account is disabled, and after a second account that lists the same devices is added), `us_customary.json` (the sensors' rows and states after the first scenario on an install set to US customary units) and `ha-api/src/ws/vectors/integration_descriptions.json` (`integration/descriptions` for the integrations hars runs) | HA 2026.6.1's core with `http`, `auth`, `websocket_api`, `config` (`config_entries.py`'s views and commands on `helpers/data_entry_flow.py`), `data_entry_flow.py`, `config_entries.py` and `switchbot_cloud` (its `config_flow.py`, setup and platforms) on switchbot-api 2.11.0, voluptuous 0.15.2 and voluptuous-serialize 2.7.0, booted in a temporary directory with an admin and a non-admin user, the library's `_API_HOST` pointed at a server on 127.0.0.1 that answers with the recorded responses in `switchbot_cloud/vectors/` |
| `hue_flow.py` | `ha-api/src/config_entries/vectors/hue_flow.json` (9 scenarios of adding a Philips Hue bridge from the UI: each HTTP request to the flow views and each WebSocket message with its reply and the `config_entries/subscribe` events after it, every request aiohue made of the bridge and of the discovery service during the step, the entries stored at the end, and for the first scenario the registry rows and states) and `ha-integrations/src/hue/vectors/fresh_setup.json` (the entry, the device, entity and area registry rows and the states of the entry the first scenario adds) and `us_customary.json` (the sensors' rows and states after the same scenario on an install set to US customary units) | `config_flow.py`'s Home Assistant (it passes its own fake to that script's `run`) with `hue` (its `config_flow.py`, `bridge.py`, `v2/device.py` and entity platforms) on aiohue 4.8.1. aiohttp's requests to the made-up bridge address `192.0.2.10` and to `discovery.meethue.com` are redirected to two servers on 127.0.0.1: one answers as a v2 bridge with the recorded resources in `hue/vectors/resource.json` and hands out a made-up application key once its link button is "pressed", the other as the discovery service |
| `hue_rows.py` | `ha-integrations/src/hue/vectors/imported/`: `resource.json` (the bridge's resources: the recorded ones and a copy of the bedroom under made-up ids, names and addresses), `core.config_entries`, `core.device_registry`, `core.entity_registry` and `core.area_registry` (the store files HA left of an install that was used: entities of every kind disabled, hidden, renamed, moved, given another id, unit and precision, rows that never ran, rows whose resources are gone, removed rows and devices; ids and times replaced by made-up ones of the same form, rows put in an order of their own), `states.json` (the states then), `us_customary.json` (the temperature sensors' rows and states after a start of that install set to US customary units) and `restored.json` (rows the user removed whose resources the bridge still has, and those rows after the next start, with the entry's "new entities start disabled" on and off) | Home Assistant 2026.6.1 with `hue` on aiohue 4.8.1, **started nine times** on config directories that carry over, against `hue_flow.py`'s bridge on 127.0.0.1: the bridge is added through its flow, the user's changes are made through the registries as the frontend makes them, the bridge's resources change between the first and the second start, and the script checks that a third start changes no store file. So: `hue`'s set-up over rows that exist (`v2/device.py`, the entity platforms), `helpers/entity_platform.py`, `helpers/entity_registry.py` (restoring deleted rows, `pref_disable_new_entities`), `helpers/device_registry.py`, `components/sensor` (units, precision, `sensor.private`) and the stores' writers |
| `lifx_flow.py` | `ha-api/src/config_entries/vectors/lifx_flow.json` (8 scenarios of adding a LIFX bulb from the UI: each HTTP request to the flow views and each WebSocket message with its reply and the `config_entries/subscribe` events after it, every datagram HA sent to a bulb or broadcast during the step, the entries stored at the end, and for the scenarios that add a bulb the registry rows and states) and `ha-integrations/src/lifx/vectors/fresh_setup.json` (the entry, the rows and the states after a colour bulb is added by its address and a white one is picked from discovery) | `config_flow.py`'s Home Assistant with `lifx` (its `config_flow.py`, `discovery.py`, `coordinator.py` and platforms) on aiolifx 1.2.1. Every UDP endpoint HA opens goes through the script: one to a made-up bulb address is connected to a fake bulb on 127.0.0.1, whose answers seem to come from the bulb's address; a discovery socket is bound to 127.0.0.1 and its broadcast goes to a fake; anything else stops the script. HA's own search, which starts discovery flows `hars` has none of, is put off past each run |
| `lifx_rows.py` | `ha-integrations/src/lifx/vectors/imported/`: the four store files HA left of a used LIFX install (a light renamed, re-id'd, given an icon and an area, a device renamed and moved, buttons disabled, hidden and renamed, the disabled RSSI sensor renamed, a white light disabled, an entry that disables new entities, a bulb that no longer answers), `states.json`, and `restored.json` (rows removed and coming back, with the preference on and off) | HA 2026.6.1 with `lifx` and aiolifx 1.2.1 started several times on config directories that carry over, against `lifx_flow.py`'s fake bulbs, with `hue_rows.py`'s helpers |
| `lifx_products.py` | `ha-integrations/src/lifx/products.rs`: aiolifx's product table (the model HA shows, and the features `lifx_features` reads) as Rust | aiolifx 1.2.1's `products_defs` |
| `device_step.py` | `ha-api/src/config_entries/vectors/device_step.json` (4 scenarios of what the Add integration dialog's last step and the device page's dialog send after SwitchBot Cloud is added: `config/area_registry/create`, `config/device_registry/update`, `config/entity_registry/get_automatic_entity_ids` and `config/entity_registry/update` with a `new_entity_id`, valid and not, from an admin and a user; each with its reply, the `device_registry_updated`, `area_registry_updated`, `entity_registry_updated` and `state_changed` events it fired, and at the end the stored devices, entities and areas and the entities' states) | `config_flow.py`'s Home Assistant (it imports that script's `run`), so `components/config/device_registry.py`, `area_registry.py` and `entity_registry.py` on `helpers/device_registry.py`, `area_registry.py` and `entity_registry.py`, with `switchbot_cloud`'s entities following their rows (`helpers/entity.py`) |
| `device_names.py` | `ha-api/src/config_entries/vectors/device_names.json` (2 scenarios, in `device_step.py`'s form, of naming a Philips Hue device (a lamp, a room, the bridge, a sensor) and a LIFX bulb in the Add integration dialog's last step and on the device page, with the entity ids that follow; and `restarted`: the states after Home Assistant is started again on the files those changes left) | `config_flow.py`'s Home Assistant with `hue_flow.py`'s fake bridge (aiohue 4.8.1) or `lifx_flow.py`'s fake bulbs (aiolifx 1.2.1), so `hue`'s and `lifx`'s entities following their rows and devices (`helpers/entity.py`) and `helpers/entity_registry.py`'s `async_device_modified`; for `restarted`, `hue_rows.py`'s `boot` started twice on one config directory |
| `entity_components.py` | `ha-integrations/tests/vectors/entity_components.json` (49 service calls, validation errors, feature-gate refusals, number steps, and unrecorded-attribute sets across `select`, `number`, `cover`, `lock`, and `fan`) | HA 2026.6.1's `components/{select,number,cover,lock,fan}` with `bootstrap.async_load_base_functionality` and fake entities |

`render_template.py` and `template_functions.py` write two crates'
vectors because they are two halves of one behaviour: the engine's answers
and the command's messages around them.

`import_releases.py` is the one generator that runs releases other than the
pin: its subject is what later releases leave on disk and what the pinned
release does with that. When the pin moves, its `SEED` moves with it.

## Writing one

- **One script per vector set**, named after the module whose `vectors/` it
  writes (`hue.py` writes `crates/ha-integrations/src/hue/vectors/`).
- **The docstring says** what it writes, the exact command to run it, and the
  pinned versions it runs against: HA 2026.6.1 and the library underneath it
  (aiohue 4.8.1, aiolifx 1.2.1, pyMetno 0.13.0, …).
- **Check the versions first** and stop if they don't match, so vectors are
  never made from the wrong code by accident (see the skeleton below).
- **Run HA's code, don't rewrite it.** Import HA's component and the library
  and call them. Only fake what is outside HA: the network (replay the
  recorded responses), the clock and randomness.
- **Inputs are committed, sanitised**: recorded device responses and registry
  files go in `vectors/` only after everything that points at a real person,
  home or device is replaced with a fake value
  ([what to replace](#sanitising-a-fixture)).
- **Same input, same output**: a fixed clock and seed, so rerunning on
  unchanged inputs leaves `git diff` empty. That is the check that the script
  still matches what is committed.
- **The Rust test points back**: the comment at the top of the module's
  `tests.rs` names the script.

## Sanitising a fixture

Recordings come from a real install. Before one is committed, replace each
of these with a fake value, the same fake everywhere it appears (inputs,
expected outputs and the Rust tests):

| Replace | With |
|---|---|
| MAC addresses, bridge ids, serial numbers and other hardware ids | The vendor prefix kept, the device part made up (`00:17:88:01:00:00:00:01`, `d0:73:d5:01:02:03`) |
| Network ids: Zigbee extended PAN id, Wi-Fi SSID and BSSID | A made-up one (`FakeSSID`, `02:00:00:00:00:01`) |
| IP addresses and host names of the home network | A documentation address (`192.0.2.60`) or `127.0.0.1` |
| People's names, logins and device names that hold one, and their slugs | A made-up name (`alex`, `Alex's iPhone`, `person.alex`, `alex_s_iphone`) |
| Coordinates and addresses | Rounded to a city centre, or made up (`0.5`, `0.5`) |
| Tokens, keys, passwords, webhook ids, push tokens, cloud account ids | An obvious fake (`fake-push-token`, `0f0f…`) |

A hardware id has several spellings, and all of them must go: with colons,
without, upper case, inside a unique id (`<mac>_rssi`), inside another id
made from it (a Hue bridge id is its MAC with `fffe` in the middle),
inside a hex-encoded frame, and as a byte array in a test
(`[0xd0, 0x73, …]`). Decode recorded frames and encrypted payloads and
check what is inside.

Random ids that name nothing outside the fixture can stay: Hue resource
UUIDs, HA registry, device and config entry ids. So can vendor, model and
firmware strings, and room or device names that name no person ("Kitchen").

Use same-length fakes that sort the same way where you can, so offsets and
orders HA produced stay true. Where a value feeds something HA computed
that a rename can't keep (a hash, a signature, a sort), run the generator
again on the sanitised input instead of editing the output. Then search for
every old value before committing:

```sh
git grep -n -i -E '<device part of the id, with any separator>'   # must print nothing
```

## Running one

Each script runs in its own virtualenv with HA and its library at the pinned
versions. HA 2026.6.1 requires Python 3.14.2 or later; use that interpreter
for the virtualenv, even though the repository's script tests need only 3.11.
For `onboarding.py`:

```sh
python3.14 -m venv .venv-onboarding  # Python 3.14.2+ installed
.venv-onboarding/bin/pip install homeassistant==2026.6.1 aiohasupervisor==0.4.3 PyMetno==0.13.0
.venv-onboarding/bin/python -I scripts/vectors/onboarding.py
cargo test -p ha-api onboarding
```

For `render_template.py` (HA's own requirements bring Jinja 3.1.6):

```sh
python3.14 -m venv .venv-template  # Python 3.14.2+ installed
.venv-template/bin/pip install homeassistant==2026.6.1
.venv-template/bin/python -I scripts/vectors/render_template.py
.venv-template/bin/python -I scripts/vectors/template_functions.py
cargo test -p ha-core template && cargo test -p ha-api render_template
```

For `mobile_app_notify.py` (importing `mobile_app` imports `cloud`, and
through it `camera` and `stream`, so their requirements are needed too):

```sh
python3.14 -m venv .venv  # Python 3.14.2+ installed
.venv/bin/pip install homeassistant==2026.6.1 PyNaCl==1.6.2 numpy==2.3.2 av==16.0.1 \
    PyTurboJPEG==1.8.3 Pillow==12.2.0 hass-nabucasa==2.2.0 hassil==3.5.0 \
    home-assistant-intents==2026.6.1 ha-ffmpeg==3.2.2 mutagen==1.47.0 \
    pymicro-vad==1.0.1 pyspeex-noise==1.0.2 aiohasupervisor==0.4.3 \
    securetar==2026.4.1 cronsim==2.7
.venv/bin/python -I scripts/vectors/mobile_app_notify.py
cargo test -p ha-api ws::mobile_app
```

For `http.py` (HA's own requirements are enough):

```sh
.venv/bin/python -I scripts/vectors/http.py
cargo test -p ha-api cors:: rest::post_state
```

For `automation_templates.py`, `automation_reload.py` and `yaml_dump.py`
(HA's own requirements are enough):

```sh
.venv-template/bin/python -I scripts/vectors/automation_templates.py
cargo test -p ha-automation template_tests
.venv-template/bin/python -I scripts/vectors/automation_reload.py
cargo test -p ha-automation reload_tests
.venv-template/bin/python -I scripts/vectors/yaml_dump.py
cargo test -p ha-core yaml::tests::dumps
.venv-template/bin/python -I scripts/vectors/automation_editor.py
cargo test -p ha-api automation::
.venv-template/bin/python -I scripts/vectors/entity_registry_ws.py
cargo test -p ha-api entity_registry
```

For `config_flow.py` (HA's own requirements, plus `switchbot_cloud`'s
library, which imports pycryptodome):

```sh
.venv-template/bin/pip install --no-deps switchbot-api==2.11.0 pycryptodome
.venv-template/bin/python -I scripts/vectors/config_flow.py
.venv-template/bin/python -I scripts/vectors/device_step.py
.venv-template/bin/python -I scripts/vectors/hue_flow.py
cargo test -p ha-api config_entries && cargo test -p ha-integrations flow
```

`device_names.py` needs both `hue_flow.py`'s aiohue 4.8.1 and
`lifx_flow.py`'s aiolifx 1.2.1 in the same virtualenv:

```sh
.venv-template/bin/python -I scripts/vectors/device_names.py
cargo test -p ha-api config_entries
```

`automation_templates.py` and `automation_reload.py` run each scenario on an event loop whose clock moves only when a step
says so (every timer due on the way runs, in order), with `time.time` and
`dt_util.utcnow`/`now` following it, and seeds `random` (the time trackers'
offsets). Automations that one change triggers run side by side in an
order Python's string hashing picks, so observations at one instant are
written grouped by kind and by automation; the Rust test compares them the
same way, and to the second.

For `import_releases.py`, one virtualenv per release. Each needs the
libraries that release's `hue`, `lifx`, `met` and `switchbot_cloud` import,
as their `manifest.json` `requirements` pin them (shown for 2026.7.4; from
2026.10 `lifx-async` is in place of the `aiolifx` three), and the packages
`mobile_app` needs (the list above for `mobile_app_notify.py`) at the
versions in the release's `homeassistant/package_constraints.txt` (`-c`
below, from the unpacked wheel). 2026.6.x also needs `pycryptodome`, and
2026.9 and later `gazetteer-matcher`:

```sh
python3.14 -m venv .venv-2026.7.4  # and one for each other release
.venv-2026.7.4/bin/pip install homeassistant==2026.7.4 aiohue==4.8.1 aiolifx==1.2.2 \
    aiolifx-effects==0.3.2 aiolifx-themes==1.0.4 PyMetno==0.13.0 switchbot-api==2.12.0 \
    PyNaCl numpy av PyTurboJPEG Pillow hass-nabucasa hassil home-assistant-intents \
    ha-ffmpeg mutagen pymicro-vad pyspeex-noise aiohasupervisor securetar cronsim \
    -c package_constraints.txt
.venv-2026.6.1/bin/python -I scripts/vectors/import_releases.py seed
for v in 2026.6.4 2026.7.0 2026.7.4 2026.8.3 2026.9.4 2026.10.4 2026.11.2; do
    .venv-$v/bin/python -I scripts/vectors/import_releases.py upgrade
done
.venv-2026.6.1/bin/python -I scripts/vectors/import_releases.py downgrade
cargo test -p ha-server --test import && cargo test -p ha-integrations store_downgrade
```

`seed` makes the install with 2026.6.1; `upgrade` starts each later release
on it, as an update does, and changes every store through that release;
`downgrade` records what 2026.6.1 does with each result. To test another
release, run `upgrade` under it, `downgrade` again, add it to the lists in
`imports_every_release` and, if hars converts something in it,
`a_converted_install_is_the_2026_6_1_install_again`, and, if it imports, to
`TESTED_HA` in `crates/ha-server/src/import.rs`. Ids, tokens and times are
replaced by fixed ones, and the sets Home Assistant lists in a changing
order are sorted, so a second run changes nothing.

The seed install is the start of a round trip: `hars import` must turn
what each later release makes of it back into it
(`a_converted_install_is_the_2026_6_1_install_again`). So it holds what
those releases store differently: a device two integrations share, a
device of a subentry, a device reached through another, disabled, renamed
and removed devices; entities named by the user with and without the
device's name, hidden, disabled, exposed to Assist, with options; config
entries shaped as each integration's 2026.6.1 config flow makes them; last
states with and without a registry row.

One more mode checks the other direction by hand. It runs 2026.6.1's base
bootstrap on a directory `hars import` wrote, and says whether it loads
and what 2026.6.1's own writers change in the two registries:

```sh
hars import /path/to/a/2026.11/config   # with HARS_CONFIG=/tmp/converted
.venv-2026.6.1/bin/python -I scripts/vectors/import_releases.py loads /tmp/converted
```

For each of the four converted fixtures it answers `"loads": true`, the
entity registry `"as hars wrote it"`, and the device registry the same
apart from the fields 2026.6.1 leaves out (`composite_device_id`,
`composite_primary_config_entry`, `has_composite_identifiers`,
`parent_device_id`, `split_at`). The comparison is of the data, with
`modified_at` and the order of every list left aside (Home Assistant
lists a set in any order), so it does not show a changed order.

`template_functions.py` stops HA's clock (`homeassistant.util.dt.now` and
`utcnow`) at the home's `now` and sets the process's `TZ`, which Python
uses for a naive datetime's `timestamp()`. Its `strftime` cases avoid what
macOS's C library does differently from glibc (`%P`, the `-` flag): hars
follows glibc, which HA runs on.

## Skeleton

```python
#!/usr/bin/env python3
"""Vectors for `hue`: <what it writes>.

    .venv/bin/python -I scripts/vectors/hue.py

Runs HA 2026.6.1's `components/hue` with aiohue 4.8.1 on the recorded
bridge responses in crates/ha-integrations/src/hue/vectors/.
"""

import importlib.metadata
import json
import pathlib
import sys

PINNED = {"homeassistant": "2026.6.1", "aiohue": "4.8.1"}
OUT = pathlib.Path(__file__).resolve().parents[2] / "crates/ha-integrations/src/hue/vectors"


def check_versions():
    for pkg, want in PINNED.items():
        have = importlib.metadata.version(pkg)
        if have != want:
            sys.exit(f"{pkg} is {have}; these vectors are made with {want}")


def write(name, data):
    (OUT / name).write_text(json.dumps(data, indent=1, ensure_ascii=False) + "\n")


def main():
    check_versions()
    # Load the recorded inputs from OUT, run HA's code on them, then
    # write(...) what it produced.


if __name__ == "__main__":
    main()
```

## Vector sets made before this rule

Paths below are relative to `crates/`. Each directory row covers **all files**
there, including recorded inputs, registry snapshots and expected outputs.
The HA paths are relative to `homeassistant/` at 2026.6.1.

**Status of every row:** no generator is committed. Preserve the inputs,
write a version-checked generator and run it at the current pin to reproduce
the committed expectations before moving the pin. This is the
[roadmap backlog](../../docs/ROADMAP.md#test-oracle).

| Vector set / fixtures | HA code it came from |
|---|---|
| `ha-core/src/yaml/vectors.json` | `util/yaml`, annotatedyaml 1.0.2 on PyYAML 6.0.3's `CSafeLoader` (217 typed loader cases) |
| `ha-core/src/entityfilter/vectors.json` | `helpers/entityfilter.py::convert_include_exclude_filter`; Python `fnmatch.translate` and `re.match` for globs |
| `ha-integrations/src/sun/vectors.json` | `components/sun`: `Sun.update_events`, `update_sun_position`; astral 2.2 |
| `ha-integrations/src/met/vectors/` | `components/met`, `components/weather`, pyMetno 0.13.0; `fixture_*.json` are forecast inputs, the other files are expected scenarios |
| `ha-integrations/src/light/vectors/` | `components/light`, `util/color.py` (colour conversions, brightness and service processing) |
| `ha-integrations/src/hue/vectors/` | `components/hue/v2`, aiohue 4.8.1 (resource/registry inputs, entity snapshots, SSE and service scenarios) |
| `ha-integrations/src/lifx/vectors/` | `components/lifx`, aiolifx 1.2.1 (probe replies, registries and service vectors) |
| `ha-integrations/src/switchbot_cloud/vectors/` | `components/switchbot_cloud` and its library (GET/registry inputs, signing, startup and service results) |
| `ha-integrations/src/mobile_app/vectors/crypto.json` | `components/mobile_app` encryption helpers, with fixed nonces |
| `ha-integrations/src/config_entries/vectors/` | `config_entries.py` (before/after `update_registration` storage) |
| `ha-api/src/mobile_app/vectors/` | `components/mobile_app` run over HTTP (transport, commands, registrations, sensors, startup, restore and location; registry inputs included) |
| `ha-components/src/vectors/zone_math.json` | `util/location.py::distance`, `components/zone` inside test |
| `ha-components/src/vectors/location.json` | `components/mobile_app` tracker updates → `components/person` → `components/zone` (also covered by the API's location fixture) |
| `ha-registry/src/vectors/` | `helpers/entity_registry.py`, `helpers/device_registry.py` registry-write runs; includes `files/` with the exact storage files HA wrote |
| `ha-automation/src/vectors/` | `components/automation`, state/numeric-state/time triggers, `helpers/condition.py`, `helpers/script.py`; `automations.yaml` and registry JSON are inputs, `INDEX.json` describes the scenario outputs |

### Inline expectations

Some HA/Python-produced answers are embedded in Rust tests rather than JSON.
They have no generator either. Include them when rebuilding the relevant set:

- `ha-registry/src/tests.rs`: `_async_strip_prefix_from_entity_name` cases
  run in Python (`helpers/entity_registry.py`).
- `ha-api/src/ws/mod.rs` tests: voluptuous 0.15.2 `humanize_error` in
  CPython 3.14; `ha-api/src/mobile_app/tests.rs`: HA's `RequestDataValidator`
  messages. The API's `ws/tests.rs` also embeds HA `subscribe_entities`
  schema/filter expectations alongside cases from the mobile-app runs.
- Python-only numeric/string expectations: `ha-core/src/casefold.rs`,
  `ha-integrations/src/num.rs`, `sun/tests.rs`, `met/tests.rs`,
  `mobile_app/crypto.rs` and `mobile_app/sensor.rs` (case folding, float
  representation/rounding/summation, base64 and numeric coercion). These
  check the Python semantics HA relies on, rather than a separate HA fixture
  directory; review them when changing HA's Python or library versions.

## Generated compatibility tables

These are **runtime data**, not test vector sets. They must also be refreshed
when the HA pin moves. Three have generators today; the others need a
reproducible extraction added to the same roadmap backlog.

| Destination (relative to `crates/`) | Source and regeneration status |
|---|---|
| `ha-api/src/i18n_data.rs` | HA wheel's `components/*/translations/en.json`, `icons.json` and manifest titles; `scripts/gen-i18n.py WHEEL` emits stdout. Redirect to this file. |
| `ha-api/src/onboarding_data.rs` | HA wheel's `generated/{countries,currencies,languages}.py` (`cv.country`, `cv.currency`, `cv.language`) and `components/onboarding/{const.py,translations/*.json}` (default areas, icons, names per language with English fallback); `scripts/gen-onboarding.py WHEEL` emits stdout. Redirect to this file. |
| `ha-api/src/manifests_data.rs` | HA wheel's `components/*/manifest.json`; `scripts/gen-manifests.py WHEEL` writes this file from the repository root, adding built-in flags and `default_config` dependencies. |
| `ha-api/src/sensor_units.json` | `components/sensor/websocket_api.py`: `_NUMERIC_DEVICE_CLASSES`, `DEVICE_CLASS_UNITS` for classes with a converter, sorted with `str.casefold`. Generated by running HA; no committed generator. |
| `ha-api/src/ws/statistic_units.json` | `components/recorder/websocket_api.py::UNIT_SCHEMA`, unit converters and voluptuous validation messages, as documented by `ws/recorder.rs`. Generated from HA; no committed generator. |
| `ha-api/src/labs.rs::FEATURES` | Copied from HA's `generated/labs.py`; no committed extraction script. Review the feature list and URLs against the new release. |
| `ha-core/src/casefold.rs::FOLDS` | Python-derived, not HA-generated: Python 3.14.7 / Unicode 16.0.0, every code point whose `casefold()` differs from `lower()`. No committed generator. |

The exact wheel download, both generator commands and formatting/check steps
are in the [integration guide](../../docs/guides/add-an-integration.md#6-translations-icons-and-manifests).

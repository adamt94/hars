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
| `automation_editor.py` | `ha-api/src/automation/vectors/editor.json` (18 scenarios: `automations.yaml` at start and, for each `GET`, `POST` and `DELETE` to `/api/config/automation/config/<id>`, the reply's status, content type and body, the file afterwards, and the automations' `state_changed`, `entity_registry_updated` and `automation_reloaded` events that follow; then the registry rows and states) | HA 2026.6.1's core with `http`, `auth`, `api`, `config` (`EditAutomationConfigView`) and `automation`, booted in a temporary directory with an admin and a non-admin user, over real HTTP (aiohttp 3.13.5, annotatedyaml 1.0.2) |

`render_template.py` and `template_functions.py` write two crates'
vectors because they are two halves of one behaviour: the engine's answers
and the command's messages around them.

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
  files go in `vectors/` with ids replaced, no tokens, coordinates rounded
  (the roadmap's fixture rule).
- **Same input, same output**: a fixed clock and seed, so rerunning on
  unchanged inputs leaves `git diff` empty. That is the check that the script
  still matches what is committed.
- **The Rust test points back**: the comment at the top of the module's
  `tests.rs` names the script.

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
```

It runs each scenario on an event loop whose clock moves only when a step
says so (every timer due on the way runs, in order), with `time.time` and
`dt_util.utcnow`/`now` following it, and seeds `random` (the time trackers'
offsets). Automations that one change triggers run side by side in an
order Python's string hashing picks, so observations at one instant are
written grouped by kind and by automation; the Rust test compares them the
same way, and to the second.

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

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
someone asks how an expected value was produced. Every vector set in the
repository today was made before this rule, so none has its script yet; they
are listed [at the end](#vector-sets-made-before-this-rule) as a backlog.

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
No generator has been written yet (see the table below), so this is what
running the future `hue.py` will look like:

```sh
python3.14 -m venv .venv-hue  # Python 3.14.2+ installed
.venv-hue/bin/pip install homeassistant==2026.6.1 aiohue==4.8.1
.venv-hue/bin/python -I scripts/vectors/hue.py
cargo test -p ha-integrations hue
```

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
when the HA pin moves. Two have generators today; the others need a
reproducible extraction added to the same roadmap backlog.

| Destination (relative to `crates/`) | Source and regeneration status |
|---|---|
| `ha-api/src/i18n_data.rs` | HA wheel's `components/*/translations/en.json`, `icons.json` and manifest titles; `scripts/gen-i18n.py WHEEL` emits stdout. Redirect to this file. |
| `ha-api/src/manifests_data.rs` | HA wheel's `components/*/manifest.json`; `scripts/gen-manifests.py WHEEL` writes this file from the repository root, adding built-in flags and `default_config` dependencies. |
| `ha-api/src/sensor_units.json` | `components/sensor/websocket_api.py`: `_NUMERIC_DEVICE_CLASSES`, `DEVICE_CLASS_UNITS` for classes with a converter, sorted with `str.casefold`. Generated by running HA; no committed generator. |
| `ha-api/src/ws/statistic_units.json` | `components/recorder/websocket_api.py::UNIT_SCHEMA`, unit converters and voluptuous validation messages, as documented by `ws/recorder.rs`. Generated from HA; no committed generator. |
| `ha-api/src/labs.rs::FEATURES` | Copied from HA's `generated/labs.py`; no committed extraction script. Review the feature list and URLs against the new release. |
| `ha-core/src/casefold.rs::FOLDS` | Python-derived, not HA-generated: Python 3.14.7 / Unicode 16.0.0, every code point whose `casefold()` differs from `lower()`. No committed generator. |

The exact wheel download, both generator commands and formatting/check steps
are in the [integration guide](../../docs/guides/add-an-integration.md#6-translations-icons-and-manifests).

# Vector generators

How hars's tests are checked against Home Assistant. Why:
[ADR 0008](../../docs/adr/0008-vectors-from-home-assistant.md). Where this
fits in testing: [CONTRIBUTING.md](../../CONTRIBUTING.md#tests).

A *vector* is a test case whose expected answer came from running Home
Assistant's own code: real input (a bridge's JSON, a bulb's reply, a service
call) and what HA 2026.6.1 did with it (states, attributes, the request it
sent, the error it raised). The Rust tests feed the same input to `hars` and
fail on any difference. They live next to the code they test, in
`crates/<crate>/src/<module>/vectors/`.

The script that runs HA's code to make a vector set is committed here, as
`scripts/vectors/<module>.py`. Without it, vectors can't be remade when the
pinned HA version moves, when an input is added, or when someone asks how an
expected value was produced.

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

These were made by scripts that weren't kept. Each needs its generator written
(and run, to confirm it reproduces what is committed) before the pinned HA
version can move.

| Vector set | HA code it came from |
|---|---|
| `ha-integrations/src/sun` (`vectors.json`) | `components/sun`, astral 2.2 |
| `ha-integrations/src/met` | `components/met`, `weather`, pyMetno 0.13.0 |
| `ha-integrations/src/light` | `components/light`, `util/color` |
| `ha-integrations/src/hue` | `components/hue/v2`, aiohue 4.8.1 |
| `ha-integrations/src/lifx` | `components/lifx`, aiolifx 1.2.1 |
| `ha-integrations/src/switchbot_cloud` | `components/switchbot_cloud` |
| `ha-integrations/src/mobile_app` | `components/mobile_app` (encryption) |
| `ha-integrations/src/config_entries` | `config_entries` |
| `ha-api/src/mobile_app` | `components/mobile_app` (webhooks) |
| `ha-components/src` | `components/zone`, `util/location` |
| `ha-registry/src` | `helpers/*_registry` |
| `ha-automation/src` | `components/automation`, triggers and conditions |

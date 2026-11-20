# Contributing to hars

Thanks for helping. This file covers setup, the checks a change must pass,
how tests are written, and how pull requests are reviewed. How hars is built
is in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md); the
[docs index](docs/README.md) lists everything else.

hars has one overriding goal: **use as little memory, CPU, wakeups and disk
as possible** while behaving like Home Assistant. Every change is judged on
both. Read [docs/PERF.md](docs/PERF.md) before your first change.

## Prerequisites

- **Rust** through [rustup](https://rustup.rs). `rust-toolchain.toml` picks
  the stable toolchain with `rustfmt` and `clippy`.
- **Python 3.11+**, for the script tests and table generators. Running
  Home Assistant itself for vectors needs **Python 3.14.2+** at the current
  HA pin (see [the vector guide](scripts/vectors/README.md)).
- **Docker**, to build the image, and on macOS to run the performance
  harness (it needs Linux).

## Build, run, check

```sh
scripts/check.sh                          # what CI's check job runs, in order
cargo build --release                     # target/release/hars
HARS_CONFIG=./config cargo run --release -- auth add me my-password
HARS_CONFIG=./config cargo run --release  # http://localhost:8123
```

`auth add` makes the owner without a browser. With the UI served (below),
you can skip it and onboard in the browser instead, as a user would.

`scripts/check.sh` runs `cargo fmt --all --check`, `cargo clippy
--all-targets --locked` and `cargo test --workspace --locked` with
`RUSTFLAGS=-D warnings`, then `python3 -m unittest discover -s scripts`.
`cargo fmt --all` fixes formatting.

Without `HARS_FRONTEND`, only the API is served. To get the UI, copy the
pinned frontend out of the image and point at it:

```sh
docker cp "$(docker create ghcr.io/adamt94/homeassistant-rust)":/frontend ./frontend
HARS_FRONTEND=./frontend HARS_CONFIG=./config cargo run --release
```

(`docker build -t hars .` and `hars` in place of the ghcr.io name works
too.) The API needs a token, as in Home Assistant:
`scripts/hars-token.sh http://localhost:8123 me my-password` prints one.

CI also has a `docker` job: it builds the image, smoke-tests it, and runs the
resource budget gate (`PERF_GATE=1 scripts/perf.sh`, see
[docs/PERF.md](docs/PERF.md#budgets)). To run the same gate locally,
`PERF_GATE=1 scripts/perf-docker.sh` (macOS or Linux, with Docker).

## Tests

**Every behaviour change ships with a test.** The conventions:

- **Unit tests sit next to the code**, in one of two forms. Most files end
  with an inline `#[cfg(test)] mod tests { … }`. A module whose tests are
  long or use fixtures keeps them in a `tests.rs` beside it, declared with
  `#[cfg(test)] mod tests;`, as each integration in `ha-integrations` does
  (`sun/tests.rs`, `met/tests.rs`, …). Use the inline form for a small
  module and move to `tests.rs` once the tests outgrow the file. Tests that
  need a process or a binary go in
  `crates/<crate>/tests/` (for example `ha-server/tests/shutdown.rs` and the
  recorder's `kill -9` test, `ha-recorder/tests/crash.rs`).
- **Time is simulated**, never slept through. Build `Hass` with
  `ha_core::SimulatedClock` and use `#[tokio::test(start_paused = true)]`
  with `tokio::time::advance`; see `crates/ha-integrations/src/sun/tests.rs`.
- **No real network.** Inject the URL, host or port and serve canned replies
  from a local `TcpListener` or `UdpSocket`; see
  `crates/ha-integrations/src/met/tests.rs`.
- **Expected values come from Home Assistant, not from reading its code.**
  Where hars must match HA (states, attributes, maths, requests, errors), the
  test compares with *vectors*: inputs run through HA 2026.6.1's own code,
  saved in the module's `vectors/` folder. A new vector set commits the
  script that made it in `scripts/vectors/` (`onboarding.py`,
  `render_template.py`, `mobile_app_notify.py`, `template_functions.py`,
  `automation_templates.py`, `automation_reload.py`, `yaml_dump.py`,
  `automation_editor.py` and `entity_registry_ws.py`
  so far); the sets made before that rule have none yet
  and are listed there as a backlog. The rules, a skeleton
  and the backlog are in [scripts/vectors/README.md](scripts/vectors/README.md).
- **Fixtures are sanitised:** no tokens or keys, ids replaced, coordinates
  rounded. Nothing personal is committed.
- **Python scripts** have `test_*.py` tests in `scripts/`
  (`scripts/test_shadow_diff.py`). `scripts/test_crate_layers.py` checks that
  crates depend only downwards.

## Adding things

Step-by-step guides that walk through the real files:

- [Add an integration](docs/guides/add-an-integration.md) (uses `met`)
- [Add an entity component](docs/guides/add-an-entity-component.md) (uses `switch`)
- [Add a WebSocket command or REST view](docs/guides/add-an-api-command.md)

Where code goes, and when a new crate is justified, is in
[ARCHITECTURE.md](docs/ARCHITECTURE.md#where-new-code-goes).

## Rules for code

- **Match Home Assistant.** For any API, JSON shape, storage file or
  behaviour, check HA's source at the pinned version
  ([compatibility](docs/ARCHITECTURE.md#home-assistant-compatibility)) rather
  than guessing, and name the HA file you mirror in the doc comment
  (`components/met/weather.py`). Deliberate differences are written down in
  the doc comment and the roadmap.
- **Performance rules** (sharing with `Arc`, no `String` clones on hot
  paths, no blocking I/O on async threads, no polling, binary-size cost) are
  in [docs/PERF.md](docs/PERF.md#rules), each with its reason.
- `unsafe` is denied workspace-wide. An exception needs a written reason and
  a maintainer's agreement in review.
- Clippy warnings are errors in CI. Don't silence one without a comment
  saying why.
- Comments say *why*, and what HA does when hars does the same.

## Commits and pull requests

- **One topic per pull request.** Pull requests are squash-merged, so the PR
  title becomes the commit message.
- **Titles use Conventional Commits:** `feat(scope): …`, `fix(scope): …`,
  `test(scope): …`, `perf(scope): …`, `docs: …`, `refactor(scope): …`,
  `chore: …`. The scope is the crate or integration (`met`, `recorder`,
  `ws`). Write what changes for a user, in plain words: `feat(history):
  history/stream over the recorder`.
- **The description** says what changed and why, how it was tested
  (including which vectors), and the performance evidence
  [PERF.md](docs/PERF.md#what-a-pull-request-must-show) asks for. The
  [pull request template](.github/pull_request_template.md) has the list.
- **Docs move with the code.** In the same pull request: tick the
  [roadmap](docs/ROADMAP.md) item it finishes, update
  [ARCHITECTURE.md](docs/ARCHITECTURE.md) if it changes structure or adds a
  crate, add a [results](docs/perf/RESULTS.md) entry if it changes the
  numbers, and update the README's "What works today" and
  [docs/INTEGRATIONS.md](docs/INTEGRATIONS.md) if users see it.

## Review

- A maintainer reviews every pull request; CI must be green to merge.
- Reviewers check: does it match HA (vectors or a cited HA source), is there
  a test for each behaviour change, is the performance evidence there and
  within budget, are the docs updated, is it the smallest change that does
  the job.
- Expect questions about allocations and wakeups. They are the product.
- Address each comment with a change or a reply. The author resolves
  threads; the reviewer approves.

## Reporting bugs and asking for integrations

Use the issue templates. Security problems go through
[SECURITY.md](SECURITY.md), not public issues.

## Licence and affiliation

hars is licensed under [Apache-2.0](LICENSE). By contributing you agree that
your contribution is licensed under the same terms (Apache-2.0, section 5).
When you port code from Home Assistant (Apache-2.0) or a library it uses,
check the library's licence is compatible (Apache-2.0, MIT, BSD), say what
you ported in the module's doc comment, and add the library to
[NOTICE](NOTICE) with its licence and copyright line. After any change to
`Cargo.lock`, run `python3 scripts/gen-third-party.py`: it rewrites
[THIRD-PARTY-LICENSES.txt](THIRD-PARTY-LICENSES.txt), and a script test
fails until you do.

hars is an independent project. It is not affiliated with or endorsed by the
Home Assistant project or Nabu Casa. Please don't ask for support with hars
in Home Assistant's channels.

Everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md).

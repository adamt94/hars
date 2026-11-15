# hars documentation

## Using hars

- [Install, upgrade, back up, troubleshoot](../README.md#install): the main
  README.
- [Moving from Home Assistant](../README.md#moving-from-home-assistant):
  importing an existing install.
- [Shadow mode](SHADOW.md): run hars beside Home Assistant, with nothing it
  decides reaching a device, and compare their decisions.
- [Security policy](../SECURITY.md): reporting a vulnerability, and what
  hars protects.

## Working on hars

- [Contributing](../CONTRIBUTING.md): setup, checks, tests, pull requests,
  review, licence.
- [Architecture](ARCHITECTURE.md): the crates and their dependency rules,
  how hars runs, the API surface, Home Assistant compatibility, and where
  new code goes.
- Guides:
  - [Add an integration](guides/add-an-integration.md)
  - [Add an entity component](guides/add-an-entity-component.md)
  - [Add a WebSocket command or REST view](guides/add-an-api-command.md)
- [Performance guide](PERF.md): budgets, rules, how to measure, what a pull
  request must show.
- [Vector generators](../scripts/vectors/README.md): making test cases from
  Home Assistant's own code.
- [AGENTS.md](../AGENTS.md): the short version, for coding agents.

## Maintaining hars

- [Roadmap](ROADMAP.md): milestones, the MVP inventory, what's next.
- [Decision records](adr/README.md): why hars is built the way it is.
- [Performance results](perf/RESULTS.md): measured numbers over time.
- [Code of conduct](../CODE_OF_CONDUCT.md).
- CI and publishing: [`ci.yml`](../.github/workflows/ci.yml) (checks, image
  smoke test, resource budgets) and
  [`publish.yml`](../.github/workflows/publish.yml) (the ghcr.io image).

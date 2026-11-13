# homeassistant-rust

A ground-up rebuild of [Home Assistant](https://www.home-assistant.io/) in Rust.

**Primary goal: maximum performance per unit of hardware** — minimal RAM, CPU,
disk I/O and startup time, so a full smart-home hub runs comfortably on a
Raspberry Pi Zero–class device (or alongside everything else on a server).

| | Home Assistant (Python) | target for `hars` |
|---|---|---|
| Idle RSS | ~300–500 MB | **< 20 MB** |
| Cold start | 30–90 s | **< 1 s** |
| `state_changed` throughput | ~10³/s | **> 10⁶/s** |
| Install size | ~1 GB+ | **single static binary, < 20 MB** |

Current scaffold: 1.5 MB release binary, ~4.4 MB RSS at idle.

## Status

Early scaffold. Working today:

- `ha-core`: `EntityId`, immutable `Arc<State>` snapshots, `StateMachine`, broadcast `EventBus`, `Hass` handle
- `ha-server` (`hars` binary): `GET /api/`, `GET /api/states`, `GET|POST /api/states/{entity_id}` — same shapes as Home Assistant's REST API

See [`docs/ROADMAP.md`](docs/ROADMAP.md) for what's next and
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the design.

## Quick start

```sh
cargo run --release            # listens on 0.0.0.0:8123 (HARS_BIND to override)
curl -X POST localhost:8123/api/states/light.kitchen \
  -H 'content-type: application/json' \
  -d '{"state":"on","attributes":{"brightness":200}}'
curl localhost:8123/api/states
```

## Docker

```sh
docker build -t hars .
docker run --rm -p 8123:8123 -v hars-config:/config hars
```

The image is `FROM scratch`: a static musl binary, CA roots and tzdata, about
5 MB. `docker stop` sends SIGTERM, which hars handles by draining HTTP and
exiting 0. For LAN integrations (LIFX broadcast, mDNS) use `--network host`.

## Development

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test --workspace
cargo build --profile profiling   # optimised + debug symbols, for perf/flamegraph
```

## License

Apache-2.0, same as Home Assistant. This is an independent reimplementation and
is not affiliated with or endorsed by the Home Assistant project or Nabu Casa.

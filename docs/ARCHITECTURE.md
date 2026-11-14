# Architecture

## Guiding principles

1. **Performance is the feature.** Every design choice is judged by RAM, CPU,
   allocations and wakeups. Measure before and after; regressions are bugs.
2. **API compatibility where it buys ecosystem.** Match Home Assistant's REST +
   WebSocket API and YAML config shapes so the existing frontend, mobile apps
   and tooling can talk to `hars` unchanged.
3. **One static binary.** No Python, no runtime package installs. Integrations
   are compiled in behind Cargo features (later: optional WASM plugins).
4. **Idle means idle.** No polling loops when nothing is happening; timers are
   coalesced; the process should sit at ~0% CPU.

## Crate layout

```
crates/
  ha-core/     event bus, state machine, services, clock, `.storage` files
  ha-auth/     users, credentials, login flows, refresh/access tokens (HA's `homeassistant/auth`)
  ha-registry/ entity, device, area, floor and label registries (HA's `helpers/*_registry`)
  ha-components/ built-in components that create entities: zone, person
  ha-integrations/ integrations started from config entries: sun
  ha-api/      REST API, auth HTTP views, the official frontend
  ha-server/   `hars` binary: wiring, signals, allocator, `hars auth add`
```

Planned crates (create as they're needed, not before):

| crate | purpose |
|---|---|
| `ha-config` | `configuration.yaml` loading, `!include`/`!secret`, validation |
| `ha-recorder` | state history in SQLite, batched writes, compact encoding |
| `ha-automation` | triggers / conditions / actions engine, templates |
| `ha-integrations-*` | one crate per integration family (mqtt, zigbee, esphome, …) |

## Core model

- **`EntityId`** — `domain.object_id`, validated once at the edge, stored as
  `SmolStr` (inline ≤ 23 bytes, so almost always zero heap allocations).
- **`State`** — immutable snapshot, always handed around as `Arc<State>`.
  The state machine, `state_changed` events and API responses all share the
  same allocation.
- **`StateMachine`** — `RwLock<HashMap<EntityId, Arc<State>>>`. A write that
  changes nothing returns the existing `Arc` and fires no event.
- **`EventBus`** — bounded `tokio::sync::broadcast` of `Arc<Event>`. Slow
  subscribers lag and must resync from the state machine rather than growing
  memory without limit. That is fine for API clients, but not for consumers
  that must see every transition (automations, recorder): those use
  `Hass::subscribe_state_changes`, an unbounded, lossless feed with a snapshot
  taken atomically with the subscription. Writers notify the feeds and the bus
  while still holding the state lock, so every consumer sees one order.
- **`Hass`** — cheap-to-clone handle (`Arc`) passed to every integration.

## Performance toolbox

- `mimalloc` global allocator in the binary.
- Release profile: fat LTO, `codegen-units = 1`, `panic = "abort"`, stripped.
- `profiling` profile for `perf` / `cargo flamegraph` / `samply`.
- Add `criterion` benches for hot paths (state set, event fan-out,
  JSON serialisation) before optimising them.

## Open design questions

- Sharded / lock-free state map (e.g. `papaya`, `scc`) vs single `RwLock` —
  decide with benchmarks once there are realistic workloads.
- Attribute storage: `serde_json::Map` today; consider a compact typed
  representation to cut allocations.
- Integration plugin model: compile-time features vs WASM (wasmtime) vs
  out-of-process. Probably features first, WASM for third-party.
- Single-threaded (`current_thread`) runtime for tiny devices vs multi-thread.

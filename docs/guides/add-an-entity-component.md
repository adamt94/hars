# Add an entity component

An *entity component* is HA's `EntityComponent` for one domain (`light`,
`switch`, `climate`): the domain's services, their schemas, and the list of
entities that platforms (integrations) provide. Several integrations can feed
one component, so it lives apart from any of them. This guide adds a new one,
using `switch` (`crates/ha-integrations/src/switch.rs`, about 100 lines) as
the reference and `button` for the tests.

You need one only for a domain **with services** that hars doesn't have yet
(`cover`, `fan`, `lock` …). For `sensor` and `binary_sensor`, integrations
just write states. For a new *kind* of an existing domain (another light),
implement the existing trait in your integration instead
([add-an-integration.md](add-an-integration.md#4-start-it-from-config-entries)).

## 1. The file and the trait

Create `crates/ha-integrations/src/<domain>.rs`, add `pub mod <domain>;` to
`lib.rs`, and start with a doc comment naming HA's
`components/<domain>/__init__.py`.

Define the trait platforms implement: what the services need to read and do.
`switch` has:

```rust
pub trait Switch: Send + Sync {
    fn entity_id(&self) -> &EntityId;
    fn available(&self) -> bool;
    fn is_on(&self) -> bool;
    fn turn(&self, on: bool) -> BoxFuture<'_>;   // the device call
}
```

Async methods return `light::BoxFuture` so the trait stays object-safe
(`Arc<dyn Switch>`). Keep the trait to what the services need; the state
itself is written by the platform with `hass.set_state`.

## 2. The registry of entities

A struct holding the running entities, with `add` and `remove`:

```rust
#[derive(Default)]
pub struct Switches {
    items: RwLock<Vec<Arc<dyn Switch>>>,
}
```

## 3. The services

`register_services(self: &Arc<Self>, hass, registries)` registers each of
HA's services for the domain with `hass.services().register_with_schema`:

- the schema is HA's: `Schema::entity_service()` for the target keys, plus
  `.required(...)`/`.optional(...)` for the service's fields, with HA's
  `Kind`s, ranges and exclusive groups (see `light/` for a large one);
- the handler resolves targets with `call.entity_ids()` and
  `registries.target_entities(&call.data)`, picks the available entities
  with `switch::select`, and runs their calls concurrently
  (`light::join_all`);
- errors use `ServiceError` variants with HA's messages and translation
  keys (`climate.rs` has `service_not_supported` and validation errors).

## 4. Register it once

Registering a service twice replaces the first handler, which would drop the
first platform's entities. So each component is created once, by an accessor
on `components::Components` (`crates/ha-integrations/src/components.rs`):

```rust
pub fn switches(&self) -> Arc<Switches> {
    self.switches
        .get_or_init(|| {
            let c = Arc::new(Switches::default());
            c.register_services(&self.hass, self.registries.clone());
            c
        })
        .clone()
}
```

Add a `OnceLock` field and an accessor for your domain the same way.
Integrations call `components.<domain>()` in `ha_integrations::start` to
make the services exist at start, then `.add(...)` their entities.

## 5. Other places that know about domains

- `ha_core::recorded` lists attributes HA leaves out of history per domain;
  add yours if HA's component has `_unrecorded_attributes`.
- Shadow mode holds back every domain not in `ha_core::shadow::RUNS`, which
  is right for a domain that acts on devices; don't add it there.
- Add the domain to `scripts/gen-i18n.py` and `scripts/gen-manifests.py`
  and regenerate (see
  [add-an-integration.md](add-an-integration.md#6-translations-icons-and-manifests)).
- Add it to the integration's `running.extend([...])` list so `/api/config`
  reports it.

## 6. Tests

In the component's file (or `<domain>/tests.rs`), with a fake platform, as
`button.rs`'s tests do:

- a `Fake` struct implementing the trait and recording calls;
- `Hass::new()`, the component with `register_services`, `add(fake)`;
- `hass.call_service("<domain>", "<service>", data, None, false, context)`
  and assertions on the fake's calls, the states and the error.

Cover HA's schema (good and bad data), targeting (entity, `all`, an
unavailable entity), and every error path. Where the maths or validation
must match HA, make vectors from HA's component
([scripts/vectors/README.md](../../scripts/vectors/README.md)); `light`'s
colour conversions are the example.

## 7. Performance

A component runs per service call, not per state change, so it is not a hot
path. Still: no allocation per entity beyond what the call needs, no locks
held across `.await`, and a binary size note in the pull request
([PERF.md](../PERF.md#what-a-pull-request-must-show)).

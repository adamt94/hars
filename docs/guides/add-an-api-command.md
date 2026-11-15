# Add a WebSocket command or REST view

Everything served over HTTP is in `crates/ha-api`. The frontend talks almost
only WebSocket, so most new API work is a WebSocket command. Match HA's
shapes exactly: the message keys, the result, the error codes and messages,
and who may call it. Read HA's handler at the pinned version first
(`homeassistant/components/<domain>/websocket_api.py`, or the `@websocket_command`
in the component).

## WebSocket command

The reference is `sensor/numeric_device_classes` in
`crates/ha-api/src/ws/sensor.rs`, about 70 lines.

### 1. Put it in the domain's module

Commands are grouped by HA domain in `crates/ha-api/src/ws/<domain>.rs`.
Each module has a `dispatch` that claims its command types:

```rust
pub(super) fn dispatch(
    conn: &mut Conn,
    id: u64,
    ty: &str,
    msg: &Map<String, Value>,
) -> Option<Result<(), Invalid>> {
    Some(match ty {
        "sensor/numeric_device_classes" => validate(msg, false).map(|()| {
            conn.out.result(id, json!({"numeric_device_classes": tables()["numeric"]}));
        }),
        _ => return None,
    })
}
```

Return `None` for types that aren't yours, or when a part of `ApiConfig`
you need isn't there (see `ws/history.rs`, which needs the recorder).

For a new domain, add `mod <domain>;` in `ws/mod.rs` and chain it in
`ws/commands.rs::dispatch`:
`.or_else(|| super::<domain>::dispatch(conn, id, ty, msg))`.

### 2. Validate as HA does

HA validates with voluptuous and answers `invalid_format` with its message.
Return `Err(message)` from the handler and `dispatch` sends it. For simple
commands use `only_keys(msg, &["key", ...])`; for HA's exact messages, build
them as `sensor.rs`'s `validate` does.

### 3. Reply

Use `conn.out` (`ws/mod.rs`):

- `result(id, value)`: the success result.
- `error(id, code, message)`: HA's error codes (`not_found`,
  `unauthorized`, `home_assistant_error` …).
- `event(id, &value)`: an event for a subscription.

**Admin-only** commands check first, as HA's `@require_admin`:

```rust
if conn.principal.permission < Permission::Admin {
    conn.out.error(json!(id), "unauthorized", "Unauthorized");
    return Ok(());
}
```

**Slow work** (disk reads, recorder queries) must not block the
connection: clone `conn.out`, `tokio::spawn` the work, run blocking parts on
`spawn_blocking`, and reply from the task (`call_service` in
`ws/commands.rs`, `ws/history.rs`).

**Subscriptions** send a result, then events, until `unsubscribe_events` or
disconnect. Store the subscription in `conn.subs` as a `Subscription`
(`ws/mod.rs`) so both end it; `subscribe_entities` in `ws/commands.rs` is the
example. A subscription that can fall behind must be bounded and must
disconnect or resync, never grow
([PERF.md](../PERF.md#rules), rule 5).

### 4. Data from elsewhere

If the command needs something from an integration or another crate (the
weather forecasts, the recorder), it comes through an `ApiConfig` field
(`crates/ha-api/src/lib.rs`), which `ha-server`'s `main.rs` fills in.
`ha-api` never starts integrations itself.

### 5. Test it

WebSocket tests are in `crates/ha-api/src/ws/tests.rs`. They start a real
server on a random port and drive it with a client:

```rust
#[tokio::test]
async fn sensor_unit_tables_are_has() {
    let s = server().await;                     // or server_with(ApiConfig { .. })
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    let r = c
        .call(json!({"id": 2, "type": "sensor/numeric_device_classes"}))
        .await;
    // assert on r["result"], and on r["error"] for bad input
}
```

Test the success shape, each `invalid_format` message, the admin check, and
for subscriptions the events and the unsubscribe. If HA's output must be
matched exactly, compare with vectors made from HA's handler.

## REST view

The reference is `GET /api/config/automation/config/{id}` in
`crates/ha-api/src/automation.rs`.

1. Write an axum handler in the area's module (`rest.rs` for `/api/states`
   and `/api/services`, or a module of its own), returning HA's JSON and
   status codes. Get the caller with
   `Option<axum::Extension<ha_auth::Principal>>` if you need it.
2. Add it to the module's `routes()` and merge that in `ha_api::router`
   (`lib.rs`): inside the `/api` group, which `auth::require_auth` protects,
   unless HA serves it without auth.
3. Give it a permission in `auth::required_permission` (`auth.rs`): `Admin`
   for anything that changes config, `Control` for service calls, `Read`
   otherwise.
4. Test it through the full router with a token, as the test
   `serves_the_entry_as_loaded_to_admins` in `automation.rs` does, including
   a non-admin and a missing token.

## Before you open the pull request

- The command is listed in the [roadmap](../ROADMAP.md) item you tick.
- If it runs per state change (a subscription), it is a hot path: add a
  bench or a `perf.sh` row ([PERF.md](../PERF.md#what-a-pull-request-must-show)).

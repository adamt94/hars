# Writing a hars plugin

`hars` plugins are standalone executables (or sidecar services) that talk to `hars` over newline-delimited JSON-RPC 2.0 (`stdin`/`stdout` or a Unix socket). They work in any language—static Rust/Go/Zig binaries install with one click inside `hars`'s `FROM scratch` Docker image, and Python/Node plugins can run natively or as a sidecar container.

## Directory layout

Place a plugin under `<config>/plugins/<domain>/`:

```text
<config>/plugins/my_plug/
  plugin.json            # required manifest (schema_version: 1)
  icon.png               # optional local brand icon
  translations/en.json   # optional HA-format strings (config flow & entity labels)
  icons.json             # optional HA-format entity/service icons
  bin/my_plug            # executable referenced by transport.command
```

## `plugin.json`

```json
{
  "schema_version": 1,
  "domain": "my_plug",
  "name": "My Smart Plug",
  "version": "0.1.0",
  "iot_class": "local_push",
  "config_flow": true,
  "transport": {
    "type": "stdio",
    "command": "bin/my_plug",
    "args": []
  },
  "capabilities": {
    "entity_domains": ["switch", "light", "button", "scene", "vacuum", "sensor", "binary_sensor"],
    "config_entry_version": 1
  }
}
```

For a sidecar container (e.g. Python/Node), set `transport` to:

```json
{ "type": "unix_socket" }
```

Your sidecar listens on `<config>/plugins/run/<domain>.sock` and speaks the exact same NDJSON JSON-RPC 2.0 protocol.

## Wire protocol (NDJSON JSON-RPC 2.0)

Every message is one JSON object followed by `\n` (max 256 KiB per line).

### Requests from `hars` → plugin

1. **`initialize`** — handshake sent immediately on connect:
   - params: `{"protocol_version": 1, "hars_version": "...", "domain": "my_plug", "data_dir": "..."}`
   - result: `{"protocol_version": 1, "plugin_version": "0.1.0"}`
2. **`flow.step`** — runs one step of the **Add integration** dialog:
   - params: `{"flow_id": "...", "step_id": "user", "input": null | {...}}`
   - result: one of:
     - `{"type": "form", "step_id": "user", "schema": [{"name": "host", "required": true, "kind": {"type": "string"}}], "errors": {}}`
     - `{"type": "create_entry", "title": "Desk Plug", "unique_id": "plug-1", "data": {"host": "192.168.1.50"}}`
     - `{"type": "abort", "reason": "cannot_connect"}`
3. **`flow.cancel`** — user closed the dialog (`{"flow_id": "..."}`).
4. **`entry.setup`** — start running one config entry (`{"entry_id": "...", "title": "...", "data": {...}, "options": {...}}`).
5. **`entry.unload`** — stop managing an entry (`{"entry_id": "..."}`).
6. **`entity.call`** — user or automation called a service (`{"entry_id": "...", "unique_id": "...", "domain": "switch", "service": "turn_on", "data": {...}}`).
7. **`shutdown`** — graceful exit before `hars` stops.

### Notifications / requests from plugin → `hars`

Send without `"id"` for fire-and-forget, or include `"id"` to receive `{"result": ...}` / `{"error": ...}`:

- **`device.upsert`**: `{"entry_id", "identifier", "name", "manufacturer", "model", "sw_version", "suggested_area"}` (devices are matched strictly by `(domain, identifier)` so plugins cannot collide with other integrations)
- **`entity.upsert`**: `{"entry_id", "unique_id", "domain", "device_identifier", "name", "original_icon", "device_class", "unit_of_measurement", "supported_features", "supported_color_modes": [...]}`
- **`state.set`**: `{"entry_id", "unique_id", "state", "attributes": {...}}`
- **`entry.status`**: `{"entry_id", "state": "loaded" | "setup_retry" | "setup_error", "reason": "..."}`
- **`log`**: `{"level": "info" | "warn" | "error" | "debug", "message": "..."}`

## Installing and testing

- **From a local folder:** `HARS_CONFIG=./config hars plugin install ./my_plug`
- **From a bundle URL:** `HARS_CONFIG=./config hars plugin install https://example.com/my_plug.json`
- **In the UI:** **Settings → Devices & services → Add integration → Hars Plugins** (zero restart required).

See [`plugins/roborock/`](../../plugins/roborock/README.md) for a complete
working plugin supporting both `stdio` and Docker `unix_socket` transports.

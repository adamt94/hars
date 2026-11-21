# 0011: Out-of-process plugins over NDJSON JSON-RPC

Status: accepted (2026-11-21)

## Context

[ADR 0005](0005-integrations-compiled-in.md) compiles built-in integrations directly into `hars` with no Python interpreter. That keeps the binary small (≤ 15 MiB) and idle memory low (≤ 20 MiB), and remains the right model for core protocols and widely used integrations.

However, the smart-home ecosystem has a long tail of thousands of devices, niche APIs, and fast-moving vendor protocols that cannot all live in one repository or wait for a core release. Users need a simple way to install, run, and remove third-party integrations without forking `hars` or recompiling Rust.

Dynamic shared libraries (`.so`/`.dylib`) are ruled out by `unsafe` denial, Rust ABI instability, crash isolation, and the static musl `FROM scratch` Docker image. Embedded WebAssembly runtimes such as `wasmtime` add 10–25 MiB to the release binary (exceeding the entire binary budget) and complicate raw LAN networking (UDP broadcasts, mDNS, self-signed TLS).

## Decision

`hars` supports out-of-process plugins under `<config>/plugins/<domain>/` communicating over newline-delimited JSON-RPC 2.0:

- **Two transports, one protocol:**
  1. `stdio`: `hars` spawns a static executable on demand and talks over `stdin`/`stdout` (works inside `FROM scratch` Docker images).
  2. `unix_socket`: a sidecar process or companion container connects through `<config>/plugins/run/<domain>.sock` (allows Python, Node, or C++ SDK plugins without putting any interpreter in `hars`).
- **Lazy, zero-idle lifecycle:** installing a plugin reads only its `plugin.json` metadata, translations, icons, and optional brand image. No process is started until a user opens its config flow or enables a config entry. Removing the last entry stops the process cleanly.
- **Native Home Assistant UI integration:** installed plugins are served under `custom.integration` in `integration/descriptions`, `manifest/list`, `manifest/get`, `frontend/get_translations`, `frontend/get_icons`, `/api/brands/integration/{domain}/{image}`, and `/api/config/config_entries/flow*`. A built-in `hars_plugins` config flow lets users install from a SHA-256-verified catalog, install from a bundle URL, or remove plugins right inside **Settings → Devices & services → Add integration** without restarting `hars`. CLI subcommands (`hars plugin list|install|remove`) provide the same operations offline.
- **Strict domain and capability scoping:** a plugin can only manage devices, entities, and states attached to its own active config entries and entity domains declared in `plugin.json`. Frames are bounded to 256 KiB and outboxes to 256 messages. Crashed plugins have their entities marked `unavailable`, entries moved to `setup_retry`, and are restarted on HA's backoff ladder (5 s doubling to 600 s).

## Consequences

- `hars` gains full third-party extensibility with ~40 KiB of code, zero new crate dependencies, zero `unsafe`, and 0 B / 0 wakeups overhead when no plugins are running.
- Plugins can be written in any language (static Rust/Go/Zig binaries for 1-click install, or Python/Node sidecars over Unix sockets).
- Core and widely used integrations still belong compiled into `ha-integrations` so common homes stay single-binary and maximally efficient.

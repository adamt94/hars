# 0006: TLS roots compiled into the binary

Status: accepted (2026-11-14)

## Context

Integrations such as `met` call HTTPS services. The client needs root
certificates. Loading the system's CA bundle (`rustls-native-certs`) is
blocking file I/O at start, needs a CA bundle in the image, and measured
0.5 MiB more anonymous memory than compiled-in roots.

## Decision

`ha-http` uses rustls with Mozilla's roots compiled in (`webpki-roots`), as
Home Assistant ships `certifi`. The Docker image carries no CA bundle.

## Consequences

- No file I/O for TLS setup and a smaller idle footprint.
- The roots update when hars is rebuilt, not when the host updates. A
  release must not lag far behind `webpki-roots`.
- Integrations that create a client per fetch (`met`, hourly) hold no TLS
  state between fetches.

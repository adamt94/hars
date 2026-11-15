# 0004: Serve the official frontend from disk, not embedded

Status: accepted (2026-11-14)

## Context

The official frontend is about 234 MB unpacked (without source maps). It
could be embedded in the binary for a single-file install, but it would
dwarf everything else, and the binary budget (15 MiB) exists to keep mapped
pages and start-up small.

## Decision

The frontend is served from a directory (`HARS_FRONTEND`) with its
precompressed `.br` and `.gz` files and HA's cache headers. The Docker image
fetches the pinned wheel, checks its sha256 and ships its `hass_frontend`
folder next to the binary. Small, hot data (translations, icons, manifests)
is compiled in, because it is read only while a request is answered and
costs file-backed pages, not heap.

## Consequences

- The binary stays small; the image is mostly the frontend.
- Running the bare binary needs the frontend folder for the UI; without it,
  only the API is served.

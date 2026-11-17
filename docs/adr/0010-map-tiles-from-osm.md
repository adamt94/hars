# 0010: Map tiles from OpenStreetMap, relayed by hars

Status: accepted (2026-11-17)

## Context

The pinned frontend (20260527.4) draws every map on CARTO's raster tiles.
CARTO has since started requiring an API key and stamps "API KEY REQUIRED"
over every tile, so HA 2026.6.1's own maps are broken in the same way
(home-assistant/frontend#53800). HA 2026.9 moved to OpenStreetMap vector
tiles drawn by MapLibre (frontend#53816). That change is a 5.4 MB asset set
plus a new renderer, and it comes with a frontend that expects a newer core.

OSM's raster tiles have no overlay, but they block a browser that sends no
`Referer` (with an "Access blocked" PNG and HTTP 200). HA's `index.html`
sets `referrer: same-origin`, so the browser sends none to another site.
It does send one to its own origin. OSM's tile policy asks for a clear
application `User-Agent`, and asks a relay to keep the website's
`Referer`.

## Decision

- `hars` serves `GET /hars/map_tiles/{z}/{x}/{y}` (`ha-api/src/map_tiles.rs`).
  It fetches the tile from `tile.openstreetmap.org` as `hars/<version>`, with
  at most two connections, as OSM's tile policy allows. The browser's
  `Referer` goes upstream cut to its origin (`http://<host>:8123/`), so the
  page's path stays local.
- It stores nothing. The upstream caching headers (`Cache-Control`, `ETag`,
  `Age`, `Date`, …) pass to the browser, so the browser caches the tiles, and its
  revalidations go upstream as conditional requests.
- The image build rewrites the frontend's one tile layer to use that URL,
  with OSM's attribution (`scripts/patch_frontend_map.py`). The script fails
  the build if a frontend upgrade moves or changes that layer.
- The route needs no login. Tiles load as plain images with no bearer token,
  and they are public data. Only valid OSM tile coordinates (zoom 0–19) are
  fetched, and only from OSM.

## Consequences

- Maps render again. Markers, zones, paths and dark mode (the frontend's CSS
  filter) are unchanged.
- This is a deliberate difference from the pinned frontend, which still has
  the broken CARTO layer. A browser that cached the old chunk (31-day cache)
  needs one hard refresh; the iOS app needs Settings → Companion App →
  Debugging → Reset frontend cache.
- Tiles are 256 px, so maps look softer on high-DPI screens than CARTO's
  `@2x` tiles did.
- Anyone who can reach `hars` can fetch OSM tiles through it. That's fine on
  a LAN. Once remote access exists, revisit it: put the route behind a token,
  or rate-limit it.
- Revisit when the frontend pin moves to 2026.9 or later. Its vector map
  replaces this, and the patch script will fail the build until it is
  removed.

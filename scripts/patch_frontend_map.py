#!/usr/bin/env python3
"""Point the pinned frontend's map tiles at hars's OpenStreetMap relay.

    scripts/patch_frontend_map.py <hass_frontend dir>

The frontend HA 2026.6.1 ships draws every map on CARTO raster tiles, and
CARTO now stamps "API KEY REQUIRED" over them (home-assistant/frontend
#53800; HA moved to OSM vector tiles in 2026.9). This rewrites the one
tile layer in each build (`frontend_latest`, `frontend_es5`) to load
`/hars/map_tiles/{z}/{x}/{y}` (crates/ha-api/src/map_tiles.rs) with OSM's
attribution and zoom range. Only the base layer changes. Markers, zones
and dark mode stay as they are.

Each patched chunk gets a fresh `.gz` and loses its `.br`, so no
precompressed copy serves the old code. If either build doesn't contain
the tile layer exactly once, the script exits non-zero, so a frontend
upgrade that moves it fails the image build.
"""

import gzip
import pathlib
import re
import sys

BUILDS = ("frontend_latest", "frontend_es5")

# `L.tileLayer(<url>, <options>)` as both builds minify it; `(\w+)` is the
# Leaflet module's minified name.
CARTO = re.compile(
    r'"https://basemaps\.cartocdn\.com/rastertiles/voyager/\{z\}/\{x\}/\{y\}"'
    r'\+\((\w+)\.Browser\.retina\?"@2x\.png":"\.png"\),'
    r"\{attribution:'[^']*',subdomains:\"abcd\",minZoom:0,maxZoom:20\}"
)
# Same zoom range for the map; OSM serves up to 19, and Leaflet scales up
# past it.
OSM = (
    '"/hars/map_tiles/{z}/{x}/{y}",'
    "{attribution:'&copy; <a href=\"https://www.openstreetmap.org/copyright\">"
    "OpenStreetMap</a> contributors',minZoom:0,maxZoom:20,maxNativeZoom:19}"
)


def patch(root: pathlib.Path) -> list[pathlib.Path]:
    """Patch both builds under `root`; return the files changed."""
    changed = []
    for build in BUILDS:
        hits = []
        for js in sorted((root / build).glob("*.js")):
            text = js.read_text(encoding="utf-8")
            if "basemaps.cartocdn.com" in text:
                hits.append((js, text))
        if len(hits) != 1:
            names = [h[0].name for h in hits]
            raise SystemExit(f"{build}: expected one CARTO tile layer, found {names}")
        js, text = hits[0]
        new, n = CARTO.subn(OSM, text)
        if n != 1 or "cartocdn" in new:
            raise SystemExit(f"{js}: the CARTO tile layer isn't the expected code")
        js.write_text(new, encoding="utf-8")
        js.with_name(js.name + ".gz").write_bytes(
            gzip.compress(new.encode("utf-8"), compresslevel=9, mtime=0)
        )
        js.with_name(js.name + ".br").unlink(missing_ok=True)
        changed.append(js)
    return changed


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    for js in patch(pathlib.Path(sys.argv[1])):
        print(f"patched {js}")


if __name__ == "__main__":
    main()

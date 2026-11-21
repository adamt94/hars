#!/usr/bin/env python3
"""LIFX's products as aiolifx 1.2.1 knows them, as a Rust table.

    .venv/bin/python -I scripts/vectors/lifx_products.py
    cargo test -p ha-integrations lifx

Writes crates/ha-integrations/src/lifx/products.rs from aiolifx's
`products_defs.product_map` (the model HA gives a device) and
`features_map` (what HA's `lifx_features` reads: colour, the kelvin range,
multizone, matrix, relays, HEV, infrared, buttons). HA uses the table as it
is, whatever the bulb's firmware, and so does `hars`. Run it twice: the
output must not change.
"""

import importlib.metadata
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
OUT = ROOT / "crates/ha-integrations/src/lifx/products.rs"
AIOLIFX = "1.2.1"
FLAGS = ["color", "multizone", "extended_multizone", "matrix", "chain", "relays", "hev",
         "infrared", "buttons"]


def main():
    found = importlib.metadata.version("aiolifx")
    if found != AIOLIFX:
        sys.exit(f"aiolifx is {found}, the table is pinned to {AIOLIFX}")
    from aiolifx.products_defs import features_map, product_map

    lines = []
    for pid in sorted(product_map):
        f = features_map[pid]
        flags = " | ".join(name.upper() for name in FLAGS if f.get(name)) or "0"
        kelvin = (f"Some(({f['min_kelvin']}, {f['max_kelvin']}))"
                  if f.get("min_kelvin") is not None else "None")
        name = product_map[pid].replace("\\", "\\\\").replace('"', '\\"')
        lines.append(f'    Product {{ id: {pid}, name: "{name}", flags: {flags}, kelvin: {kelvin} }},')
    consts = "\n".join(f"pub const {name.upper()}: u16 = 1 << {i};"
                       for i, name in enumerate(FLAGS))
    OUT.write_text(f"""//! LIFX's products as aiolifx {AIOLIFX} knows them (`products_defs`):
//! written by `scripts/vectors/lifx_products.py`, not by hand.

/// A product: its id, the model HA shows, its features (the flags below)
/// and its colour temperature range in kelvin, if it has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Product {{
    pub id: u32,
    pub name: &'static str,
    pub flags: u16,
    pub kelvin: Option<(u16, u16)>,
}}

{consts}

#[rustfmt::skip]
pub const PRODUCTS: &[Product] = &[
{chr(10).join(lines)}
];
""")


if __name__ == "__main__":
    main()

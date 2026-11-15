"""The crates depend on each other only downwards (docs/ARCHITECTURE.md).

    python3 -m unittest discover -s scripts

Fails when a crate's Cargo.toml names a workspace crate its layer may not
use, or when a crate is added without a layer here. Change LAYERS and the
diagram in docs/ARCHITECTURE.md together.
"""

import pathlib
import tomllib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]

# Lowest first. A crate may depend on crates in lower layers only.
LAYERS = [
    {"ha-core"},
    {"ha-auth", "ha-registry", "ha-http", "ha-recorder"},
    {"ha-components"},
    {"ha-integrations", "ha-automation"},
    {"ha-api"},
    {"ha-server"},
]


def layer_of(crate):
    for i, layer in enumerate(LAYERS):
        if crate in layer:
            return i
    return None


def workspace_deps(manifest):
    """Workspace crates named in [dependencies] and [dev-dependencies]."""
    data = tomllib.loads(manifest.read_text())
    names = set()
    for table in ("dependencies", "dev-dependencies", "build-dependencies"):
        names |= {n for n in data.get(table, {}) if n.startswith("ha-")}
    return data["package"]["name"], names


class CrateLayers(unittest.TestCase):
    def test_every_crate_has_a_layer(self):
        for manifest in sorted(ROOT.glob("crates/*/Cargo.toml")):
            name, _ = workspace_deps(manifest)
            self.assertIsNotNone(layer_of(name), f"{name} has no layer in {__file__}")

    def test_dependencies_point_down(self):
        for manifest in sorted(ROOT.glob("crates/*/Cargo.toml")):
            name, deps = workspace_deps(manifest)
            mine = layer_of(name)
            for dep in sorted(deps):
                theirs = layer_of(dep)
                self.assertIsNotNone(theirs, f"{name} depends on {dep}, which has no layer")
                self.assertLess(
                    theirs, mine, f"{name} (layer {mine}) may not depend on {dep} (layer {theirs})"
                )

    def test_a_layer_violation_is_caught(self):
        # The check itself: ha-core using ha-api must fail.
        self.assertFalse(layer_of("ha-api") < layer_of("ha-core"))


if __name__ == "__main__":
    unittest.main()

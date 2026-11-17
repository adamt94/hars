"""Tests for patch_frontend_map.py, on the tile layer as the pinned
frontend (20260527.4) minifies it in each build."""

import gzip
import importlib.util
import pathlib
import tempfile
import unittest

HERE = pathlib.Path(__file__).parent
spec = importlib.util.spec_from_file_location("patch", HERE / "patch_frontend_map.py")
patch = importlib.util.module_from_spec(spec)
spec.loader.exec_module(patch)

ATTRIBUTION = (
    "'&copy; <a href=\"https://www.openstreetmap.org/copyright\">OpenStreetMap</a>, "
    "&copy; <a href=\"https://carto.com/attributions\">CARTO</a>'"
)
LAYER = (
    's=t=>t.tileLayer("https://basemaps.cartocdn.com/rastertiles/voyager/{z}/{x}/{y}"'
    '+(t.Browser.retina?"@2x.png":".png"),{attribution:' + ATTRIBUTION + ','
    'subdomains:"abcd",minZoom:0,maxZoom:20})},3894(t,e,i){'
)
PATCHED = (
    's=t=>t.tileLayer("/hars/map_tiles/{z}/{x}/{y}",{attribution:\'&copy; '
    '<a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> '
    "contributors',minZoom:0,maxZoom:20,maxNativeZoom:19})},3894(t,e,i){"
)


class PatchTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.tmp.name)
        for build in patch.BUILDS:
            d = self.root / build
            d.mkdir()
            (d / "1.abc.js").write_text("other code")
            (d / "12122.abc.js").write_text(LAYER)
            (d / "12122.abc.js.gz").write_bytes(gzip.compress(LAYER.encode()))
            (d / "12122.abc.js.br").write_bytes(b"old")

    def tearDown(self):
        self.tmp.cleanup()

    def test_rewrites_the_tile_layer_in_both_builds(self):
        changed = patch.patch(self.root)
        self.assertEqual(len(changed), 2)
        for build in patch.BUILDS:
            d = self.root / build
            self.assertEqual((d / "12122.abc.js").read_text(), PATCHED)
            self.assertEqual(
                gzip.decompress((d / "12122.abc.js.gz").read_bytes()).decode(), PATCHED
            )
            self.assertFalse((d / "12122.abc.js.br").exists())
            self.assertEqual((d / "1.abc.js").read_text(), "other code")

    def test_a_different_frontend_fails_the_build(self):
        # The layer moved to a second chunk.
        (self.root / "frontend_es5" / "2.abc.js").write_text(LAYER)
        with self.assertRaises(SystemExit):
            patch.patch(self.root)

    def test_changed_options_fail_the_build(self):
        js = self.root / "frontend_latest" / "12122.abc.js"
        js.write_text(LAYER.replace("maxZoom:20", "maxZoom:22"))
        with self.assertRaises(SystemExit):
            patch.patch(self.root)
        # Nothing half-done.
        self.assertIn("cartocdn", js.read_text())

    def test_no_tile_layer_fails_the_build(self):
        (self.root / "frontend_latest" / "12122.abc.js").write_text("x")
        with self.assertRaises(SystemExit):
            patch.patch(self.root)


if __name__ == "__main__":
    unittest.main()

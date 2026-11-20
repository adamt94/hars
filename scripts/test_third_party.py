"""THIRD-PARTY-LICENSES.txt lists exactly the registry crates in Cargo.lock.

    python3 -m unittest discover -s scripts

Fails when a dependency was added, removed or upgraded without running
`python3 scripts/gen-third-party.py` again.
"""

import pathlib
import re
import tomllib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]


def locked():
    """`name version` of each Cargo.lock package that has a source."""
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
    return sorted(f"{p['name']} {p['version']}" for p in lock["package"] if "source" in p)


def listed():
    """`name version` of each crate in the file's "Crates" section."""
    text = (ROOT / "THIRD-PARTY-LICENSES.txt").read_text(encoding="utf-8")
    crates = text.split("Crates\n------\n", 1)[1].split("Licence texts\n", 1)[0]
    return sorted(re.findall(r"^(\S+ \S+)\n  licence: ", crates, flags=re.M))


class ThirdPartyLicences(unittest.TestCase):
    def test_the_file_lists_the_locked_crates(self):
        self.assertEqual(
            listed(),
            locked(),
            "Cargo.lock changed: run python3 scripts/gen-third-party.py",
        )

    def test_every_crate_names_a_licence(self):
        text = (ROOT / "THIRD-PARTY-LICENSES.txt").read_text(encoding="utf-8")
        self.assertNotIn("licence: \n", text)


if __name__ == "__main__":
    unittest.main()

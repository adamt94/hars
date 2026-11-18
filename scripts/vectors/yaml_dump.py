#!/usr/bin/env python3
"""Vectors for the YAML writer.

    .venv-template/bin/python -I scripts/vectors/yaml_dump.py
    cargo test -p ha-core yaml::tests::dumps

Runs Home Assistant 2026.6.1's `homeassistant.util.yaml.dump`
(annotatedyaml 1.0.2's `dump`: PyYAML 6.0.3's `CSafeDumper`, so libyaml's
emitter, with `: null` at a line's end cut to `:`) and writes
crates/ha-core/src/yaml/vectors/dump.json:

- `values`: a Python value, in the typed form of `yaml/vectors.json`
  (`[type, payload]`), and the text HA writes for it;
- `documents`: a YAML text, loaded with HA's loader
  (`homeassistant.util.yaml.parse_yaml`) and written back: what the
  automation editor does to `automations.yaml` on every save.

Nothing is faked. Run it twice: the output must not change.
"""

import base64
import datetime as dt
import importlib.metadata
import json
import pathlib
import sys

PINNED = {"homeassistant": "2026.6.1", "annotatedyaml": "1.0.2", "PyYAML": "6.0.3"}
ROOT = pathlib.Path(__file__).resolve().parents[2]
OUT = ROOT / "crates/ha-core/src/yaml/vectors"
USER = ROOT / "crates/ha-core/src/yaml/user"

LONG = (
    "Turn on hallway lights when motion is detected below 30 lux, using dim overnight and "
    "gentle morning levels, then turn them off after 5 minutes with no motion."
)

STRINGS = [
    # What YAML 1.1 reads as something else must be quoted.
    "on", "off", "On", "OFF", "yes", "no", "y", "n", "Y", "N", "true", "True", "TRUE", "false",
    "False", "null", "Null", "NULL", "~", "",
    "23:00:00", "06:30:00", "09:30:00", "10:00:00", "00:05:00", "0:30", "1:30", "12:00", "24:00",
    "1:2:3", "190:20:30.15", "-1:30",
    "123", "-5", "+5", "0", "00", "0x1F", "0X1f", "017", "08", "0o17", "0b101", "1_000", "1.5",
    "-1.5", "1.", ".5", "1e3", "1E3", "1.0e+3", "1.5e-3", ".inf", "-.inf", ".Inf", ".nan", ".NaN",
    "NaN", "inf", "1__0", "_1", "+", "-", ".", "=", "<<",
    "2026-11-14", "2026-11-14T17:42:05+00:00", "2026-11-14 17:42:05", "2026-6-6", "2026-11-14t17:42:05Z",
    # Ordinary text.
    "light.turn_on", "binary_sensor.hallway_motion", "mdi:lightbulb", "below_horizon",
    "Kitchen lights - off at 11:30pm", "Hallway lights - motion on when dark",
    "Turn off the kitchen light and lamp every night at 11:30pm.",
    # Indicators.
    "a: b", "a:b", "a:", ":a", ": a", "a #b", "a#b", "#a", "# a", "- a", "-a", "- ", "? a", "?a",
    "!a", "&a", "*a", "|a", ">a", "%a", "@a", "`a", ",a", "[", "]", "{", "}", "'quoted'",
    '"dq"', "it's", "say \"hi\"", "it's \"both\"", "{a}", "[a]", "a, b", "a,b", "a [b]", "a {b}",
    "key: value # comment", "---", "--- a", "...", "... a", "{{ states('sun.sun') }}",
    "{% if is_state('light.hall', 'on') %}on{% endif %}", "a | b", "a > b", "50%", "a & b",
    "C:\\path\\file", "a\\nb", "a/b", "http://example.com:8123/x?y=1&z=2#frag",
    # Spaces, tabs and breaks.
    " leading", "trailing ", " both ", "two  spaces", "three   spaces", " ", "  ", "tab\there",
    "\ttab first", "tab last\t", "a \tb", "line1\nline2", "line1\nline2\n", "ends\n", "ends\n\n",
    "\nstarts", "\n", "\n\n", "a\n\nb", "a\n b", "a \nb", "a\n\tb", "  indented\nnot", "a\r\nb",
    "a\rb", "short multi\nline", "first\n  second indented\nthird",
    # Unicode and what isn't printable.
    "café", "Ünïcödé ✓", "😀 emoji", "emoji 😀 inside", "日本語", " nbsp", "nb sp",
    " sep", "a b", "a b", "a\u0085b", "\x07bell", "a\x00b", "a\x1bb", "a\x7fb", "\ufeffbom",
    "a\ufeffb", "\ue000 private", "\ufffd", "é", "\u00ad soft hyphen", "\u200b zero width",
    # Width: plain text folds at a space past column 80.
    "x" * 81, "x" * 200, LONG, LONG + " " + LONG,
    "A description with a newline\nin the middle that is also rather long so it might be "
    "folded by the emitter at eighty columns or so.",
    "averyveryveryveryveryveryveryveryveryveryveryveryveryveryveryveryveryveryverylongwordwit"
    "houtspaces_over_80",
    "word " * 40, ("word " * 40).strip(), "two  spaces " * 12, "a: b " * 30, "it's " * 30,
    LONG + "\tand a tab so it is double quoted, " + LONG,
    "😀 " + LONG, LONG + " \x07 " + LONG, "say \"hi\" " * 20, "back\\slash " * 20,
    # Past U+FFFF: libyaml escapes it, in 10 columns; a key's limit counts
    # its 4 bytes.
    "😀 " * 30, "😀" * 30, "😀" + "x" * 124, "😀" + "x" * 125, "x" * 120 + "😀😀", "x" * 121 + "😀😀",
    "\x01\x07\x80 😀", "\\x01\\a\\x80 😀", "𝒳 and 😀 and \U0010ffff", "😀\n😀", " 😀", "😀 ",
    LONG + "\n" + LONG, LONG + "\n\n  " + LONG + "\n", "'" + LONG + "'",
    ("x" * 79) + " y", ("x" * 80) + " y", ("x" * 77) + " y z", ("x" * 40) + "  " + ("y" * 60),
]

AWARE = dt.datetime(2026, 11, 14, 17, 42, 5, 123456,
                    tzinfo=dt.timezone(dt.timedelta(hours=-5)))

VALUES = [
    0, 1, -1, 12, 2202, 82800, 10 ** 20, -(10 ** 20), 2 ** 63, 0.1, 1.0, -1.0, 1.5, 100.0, 0.0,
    -0.0, 1e16, 1e17, 1e20, 1e-7, 1.5e-7, 1.5e300, 123456789012345680.0, 0.30000000000000004,
    float("inf"), float("-inf"), float("nan"), True, False, None,
    dt.date(2026, 11, 14), dt.datetime(2026, 11, 14, 17, 42, 5), dt.datetime(2026, 11, 14),
    dt.datetime(2026, 11, 14, 17, 42, 5, tzinfo=dt.UTC), AWARE,
    [], {}, [None], [None, None], {"a": None}, {"a": None, "b": None}, {"a": []}, {"a": {}},
    {"a": [None]}, {"a": {"b": None}}, [[]], [{}], [[], {}], {"a": [[]]},
    {"z": 1, "a": 2}, {1: "int key", True: "bool key"}, {1.5: "float key", None: "null key"},
    {"on": 1, "off": 2, "yes": 3, "": 4, "a b": 5, "a: b": 6, "it's": 7, "#a": 8},
    {"x" * 127: 1}, {"x" * 128: 1}, {"x" * 129: 1}, {"x" * 129: {"a": 1}}, {"x" * 129: [1]},
    {"multi\nline": 1}, {" lead": 1}, {"😀": 1}, {"a\tb": 1}, {"word " * 30: 1},
    {dt.date(2026, 11, 14): "date key"},
    {"list": ["a", "b"], "nested": {"list": [{"x": 1, "y": [1, 2]}]}},
    [[1, 2], [3]], [[[1]]], [[1, [2, [3]]]], [{"a": 1, "b": 2}, {"c": 3}], [{"a": [{"b": [1]}]}],
    [[{"a": 1}]], [{"a": {"b": {"c": {"d": {"e": LONG}}}}}], [LONG], [[LONG]], [{"a": [LONG]}],
    {"a": {"b": {"c": {"d": {"e": {"f": {"g": {"h": LONG}}}}}}}},
    {"a": "line1\nline2", "b": ["line1\nline2", {"c": "line1\nline2\n"}]},
    {"a": ["", " ", "\n"], "b": {"": ""}},
    {"to": "on", "at": "23:00:00", "after": "06:30:00", "below": 30, "transition": 1},
    {"entity_id": ["light.kitchen_kitchen_light", "light.bedroom_lamp"]},
    {"data": {"brightness_pct": 50, "rgb_color": [255, 0, 0], "transition": 0.5}},
    {"message": "It's {{ trigger.to_state.state }}°C in the lounge "
                "({{ states('sensor.lounge_humidity') }}% humidity)"},
    # A new automation, as the frontend posts it and HA orders it.
    [{"id": "1794680000000", "alias": "Hallway test: on at 50%", "description": "",
      "triggers": [{"trigger": "state", "entity_id": ["binary_sensor.hallway_motion"],
                    "to": "on", "for": {"hours": 0, "minutes": 0, "seconds": 5}}],
      "conditions": [], "actions": [{"action": "light.turn_on", "metadata": {},
                                     "data": {"brightness_pct": 50},
                                     "target": {"entity_id": "light.hallway_hallway"}}],
      "mode": "single"}],
]

# Documents that are loaded, then written.
DOCUMENTS = {
    "empty_list": "[]\n",
    "empty_file": "",
    "flow_becomes_block": "- {id: a, alias: A, triggers: [{trigger: time, at: '23:30:00'}]}\n",
    "quotes_requoted": '- to: "on"\n  at: "23:00:00"\n  after: "06:30:00"\n  x: \'plain\'\n',
    "unquoted_yaml11": "- at: 23:00:00\n  to: on\n  for: 5:00\n  n: 010\n  d: 2026-11-14\n"
                       "  t: 2026-11-14 17:42:05\n  z: 2026-11-14T17:42:05Z\n  f: 1e3\n  g: 1.e3\n",
    "comments_dropped": "# header\n- id: a # trailing\n  # inside\n  alias: A\n",
    "block_scalars": "- description: |\n    line one\n    line two\n  folded: >\n    folded text\n"
                     "    goes on\n  keep: |+\n    kept\n\n  strip: |-\n    stripped\n",
    "nulls": "- a:\n  b: null\n  c: ~\n  d: [null, ~]\n  e: {f: }\n",
    "nested_lists": "- - 1\n  - - 2\n    - 3\n- a:\n  - b:\n    - c\n",
    "long_description": "- id: a\n  description: " + LONG + "\n  actions:\n  - data:\n"
                        "      message: " + LONG + "\n",
    "anchors": "- &one {a: 1}\n- *one\n- x: &text some text\n  y: *text\n",
    "merge": "- base: &base {a: 1, b: 2}\n- <<: *base\n  b: 3\n",
    "duplicate_keys": "- a: 1\n  a: 2\n  b: 3\n",
    "non_string_keys": "- 1: int\n  true: bool\n  1.5: float\n  null: none\n  on: on\n",
    # A set of one: Python writes a larger one in hash order.
    "binary_and_sets": "- b: !!binary aGVsbG8=\n  s: !!set {a}\n  o: !!omap [{a: 1}]\n"
                       "  long: !!binary " + "QUJD" * 40 + "\n",
    "blueprint_input": "- a: !input some_input\n  b: [!input other]\n",
    "unicode": "- alias: \"caf\\xe9 \\U0001F600 \\u2713\"\n  bell: \"a\\ab\"\n",
}


def check_versions():
    for pkg, want in PINNED.items():
        have = importlib.metadata.version(pkg)
        if have != want:
            sys.exit(f"{pkg} is {have}; these vectors are made with {want}")


def enc(v):
    """The typed form of `yaml/vectors.json`: `[type, payload]`."""
    if v is None:
        return ["null", None]
    if isinstance(v, bool):
        return ["bool", v]
    if isinstance(v, int):
        return ["int", str(v)]
    if isinstance(v, float):
        return ["float", repr(v)]
    if isinstance(v, str):
        return ["str", str(v)]
    if isinstance(v, (bytes, bytearray)):
        return ["bytes", base64.b64encode(bytes(v)).decode()]
    if isinstance(v, dt.datetime):
        return ["datetime", v.isoformat()]
    if isinstance(v, dt.date):
        return ["date", v.isoformat()]
    if isinstance(v, dict):
        return ["map", [[enc(k), enc(x)] for k, x in v.items()]]
    if isinstance(v, list):
        return ["seq", [enc(x) for x in v]]
    if isinstance(v, tuple):
        return ["tuple", [enc(x) for x in v]]
    if isinstance(v, (set, frozenset)):
        return ["set", sorted((enc(x) for x in v), key=repr)]
    sys.exit(f"no typed form for {type(v).__name__}")


def main():
    check_versions()
    from homeassistant.util.yaml import dump, parse_yaml

    def written(value):
        try:
            return {"dump": dump(value)}
        except Exception as err:  # noqa: BLE001
            return {"error": type(err).__name__}

    values = []
    for s in STRINGS:
        # As a mapping's value, a list's item and a mapping's key.
        for shape in ({"v": s}, [s], {s: 1}):
            values.append({"typed": enc(shape), **written(shape)})
    for v in VALUES:
        for shape in ({"v": v}, [v]) if not isinstance(v, (list, dict)) else (v, {"v": v}, [v]):
            values.append({"typed": enc(shape), **written(shape)})
    documents = []
    texts = dict(DOCUMENTS)
    # The reference install's automations (its other files are empty or
    # include this one).
    texts["user/automations.yaml"] = (USER / "automations.yaml").read_text()
    for name, text in texts.items():
        loaded = parse_yaml(text)
        documents.append({"name": name, "yaml": text, **written(loaded)})
    OUT.mkdir(exist_ok=True)
    data = {"values": values, "documents": documents}
    (OUT / "dump.json").write_text(json.dumps(data, indent=1, ensure_ascii=False) + "\n")
    print(len(values), "values,", len(documents), "documents")


if __name__ == "__main__":
    main()

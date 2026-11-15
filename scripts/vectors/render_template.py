#!/usr/bin/env python3
"""Vectors for templates and the WebSocket `render_template` command.

    .venv/bin/python -I scripts/vectors/render_template.py

Runs HA 2026.6.1's `helpers/template` (Jinja2 3.1.6) and
`components/websocket_api/commands.py::handle_render_template`, and writes:

- crates/ha-core/src/template/vectors/render.json: each template rendered
  with `Template.async_render_to_info` (what `render_template` uses), with
  the result as HA's WebSocket would send it, the error, and what HA's
  `log_fn` was told.
- crates/ha-core/src/template/vectors/parse_result.json: rendered strings
  through `Template._parse_result` (`literal_eval` and HA's filters).
- crates/ha-core/src/template/vectors/names.json: the globals, filters and
  tests HA's template environment has.
- crates/ha-api/src/ws/vectors/render_template.json: whole conversations
  with `render_template` (and `unsubscribe_events`) on a connection, each
  on a fresh `HomeAssistant`, with every message HA sent back, in order.

Nothing outside HA is faked except the WebSocket transport: messages go
through `ActiveConnection.async_handle` and come back through its
`send_message`, serialised as HA's writer does.
"""

import asyncio
import importlib.metadata
import json
import logging
import pathlib
import re
import sys
import tempfile

PINNED = {"homeassistant": "2026.6.1", "jinja2": "3.1.6"}
ROOT = pathlib.Path(__file__).resolve().parents[2]
CORE = ROOT / "crates/ha-core/src/template/vectors"
API = ROOT / "crates/ha-api/src/ws/vectors"

# A template that takes long enough (a hundred thousand loop turns) that
# a nanosecond `timeout` always runs out first, and that stays inside
# hars's render budget.
SLOW = "{% for i in range(100000) %}{% endfor %}x"

# (template, variables, strict). Variables are what `render_template`
# passes; `None` is no `variables` key.
RENDER = [
    # The default dashboard's heading (hui-markdown-card).
    ("## Welcome {{ user }}", {"user": "Ann", "config": {"type": "markdown"}}, True),
    ("## Welcome {{ user }}", {"user": "Ann"}, False),
    ("## Welcome {{ user }}", None, True),
    ("## Welcome {{ user }}", None, False),
    # Static: no `{{`, `{%` or `{#`; stripped, never parsed.
    ("hello", None, False),
    ("  [1, 2]  ", None, False),
    ("  42 ", None, True),
    ("{ not a template }", None, False),
    ("", None, False),
    ("   ", None, False),
    # Template source is stripped before compiling, and the output after.
    ("  {{ 'a' }}  \n", None, False),
    ("{{ ' padded ' }}", None, False),
    ("x\n{{ 1 }}\n", None, False),
    # Results are parsed (literal_eval) unless they stay strings.
    ("{{ 1 + 1 }}", None, False),
    ("{{ 7 / 2 }}", None, False),
    ("{{ 10 / 2 }}", None, False),
    ("{{ 7 // 2 }}", None, False),
    ("{{ 7 % 3 }}", None, False),
    ("{{ 2 ** 10 }}", None, False),
    ("{{ -3 }}", None, False),
    ("{{ 0.1 + 0.2 }}", None, False),
    ("{{ 1e20 }}", None, False),
    ("{{ 1.5e-7 }}", None, False),
    ("{{ true }}", None, False),
    ("{{ false }}", None, False),
    ("{{ none }}", None, False),
    ("{{ 'abc' }}", None, False),
    ("{{ '007' }}", None, False),
    ("{{ '1e5' }}", None, False),
    ("{{ [1, 'a', none, true, {'b': 2.0}] }}", None, False),
    ("{{ {'b': 1, 'a': [1, 2]} }}", None, False),
    ("{{ [] }}", None, False),
    ("{{ {} }}", None, False),
    ("{{ 'it''s' }}", None, False),
    ('{{ "it\'s" }}', None, False),
    ("{{ ['it\\'s', 'say \"hi\"'] }}", None, False),
    ("{{ 'b' }}'x'", None, False),
    ("[{{ 1 }}, {{ 2 }}]", None, False),
    ("{{ 1 }}, {{ 2 }}", None, False),
    ("({{ 1 }})", None, False),
    ("{{ 1 }} + 2j", None, False),
    ("{{ '...' }}", None, False),
    ("{{ 'a' ~ 1 ~ 2.5 }}", None, False),
    ("{{ 'ab' * 3 }}", None, False),
    ("{{ 'é€' }}", None, False),
    ("{{ [1.0, 1e16, -0.0, 0.5, 1e-5] }}x", None, False),
    ("{{ {'a': 'it\\'s', 'b': none, 'c': [true, false]} }}x", None, False),
    ("{{ 2 ** 62 }}", None, False),
    ("{{ 'x' ~ 'y' }}", None, False),
    # Variables, as JSON gives them.
    ("{{ n }} {{ f }} {{ b }} {{ z }} {{ s }}", {"n": 3, "f": 2.5, "b": True, "z": None, "s": "x"}, False),
    ("{{ n }}", {"n": 3}, False),
    ("{{ f }}", {"f": 1.0}, False),
    ("{{ config }}", {"config": {"type": "markdown", "content": "hi", "n": [1, 2.0, None, False]}}, False),
    ("{{ config.type }}", {"config": {"type": "markdown"}}, True),
    ("{{ config['type'] }}", {"config": {"type": "markdown"}}, True),
    ("{{ items }}", {"items": ["a", "it's", 'say "hi"', "tab\there", "nl\nx"]}, False),
    ("{{ items[1] }}", {"items": [10, 20]}, False),
    ("{{ items | length }}", {"items": [10, 20]}, False),
    ("{% for i in items %}{{ i }},{% endfor %}", {"items": [1, 2, 3]}, False),
    ("{% for k in config %}{{ k }}={{ config[k] }};{% endfor %}", {"config": {"a": 1, "b": 2}}, False),
    ("{{ range }}", {"range": 5}, False),
    ("{{ states }} {{ now }}", {"states": "s", "now": 1}, True),
    # Undefined: printed as "" (non-strict, warns) or an error (strict).
    ("{{ user }}x", None, False),
    ("{{ user }}x", None, True),
    ("{{ config.missing }}x", {"config": {"type": "markdown"}}, False),
    ("{{ config.missing }}x", {"config": {"type": "markdown"}}, True),
    ("{{ config['missing'] }}x", {"config": {}}, True),
    ("{{ config[0] }}x", {"config": {}}, True),
    ("{{ x.y }}", None, False),
    ("{{ x.y }}", None, True),
    ("{{ x.y.z }}", {"x": {}}, False),
    ("{{ x is defined }}", None, True),
    ("{{ x is undefined }}", None, False),
    ("{{ x | default('d') }}", None, True),
    ("{{ x | default('d') }}", None, False),
    ("{{ config.missing | default('d') }}", {"config": {}}, True),
    ("{% if x %}yes{% else %}no{% endif %}", None, True),
    ("{% if x is defined and x %}yes{% else %}no{% endif %}", None, True),
    ("{% for i in x %}{{ i }}{% endfor %}done", None, True),
    ("{{ x() }}", None, False),
    ("{{ x + 1 }}", None, True),
    # Statements HA's environment enables (loopcontrols, do).
    ("{% for i in [1, 2, 3, 4] %}{% if i == 3 %}{% break %}{% endif %}{{ i }}{% endfor %}", None, False),
    ("{% for i in [1, 2, 3, 4] %}{% if i is even %}{% continue %}{% endif %}{{ i }}{% endfor %}", None, False),
    ("{% set ns = namespace(n=0) %}{% for i in [1, 2, 3] %}{% set ns.n = ns.n + i %}{% endfor %}{{ ns.n }}", None, False),
    ("{% set x = 5 %}{{ x * 2 }}", None, True),
    ("{% macro hi(n) %}Hi {{ n }}{% endmacro %}{{ hi('Ann') }}", None, False),
    ("{%- if true -%}  a  {%- endif -%}", None, False),
    ("{% for i in items %}{{ loop.index }}/{{ loop.length }}{{ ',' if not loop.last }}{% endfor %}", {"items": ["a", "b"]}, False),
    ("{{ dict(a=1) }}", None, False),
    ("{{ range(3) | list }}", None, False),
    ("{{ range(100001) | list | length }}", None, False),
    ("{# a comment #}x", None, False),
    ("{% raw %}{{ x }}{% endraw %}", None, False),
    ("<b>{{ '<i>' }}</b>", None, False),
    # Jinja's own filters and tests hars keeps.
    ("{{ 'hello world' | upper }} {{ 'ABC' | lower }}", None, False),
    ("{{ 'hello wORLD' | title }} {{ 'hello WORLD' | capitalize }}", None, False),
    ("{{ '  x  ' | trim }}|", None, False),
    ("{{ 'aXbX' | replace('X', '-') }}", None, False),
    ("{{ [3, 1, 2] | sort }} {{ [3, 1, 2] | sort(reverse=true) }}", None, False),
    ("{{ [1, 2, 3] | first }} {{ [1, 2, 3] | last }}", None, False),
    ("{{ [1, 2, 3] | join(', ') }}", None, False),
    ("{{ [1, 2, 3] | sum }} {{ [1, 2, 3] | min }} {{ [1, 2, 3] | max }}", None, False),
    ("{{ [1, 2, 2, 1] | unique | list }}", None, False),
    ("{{ [1, 2, 3] | reverse | list }}", None, False),
    ("{{ -3 | abs }}", None, False),
    ("{{ 'abc' | length }} {{ 'abc' | count }}", None, False),
    ("{{ 'abc' | list }}", None, False),
    ("{{ [1, 2, 3, 4] | select('odd') | list }}", None, False),
    ("{{ [1, 2, 3, 4] | reject('odd') | list }}", None, False),
    ("{{ users | selectattr('on') | map(attribute='n') | list }}", {"users": [{"n": "a", "on": True}, {"n": "b", "on": False}]}, False),
    ("{{ users | rejectattr('on') | map(attribute='n') | join }}", {"users": [{"n": "a", "on": True}, {"n": "b", "on": False}]}, False),
    ("{{ [1, 2, 3, 4, 5] | batch(2) | list }}", None, False),
    ("{{ {'a': 1, 'b': 2} | items | list }}", None, False),
    ("{{ {'b': 1, 'a': 2} | dictsort }}", None, False),
    ("{{ ['b', 'A', 'a', 'B'] | sort }}", None, False),
    ("{{ ['b', 'A', 'a', 'B'] | sort(case_sensitive=true) }}", None, False),
    ("{{ users | sort(attribute='n') | map(attribute='n') | list }}", {"users": [{"n": "b"}, {"n": "a"}]}, False),
    ("{{ ['a', 'A', 'b'] | unique | list }}", None, False),
    ("{{ ['b', 'A', 'a'] | min }} {{ ['b', 'A', 'a'] | max }}", None, False),
    ("{{ {'b': 1, 'A': 2, 'a': 3} | dictsort }}", None, False),
    ("{{ [1, 2, 3, 4, 5] | slice(2) | list }}", None, False),
    ("{{ [1, 2, 3] | batch(2, 0) | list }}", None, False),
    ("{{ \"it's o'neil mc-x\" | title }}", None, False),
    ("{{ 'xxhixx' | trim('x') }}", None, False),
    ("{{ 'aaa' | replace('a', 'b', 2) }}", None, False),
    ("{{ users | join(', ', attribute='n') }}", {"users": [{"n": "a"}, {"n": "b"}]}, False),
    ("{{ '' | default('d') }} {{ '' | default('d', true) }}", None, False),
    ("{% set e = [] %}{{ e | first }}x{{ e | last }}", None, False),
    ("{{ '%s-%d' | format('a', 3) }}", None, False),
    ("{{ [1, 2] | pprint }}", None, False),
    ("{{ users | groupby('g') | list }}", {"users": [{"g": 1, "n": "a"}, {"g": 1, "n": "b"}]}, False),
    ("{{ 'a\nb' | indent(2) }}", None, False),
    ("{{ [1, 2, 3] | map('string') | list }}", None, False),
    ("{{ ['a', 'b'] | map('upper') | join }}", None, False),
    ("{{ [1, 2] | sum(start=10) }}", None, False),
    ("{{ users | map(attribute='n') | list }}", {"users": [{"n": "a"}, {}]}, False),
    ("{{ 5 | string }}", None, False),
    ("{{ true | string }}", None, False),
    ("{{ 'x' | e }}", None, False),
    ("{{ '<a href=\"x\">\\'&' | e }}", None, False),
    ("{{ '<b>' | safe }}{{ '<i>' | e | e }}", None, False),
    ("{{ [1, '<'] | e }}", None, False),
    ("{{ 3 is odd }} {{ 4 is divisibleby 2 }} {{ 'a' is string }} {{ 1 is number }}", None, False),
    ("{{ none is none }} {{ [1] is sequence }} {{ {} is mapping }}", None, False),
    ("{{ 1 in [1, 2] }} {{ 'a' in 'abc' }}", None, False),
    ("{{ 'abc'[1] }} {{ 'abcdef'[1:3] }}", None, False),
    # HA's functions, filters and tests that hars hasn't got yet: an
    # error, never a wrong value.
    ("{{ states('sun.sun') }}", None, False),
    ("{{ states.sun.sun.state }}", None, False),
    ("{{ now().year > 2000 }}", None, False),
    ("{{ is_state('sun.sun', 'above_horizon') }}", None, False),
    ("{{ 1 | float }}", None, False),
    ("{{ '1' | int }}", None, False),
    ("{{ 2.5 | round }}", None, False),
    ("{{ 'x' is match('x') }}", None, False),
    ("{{ 'abc' | truncate(2) }}", None, False),
    ("{{ cycler }}", None, False),
    # Unknown to HA too.
    ("{{ 1 | nofilter }}", None, False),
    ("{{ 1 is notest }}", None, False),
    ("{{ 'a,b' | split(',') }}", None, False),
    ("{{ nofunction() }}", None, False),
    # Syntax errors.
    ("{{ 1 + }}", None, False),
    ("{{ x", None, False),
    ("{% if x %}", None, False),
    ("{% for %}", None, False),
    ("{% foo %}", None, False),
    ("{% endif %}", None, False),
    ("{{ }}", None, False),
    ("{{ 'abc }}", None, False),
    ("{{ 1 2 }}", None, False),
    ("{% if true %}{% endfor %}", None, False),
    ("{% if a %}{% for x in y %}{% endif %}", None, False),
    ("{% for x in y %}{% else %}{% else %}{% endfor %}", None, False),
    ("{% for x in y %}{% elif %}{% endfor %}", None, False),
    ("{% if a %}{% else %}{% elif b %}{% endif %}", None, False),
    ("{% set x %}abc", None, False),
    ("{% macro m() %}x", None, False),
    ("{% else %}", None, False),
    ("{% if a %}{% raw %}{% endif %}{% endraw %}", None, False),
    ("{% if a %}{{ '%}' }}{# {% endif %} #}", None, False),
    ("{%- if a -%}x", None, False),
    ("{% with %}{% if a %}{% endwith %}", None, False),
    # Runtime errors.
    ("{{ 1 / 0 }}", None, False),
    ("{{ 1 + 'a' }}", None, False),
    # Output over 256 KiB.
    ("{% for i in range(65537) %}abcd{% endfor %}", None, False),
    ("{% for i in range(65536) %}abcd{% endfor %}", None, False),
]

# Strings `async_render` produced, through `_parse_result`.
PARSE = [
    "1", "-1", "+5", "1.0", "1.", ".5", "-0", "-0.0", "007", "00", "0", "1_000", "1e5", "1E5",
    "0x1F", "0o17", "0b11", "inf", "nan", "1e999", "123456789012345678901234567890",
    "9223372036854775807", "9223372036854775808", "18446744073709551615", "18446744073709551616",
    "-9223372036854775808", "-9223372036854775809",
    "True", "False", "None", "true", "none", "abc", "'abc'", '"abc"', "'a' 'b'", "b'x'",
    "...", "1j", "1+2j", "(1)", "(1,)", "()", "1, 2", "[1, 2,]", "[1, [2, (3, 4)]]",
    "{'a': 1, 'b': [True, None]}", "{1: 'a', 2.5: 'b', True: 'c', None: 'd'}", "{1: 'a', True: 'b'}",
    "{'a': 1, 'a': 2}", "{(1, 2): 3}", "{[1]: 2}", "{1, 2}", "set()", "{}", "[]",
    "[1e999]", "[1+2j]", "[b'x']", "[...]", "[123456789012345678901234567890]",
    "[-1, +2, -3.5, - 4]", "[0x10, 0o10, 0b10, 1_0]", "[1e3, 1.5e-7, 2E+2]",
    "['a\\nb', 'tab\\t', '\\x41\\u00e9\\U0001F600', '\\101', 'q\\'s', \"d\\\"q\", 'back\\\\slash']",
    "[r'a\\nb', u'x', R'y']", "['''tri''', \"\"\"ple\"\"\"]", "['\\N{BULLET}']", "['a' 'b', \"c\" 'd']",
    "['unknown \\q escape']", "[f'x']", "[1 + 2]", "[1 - 2j]", "[-(1)]", "[not True]",
    "[1, 2] # comment", "[1,\n 2]", " [1]", "[1]\n", "[1] [2]", "{'a': }", "[", "'", "''",
    "[1, 'it\\'s', \"say \\\"hi\\\"\"]", "[0.1, 100.0, 1e16, 1e-5, 123.456]",
    "['é€', '\\u20ac']", "[True, False, None]", "{'nested': {'deep': [1, {'x': ()}]}}",
    "-", "+", "1-", "--1", "-+1", "+-1", "- 1", "1 ", " 1", "1\n", "(1, 2)", "((1, 2),)",
    "[1 for x in y]", "x", "[x]", "{**a}", "[*a]", "1 if 1 else 2", "lambda: 1", "__import__('os')",
]


# Every Jinja filter and test hars serves, over values of every kind
# (`x` is undefined). Where HA answers, hars must give the same answer;
# where HA raises, hars must fail too.
VALUES = [
    "'abc'", "''", "'aBc dEf'", "0", "1", "2", "-3", "1.5", "true", "false", "none",
    "[]", "[3, 1, 2]", "['b', 'A', 'a']", "{}", "{'b': 1, 'a': 2}", "(1, 2)", "range(3)", "x",
]
TESTS = [
    "boolean", "defined", "undefined", "even", "odd", "false", "true", "float", "integer",
    "iterable", "lower", "upper", "mapping", "none", "number", "sequence", "string",
    "escaped", "divisibleby 2", "eq 1", "equalto 1", "ne 1", "lt 2", "lessthan 2", "le 1",
    "gt 1", "greaterthan 1", "ge 1", "in [1, 'abc', none]", "in 'abcd'", "sameas none",
    "sameas true",
]
OPERATOR_TESTS = ["==", "!=", "<", "<=", ">", ">="]
FILTERS = [
    "abs", "capitalize", "count", "first", "last", "length", "list", "lower", "upper",
    "max", "min", "reverse", "sort", "sum", "title", "trim", "unique", "items",
    "dictsort", "e", "escape", "string", "safe", "d('D')", "default('D', true)",
    "join", "join('-')", "batch(2)", "slice(2)", "replace('a', 'X')", "indent(2)",
    "map('string')", "select('odd')", "reject('odd')", "attr('a')", "sort(reverse=true)",
    "sort(case_sensitive=true)", "unique(case_sensitive=true)", "dictsort(by='value')",
    "max(case_sensitive=true)", "first | string", "replace('a', 'X', 1)",
    "trim('a')", "indent(2, true)", "batch(2, 0)", "slice(2, 0)",
]
# The value goes through `set` first: Jinja folds a filter or test on a
# literal at compile time, in HA's lenient environment whatever the
# strictness, which no template with real data sees.
KEPT = (
    [f"{{% set v = {v} %}}{{{{ v is {t} }}}}" for t in TESTS for v in VALUES]
    + [f"{{% set v = {v} %}}{{{{ [v] | select('{t}', 1) | list }}}}" for t in OPERATOR_TESTS for v in VALUES]
    + [f"{{% set v = {v} %}}{{{{ v | {f} }}}}" for f in FILTERS for v in VALUES]
    + [
        "{{ users | map(attribute='n') | list }}",
        "{{ users | map(attribute='n', default='-') | list }}",
        "{{ users | selectattr('n', 'eq', 'a') | list }}",
        "{{ users | rejectattr('n') | list }}",
        "{{ users | sort(attribute='n') | map(attribute='n') | list }}",
        "{{ users | sort(attribute='n,g') | map(attribute='n') | list }}",
        "{{ users | unique(attribute='g') | list }}",
        "{{ users | min(attribute='n') }}",
        "{{ users | max(attribute='n') }}",
        "{{ users | sum(attribute='g') }}",
        "{{ users | sum(attribute='g', start=10) }}",
        "{{ users | join(',', attribute='n') }}",
        "{{ users | groupby('g') | list }}",
        "{{ users | groupby('g', default=0) | list }}",
        "{{ users | groupby('g') | map(attribute='grouper') | list }}",
        "{{ users | groupby('g') | map(attribute='list') | list }}",
        "{{ users | dictsort }}",
        "{{ users[0] | dictsort }}",
        "{{ users[0] | items | list }}",
        "{{ users[0] | attr('n') }}",
        "{{ users[0] | length }}",
        "{{ users | first }}",
        "{{ 'a\n\nb' | indent(2, blank=true) }}",
        "{{ 'a\nb' | indent('> ') }}",
        "{{ [1.5, 2] | sum }}",
        "{{ [1, 'a'] | sort }}",
        "{{ [[2], [1]] | sort }}",
        "{{ 'b,a' | list | sort }}",
        "{{ [none, 1] | join }}",
        "{{ [true, 1.0, 1e20] | join(' ') }}",
        "{{ [1, 2, 3, 4, 5] | slice(3) | list }}",
        "{{ [1, 2, 3, 4] | batch(3, 'x') | list }}",
        "{{ 'aaa' | replace('a', 'b', 0) }}",
        "{{ 123 | replace('2', 'x') }}",
        "{{ none | replace('o', '0') }}",
        "{{ '  a  ' | trim(' a') }}",
        "{{ 'HELLO' | capitalize }}",
        "{{ \"it's-a_b c\" | title }}",
        "{{ 'ÉCOLE ß' | lower }} {{ 'straße' | upper }}",
        "{{ 3 | abs }} {{ -1.5 | abs }} {{ -0 | abs }}",
        "{{ 5 | string ~ 'x' }}",
        "{{ {'a': none} | string }}",
        "{{ [1, 2] | map('abs') | list }}",
        "{{ ['a', 'B'] | map('upper') | join }}",
        "{{ [1, 2, 3] | select('>', 1) | list }}",
        "{{ [1, 2, 3] | select('in', [1, 3]) | list }}",
        "{{ [1, 2, 3] | select('divisibleby', 3) | list }}",
        "{{ [1, none, 0, 'a'] | select | list }}",
        "{{ [1, none, 0, 'a'] | reject | list }}",
        "{{ {'b': 1, 'a': 2} | dictsort(reverse=true) }}",
        "{{ {'b': 1, 'a': 2} | dictsort(false, 'value', true) }}",
        "{{ [1, 2, 3] | reverse }}",
        "{{ 'abc' | reverse }}",
        "{{ range(3) | reverse | list }}",
        "{{ [3, 1] | first }}{{ [3, 1] | last }}",
        "{{ 'abc' | first }}{{ 'abc' | last }}",
        "{{ {'a': 1} | first }}",
        "{% set e = [] %}{{ e | max }}x{{ e | min }}",
        "{% set e = [] %}{{ e | sum }}",
        "{% set e = [] %}{{ e | first is defined }}",
        "{{ x | default }}",
        "{{ x | d }}",
        "{{ none | default('d') }}",
        "{{ x is string }}",
        "{{ 1 is in [1] }}",
        "{% autoescape true %}{{ '<x>' }}{{ '<y>' | safe }}{{ none }}{{ ['<'] }}{% endautoescape %}",
        "{% autoescape true %}{{ x }}{{ '&' | e }}{% endautoescape %}{{ '<z>' }}",
        "{% autoescape false %}{{ '<x>' }}{% endautoescape %}",
        "{{ x | string }}",
        "{{ x | e }}",
        "{{ x | escape }}",
        "{{ x | string | length }}",
        "{{ config.missing | string }}",
    ]
)
KEPT_VARIABLES = {
    # Keys in sorted order: hars's JSON objects are sorted, HA's keep
    # their order, and that difference is documented, not tested.
    "users": [{"g": 1, "n": "b"}, {"g": 1, "n": "a"}, {"g": 2, "n": "c"}],
    "config": {"type": "markdown"},
}


def check_versions():
    for pkg, want in PINNED.items():
        have = importlib.metadata.version(pkg)
        if have != want:
            sys.exit(f"{pkg} is {have}; these vectors are made with {want}")


def write(folder, name, data):
    folder.mkdir(parents=True, exist_ok=True)
    (folder / name).write_text(json.dumps(data, indent=1, ensure_ascii=False) + "\n")


def as_sent(value):
    """`value` as HA's WebSocket would send it, or the marker for the
    `Invalid JSON in response` error it sends instead. A string over 1 KiB
    is kept as its length and first characters."""
    from homeassistant.helpers.json import json_bytes

    try:
        sent = json.loads(json_bytes(value))
    except (TypeError, ValueError):
        return {"unserializable": True}
    if isinstance(sent, str) and re.search(r"<[^<>]* at 0x[0-9a-f]+>", sent):
        # A generator's or iterator's repr: its address changes every run.
        return {"repr_with_address": True}
    if isinstance(sent, str) and len(sent) > 1024:
        return {"long_string": {"length": len(sent), "starts": sent[:16]}}
    return {"json": sent}


async def render_vectors():
    from homeassistant.core import HomeAssistant
    from homeassistant.exceptions import TemplateError
    from homeassistant.helpers import template

    out = []
    for source, variables, strict in RENDER:
        hass = HomeAssistant(tempfile.mkdtemp())
        log = []
        tpl = template.Template(source, hass)
        info = tpl.async_render_to_info(
            variables, strict=strict, log_fn=lambda level, msg: log.append([logging.getLevelName(level), msg])
        )
        case = {"template": source, "strict": strict}
        if variables is not None:
            case["variables"] = variables
        try:
            case["result"] = as_sent(info.result())
        except TemplateError as err:
            case["error"] = str(err)
        case["log"] = log
        out.append(case)
        await hass.async_stop(force=True)
    return out


async def kept_vectors():
    """Each `KEPT` template, lenient and strict."""
    from homeassistant.core import HomeAssistant
    from homeassistant.exceptions import TemplateError
    from homeassistant.helpers import template

    hass = HomeAssistant(tempfile.mkdtemp())
    out = []
    for source in KEPT:
        for strict in (False, True):
            log = []
            tpl = template.Template(source, hass)
            info = tpl.async_render_to_info(
                KEPT_VARIABLES,
                strict=strict,
                log_fn=lambda level, msg: log.append([logging.getLevelName(level), msg]),
            )
            case = {"template": source, "strict": strict}
            try:
                case["result"] = as_sent(info.result())
            except TemplateError as err:
                case["error"] = str(err)
            case["log"] = log
            out.append(case)
    await hass.async_stop(force=True)
    return out


def parse_vectors():
    from homeassistant.helpers import template

    tpl = template.Template("{{ x }}", None)
    return [{"rendered": s, "result": as_sent(tpl._parse_result(s))} for s in PARSE]


async def names():
    from jinja2 import defaults
    from homeassistant.core import HomeAssistant
    from homeassistant.helpers.template import TemplateEnvironment

    hass = HomeAssistant(tempfile.mkdtemp())
    env = TemplateEnvironment(hass)
    await hass.async_stop(force=True)
    return {
        "globals": sorted(env.globals),
        "filters": sorted(env.filters),
        "tests": sorted(env.tests),
        # Jinja's own, unchanged by HA: what hars may serve from minijinja.
        "jinja_filters": sorted(k for k in env.filters if env.filters[k] is defaults.DEFAULT_FILTERS.get(k)),
        "jinja_tests": sorted(k for k in env.tests if env.tests[k] is defaults.DEFAULT_TESTS.get(k)),
    }


def rt(n, template, **kw):
    return {"id": n, "type": "render_template", "template": template, **kw}


# Conversations: messages sent on one connection, in order.
CONVERSATIONS = [
    ("welcome", [rt(1, "## Welcome {{ user }}", variables={"config": {"type": "markdown", "text_only": True}, "user": "Ann"}, strict=True, report_errors=False)]),
    ("welcome preview", [rt(1, "## Welcome {{ user }}", variables={"user": "Ann"}, strict=True, report_errors=True)]),
    ("static", [rt(1, "  hello  ")]),
    ("number", [rt(1, "{{ 1 + 1 }}")]),
    ("list", [rt(1, "{{ [1, 'a', none, true, {'b': 2.0}] }}")]),
    ("unserializable result", [rt(1, "{{ 'b' }}'x'")]),
    ("unserializable result, reported", [rt(1, "{{ 'b' }}'x'", report_errors=True)]),
    ("undefined strict", [rt(1, "{{ user }}", strict=True)]),
    ("undefined strict reported", [rt(1, "{{ user }}", strict=True, report_errors=True)]),
    ("undefined lenient", [rt(1, "{{ user }}x")]),
    ("undefined lenient reported", [rt(1, "{{ user }}x", report_errors=True)]),
    ("undefined attribute reported", [rt(1, "{{ x.y }}", report_errors=True)]),
    ("missing key strict reported", [rt(1, "{{ config.missing }}", variables={"config": {}}, strict=True, report_errors=True)]),
    ("syntax error", [rt(1, "{{ 1 + }}")]),
    ("syntax error reported", [rt(1, "{{ 1 + }}", report_errors=True)]),
    ("syntax error with timeout", [rt(1, "{{ 1 + }}", timeout=3)]),
    ("unknown filter reported", [rt(1, "{{ 1 | nofilter }}", report_errors=True)]),
    ("timed out", [rt(1, SLOW, timeout=1e-9)]),
    ("timed out, string timeout", [rt(1, SLOW, timeout="0.000000001", report_errors=True)]),
    ("timeout not reached", [rt(1, "{{ 2 }}", timeout=3)]),
    ("timeout not reached, string", [rt(1, "{{ 2 }}", timeout=" 3 ")]),
    ("timeout zero", [rt(1, SLOW, timeout=0)]),
    ("timeout on static", [rt(1, "static", timeout=1e-9)]),
    ("timeout, undefined reported", [rt(1, "{{ user }}x", timeout=3, report_errors=True)]),
    ("timeout, strict error", [rt(1, "{{ user }}", timeout=3, strict=True, report_errors=True)]),
    ("timeout, undefined attribute reported", [rt(1, "{{ x.y }}", timeout=3, report_errors=True)]),
    ("timeout, syntax error reported", [rt(1, "{{ 1 + }}", timeout=3, report_errors=True)]),
    ("timeout, cached", [rt(1, "{{ user }}x", timeout=3), rt(2, "{{ user }}x", strict=True)]),
    ("unsubscribe", [rt(1, "{{ 1 }}"), {"id": 2, "type": "unsubscribe_events", "subscription": 1}, {"id": 3, "type": "unsubscribe_events", "subscription": 1}]),
    ("unsubscribe after timeout", [rt(1, SLOW, timeout=1e-9), {"id": 2, "type": "unsubscribe_events", "subscription": 1}]),
    ("unsubscribe after error", [rt(1, "{{ 1 + }}"), {"id": 2, "type": "unsubscribe_events", "subscription": 1}]),
    # The cached template keeps the strictness it was first compiled with.
    ("cache strict first", [rt(1, "{{ user }}", strict=True), rt(2, "{{ user }}"), rt(3, "{{ user }}", report_errors=True)]),
    ("cache lenient first", [rt(1, "{{ user }}"), rt(2, "{{ user }}", strict=True)]),
    ("cache key is the raw string", [rt(1, "{{ user }}", strict=True), rt(2, " {{ user }}")]),
    ("entity_ids accepted", [rt(1, "{{ 1 }}", entity_ids="sun.sun, Light.Kitchen"), rt(2, "{{ 2 }}", entity_ids=["sun.sun"])]),
    # Nested past what Python's recursion limit allows Jinja to compile:
    # a RecursionError, which isn't a TemplateError, so HA's catch-all.
    ("deep expression", [rt(1, "{{ " + "1+" * 600 + "1 }}")]),
    ("deep expression reported", [rt(1, "{{ " + "not " * 600 + "true }}", report_errors=True)]),
    ("deep filters, with timeout", [rt(1, "{{ [1]" + "|list" * 600 + " }}", timeout=3)]),
    ("deep but within the limit", [rt(1, "{{ " + "1+" * 300 + "1 }}")]),
    # Schema errors.
    ("no template", [{"id": 1, "type": "render_template"}]),
    ("template not str", [rt(1, 5)]),
    ("variables not dict", [rt(1, "x", variables=[1])]),
    ("strict not bool", [rt(1, "x", strict=1)]),
    ("report_errors not bool", [rt(1, "x", report_errors="yes")]),
    ("timeout not float", [rt(1, "x", timeout="soon")]),
    ("timeout null", [rt(1, "x", timeout=None)]),
    ("timeout bool", [rt(1, "{{ 2 }}", timeout=True)]),
    ("extra key", [rt(1, "x", extra=1)]),
    ("several errors", [{"id": 1, "type": "render_template", "strict": "x", "variables": 1}]),
    ("entity_ids invalid", [rt(1, "x", entity_ids="nope")]),
    ("entity_ids invalid in list", [rt(1, "x", entity_ids=["sun.sun", "Bad"])]),
    ("entity_ids null", [rt(1, "x", entity_ids=None)]),
    ("entity_ids number", [rt(1, "x", entity_ids=5)]),
    ("entity_ids dict", [rt(1, "x", entity_ids={"sun.sun": 1})]),
]


async def conversation(msgs):
    from homeassistant.auth.models import User
    from homeassistant.components.websocket_api import async_register_command
    from homeassistant.components.websocket_api import commands, connection
    from homeassistant.components.websocket_api import const as ws_const
    from homeassistant.components.websocket_api.messages import message_to_json_bytes
    from homeassistant.core import HomeAssistant

    hass = HomeAssistant(tempfile.mkdtemp())
    hass.data[ws_const.DOMAIN] = {}
    async_register_command(hass, commands.handle_render_template)
    async_register_command(hass, commands.handle_unsubscribe_events)
    sent = []

    def send(message):
        # As `WebSocketHandler._send_message` serialises a dict.
        if isinstance(message, dict):
            message = message_to_json_bytes(message)
        sent[-1].append(json.loads(message))

    user = User(name="Ann", is_owner=False, is_active=True, system_generated=False, groups=[], perm_lookup=None)
    conn = connection.ActiveConnection(logging.getLogger("vectors"), hass, send, user, None, None)
    for msg in msgs:
        sent.append([])
        conn.async_handle(msg)
        await hass.async_block_till_done(wait_background_tasks=True)
        # The refresh is scheduled with call_soon_threadsafe.
        for _ in range(5):
            await asyncio.sleep(0)
        await hass.async_block_till_done(wait_background_tasks=True)
    conn.async_handle_close()
    await hass.async_stop(force=True)
    return [{"send": m, "replies": r} for m, r in zip(msgs, sent)]


async def conversations():
    return [{"name": name, "steps": await conversation(msgs)} for name, msgs in CONVERSATIONS]


def main():
    check_versions()
    logging.disable(logging.CRITICAL)
    write(CORE, "render.json", asyncio.run(render_vectors()))
    write(CORE, "parse_result.json", parse_vectors())
    write(CORE, "kept.json", asyncio.run(kept_vectors()))
    write(CORE, "names.json", asyncio.run(names()))
    write(API, "render_template.json", asyncio.run(conversations()))


if __name__ == "__main__":
    main()

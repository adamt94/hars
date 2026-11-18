#!/usr/bin/env python3
"""Vectors for the automation editor's saves and deletes.

    .venv-template/bin/python -I scripts/vectors/automation_editor.py
    cargo test -p ha-api automation::

Boots Home Assistant 2026.6.1's core in a temporary config directory with
`http`, `auth`, `api`, `config` (its `EditAutomationConfigView`) and
`automation`, from a `configuration.yaml` that includes `automations.yaml`,
an admin and a non-admin user, then sends each scenario's requests to
`/api/config/automation/config/<id>` over real HTTP. It writes
crates/ha-api/src/automation/vectors/editor.json: for each scenario the
file at start and, for each request (`sent`: the body's text), the status,
the content type and the body of the reply, `automations.yaml` afterwards (`null`: unchanged), and
what happened once HA had settled, in order: each automation's
`state_changed` (state and attributes), the `entity_registry_updated`
and `automation_reloaded` events. After each scenario: the automations'
registry rows and states.

The id HA gives an entry that has none (`uuid4().hex`) is written
`<hex32>`. Nothing reaches the network. Run it twice: the output must not
change.
"""

import asyncio
import importlib.metadata
import json
import os
import pathlib
import re
import shutil
import socket
import sys
import tempfile

PINNED = {"homeassistant": "2026.6.1", "aiohttp": "3.13.5", "annotatedyaml": "1.0.2"}
OUT = pathlib.Path(__file__).resolve().parents[2] / "crates/ha-api/src/automation/vectors"
CLIENT = "http://127.0.0.1:8123/"
BASE = "/api/config/automation/config/"

ORIGINAL = """\
- id: kitchen_off
  alias: Kitchen lights - off at 11:30pm
  description: Turn off the kitchen light and lamp every night at 11:30pm.
  mode: single
  triggers:
  - trigger: time
    at: "23:30:00"
  actions:
  - action: light.turn_off
    target:
      entity_id:
      - light.kitchen
      - light.lamp
    data:
      transition: 10
- id: hall_motion
  alias: Hallway lights - motion on when dark
  description: Turn on hallway lights when motion is detected below 30 lux, using dim overnight and gentle morning levels.
  mode: restart
  triggers:
  - trigger: state
    entity_id: binary_sensor.hall_motion
    to: "on"
  conditions:
  - condition: numeric_state
    entity_id: sensor.hall_illuminance
    below: 30
  - condition: time
    after: "06:30:00"
    before: "23:00:00"
  actions:
  - action: light.turn_on
    target:
      entity_id: light.hall
    data:
      brightness_pct: 60
"""

KITCHEN = {
    "id": "kitchen_off",
    "alias": "Kitchen lights - off at 11:30pm",
    "description": "Turn off the kitchen light and lamp every night at 11:30pm.",
    "mode": "single",
    "triggers": [{"trigger": "time", "at": "23:30:00"}],
    "actions": [{"action": "light.turn_off",
                 "target": {"entity_id": ["light.kitchen", "light.lamp"]},
                 "data": {"transition": 10}}],
}
# A new automation, as the frontend's editor posts it: no `id`, and the
# keys in this order.
NEW = {
    "description": "",
    "mode": "single",
    "triggers": [{"trigger": "state", "entity_id": ["binary_sensor.hall_motion"], "to": "on"}],
    "conditions": [],
    "actions": [{"action": "light.turn_on", "metadata": {}, "data": {"brightness_pct": 50},
                 "target": {"entity_id": "light.hall"}}],
    "alias": "Hallway test: on at 50%",
}


def get(id_, user="admin"):
    return {"method": "GET", "id": id_, "user": user}


def post(id_, body, user="admin"):
    """`body`: a value sent as JSON, or a str sent as it is."""
    return {"method": "POST", "id": id_, "user": user, "body": body}


def delete(id_, user="admin"):
    return {"method": "DELETE", "id": id_, "user": user}


def service(name, **data):
    return {"service": name, "data": data}


def edited(**changes):
    return {**KITCHEN, **changes}


# (name, doc, automations.yaml at start (None: no file), requests)
SCENARIOS = [
    (
        "resave_unchanged",
        "What the entry's GET answered, posted back: the whole file is written again in "
        "HA's style and the posted entry's keys in HA's order; nothing is reloaded but "
        "the event fires.",
        ORIGINAL,
        [get("kitchen_off"), post("kitchen_off", KITCHEN), get("kitchen_off")],
    ),
    (
        "edit",
        "A changed alias, description, time and data: the entity is made again under its "
        "entity id.",
        ORIGINAL,
        [post("kitchen_off", edited(
            alias="Kitchen lights - off at 11:45pm",
            description="Turn off the kitchen light and lamp every night.\nSecond line: "
                        "moved from 23:30 to 23:45 because the evening routine runs later "
                        "on weekends and holidays.",
            triggers=[{"trigger": "time", "at": "23:45:00"}])),
         get("kitchen_off")],
    ),
    (
        "edit_keeps_off",
        "An automation turned off, then edited, stays off.",
        ORIGINAL,
        [service("turn_off", entity_id="automation.kitchen_lights_off_at_11_30pm"),
         post("kitchen_off", edited(mode="restart"))],
    ),
    (
        "add_new",
        "A new automation as the frontend posts it: appended, with the URL's id first; its "
        "entity id comes from its alias.",
        ORIGINAL,
        [post("1794680000000", NEW), get("1794680000000")],
    ),
    (
        "add_then_delete",
        "Added, then deleted: its entity and registry row go; no reload event.",
        ORIGINAL,
        [post("1794680000000", NEW), delete("1794680000000"), get("1794680000000")],
    ),
    (
        "delete",
        "Deleting an automation, then again (now unknown: a 400, not a 404).",
        ORIGINAL,
        [delete("hall_motion"), delete("hall_motion"), delete("never_was")],
    ),
    (
        "delete_then_add_again",
        "Deleted while off, then added again under the same id.",
        ORIGINAL,
        [service("turn_off", entity_id="automation.kitchen_lights_off_at_11_30pm"),
         delete("kitchen_off"), post("kitchen_off", KITCHEN)],
    ),
    (
        "body_id_differs",
        "A body whose `id` is not the URL's.",
        ORIGINAL,
        [post("kitchen_off", edited(id="other_id", alias="Other")), get("kitchen_off"),
         get("other_id")],
    ),
    (
        "legacy_keys",
        "`trigger`, `condition`, `action`, `platform` and `service`, as older files have "
        "them.",
        ORIGINAL,
        [post("legacy", {
            "alias": "Legacy",
            "trigger": {"platform": "state", "entity_id": "binary_sensor.hall_motion",
                        "to": "on"},
            "condition": {"condition": "state", "entity_id": "light.hall", "state": "off"},
            "action": [{"service": "light.turn_on", "target": {"entity_id": "light.hall"}},
                       {"choose": [{"conditions": [{"condition": "state",
                                                    "entity_id": "light.hall",
                                                    "state": "on"}],
                                    "sequence": [{"service": "light.turn_off",
                                                  "target": {"entity_id": "light.hall"}}]}]}],
        }), get("legacy")],
    ),
    (
        "entry_without_id",
        "An entry in the file with no id gets one when the file is next written.",
        "- alias: No id\n  triggers:\n  - trigger: time\n    at: '10:00:00'\n  actions: []\n"
        + ORIGINAL,
        [post("kitchen_off", edited(mode="restart"))],
    ),
    (
        "no_file",
        "No `automations.yaml` yet: reading finds nothing, a save makes the file.",
        None,
        [get("first"), delete("first"), post("first", NEW), get("first")],
    ),
    (
        "empty_file",
        "An empty file, and one holding `[]`.",
        "",
        [get("first"), post("first", NEW), delete("first"), post("second", NEW)],
    ),
    (
        "numeric_id_in_file",
        "An entry whose `id` YAML reads as a number.",
        "- id: 123\n  alias: Numbered\n  triggers:\n  - trigger: time\n    at: '10:00:00'\n"
        "  actions: []\n",
        [get("123"), post("123", {"alias": "Numbered again",
                                  "triggers": [{"trigger": "time", "at": "11:00:00"}],
                                  "actions": []}),
         delete("123")],
    ),
    (
        "unicode",
        "Text that HA's writer quotes or folds: an emoji, a colon, a multi-line "
        "description, a template.",
        ORIGINAL,
        [post("fancy", {
            "alias": "🚨 Alert: door",
            "description": "Line one\nLine two is long enough to need folding when it is "
                           "written out by the emitter, at eighty columns.\n",
            "triggers": [{"trigger": "state", "entity_id": "binary_sensor.door", "to": "on"}],
            "actions": [{"action": "light.turn_on", "target": {"entity_id": "light.hall"},
                         "data": {"brightness_pct": "{{ 50 if is_state('sun.sun', "
                                                    "'below_horizon') else 100 }}"}}],
            "mode": "single",
        }), get("fancy")],
    ),
    (
        "invalid_bodies",
        "Bodies HA refuses: nothing is written.",
        ORIGINAL,
        [post("bad", "{not json"), post("bad", ""), post("bad", [KITCHEN]), post("bad", "5"),
         post("bad", '"text"'), post("bad", "null"), post("bad", "true"), post("bad", "1.5"),
         # Not `{}`: which of the two missing keys HA names changes from
         # run to run (voluptuous walks a set).
         post("bad", "[]"),
         post("bad", {"alias": "Both", "trigger": [{"trigger": "time", "at": "10:00:00"}],
                      "triggers": [{"trigger": "time", "at": "10:00:00"}], "actions": []}),
         post("bad", {"alias": "No triggers", "actions": []}),
         post("bad", {"alias": "No actions",
                      "triggers": [{"trigger": "time", "at": "10:00:00"}]}),
         post("bad", {"alias": "Bad trigger", "triggers": [{"trigger": "state", "to": "on"}],
                      "actions": []}),
         post("bad", {"alias": "Unknown trigger", "triggers": [{"trigger": "nope"}],
                      "actions": []}),
         post("bad", {"alias": "Bad action",
                      "triggers": [{"trigger": "time", "at": "10:00:00"}],
                      "actions": [{"action": "nodot"}]}),
         post("bad", {"alias": "Extra key", "extra": 1,
                      "triggers": [{"trigger": "time", "at": "10:00:00"}], "actions": []}),
         post("bad", {"alias": "Bad mode", "mode": "sideways",
                      "triggers": [{"trigger": "time", "at": "10:00:00"}], "actions": []}),
         get("bad")],
    ),
    (
        "who_may",
        "Every method needs an admin.",
        ORIGINAL,
        [get("kitchen_off", user="user"), post("kitchen_off", KITCHEN, user="user"),
         delete("kitchen_off", user="user"), get("kitchen_off", user=None),
         post("kitchen_off", KITCHEN, user=None), delete("kitchen_off", user=None)],
    ),
    (
        "file_not_a_list",
        "A file that isn't a list of mappings.",
        "id: x\nalias: X\n",
        [get("x"), post("x", NEW), delete("x")],
    ),
    (
        "file_does_not_parse",
        "A file that doesn't parse.",
        "- id: x\n  alias: [unclosed\n",
        [get("x"), post("x", NEW), delete("x")],
    ),
]


def check_versions():
    for pkg, want in PINNED.items():
        have = importlib.metadata.version(pkg)
        if have != want:
            sys.exit(f"{pkg} is {have}; these vectors are made with {want}")


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def plain(value):
    if isinstance(value, dict):
        return {str(k): plain(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [plain(v) for v in value]
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    if hasattr(value, "isoformat"):
        return "<time>"
    return f"<{type(value).__name__}>"


def state_json(state):
    if state is None:
        return None
    attrs = plain(dict(state.attributes))
    if attrs.get("last_triggered"):
        attrs["last_triggered"] = "<time>"
    return {"state": state.state, "attributes": attrs}


HEX32 = re.compile(r"\b[0-9a-f]{32}\b")


async def run(start, requests):
    from homeassistant import bootstrap, config as conf_util, core, loader
    from homeassistant.auth import auth_manager_from_config
    from homeassistant.config_entries import ConfigEntries
    from homeassistant.helpers import entity_registry as er
    from homeassistant.setup import async_setup_component
    import aiohttp

    tmp = pathlib.Path(tempfile.mkdtemp())
    file = tmp / "automations.yaml"
    try:
        (tmp / "configuration.yaml").write_text("automation: !include automations.yaml\n")
        if start is not None:
            file.write_text(start)
        hass = core.HomeAssistant(str(tmp))
        hass.config.skip_pip = True
        await hass.config.async_load()
        loader.async_setup(hass)
        hass.config_entries = ConfigEntries(hass, {})
        await hass.config_entries.async_initialize()
        await bootstrap.async_load_base_functionality(hass)
        hass.auth = await auth_manager_from_config(hass, [{"type": "homeassistant"}], [])
        port = free_port()
        events = []

        @core.callback
        def state_changed(event):
            entity_id = event.data["entity_id"]
            if entity_id.startswith("automation."):
                events.append({"kind": "state", "entity_id": entity_id,
                               "old": state_json(event.data["old_state"]),
                               "new": state_json(event.data["new_state"])})

        @core.callback
        def registry_updated(event):
            events.append({"kind": "registry", "data": plain(dict(event.data))})

        @core.callback
        def reloaded(event):
            events.append({"kind": "reloaded", "data": plain(dict(event.data)),
                           "user": event.context.user_id is not None})

        hass.bus.async_listen("state_changed", state_changed)
        hass.bus.async_listen("entity_registry_updated", registry_updated)
        hass.bus.async_listen("automation_reloaded", reloaded)
        try:
            conf = await conf_util.async_hass_config_yaml(hass)
        except Exception:  # noqa: BLE001
            # A file that doesn't parse: HA starts in recovery mode; here,
            # with no automations.
            conf = {}
        conf["http"] = {"server_port": port, "server_host": "127.0.0.1"}
        assert await async_setup_component(hass, "homeassistant", conf)
        assert await async_setup_component(hass, "http", conf)
        for domain in ("auth", "api", "config"):
            assert await async_setup_component(hass, domain, conf), domain
        await async_setup_component(hass, "automation", conf)
        await hass.async_start()
        await hass.async_block_till_done()
        tokens = {}
        for name, group in (("admin", "system-admin"), ("user", "system-users")):
            user = await hass.auth.async_create_user(name, group_ids=[group])
            refresh = await hass.auth.async_create_refresh_token(user, CLIENT)
            tokens[name] = hass.auth.async_create_access_token(refresh)
        start_events = list(events)
        events.clear()
        steps = []
        async with aiohttp.ClientSession() as session:
            for request in requests:
                before = file.read_text() if file.exists() else None
                if "service" in request:
                    await hass.services.async_call("automation", request["service"],
                                                   request["data"], blocking=True)
                    reply = {}
                else:
                    headers = {}
                    if request["user"] is not None:
                        headers["Authorization"] = f"Bearer {tokens[request['user']]}"
                    body = request.get("body")
                    data = None
                    if body is not None:
                        data = body if isinstance(body, str) else json.dumps(body)
                        # The text sent: its key order is what HA writes.
                        request = {**request, "sent": data}
                        del request["body"]
                    url = f"http://127.0.0.1:{port}{BASE}{request['id']}"
                    async with session.request(request["method"], url, data=data,
                                               headers=headers,
                                               skip_auto_headers=["Content-Type"]) as r:
                        reply = {"status": r.status,
                                 "content_type": r.headers.get("Content-Type"),
                                 "body": await r.text()}
                for _ in range(3):
                    await hass.async_block_till_done()
                after = file.read_text() if file.exists() else None
                step = {"request": request, **reply}
                if after != before:
                    step["file"] = HEX32.sub("<hex32>", after)
                step["events"] = list(events)
                events.clear()
                steps.append(step)
        rows = [
            {"entity_id": e.entity_id, "unique_id": e.unique_id, "name": e.name,
             "original_name": e.original_name, "object_id_base": e.object_id_base,
             "capabilities": plain(e.capabilities), "options": plain(dict(e.options))}
            for e in er.async_get(hass).entities.values()
            if e.platform == "automation"
        ]
        states = {s.entity_id: state_json(s) for s in hass.states.async_all("automation")}
        await hass.async_stop(force=True)
    finally:
        shutil.rmtree(tmp)
    return start_events, steps, rows, states


async def main():
    check_versions()
    scenarios = []
    for name, doc, start, requests in SCENARIOS:
        requests = json.loads(json.dumps(requests))
        start_events, steps, rows, states = await run(start, requests)
        scenarios.append({"name": name, "doc": doc, "file": start, "start": start_events,
                          "steps": steps, "registry": rows, "final": states})
    OUT.mkdir(parents=True, exist_ok=True)
    text = json.dumps({"scenarios": scenarios}, indent=1, ensure_ascii=False) + "\n"
    (OUT / "editor.json").write_text(text)


if __name__ == "__main__":
    asyncio.run(main())
    # CPython 3.14.7 can crash collecting HA's modules at shutdown, after
    # everything is written: skip the teardown.
    sys.stdout.flush()
    os._exit(0)

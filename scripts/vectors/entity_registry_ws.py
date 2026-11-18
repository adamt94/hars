#!/usr/bin/env python3
"""Vectors for the entity registry's WebSocket commands the Settings pages use.

    .venv-template/bin/python -I scripts/vectors/entity_registry_ws.py
    cargo test -p ha-api entity_registry

Boots Home Assistant 2026.6.1's core in a temporary config directory with
`http`, `auth`, `websocket_api` and `config` (its `entity_registry` and
`category_registry` commands), an admin and a non-admin user, an entity
registry holding rows of each kind (a user's name, icon, area, labels and
categories; disabled, hidden; with options and capabilities) and a
category registry, then sends each message below over a real
WebSocket. It writes crates/ha-api/src/ws/vectors/entity_registry.json:

- `stores`: `core.entity_registry`, `core.device_registry` and
  `core.category_registry` as HA had
  them before the first message (what hars loads), their times and the
  categories' ids replaced by fixed ones;
- `steps`: each message (`user`: who sent it), its reply, the
  `entity_registry_updated` events it caused, and the rows it changed, as
  stored afterwards.

In replies and rows, times are written `<time>` (ISO text) and `<ts>`
(seconds), a category's id `<ulid>`, and labels sorted (a set in HA,
listed in an order that changes from run to run); what an update moves is
listed in `moved`. Nothing reaches the network. Run it twice: the
output must not change.
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
from types import MappingProxyType

PINNED = {"homeassistant": "2026.6.1", "aiohttp": "3.13.5"}
OUT = pathlib.Path(__file__).resolve().parents[2] / "crates/ha-api/src/ws/vectors"
CLIENT = "http://127.0.0.1:8123/"
AUTO = "automation.hall_motion"
LIGHT = "light.hall"

# (who, message[, note])
STEPS = [
    ("admin", {"type": "config/entity_registry/list"}),
    ("user", {"type": "config/entity_registry/list"}),
    ("admin", {"type": "config/entity_registry/get", "entity_id": AUTO}),
    ("admin", {"type": "config/entity_registry/get", "entity_id": LIGHT}),
    ("user", {"type": "config/entity_registry/get", "entity_id": LIGHT}),
    ("admin", {"type": "config/entity_registry/get", "entity_id": "light.nope"}),
    ("admin", {"type": "config/entity_registry/get", "entity_id": "not an id"}),
    ("admin", {"type": "config/entity_registry/get"}),
    ("admin", {"type": "config/entity_registry/get", "entity_id": "sensor.kitchen_light_power"}),
    ("admin", {"type": "config/entity_registry/get", "entity_id": "LIGHT.Hall"}),
    ("admin", {"type": "config/entity_registry/get_entries",
               "entity_ids": [LIGHT, "light.nope", AUTO]}),
    ("admin", {"type": "config/entity_registry/get_entries",
               "entity_ids": "light.hall, light.nope,AUTOMATION.hall_motion"}),
    ("admin", {"type": "config/entity_registry/get_entries", "entity_ids": []}),
    ("admin", {"type": "config/entity_registry/get_entries", "entity_ids": ["not an id"]}),
    ("admin", {"type": "config/entity_registry/get_entries", "entity_ids": 5}),
    ("admin", {"type": "config/entity_registry/get_entries"}),
    ("user", {"type": "config/entity_registry/get_entries", "entity_ids": [LIGHT]}),
    ("admin", {"type": "config/entity_registry/update", "entity_id": "AUTOMATION.Hall_Motion",
               "labels": []}),
    # What the automation editor sends after saving a new automation:
    # nothing changes, so no event.
    ("admin", {"type": "config/entity_registry/update", "entity_id": AUTO,
               "categories": {"automation": None}, "labels": [], "area_id": None}),
    # The automation list's menus: a category, labels, an area.
    ("admin", {"type": "config/entity_registry/update", "entity_id": AUTO,
               "categories": {"automation": "cat_lights"}}),
    ("admin", {"type": "config/entity_registry/update", "entity_id": AUTO,
               "labels": ["night", "hall"]}),
    ("admin", {"type": "config/entity_registry/update", "entity_id": AUTO,
               "area_id": "hallway"}),
    ("admin", {"type": "config/entity_registry/update", "entity_id": AUTO,
               "categories": {"automation": "cat_other", "helpers": "cat_h"},
               "labels": ["hall"], "area_id": "hallway"}),
    ("admin", {"type": "config/entity_registry/update", "entity_id": AUTO,
               "categories": {"automation": None, "never_set": None}}),
    ("admin", {"type": "config/entity_registry/update", "entity_id": AUTO,
               "labels": [], "area_id": None}),
    ("admin", {"type": "config/entity_registry/update", "entity_id": AUTO}),
    ("admin", {"type": "config/entity_registry/update", "entity_id": LIGHT,
               "area_id": "an area nobody made", "labels": ["x", "x", "a"]}),
    ("admin", {"type": "config/entity_registry/get", "entity_id": AUTO}),
    # Refused.
    ("user", {"type": "config/entity_registry/update", "entity_id": AUTO, "labels": ["a"]}),
    ("admin", {"type": "config/entity_registry/update", "entity_id": "light.nope",
               "labels": ["a"]}),
    ("admin", {"type": "config/entity_registry/update", "labels": ["a"]}),
    ("admin", {"type": "config/entity_registry/update", "entity_id": AUTO, "labels": "a"}),
    ("admin", {"type": "config/entity_registry/update", "entity_id": AUTO, "labels": [1]}),
    ("admin", {"type": "config/entity_registry/update", "entity_id": AUTO, "area_id": 5}),
    ("admin", {"type": "config/entity_registry/update", "entity_id": AUTO,
               "categories": ["automation"]}),
    ("admin", {"type": "config/entity_registry/update", "entity_id": AUTO,
               "categories": {"Not A Slug": "x"}}),
    ("admin", {"type": "config/entity_registry/update", "entity_id": AUTO,
               "categories": {"automation": 5}}),
    ("admin", {"type": "config/entity_registry/update", "entity_id": AUTO, "nope": 1}),
    # Categories.
    ("admin", {"type": "config/category_registry/list", "scope": "automation"}),
    ("admin", {"type": "config/category_registry/list", "scope": "script"}),
    ("user", {"type": "config/category_registry/list", "scope": "automation"}),
    ("admin", {"type": "config/category_registry/list"}),
    ("admin", {"type": "config/category_registry/list", "scope": 5}),
    ("admin", {"type": "config/category_registry/list", "scope": "automation", "extra": 1}),
    # Blueprints (the `blueprint` component isn't set up here: its command
    # is unknown, so only who may ask is shown by hars's own test).
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


TIME = re.compile(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?\+00:00$")
ULID = re.compile(r"^[0-9A-HJKMNP-TV-Z]{26}$")
STAMPS = ("created_at", "modified_at")
# What the stores hars loads hold where HA's are random or the time.
THEN = "2026-11-10T16:00:00+00:00"
DEVICE_ID = "0" * 31 + "d"
CATEGORY_IDS = ["01M7QYB0J64RQX9K1AEP1VY6G7", "01M7QYB0J63K8M369AGWZZ93X6",
                "01M7QYB0J61GDSZ6Y4TMTMR9DH"]


def scrub(value, key=None):
    if isinstance(value, dict):
        return {k: scrub(v, k) for k, v in value.items()}
    if isinstance(value, list):
        # Labels are a set in HA: written in an order that changes from
        # run to run.
        return sorted(value) if key == "labels" else [scrub(v) for v in value]
    if isinstance(value, str) and TIME.match(value):
        return "<time>"
    if isinstance(value, str) and ULID.match(value):
        return "<ulid>"
    if key in STAMPS and isinstance(value, float):
        return "<ts>"
    return value


def fixed(value, ids, key=None):
    """A store as hars is given it: times and category ids fixed."""
    if isinstance(value, dict):
        return {k: fixed(v, ids, k) for k, v in value.items()}
    if isinstance(value, list):
        return sorted(value) if key == "labels" else [fixed(v, ids) for v in value]
    if key in STAMPS:
        return THEN
    if key == "category_id":
        return next(ids)
    return value


async def main():
    check_versions()
    from homeassistant import bootstrap, core, loader
    from homeassistant.auth import auth_manager_from_config
    from homeassistant.config_entries import ConfigEntries, ConfigEntry
    from homeassistant.helpers import category_registry as cr
    from homeassistant.helpers import device_registry as dr
    from homeassistant.helpers import entity_registry as er
    from homeassistant.helpers.json import json_dumps
    from homeassistant.setup import async_setup_component
    import aiohttp

    tmp = pathlib.Path(tempfile.mkdtemp())
    try:
        hass = core.HomeAssistant(str(tmp))
        hass.config.skip_pip = True
        await hass.config.async_load()
        loader.async_setup(hass)
        hass.config_entries = ConfigEntries(hass, {})
        await hass.config_entries.async_initialize()
        await bootstrap.async_load_base_functionality(hass)
        hass.auth = await auth_manager_from_config(hass, [{"type": "homeassistant"}], [])
        port = free_port()
        conf = {"http": {"server_port": port, "server_host": "127.0.0.1"}}
        for domain in ("http", "auth", "websocket_api", "config"):
            assert await async_setup_component(hass, domain, conf), domain
        await hass.async_start()
        await hass.async_block_till_done()

        entities = er.async_get(hass)
        categories = cr.async_get(hass)
        # Rows of each kind. Ids are fixed so the output never changes.
        ids = iter(f"{n:032x}" for n in range(1, 100))

        def row(domain, platform, unique_id, **kwargs):
            return entities.async_get_or_create(domain, platform, unique_id, **kwargs)

        row("automation", "automation", "hall", suggested_object_id="hall_motion",
            original_name="Hall motion", capabilities={"id": "hall"})
        row("automation", "automation", "porch", suggested_object_id="porch_door",
            original_name="Porch door", capabilities={"id": "porch"})
        row("light", "hue", "aa:bb-1", suggested_object_id="hall", original_name="Hall",
            capabilities={"supported_color_modes": ["brightness"]}, supported_features=44,
            original_icon="mdi:lightbulb", has_entity_name=True, translation_key="hall")
        entities.async_update_entity("light.hall", name="Hallway light", icon="mdi:lamp",
                                     area_id="hallway", labels={"night"},
                                     categories={"helpers": "cat_h"}, aliases=["hall lamp"])
        entities.async_update_entity_options("light.hall", "conversation",
                                             {"should_expose": True})
        row("sensor", "hue", "aa:bb-2", suggested_object_id="hall_temperature",
            original_name="Hall temperature", original_device_class="temperature",
            unit_of_measurement="°C", entity_category=er.EntityCategory.DIAGNOSTIC,
            disabled_by=er.RegistryEntryDisabler.INTEGRATION)
        row("switch", "demo", "sw-1", suggested_object_id="hidden_one",
            original_name="Hidden one", hidden_by=er.RegistryEntryHider.USER)
        # An entity named the old way, with its device's name in front:
        # HA answers with the name without it.
        entry = ConfigEntry(
            data={}, discovery_keys=MappingProxyType({}), domain="hue", minor_version=1,
            options={}, source="user", subentries_data=None, title="Hue", unique_id=None,
            version=1, entry_id="0" * 31 + "e",
        )
        hass.config_entries._entries[entry.entry_id] = entry
        device = dr.async_get(hass).async_get_or_create(
            config_entry_id=entry.entry_id, identifiers={("hue", "dev-1")},
            name="Kitchen Light")
        device_id = device.id
        row("sensor", "hue", "aa:bb-3", suggested_object_id="kitchen_light_power",
            original_name="Kitchen Light Power", device_id=device.id, config_entry=entry)
        # Replace the random ids by fixed ones, in the order made.
        import attr

        for entity_id in list(entities.entities):
            entry = entities.entities[entity_id]
            entities.entities[entity_id] = attr.evolve(entry, id=next(ids))
        for name, scope in (("Lights", "automation"), ("Other", "automation"),
                            ("Scripts", "script")):
            categories.async_create(name=name, scope=scope)
        stores = {
            "core.entity_registry": fixed(
                json.loads(json_dumps(entities._data_to_save())), iter(())),
            "core.category_registry": fixed(
                json.loads(json_dumps(categories._data_to_save())), iter(CATEGORY_IDS)),
            "core.device_registry": fixed(
                json.loads(json_dumps(dr.async_get(hass)._data_to_save())), iter(())),
        }

        events = []

        def plain(value):
            """An event's data as JSON; a set (the labels before) sorted."""
            if isinstance(value, dict):
                return {k: plain(v) for k, v in value.items()}
            if isinstance(value, (set, frozenset)):
                return sorted(value)
            if isinstance(value, (list, tuple)):
                return [plain(v) for v in value]
            return value

        @core.callback
        def registry_updated(event):
            events.append(plain(dict(event.data)))

        hass.bus.async_listen("entity_registry_updated", registry_updated)

        def rows():
            data = json.loads(json_dumps(entities._data_to_save()))
            return {e["entity_id"]: e for e in data["entities"]}

        tokens = {}
        for name, group in (("admin", "system-admin"), ("user", "system-users")):
            user = await hass.auth.async_create_user(name, group_ids=[group])
            refresh = await hass.auth.async_create_refresh_token(user, CLIENT)
            tokens[name] = hass.auth.async_create_access_token(refresh)
        steps = []
        async with aiohttp.ClientSession() as session:
            sockets = {}
            for name, token in tokens.items():
                ws = await session.ws_connect(f"http://127.0.0.1:{port}/api/websocket")
                await ws.receive_json()
                await ws.send_json({"type": "auth", "access_token": token})
                assert (await ws.receive_json())["type"] == "auth_ok"
                sockets[name] = ws
            ws_id = dict.fromkeys(sockets, 0)
            for who, msg in STEPS:
                before = rows()
                ws_id[who] += 1
                sent = {"id": ws_id[who], **msg}
                await sockets[who].send_json(sent)
                reply = await asyncio.wait_for(sockets[who].receive_json(), 10)
                await hass.async_block_till_done()
                after = rows()
                step = {"user": who, "ws": sent, "reply": scrub(reply),
                        "events": scrub(list(events))}
                events.clear()
                changed = {k: v for k, v in after.items() if v != before.get(k)}
                if changed:
                    step["rows"] = scrub(changed)
                    step["moved"] = {
                        k: [s for s in STAMPS if v[s] != before[k][s]]
                        for k, v in changed.items()
                    }
                steps.append(step)
            for ws in sockets.values():
                await ws.close()
        await hass.async_stop(force=True)
    finally:
        shutil.rmtree(tmp)
    OUT.mkdir(parents=True, exist_ok=True)
    data = {"stores": stores, "steps": steps}
    text = json.dumps(data, indent=1, ensure_ascii=False) + "\n"
    # The device's id is random: a fixed one everywhere.
    text = text.replace(device_id, DEVICE_ID)
    (OUT / "entity_registry.json").write_text(text)


if __name__ == "__main__":
    asyncio.run(main())
    # CPython 3.14.7 can crash collecting HA's modules at shutdown, after
    # everything is written: skip the teardown.
    sys.stdout.flush()
    os._exit(0)

#!/usr/bin/env python3
"""Vectors for Hue's registry rows on an install that already has them.

    .venv/bin/python -I scripts/vectors/hue_rows.py
    cargo test -p ha-api hue && cargo test -p ha-integrations hue

Home Assistant 2026.6.1 with `hue` and aiohue 4.8.1 is started several
times on one config directory, as an install is over its life, against
`hue_flow.py`'s bridge on 127.0.0.1 (nothing reaches a bridge or
discovery.meethue.com). The bridge's resources are the recorded ones in
crates/ha-integrations/src/hue/vectors/resource.json, plus copies of the
bedroom (its room, lamp, sensor, a scene and the bridge's automation)
under made-up ids, names and addresses:

- "Old ...": there at first, gone from the second start on. HA removes
  their devices, so every kind of entity leaves a removed row; the scene
  and the automation that had no device of their own keep their rows.
- "New ...": there from the second start on, when the entry says new
  entities start disabled: rows of every kind that never ran.

Between the first and the second start the user's changes are made as the
frontend makes them: entities disabled, hidden, renamed, given another id,
an area, an icon, another unit and precision; a device renamed, moved and
one disabled; one entity that starts disabled enabled.

It writes, in crates/ha-integrations/src/hue/vectors/imported/:

- `resource.json`: what the bridge has from the second start on;
- `core.config_entries`, `core.device_registry`, `core.entity_registry`,
  `core.area_registry`: the files as HA left them after the third start
  (checked: the same as after the second, so HA itself changes nothing
  on a start), and `states.json`: the entities' states then. A start of
  `hars` on these files must leave them byte for byte;
- `us_customary.json`: the temperature sensors' rows and states after a
  start of the same install set to US customary units;
- `restored.json`: rows the user removed whose resources the bridge still
  has come back at the next start, once with the entry's "new entities
  start disabled" on and once with it off. Before that start the stores
  are the files above but for the entry's preference, the removed rows
  (named by their unique ids) and the entity registry's
  `deleted_entities`, which is given; after it, the rows that came back
  (checked: no other row changed).

Ids and times are replaced, in order of appearance, by made-up ones of the
same form, so that the files stay what HA would read. Run it twice: the
output must not change.
"""

import asyncio
import gc
import importlib.metadata
import json
import os
import pathlib
import re
import shutil
import sys
import tempfile
import uuid

sys.path.append(str(pathlib.Path(__file__).resolve().parent))

import config_flow as cf  # noqa: E402
import hue_flow as hf  # noqa: E402

OUT = hf.HUE / "imported"
STORES = ("core.config_entries", "core.device_registry", "core.entity_registry",
          "core.area_registry")
BEDROOM = "cb21f2c5-e722-494b-9d90-6bd3b26dcf10"
BULB_DEVICE = "19f631d2-af86-4307-a089-f5a46a2e4b55"
SENSOR_DEVICE = "df9083a4-75b9-46a1-9575-133f738d893d"
SCENE = "7bbe449e-043c-4fb7-80f9-7fc26388738b"
AUTOMATION = "eb9a4b7a-240f-4143-8d71-08b91593e58d"
NAMESPACE = uuid.UUID("00000000-0000-4000-8000-000000000000")


def recorded():
    return json.loads((hf.HUE / "resource.json").read_text())["data"]


def copies(tag, number):
    """The bedroom again, as `tag` ("Old", "New"): its room and the room's
    services, its two devices and theirs, one of its scenes, a scene of
    the hallway and the bridge's automation."""
    resources = recorded()
    by_id = {r["id"]: r for r in resources}
    hall_scene = next(r["id"] for r in resources if r["id"].startswith("567612c9"))
    wanted = [BEDROOM, BULB_DEVICE, SENSOR_DEVICE, SCENE, hall_scene, AUTOMATION]
    for owner in (BEDROOM, BULB_DEVICE, SENSOR_DEVICE):
        wanted += [s["rid"] for s in by_id[owner]["services"]]
    text = json.dumps([by_id[i] for i in wanted])
    for old in wanted:
        text = text.replace(old, str(uuid.uuid5(NAMESPACE, f"{tag}:{old}")))
    made = json.loads(text)
    names = {"Bedroom": {"room": f"{tag} room", "device": f"{tag} sensor"},
             "Light bulb": {"device": f"{tag} bulb"},
             "Nightlight": {"scene": f"{tag} scene"},
             "Go to sleep": {"behavior_instance": f"{tag} routine"}}
    addresses = 0
    for r in made:
        r.pop("id_v1", None)
        name = r.get("metadata", {}).get("name")
        if name in names and r["type"] in names[name]:
            r["metadata"]["name"] = names[name][r["type"]]
        if r["type"] == "zigbee_connectivity":
            addresses += 1
            r["mac_address"] = f"00:17:88:01:ff:{number:02x}:00:{addresses:02x}"
    return made


def rid(tag, old):
    return str(uuid.uuid5(NAMESPACE, f"{tag}:{old}"))


class Normal:
    """Ids and times as made-up ones of the same form, in order of
    appearance: a ULID, a 32-digit hex id, an ISO time."""

    ULID = re.compile(r"\b[0-9A-HJKMNP-TV-Z]{26}\b")
    HEX = re.compile(r"\b[0-9a-f]{32}\b")
    TIME = re.compile(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{6}\+00:00")

    def __init__(self):
        self.ulids, self.hexes = {}, {}

    def text(self, text):
        text = self.ULID.sub(
            lambda m: self.ulids.setdefault(m[0], f"01J{len(self.ulids) + 1:023d}"), text)
        text = self.HEX.sub(
            lambda m: self.hexes.setdefault(m[0], f"{len(self.hexes) + 1:032x}"), text)
        return self.TIME.sub("2026-06-01T12:00:00.000000+00:00", text)

    def value(self, value):
        return json.loads(self.text(json.dumps(value)))


async def boot(tmp, fake, units=None):
    """Home Assistant started on `tmp`, its stored entries set up."""
    from homeassistant import bootstrap, core, loader
    from homeassistant.auth import auth_manager_from_config
    from homeassistant.config_entries import ConfigEntries
    from homeassistant.setup import async_setup_component

    hass = core.HomeAssistant(str(tmp))
    hass.config.skip_pip = True
    await hass.config.async_load()
    if units == "us_customary":
        from homeassistant.util.unit_system import US_CUSTOMARY_SYSTEM

        hass.config.units = US_CUSTOMARY_SYSTEM
    loader.async_setup(hass)
    hass.config_entries = ConfigEntries(hass, {})
    await hass.config_entries.async_initialize()
    await bootstrap.async_load_base_functionality(hass)
    hass.auth = await auth_manager_from_config(hass, [{"type": "homeassistant"}], [])
    # `network`: HA's HTTP client asks it for the adapters. It needs
    # `http`, as `config_flow.py` sets it up.
    conf = {"http": {"server_port": cf.free_port(), "server_host": "127.0.0.1"}}
    for domain in ("homeassistant", "http", "network"):
        assert await async_setup_component(hass, domain, conf), domain
    for domain in sorted({e.domain for e in hass.config_entries.async_entries()}):
        assert await async_setup_component(hass, domain, {}), domain
    await hass.async_start()
    await settle(hass)
    return hass


async def settle(hass):
    for _ in range(3):
        await hass.async_block_till_done()


async def stop(hass, tmp):
    """Stop, as a shutdown does (the stores are written), and the files."""
    await settle(hass)
    await hass.async_stop(force=True)
    return {name: (tmp / ".storage" / name).read_text() for name in STORES}


def states_of(hass):
    from homeassistant.helpers import entity_registry as er
    from homeassistant.helpers.json import json_dumps

    entities = er.async_get(hass)
    states = {}
    for s in sorted(hass.states.async_all(), key=lambda s: s.entity_id):
        if not entities.async_get(s.entity_id):
            continue
        attributes = dict(s.attributes)
        # Sets in HA: a group's scenes and members.
        for key in ("hue_scenes", "lights", "entity_id"):
            if isinstance(attributes.get(key), (list, set, tuple, frozenset)):
                attributes[key] = sorted(attributes[key])
        states[s.entity_id] = {"state": s.state, "attributes": attributes}
    return json.loads(json_dumps(states))


async def add_bridge(hass, fake):
    """The bridge added through its flow."""
    flows = hass.config_entries.flow
    result = await flows.async_init("hue", context={"source": "user"})
    result = await flows.async_configure(result["flow_id"], {"id": hf.BRIDGE})
    fake.set("pressed")
    result = await flows.async_configure(result["flow_id"], {})
    assert result["type"].value == "create_entry", result
    await settle(hass)
    return result["result"]


def user_changes(hass):
    """What a user changes in Settings, on rows of every kind."""
    from homeassistant.helpers import (area_registry as ar, device_registry as dr,
                                       entity_registry as er)

    entities, devices, areas = er.async_get(hass), dr.async_get(hass), ar.async_get(hass)
    office = areas.async_create("Office").id
    user, hidden = er.RegistryEntryDisabler.USER, er.RegistryEntryHider.USER
    by_unique = {(e.domain, e.unique_id): e.entity_id for e in entities.entities.values()
                 if e.platform == "hue"}

    def unique(domain, start):
        found = [i for (d, u), i in by_unique.items() if d == domain and u.startswith(start)]
        assert len(found) == 1, (domain, start, found)
        return found[0]

    def device(start):
        return next(d for d in devices.devices.values()
                    if any(i[1].startswith(start) for i in d.identifiers))

    change = entities.async_update_entity
    # Lights, and rooms as lights.
    change(unique("light", "8d3138ee"), disabled_by=user)
    change(unique("light", "a5ffb575"), hidden_by=hidden)
    change(unique("light", "81c174c4"), name="Reading light",
           new_entity_id="light.reading", icon="mdi:lamp", area_id=office)
    change(unique("light", "df1e030d"), name="All of the hallway")
    change(unique("light", "5f918504"), disabled_by=user)
    # Scenes.
    change(unique("scene", "f2d28599"), disabled_by=user)
    change(unique("scene", "7bbe449e"), hidden_by=hidden)
    change(unique("scene", "e137be82"), name="Holiday", new_entity_id="scene.holiday")
    change(unique("scene", "567612c9"), area_id=office)
    # Sensors: another unit and precision for the temperature.
    temperature = unique("sensor", "0cfa6b7f")
    entities.async_update_entity_options(
        temperature, "sensor", {"unit_of_measurement": "°F", "display_precision": 2})
    change(unique("sensor", "53720b51"), disabled_by=user)
    change(unique("sensor", "6c54c53d"), hidden_by=hidden)
    change(unique("sensor", "b2ed1e39"), name="Bedroom brightness")
    # One that starts disabled is enabled; another gets a name while off.
    change(unique("sensor", "37abe652"), disabled_by=None)
    change(unique("sensor", "4f8826ef"), name="Bridge radio")
    # Binary sensors and switches.
    change(unique("binary_sensor", "c866df7d"), name="Someone in the bedroom")
    change(unique("binary_sensor", "e76de930"), disabled_by=user)
    change(unique("switch", "6c54c53d"), disabled_by=user)
    change(unique("switch", "c866df7d"), hidden_by=hidden)
    change(unique("switch", "eb9a4b7a"), name="Bedtime", new_entity_id="switch.bedtime")
    # Devices: a name and an area of the user's, and one disabled.
    devices.async_update_device(device("13592c33").id, name_by_user="Desk lamp",
                                area_id=office)
    devices.async_update_device(device("c6c99b57").id, area_id=office)
    devices.async_update_device(device("dac0d5e2").id,
                                disabled_by=dr.DeviceEntryDisabler.USER)
    # And on what will be gone: its removed rows keep them.
    change(unique("light", rid("Old", "c60ee3d8-e55f-4fc9-8d31-9e3dc6aa077d")[:8]),
           name="Old favourite", area_id=office)
    change(unique("sensor", rid("Old", "0cfa6b7f-fdc8-490a-9261-6e721a4afc58")[:8]),
           disabled_by=user)


# Rows the user removes though the bridge still has their resources:
# (domain, start of the unique id, what it was).
REMOVED = [
    ("light", "c60ee3d8", "a light that ran"),
    ("light", "8d3138ee", "a light the user disabled"),
    ("light", "ad2abd7a", "a room's light"),
    ("scene", "7bbe449e", "a scene the user hid"),
    ("sensor", "0cfa6b7f", "the temperature, with the user's unit"),
    ("sensor", "37abe652", "a sensor that starts disabled, which the user enabled"),
    ("sensor", "0a8892fe", "a sensor that starts disabled"),
    ("binary_sensor", "c866df7d", "a renamed motion sensor"),
    ("switch", "eb9a4b7a", "the automation's switch, with another id"),
    ("sensor", rid("New", "0cfa6b7f-fdc8-490a-9261-6e721a4afc58")[:8],
     "a temperature that never ran"),
]


def remove_rows(hass):
    from homeassistant.helpers import entity_registry as er

    entities = er.async_get(hass)
    for domain, start, _ in REMOVED:
        found = [e.entity_id for e in entities.entities.values()
                 if e.platform == "hue" and e.domain == domain
                 and e.unique_id.startswith(start)]
        assert len(found) == 1, (domain, start, found)
        entities.async_remove(found[0])


def in_order(files):
    """The registries' rows in an order of their own. HA sets the entity
    platforms up side by side, so the order it makes rows in changes from
    run to run, and so does the order of a device's identifiers and
    connections (sets); a file is as good to HA in any order. Written
    again as HA writes a store (checked: that gives the file back as it
    was)."""
    import orjson

    def written(doc, keys):
        # A registry's rows are written whole, each on a line of its own,
        # inside a store that is indented.
        rows, outer = [], {**doc, "data": dict(doc["data"])}
        for key in keys:
            outer["data"][key] = [f"<row {len(rows) + i}>"
                                  for i in range(len(doc["data"][key]))]
            rows += doc["data"][key]
        text = orjson.dumps(outer, option=orjson.OPT_INDENT_2).decode()
        for i, row in enumerate(rows):
            text = text.replace(f'"<row {i}>"', orjson.dumps(row).decode())
        return text

    def identifiers(device):
        return json.dumps(sorted(device["identifiers"]))

    out = dict(files)
    for name, lists in (("core.entity_registry",
                         (("entities", lambda e: e["entity_id"]),
                          ("deleted_entities", lambda e: (e["entity_id"], e["unique_id"])))),
                        ("core.device_registry",
                         (("devices", identifiers), ("deleted_devices", identifiers)))):
        doc = json.loads(files[name])
        keys = [key for key, _ in lists]
        assert written(doc, keys) == files[name], name
        for key, order in lists:
            for row in doc["data"][key]:
                # Sets in HA, written in whatever order they come out.
                for field in ("connections", "identifiers", "labels"):
                    if field in row:
                        row[field].sort()
            doc["data"][key].sort(key=order)
        out[name] = written(doc, keys)
    return out


def entry_of(hass):
    return hass.config_entries.async_entries("hue")[0]


def rows(files):
    listed = json.loads(files["core.entity_registry"])["data"]["entities"]
    return sorted(listed, key=lambda e: e["entity_id"])


async def main():
    cf.check_versions()
    found = importlib.metadata.version("aiohue")
    if found != hf.AIOHUE:
        sys.exit(f"aiohue is {found}, the vectors are pinned to {hf.AIOHUE}")
    first = json.dumps({"errors": [], "data": recorded() + copies("Old", 1)})
    later = {"errors": [], "data": recorded() + copies("New", 2)}
    fake = hf.Hue()
    await fake.start()
    tmp = pathlib.Path(tempfile.mkdtemp())
    scratch = []
    try:
        # The first start: the bridge is added, the user changes things.
        fake.resources = first
        hass = await boot(tmp, fake)
        entry = await add_bridge(hass, fake)
        user_changes(hass)
        await settle(hass)
        hass.config_entries.async_update_entry(entry, pref_disable_new_entities=True)
        await stop(hass, tmp)

        # From the second start on the bridge has changed. A start after
        # that changes nothing.
        fake.resources = json.dumps(later)
        second = await stop(await boot(tmp, fake), tmp)
        hass = await boot(tmp, fake)
        states = states_of(hass)
        imported = await stop(hass, tmp)
        for name in STORES:
            if second[name] != imported[name]:
                sys.exit(f"HA changed {name} on a start with nothing new")
        imported = in_order(imported)

        def branch():
            other = pathlib.Path(tempfile.mkdtemp())
            scratch.append(other)
            shutil.copytree(tmp, other, dirs_exist_ok=True)
            return other

        # The same install set to US customary units.
        other = branch()
        hass = await boot(other, fake, units="us_customary")
        us_states = states_of(hass)
        us_rows = rows(await stop(hass, other))

        # Rows removed and coming back, with the preference on and off.
        restored = []
        for disable_new in (True, False):
            other = branch()
            hass = await boot(other, fake)
            # HA reloads the entry when its preference changes: before the
            # rows are removed, or the reload would bring them back.
            hass.config_entries.async_update_entry(
                entry_of(hass), pref_disable_new_entities=disable_new)
            await settle(hass)
            remove_rows(hass)
            before = in_order(await stop(hass, other))
            after = in_order(await stop(await boot(other, fake), other))
            restored.append((disable_new, before, after))
    finally:
        await fake.stop()
        for path in (tmp, *scratch):
            shutil.rmtree(path, ignore_errors=True)

    normal = Normal()
    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / "resource.json").write_text(json.dumps(later, ensure_ascii=False) + "\n")
    for name in STORES:
        (OUT / name).write_text(normal.text(imported[name]))
    cf.write(OUT / "states.json", normal.value(states))

    def temperatures(listed):
        return [e for e in listed if e["original_device_class"] == "temperature"]

    cf.write(OUT / "us_customary.json", normal.value({
        "doc": "The temperature sensors of the imported install after a start set to"
               " US customary units: their rows and states.",
        "entities": temperatures(us_rows),
        "states": {e["entity_id"]: us_states[e["entity_id"]]
                   for e in temperatures(us_rows) if e["entity_id"] in us_states},
    }))
    kept = json.loads(imported["core.entity_registry"])["data"]["entities"]
    cases = []
    for disable_new, before, after in restored:
        was = json.loads(before["core.entity_registry"])["data"]
        # By the row's own id: a unique id can be two entities' (a motion
        # sensor and the switch that enables it).
        gone = [e["id"] for e in kept if e not in was["entities"]]
        assert len(gone) == len(REMOVED), gone
        assert was["entities"] == [e for e in kept if e["id"] not in gone]
        for name in ("core.device_registry", "core.area_registry"):
            assert before[name] == imported[name] == after[name], name
        back = [e for e in rows(after) if e["id"] in gone]
        assert len(back) == len(gone), back
        rest = [e for e in rows(after) if e["id"] not in gone]
        assert rest == sorted(was["entities"], key=lambda e: e["entity_id"])
        cases.append({"pref_disable_new_entities": disable_new,
                      "deleted_entities": was["deleted_entities"], "after": back})
    cf.write(OUT / "restored.json", normal.value({
        "doc": "Rows the user removed whose resources the bridge still has, and those"
               " rows after the next start.",
        "removed": [{"domain": d, "unique_id_starts": s, "what": w}
                    for d, s, w in REMOVED],
        "cases": cases,
    }))


if __name__ == "__main__":
    # Home Assistant is started nine times in this process. CPython 3.14.7
    # can crash collecting what an instance leaves behind (as it can at
    # shutdown, see `config_flow.py`): nothing is collected.
    gc.disable()
    asyncio.run(main())
    # As `config_flow.py`: skip CPython's teardown.
    sys.stdout.flush()
    os._exit(0)

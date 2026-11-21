#!/usr/bin/env python3
"""Vectors for naming a Philips Hue or LIFX device in the add-integration
dialog's last step, and on the device page.

    .venv/bin/python -I scripts/vectors/device_names.py
    cargo test -p ha-api config_entries

Runs `device_step.py`'s messages on `config_flow.py`'s Home Assistant
2026.6.1 (its `run`), with `hue_flow.py`'s fake bridge (aiohue 4.8.1) or
`lifx_flow.py`'s fake bulbs (aiolifx 1.2.1) on 127.0.0.1 in place of
SwitchBot Cloud. Each scenario adds the integration through its flow, then
sends what frontend 20260527.4 sends: `config/area_registry/create`,
`config/device_registry/update` with `name_by_user` (and an area), and for
a device that was named `config/entity_registry/get_automatic_entity_ids`
for its enabled entities (the frontend's `hass.entities`) and
`config/entity_registry/update` with each `new_entity_id` it got. A device
named on its own page is named only (its entities keep their ids).

It writes crates/ha-api/src/config_entries/vectors/device_names.json, in
`device_step.py`'s form: for each scenario its steps and replies, the
registry and `state_changed` events each step fired, and at the end the
stored devices, entities and areas and the entities' states. And
`restarted`: the states after a restart. For those the scenario's changes
are made again on an install of its own (`hue_rows.py`'s `boot`, the
integration added through its flow, the changes made through the
registries as those commands make them), checked to leave the same
states, and Home Assistant is stopped and started again on its files.

Placeholders, sorting and the Hue group attributes as in those scripts. Run
it twice: the output must not change.
"""

import asyncio
import gc
import importlib.metadata
import json
import os
import pathlib
import sys

# After the standard library: this directory has an `http.py` of its own.
sys.path.append(str(pathlib.Path(__file__).resolve().parent))

import config_flow as cf  # noqa: E402
import device_step as ds  # noqa: E402
import hue_flow as hf  # noqa: E402
import hue_rows as hr  # noqa: E402
import lifx_flow as lf  # noqa: E402
import lifx_rows as lr  # noqa: E402

OUT = cf.API_OUT / "device_names.json"

# Hue devices, by the bridge's resource id.
LAMP = "13592c33-2ede-42f1-8e4a-c0e241bf29c4"
# The bedroom room: its light group, scenes and sensors.
BEDROOM_ROOM = "cb21f2c5-e722-494b-9d90-6bd3b26dcf10"
SENSOR = "df9083a4-75b9-46a1-9575-133f738d893d"
BRIDGE = "375f4b1e-5632-44d2-bafd-7bf40e2571d3"
LIFX_BULB = lf.COLOUR_SERIAL

device, area, entity, automatic = ds.device, ds.area, ds.entity, ds.automatic
LISTS = [*ds.LISTS, cf.ws({"type": "config/entity_registry/list"})]

SCENARIOS = [
    ("hue",
     "The dialog's last step after a Hue bridge is added: an area is made, the"
     " lamp is named and put in it, then the bedroom (a room: its light group,"
     " scenes and sensors) and the bridge are named, and each named device's"
     " entities take the ids that follow. Then the motion sensor is named on"
     " its device page (its entities keep their ids), named back, named again"
     " and given an empty name (its own counts again).",
     hf.Hue,
     [*hf.ADD,
      area(name="Study", **ds.BLANK),
      device(LAMP, name_by_user="Reading lamp", area_id="study"),
      device(BEDROOM_ROOM, name_by_user="Main bedroom"),
      device(BRIDGE, name_by_user="Hub"),
      automatic(["light.living_room_lamp",
                 "binary_sensor.bedroom_bedroom_motion_2", "light.bedroom_bedroom",
                 "scene.bedroom_bedroom_bright", "scene.bedroom_bedroom_dimmed",
                 "scene.bedroom_bedroom_honolulu", "sensor.bedroom_bedroom_illuminance_2",
                 "switch.hue_bridge_automation_go_to_sleep"]),
      entity("light.living_room_lamp", new_entity_id="light.study_reading_lamp"),
      entity("binary_sensor.bedroom_bedroom_motion_2",
             new_entity_id="binary_sensor.bedroom_main_bedroom_motion"),
      entity("light.bedroom_bedroom", new_entity_id="light.bedroom_main_bedroom"),
      entity("scene.bedroom_bedroom_bright", new_entity_id="scene.bedroom_main_bedroom_bright"),
      entity("scene.bedroom_bedroom_dimmed", new_entity_id="scene.bedroom_main_bedroom_dimmed"),
      entity("scene.bedroom_bedroom_honolulu",
             new_entity_id="scene.bedroom_main_bedroom_honolulu"),
      entity("sensor.bedroom_bedroom_illuminance_2",
             new_entity_id="sensor.bedroom_main_bedroom_illuminance"),
      entity("switch.hue_bridge_automation_go_to_sleep",
             new_entity_id="switch.hub_automation_go_to_sleep"),
      device(SENSOR, name_by_user="Bed sensor"),
      device(SENSOR, name_by_user=None),
      device(SENSOR, name_by_user="Bed sensor"),
      device(SENSOR, name_by_user=""),
      *LISTS]),
    ("lifx",
     "The dialog's last step after a LIFX bulb is added: an area is made, the"
     " bulb is named and put in it, and its enabled entities take the ids that"
     " follow. Then it is named again on its device page (its entities keep"
     " their ids), the name is taken away, made empty, and made the bulb's own.",
     lf.Lifx,
     [*lf.ADD,
      area(name="Study", **ds.BLANK),
      device(LIFX_BULB, name_by_user="Reading lamp", area_id="study"),
      automatic(["button.office_desk_lamp_identify", "button.office_desk_lamp_restart",
                 "light.office_desk_lamp"]),
      entity("button.office_desk_lamp_identify",
             new_entity_id="button.study_reading_lamp_identify"),
      entity("button.office_desk_lamp_restart",
             new_entity_id="button.study_reading_lamp_restart"),
      entity("light.office_desk_lamp", new_entity_id="light.study_reading_lamp"),
      device(LIFX_BULB, name_by_user="Desk"),
      device(LIFX_BULB, name_by_user=None),
      device(LIFX_BULB, name_by_user=""),
      device(LIFX_BULB, name_by_user="Desk Lamp"),
      *LISTS]),
]


async def main():
    cf.check_versions()
    for package, pinned in (("aiohue", hf.AIOHUE), ("aiolifx", lf.AIOLIFX)):
        found = importlib.metadata.version(package)
        if found != pinned:
            sys.exit(f"{package} is {found}, the vectors are pinned to {pinned}")
    order = [name for name, _ in ds.SUBSCRIBE]
    scenarios = []
    for name, doc, backend, steps in SCENARIOS:
        result = await cf.run(ds.SUBSCRIBE, json.loads(json.dumps(steps)), backend())
        result.pop("snapshots")
        for step in result["steps"]:
            step.pop("requests", None)
            if "events" in step:
                step["events"].sort(key=lambda e: (order.index(e["sub"]), ds.by_row(e)))
                for event in step["events"]:
                    for key in ("old_state", "new_state"):
                        state = event["event"]["data"].get(key) or {}
                        sort_sets(state.get("attributes", {}))
            kind = step.get("ws", {}).get("msg", {}).get("type", "")
            if kind in ds.SORTED and step["replies"][-1].get("success"):
                step["replies"][-1]["result"].sort(key=ds.SORTED[kind])
            if kind == "config/entity_registry/get_automatic_entity_ids":
                check_ids(steps, step)
        for state in result["states"].values():
            sort_sets(state["attributes"])
        restarted = await restart(name, backend, steps, result["states"])
        scenarios.append({"name": name, "doc": doc, **result, "restarted": restarted})
    cf.write(OUT, {"scenarios": scenarios})


async def restart(name, backend, steps, states):
    """The scenario's changes made again on an install of its own, which
    must leave `states`; the states after Home Assistant is started again
    on its files."""
    import pathlib
    import shutil
    import tempfile

    fake = backend()
    await fake.start()
    tmp = pathlib.Path(tempfile.mkdtemp())
    try:
        hass = await hr.boot(tmp, fake)
        if name == "hue":
            await hr.add_bridge(hass, fake)
        else:
            await lr.add(hass, lf.COLOUR)
        change(hass, name, steps)
        await hr.settle(hass)
        ids = cf.Ids()
        if ids.plain(hr.states_of(hass)) != states:
            sys.exit(f"{name}: the changes made again leave other states")
        await hr.stop(hass, tmp)
        hass = await hr.boot(tmp, fake)
        after = hr.states_of(hass)
        await hr.stop(hass, tmp)
    finally:
        await fake.stop()
        shutil.rmtree(tmp, ignore_errors=True)
    return ids.plain(after)


def change(hass, domain, steps):
    """The scenario's registry commands, as the WebSocket API makes them."""
    from homeassistant.helpers import (area_registry as ar, device_registry as dr,
                                       entity_registry as er)

    areas, devices, entities = ar.async_get(hass), dr.async_get(hass), er.async_get(hass)
    for step in steps:
        msg = step.get("ws", {}).get("msg", {})
        kind = msg.get("type")
        if kind == "config/area_registry/create":
            areas.async_create(msg["name"])
        elif kind == "config/device_registry/update":
            ident = msg["device_id"][len("<device:"):-1]
            device = devices.async_get_device(identifiers={(domain, ident)})
            fields = {k: msg[k] for k in ("name_by_user", "area_id") if k in msg}
            devices.async_update_device(device.id, **fields)
        elif kind == "config/entity_registry/update":
            entities.async_update_entity(msg["entity_id"],
                                         new_entity_id=msg["new_entity_id"])


def sort_sets(attributes):
    """A Hue group's scenes and members: sets in HA."""
    for key in ("hue_scenes", "lights", "entity_id"):
        if isinstance(attributes.get(key), list):
            attributes[key].sort()


def check_ids(steps, step):
    """The ids sent afterwards are the ones HA gave (a script that sends
    others would record something no frontend sends)."""
    given = step["replies"][-1]["result"]
    sent = {s["ws"]["msg"]["entity_id"]: s["ws"]["msg"]["new_entity_id"]
            for s in steps if "new_entity_id" in s.get("ws", {}).get("msg", {})}
    wanted = {old: new for old, new in given.items() if new}
    if wanted != sent:
        sys.exit(f"HA gives {wanted}, the script sends {sent}")


if __name__ == "__main__":
    # As `hue_rows.py`: HA is started several times in this process.
    gc.disable()
    asyncio.run(main())
    # As `config_flow.py`: skip CPython's teardown.
    sys.stdout.flush()
    os._exit(0)

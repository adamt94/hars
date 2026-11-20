#!/usr/bin/env python3
"""Vectors for the add-integration dialog's last step, where each new device
is given a name and an area, and for the same changes made from the Devices
page.

    .venv/bin/python -I scripts/vectors/device_step.py
    cargo test -p ha-api config_entries

Runs on `config_flow.py`'s Home Assistant 2026.6.1 (its `run`: `http`,
`auth`, `websocket_api`, `config` and `switchbot_cloud`, against the
recorded SwitchBot responses on 127.0.0.1) and sends what frontend
20260527.4 sends: `config/area_registry/create` (the area picker's dialog),
`config/device_registry/update` (`step-flow-create-entry.ts` and the device
page's dialog), then for a device that was named
`config/entity_registry/get_automatic_entity_ids` and
`config/entity_registry/update` with each `new_entity_id`. It writes
crates/ha-api/src/config_entries/vectors/device_step.json: for each
scenario its steps and replies, the `device_registry_updated`,
`area_registry_updated`, `entity_registry_updated` and `state_changed`
events each step fired (a state as its text and attributes), and at the end
the stored devices, entities and areas and the entities' states.

Placeholders are `config_flow.py`'s. Within a step, a subscription's events
are sorted by the row they are about, keeping each row's own order (HA sets
the platforms up side by side, in an order that changes from run to run).
The device and entity lists are sorted too. A label list has one label at
most (a set in HA). Run it twice: the output
must not change.
"""

import asyncio
import json
import os
import pathlib
import sys

# After the standard library: this directory has an `http.py` of its own.
sys.path.append(str(pathlib.Path(__file__).resolve().parent))

import config_flow as cf  # noqa: E402

OUT = cf.API_OUT / "device_step.json"
BOT, HUB, AC = cf.BOT, cf.HUB, "01-202301010000-00000001"


def events(event_type):
    return {"type": "subscribe_events", "event_type": event_type}


SUBSCRIBE = [("devices", events("device_registry_updated")),
             ("areas", events("area_registry_updated")),
             ("entities", events("entity_registry_updated")),
             ("states", events("state_changed"))]
ADD = [cf.START, cf.http("POST", f"{cf.FLOW}/<flow:1>", cf.GOOD)]


def device(ident, user="admin", **fields):
    return cf.ws({"type": "config/device_registry/update",
                  "device_id": f"<device:{ident}>", **fields}, user)


def area(user="admin", **fields):
    return cf.ws({"type": "config/area_registry/create", **fields}, user)


def entity(entity_id, user="admin", **fields):
    return cf.ws({"type": "config/entity_registry/update", "entity_id": entity_id,
                  **fields}, user)


def automatic(entity_ids, user="admin"):
    return cf.ws({"type": "config/entity_registry/get_automatic_entity_ids",
                  "entity_ids": entity_ids}, user)


LISTS = [cf.ws({"type": f"config/{name}_registry/list"}) for name in ("device", "area")]
# What the area dialog sends for a new area with only a name.
BLANK = {"labels": [], "aliases": [], "temperature_entity_id": None,
         "humidity_entity_id": None}

SCENARIOS = [
    ("dialog",
     "The dialog's last step after SwitchBot Cloud is added: an area is made in"
     " the picker, the Bot is named and put in it, its entities take the ids"
     " that follow from the name, the Hub is only put in the area, and the air"
     " conditioner is only named.",
     SUBSCRIBE,
     [*ADD,
      area(name="Kitchen", **BLANK),
      device(BOT, name_by_user="Coffee machine", area_id="kitchen"),
      automatic(["button.towel_switch", "sensor.towel_switch_battery"]),
      entity("button.towel_switch", new_entity_id="button.kitchen_coffee_machine"),
      entity("sensor.towel_switch_battery",
             new_entity_id="sensor.kitchen_coffee_machine_battery"),
      device(HUB, area_id="kitchen"),
      device(AC, name_by_user="Bedroom AC"),
      automatic(["climate.air_conditioner", "switch.air_conditioner",
                 "sensor.hub_2_9d_humidity"]),
      *LISTS,
      cf.ws({"type": "config/entity_registry/list"})]),
    ("device_update",
     "What `config/device_registry/update` takes and refuses, and what each"
     " change fires: the device page's dialog sends all four fields.",
     SUBSCRIBE,
     [*ADD,
      device(BOT),
      device(BOT, name_by_user=None, area_id=None, labels=[], disabled_by=None),
      device(BOT, name_by_user="Towel rail"),
      device(BOT, name_by_user="Towel rail"),
      device(BOT, name_by_user=""),
      device(BOT, name_by_user=None),
      device(BOT, area_id="nowhere"),
      device(BOT, area_id=None),
      device(BOT, labels=["bath"]),
      device(BOT, labels=["bath", "bath"]),
      device(BOT, labels=[]),
      device(BOT, name_by_user="Towel rail", area_id="attic", labels=["bath"],
             disabled_by=None),
      cf.ws({"type": "config/device_registry/update", "device_id": "nobody",
             "name_by_user": "x"}),
      cf.ws({"type": "config/device_registry/update", "name_by_user": "x"}),
      cf.ws({"type": "config/device_registry/update", "device_id": 5}),
      device(BOT, area_id=5),
      device(BOT, name_by_user=5),
      device(BOT, labels="bath"),
      device(BOT, labels=[1]),
      device(BOT, disabled_by="integration"),
      device(BOT, disabled_by="config_entry"),
      device(BOT, disabled_by=5),
      device(BOT, name="x"),
      device(BOT, "user", name_by_user="Mine"),
      cf.ws({"type": "config/device_registry/update", "device_id": "nobody"}, "user"),
      *LISTS]),
    ("area_create",
     "What `config/area_registry/create` takes and refuses: ids from the name,"
     " a name already in use, each field, and sensors that must exist and be"
     " of the right kind.",
     SUBSCRIBE,
     [*ADD,
      area(name="Living Room"),
      area(name="living room"),
      area(name="LivingRoom"),
      area(name=" Living Room "),
      area(name="Living-Room"),
      area(name="Living_Room"),
      area(name="Über Büro", **BLANK),
      area(name="!!!"),
      area(name="???"),
      area(name=""),
      area(name="Office", aliases=[" desk ", "", "desk"], floor_id="ground",
           icon="mdi:desk", labels=["work"], picture="/api/image/serve/abc/512x512"),
      area(name="Bath", temperature_entity_id="sensor.hub_2_9d_temperature",
           humidity_entity_id="sensor.hub_2_9d_humidity"),
      area(name="Hall", picture=None),
      area(name="Sauna", humidity_entity_id="Sensor.Hub_2_9D_Humidity"),
      area(name="Loft", temperature_entity_id="sensor.nope"),
      area(name="Loft", humidity_entity_id="sensor.nope"),
      area(name="Loft", temperature_entity_id="sensor.hub_2_9d_humidity"),
      area(name="Loft", humidity_entity_id="sensor.hub_2_9d_temperature"),
      area(name="Loft", temperature_entity_id="button.towel_switch"),
      area(name="Loft", humidity_entity_id="button.towel_switch"),
      area(name="Living Room", temperature_entity_id="sensor.nope"),
      area(),
      area(name=5),
      area(name=None),
      area(name="Loft", aliases="desk"),
      area(name="Loft", aliases=[5]),
      area(name="Loft", labels="work"),
      area(name="Loft", labels=[1]),
      area(name="Loft", icon=None),
      area(name="Loft", floor_id=None),
      area(name="Loft", picture=5),
      area(name="Loft", temperature_entity_id=5),
      area(name="Loft", area_id="loft"),
      area("user", name="Mine"),
      area("user"),
      *LISTS]),
    ("entity_ids",
     "The ids that follow from the names, and what a new entity id is checked"
     " against.",
     SUBSCRIBE,
     [*ADD,
      automatic(["sensor.hub_2_9d_humidity", "sensor.nope", "button.towel_switch"]),
      automatic([]),
      automatic("sensor.hub_2_9d_humidity, sensor.hub_2_9d_temperature"),
      automatic(["Sensor.Hub_2_9D_Humidity"]),
      automatic(["not an id"]),
      automatic(5),
      automatic(None),
      automatic({"sensor.hub_2_9d_humidity": 1, "sensor.nope": 2}),
      cf.ws({"type": "config/entity_registry/get_automatic_entity_ids"}),
      automatic(["sensor.hub_2_9d_humidity"], "user"),
      entity("sensor.hub_2_9d_humidity", new_entity_id="sensor.hub_2_9d_temperature"),
      entity("sensor.hub_2_9d_humidity", new_entity_id="sensor.Not An Id"),
      # The state machine is asked in lower case, before the id is checked.
      entity("sensor.hub_2_9d_humidity", new_entity_id="sensor.Hub_2_9D_Temperature"),
      entity("sensor.hub_2_9d_humidity", new_entity_id="sensor.Damp"),
      entity("sensor.hub_2_9d_humidity", new_entity_id="binary_sensor.damp"),
      entity("sensor.hub_2_9d_humidity", new_entity_id="sensor.hub_2_9d_humidity"),
      entity("sensor.hub_2_9d_humidity", new_entity_id=5),
      entity("sensor.nope", new_entity_id="sensor.damp"),
      entity("sensor.hub_2_9d_humidity", "user", new_entity_id="sensor.damp"),
      entity("sensor.hub_2_9d_humidity", new_entity_id="sensor.damp"),
      automatic(["sensor.damp", "sensor.hub_2_9d_humidity"]),
      # A device given another device's name: its entities' ids follow.
      device(HUB, name_by_user="Towel switch"),
      automatic(["sensor.damp", "sensor.hub_2_9d_temperature",
                 "sensor.towel_switch_battery", "button.towel_switch"]),
      entity("sensor.damp", new_entity_id="sensor.towel_switch_humidity"),
      entity("sensor.towel_switch_humidity", new_entity_id="sensor.damp",
             area_id="cellar"),
      cf.ws({"type": "config/entity_registry/get", "entity_id": "sensor.damp"}),
      cf.ws({"type": "config/entity_registry/get",
             "entity_id": "sensor.hub_2_9d_humidity"})]),
]


SORTED = {"config/device_registry/list": lambda d: json.dumps(d["identifiers"]),
          "config/entity_registry/list": lambda e: e["entity_id"]}


def by_row(event):
    """The registry row or entity an event is about."""
    data = event["event"]["data"]
    return data.get("entity_id") or data.get("device_id") or data.get("area_id") or ""


async def main():
    cf.check_versions()
    order = [name for name, _ in SUBSCRIBE]
    scenarios = []
    for name, doc, subscriptions, steps in SCENARIOS:
        result = await cf.run(subscriptions, json.loads(json.dumps(steps)))
        result.pop("snapshots")
        for step in result["steps"]:
            if "events" in step:
                step["events"].sort(key=lambda e: (order.index(e["sub"]), by_row(e)))
            # The lists are in the order HA made the rows.
            kind = step.get("ws", {}).get("msg", {}).get("type", "")
            if kind in SORTED and step["replies"][-1].get("success"):
                step["replies"][-1]["result"].sort(key=SORTED[kind])
        scenarios.append({"name": name, "doc": doc, **result})
    cf.write(OUT, {"scenarios": scenarios})


if __name__ == "__main__":
    asyncio.run(main())
    # As `config_flow.py`: skip CPython's teardown.
    sys.stdout.flush()
    os._exit(0)

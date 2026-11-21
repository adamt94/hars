#!/usr/bin/env python3
"""Vectors for LIFX's registry rows on an install that already has them.

    .venv/bin/python -I scripts/vectors/lifx_rows.py
    cargo test -p ha-api lifx && cargo test -p ha-server --test lifx_imported

Home Assistant 2026.6.1 with `lifx` and aiolifx 1.2.1 is started several
times on one config directory, as an install is over its life, against
`lifx_flow.py`'s fake bulbs (nothing reaches the network; HA's own scan,
which starts discovery flows `hars` doesn't have, is put off as there).

At the first start the colour bulb, the white bulb and a third, "Porch",
are added through the flow, and the user changes things as the frontend
does: the colour light renamed, given another id, an icon and an area;
the colour bulb's device renamed and moved; a button disabled, another
hidden; the RSSI sensor (disabled by default) renamed; the white light
disabled; then the colour bulb's entry set to "new entities start
disabled". From the second start on the porch bulb answers no more: its
entry waits for a retry and its rows stay as they are.

It writes, in crates/ha-integrations/src/lifx/vectors/imported/:

- `core.config_entries`, `core.device_registry`, `core.entity_registry`,
  `core.area_registry`: the files as HA left them after the third start
  (checked: the same as after the second, so HA itself changes nothing on
  a start), and `states.json`: the entities' states then. A start of
  `hars` on these files must leave them byte for byte;
- `duplicate.json`: a second entry for the colour bulb, at another of its
  addresses, added once the first is set up (an imported install can have
  two): the entries and the device and entity rows after its set-up, the
  first entry as `<first>` and the second as `<second>`;
- `restored.json`: rows the user removed come back at the next start,
  once with the colour entry's "new entities start disabled" on and once
  with it off. Before that start the stores are the files above but for
  that preference, the removed rows (named by their ids) and the entity
  registry's `deleted_entities`, which is given; after it, the rows that
  came back (checked: no other row changed).

Ids and times are replaced as `hue_rows.py` replaces them, and rows put in
an order of their own. Run it twice: the output must not change.
"""

import asyncio
import gc
import importlib.metadata
import json
import os
import pathlib
import shutil
import sys
import tempfile

sys.path.append(str(pathlib.Path(__file__).resolve().parent))

import config_flow as cf  # noqa: E402
import hue_rows as hr  # noqa: E402
import lifx_flow as lf  # noqa: E402

OUT = lf.LIFX / "imported"
STORES = hr.STORES
PORCH = "192.0.2.27"
PORCH_SERIAL = "d0:73:d5:00:00:27"


class Fake(lf.Lifx):
    def make_bulbs(self):
        super().make_bulbs()
        self.bulbs[PORCH] = lf.Bulb(PORCH, PORCH_SERIAL, "Porch", "Garden", 91, (3, 70),
                                    discovery=False)


async def add(hass, host):
    """A bulb added by its address."""
    flows = hass.config_entries.flow
    result = await flows.async_init("lifx", context={"source": "user"})
    result = await flows.async_configure(result["flow_id"], {"host": host})
    assert result["type"].value == "create_entry", result
    await hr.settle(hass)
    return result["result"]


def user_changes(hass):
    """What a user changes in Settings."""
    from homeassistant.helpers import (area_registry as ar, device_registry as dr,
                                       entity_registry as er)

    entities, devices, areas = er.async_get(hass), dr.async_get(hass), ar.async_get(hass)
    den = areas.async_create("Den").id
    user, hidden = er.RegistryEntryDisabler.USER, er.RegistryEntryHider.USER

    def unique(domain, unique_id):
        return entities.async_get_entity_id(domain, "lifx", unique_id)

    colour, white = lf.COLOUR_SERIAL, lf.WHITE_SERIAL
    change = entities.async_update_entity
    change(unique("light", colour), name="Reading light", new_entity_id="light.reading",
           icon="mdi:lamp", area_id=den)
    change(unique("button", f"{colour}_identify"), disabled_by=user)
    change(unique("button", f"{colour}_restart"), hidden_by=hidden)
    change(unique("sensor", f"{colour}_rssi"), name="Signal")
    change(unique("light", white), disabled_by=user)
    change(unique("button", f"{white}_identify"), name="Blink",
           new_entity_id="button.blink")
    device = devices.async_get_device(identifiers={("lifx", colour)})
    devices.async_update_device(device.id, name_by_user="Desk", area_id=den)


# Rows the user removes though their bulb is still there: (domain, unique id).
REMOVED = [
    ("light", lf.COLOUR_SERIAL),
    ("button", f"{lf.COLOUR_SERIAL}_identify"),
    ("button", f"{lf.COLOUR_SERIAL}_restart"),
    ("sensor", f"{lf.COLOUR_SERIAL}_rssi"),
]


def remove_rows(hass):
    from homeassistant.helpers import entity_registry as er

    entities = er.async_get(hass)
    for domain, unique_id in REMOVED:
        entities.async_remove(entities.async_get_entity_id(domain, "lifx", unique_id))


async def duplicate(fake):
    """The colour bulb added, then a second entry for it at its other
    address: the stores after the second's set-up."""
    from types import MappingProxyType

    from homeassistant.config_entries import ConfigEntry

    tmp = pathlib.Path(tempfile.mkdtemp())
    try:
        hass = await hr.boot(tmp, fake)
        first = await add(hass, lf.COLOUR)
        second = ConfigEntry(data={"host": lf.ALIAS}, discovery_keys=MappingProxyType({}),
                             domain="lifx", minor_version=1, options={}, source="user",
                             subentries_data=None, title="Desk Lamp",
                             unique_id=lf.COLOUR_SERIAL, version=1)
        await hass.config_entries.async_add(second)
        await hr.settle(hass)
        states = sorted(s.entity_id for s in hass.states.async_all()
                        if s.entity_id.split(".")[0] in ("light", "button", "sensor"))
        files = hr.in_order(await hr.stop(hass, tmp))
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    text = {name: files[name].replace(first.entry_id, "<first>")
            .replace(second.entry_id, "<second>") for name in STORES}
    normal = hr.Normal()
    return normal.value({
        "doc": "A second entry for the colour bulb, at another of its addresses, added"
               " once the first is set up: the stores after its set-up, and the"
               " entities that have a state.",
        "entries": json.loads(text["core.config_entries"])["data"]["entries"],
        "devices": json.loads(text["core.device_registry"])["data"]["devices"],
        "entities": json.loads(text["core.entity_registry"])["data"]["entities"],
        "states": states,
    })


def colour_entry(hass):
    return next(e for e in hass.config_entries.async_entries("lifx")
                if e.unique_id == lf.COLOUR_SERIAL)


async def main():
    cf.check_versions()
    found = importlib.metadata.version("aiolifx")
    if found != lf.AIOLIFX:
        sys.exit(f"aiolifx is {found}, the vectors are pinned to {lf.AIOLIFX}")
    fake = Fake()
    await fake.start()
    duplicated = await duplicate(fake)
    tmp = pathlib.Path(tempfile.mkdtemp())
    scratch = []
    try:
        hass = await hr.boot(tmp, fake)
        for host in (lf.COLOUR, lf.WHITE, PORCH):
            await add(hass, host)
        user_changes(hass)
        await hr.settle(hass)
        hass.config_entries.async_update_entry(colour_entry(hass),
                                               pref_disable_new_entities=True)
        await hr.settle(hass)
        await hr.stop(hass, tmp)

        # From the second start on the porch bulb is gone. A start after
        # that changes nothing.
        fake.bulbs[PORCH].silent = True
        second = await hr.stop(await hr.boot(tmp, fake), tmp)
        hass = await hr.boot(tmp, fake)
        states = hr.states_of(hass)
        imported = await hr.stop(hass, tmp)
        for name in STORES:
            if second[name] != imported[name]:
                sys.exit(f"HA changed {name} on a start with nothing new")
        imported = hr.in_order(imported)

        def branch():
            other = pathlib.Path(tempfile.mkdtemp())
            scratch.append(other)
            shutil.copytree(tmp, other, dirs_exist_ok=True)
            return other

        restored = []
        for disable_new in (True, False):
            other = branch()
            hass = await hr.boot(other, fake)
            # HA reloads the entry when its preference changes: before the
            # rows are removed, or the reload would bring them back.
            hass.config_entries.async_update_entry(
                colour_entry(hass), pref_disable_new_entities=disable_new)
            await hr.settle(hass)
            remove_rows(hass)
            before = hr.in_order(await hr.stop(hass, other))
            after = hr.in_order(await hr.stop(await hr.boot(other, fake), other))
            restored.append((disable_new, before, after))
    finally:
        await fake.stop()
        for path in (tmp, *scratch):
            shutil.rmtree(path, ignore_errors=True)

    normal = hr.Normal()
    OUT.mkdir(parents=True, exist_ok=True)
    for name in STORES:
        (OUT / name).write_text(normal.text(imported[name]))
    cf.write(OUT / "states.json", normal.value(states))
    cf.write(OUT / "duplicate.json", duplicated)
    kept = json.loads(imported["core.entity_registry"])["data"]["entities"]
    cases = []
    for disable_new, before, after in restored:
        was = json.loads(before["core.entity_registry"])["data"]
        gone = [e["id"] for e in kept if e not in was["entities"]]
        assert len(gone) == len(REMOVED), gone
        assert was["entities"] == [e for e in kept if e["id"] not in gone]
        for name in ("core.device_registry", "core.area_registry"):
            assert before[name] == imported[name] == after[name], name
        back = [e for e in hr.rows(after) if e["id"] in gone]
        assert len(back) == len(gone), back
        rest = [e for e in hr.rows(after) if e["id"] not in gone]
        assert rest == sorted(was["entities"], key=lambda e: e["entity_id"])
        cases.append({"pref_disable_new_entities": disable_new,
                      "deleted_entities": was["deleted_entities"], "after": back})
    cf.write(OUT / "restored.json", normal.value({
        "doc": "Rows the user removed whose bulb is still there, and those rows after"
               " the next start.",
        "removed": [{"domain": d, "unique_id": u} for d, u in REMOVED],
        "cases": cases,
    }))


if __name__ == "__main__":
    # As `hue_rows.py`: HA is started several times in this process.
    gc.disable()
    asyncio.run(main())
    sys.stdout.flush()
    os._exit(0)

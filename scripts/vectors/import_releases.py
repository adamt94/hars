#!/usr/bin/env python3
"""Fixtures for `hars import`: one synthetic install as each Home Assistant
release leaves it on disk.

    # One virtualenv per release (Python 3.14.2+); what each needs is in
    # scripts/vectors/README.md ("Running one").
    .venv-2026.6.1/bin/python -I scripts/vectors/import_releases.py seed
    # ... then, under each later release:
    .venv-2026.7.4/bin/python -I scripts/vectors/import_releases.py upgrade
    # ... and last, with 2026.6.1 again:
    .venv-2026.6.1/bin/python -I scripts/vectors/import_releases.py downgrade
    cargo test -p ha-server --test import
    cargo test -p ha-integrations store_downgrade

Unlike the other generators, which run the pinned release only, this one
runs later releases too: its subject is what they leave on disk, and what
the pinned release (`SEED`) does with that.

It writes crates/ha-server/tests/vectors/import/:

- `2026.6.1.json` (`seed`): a config directory made from nothing by Home
  Assistant 2026.6.1's own code. It is the start of a round trip (`hars
  import` must turn what a later release makes of it back into it), so it
  holds what the later releases store differently: an owner with a
  session, a location, a person, a zone, a dashboard, the owner's frontend
  settings; config entries for the integrations hars runs, shaped as each
  one's 2026.6.1 config flow makes them (a `lifx` bulb and the shared
  `lifx` entry from before one entry per bulb, a `met` forecast that
  follows the home and one for a fixed place, `hue`, `switchbot_cloud`,
  `sun`, a phone's `mobile_app` registration), and for three it doesn't
  (one with a subentry); areas, a floor, a label, a category; devices (one
  shared by two config entries, one reached through it, one of a subentry
  and one reached through that, one renamed and disabled by the user, one
  removed, the Hue ones with their Zigbee address) and entities (named by
  the user with the device's name in front, without it, exactly it, and
  with an area of their own; named the old way; hidden, disabled, exposed
  to Assist, with options; one removed); the last states of entities with
  and without a registry row; the analytics choices, a Labs preview
  switched on, `mobile_app`'s store and the default YAML with three
  automations, one of which names the shared device by its id.
- `<release>.json` (`upgrade`, run under each later release): that
  directory after the release started on it, as a user who updated Home
  Assistant has it. The release's own code loads every store (running its
  storage migrations), migrates the config entries of the integrations hars
  runs (`ConfigEntry.async_migrate`, which calls the integration's
  `async_migrate_entry`), and then changes something in each store through
  its own API so the release's writer produces the file. Three
  integrations also change their registry rows when they are set up; they
  can't be set up here, so the registry calls they make are made instead
  (`register_as_setup_does`, which says what is read from source rather
  than run). `written_by` in the fixture says which files the release
  wrote and which it left as 2026.6.1 wrote them (Home Assistant only
  rewrites a store that changes).
- `downgrade.json` (`downgrade`, run under 2026.6.1): what Home Assistant
  2026.6.1 itself does with each of those directories. `loads`: whether
  its `bootstrap.async_load_base_functionality` (registries, config
  entries, restore state) accepts it, with the errors it logged when it
  doesn't. hars copies the ones it loads and converts the ones it refuses
  (elsewhere it is stricter than Home Assistant, for example on an older
  `core.config_entries`, which Home Assistant migrates). This is
  the base of 2026.6.1's start-up only: it sets no integration up, and it
  stops at the first store it can't read (the device registry, for 2026.8
  and later), so it says nothing about what else in those directories
  2026.6.1 could not use.

and, also from `downgrade`,
crates/ha-integrations/src/mobile_app/vectors/store_downgrade.json: for
each directory 2026.6.1 loads whose `mobile_app` store is of a newer minor
version (1.2 from 2026.7, with Live Activity tokens), the file after
2026.6.1's `mobile_app` loaded it, and after its next change (a removed
registration, `async_remove_entry`). hars must keep what this keeps
(`cargo test -p ha-integrations store_downgrade`).

A fourth mode writes nothing: `loads <directory>`, under 2026.6.1, runs
that same bootstrap on a directory `hars import` wrote and prints whether
it loads and what 2026.6.1's writers change in the registries hars
converted ([README](README.md), "Running one").

Each fixture is `{"ha_version", "notes", "written_by", "files": {path:
text}}` (`notes`: the config entries the release migrated, with their
versions before and after, and the integrations whose registry calls were
made for them), the
files exactly as Home Assistant wrote them except for what changes from run
to run: ids, tokens and times are replaced by fixed ones
(`normalise` below), a set Home Assistant lists in whatever order it
iterates is sorted, and the session's `expire_at` is far in the future so
the fixture doesn't expire. Passwords are hashed with a fixed salt. The
restore state's times are fixed too, so its rows are older than the seven
days Home Assistant keeps an unclaimed one: before a release starts on the
directory they are made current (`refresh_restore_state`).

Everything is synthetic: no real install is read, the coordinates are a
rounded city centre, the hosts are documentation addresses (192.0.2.0/24),
the hardware ids keep only their vendor's prefix and the keys are made up.
Nothing reaches the network: the only integrations set up are `http` (on a
free loopback port), `person`, `zone` and `labs`; of the others only the
storage and migration code runs. Run it twice: the output must not change.

`mobile_app`'s store class and its Live Activity tokens live in the
integration itself, which imports its camera and cloud libraries: the
virtualenvs also need the packages `mobile_app_notify.py` lists
([README](README.md)), at each release's own pins
(`homeassistant/package_constraints.txt` in its wheel).
"""

import asyncio
import datetime
import gc
import hashlib
import importlib.metadata
import json
import os
import pathlib
import re
import socket
import sys
import tempfile
from unittest.mock import patch

SEED = "2026.6.1"
OUT = (
    pathlib.Path(__file__).resolve().parents[2]
    / "crates/ha-server/tests/vectors/import"
)
MOBILE_APP_OUT = (
    pathlib.Path(__file__).resolve().parents[2]
    / "crates/ha-integrations/src/mobile_app/vectors/store_downgrade.json"
)
CLIENT = "http://127.0.0.1:8123/"
PASSWORD = "synthetic-password"
# A fixed bcrypt salt (22 characters of bcrypt's base64), so the hash of
# the synthetic password is the same on every run.
SALT = b"$2b$12$" + b"abcdefghijklmnopqrstuu"
# 2100-01-01: the session in the fixture never expires.
FAR_FUTURE = "4102444800.0"

# The integrations hars runs, whose entries each release migrates.
RUNS = ["sun", "met", "hue", "switchbot_cloud", "lifx", "mobile_app"]
# Made-up hardware ids, each with its vendor's prefix.
LIFX_SERIAL = "d0:73:d5:00:00:01"
LIFX_LABEL = "Desk"
SWITCHBOT_BOT = "AABBCC000001"
HUE_BRIDGE = "00000000-0000-4000-8000-000000000100"
HUE_HALL = "00000000-0000-4000-8000-000000000101"
HUE_PORCH = "00000000-0000-4000-8000-000000000102"
HUE_GONE = "00000000-0000-4000-8000-000000000103"
WEBHOOK_ID = hashlib.sha256(b"synthetic webhook").hexdigest()

AUTOMATIONS = """\
- id: '1718000000001'
  alias: Hall light at dusk
  description: ''
  triggers:
  - trigger: time
    at: '21:30:00'
  conditions: []
  actions:
  - action: light.turn_on
    metadata: {}
    data: {}
    target:
      entity_id: light.hall
  mode: single
- id: '1718000000002'
  alias: Wait then off
  description: ''
  triggers:
  - trigger: time
    at: '23:30:00'
  conditions: []
  actions:
  - delay: '{{ 5 }}'
  - action: light.turn_off
    metadata: {}
    data: {}
    target:
      entity_id: light.hall
  mode: single
"""
# An action on a device, by its id: the device two integrations share,
# which 2026.8 replaces with two devices of other ids.
AUTOMATION_DEVICE = """\
- id: '1718000000004'
  alias: Desk off at night
  description: ''
  triggers:
  - trigger: time
    at: '00:30:00'
  conditions: []
  actions:
  - action: light.turn_off
    metadata: {{}}
    data: {{}}
    target:
      device_id: {device_id}
  mode: single
"""
# What the automation editor of 2026.7 and later can save: a trigger that
# 2026.6 keeps behind a Labs preview.
AUTOMATION_NEW_TRIGGER = """\
- id: '1720000000003'
  alias: When the hall light turns on
  description: ''
  triggers:
  - trigger: light.turned_on
    target:
      entity_id: light.hall
    options:
      behavior: each
  conditions: []
  actions:
  - action: light.turn_on
    metadata: {}
    data: {}
    target:
      entity_id: light.desk
  mode: single
"""


def ha_version():
    return importlib.metadata.version("homeassistant")


# ----- normalising what changes from run to run -----

HEX32 = re.compile(r'(?<![0-9a-f])[0-9a-f]{32}(?![0-9a-f])')
HEX64 = re.compile(r'(?<![0-9a-f])[0-9a-f]{64}(?![0-9a-f])')
HEX128 = re.compile(r'(?<![0-9a-f])[0-9a-f]{128}(?![0-9a-f])')
ULID = re.compile(r'(?<![0-9A-Z])[0-9A-HJKMNP-TV-Z]{26}(?![0-9A-Z])')
ISO = re.compile(r'\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?\+00:00')
FLOAT_TIMES = re.compile(r'"(expire_at|orphaned_timestamp)":\s*(\d+\.\d+)')
# Sets in Home Assistant, listed in an order that changes from run to run.
SETS = re.compile(
    r'"(config_entries|labels)":(\[[^\]]*\])'
    r'|"(identifiers|connections)":(\[(?:\[[^\]]*\],?)*\])'
)
ID = re.compile("|".join(p.pattern for p in (HEX128, HEX64, HEX32, ULID)))


def tag(version):
    """`2026.7.4` as `0704`: marks the ids and times a release made."""
    _, month, patch_ = version.split(".")
    return f"{int(month):02d}{int(patch_):02d}"


def normalise(files, version):
    """Replace ids, tokens and times by fixed ones, the same in every file.

    A value an earlier release's fixture already fixed is kept: it starts
    with zeros (ids) or is one of the fixed times.
    """
    mark = tag(version)
    month = int(version.split(".")[1])
    fixed_time = f"2026-{month:02d}-15T12:00:00.{mark}00+00:00"
    known_times = re.compile(r"2026-\d\d-15T12:00:00\.\d{4}00\+00:00")
    ids = {}

    def new_id(match):
        old = match.group(0)
        if old.startswith("0" * 14):
            return old
        if old not in ids:
            n = len(ids) + 1
            ids[old] = "0" * (len(old) - 12) + mark + f"{n:08d}"
        return ids[old]

    def new_time(match):
        old = match.group(0)
        return old if known_times.fullmatch(old) else fixed_time

    out = {}
    # File names carry ids too (`frontend.user_data_<user id>`).
    for path in sorted(files):
        text = ID.sub(new_id, files[path])
        text = ISO.sub(new_time, text)
        text = FLOAT_TIMES.sub(
            lambda m: f'"{m.group(1)}": '
            + (FAR_FUTURE if m.group(1) == "expire_at" else "1781524800.0"),
            text,
        )
        text = SETS.sub(
            lambda m: f'"{m.group(1) or m.group(3)}":'
            + json.dumps(
                sorted(json.loads(m.group(2) or m.group(4))),
                separators=(",", ":"),
                ensure_ascii=False,
            ),
            text,
        )
        out[ID.sub(new_id, path)] = text
    return dict(sorted(out.items()))


def read_dir(root):
    files = {}
    for path in sorted(root.rglob("*")):
        rel = path.relative_to(root).as_posix()
        # `.storage/http` holds the free port this run's `http` was given.
        if path.is_file() and rel != ".storage/http":
            if rel.endswith((".log", ".db", ".migration_backup")) or ".log." in rel:
                continue
            files[rel] = path.read_text()
    return files


def write_dir(root, files):
    for rel, text in files.items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)


def refresh_restore_state(config_dir):
    """Make the restore state's times now.

    The fixed times a fixture holds are in the past, and Home Assistant
    drops a restored state it hasn't seen for seven days when it next
    dumps them. `normalise` fixes the times again afterwards.
    """
    path = config_dir / ".storage/core.restore_state"
    now = datetime.datetime.now(datetime.UTC).isoformat()
    path.write_text(ISO.sub(now, path.read_text()))


def save_fixture(name, payload):
    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / name).write_text(json.dumps(payload, indent=1, ensure_ascii=False) + "\n")


def load_fixture(version):
    return json.loads((OUT / f"{version}.json").read_text())


# ----- Home Assistant, as far as its storage -----


async def boot(config_dir):
    """A `HomeAssistant` with its registries, config entries and auth
    loaded from `config_dir`, as `bootstrap` does before any integration."""
    from homeassistant import bootstrap, core, loader
    from homeassistant.auth import auth_manager_from_config
    from homeassistant.config_entries import ConfigEntries
    from homeassistant.setup import async_setup_component

    hass = core.HomeAssistant(str(config_dir))
    hass.config.skip_pip = True
    await hass.config.async_load()
    loader.async_setup(hass)
    hass.config_entries = ConfigEntries(hass, {})
    loads = await bootstrap.async_load_base_functionality(hass)
    if not loads:
        return hass, False
    hass.auth = await auth_manager_from_config(hass, [{"type": "homeassistant"}], [])
    # `person` depends on `http`: give it a free loopback port, never the
    # default 8123 (a real Home Assistant may be running on this machine).
    http = {"server_host": "127.0.0.1", "server_port": free_port()}
    assert await async_setup_component(hass, "http", {"http": http})
    return hass, True


async def stop(hass):
    """Flush every delayed write (`homeassistant_final_write`)."""
    await hass.async_block_till_done()
    await hass.async_stop(force=True)


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def instances(cls):
    return [o for o in gc.get_objects() if isinstance(o, cls)]


def mobile_app_store(hass):
    """`mobile_app`'s store, made as this release's `async_setup` makes it."""
    from homeassistant.components import mobile_app

    cls = getattr(mobile_app, "_MobileAppStore", None)
    if cls is None:  # before 2026.7
        return mobile_app.Store(hass, mobile_app.STORAGE_VERSION, mobile_app.STORAGE_KEY)
    return cls(
        hass,
        mobile_app.STORAGE_VERSION,
        mobile_app.STORAGE_KEY,
        minor_version=mobile_app.STORAGE_VERSION_MINOR,
    )


def add_entry(
    hass, domain, title, data, *, unique_id=None, source="user", options=None, subentries=None
):
    """A config entry as this release's `ConfigEntry` stores it, without
    setting the integration up (nothing reaches the network)."""
    from types import MappingProxyType

    from homeassistant import config_entries

    entry = config_entries.ConfigEntry(
        data=data,
        discovery_keys=MappingProxyType({}),
        domain=domain,
        minor_version=1,
        options=options or {},
        source=source,
        subentries_data=subentries,
        title=title,
        unique_id=unique_id,
        version=1,
    )
    hass.config_entries._entries[entry.entry_id] = entry
    hass.config_entries._async_schedule_save()
    return entry


async def seed(config_dir):
    """The install, made by 2026.6.1."""
    from homeassistant import config as conf_util
    from homeassistant.auth.const import GROUP_ID_ADMIN
    from homeassistant.components import onboarding, person, zone
    from homeassistant.components.analytics.analytics import Analytics
    from homeassistant.components.frontend import storage as frontend_storage
    from homeassistant.components.labs.helpers import async_update_preview_feature
    from homeassistant.components.lovelace import dashboard
    from homeassistant.const import EntityCategory
    from homeassistant.core import State
    from homeassistant.helpers import (
        area_registry as ar,
        category_registry as cr,
        device_registry as dr,
        entity_registry as er,
        floor_registry as fr,
        label_registry as lr,
        restore_state,
    )
    from homeassistant.setup import async_setup_component
    from homeassistant.util import dt as dt_util

    await conf_util.async_ensure_config_exists(await boot_bare(config_dir))
    (config_dir / "automations.yaml").write_text(AUTOMATIONS)
    (config_dir / ".HA_VERSION").write_text(ha_version())
    hass, loads = await boot(config_dir)
    assert loads

    # The owner, as onboarding's user step makes them, with a session.
    provider = hass.auth.auth_providers[0]
    await provider.async_initialize()
    await hass.async_add_executor_job(provider.data.add_auth, "owner", PASSWORD)
    await provider.data.async_save()
    credentials = await provider.async_get_or_create_credentials({"username": "owner"})
    owner = await hass.auth.async_create_user("Test Owner", group_ids=[GROUP_ID_ADMIN])
    await hass.auth.async_link_user(owner, credentials)
    await hass.auth.async_create_refresh_token(owner, CLIENT, credential=credentials)
    await onboarding.OnboardingStorage(
        hass, onboarding.STORAGE_VERSION, onboarding.STORAGE_KEY, private=True
    ).async_save({"done": ["user", "core_config", "analytics", "integration"]})

    await hass.config.async_update(
        latitude=52.37,
        longitude=4.89,
        elevation=3,
        unit_system="metric",
        location_name="Home",
        time_zone="Europe/Amsterdam",
        currency="EUR",
        country="NL",
        language="en",
    )

    user_store = await frontend_storage.async_user_store(hass, owner.id)
    await user_store.async_set_item("core", {"default_panel": "lovelace"})
    system_store = await frontend_storage.async_system_store(hass)
    await system_store.async_set_item("home", {"favorite_entities": ["light.hall"]})

    # Config entries shaped as each integration's 2026.6.1 config flow
    # makes them (`components/<domain>/config_flow.py`), with made-up ids.
    entries = {
        "sun": add_entry(hass, "sun", "Sun", {}, source="import"),
        "met": add_entry(hass, "met", "Home", {"track_home": True}, source="onboarding"),
        "met_fixed": add_entry(
            hass,
            "met",
            "Cabin",
            {"name": "Cabin", "latitude": 60.39, "longitude": 5.32, "elevation": 12},
        ),
        "hue": add_entry(
            hass,
            "hue",
            "Hue Bridge",
            {"host": "192.0.2.10", "api_key": "synthetic-hue-key", "api_version": 2},
            unique_id="001788fffe000001",
            options={"allow_hue_groups": False},
        ),
        "switchbot_cloud": add_entry(
            hass,
            "switchbot_cloud",
            "SwitchBot Cloud",
            {"api_token": "synthetic-switchbot-token", "api_key": "synthetic-switchbot-key"},
            unique_id="synthetic-switchbot-token",
        ),
        # One entry per bulb, its unique id the bulb's serial written as a
        # MAC address (`formatted_serial`) ...
        "lifx": add_entry(
            hass, "lifx", LIFX_LABEL, {"host": "192.0.2.21"}, unique_id=LIFX_SERIAL
        ),
        # ... and the shared entry from before that, which 2026.10 removes.
        "lifx_legacy": add_entry(hass, "lifx", "LIFX", {}, unique_id="lifx"),
        "mobile_app": add_entry(
            hass,
            "mobile_app",
            "Test Phone",
            {
                "app_data": {},
                "app_id": "io.robbie.HomeAssistant",
                "app_name": "Home Assistant",
                "app_version": "2026.9.0",
                "device_name": "Test Phone",
                "manufacturer": "Apple",
                "model": "iPhone17,1",
                "os_name": "iOS",
                "os_version": "27.0",
                "supports_encryption": True,
                "device_id": "synthetic-phone",
                "user_id": owner.id,
                "cloudhook_url": None,
                "remote_ui_url": None,
                "secret": hashlib.sha256(b"synthetic secret").hexdigest(),
                "webhook_id": WEBHOOK_ID,
            },
            unique_id="synthetic-phone",
            source="registration",
        ),
        # Three integrations hars doesn't run: one that shares a device
        # with `lifx`, one whose devices belong to a subentry.
        "radio_browser": add_entry(hass, "radio_browser", "Radio Browser", {}, source="onboarding"),
        "homekit_controller": add_entry(
            hass,
            "homekit_controller",
            "Desk",
            {"AccessoryPairingID": "00:00:00:00:00:01"},
            unique_id="00:00:00:00:00:01",
        ),
        "mqtt": add_entry(
            hass,
            "mqtt",
            "192.0.2.30",
            {"broker": "192.0.2.30", "port": 1883},
            subentries=[
                {
                    "data": {"device": {"name": "Power strip"}},
                    "subentry_type": "device",
                    "title": "Power strip",
                    "unique_id": None,
                }
            ],
        ),
    }
    (strip_subentry,) = entries["mqtt"].subentries

    floors, areas, labels = fr.async_get(hass), ar.async_get(hass), lr.async_get(hass)
    ground = floors.async_create("Ground floor", level=0)
    hall = areas.async_create("Hall", floor_id=ground.floor_id)
    study = areas.async_create("Study")
    lights = labels.async_create("Lights", color="amber")
    category = cr.async_get(hass).async_create(name="Lighting", scope="automation")

    devices, entities = dr.async_get(hass), er.async_get(hass)
    hue, lifx, homekit = entries["hue"], entries["lifx"], entries["homekit_controller"]

    # `hue` (`components/hue/v2/device.py`): the bridge with both its ids,
    # each device linked through it, the Zigbee address as a `mac`
    # connection (2026.10 moves those).
    bridge = devices.async_get_or_create(
        config_entry_id=hue.entry_id,
        identifiers={("hue", HUE_BRIDGE), ("hue", "001788fffe000001")},
        connections={("mac", "00:17:88:01:00:00:00:01")},
        manufacturer="Signify Netherlands B.V.",
        model="Hue Bridge",
        model_id="BSB002",
        name="Hue Bridge",
        sw_version="1.70.0",
    )
    hall_light = devices.async_get_or_create(
        config_entry_id=hue.entry_id,
        identifiers={("hue", HUE_HALL)},
        connections={("mac", "00:17:88:01:00:00:00:02")},
        manufacturer="Signify Netherlands B.V.",
        model="Hue color lamp",
        model_id="LCA001",
        name="Hall",
        sw_version="1.104.2",
        via_device=("hue", HUE_BRIDGE),
    )
    devices.async_update_device(hall_light.id, area_id=hall.id)
    # A device the user renamed and disabled.
    porch = devices.async_get_or_create(
        config_entry_id=hue.entry_id,
        identifiers={("hue", HUE_PORCH)},
        connections={("mac", "00:17:88:01:00:00:00:03")},
        manufacturer="Signify Netherlands B.V.",
        model="Hue white lamp",
        model_id="LWA001",
        name="Porch",
        via_device=("hue", HUE_BRIDGE),
    )
    devices.async_update_device(
        porch.id,
        name_by_user="Porch lantern",
        labels={lights.label_id},
        disabled_by=dr.DeviceEntryDisabler.USER,
    )
    # A device that was removed.
    gone = devices.async_get_or_create(
        config_entry_id=hue.entry_id,
        identifiers={("hue", HUE_GONE)},
        manufacturer="Signify Netherlands B.V.",
        name="Old dimmer",
        via_device=("hue", HUE_BRIDGE),
    )
    devices.async_remove_device(gone.id)

    # One bulb known to two integrations by its MAC address: before
    # 2026.8 that is one device with two config entries
    # (`components/lifx/entity.py`: the serial as identifier, the MAC as
    # connection).
    desk = devices.async_get_or_create(
        config_entry_id=lifx.entry_id,
        identifiers={("lifx", LIFX_SERIAL)},
        connections={("mac", LIFX_SERIAL)},
        manufacturer="LIFX",
        model="LIFX A19",
        name=LIFX_LABEL,
        sw_version="3.70",
    )
    devices.async_get_or_create(
        config_entry_id=homekit.entry_id,
        identifiers={("homekit_controller:accessory-id", "00:00:00:00:00:01:aid:1")},
        connections={("mac", LIFX_SERIAL)},
    )
    devices.async_update_device(desk.id, area_id=study.id, name_by_user="Desk lamp")
    # A device reached through that shared one (2026.8 moves the link to
    # one of the two devices it becomes).
    desk_sensor = devices.async_get_or_create(
        config_entry_id=homekit.entry_id,
        identifiers={("homekit_controller:accessory-id", "00:00:00:00:00:01:aid:2")},
        manufacturer="LIFX",
        name="Desk sensor",
    )
    devices.async_update_device(desk_sensor.id, via_device_id=desk.id)
    assert len({bridge.id, hall_light.id, porch.id, desk.id, desk_sensor.id}) == 5
    with (config_dir / "automations.yaml").open("a") as f:
        f.write(AUTOMATION_DEVICE.format(device_id=desk.id))
    assert len(devices.async_get(desk.id).config_entries) == 2

    # `switchbot_cloud` (`components/switchbot_cloud/entity.py`).
    towel = devices.async_get_or_create(
        config_entry_id=entries["switchbot_cloud"].entry_id,
        identifiers={("switchbot_cloud", SWITCHBOT_BOT)},
        manufacturer="SwitchBot",
        model="Bot",
        name="Towel rail",
        sw_version="V6.6",
    )
    # `mobile_app` (`components/mobile_app/__init__.py`).
    phone = devices.async_get_or_create(
        config_entry_id=entries["mobile_app"].entry_id,
        identifiers={("mobile_app", "synthetic-phone")},
        manufacturer="Apple",
        model="iPhone17,1",
        name="Test Phone",
        sw_version="27.0",
    )
    # `met` and `sun`: one service device per entry.
    forecast = {
        key: devices.async_get_or_create(
            config_entry_id=entries[key].entry_id,
            identifiers={("met", entries[key].entry_id)},
            entry_type=dr.DeviceEntryType.SERVICE,
            manufacturer="Met.no",
            model="Forecast",
            name="Forecast",
            configuration_url="https://www.met.no/en",
        )
        for key in ("met", "met_fixed")
    }
    sun_device = devices.async_get_or_create(
        config_entry_id=entries["sun"].entry_id,
        identifiers={("sun", entries["sun"].entry_id)},
        entry_type=dr.DeviceEntryType.SERVICE,
        name="Sun",
    )
    # A device of a subentry, and one reached through it (2026.9 lets an
    # integration make that one a child device).
    strip = devices.async_get_or_create(
        config_entry_id=entries["mqtt"].entry_id,
        config_subentry_id=strip_subentry,
        identifiers={("mqtt", "synthetic-strip")},
        manufacturer="Example",
        model="Strip 4",
        name="Power strip",
    )
    socket_ = devices.async_get_or_create(
        config_entry_id=entries["mqtt"].entry_id,
        config_subentry_id=strip_subentry,
        identifiers={("mqtt", "synthetic-strip-socket-1")},
        manufacturer="Example",
        model="Socket",
        name="Socket 1",
        via_device=("mqtt", "synthetic-strip"),
    )

    def entity(domain, platform, unique_id, entry, device, object_id, **kwargs):
        kwargs.setdefault("has_entity_name", True)
        kwargs.setdefault("original_name", None)
        return entities.async_get_or_create(
            domain,
            platform,
            unique_id,
            config_entry=entry,
            device_id=device.id if device else None,
            suggested_object_id=object_id,
            **kwargs,
        )

    exposed = lambda: {"conversation": {"should_expose": True}}  # noqa: E731

    entity(
        "light", "hue", "00000000-0000-4000-8000-000000000201", hue, hall_light, "hall",
        capabilities={"supported_color_modes": ["color_temp", "xy"]},
    )
    entities.async_update_entity("light.hall", labels={lights.label_id})
    # The user's name without the device's name in it, exposed to Assist:
    # 2026.11 shows it with the device's name in front and keeps the old
    # one (`compat_name`, and as the first alias).
    entity(
        "sensor", "hue", "00000000-0000-4000-8000-000000000202", hue, hall_light,
        "hall_temperature", original_name="Temperature", original_device_class="temperature",
        unit_of_measurement="°C", capabilities={"state_class": "measurement"},
        get_initial_options=exposed,
    )
    entities.async_update_entity("sensor.hall_temperature", name="Warmth downstairs")
    entities.async_update_entity_options(
        "sensor.hall_temperature", "sensor", {"display_precision": 1}
    )
    # The user's name with the device's name in it, on an entity with an
    # area of its own (2026.11 leaves such a name alone).
    entity(
        "binary_sensor", "hue", "00000000-0000-4000-8000-000000000203", hue, hall_light,
        "hall_motion", original_name="Motion", original_device_class="motion",
    )
    entities.async_update_entity(
        "binary_sensor.hall_motion", name="Hall movement", area_id=study.id
    )
    # Named the old way (`has_entity_name` false), the device's name in
    # front; hidden by the user.
    entity(
        "sensor", "hue", "00000000-0000-4000-8000-000000000204", hue, hall_light,
        "hall_illuminance", has_entity_name=False, original_name="Hall Illuminance",
        original_device_class="illuminance", unit_of_measurement="lx",
        hidden_by=er.RegistryEntryHider.USER,
    )
    # Entities of the disabled device: one follows it, one the user
    # disabled.
    entity(
        "light", "hue", "00000000-0000-4000-8000-000000000205", hue, porch, "porch",
        disabled_by=er.RegistryEntryDisabler.DEVICE,
    )
    entity(
        "sensor", "hue", "00000000-0000-4000-8000-000000000206", hue, porch,
        "porch_signal", original_name="Signal", entity_category=EntityCategory.DIAGNOSTIC,
        disabled_by=er.RegistryEntryDisabler.USER,
    )

    # `lifx` (`components/lifx/light.py`, `button.py`): unique ids made
    # from the serial as the device registry formats a MAC address. The
    # user's name starts with the device's name: 2026.11 stores it
    # without.
    entity(
        "light", "lifx", LIFX_SERIAL, lifx, desk, "desk",
        capabilities={"supported_color_modes": ["color_temp", "hs"]},
        supported_features=36, get_initial_options=exposed,
    )
    entities.async_update_entity("light.desk", name="Desk lamp reading light")
    entity(
        "button", "lifx", f"{LIFX_SERIAL}_restart", lifx, desk, "desk_restart",
        original_name="Restart", original_device_class="restart",
        entity_category=EntityCategory.CONFIG,
    )
    # The other integration's entity on the shared device.
    entity(
        "light", "homekit_controller", "00:00:00:00:00:01_1_2", homekit, desk, "desk_homekit",
    )
    entity(
        "sensor", "homekit_controller", "00:00:00:00:00:01_2_1", homekit, desk_sensor,
        "desk_sensor_temperature", original_name="Temperature",
    )

    # `switchbot_cloud`: the user's name is the device's name, exactly.
    entity("switch", "switchbot_cloud", SWITCHBOT_BOT, entries["switchbot_cloud"], towel, "towel_rail")
    entities.async_update_entity("switch.towel_rail", name="Towel rail")
    entity(
        "sensor", "switchbot_cloud", f"{SWITCHBOT_BOT}_battery", entries["switchbot_cloud"],
        towel, "towel_rail_battery", original_name="Battery", original_device_class="battery",
        unit_of_measurement="%", capabilities={"state_class": "measurement"},
    )

    # `mobile_app` (`device_tracker.py`, `sensor.py`).
    entity(
        "device_tracker", "mobile_app", "synthetic-phone", entries["mobile_app"], phone,
        "test_phone",
    )
    entity(
        "sensor", "mobile_app", f"{WEBHOOK_ID}_battery_level", entries["mobile_app"], phone,
        "test_phone_battery_level", original_name="Battery Level",
        original_device_class="battery", unit_of_measurement="%",
    )

    entity(
        "sensor", "sun", f"{entries['sun'].entry_id}-next_dawn", entries["sun"], sun_device,
        "sun_next_dawn", original_name="Next dawn",
        disabled_by=er.RegistryEntryDisabler.INTEGRATION,
    )
    # `met` (`components/met/weather.py`): the entity is named after the
    # place, its device "Forecast".
    entity(
        "weather", "met", "home", entries["met"], forecast["met"], "forecast_home",
        original_name="Home",
    )
    entity(
        "weather", "met", "60.39-5.32", entries["met_fixed"], forecast["met_fixed"],
        "forecast_cabin", original_name="Cabin",
    )

    entities.async_get_or_create(
        "switch", "mqtt", "synthetic-strip-socket-1", config_entry=entries["mqtt"],
        config_subentry_id=strip_subentry, device_id=socket_.id, has_entity_name=True,
        original_name=None, suggested_object_id="power_strip_socket_1",
    )
    assert strip.config_entries_subentries == {entries["mqtt"].entry_id: {strip_subentry}}

    removed = entity(
        "sensor", "hue", "00000000-0000-4000-8000-000000000301", hue, hall_light,
        "hall_old_sensor", original_name="Old sensor",
    )
    entities.async_remove(removed.entity_id)
    for uid, alias in (
        ("1718000000001", "Hall light at dusk"),
        ("1718000000002", "Wait then off"),
        ("1718000000004", "Desk off at night"),
    ):
        row = entities.async_get_or_create(
            "automation", "automation", uid, original_name=alias, suggested_object_id=alias,
            capabilities={"id": uid},
        )
        if uid.endswith("1"):
            entities.async_update_entity(
                row.entity_id, categories={"automation": category.category_id}
            )

    assert await async_setup_component(hass, "person", {})
    assert await async_setup_component(hass, "zone", {})
    await hass.async_block_till_done()
    (people,) = instances(person.PersonStorageCollection)
    await people.async_create_item(
        {"name": "Test Owner", "user_id": owner.id, "device_trackers": []}
    )
    (zones,) = instances(zone.ZoneStorageCollection)
    await zones.async_create_item(
        {"name": "Office", "latitude": 52.34, "longitude": 4.87, "radius": 150, "icon": "mdi:briefcase"}
    )

    dashboards = dashboard.DashboardsCollection(hass)
    await dashboards.async_load()
    item = await dashboards.async_create_item(
        {"url_path": "synthetic-map", "title": "Map", "icon": "mdi:map", "require_admin": False,
         "show_in_sidebar": True, "mode": "storage"}
    )
    await dashboard.LovelaceStorage(hass, item).async_save({"strategy": {"type": "map"}})
    await dashboard.LovelaceStorage(hass, None).async_save(
        {"views": [{"title": "Home", "cards": [{"type": "entities", "entities": ["light.hall"]}]}]}
    )

    # Last states: of an entity with no registry row, and of two with one
    # (2026.11 keys those by the row's id, not the entity id).
    data = restore_state.async_get(hass)
    data.last_states["scene.evening"] = restore_state.StoredState(
        State("scene.evening", "2026-06-10T19:00:00+00:00"), None, dt_util.utcnow()
    )
    data.last_states["switch.towel_rail"] = restore_state.StoredState(
        State("switch.towel_rail", "on", {"friendly_name": "Towel rail"}), None, dt_util.utcnow()
    )
    data.last_states["light.desk"] = restore_state.StoredState(
        State("light.desk", "off"), None, dt_util.utcnow()
    )
    await data.async_dump_states()

    await mobile_app_store(hass).async_save(
        {"deleted_ids": [hashlib.sha256(b"a deleted phone").hexdigest()]}
    )

    # A Labs preview switched on (2026.7 makes it a plain feature and
    # drops the entry).
    assert await async_setup_component(hass, "labs", {})
    await async_update_preview_feature(hass, "automation", "new_triggers_conditions", True)

    # Onboarding's analytics step, with nothing shared (so nothing is ever
    # sent).
    analytics = Analytics(hass)
    await analytics.load()
    await analytics.save_preferences({"base": False, "diagnostics": False})

    await stop(hass)
    return {}


async def boot_bare(config_dir):
    """A `HomeAssistant` that has loaded nothing, to write the default YAML."""
    from homeassistant import core

    return core.HomeAssistant(str(config_dir))


async def upgrade(config_dir):
    """Start this release on the install and change every store through it."""
    from homeassistant import loader
    from homeassistant.components import person, zone
    from homeassistant.components.analytics.analytics import Analytics
    from homeassistant.components.frontend import storage as frontend_storage
    from homeassistant.components.lovelace import dashboard
    from homeassistant.components.mobile_app import const as mobile_app_const
    from homeassistant.helpers import (
        area_registry as ar,
        category_registry as cr,
        device_registry as dr,
        entity_registry as er,
        floor_registry as fr,
        label_registry as lr,
        restore_state,
    )
    from homeassistant.setup import async_setup_component

    version = ha_version()
    (config_dir / ".HA_VERSION").write_text(version)
    month = int(version.split(".")[1])
    if month >= 7:
        with (config_dir / "automations.yaml").open("a") as f:
            f.write(AUTOMATION_NEW_TRIGGER)
    refresh_restore_state(config_dir)
    hass, loads = await boot(config_dir)
    assert loads
    notes = {}

    # The config entries of the integrations hars runs, migrated as this
    # release migrates them when it sets them up.
    migrated = {}
    for entry in list(hass.config_entries.async_entries()):
        if entry.domain not in RUNS:
            continue
        before = (entry.version, entry.minor_version)
        try:
            # The flow handler holds the version this release is at.
            integration = await loader.async_get_integration(hass, entry.domain)
            await integration.async_get_platform("config_flow")
            ok = await entry.async_migrate(hass)
        except ImportError as err:
            ok = f"not run: {err}"
        await hass.async_block_till_done()
        after = (entry.version, entry.minor_version)
        if ok is not True or before != after:
            migrated[f"{entry.domain} ({entry.title})"] = {
                "from": list(before), "to": list(after), "result": ok,
            }
    notes["config_entries_migrated"] = migrated
    first = hass.config_entries.async_entries("sun")[0]
    hass.config_entries.async_update_entry(first, title=f"Sun ({version})")

    owner = next(u for u in await hass.auth.async_get_users() if u.is_owner)
    await hass.auth.async_create_refresh_token(
        owner, f"http://127.0.0.1:8123/{version}", credential=owner.credentials[0]
    )
    await hass.config.async_update(elevation=month)

    user_store = await frontend_storage.async_user_store(hass, owner.id)
    await user_store.async_set_item("written_by", version)
    system_store = await frontend_storage.async_system_store(hass)
    await system_store.async_set_item("written_by", version)

    fr.async_get(hass).async_create(f"Floor {version}", level=month)
    ar.async_get(hass).async_create(f"Area {version}")
    lr.async_get(hass).async_create(f"Label {version}")
    cr.async_get(hass).async_create(name=f"Category {version}", scope="automation")
    entities, devices = er.async_get(hass), dr.async_get(hass)
    entities.async_update_entity("light.hall", icon="mdi:ceiling-light")
    hue = hass.config_entries.async_entries("hue")[0]
    # A device renamed by the user: one with no entity the user named
    # (2026.11 forgets an entity's `compat_name` when its device's name
    # changes).
    bridge = next(
        d
        for d in dr.async_entries_for_config_entry(devices, hue.entry_id)
        if d.name == "Hue Bridge"
    )
    devices.async_update_device(bridge.id, name_by_user=f"Bridge ({version})")
    # An entity id changed after its last state was stored.
    entities.async_update_entity("switch.towel_rail", new_entity_id="switch.towel_warmer")
    notes["registered_as_setup_does"] = register_as_setup_does(hass, month)
    if month >= 7:
        entities.async_get_or_create(
            "automation", "automation", "1720000000003",
            original_name="When the hall light turns on",
            suggested_object_id="When the hall light turns on",
            capabilities={"id": "1720000000003"},
        )

    assert await async_setup_component(hass, "person", {})
    assert await async_setup_component(hass, "zone", {})
    await hass.async_block_till_done()
    (zones,) = instances(zone.ZoneStorageCollection)
    await zones.async_create_item(
        {"name": f"Zone {version}", "latitude": 52.36, "longitude": 4.9, "radius": 100}
    )
    (people,) = instances(person.PersonStorageCollection)
    await people.async_create_item({"name": f"Person {version}", "device_trackers": []})

    await dashboard.LovelaceStorage(hass, None).async_save(
        {"views": [{"title": f"Home {version}", "cards": []}]}
    )

    await restore_state.async_get(hass).async_dump_states()

    analytics = Analytics(hass)
    await analytics.load()
    await analytics.save_preferences({"diagnostics": month % 2 == 0})

    # `labs.async_setup` loads `core.labs` and drops (and saves without)
    # the previews this release no longer has.
    assert await async_setup_component(hass, "labs", {})

    # `mobile_app.async_setup`: load the store (migrating it) ...
    store = mobile_app_store(hass)
    app_config = await store.async_load()
    tokens = getattr(mobile_app_const, "DATA_LIVE_ACTIVITY_TOKENS", None)
    if tokens is None:
        await store.async_save(app_config)
    else:
        # ... and, from 2026.7, a Live Activity's push token, stored as
        # the app's `update_live_activity_token` webhook stores it.
        from homeassistant.components.mobile_app.helpers import savable_state
        from homeassistant.components.mobile_app.live_activity.store import (
            store_live_activity_token,
        )

        hass.data[mobile_app_const.DOMAIN] = {
            mobile_app_const.DATA_DELETED_IDS: app_config[mobile_app_const.DATA_DELETED_IDS],
            tokens: app_config[tokens],
            mobile_app_const.DATA_LIVE_ACTIVITY_CLEANUP_CANCEL: None,
            mobile_app_const.DATA_STORE: store,
        }
        (phone,) = hass.config_entries.async_entries("mobile_app")
        store_live_activity_token(
            hass, phone.data["webhook_id"], "synthetic-activity", "synthetic-push-token",
            float(FAR_FUTURE),
        )
        await store.async_save(savable_state(hass))

    await stop(hass)
    # The scene's last state, which nothing claimed, is still there.
    restored = json.loads((config_dir / ".storage/core.restore_state").read_text())
    assert "scene.evening" in [row["state"]["entity_id"] for row in restored["data"]]
    return notes


def register_as_setup_does(hass, month):
    """The device and entity registry calls three integrations make when
    this release sets them up, made here without the integrations (they
    would need a bulb, a bridge and the forecast service).

    The registries' code is the release's own; which call each integration
    makes, and with what, is read from the release's source and written
    out here, so this is the one part of a fixture that follows a reading
    of Home Assistant's code rather than running it:

    - `lifx` (`components/lifx/entity.py`, every release): the bulb's
      device, by its serial as the entry's unique id has it and its MAC
      address. From 2026.8 this is the "first re-registration" that
      replaces the identifiers a split device copied from the shared one.
      The other integration's half is not registered again, so it keeps
      its copies.
    - `hue` (`components/hue/v2/device.py`, from 2026.10): a device's
      Zigbee address as a `zigbee` connection.
    - `met` (`components/met/weather.py`, from 2026.11): the device named
      after the place (the entry's title, or the home's name) and the
      entity without a name of its own.
    - a device made a child of the one it was reached through
      (`async_get_or_create_child`, from 2026.9), as `mqtt` and others do.
    """
    from homeassistant.helpers import device_registry as dr, entity_registry as er

    devices, entities = dr.async_get(hass), er.async_get(hass)
    done = []
    (lifx,) = [e for e in hass.config_entries.async_entries("lifx") if e.unique_id != "lifx"]
    devices.async_get_or_create(
        config_entry_id=lifx.entry_id,
        identifiers={("lifx", lifx.unique_id)},
        connections={("mac", LIFX_SERIAL)},
        manufacturer="LIFX",
        model="LIFX A19",
        name=LIFX_LABEL,
        sw_version="3.70",
    )
    done.append("lifx")
    if month >= 10:
        (hue,) = hass.config_entries.async_entries("hue")
        for hue_id, address in ((HUE_BRIDGE, "01"), (HUE_HALL, "02")):
            devices.async_get_or_create(
                config_entry_id=hue.entry_id,
                identifiers={("hue", hue_id)},
                connections={("zigbee", f"00:17:88:01:00:00:00:{address}")},
            )
        done.append("hue")
    if month >= 11:
        for entry in hass.config_entries.async_entries("met"):
            track_home = entry.data.get("track_home", False)
            device = devices.async_get_or_create(
                config_entry_id=entry.entry_id,
                identifiers={("met", entry.entry_id)},
                entry_type=dr.DeviceEntryType.SERVICE,
                manufacturer="Met.no",
                model="Forecast",
                name=hass.config.location_name if track_home else entry.title,
                configuration_url="https://www.met.no/en",
            )
            location = entry.data.get("location", {})
            entities.async_get_or_create(
                "weather",
                "met",
                "home" if track_home else f"{location['latitude']}-{location['longitude']}",
                config_entry=entry,
                device_id=device.id,
                has_entity_name=True,
                original_name=None,
            )
        done.append("met")
    if hasattr(devices, "async_get_or_create_child"):
        (mqtt,) = hass.config_entries.async_entries("mqtt")
        (subentry_id,) = mqtt.subentries
        strip = next(
            d
            for d in dr.async_entries_for_config_entry(devices, mqtt.entry_id)
            if d.name == "Power strip"
        )
        devices.async_get_or_create_child(
            config_entry_id=mqtt.entry_id,
            config_subentry_id=subentry_id,
            identifiers={("mqtt", "synthetic-strip-socket-1")},
            name="Socket 1",
            parent_device_id=strip.id,
        )
        done.append("child device")
    return done


async def downgrade():
    """What 2026.6.1 itself does with each release's directory."""
    import logging

    from homeassistant import bootstrap
    from homeassistant.components import mobile_app
    from homeassistant.components.mobile_app.helpers import savable_state

    class Errors(logging.Handler):
        def __init__(self):
            super().__init__(logging.ERROR)
            self.lines = []

        def emit(self, record):
            self.lines.append(record.getMessage())

    def version_key(path):
        return [int(part) for part in path.stem.split(".")]

    cases, stores = [], []
    for path in sorted(OUT.glob("2026.*.json"), key=version_key):
        fixture = json.loads(path.read_text())
        case = {"from": fixture["ha_version"]}
        with tempfile.TemporaryDirectory() as tmp:
            config_dir = pathlib.Path(tmp)
            write_dir(config_dir, fixture["files"])
            refresh_restore_state(config_dir)
            # `bootstrap.async_load_base_functionality`: the registries,
            # the config entries and the restore state. It answers False,
            # and Home Assistant starts in recovery mode with the files
            # left alone, when a store is of a newer major version.
            errors = Errors()
            logging.getLogger(bootstrap.__name__).addHandler(errors)
            hass, loads = await boot(config_dir)
            logging.getLogger(bootstrap.__name__).removeHandler(errors)
            case["loads"] = loads
            case["errors"] = [line.replace(str(config_dir), "<config>") for line in errors.lines]
            text = fixture["files"].get(".storage/mobile_app")
            if loads and text and json.loads(text).get("minor_version", 1) > 1:
                # `mobile_app.async_setup`: load the store ...
                store = mobile_app.Store(hass, mobile_app.STORAGE_VERSION, mobile_app.STORAGE_KEY)
                app_config = await store.async_load()
                await hass.async_block_till_done()
                after_load = (config_dir / ".storage/mobile_app").read_text()
                hass.data[mobile_app.DOMAIN] = {
                    mobile_app.DATA_DELETED_IDS: app_config.get(mobile_app.DATA_DELETED_IDS, []),
                }
                # ... and `async_remove_entry`: the registration's webhook
                # id joins the deleted ones, saved now.
                (phone,) = hass.config_entries.async_entries(mobile_app.DOMAIN)
                webhook_id = phone.data["webhook_id"]
                hass.data[mobile_app.DOMAIN][mobile_app.DATA_DELETED_IDS].append(webhook_id)
                await store.async_save(savable_state(hass))
                stores.append(
                    {
                        "from": fixture["ha_version"],
                        "input": text,
                        "after_load": after_load,
                        "removed_webhook_id": webhook_id,
                        "after_change": (config_dir / ".storage/mobile_app").read_text(),
                    }
                )
            await stop(hass)
        cases.append(case)
    return cases, stores


async def loads(directory):
    """Whether 2026.6.1 loads a directory `hars import` wrote, and what its
    own writers make of the registries hars converted: each file after
    2026.6.1 loaded it and saved it again, compared with what hars wrote
    (as data: Home Assistant lists a set in any order).
    """
    import logging
    import shutil

    from homeassistant import bootstrap
    from homeassistant.helpers import device_registry as dr, entity_registry as er

    class Errors(logging.Handler):
        def __init__(self):
            super().__init__(logging.ERROR)
            self.lines = []

        def emit(self, record):
            self.lines.append(record.getMessage())

    def canonical(value):
        if isinstance(value, dict):
            return {k: canonical(v) for k, v in value.items() if k != "modified_at"}
        if isinstance(value, list):
            return sorted((canonical(v) for v in value), key=json.dumps)
        return value

    stores = (".storage/core.device_registry", ".storage/core.entity_registry")
    with tempfile.TemporaryDirectory() as tmp:
        config_dir = pathlib.Path(tmp) / "config"
        shutil.copytree(directory, config_dir)
        before = {s: json.loads((config_dir / s).read_text()) for s in stores}
        refresh_restore_state(config_dir)
        errors = Errors()
        logging.getLogger().addHandler(errors)
        hass, ok = await boot(config_dir)
        result = {"loads": ok}
        if ok:
            dr.async_get(hass).async_schedule_save()
            er.async_get(hass).async_schedule_save()
            await stop(hass)
            for s in stores:
                after = json.loads((config_dir / s).read_text())
                if canonical(after) == canonical(before[s]):
                    result[s] = "as hars wrote it"
                    continue
                # Fields 2026.6.1 doesn't know, which it leaves out when it
                # writes a row: are they the only difference?
                unknown = set()

                def strip(value, known):
                    if isinstance(value, dict) and isinstance(known, dict):
                        unknown.update(set(value) - set(known))
                        return {k: strip(v, known[k]) for k, v in value.items() if k in known}
                    if isinstance(value, list) and isinstance(known, list):
                        if len(value) == len(known):
                            return [strip(v, k) for v, k in zip(value, known)]
                    return value

                same = canonical(strip(before[s], after)) == canonical(after)
                result[s] = {
                    "left_out_by_2026.6.1": sorted(unknown),
                    "otherwise_as_hars_wrote_it": same,
                }
        logging.getLogger().removeHandler(errors)
        result["errors"] = [line.replace(str(config_dir), "<config>") for line in errors.lines]
    return result


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else ""
    version = ha_version()
    if mode == "loads" and version == SEED and len(sys.argv) == 3:
        print(json.dumps(asyncio.run(loads(pathlib.Path(sys.argv[2]))), indent=1))
        return
    if mode in ("seed", "downgrade") and version != SEED:
        sys.exit(f"homeassistant is {version}; `{mode}` runs under {SEED}")
    if mode == "upgrade" and version == SEED:
        sys.exit("`upgrade` runs under a release later than the seed's")
    if mode not in ("seed", "upgrade", "downgrade"):
        sys.exit(__doc__)

    import bcrypt

    with tempfile.TemporaryDirectory() as tmp, patch.object(
        bcrypt, "gensalt", lambda *a, **k: SALT
    ):
        config_dir = pathlib.Path(tmp) / "config"
        config_dir.mkdir()
        if mode == "downgrade":
            cases, stores = asyncio.run(downgrade())
            save_fixture("downgrade.json", {"ha_version": version, "cases": cases})
            MOBILE_APP_OUT.write_text(
                json.dumps({"ha_version": version, "cases": stores}, indent=1) + "\n"
            )
            return
        if mode == "seed":
            notes = asyncio.run(seed(config_dir))
            before = {}
        else:
            before = load_fixture(SEED)["files"]
            write_dir(config_dir, before)
            notes = asyncio.run(upgrade(config_dir))
        files = normalise(read_dir(config_dir), version)
        written_by = {
            path: version if before.get(path) != text else SEED
            for path, text in files.items()
        }
        save_fixture(
            f"{version}.json",
            {
                "ha_version": version,
                "notes": notes,
                "written_by": written_by,
                "files": files,
            },
        )


if __name__ == "__main__":
    main()
    # Everything is written. Leave without tearing the interpreter down:
    # with Home Assistant's compiled libraries loaded, CPython 3.14's
    # module finalisation can crash (seen under 2026.6.4 to 2026.8.3).
    sys.stdout.flush()
    sys.stderr.flush()
    os._exit(0)

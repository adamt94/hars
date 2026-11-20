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
  Assistant 2026.6.1's own code: an owner with a session, a location, a
  person, a zone, a dashboard, the owner's frontend settings, config
  entries for the integrations hars runs (and two it doesn't), areas, a
  floor, a label, a category, devices (one shared by two config entries)
  and entities (one renamed by the user, one disabled, one deleted), the
  restore state, the analytics choices, a Labs preview switched on,
  `mobile_app`'s store and the default YAML with two automations.
- `<release>.json` (`upgrade`, run under each later release): that
  directory after the release started on it, as a user who updated Home
  Assistant has it. The release's own code loads every store (running its
  storage migrations), migrates the config entries of the integrations hars
  runs (`ConfigEntry.async_migrate`, which calls the integration's
  `async_migrate_entry`), and then changes something in each store through
  its own API so the release's writer produces the file. `written_by` in
  the fixture says which files the release wrote and which it left as
  2026.6.1 wrote them (Home Assistant only rewrites a store that changes).
- `downgrade.json` (`downgrade`, run under 2026.6.1): what Home Assistant
  2026.6.1 itself does with each of those directories. `loads`: whether
  its `bootstrap.async_load_base_functionality` (registries, config
  entries, restore state) accepts it, with the errors it logged when it
  doesn't; hars imports a directory exactly when this is true. This is
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

Each fixture is `{"ha_version", "notes", "written_by", "files": {path:
text}}` (`notes`: the config entries the release migrated, with their
versions before and after), the
files exactly as Home Assistant wrote them except for what changes from run
to run: ids, tokens and times are replaced by fixed synthetic ones
(`normalise` below), and the session's `expire_at` is far in the future so
the fixture doesn't expire. Passwords are hashed with a fixed salt. The
restore state's times are fixed too, so its rows are older than the seven
days Home Assistant keeps an unclaimed one: before a release starts on the
directory they are made current (`refresh_restore_state`).

Everything is synthetic: no real install is read, the coordinates are a
rounded city centre, the hosts are documentation addresses (192.0.2.0/24)
and the keys are made up. Nothing reaches the network: the only
integrations set up are `http` (on a free loopback port), `person`, `zone`
and `labs`; of the others only the storage and migration code runs. Run
it twice: the output must not change.

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
CONFIG_ENTRY_SET = re.compile(r'"config_entries":(\[[^\]]*\])')
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
        # A set in Home Assistant, listed in an order that changes from
        # run to run.
        text = CONFIG_ENTRY_SET.sub(
            lambda m: '"config_entries":'
            + json.dumps(sorted(json.loads(m.group(1))), separators=(",", ":")),
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


def add_entry(hass, domain, title, data, *, unique_id=None, source="user", options=None):
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
        subentries_data=None,
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

    entries = {
        "sun": add_entry(hass, "sun", "Sun", {}, source="import"),
        "met": add_entry(hass, "met", "Home", {"track_home": True}, source="onboarding"),
        "met_fixed": add_entry(
            hass,
            "met",
            "Cabin",
            {"name": "Cabin", "latitude": 60.39, "longitude": 5.32, "elevation": 12},
            unique_id="60.39-5.32",
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
        "lifx": add_entry(
            hass, "lifx", "Desk", {"host": "192.0.2.21"}, unique_id="d0:73:d5:00:00:01"
        ),
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
                "webhook_id": hashlib.sha256(b"synthetic webhook").hexdigest(),
            },
            unique_id="synthetic-phone",
            source="registration",
        ),
        # Two integrations hars doesn't run.
        "radio_browser": add_entry(hass, "radio_browser", "Radio Browser", {}, source="onboarding"),
        "homekit_controller": add_entry(
            hass,
            "homekit_controller",
            "Desk",
            {"AccessoryPairingID": "00:00:00:00:00:01"},
            unique_id="00:00:00:00:00:01",
        ),
    }

    floors, areas, labels = fr.async_get(hass), ar.async_get(hass), lr.async_get(hass)
    ground = floors.async_create("Ground floor", level=0)
    hall = areas.async_create("Hall", floor_id=ground.floor_id)
    study = areas.async_create("Study")
    lights = labels.async_create("Lights", color="amber")
    category = cr.async_get(hass).async_create(name="Lighting", scope="automation")

    devices, entities = dr.async_get(hass), er.async_get(hass)
    bridge = devices.async_get_or_create(
        config_entry_id=entries["hue"].entry_id,
        identifiers={("hue", "001788fffe000001")},
        manufacturer="Signify Netherlands B.V.",
        model="BSB002",
        name="Hue Bridge",
        sw_version="1.70.0",
    )
    hall_light = devices.async_get_or_create(
        config_entry_id=entries["hue"].entry_id,
        identifiers={("hue", "00000000-0000-4000-8000-000000000101")},
        manufacturer="Signify Netherlands B.V.",
        model="LCA001",
        name="Hall",
        via_device=("hue", "001788fffe000001"),
    )
    devices.async_update_device(hall_light.id, area_id=hall.id)
    # One bulb known to two integrations by its MAC address: before
    # 2026.8 that is one device with two config entries.
    desk = devices.async_get_or_create(
        config_entry_id=entries["lifx"].entry_id,
        connections={("mac", "d0:73:d5:00:00:01")},
        manufacturer="LIFX",
        model="LIFX A19",
        name="Desk",
    )
    devices.async_get_or_create(
        config_entry_id=entries["homekit_controller"].entry_id,
        connections={("mac", "d0:73:d5:00:00:01")},
    )
    devices.async_update_device(desk.id, area_id=study.id, name_by_user="Desk lamp")
    assert bridge.id != hall_light.id

    entities.async_get_or_create(
        "light",
        "hue",
        "00000000-0000-4000-8000-000000000201",
        config_entry=entries["hue"],
        device_id=hall_light.id,
        has_entity_name=True,
        original_name=None,
        suggested_object_id="hall",
        capabilities={"supported_color_modes": ["color_temp", "xy"]},
    )
    entities.async_update_entity("light.hall", labels={lights.label_id})
    # A light whose name the user set: 2026.11 stores such names differently.
    entities.async_get_or_create(
        "light",
        "lifx",
        "d073d5000001",
        config_entry=entries["lifx"],
        device_id=desk.id,
        has_entity_name=True,
        original_name=None,
        suggested_object_id="desk",
    )
    entities.async_update_entity("light.desk", name="Desk lamp reading light")
    entities.async_get_or_create(
        "sensor",
        "sun",
        f"{entries['sun'].entry_id}-next_dawn",
        config_entry=entries["sun"],
        has_entity_name=True,
        original_name="Next dawn",
        suggested_object_id="sun_next_dawn",
        disabled_by=er.RegistryEntryDisabler.INTEGRATION,
    )
    entities.async_get_or_create(
        "weather", "met", "home", config_entry=entries["met"], original_name="Home",
        suggested_object_id="home",
    )
    removed = entities.async_get_or_create(
        "sensor", "hue", "00000000-0000-4000-8000-000000000301",
        config_entry=entries["hue"], device_id=hall_light.id, original_name="Old sensor",
    )
    entities.async_remove(removed.entity_id)
    for uid, alias in (("1718000000001", "Hall light at dusk"), ("1718000000002", "Wait then off")):
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

    data = restore_state.async_get(hass)
    data.last_states["scene.evening"] = restore_state.StoredState(
        State("scene.evening", "2026-06-10T19:00:00+00:00"), None, dt_util.utcnow()
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
    hall = next(
        d for d in dr.async_entries_for_config_entry(devices, hue.entry_id) if d.name == "Hall"
    )
    devices.async_update_device(hall.id, name_by_user=f"Hall ({version})")
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


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else ""
    version = ha_version()
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

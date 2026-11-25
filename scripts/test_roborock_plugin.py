"""Unit and protocol tests for the out-of-process Roborock plugin.

Runs with stdlib Python only (no real network and no python-roborock install
required) as part of `scripts/check.sh`.
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
from enum import Enum
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
from typing import Callable
import unittest

ROOT = Path(__file__).resolve().parents[1]
PLUGIN_DIR = ROOT / "plugins" / "roborock"
sys.path.insert(0, str(PLUGIN_DIR))

import roborock_plugin as rp  # noqa: E402


class RoborockAccountDoesNotExist(Exception):
    pass


class RoborockInvalidCode(Exception):
    pass


class RoborockTooFrequentCodeRequests(Exception):
    pass


class RoborockInvalidCredentials(Exception):
    pass


class RoborockException(Exception):
    pass


class FakeFanMode(Enum):
    QUIET = "quiet"
    BALANCED = "balanced"
    TURBO = "turbo"
    MAX = "max"


class FakeStateCode(Enum):
    charging = 8
    cleaning = 5
    paused = 10
    returning_home = 6


class FakeDockType(Enum):
    auto_empty_dock_pure = 3


class FakeDockError(Enum):
    ok = 0


class FakeConsumableAttribute(Enum):
    MAIN_BRUSH_WORK_TIME = "main_brush_work_time"
    SIDE_BRUSH_WORK_TIME = "side_brush_work_time"
    FILTER_WORK_TIME = "filter_work_time"
    SENSOR_DIRTY_TIME = "sensor_dirty_time"


class FakeB01Props(Enum):
    STATUS = "status"
    WIND = "wind"
    QUANTITY = "quantity"
    CLEANING_TIME = "cleaning_time"


class FakeSCWindMapping(Enum):
    QUIET = (1, "quiet")
    BALANCED = (2, "balanced")
    TURBO = (3, "turbo")
    MAX = (4, "max")
    MAX_PLUS = (5, "max_plus")

    @property
    def code(self) -> int:
        return self.value[0]

    @property
    def label(self) -> str:
        return self.value[1]

    @classmethod
    def keys(cls) -> list[str]:
        return [m.label for m in cls]

    @classmethod
    def from_value(cls, val: str) -> "FakeSCWindMapping":
        for m in cls:
            if m.label == val:
                return m
        raise ValueError(f"invalid Q7 fan speed {val!r}")


class FakeQ7Status(Enum):
    CHARGING = 8
    SWEEP_MOPING = 5


class FakeQ10Status(Enum):
    CLEANING = 3
    CHARGING = 6


class FakeYXFanLevel(Enum):
    OFF = "off"
    QUIET = "quiet"
    BALANCED = "balanced"
    TURBO = "turbo"
    MAX = "max"
    MAX_PLUS = "max_plus"
    UNKNOWN = "unknown"

    @property
    def code(self) -> int:
        return list(FakeYXFanLevel).index(self)

    @classmethod
    def from_value(cls, val: str) -> "FakeYXFanLevel":
        for m in cls:
            if m.value == val:
                return m
        raise ValueError(f"invalid Q10 fan level {val!r}")


class FakeQ10DP(Enum):
    SEEK = (11, "dpSeek")
    EMPTY = (12, "dpEmpty")

    @property
    def code(self) -> int:
        return self.value[0]

    @classmethod
    def from_any_optional(cls, raw: str) -> "FakeQ10DP | None":
        for m in cls:
            if raw in (m.name, m.value[1], str(m.value[0])):
                return m
        return None


@dataclass
class FakeUserData:
    rruid: str = "rr-user-42"
    token: str = "fake-token"

    def as_dict(self) -> dict[str, str]:
        return {"rruid": self.rruid, "token": self.token}


class FakeAuthClient:
    def __init__(self, username: str, base_url: str | None) -> None:
        self.username = username
        self._base_url = base_url or "https://usiot.roborock.com"
        self.code_requested = False

    @property
    async def base_url(self) -> str:
        return self._base_url

    async def request_code_v4(self) -> None:
        if self.username == "missing@example.invalid":
            raise RoborockAccountDoesNotExist("no such account")
        if self.username == "rate@example.invalid":
            raise RoborockTooFrequentCodeRequests("slow down")
        self.code_requested = True

    async def code_login_v4(self, code: str) -> FakeUserData:
        if code != "123456":
            raise RoborockInvalidCode("bad code")
        return FakeUserData()


class FakeTrait:
    def __init__(self, is_on: bool = False) -> None:
        self.is_on = is_on
        self.refreshes = 0

    async def refresh(self) -> None:
        self.refreshes += 1

    async def enable(self) -> None:
        self.is_on = True

    async def disable(self) -> None:
        self.is_on = False


class FakeCommandApi:
    def __init__(self) -> None:
        self.sent: list[tuple[str, object]] = []

    async def send(self, command: str, params: object = None) -> None:
        self.sent.append((command, params))


class FakeV1Status:
    def __init__(self) -> None:
        self.state = FakeStateCode.charging
        self.state_name = "charging"
        self.error_code_name = "none"
        self.battery = 98
        self.in_cleaning = 0
        self.in_returning = 0
        self.fan_speed_options = list(FakeFanMode)
        self.fan_speed_name = "balanced"
        self.fan_speed_mapping = {
            101: "quiet",
            102: "balanced",
            103: "turbo",
            104: "max",
        }
        self.square_meter_clean_area = 42.5
        self.clean_time = 1800
        self.clean_percent = 100
        self.water_box_carriage_status = 1
        self.water_box_status = 1
        self.water_shortage_status = 0
        self.dry_status = 0
        self.dock_type = FakeDockType.auto_empty_dock_pure
        self.dock_error_status = FakeDockError.ok

    async def refresh(self) -> None:
        pass


class FakeConsumables:
    def __init__(self) -> None:
        self.main_brush_time_left = 10000
        self.side_brush_time_left = 8000
        self.filter_time_left = 6000
        self.sensor_time_left = 4000
        self.reset_calls: list[str] = []

    async def refresh(self) -> None:
        pass

    async def reset_consumable(self, attr: FakeConsumableAttribute) -> None:
        # Mirror real python-roborock: evaluates `attr.value`, rejecting raw strings.
        if not isinstance(attr, FakeConsumableAttribute):
            raise AttributeError("ConsumableAttribute enum required")
        self.reset_calls.append(attr.value)


class FakeCleanSummary:
    def __init__(self) -> None:
        self.square_meter_clean_area = 1234.5
        self.clean_time = 90000
        self.clean_count = 75

    async def refresh(self) -> None:
        pass


class FakeV1Props:
    def __init__(self) -> None:
        self.status = FakeV1Status()
        self.consumables = FakeConsumables()
        self.clean_summary = FakeCleanSummary()
        self.dnd = FakeTrait(is_on=True)
        self.child_lock = FakeTrait(is_on=False)
        self.flow_led_status = FakeTrait(is_on=True)
        self.command = FakeCommandApi()


class FakeProduct:
    def __init__(self, model: str) -> None:
        self.model = model


class FakeDeviceInfo:
    def __init__(self, fv: str) -> None:
        self.fv = fv


class FakeV1Device:
    def __init__(self) -> None:
        self.duid = "DUID-S8-01"
        self.name = "Living Room Roborock"
        self.product = FakeProduct("roborock.vacuum.a70")
        self.device_info = FakeDeviceInfo("02.18.42")
        self.is_local_connected = True
        self.v1_properties = FakeV1Props()
        self.b01_q7_properties = None
        self.b01_q10_properties = None


class FakeQ7Props:
    def __init__(self) -> None:
        self.calls: list[tuple[str, object]] = []

    async def query_values(self, props: list[FakeB01Props]) -> object:
        # Enforce real python-roborock enum list requirement.
        if not all(isinstance(p, FakeB01Props) for p in props):
            raise TypeError("RoborockB01Props enum list required")

        @dataclass
        class Data:
            status: Enum = FakeQ7Status.CHARGING
            wind_name: str = "turbo"
            quantity: int = 85

        return Data()

    async def start_clean(self) -> None:
        self.calls.append(("start_clean", None))

    async def pause_clean(self) -> None:
        self.calls.append(("pause_clean", None))

    async def stop_clean(self) -> None:
        self.calls.append(("stop_clean", None))

    async def return_to_dock(self) -> None:
        self.calls.append(("return_to_dock", None))

    async def find_me(self) -> None:
        self.calls.append(("find_me", None))

    async def set_fan_speed(self, speed: FakeSCWindMapping) -> None:
        # Mirror real python-roborock: accesses `.code`.
        self.calls.append(("set_fan_speed", speed.code))

    async def send(self, cmd: str, params: object = None) -> None:
        self.calls.append(("send", (cmd, params)))


class FakeQ7Device:
    def __init__(self) -> None:
        self.duid = "DUID-Q7-01"
        self.name = "Bedroom Q7"
        self.product = FakeProduct("roborock.vacuum.q7")
        self.device_info = FakeDeviceInfo("01.02.03")
        self.v1_properties = None
        self.b01_q7_properties = FakeQ7Props()
        self.b01_q10_properties = None


class FakeQ10StatusTrait:
    def __init__(self) -> None:
        self.status: FakeQ10Status | None = None
        self.fan_level: FakeYXFanLevel | None = None
        self.battery: int | None = None
        self._listeners: list[Callable[[], None]] = []

    def add_update_listener(self, cb: Callable[[], None]) -> Callable[[], None]:
        self._listeners.append(cb)

        def unsub() -> None:
            if cb in self._listeners:
                self._listeners.remove(cb)

        return unsub

    def emit_push(
        self, status: FakeQ10Status, fan_level: FakeYXFanLevel, battery: int
    ) -> None:
        self.status = status
        self.fan_level = fan_level
        self.battery = battery
        for cb in list(self._listeners):
            cb()


class FakeQ10VacuumApi:
    def __init__(self) -> None:
        self.calls: list[tuple[str, object]] = []

    async def start_clean(self) -> None:
        self.calls.append(("start_clean", None))

    async def set_fan_level(self, level: FakeYXFanLevel) -> None:
        # Mirror real python-roborock: reads `.code`.
        self.calls.append(("set_fan_level", level.code))


class FakeQ10CommandApi:
    def __init__(self) -> None:
        self.calls: list[tuple[int, object]] = []

    async def send(self, dp: FakeQ10DP, params: object = None) -> None:
        # Mirror real python-roborock MQTT encoder: reads `.code`.
        self.calls.append((dp.code, params))


class FakeQ10Props:
    def __init__(self) -> None:
        self.status = FakeQ10StatusTrait()
        self.vacuum = FakeQ10VacuumApi()
        self.command = FakeQ10CommandApi()

    async def refresh(self) -> None:
        # Real Q10 refresh is fire-and-forget; push arrives asynchronously later.
        pass


class FakeQ10Device:
    def __init__(self) -> None:
        self.duid = "DUID-Q10-01"
        self.name = "Hall Q10"
        self.product = FakeProduct("roborock.vacuum.q10")
        self.device_info = FakeDeviceInfo("01.00.01")
        self.v1_properties = None
        self.b01_q7_properties = None
        self.b01_q10_properties = FakeQ10Props()


class FakeManager:
    def __init__(self, devices: list[object]) -> None:
        self._devices = devices
        self.closed = False

    async def get_devices(self) -> list[object]:
        return self._devices

    async def close(self) -> None:
        self.closed = True


class FakeCache:
    def __init__(self) -> None:
        self.flushed = False

    async def flush(self) -> None:
        self.flushed = True


class FakeBackend:
    def __init__(
        self,
        devices: list[object] | None = None,
        setup_error: Exception | None = None,
        setup_gate: dict[str, asyncio.Event] | None = None,
    ) -> None:
        self.devices = devices if devices is not None else [FakeV1Device()]
        self.setup_error = setup_error
        self.setup_gate = setup_gate or {}
        self.managers: list[FakeManager] = []
        self.caches: list[FakeCache] = []
        self.sessions_closed = 0

    def resolve_consumable_attribute(self, attr_name: str) -> FakeConsumableAttribute:
        return FakeConsumableAttribute[attr_name.upper()]

    def q7_request_props(self) -> list[FakeB01Props]:
        return [
            FakeB01Props.STATUS,
            FakeB01Props.WIND,
            FakeB01Props.QUANTITY,
            FakeB01Props.CLEANING_TIME,
        ]

    def q7_fan_speed_list(self) -> list[str]:
        return FakeSCWindMapping.keys()

    def resolve_q7_fan_speed(self, fan_speed: str) -> FakeSCWindMapping:
        return FakeSCWindMapping.from_value(fan_speed)

    def q10_fan_speed_list(self) -> list[str]:
        return [m.value for m in FakeYXFanLevel if m != FakeYXFanLevel.UNKNOWN]

    def resolve_q10_fan_level(self, fan_speed: str) -> FakeYXFanLevel:
        return FakeYXFanLevel.from_value(fan_speed)

    def resolve_q10_dp(self, command: str) -> FakeQ10DP:
        dp = FakeQ10DP.from_any_optional(command)
        if dp is None:
            raise ValueError(f"invalid Q10 command {command!r}")
        return dp

    async def create_auth_client(self, username: str, base_url: str | None):
        client = FakeAuthClient(username, base_url)

        async def close_session() -> None:
            self.sessions_closed += 1

        return client, close_session

    async def create_device_manager(
        self,
        *,
        entry_id: str,
        username: str,
        user_data_dict: dict[str, object],
        base_url: str | None,
        data_dir: Path,
    ):
        if entry_id in self.setup_gate:
            await self.setup_gate[entry_id].wait()
        if self.setup_error is not None:
            raise self.setup_error
        mgr = FakeManager(self.devices)
        cache = FakeCache()
        self.managers.append(mgr)
        self.caches.append(cache)

        async def close_session() -> None:
            self.sessions_closed += 1

        return mgr, cache, close_session


class RoborockPluginTests(unittest.TestCase):
    def _make_plugin(self, backend: FakeBackend):
        sent: list[dict[str, object]] = []

        async def send_fn(msg: dict[str, object]) -> None:
            sent.append(msg)

        plugin = rp.RoborockPlugin(send_fn, backend=backend, poll_enabled=False)
        return plugin, sent

    def test_manifests_and_assets_are_valid_json(self) -> None:
        for rel in (
            "plugin.json",
            "plugin.sidecar.json",
            "translations/en.json",
            "icons.json",
        ):
            data = json.loads((PLUGIN_DIR / rel).read_text(encoding="utf-8"))
            self.assertIsInstance(data, dict)
        m = json.loads((PLUGIN_DIR / "plugin.json").read_text(encoding="utf-8"))
        self.assertEqual(m["domain"], "roborock")
        self.assertIn("vacuum", m["capabilities"]["entity_domains"])
        self.assertEqual(m["transport"]["type"], "stdio")
        s = json.loads((PLUGIN_DIR / "plugin.sidecar.json").read_text(encoding="utf-8"))
        self.assertEqual(s["transport"]["type"], "unix_socket")

    def test_config_flow_happy_path_and_errors(self) -> None:
        async def run() -> None:
            backend = FakeBackend()
            plugin, sent = self._make_plugin(backend)

            await plugin.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {"protocol_version": 1, "domain": "roborock"},
                }
            )
            self.assertEqual(sent[-1]["result"]["protocol_version"], 1)

            await plugin.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "flow.step",
                    "params": {"flow_id": "f1", "step_id": "user", "input": None},
                }
            )
            res = sent[-1]["result"]
            self.assertEqual(res["type"], "form")
            self.assertEqual(res["step_id"], "user")
            self.assertEqual([f["name"] for f in res["schema"]], ["username", "region"])

            await plugin.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "flow.step",
                    "params": {
                        "flow_id": "f1",
                        "step_id": "user",
                        "input": {"username": "missing@example.invalid", "region": "us"},
                    },
                }
            )
            self.assertEqual(sent[-1]["result"]["errors"], {"base": "invalid_email"})

            await plugin.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 4,
                    "method": "flow.step",
                    "params": {
                        "flow_id": "f1",
                        "step_id": "user",
                        "input": {"username": "alex@example.invalid", "region": "auto"},
                    },
                }
            )
            self.assertEqual(sent[-1]["result"]["step_id"], "code")

            await plugin.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 5,
                    "method": "flow.step",
                    "params": {
                        "flow_id": "f1",
                        "step_id": "code",
                        "input": {"code": "000000"},
                    },
                }
            )
            self.assertEqual(sent[-1]["result"]["errors"], {"base": "invalid_code"})

            await plugin.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 6,
                    "method": "flow.step",
                    "params": {
                        "flow_id": "f1",
                        "step_id": "code",
                        "input": {"code": "123456"},
                    },
                }
            )
            created = sent[-1]["result"]
            self.assertEqual(created["type"], "create_entry")
            self.assertEqual(created["title"], "alex@example.invalid")
            self.assertEqual(created["unique_id"], "rr-user-42")
            self.assertEqual(created["data"]["username"], "alex@example.invalid")
            self.assertEqual(created["data"]["user_data"]["rruid"], "rr-user-42")
            self.assertEqual(created["data"]["base_url"], "https://usiot.roborock.com")
            self.assertGreaterEqual(backend.sessions_closed, 2)

        asyncio.run(run())

    def test_v1_setup_buttons_recovery_service_calls_and_unload(self) -> None:
        async def run() -> None:
            dev = FakeV1Device()
            backend = FakeBackend(devices=[dev])
            plugin, sent = self._make_plugin(backend)

            await plugin.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "entry.setup",
                    "params": {
                        "entry_id": "e1",
                        "title": "alex@example.invalid",
                        "data": {
                            "username": "alex@example.invalid",
                            "user_data": {"rruid": "rr-user-42", "token": "fake"},
                            "base_url": "https://usiot.roborock.com",
                        },
                    },
                }
            )

            status_msgs = [m for m in sent if m.get("method") == "entry.status"]
            self.assertEqual(status_msgs[-1]["params"]["state"], "loaded")

            upserted = {
                m["params"]["unique_id"]: m["params"]
                for m in sent
                if m.get("method") == "entity.upsert"
            }
            self.assertEqual(upserted["duid_s8_01"]["domain"], "vacuum")
            self.assertEqual(
                upserted["duid_s8_01"]["fan_speed_list"],
                ["quiet", "balanced", "turbo", "max"],
            )
            self.assertEqual(upserted["battery_duid_s8_01"]["domain"], "sensor")
            self.assertEqual(
                upserted["in_cleaning_duid_s8_01"]["domain"], "binary_sensor"
            )
            self.assertEqual(
                upserted["water_shortage_duid_s8_01"]["domain"], "binary_sensor"
            )
            self.assertEqual(upserted["child_lock_duid_s8_01"]["domain"], "switch")
            self.assertEqual(upserted["empty_dustbin_duid_s8_01"]["domain"], "button")

            # Buttons get initial `unknown` state (so they aren't left unavailable).
            states = {
                m["params"]["unique_id"]: m["params"]
                for m in sent
                if m.get("method") == "state.set"
            }
            self.assertEqual(states["duid_s8_01"]["state"], "docked")
            self.assertEqual(states["battery_duid_s8_01"]["state"], "98")
            self.assertEqual(states["battery_charging_duid_s8_01"]["state"], "on")
            self.assertEqual(states["child_lock_duid_s8_01"]["state"], "off")
            self.assertEqual(states["empty_dustbin_duid_s8_01"]["state"], "unknown")
            self.assertEqual(
                states["reset_main_brush_consumable_duid_s8_01"]["state"], "unknown"
            )

            # Outage marks buttons unavailable; subsequent refresh restores them.
            entry = plugin._entries["e1"]
            managed = entry.devices["DUID-S8-01"]
            sent.clear()
            await plugin._mark_device_unavailable(entry, managed)
            unavail_states = {
                m["params"]["unique_id"]: m["params"]["state"]
                for m in sent
                if m.get("method") == "state.set"
            }
            self.assertEqual(unavail_states["empty_dustbin_duid_s8_01"], "unavailable")

            sent.clear()
            await plugin._refresh_device(entry, managed, initial=False)
            recovered_states = {
                m["params"]["unique_id"]: m["params"]["state"]
                for m in sent
                if m.get("method") == "state.set"
            }
            self.assertEqual(recovered_states["empty_dustbin_duid_s8_01"], "unknown")
            self.assertEqual(
                recovered_states["reset_main_brush_consumable_duid_s8_01"], "unknown"
            )
            # Healthy subsequent poll does NOT overwrite button state again.
            sent.clear()
            await plugin._refresh_device(entry, managed, initial=False)
            healthy_states = {
                m["params"]["unique_id"]: m["params"]["state"]
                for m in sent
                if m.get("method") == "state.set"
            }
            self.assertNotIn("empty_dustbin_duid_s8_01", healthy_states)

            # Vacuum service calls (including smart segment resume on start).
            dev.v1_properties.status.in_cleaning = 3
            for idx, (svc, data) in enumerate(
                [
                    ("start", {}),
                    ("pause", {}),
                    ("stop", {}),
                    ("return_to_base", {}),
                    ("clean_spot", {}),
                    ("locate", {}),
                    ("set_fan_speed", {"fan_speed": "turbo"}),
                    (
                        "send_command",
                        {"command": "app_segment_clean", "params": [{"segments": [16]}]},
                    ),
                ],
                start=10,
            ):
                await plugin.handle_message(
                    {
                        "jsonrpc": "2.0",
                        "id": idx,
                        "method": "entity.call",
                        "params": {
                            "entry_id": "e1",
                            "unique_id": "duid_s8_01",
                            "domain": "vacuum",
                            "service": svc,
                            "data": data,
                        },
                    }
                )
                self.assertEqual(sent[-1]["result"], {"ok": True})

            self.assertEqual(
                dev.v1_properties.command.sent,
                [
                    ("resume_segment_clean", None),
                    ("app_pause", None),
                    ("app_stop", None),
                    ("app_charge", None),
                    ("app_spot", None),
                    ("find_me", None),
                    ("set_custom_mode", [103]),
                    ("app_segment_clean", [{"segments": [16]}]),
                ],
            )

            # Switch toggle, dock button, and enum-backed consumable reset.
            await plugin.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 30,
                    "method": "entity.call",
                    "params": {
                        "entry_id": "e1",
                        "unique_id": "child_lock_duid_s8_01",
                        "domain": "switch",
                        "service": "turn_on",
                        "data": {},
                    },
                }
            )
            self.assertTrue(dev.v1_properties.child_lock.is_on)

            await plugin.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 31,
                    "method": "entity.call",
                    "params": {
                        "entry_id": "e1",
                        "unique_id": "empty_dustbin_duid_s8_01",
                        "domain": "button",
                        "service": "press",
                        "data": {},
                    },
                }
            )
            self.assertEqual(
                dev.v1_properties.command.sent[-1], ("app_start_collect_dust", None)
            )

            await plugin.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 32,
                    "method": "entity.call",
                    "params": {
                        "entry_id": "e1",
                        "unique_id": "reset_main_brush_consumable_duid_s8_01",
                        "domain": "button",
                        "service": "press",
                        "data": {},
                    },
                }
            )
            self.assertEqual(
                dev.v1_properties.consumables.reset_calls, ["main_brush_work_time"]
            )

            # Unload cleans up manager, cache, and aiohttp session.
            await plugin.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 40,
                    "method": "entry.unload",
                    "params": {"entry_id": "e1"},
                }
            )
            self.assertTrue(backend.managers[0].closed)
            self.assertTrue(backend.caches[0].flushed)
            self.assertEqual(backend.sessions_closed, 1)

        asyncio.run(run())

    def test_q7_and_q10_enums_and_q10_push_listener(self) -> None:
        async def run() -> None:
            q7 = FakeQ7Device()
            q10 = FakeQ10Device()
            backend = FakeBackend(devices=[q7, q10])
            plugin, sent = self._make_plugin(backend)
            await plugin.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "entry.setup",
                    "params": {
                        "entry_id": "e-b01",
                        "title": "B01",
                        "data": {"username": "a@b", "user_data": {"rruid": "1"}},
                    },
                }
            )

            # Q7 fan speed list matches SCWindMapping (no `off`, includes `max_plus`).
            upserted = {
                m["params"]["unique_id"]: m["params"]
                for m in sent
                if m.get("method") == "entity.upsert"
            }
            self.assertEqual(
                upserted["duid_q7_01"]["fan_speed_list"],
                ["quiet", "balanced", "turbo", "max", "max_plus"],
            )

            # Q7 set_fan_speed passes enum with `.code` (3 for turbo).
            await plugin.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "entity.call",
                    "params": {
                        "entry_id": "e-b01",
                        "unique_id": "duid_q7_01",
                        "domain": "vacuum",
                        "service": "set_fan_speed",
                        "data": {"fan_speed": "turbo"},
                    },
                }
            )
            self.assertEqual(q7.b01_q7_properties.calls, [("set_fan_speed", 3)])

            # Q10 push listener publishes state immediately when MQTT push arrives.
            sent.clear()
            q10.b01_q10_properties.status.emit_push(
                FakeQ10Status.CLEANING, FakeYXFanLevel.TURBO, 77
            )
            await asyncio.sleep(0)
            pushed = {
                m["params"]["unique_id"]: m["params"]
                for m in sent
                if m.get("method") == "state.set"
            }
            self.assertEqual(pushed["duid_q10_01"]["state"], "cleaning")
            self.assertEqual(pushed["duid_q10_01"]["attributes"]["fan_speed"], "turbo")
            self.assertEqual(pushed["battery_duid_q10_01"]["state"], "77")

            # Q10 locate, set_fan_speed, and send_command resolve real enums.
            await plugin.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "entity.call",
                    "params": {
                        "entry_id": "e-b01",
                        "unique_id": "duid_q10_01",
                        "domain": "vacuum",
                        "service": "locate",
                        "data": {},
                    },
                }
            )
            await plugin.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 4,
                    "method": "entity.call",
                    "params": {
                        "entry_id": "e-b01",
                        "unique_id": "duid_q10_01",
                        "domain": "vacuum",
                        "service": "set_fan_speed",
                        "data": {"fan_speed": "turbo"},
                    },
                }
            )
            await plugin.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 5,
                    "method": "entity.call",
                    "params": {
                        "entry_id": "e-b01",
                        "unique_id": "duid_q10_01",
                        "domain": "vacuum",
                        "service": "send_command",
                        "data": {"command": "dpEmpty", "params": [1]},
                    },
                }
            )
            self.assertEqual(
                q10.b01_q10_properties.command.calls,
                [(11, None), (12, [1])],
            )
            self.assertEqual(
                q10.b01_q10_properties.vacuum.calls,
                [("set_fan_level", 3)],
            )

            await plugin.handle_message({"jsonrpc": "2.0", "id": 6, "method": "shutdown"})
            # Listener unsubscribed on shutdown.
            self.assertEqual(q10.b01_q10_properties.status._listeners, [])

            # Auth error -> setup_error; transient error -> setup_retry.
            for exc, expected in (
                (RoborockInvalidCredentials("expired"), "setup_error"),
                (RoborockException("timeout"), "setup_retry"),
            ):
                p2, s2 = self._make_plugin(FakeBackend(setup_error=exc))
                await p2.handle_message(
                    {
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "entry.setup",
                        "params": {
                            "entry_id": "e-fail",
                            "title": "Fail",
                            "data": {"username": "a@b", "user_data": {"rruid": "1"}},
                        },
                    }
                )
                status = [m for m in s2 if m.get("method") == "entry.status"][-1]
                self.assertEqual(status["params"]["state"], expected)

        asyncio.run(run())

    def test_concurrent_stream_dispatch_avoids_head_of_line_blocking(self) -> None:
        async def run() -> None:
            gate = asyncio.Event()
            dev = FakeV1Device()
            backend = FakeBackend(devices=[dev], setup_gate={"e-slow": gate})

            reader = asyncio.StreamReader()
            sent: list[dict[str, object]] = []
            call_done = asyncio.Event()

            async def send_fn(obj: dict[str, object]) -> None:
                sent.append(obj)
                if obj.get("id") == 3:
                    call_done.set()

            serve_task = asyncio.create_task(
                rp._serve_stream(reader, send_fn, backend=backend, poll_enabled=False)
            )

            def push(msg: dict[str, object]) -> None:
                reader.feed_data((json.dumps(msg) + "\n").encode("utf-8"))

            # Load fast entry `e-fast`.
            push(
                {
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "entry.setup",
                    "params": {
                        "entry_id": "e-fast",
                        "title": "Fast",
                        "data": {"username": "a@b", "user_data": {"rruid": "1"}},
                    },
                }
            )
            await asyncio.sleep(0.05)

            # Start stalled setup for `e-slow`, then immediately call `locate` on `e-fast`.
            push(
                {
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "entry.setup",
                    "params": {
                        "entry_id": "e-slow",
                        "title": "Slow",
                        "data": {"username": "b@c", "user_data": {"rruid": "2"}},
                    },
                }
            )
            push(
                {
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "entity.call",
                    "params": {
                        "entry_id": "e-fast",
                        "unique_id": "duid_s8_01",
                        "domain": "vacuum",
                        "service": "locate",
                        "data": {},
                    },
                }
            )

            # `entity.call` must complete even though `e-slow` is still blocked.
            await asyncio.wait_for(call_done.wait(), timeout=2.0)
            self.assertIn(("find_me", None), dev.v1_properties.command.sent)

            # Shutdown must cancel the stalled setup cleanly without hanging.
            push({"jsonrpc": "2.0", "id": 4, "method": "shutdown", "params": {}})
            reader.feed_eof()
            await asyncio.wait_for(serve_task, timeout=2.0)

        asyncio.run(run())

    def test_unix_socket_refuses_to_unlink_regular_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            regular = Path(tmp) / "important.txt"
            regular.write_text("keep me", encoding="utf-8")
            with self.assertRaises(RuntimeError):
                rp._remove_stale_socket(regular)
            self.assertEqual(regular.read_text(encoding="utf-8"), "keep me")

    def test_atomic_json_cache_roundtrip_and_mode(self) -> None:
        @dataclass
        class DummyCacheData:
            value: str = ""

            @classmethod
            def from_dict(cls, d: dict[str, str]) -> "DummyCacheData":
                return cls(value=d.get("value", ""))

            def as_dict(self) -> dict[str, str]:
                return {"value": self.value}

        async def run() -> None:
            with tempfile.TemporaryDirectory() as tmp:
                path = Path(tmp) / "entry-cache.json"
                cache = rp.AtomicJsonFileCache(path, DummyCacheData)
                data = await cache.get()
                self.assertEqual(data.value, "")
                await cache.set(DummyCacheData(value="secret-key"))
                await cache.flush()
                self.assertTrue(path.is_file())
                if os.name == "posix":
                    self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)

                cache2 = rp.AtomicJsonFileCache(path, DummyCacheData)
                loaded = await cache2.get()
                self.assertEqual(loaded.value, "secret-key")

        asyncio.run(run())


if __name__ == "__main__":
    unittest.main()

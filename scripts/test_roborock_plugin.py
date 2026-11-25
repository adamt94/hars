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


class FakeQ7Status(Enum):
    CHARGING = 8
    SWEEP_MOPING = 5


class FakeQ10Status(Enum):
    CLEANING = 3
    CHARGING = 6


class FakeQ10Fan(Enum):
    TURBO = "turbo"


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
        self.fan_speed_mapping = {101: "quiet", 102: "balanced", 103: "turbo", 104: "max"}
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

    async def reset_consumable(self, attr: str) -> None:
        self.reset_calls.append(attr)


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

    async def query_values(self, _props: list[str]) -> object:
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

    async def set_fan_speed(self, speed: str) -> None:
        self.calls.append(("set_fan_speed", speed))

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
    ) -> None:
        self.devices = devices if devices is not None else [FakeV1Device()]
        self.setup_error = setup_error
        self.managers: list[FakeManager] = []
        self.caches: list[FakeCache] = []
        self.sessions_closed = 0

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

            # Show initial user form.
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

            # Non-existent email surfaces HA's `invalid_email` error key.
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

            # Valid email advances to `code` step.
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

            # Bad verification code keeps user on `code` step with `invalid_code`.
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

            # Valid code creates HA-compatible entry data.
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

    def test_v1_setup_entities_service_calls_and_unload(self) -> None:
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

            methods = [m.get("method") for m in sent if "method" in m]
            self.assertIn("device.upsert", methods)
            self.assertIn("entity.upsert", methods)
            self.assertIn("state.set", methods)
            self.assertIn("entry.status", methods)

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
            self.assertEqual(upserted["in_cleaning_duid_s8_01"]["domain"], "binary_sensor")
            self.assertEqual(upserted["child_lock_duid_s8_01"]["domain"], "switch")
            self.assertEqual(upserted["empty_dustbin_duid_s8_01"]["domain"], "button")

            states = {
                m["params"]["unique_id"]: m["params"]
                for m in sent
                if m.get("method") == "state.set"
            }
            self.assertEqual(states["duid_s8_01"]["state"], "docked")
            self.assertEqual(states["battery_duid_s8_01"]["state"], "98")
            self.assertEqual(states["battery_charging_duid_s8_01"]["state"], "on")
            self.assertEqual(states["child_lock_duid_s8_01"]["state"], "off")

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

            # Switch toggle and dock button press.
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

    def test_q7_device_and_setup_error_classification(self) -> None:
        async def run() -> None:
            q7 = FakeQ7Device()
            backend = FakeBackend(devices=[q7])
            plugin, sent = self._make_plugin(backend)
            await plugin.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "entry.setup",
                    "params": {
                        "entry_id": "e-q7",
                        "title": "Q7",
                        "data": {"username": "a@b", "user_data": {"rruid": "1"}},
                    },
                }
            )
            await plugin.handle_message(
                {
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "entity.call",
                    "params": {
                        "entry_id": "e-q7",
                        "unique_id": "duid_q7_01",
                        "domain": "vacuum",
                        "service": "start",
                        "data": {},
                    },
                }
            )
            self.assertEqual(q7.b01_q7_properties.calls, [("start_clean", None)])
            await plugin.handle_message({"jsonrpc": "2.0", "id": 3, "method": "shutdown"})

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

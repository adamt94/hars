#!/usr/bin/env python3
"""Out-of-process Roborock plugin for hars (NDJSON JSON-RPC 2.0).

Supports both `stdio` (spawned directly by hars) and `unix_socket` (sidecar
container listening on `<config>/plugins/run/roborock.sock`).

Mirrors Home Assistant 2026.6.1's `components/roborock` config flow, entry data
shape (`username`, `user_data`, `base_url`), state mappings, polling intervals,
and vacuum/sensor/binary_sensor/switch/button entities using `python-roborock`.
"""

from __future__ import annotations

import argparse
import asyncio
from dataclasses import dataclass, field
import json
import os
from pathlib import Path
import re
import stat
import sys
import tempfile
import time
from typing import Any, Awaitable, Callable

PROTOCOL_VERSION = 1
PLUGIN_VERSION = "0.1.0"
DOMAIN = "roborock"
MAX_CONCURRENT_REQUESTS = 64

# Home Assistant VacuumEntityFeature bitmask (`components/vacuum/const.py`).
FEAT_PAUSE = 4
FEAT_STOP = 8
FEAT_RETURN_HOME = 16
FEAT_FAN_SPEED = 32
FEAT_SEND_COMMAND = 256
FEAT_LOCATE = 512
FEAT_CLEAN_SPOT = 1024
FEAT_STATE = 4096
FEAT_START = 8192

V1_VACUUM_FEATURES = (
    FEAT_PAUSE
    | FEAT_STOP
    | FEAT_RETURN_HOME
    | FEAT_FAN_SPEED
    | FEAT_SEND_COMMAND
    | FEAT_LOCATE
    | FEAT_CLEAN_SPOT
    | FEAT_STATE
    | FEAT_START
)
B01_VACUUM_FEATURES = (
    FEAT_PAUSE
    | FEAT_STOP
    | FEAT_RETURN_HOME
    | FEAT_FAN_SPEED
    | FEAT_SEND_COMMAND
    | FEAT_LOCATE
    | FEAT_STATE
    | FEAT_START
)

REGION_OPTIONS: list[list[str]] = [
    ["auto", "Auto"],
    ["us", "US"],
    ["eu", "EU"],
    ["ru", "RU"],
    ["cn", "CN"],
]

# HA 2026.6.1 `components/roborock/const.py` polling intervals (seconds).
V1_LOCAL_CLEANING_INTERVAL = 15.0
V1_LOCAL_IDLE_INTERVAL = 30.0
V1_CLOUD_CLEANING_INTERVAL = 30.0
V1_CLOUD_IDLE_INTERVAL = 60.0
B01_POLL_INTERVAL = 60.0
# HA `MIN_UNAVAILABLE_DURATION`: suppress transient <2 min reconnect blips.
MIN_UNAVAILABLE_SECONDS = 120.0

# HA 2026.6.1 `components/roborock/vacuum.py` state code -> VacuumActivity.
V1_STATE_NAME_TO_ACTIVITY: dict[str, str] = {
    "starting": "idle",
    "attaching_the_mop": "docked",
    "charger_disconnected": "idle",
    "idle": "idle",
    "remote_control_active": "cleaning",
    "cleaning": "cleaning",
    "detaching_the_mop": "docked",
    "returning_home": "returning",
    "manual_mode": "cleaning",
    "charging": "docked",
    "charging_problem": "error",
    "paused": "paused",
    "spot_cleaning": "cleaning",
    "error": "error",
    "shutting_down": "idle",
    "updating": "docked",
    "docking": "returning",
    "going_to_target": "cleaning",
    "zoned_cleaning": "cleaning",
    "segment_cleaning": "cleaning",
    "emptying_the_bin": "docked",
    "washing_the_mop": "docked",
    "going_to_wash_the_mop": "returning",
    "charging_complete": "docked",
    "device_offline": "error",
}

Q7_STATE_NAME_TO_ACTIVITY: dict[str, str] = {
    "SLEEPING": "idle",
    "WAITING_FOR_ORDERS": "idle",
    "PAUSED": "paused",
    "DOCKING": "returning",
    "CHARGING": "docked",
    "SWEEP_MOPING": "cleaning",
    "SWEEP_MOPING_2": "cleaning",
    "MOPING": "cleaning",
    "UPDATING": "docked",
    "MOP_CLEANING": "docked",
    "MOP_AIRDRYING": "docked",
}

Q10_STATE_NAME_TO_ACTIVITY: dict[str, str] = {
    "SLEEPING": "idle",
    "IDLE": "idle",
    "CLEANING": "cleaning",
    "RETURNING_HOME": "returning",
    "REMOTE_CONTROL_ACTIVE": "cleaning",
    "CHARGING": "docked",
    "PAUSED": "paused",
    "ERROR": "error",
    "UPDATING": "docked",
    "EMPTYING_THE_BIN": "docked",
    "MAPPING": "cleaning",
    "RELOCATING": "cleaning",
    "SWEEPING": "cleaning",
    "MOPPING": "cleaning",
    "SWEEP_AND_MOP": "cleaning",
    "TRANSITIONING": "cleaning",
    "WAITING_TO_CHARGE": "docked",
}

_SLUG_RE = re.compile(r"[^a-z0-9]+")


def slugify(text: str) -> str:
    """Simple ASCII slug matching HA's duid_slug for Roborock identifiers."""
    return _SLUG_RE.sub("_", text.lower()).strip("_")


def _enum_name(value: Any) -> str | None:
    if value is None:
        return None
    name = getattr(value, "name", None)
    if isinstance(name, str):
        return name
    return str(value)


def _enum_value_str(value: Any) -> str | None:
    if value is None:
        return None
    val = getattr(value, "value", None)
    if isinstance(val, str):
        return val
    name = getattr(value, "name", None)
    if isinstance(name, str):
        return name
    return str(value)


def map_v1_activity(state_code: Any, state_name: str | None = None) -> str:
    """Map a Roborock V1 state code or name to HA VacuumActivity."""
    key = state_name or _enum_name(state_code)
    if not key:
        return "unknown"
    return V1_STATE_NAME_TO_ACTIVITY.get(key.lower(), "unknown")


def map_q7_activity(status: Any) -> str:
    key = _enum_name(status)
    if not key:
        return "unknown"
    return Q7_STATE_NAME_TO_ACTIVITY.get(key.upper(), "unknown")


def map_q10_activity(status: Any) -> str:
    key = _enum_name(status)
    if not key:
        return "unknown"
    return Q10_STATE_NAME_TO_ACTIVITY.get(key.upper(), "unknown")


def choose_v1_start_command(status: Any) -> str:
    """HA 2026.6.1 `RoborockVacuum.async_start` resume selection."""
    if status is not None:
        if getattr(status, "in_returning", None) == 1:
            return "app_charge"
        in_cleaning = getattr(status, "in_cleaning", None)
        if in_cleaning == 2:
            return "resume_zoned_clean"
        if in_cleaning == 3:
            return "resume_segment_clean"
        if in_cleaning == 4:
            return "app_resume_build_map"
    return "app_start"


def classify_auth_error(exc: Exception) -> str:
    """Map python-roborock config-flow exceptions to HA error keys."""
    name = type(exc).__name__
    mapping = {
        "RoborockAccountDoesNotExist": "invalid_email",
        "RoborockUrlException": "unknown_url",
        "RoborockInvalidEmail": "invalid_email_format",
        "RoborockTooFrequentCodeRequests": "too_frequent_code_requests",
        "RoborockInvalidCode": "invalid_code",
        "RoborockException": "unknown_roborock",
    }
    if name in mapping:
        return mapping[name]
    for cls in type(exc).__mro__:
        if cls.__name__ in mapping:
            return mapping[cls.__name__]
    return "unknown"


def is_auth_failure(exc: Exception) -> bool:
    names = {cls.__name__ for cls in type(exc).__mro__}
    return bool(
        names
        & {
            "RoborockInvalidCredentials",
            "MqttSessionUnauthorized",
        }
    )


def _remove_stale_socket(socket_path: Path) -> None:
    """Remove an existing Unix domain socket; refuse to touch non-socket files."""
    try:
        st = os.lstat(socket_path)
    except FileNotFoundError:
        return
    if not stat.S_ISSOCK(st.st_mode):
        raise RuntimeError(
            f"refusing to remove non-socket path at {socket_path}"
        )
    socket_path.unlink()


class AtomicJsonFileCache:
    """Persistent file-backed cache matching `roborock.devices.cache.Cache`."""

    def __init__(self, path: Path, cache_data_cls: Any) -> None:
        self._path = path
        self._cache_data_cls = cache_data_cls
        self._data: Any | None = None

    async def get(self) -> Any:
        if self._data is None:
            if self._path.is_file():
                try:
                    raw = json.loads(self._path.read_text(encoding="utf-8"))
                    self._data = self._cache_data_cls.from_dict(raw)
                except Exception:
                    self._data = self._cache_data_cls()
            else:
                self._data = self._cache_data_cls()
        return self._data

    async def set(self, value: Any) -> None:
        self._data = value

    async def flush(self) -> None:
        if self._data is None:
            return
        self._path.parent.mkdir(parents=True, exist_ok=True)
        payload = json.dumps(self._data.as_dict(), separators=(",", ":"))
        fd, tmp_name = tempfile.mkstemp(
            dir=str(self._path.parent),
            prefix=f".{self._path.name}.",
            suffix=".tmp",
        )
        try:
            os.fchmod(fd, 0o600)
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                handle.write(payload)
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(tmp_name, self._path)
        except Exception:
            try:
                os.unlink(tmp_name)
            except OSError:
                pass
            raise


class LiveRoborockBackend:
    """Real `python-roborock` backend (loaded lazily so tests need no deps)."""

    def _import_roborock(self) -> dict[str, Any]:
        try:
            import aiohttp
            from roborock.data import SCWindMapping, UserData
            from roborock.data.b01_q10.b01_q10_code_mappings import (
                B01_Q10_DP,
                YXFanLevel,
            )
            from roborock.devices.cache import CacheData
            from roborock.devices.device_manager import (
                UserParams,
                create_device_manager,
            )
            from roborock.devices.traits.v1.consumeable import ConsumableAttribute
            from roborock.roborock_message import RoborockB01Props
            from roborock.web_api import RoborockApiClient
        except ImportError as exc:
            raise RuntimeError(
                "python-roborock and aiohttp are required. Install them with: "
                "pip install 'python-roborock==5.12.0' 'aiohttp>=3.9' "
                "(or install `uv` so bin/hars-roborock provisions them automatically)."
            ) from exc
        return {
            "aiohttp": aiohttp,
            "UserData": UserData,
            "SCWindMapping": SCWindMapping,
            "B01_Q10_DP": B01_Q10_DP,
            "YXFanLevel": YXFanLevel,
            "CacheData": CacheData,
            "UserParams": UserParams,
            "create_device_manager": create_device_manager,
            "ConsumableAttribute": ConsumableAttribute,
            "RoborockB01Props": RoborockB01Props,
            "RoborockApiClient": RoborockApiClient,
        }

    def resolve_consumable_attribute(self, attr_name: str) -> Any:
        enum_cls = self._import_roborock()["ConsumableAttribute"]
        try:
            return enum_cls[attr_name.upper()]
        except KeyError as exc:
            raise ValueError(f"unknown consumable attribute {attr_name!r}") from exc

    def q7_request_props(self) -> list[Any]:
        props_cls = self._import_roborock()["RoborockB01Props"]
        return [
            props_cls.STATUS,
            props_cls.WIND,
            props_cls.QUANTITY,
            props_cls.CLEANING_TIME,
        ]

    def q7_fan_speed_list(self) -> list[str]:
        wind_cls = self._import_roborock()["SCWindMapping"]
        return list(wind_cls.keys())

    def resolve_q7_fan_speed(self, fan_speed: str) -> Any:
        wind_cls = self._import_roborock()["SCWindMapping"]
        return wind_cls.from_value(fan_speed)

    def q10_fan_speed_list(self) -> list[str]:
        fan_cls = self._import_roborock()["YXFanLevel"]
        return [level.value for level in fan_cls if level != fan_cls.UNKNOWN]

    def resolve_q10_fan_level(self, fan_speed: str) -> Any:
        fan_cls = self._import_roborock()["YXFanLevel"]
        return fan_cls.from_value(fan_speed)

    def resolve_q10_dp(self, command: str) -> Any:
        dp_cls = self._import_roborock()["B01_Q10_DP"]
        dp = dp_cls.from_any_optional(command)
        if dp is None:
            raise ValueError(f"invalid Q10 command {command!r}")
        return dp

    async def create_auth_client(
        self, username: str, base_url: str | None
    ) -> tuple[Any, Callable[[], Awaitable[None]]]:
        mods = self._import_roborock()
        session = mods["aiohttp"].ClientSession()
        client = mods["RoborockApiClient"](
            username,
            base_url=base_url,
            session=session,
        )
        return client, session.close

    async def create_device_manager(
        self,
        *,
        entry_id: str,
        username: str,
        user_data_dict: dict[str, Any],
        base_url: str | None,
        data_dir: Path,
    ) -> tuple[Any, Any, Callable[[], Awaitable[None]]]:
        mods = self._import_roborock()
        session = mods["aiohttp"].ClientSession()
        cache = AtomicJsonFileCache(
            data_dir / f"{entry_id}-cache.json",
            mods["CacheData"],
        )
        try:
            user_data = mods["UserData"].from_dict(user_data_dict)
            user_params = mods["UserParams"](
                username=username,
                user_data=user_data,
                base_url=base_url,
            )
            manager = await mods["create_device_manager"](
                user_params,
                cache=cache,
                session=session,
                prefer_cache=False,
            )
        except Exception:
            await session.close()
            raise
        return manager, cache, session.close


@dataclass
class FlowSession:
    username: str
    client: Any
    close_session: Callable[[], Awaitable[None]]


@dataclass
class ManagedDevice:
    kind: str  # "v1", "b01_q7", "b01_q10"
    device: Any
    duid: str
    duid_slug: str
    registered_entities: set[str] = field(default_factory=set)
    last_success: float | None = None
    unavailable: bool = False
    switches: dict[str, Any] = field(default_factory=dict)
    buttons: dict[str, Callable[[], Awaitable[None]]] = field(default_factory=dict)
    q7_data: Any = None
    unsub_listeners: list[Callable[[], None]] = field(default_factory=list)
    publish_lock: asyncio.Lock = field(default_factory=asyncio.Lock)
    push_dirty: bool = False
    push_task: asyncio.Task[None] | None = None


@dataclass
class ActiveEntry:
    entry_id: str
    device_manager: Any
    cache: Any
    close_session: Callable[[], Awaitable[None]]
    devices: dict[str, ManagedDevice] = field(default_factory=dict)
    entity_to_device: dict[str, str] = field(default_factory=dict)
    poll_task: asyncio.Task[None] | None = None
    closed: bool = False


class RoborockPlugin:
    """Stateful NDJSON JSON-RPC 2.0 handler for the Roborock plugin."""

    def __init__(
        self,
        send_fn: Callable[[dict[str, Any]], Awaitable[None]],
        backend: Any | None = None,
        poll_enabled: bool = True,
    ) -> None:
        self._send = send_fn
        self._backend = backend or LiveRoborockBackend()
        self._poll_enabled = poll_enabled
        self._data_dir = Path(".")
        self._flows: dict[str, FlowSession] = {}
        self._pending_flow_tasks: dict[str, asyncio.Task[Any]] = {}
        self._closing_tasks: set[asyncio.Task[Any]] = set()
        self._shutting_down = False
        self._entries: dict[str, ActiveEntry] = {}
        self._entry_locks: dict[str, asyncio.Lock] = {}
        self._send_lock = asyncio.Lock()

    async def send_frame(
        self,
        obj: dict[str, Any],
        guard: Callable[[], bool] | None = None,
    ) -> bool:
        async with self._send_lock:
            if guard is not None and not guard():
                return False
            await self._send(obj)
            return True

    async def _close_session_safe(
        self, close_fn: Callable[[], Awaitable[None]]
    ) -> None:
        task = asyncio.create_task(close_fn())
        self._closing_tasks.add(task)
        task.add_done_callback(self._closing_tasks.discard)
        try:
            await asyncio.shield(task)
        except Exception:
            pass

    def _entry_lock(self, entry_id: str) -> asyncio.Lock:
        lock = self._entry_locks.get(entry_id)
        if lock is None:
            lock = asyncio.Lock()
            self._entry_locks[entry_id] = lock
        return lock

    async def notify(self, method: str, params: dict[str, Any]) -> None:
        await self.send_frame({"jsonrpc": "2.0", "method": method, "params": params})

    async def log(self, level: str, message: str) -> None:
        await self.notify("log", {"level": level, "message": message})

    async def handle_message(self, msg: dict[str, Any]) -> bool:
        """Process one incoming JSON-RPC message. Returns False on shutdown."""
        method = msg.get("method")
        msg_id = msg.get("id")
        params = msg.get("params")
        if not isinstance(params, dict):
            params = {}

        if not method:
            # Ignore responses to our own notifications/requests.
            return True

        try:
            if method == "initialize":
                result = await self._on_initialize(params)
            elif method == "flow.step":
                result = await self._on_flow_step(params)
            elif method == "flow.cancel":
                result = await self._on_flow_cancel(params)
            elif method == "entry.setup":
                result = await self._on_entry_setup(params)
            elif method == "entry.unload":
                result = await self._on_entry_unload(params)
            elif method == "entity.call":
                result = await self._on_entity_call(params)
            elif method == "shutdown":
                await self._on_shutdown()
                if msg_id is not None:
                    await self.send_frame(
                        {"jsonrpc": "2.0", "id": msg_id, "result": {"ok": True}}
                    )
                return False
            else:
                if msg_id is not None:
                    await self.send_frame(
                        {
                            "jsonrpc": "2.0",
                            "id": msg_id,
                            "error": {
                                "code": -32601,
                                "message": f"unknown method {method!r}",
                            },
                        }
                    )
                return True

            if msg_id is not None:
                await self.send_frame({"jsonrpc": "2.0", "id": msg_id, "result": result})
        except Exception as exc:
            await self.log("error", f"{method} failed: {exc}")
            if msg_id is not None:
                await self.send_frame(
                    {
                        "jsonrpc": "2.0",
                        "id": msg_id,
                        "error": {"code": -32000, "message": str(exc)},
                    }
                )
        return True

    async def _on_initialize(self, params: dict[str, Any]) -> dict[str, Any]:
        data_dir = params.get("data_dir")
        if isinstance(data_dir, str) and data_dir:
            self._data_dir = Path(data_dir)
            self._data_dir.mkdir(parents=True, exist_ok=True)
        return {
            "protocol_version": PROTOCOL_VERSION,
            "plugin_version": PLUGIN_VERSION,
        }

    # ------------------------------------------------------------------
    # Config flow (matches HA 2026.6.1 `components/roborock/config_flow.py`)
    # ------------------------------------------------------------------

    def _user_form(self, error: str | None = None) -> dict[str, Any]:
        errors = {"base": error} if error else {}
        return {
            "type": "form",
            "step_id": "user",
            "schema": [
                {"name": "username", "required": True, "kind": {"type": "string"}},
                {
                    "name": "region",
                    "required": True,
                    "default": "auto",
                    "kind": {"type": "select", "options": REGION_OPTIONS},
                },
            ],
            "errors": errors,
        }

    def _code_form(self, error: str | None = None) -> dict[str, Any]:
        errors = {"base": error} if error else {}
        return {
            "type": "form",
            "step_id": "code",
            "schema": [
                {"name": "code", "required": True, "kind": {"type": "string"}},
            ],
            "errors": errors,
        }

    async def _close_flow(self, flow_id: str) -> None:
        flow = self._flows.pop(flow_id, None)
        if flow is not None:
            await self._close_session_safe(flow.close_session)

    async def _on_flow_step(self, params: dict[str, Any]) -> dict[str, Any]:
        flow_id = str(params.get("flow_id", ""))
        step_id = str(params.get("step_id", "user"))
        user_input = params.get("input")

        if step_id == "user":
            if user_input is None:
                return self._user_form()
            username = str(user_input.get("username", "")).strip()
            region = str(user_input.get("region", "auto")).strip() or "auto"
            base_url = None if region == "auto" else f"https://{region}iot.roborock.com"

            await self._close_flow(flow_id)
            try:
                client, close_fn = await self._backend.create_auth_client(
                    username, base_url
                )
            except Exception as exc:
                await self.log("error", f"Roborock auth client init failed: {exc}")
                return self._user_form("unknown")

            current = asyncio.current_task()
            if current is not None and flow_id:
                self._pending_flow_tasks[flow_id] = current
            try:
                await client.request_code_v4()
            except asyncio.CancelledError:
                await self._close_session_safe(close_fn)
                raise
            except Exception as exc:
                await self._close_session_safe(close_fn)
                err_key = classify_auth_error(exc)
                await self.log(
                    "warn", f"Roborock code request failed ({err_key}): {exc}"
                )
                return self._user_form(err_key)
            finally:
                if current is not None and self._pending_flow_tasks.get(flow_id) is current:
                    self._pending_flow_tasks.pop(flow_id, None)

            if self._shutting_down:
                await self._close_session_safe(close_fn)
                return {"type": "abort", "reason": "unknown"}

            self._flows[flow_id] = FlowSession(
                username=username,
                client=client,
                close_session=close_fn,
            )
            return self._code_form()

        if step_id == "code":
            flow = self._flows.get(flow_id)
            if flow is None:
                return {"type": "abort", "reason": "unknown"}
            if user_input is None:
                return self._code_form()
            code = str(user_input.get("code", "")).strip()
            try:
                user_data = await flow.client.code_login_v4(code)
            except Exception as exc:
                err_key = classify_auth_error(exc)
                if err_key == "invalid_email":
                    err_key = "invalid_email_or_region"
                await self.log(
                    "warn", f"Roborock code login failed ({err_key}): {exc}"
                )
                return self._code_form(err_key)

            base_url_attr = getattr(flow.client, "base_url", None)
            if asyncio.iscoroutine(base_url_attr) or hasattr(
                base_url_attr, "__await__"
            ):
                resolved_base_url = await base_url_attr
            else:
                resolved_base_url = base_url_attr

            user_data_dict = (
                user_data.as_dict()
                if hasattr(user_data, "as_dict")
                else dict(user_data)
            )
            rruid = getattr(user_data, "rruid", None) or user_data_dict.get("rruid")
            username = flow.username
            await self._close_flow(flow_id)
            return {
                "type": "create_entry",
                "title": username,
                "unique_id": str(rruid) if rruid is not None else username,
                "data": {
                    "username": username,
                    "user_data": user_data_dict,
                    "base_url": resolved_base_url,
                },
            }

        return {"type": "abort", "reason": "unknown"}

    async def _on_flow_cancel(self, params: dict[str, Any]) -> dict[str, Any]:
        flow_id = str(params.get("flow_id", ""))
        # Keep the task in `_pending_flow_tasks` until its `finally` clears it
        # so overlapping `flow.cancel` calls also await session close.
        pending = self._pending_flow_tasks.get(flow_id)
        if pending is not None and not pending.done():
            pending.cancel()
            await asyncio.gather(pending, return_exceptions=True)
        await self._close_flow(flow_id)
        return {"ok": True}

    # ------------------------------------------------------------------
    # Entry setup, polling, and unload
    # ------------------------------------------------------------------

    async def _on_entry_setup(self, params: dict[str, Any]) -> dict[str, Any]:
        entry_id = str(params.get("entry_id", ""))
        async with self._entry_lock(entry_id):
            return await self._setup_entry_locked(entry_id, params)

    async def _setup_entry_locked(
        self, entry_id: str, params: dict[str, Any]
    ) -> dict[str, Any]:
        data = params.get("data") or {}
        if entry_id in self._entries:
            await self._unload_entry_impl(entry_id)

        username = data.get("username")
        user_data_dict = data.get("user_data")
        base_url = data.get("base_url")
        if not isinstance(username, str) or not isinstance(user_data_dict, dict):
            await self.notify(
                "entry.status",
                {
                    "entry_id": entry_id,
                    "state": "setup_error",
                    "reason": "invalid Roborock config entry data",
                },
            )
            return {"ok": True}

        try:
            manager, cache, close_session = await self._backend.create_device_manager(
                entry_id=entry_id,
                username=username,
                user_data_dict=user_data_dict,
                base_url=base_url if isinstance(base_url, str) else None,
                data_dir=self._data_dir,
            )
        except Exception as exc:
            state = "setup_error" if is_auth_failure(exc) else "setup_retry"
            await self.log("error", f"Roborock setup failed ({state}): {exc}")
            await self.notify(
                "entry.status",
                {"entry_id": entry_id, "state": state, "reason": str(exc)},
            )
            return {"ok": True}

        entry = ActiveEntry(
            entry_id=entry_id,
            device_manager=manager,
            cache=cache,
            close_session=close_session,
        )
        self._entries[entry_id] = entry

        try:
            devices = await manager.get_devices()
        except Exception as exc:
            await self._unload_entry_impl(entry_id)
            state = "setup_error" if is_auth_failure(exc) else "setup_retry"
            await self.log("error", f"Roborock get_devices failed: {exc}")
            await self.notify(
                "entry.status",
                {"entry_id": entry_id, "state": state, "reason": str(exc)},
            )
            return {"ok": True}

        setup_count = 0
        for dev in devices:
            kind = self._detect_device_kind(dev)
            if kind is None:
                await self.log(
                    "warn",
                    f"Skipping unsupported Roborock device {getattr(dev, 'duid', '?')}",
                )
                continue
            managed = ManagedDevice(
                kind=kind,
                device=dev,
                duid=str(dev.duid),
                duid_slug=slugify(str(dev.duid)),
            )
            entry.devices[managed.duid] = managed
            await self._upsert_device(entry_id, managed)
            if kind == "b01_q10":
                self._attach_q10_listener(entry, managed)
            try:
                await self._refresh_device(entry, managed, initial=True)
                setup_count += 1
            except Exception as exc:
                await self.log(
                    "warn",
                    f"Initial refresh failed for Roborock {managed.duid}: {exc}",
                )

        if devices and setup_count == 0 and entry.devices:
            await self._unload_entry_impl(entry_id)
            await self.notify(
                "entry.status",
                {
                    "entry_id": entry_id,
                    "state": "setup_retry",
                    "reason": "could not reach any Roborock devices",
                },
            )
            return {"ok": True}

        if self._poll_enabled and entry.devices:
            entry.poll_task = asyncio.create_task(self._poll_loop(entry))

        await self.notify("entry.status", {"entry_id": entry_id, "state": "loaded"})
        return {"ok": True}

    def _detect_device_kind(self, dev: Any) -> str | None:
        if getattr(dev, "v1_properties", None) is not None:
            return "v1"
        if getattr(dev, "b01_q7_properties", None) is not None:
            return "b01_q7"
        if getattr(dev, "b01_q10_properties", None) is not None:
            return "b01_q10"
        return None

    async def _upsert_device(self, entry_id: str, managed: ManagedDevice) -> None:
        dev = managed.device
        product = getattr(dev, "product", None)
        dev_info = getattr(dev, "device_info", None)
        model = getattr(product, "model", None) if product is not None else None
        sw_version = getattr(dev_info, "fv", None) if dev_info is not None else None
        await self.notify(
            "device.upsert",
            {
                "entry_id": entry_id,
                "identifier": managed.duid,
                "name": getattr(dev, "name", None) or model or "Roborock",
                "manufacturer": "Roborock",
                "model": model,
                "model_id": model,
                "sw_version": sw_version,
            },
        )

    def _entry_is_current(self, entry: ActiveEntry) -> bool:
        return not entry.closed and self._entries.get(entry.entry_id) is entry

    async def _notify_if_current(
        self, entry: ActiveEntry, method: str, params: dict[str, Any]
    ) -> bool:
        return await self.send_frame(
            {"jsonrpc": "2.0", "method": method, "params": params},
            guard=lambda: self._entry_is_current(entry),
        )

    async def _ensure_entity(
        self,
        entry: ActiveEntry,
        managed: ManagedDevice,
        unique_id: str,
        domain: str,
        *,
        name: str | None = None,
        original_icon: str | None = None,
        device_class: str | None = None,
        unit_of_measurement: str | None = None,
        entity_category: str | None = None,
        supported_features: int = 0,
        fan_speed_list: list[str] | None = None,
    ) -> bool:
        """Register entity if new; returns True if newly registered."""
        if not self._entry_is_current(entry) or unique_id in managed.registered_entities:
            return False
        payload: dict[str, Any] = {
            "entry_id": entry.entry_id,
            "unique_id": unique_id,
            "domain": domain,
            "device_identifier": managed.duid,
            "supported_features": supported_features,
        }
        if name is not None:
            payload["name"] = name
        if original_icon is not None:
            payload["original_icon"] = original_icon
        if device_class is not None:
            payload["device_class"] = device_class
        if unit_of_measurement is not None:
            payload["unit_of_measurement"] = unit_of_measurement
        if entity_category is not None:
            payload["entity_category"] = entity_category
        if fan_speed_list is not None:
            payload["fan_speed_list"] = fan_speed_list
        # Mark registered before awaiting the serialized socket write so
        # concurrent publishers cannot emit duplicate `entity.upsert` frames
        # and stale entries cannot write after replacement.
        managed.registered_entities.add(unique_id)
        entry.entity_to_device[unique_id] = managed.duid
        try:
            sent = await self._notify_if_current(entry, "entity.upsert", payload)
        except BaseException:
            managed.registered_entities.discard(unique_id)
            raise
        if not sent:
            managed.registered_entities.discard(unique_id)
            return False
        return True

    async def _set_state(
        self,
        entry: ActiveEntry,
        unique_id: str,
        state: str,
        attributes: dict[str, Any] | None = None,
    ) -> None:
        await self._notify_if_current(
            entry,
            "state.set",
            {
                "entry_id": entry.entry_id,
                "unique_id": unique_id,
                "state": state,
                "attributes": attributes or {},
            },
        )

    async def _ensure_button(
        self,
        entry: ActiveEntry,
        managed: ManagedDevice,
        uid: str,
        *,
        name: str,
        icon: str,
        action: Callable[[], Awaitable[None]],
        entity_category: str | None = None,
    ) -> None:
        managed.buttons[uid] = action
        is_new = await self._ensure_entity(
            entry,
            managed,
            uid,
            "button",
            name=name,
            original_icon=icon,
            entity_category=entity_category,
        )
        # Publish initial state (`unknown`) and restore availability after an
        # outage without clobbering press timestamps during healthy polls.
        if is_new or managed.unavailable:
            await self._set_state(entry, uid, "unknown")

    async def _refresh_device(
        self, entry: ActiveEntry, managed: ManagedDevice, *, initial: bool = False
    ) -> None:
        if not self._entry_is_current(entry):
            return
        if managed.kind == "v1":
            await self._refresh_v1(entry, managed, initial=initial)
        elif managed.kind == "b01_q7":
            await self._refresh_q7(entry, managed)
        elif managed.kind == "b01_q10":
            await self._refresh_q10(entry, managed)
        if self._entry_is_current(entry):
            managed.last_success = time.monotonic()
            managed.unavailable = False

    async def _refresh_v1(
        self, entry: ActiveEntry, managed: ManagedDevice, *, initial: bool
    ) -> None:
        props = managed.device.v1_properties
        # Refresh core traits serially (matching HA `_refresh_traits` to avoid
        # overloading the vacuum or cloud server).
        await props.status.refresh()
        for attr in (
            "consumables",
            "clean_summary",
            "dnd",
            "child_lock",
            "flow_led_status",
        ):
            trait = getattr(props, attr, None)
            if trait is not None:
                try:
                    await trait.refresh()
                except Exception as exc:
                    if initial:
                        await self.log(
                            "debug",
                            f"Optional trait {attr} refresh skipped for {managed.duid}: {exc}",
                        )

        status = props.status
        consumables = getattr(props, "consumables", None)
        clean_summary = getattr(props, "clean_summary", None)
        slug = managed.duid_slug

        # 1. Primary vacuum entity (`name=None` matches HA `_attr_name = None`,
        # so the entity takes the device's name and `vacuum.<device_slug>` id).
        fan_options = [
            _enum_value_str(m)
            for m in (getattr(status, "fan_speed_options", None) or [])
        ]
        fan_speed_list = [f for f in fan_options if f]
        await self._ensure_entity(
            entry,
            managed,
            slug,
            "vacuum",
            name=None,
            supported_features=V1_VACUUM_FEATURES,
            fan_speed_list=fan_speed_list,
        )
        vac_attrs: dict[str, Any] = {
            "supported_features": V1_VACUUM_FEATURES,
            "fan_speed_list": fan_speed_list,
            "fan_speed": getattr(status, "fan_speed_name", None),
        }
        if getattr(status, "state_name", None) is not None:
            vac_attrs["status"] = status.state_name
        if getattr(status, "battery", None) is not None:
            vac_attrs["battery_level"] = status.battery
        await self._set_state(
            entry,
            slug,
            map_v1_activity(
                getattr(status, "state", None), getattr(status, "state_name", None)
            ),
            vac_attrs,
        )

        # 2. Diagnostic sensors (matching HA `components/roborock/sensor.py`)
        sensor_specs: list[tuple[str, str, Any, str | None, str | None, str | None]] = [
            (
                f"battery_{slug}",
                "Battery",
                getattr(status, "battery", None),
                "battery",
                "%",
                None,
            ),
            (
                f"status_{slug}",
                "Status",
                getattr(status, "state_name", None),
                None,
                None,
                "mdi:information-outline",
            ),
            (
                f"vacuum_error_{slug}",
                "Vacuum error",
                getattr(status, "error_code_name", None),
                None,
                None,
                "mdi:alert-circle",
            ),
            (
                f"cleaning_area_{slug}",
                "Cleaning area",
                getattr(status, "square_meter_clean_area", None),
                None,
                "m²",
                "mdi:texture-box",
            ),
            (
                f"cleaning_time_{slug}",
                "Cleaning time",
                getattr(status, "clean_time", None),
                "duration",
                "s",
                "mdi:clock-outline",
            ),
            (
                f"clean_percent_{slug}",
                "Cleaning progress",
                getattr(status, "clean_percent", None),
                None,
                "%",
                "mdi:progress-check",
            ),
        ]
        if clean_summary is not None:
            sensor_specs.extend(
                [
                    (
                        f"total_cleaning_area_{slug}",
                        "Total cleaning area",
                        getattr(clean_summary, "square_meter_clean_area", None),
                        None,
                        "m²",
                        "mdi:texture-box",
                    ),
                    (
                        f"total_cleaning_time_{slug}",
                        "Total cleaning time",
                        getattr(clean_summary, "clean_time", None),
                        "duration",
                        "s",
                        "mdi:history",
                    ),
                    (
                        f"total_cleaning_count_{slug}",
                        "Total cleaning count",
                        getattr(clean_summary, "clean_count", None),
                        None,
                        None,
                        "mdi:counter",
                    ),
                ]
            )
        if consumables is not None:
            sensor_specs.extend(
                [
                    (
                        f"main_brush_time_left_{slug}",
                        "Main brush time left",
                        getattr(consumables, "main_brush_time_left", None),
                        "duration",
                        "s",
                        "mdi:brush",
                    ),
                    (
                        f"side_brush_time_left_{slug}",
                        "Side brush time left",
                        getattr(consumables, "side_brush_time_left", None),
                        "duration",
                        "s",
                        "mdi:brush",
                    ),
                    (
                        f"filter_time_left_{slug}",
                        "Filter time left",
                        getattr(consumables, "filter_time_left", None),
                        "duration",
                        "s",
                        "mdi:air-filter",
                    ),
                    (
                        f"sensor_time_left_{slug}",
                        "Sensor time left",
                        getattr(consumables, "sensor_time_left", None),
                        "duration",
                        "s",
                        "mdi:eye-outline",
                    ),
                ]
            )
        dock_type = getattr(status, "dock_type", None)
        dock_type_name = _enum_name(dock_type)
        has_dock = dock_type is not None and dock_type_name != "no_dock"
        if has_dock and getattr(status, "dock_error_status", None) is not None:
            sensor_specs.append(
                (
                    f"dock_error_{slug}",
                    "Dock error",
                    _enum_name(status.dock_error_status),
                    None,
                    None,
                    "mdi:garage-open",
                )
            )

        for uid, name, val, dc, unit, icon in sensor_specs:
            if val is None:
                continue
            await self._ensure_entity(
                entry,
                managed,
                uid,
                "sensor",
                name=name,
                device_class=dc,
                unit_of_measurement=unit,
                original_icon=icon,
                entity_category="diagnostic",
            )
            await self._set_state(entry, uid, str(val))

        # 3. Binary sensors (matching HA `components/roborock/binary_sensor.py`)
        state_name_lower = (getattr(status, "state_name", None) or "").lower()
        bin_specs: list[tuple[str, str, Any, str, str | None]] = [
            (
                f"in_cleaning_{slug}",
                "Cleaning",
                getattr(status, "in_cleaning", None),
                "running",
                "mdi:vacuum",
            ),
            (
                f"battery_charging_{slug}",
                "Charging",
                state_name_lower in ("charging", "charging_complete"),
                "battery_charging",
                None,
            ),
            (
                f"water_box_carriage_status_{slug}",
                "Mop attached",
                getattr(status, "water_box_carriage_status", None),
                "connectivity",
                "mdi:square-rounded",
            ),
            (
                f"water_box_status_{slug}",
                "Water box attached",
                getattr(status, "water_box_status", None),
                "connectivity",
                "mdi:water",
            ),
            (
                f"water_shortage_{slug}",
                "Water shortage",
                getattr(status, "water_shortage_status", None),
                "problem",
                "mdi:water",
            ),
            (
                f"dry_status_{slug}",
                "Mop drying",
                getattr(status, "dry_status", None),
                "running",
                "mdi:heat-wave",
            ),
        ]
        for uid, name, val, dc, icon in bin_specs:
            if val is None:
                continue
            await self._ensure_entity(
                entry,
                managed,
                uid,
                "binary_sensor",
                name=name,
                device_class=dc,
                original_icon=icon,
                entity_category="diagnostic",
            )
            await self._set_state(entry, uid, "on" if bool(val) else "off")

        # 4. Switches (matching HA `components/roborock/switch.py`)
        switch_specs = [
            (
                f"child_lock_{slug}",
                "Child lock",
                getattr(props, "child_lock", None),
                "mdi:account-lock",
            ),
            (
                f"status_indicator_{slug}",
                "Status indicator light",
                getattr(props, "flow_led_status", None),
                "mdi:alarm-light-outline",
            ),
            (
                f"dnd_switch_{slug}",
                "Do not disturb",
                getattr(props, "dnd", None),
                "mdi:bell-cancel",
            ),
        ]
        for uid, name, trait, icon in switch_specs:
            if trait is None:
                continue
            is_on = getattr(trait, "is_on", None)
            if is_on is None:
                continue
            managed.switches[uid] = trait
            await self._ensure_entity(
                entry,
                managed,
                uid,
                "switch",
                name=name,
                original_icon=icon,
                entity_category="config",
            )
            await self._set_state(entry, uid, "on" if bool(is_on) else "off")

        # 5. Buttons (dock actions + consumable resets)
        command_api = getattr(props, "command", None)
        if has_dock and command_api is not None:
            await self._ensure_button(
                entry,
                managed,
                f"empty_dustbin_{slug}",
                name="Empty dustbin",
                icon="mdi:delete-empty",
                action=lambda api=command_api: api.send("app_start_collect_dust"),
            )
            await self._ensure_button(
                entry,
                managed,
                f"wash_mop_{slug}",
                name="Wash mop",
                icon="mdi:water-sync",
                action=lambda api=command_api: api.send("app_start_wash"),
            )

        if consumables is not None and hasattr(consumables, "reset_consumable"):
            reset_specs = [
                (
                    f"reset_main_brush_consumable_{slug}",
                    "Reset main brush consumable",
                    "MAIN_BRUSH_WORK_TIME",
                    "mdi:brush",
                ),
                (
                    f"reset_side_brush_consumable_{slug}",
                    "Reset side brush consumable",
                    "SIDE_BRUSH_WORK_TIME",
                    "mdi:brush",
                ),
                (
                    f"reset_air_filter_consumable_{slug}",
                    "Reset air filter consumable",
                    "FILTER_WORK_TIME",
                    "mdi:air-filter",
                ),
                (
                    f"reset_sensor_consumable_{slug}",
                    "Reset sensor consumable",
                    "SENSOR_DIRTY_TIME",
                    "mdi:eye-outline",
                ),
            ]
            for uid, name, enum_key, icon in reset_specs:
                await self._ensure_button(
                    entry,
                    managed,
                    uid,
                    name=name,
                    icon=icon,
                    entity_category="config",
                    action=lambda c=consumables, k=enum_key: c.reset_consumable(
                        self._backend.resolve_consumable_attribute(k)
                    ),
                )

    async def _refresh_q7(self, entry: ActiveEntry, managed: ManagedDevice) -> None:
        api = managed.device.b01_q7_properties
        slug = managed.duid_slug
        data = await api.query_values(self._backend.q7_request_props())
        if data is None:
            raise RuntimeError("Q7 device returned empty properties")
        managed.q7_data = data
        fan_list = self._backend.q7_fan_speed_list()
        await self._ensure_entity(
            entry,
            managed,
            slug,
            "vacuum",
            name=None,
            supported_features=B01_VACUUM_FEATURES,
            fan_speed_list=fan_list,
        )
        await self._set_state(
            entry,
            slug,
            map_q7_activity(getattr(data, "status", None)),
            {
                "supported_features": B01_VACUUM_FEATURES,
                "fan_speed_list": fan_list,
                "fan_speed": getattr(data, "wind_name", None),
            },
        )
        if getattr(data, "quantity", None) is not None:
            uid = f"battery_{slug}"
            await self._ensure_entity(
                entry,
                managed,
                uid,
                "sensor",
                name="Battery",
                device_class="battery",
                unit_of_measurement="%",
                entity_category="diagnostic",
            )
            await self._set_state(entry, uid, str(data.quantity))

    def _attach_q10_listener(
        self, entry: ActiveEntry, managed: ManagedDevice
    ) -> None:
        api = managed.device.b01_q10_properties
        status_trait = getattr(api, "status", None)
        add_listener = getattr(status_trait, "add_update_listener", None)
        if not callable(add_listener):
            return

        async def drain_pushes() -> None:
            try:
                while managed.push_dirty and self._entry_is_current(entry):
                    managed.push_dirty = False
                    await self._publish_q10_state(entry, managed)
            finally:
                managed.push_task = None
                if managed.push_dirty and self._entry_is_current(entry):
                    # Schedule one more drain if a push arrived right as we exited.
                    on_push()

        def on_push() -> None:
            if not self._entry_is_current(entry):
                return
            managed.push_dirty = True
            if managed.push_task is not None and not managed.push_task.done():
                return
            try:
                loop = asyncio.get_running_loop()
            except RuntimeError:
                return
            managed.push_task = loop.create_task(drain_pushes())

        unsub = add_listener(on_push)
        if callable(unsub):
            managed.unsub_listeners.append(unsub)

    async def _publish_q10_state(
        self, entry: ActiveEntry, managed: ManagedDevice
    ) -> None:
        async with managed.publish_lock:
            if not self._entry_is_current(entry):
                return
            api = managed.device.b01_q10_properties
            slug = managed.duid_slug
            status = api.status
            fan_list = self._backend.q10_fan_speed_list()
            await self._ensure_entity(
                entry,
                managed,
                slug,
                "vacuum",
                name=None,
                supported_features=B01_VACUUM_FEATURES,
                fan_speed_list=fan_list,
            )
            fan_level = getattr(status, "fan_level", None)
            await self._set_state(
                entry,
                slug,
                map_q10_activity(getattr(status, "status", None)),
                {
                    "supported_features": B01_VACUUM_FEATURES,
                    "fan_speed_list": fan_list,
                    "fan_speed": _enum_value_str(fan_level),
                },
            )
            if getattr(status, "battery", None) is not None:
                uid = f"battery_{slug}"
                await self._ensure_entity(
                    entry,
                    managed,
                    uid,
                    "sensor",
                    name="Battery",
                    device_class="battery",
                    unit_of_measurement="%",
                    entity_category="diagnostic",
                )
                await self._set_state(entry, uid, str(status.battery))

    async def _refresh_q10(self, entry: ActiveEntry, managed: ManagedDevice) -> None:
        api = managed.device.b01_q10_properties
        await api.refresh()
        await self._publish_q10_state(entry, managed)

    def _next_poll_interval(self, entry: ActiveEntry) -> float:
        intervals: list[float] = []
        for managed in entry.devices.values():
            if managed.kind == "v1":
                props = managed.device.v1_properties
                in_cleaning = bool(getattr(props.status, "in_cleaning", False))
                local = bool(getattr(managed.device, "is_local_connected", False))
                if in_cleaning:
                    intervals.append(
                        V1_LOCAL_CLEANING_INTERVAL
                        if local
                        else V1_CLOUD_CLEANING_INTERVAL
                    )
                else:
                    intervals.append(
                        V1_LOCAL_IDLE_INTERVAL if local else V1_CLOUD_IDLE_INTERVAL
                    )
            else:
                intervals.append(B01_POLL_INTERVAL)
        return min(intervals) if intervals else V1_CLOUD_IDLE_INTERVAL

    async def _mark_device_unavailable(
        self, entry: ActiveEntry, managed: ManagedDevice
    ) -> None:
        if not self._entry_is_current(entry):
            return
        managed.unavailable = True
        for uid in list(managed.registered_entities):
            await self._set_state(entry, uid, "unavailable")

    async def _poll_loop(self, entry: ActiveEntry) -> None:
        try:
            while not entry.closed:
                interval = self._next_poll_interval(entry)
                await asyncio.sleep(interval)
                if entry.closed:
                    break
                for managed in list(entry.devices.values()):
                    if entry.closed:
                        break
                    try:
                        await self._refresh_device(entry, managed, initial=False)
                    except asyncio.CancelledError:
                        raise
                    except Exception as exc:
                        now = time.monotonic()
                        if (
                            managed.last_success is not None
                            and (now - managed.last_success) < MIN_UNAVAILABLE_SECONDS
                        ):
                            await self.log(
                                "debug",
                                f"Suppressing transient Roborock {managed.duid} refresh error: {exc}",
                            )
                            continue
                        await self.log(
                            "warn",
                            f"Roborock {managed.duid} refresh failed: {exc}",
                        )
                        await self._mark_device_unavailable(entry, managed)
        except asyncio.CancelledError:
            return

    # ------------------------------------------------------------------
    # Service calls (`entity.call`)
    # ------------------------------------------------------------------

    async def _on_entity_call(self, params: dict[str, Any]) -> dict[str, Any]:
        entry_id = str(params.get("entry_id", ""))
        unique_id = str(params.get("unique_id", ""))
        domain = str(params.get("domain", ""))
        service = str(params.get("service", ""))
        data = params.get("data") or {}

        entry = self._entries.get(entry_id)
        if entry is None:
            raise RuntimeError(f"Roborock entry {entry_id!r} is not loaded")
        duid = entry.entity_to_device.get(unique_id)
        if duid is None or duid not in entry.devices:
            raise RuntimeError(f"unknown Roborock entity {unique_id!r}")
        managed = entry.devices[duid]

        if domain == "vacuum":
            await self._call_vacuum(managed, service, data)
        elif domain == "switch":
            trait = managed.switches.get(unique_id)
            if trait is None:
                raise RuntimeError(f"unknown switch {unique_id!r}")
            if service == "turn_on":
                await trait.enable()
            elif service == "turn_off":
                await trait.disable()
            else:
                raise RuntimeError(f"unsupported switch service {service!r}")
        elif domain == "button":
            action = managed.buttons.get(unique_id)
            if action is None:
                raise RuntimeError(f"unknown button {unique_id!r}")
            await action()
        else:
            raise RuntimeError(f"unsupported domain {domain!r}")

        # Refresh immediately so HA reflects the new device state, unless the
        # entry was unloaded or replaced while the command ran.
        if self._entry_is_current(entry):
            try:
                await self._refresh_device(entry, managed, initial=False)
            except Exception as exc:
                await self.log(
                    "debug", f"Post-command refresh failed for {duid}: {exc}"
                )
        return {"ok": True}

    async def _call_vacuum(
        self, managed: ManagedDevice, service: str, data: dict[str, Any]
    ) -> None:
        if managed.kind == "v1":
            props = managed.device.v1_properties
            cmd_api = props.command
            if service == "start":
                await cmd_api.send(choose_v1_start_command(props.status))
            elif service == "pause":
                await cmd_api.send("app_pause")
            elif service == "stop":
                await cmd_api.send("app_stop")
            elif service == "return_to_base":
                await cmd_api.send("app_charge")
            elif service == "clean_spot":
                await cmd_api.send("app_spot")
            elif service == "locate":
                await cmd_api.send("find_me")
            elif service == "set_fan_speed":
                fan_speed = str(data.get("fan_speed", ""))
                mapping = getattr(props.status, "fan_speed_mapping", None) or {}
                reverse = {v: k for k, v in mapping.items()}
                if fan_speed not in reverse:
                    raise ValueError(f"invalid fan_speed {fan_speed!r}")
                await cmd_api.send("set_custom_mode", [reverse[fan_speed]])
            elif service == "send_command":
                command = str(data.get("command", ""))
                cmd_params = data.get("params")
                await cmd_api.send(command, cmd_params)
            else:
                raise ValueError(f"unsupported vacuum service {service!r}")
            return

        if managed.kind == "b01_q7":
            api = managed.device.b01_q7_properties
            if service == "start":
                await api.start_clean()
            elif service == "pause":
                await api.pause_clean()
            elif service == "stop":
                await api.stop_clean()
            elif service == "return_to_base":
                await api.return_to_dock()
            elif service == "locate":
                await api.find_me()
            elif service == "set_fan_speed":
                wind = self._backend.resolve_q7_fan_speed(
                    str(data.get("fan_speed", ""))
                )
                await api.set_fan_speed(wind)
            elif service == "send_command":
                await api.send(str(data.get("command", "")), data.get("params"))
            else:
                raise ValueError(f"unsupported Q7 vacuum service {service!r}")
            return

        if managed.kind == "b01_q10":
            api = managed.device.b01_q10_properties
            if service == "start":
                await api.vacuum.start_clean()
            elif service == "pause":
                await api.vacuum.pause_clean()
            elif service == "stop":
                await api.vacuum.stop_clean()
            elif service == "return_to_base":
                await api.vacuum.return_to_dock()
            elif service == "locate":
                dp_seek = self._backend.resolve_q10_dp("SEEK")
                await api.command.send(dp_seek)
            elif service == "set_fan_speed":
                fan_level = self._backend.resolve_q10_fan_level(
                    str(data.get("fan_speed", ""))
                )
                await api.vacuum.set_fan_level(fan_level)
            elif service == "send_command":
                dp_cmd = self._backend.resolve_q10_dp(str(data.get("command", "")))
                await api.command.send(dp_cmd, params=data.get("params"))
            else:
                raise ValueError(f"unsupported Q10 vacuum service {service!r}")

    async def _unload_entry_impl(self, entry_id: str) -> None:
        entry = self._entries.pop(entry_id, None)
        if entry is None:
            return
        entry.closed = True
        for managed in entry.devices.values():
            for unsub in managed.unsub_listeners:
                try:
                    unsub()
                except Exception:
                    pass
            managed.unsub_listeners.clear()
            managed.push_dirty = False
            if managed.push_task is not None:
                managed.push_task.cancel()
                try:
                    await managed.push_task
                except asyncio.CancelledError:
                    pass
                managed.push_task = None
        if entry.poll_task is not None:
            entry.poll_task.cancel()
            try:
                await entry.poll_task
            except asyncio.CancelledError:
                pass
        for fn in (
            getattr(entry.device_manager, "close", None),
            getattr(entry.cache, "flush", None),
            entry.close_session,
        ):
            if fn is not None:
                try:
                    await fn()
                except Exception:
                    pass

    async def _on_entry_unload(self, params: dict[str, Any]) -> dict[str, Any]:
        entry_id = str(params.get("entry_id", ""))
        async with self._entry_lock(entry_id):
            await self._unload_entry_impl(entry_id)
        return {"ok": True}

    async def _on_shutdown(self) -> None:
        self._shutting_down = True
        for flow_id in list(self._flows.keys()):
            await self._close_flow(flow_id)
        for entry_id in list(self._entries.keys()):
            await self._unload_entry_impl(entry_id)
        if self._closing_tasks:
            await asyncio.gather(*list(self._closing_tasks), return_exceptions=True)


# ----------------------------------------------------------------------
# Transports: stdio and Unix domain socket (concurrent RPC dispatch)
# ----------------------------------------------------------------------


async def _serve_stream(
    reader: asyncio.StreamReader,
    send_fn: Callable[[dict[str, Any]], Awaitable[None]],
    backend: Any | None = None,
    poll_enabled: bool = True,
) -> None:
    plugin = RoborockPlugin(send_fn, backend=backend, poll_enabled=poll_enabled)
    in_flight: set[asyncio.Task[Any]] = set()
    cleanup_tasks: set[asyncio.Task[Any]] = set()
    busy_tasks: set[asyncio.Task[Any]] = set()

    async def run_one(msg: dict[str, Any]) -> bool:
        return await plugin.handle_message(msg)

    try:
        while True:
            raw = await reader.readline()
            if not raw:
                break
            text = raw.decode("utf-8").strip()
            if not text:
                continue
            msg = json.loads(text)
            method = msg.get("method")
            if method == "initialize":
                await plugin.handle_message(msg)
                continue
            if method == "shutdown":
                plugin._shutting_down = True
                all_tasks = list(in_flight) + list(cleanup_tasks) + list(busy_tasks)
                for task in all_tasks:
                    task.cancel()
                if all_tasks:
                    await asyncio.gather(*all_tasks, return_exceptions=True)
                in_flight.clear()
                cleanup_tasks.clear()
                busy_tasks.clear()
                await plugin.handle_message(msg)
                break
            # Cleanup RPCs (`flow.cancel`, `entry.unload`) bypass ordinary
            # saturation (bounded separately to prevent unbounded growth).
            if method in ("flow.cancel", "entry.unload"):
                if len(cleanup_tasks) < MAX_CONCURRENT_REQUESTS:
                    task = asyncio.create_task(run_one(msg))
                    cleanup_tasks.add(task)
                    task.add_done_callback(cleanup_tasks.discard)
                continue
            if len(in_flight) >= MAX_CONCURRENT_REQUESTS:
                msg_id = msg.get("id")
                if msg_id is not None and len(busy_tasks) < MAX_CONCURRENT_REQUESTS:
                    # Send busy reply asynchronously under bounded task cap so
                    # a stalled writer neither blocks shutdown nor grows memory.
                    busy_task = asyncio.create_task(
                        plugin.send_frame(
                            {
                                "jsonrpc": "2.0",
                                "id": msg_id,
                                "error": {
                                    "code": -32000,
                                    "message": "plugin busy: too many in-flight requests",
                                },
                            }
                        )
                    )
                    busy_tasks.add(busy_task)
                    busy_task.add_done_callback(busy_tasks.discard)
                continue
            task = asyncio.create_task(run_one(msg))
            in_flight.add(task)
            task.add_done_callback(in_flight.discard)
    finally:
        plugin._shutting_down = True
        all_tasks = list(in_flight) + list(cleanup_tasks) + list(busy_tasks)
        for task in all_tasks:
            task.cancel()
        if all_tasks:
            await asyncio.gather(*all_tasks, return_exceptions=True)
        await plugin._on_shutdown()


async def run_stdio(backend: Any | None = None) -> None:
    loop = asyncio.get_running_loop()
    reader = asyncio.StreamReader(limit=256 * 1024)
    protocol = asyncio.StreamReaderProtocol(reader)
    await loop.connect_read_pipe(lambda: protocol, sys.stdin)

    # Serialized by `RoborockPlugin._send_lock` (single lock across check+write).
    async def send_fn(obj: dict[str, Any]) -> None:
        sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
        sys.stdout.flush()

    await _serve_stream(reader, send_fn, backend=backend)


async def run_unix_socket(socket_path: Path, backend: Any | None = None) -> None:
    socket_path.parent.mkdir(parents=True, exist_ok=True)
    _remove_stale_socket(socket_path)

    async def handle_client(
        reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        # Serialized by `RoborockPlugin._send_lock` (single lock across check+write).
        async def send_fn(obj: dict[str, Any]) -> None:
            writer.write((json.dumps(obj, separators=(",", ":")) + "\n").encode("utf-8"))
            await writer.drain()

        try:
            await _serve_stream(reader, send_fn, backend=backend)
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:
                pass

    server = await asyncio.start_unix_server(
        handle_client, path=str(socket_path), limit=256 * 1024
    )
    os.chmod(socket_path, 0o600)
    async with server:
        await server.serve_forever()


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(description="hars Roborock plugin")
    group = parser.add_mutually_exclusive_group()
    group.add_argument(
        "--stdio", action="store_true", help="Talk NDJSON JSON-RPC over stdin/stdout"
    )
    group.add_argument(
        "--socket",
        type=Path,
        help="Listen on a Unix domain socket (sidecar container mode)",
    )
    args = parser.parse_args(argv)
    if args.socket is not None:
        asyncio.run(run_unix_socket(args.socket))
    else:
        asyncio.run(run_stdio())


if __name__ == "__main__":
    main()

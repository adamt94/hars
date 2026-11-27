#!/usr/bin/env python3
"""Generate HA 2026.6.1 vectors for select, number, cover, lock, and fan.

Run with a Python environment that has Home Assistant 2026.6.1 installed:

    /path/to/havenv/bin/python scripts/vectors/entity_components.py \
        > crates/ha-integrations/tests/vectors/entity_components.json
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
from typing import Any

# scripts/vectors/http.py shadows stdlib `http` when invoked directly without -P.
_SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
sys.path = [p for p in sys.path if os.path.abspath(p or ".") != _SCRIPT_DIR]

import voluptuous as vol

from homeassistant.components.cover import (
    CoverEntity,
    CoverEntityFeature,
    async_setup as setup_cover,
)
from homeassistant.components.fan import (
    FanEntity,
    FanEntityFeature,
    async_setup as setup_fan,
)
from homeassistant.components.lock import (
    LockEntity,
    LockEntityFeature,
    async_setup as setup_lock,
)
from homeassistant.components.number import (
    NumberEntity,
    NumberMode,
    async_setup as setup_number,
)
from homeassistant.components.select import (
    SelectEntity,
    async_setup as setup_select,
)
from homeassistant.const import __version__ as HA_VERSION
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import (
    HomeAssistantError,
    ServiceValidationError,
)
from homeassistant import bootstrap, loader
from homeassistant.config_entries import ConfigEntries
from homeassistant.helpers import translation
from homeassistant.helpers.entity_component import EntityComponent
import tempfile


class FakeSelect(SelectEntity):
    def __init__(self, entity_id: str, options: list[str], current: str | None) -> None:
        self.entity_id = entity_id
        self._attr_options = list(options)
        self._attr_current_option = current
        self.calls: list[str] = []

    async def async_select_option(self, option: str) -> None:
        self._attr_current_option = option
        self.calls.append(option)


class FakeNumber(NumberEntity):
    def __init__(
        self,
        entity_id: str,
        min_v: float,
        max_v: float,
        step: float | None = None,
        mode: NumberMode = NumberMode.AUTO,
    ) -> None:
        self.entity_id = entity_id
        self._attr_native_min_value = min_v
        self._attr_native_max_value = max_v
        if step is not None:
            self._attr_native_step = step
        self._attr_mode = mode
        self.calls: list[float] = []

    async def async_set_native_value(self, value: float) -> None:
        self._attr_native_value = value
        self.calls.append(value)


class FakeCover(CoverEntity):
    def __init__(self, entity_id: str, features: CoverEntityFeature) -> None:
        self.entity_id = entity_id
        self._attr_supported_features = features
        self._attr_is_closed = True
        self._attr_is_opening = False
        self._attr_is_closing = False
        self._attr_current_cover_position = 0
        self._attr_current_cover_tilt_position = 0
        self.calls: list[str] = []

    async def async_open_cover(self, **kwargs: Any) -> None:
        self.calls.append("open_cover")

    async def async_close_cover(self, **kwargs: Any) -> None:
        self.calls.append("close_cover")

    async def async_stop_cover(self, **kwargs: Any) -> None:
        self.calls.append("stop_cover")

    async def async_set_cover_position(self, **kwargs: Any) -> None:
        self.calls.append(f"set_cover_position:{kwargs['position']}")

    async def async_open_cover_tilt(self, **kwargs: Any) -> None:
        self.calls.append("open_cover_tilt")

    async def async_close_cover_tilt(self, **kwargs: Any) -> None:
        self.calls.append("close_cover_tilt")

    async def async_stop_cover_tilt(self, **kwargs: Any) -> None:
        self.calls.append("stop_cover_tilt")

    async def async_set_cover_tilt_position(self, **kwargs: Any) -> None:
        self.calls.append(f"set_cover_tilt_position:{kwargs['tilt_position']}")


class FakeLock(LockEntity):
    def __init__(
        self,
        entity_id: str,
        features: LockEntityFeature,
        code_format: str | None = None,
        default_code: str = "",
    ) -> None:
        self.entity_id = entity_id
        self._attr_supported_features = features
        self._attr_code_format = code_format
        self._lock_option_default_code = default_code
        self.calls: list[str] = []

    async def async_lock(self, **kwargs: Any) -> None:
        self.calls.append(f"lock:{kwargs.get('code')}")

    async def async_unlock(self, **kwargs: Any) -> None:
        self.calls.append(f"unlock:{kwargs.get('code')}")

    async def async_open(self, **kwargs: Any) -> None:
        self.calls.append(f"open:{kwargs.get('code')}")


class FakeFan(FanEntity):
    def __init__(
        self,
        entity_id: str,
        features: FanEntityFeature,
        speed_count: int = 3,
        preset_modes: list[str] | None = None,
    ) -> None:
        self.entity_id = entity_id
        self._attr_supported_features = features
        self._attr_speed_count = speed_count
        self._attr_preset_modes = preset_modes
        self._attr_percentage = 0
        self._attr_preset_mode = None
        self.calls: list[str] = []

    async def async_turn_on(
        self,
        percentage: int | None = None,
        preset_mode: str | None = None,
        **kwargs: Any,
    ) -> None:
        if percentage is not None:
            self._attr_percentage = percentage
        if preset_mode is not None:
            self._attr_preset_mode = preset_mode
        self.calls.append(f"turn_on:{percentage}:{preset_mode}")

    async def async_turn_off(self, **kwargs: Any) -> None:
        self._attr_percentage = 0
        self._attr_preset_mode = None
        self.calls.append("turn_off")

    async def async_set_percentage(self, percentage: int) -> None:
        self._attr_percentage = percentage
        self.calls.append(f"set_percentage:{percentage}")

    async def async_set_preset_mode(self, preset_mode: str) -> None:
        self._attr_preset_mode = preset_mode
        self.calls.append(f"set_preset_mode:{preset_mode}")

    async def async_oscillate(self, oscillating: bool) -> None:
        self._attr_oscillating = oscillating
        self.calls.append(f"oscillate:{oscillating}")

    async def async_set_direction(self, direction: str) -> None:
        self._attr_current_direction = direction
        self.calls.append(f"set_direction:{direction}")


async def record_call(
    hass: HomeAssistant,
    domain: str,
    service: str,
    data: dict[str, Any],
    calls_ref: list[Any],
) -> dict[str, Any]:
    before = len(calls_ref)
    try:
        await hass.services.async_call(domain, service, dict(data), blocking=True)
    except ServiceValidationError as err:
        return {
            "domain": domain,
            "service": service,
            "data": data,
            "error": {
                "kind": "validation",
                "translation_domain": err.translation_domain,
                "translation_key": err.translation_key,
                "translation_placeholders": err.translation_placeholders,
                "message": str(err),
            },
        }
    except vol.Invalid as err:
        return {
            "domain": domain,
            "service": service,
            "data": data,
            "error": {
                "kind": "invalid",
                "message": str(err),
            },
        }
    except HomeAssistantError as err:
        return {
            "domain": domain,
            "service": service,
            "data": data,
            "error": {
                "kind": "ha_error",
                "message": str(err),
            },
        }
    except Exception as err:  # noqa: BLE001
        return {
            "domain": domain,
            "service": service,
            "data": data,
            "error": {
                "kind": "unknown",
                "type": type(err).__name__,
                "message": str(err),
            },
        }
    return {
        "domain": domain,
        "service": service,
        "data": data,
        "calls": calls_ref[before:],
    }


async def generate() -> dict[str, Any]:
    with tempfile.TemporaryDirectory() as tmpdir:
        hass = HomeAssistant(tmpdir)
        hass.config.skip_pip = True
        await hass.config.async_load()
        loader.async_setup(hass)
        hass.config_entries = ConfigEntries(hass, {})
        await hass.config_entries.async_initialize()
        await bootstrap.async_load_base_functionality(hass)
        await translation.async_load_integrations(
            hass, {"homeassistant", "select", "number", "cover", "lock", "fan"}
        )
        await setup_select(hass, {})
        await setup_number(hass, {})
        await setup_cover(hass, {})
        await setup_lock(hass, {})
        await setup_fan(hass, {})
        return await _run_scenarios(hass)


async def _run_scenarios(hass: HomeAssistant) -> dict[str, Any]:

    async def add_entity(domain: str, ent: Any) -> None:
        comp: EntityComponent = hass.data[domain]
        await comp.async_add_entities([ent])

    sel = FakeSelect("select.mode", ["off", "mild", "moderate", "intense"], None)
    await add_entity("select", sel)

    num = FakeNumber("number.level", 0.0, 10.0)
    num_small = FakeNumber("number.small", 0.0, 1.0)
    await add_entity("number", num)
    await add_entity("number", num_small)

    all_cov = (
        CoverEntityFeature.OPEN
        | CoverEntityFeature.CLOSE
        | CoverEntityFeature.SET_POSITION
        | CoverEntityFeature.STOP
        | CoverEntityFeature.OPEN_TILT
        | CoverEntityFeature.CLOSE_TILT
        | CoverEntityFeature.STOP_TILT
        | CoverEntityFeature.SET_TILT_POSITION
    )
    cov = FakeCover("cover.blinds", all_cov)
    cov_open = FakeCover("cover.open_only", CoverEntityFeature.OPEN)
    await add_entity("cover", cov)
    await add_entity("cover", cov_open)

    lck = FakeLock("lock.front", LockEntityFeature.OPEN, r"^\d{4}$")
    lck_def = FakeLock("lock.side", LockEntityFeature(0), r"^\d{4}$", "4321")
    await add_entity("lock", lck)
    await add_entity("lock", lck_def)

    all_fan = (
        FanEntityFeature.SET_SPEED
        | FanEntityFeature.OSCILLATE
        | FanEntityFeature.DIRECTION
        | FanEntityFeature.PRESET_MODE
        | FanEntityFeature.TURN_OFF
        | FanEntityFeature.TURN_ON
    )
    f = FakeFan("fan.bedroom", all_fan, speed_count=3, preset_modes=["auto", "sleep", "breeze"])
    f_basic = FakeFan("fan.basic", FanEntityFeature.TURN_ON | FanEntityFeature.TURN_OFF)
    await add_entity("fan", f)
    await add_entity("fan", f_basic)

    out: dict[str, Any] = {
        "ha_version": HA_VERSION,
        "unrecorded": {
            "select": sorted(SelectEntity._entity_component_unrecorded_attributes),
            "number": sorted(NumberEntity._entity_component_unrecorded_attributes),
            "cover": sorted(CoverEntity._entity_component_unrecorded_attributes),
            "lock": sorted(LockEntity._entity_component_unrecorded_attributes),
            "fan": sorted(FanEntity._entity_component_unrecorded_attributes),
        },
        "number_steps": {
            "0_10": num.step,
            "0_1": num_small.step,
        },
        "select": [],
        "number": [],
        "cover": [],
        "lock": [],
        "fan": [],
    }

    # --- select ---
    for svc, data in [
        ("select_next", {"entity_id": "select.mode"}),
        ("select_next", {"entity_id": "select.mode"}),
        ("select_last", {"entity_id": "select.mode"}),
        ("select_next", {"entity_id": "select.mode", "cycle": True}),
        ("select_previous", {"entity_id": "select.mode", "cycle": False}),
        ("select_previous", {"entity_id": "select.mode", "cycle": True}),
        ("select_first", {"entity_id": "select.mode"}),
        ("select_option", {"entity_id": "select.mode", "option": "moderate"}),
        ("select_option", {"entity_id": "select.mode", "option": "invalid"}),
        ("select_next", {"entity_id": "select.mode", "cycle": "true"}),
    ]:
        out["select"].append(await record_call(hass, "select", svc, data, sel.calls))

    # --- number ---
    for svc, data in [
        ("set_value", {"entity_id": "number.level", "value": 0}),
        ("set_value", {"entity_id": "number.level", "value": "7.5"}),
        ("set_value", {"entity_id": "number.level", "value": 10}),
        ("set_value", {"entity_id": "number.level", "value": 11}),
        ("set_value", {"entity_id": "number.level", "value": -1.5}),
        ("set_value", {"entity_id": "number.level", "value": "bad"}),
    ]:
        out["number"].append(await record_call(hass, "number", svc, data, num.calls))

    # --- cover ---
    for svc, data in [
        ("open_cover", {"entity_id": "cover.blinds"}),
        ("close_cover", {"entity_id": "cover.blinds"}),
        ("stop_cover", {"entity_id": "cover.blinds"}),
        ("set_cover_position", {"entity_id": "cover.blinds", "position": 55}),
        ("set_cover_position", {"entity_id": "cover.blinds", "position": 101}),
        ("open_cover_tilt", {"entity_id": "cover.blinds"}),
        ("close_cover_tilt", {"entity_id": "cover.blinds"}),
        ("stop_cover_tilt", {"entity_id": "cover.blinds"}),
        ("set_cover_tilt_position", {"entity_id": "cover.blinds", "tilt_position": 25}),
        ("toggle", {"entity_id": "cover.blinds"}),
        ("toggle_cover_tilt", {"entity_id": "cover.blinds"}),
        ("toggle", {"entity_id": "cover.open_only"}),
    ]:
        target_calls = cov_open.calls if data["entity_id"] == "cover.open_only" else cov.calls
        out["cover"].append(await record_call(hass, "cover", svc, data, target_calls))

    # --- lock ---
    for svc, data in [
        ("unlock", {"entity_id": "lock.front"}),
        ("unlock", {"entity_id": "lock.front", "code": "12"}),
        ("lock", {"entity_id": "lock.front", "code": "1234"}),
        ("unlock", {"entity_id": "lock.front", "code": "1234"}),
        ("open", {"entity_id": "lock.front", "code": "1234"}),
        ("unlock", {"entity_id": "lock.side"}),
        ("open", {"entity_id": "lock.side"}),
    ]:
        target_calls = lck_def.calls if data["entity_id"] == "lock.side" else lck.calls
        out["lock"].append(await record_call(hass, "lock", svc, data, target_calls))

    # --- fan ---
    for svc, data in [
        ("turn_on", {"entity_id": "fan.bedroom", "percentage": 33, "preset_mode": "auto"}),
        ("increase_speed", {"entity_id": "fan.bedroom"}),
        ("increase_speed", {"entity_id": "fan.bedroom"}),
        ("increase_speed", {"entity_id": "fan.bedroom"}),
        ("decrease_speed", {"entity_id": "fan.bedroom"}),
        ("decrease_speed", {"entity_id": "fan.bedroom", "percentage_step": 16}),
        ("oscillate", {"entity_id": "fan.bedroom", "oscillating": "on"}),
        ("set_direction", {"entity_id": "fan.bedroom", "direction": "reverse"}),
        ("set_preset_mode", {"entity_id": "fan.bedroom", "preset_mode": "sleep"}),
        ("set_preset_mode", {"entity_id": "fan.bedroom", "preset_mode": "nope"}),
        ("turn_on", {"entity_id": "fan.bedroom", "preset_mode": "nope"}),
        ("toggle", {"entity_id": "fan.bedroom"}),
        ("toggle", {"entity_id": "fan.bedroom"}),
        ("oscillate", {"entity_id": "fan.basic", "oscillating": True}),
    ]:
        target_calls = f_basic.calls if data["entity_id"] == "fan.basic" else f.calls
        out["fan"].append(await record_call(hass, "fan", svc, data, target_calls))

    await hass.async_stop()
    return out


if __name__ == "__main__":
    assert HA_VERSION == "2026.6.1", f"expected HA 2026.6.1, got {HA_VERSION}"
    data = asyncio.run(generate())
    json.dump(data, sys.stdout, indent=2, sort_keys=False)
    sys.stdout.write("\n")

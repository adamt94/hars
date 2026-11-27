#!/usr/bin/env python3
"""Generate HA 2026.6.1 vectors for climate (all 10 services) and notify.

Run with a Python environment that has Home Assistant 2026.6.1 installed:

    /path/to/havenv/bin/python scripts/vectors/climate_notify.py \
        > crates/ha-integrations/tests/vectors/climate_notify.json
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
import tempfile
from typing import Any

# scripts/vectors/http.py shadows stdlib `http` when invoked directly without -P.
_SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
sys.path = [p for p in sys.path if os.path.abspath(p or ".") != _SCRIPT_DIR]

import voluptuous as vol

from homeassistant import bootstrap, loader
from homeassistant.components.climate import (
    ClimateEntity,
    ClimateEntityFeature,
    HVACMode,
    async_setup as setup_climate,
)
from homeassistant.components.notify import (
    NotifyEntity,
    NotifyEntityFeature,
    async_setup as setup_notify,
)
from homeassistant.config_entries import ConfigEntries
from homeassistant.const import UnitOfTemperature, __version__ as HA_VERSION
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError, ServiceValidationError
from homeassistant.helpers import translation
from homeassistant.helpers.entity_component import EntityComponent


class FakeClimate(ClimateEntity):
    _attr_temperature_unit = UnitOfTemperature.CELSIUS
    _attr_target_temperature = None
    _attr_target_temperature_high = None
    _attr_target_temperature_low = None
    _attr_target_humidity = None
    _attr_fan_mode = None
    _attr_preset_mode = None
    _attr_swing_mode = None
    _attr_swing_horizontal_mode = None

    def __init__(
        self,
        entity_id: str,
        features: ClimateEntityFeature,
        hvac_modes: list[HVACMode],
        hvac_mode: HVACMode | None = HVACMode.OFF,
        fan_modes: list[str] | None = None,
        preset_modes: list[str] | None = None,
        swing_modes: list[str] | None = None,
        swing_horizontal_modes: list[str] | None = None,
        min_temp: float = 16,
        max_temp: float = 30,
        min_humidity: float = 30,
        max_humidity: float = 80,
    ) -> None:
        self.entity_id = entity_id
        self._attr_supported_features = features
        self._attr_hvac_modes = hvac_modes
        self._attr_hvac_mode = hvac_mode
        self._attr_fan_modes = fan_modes
        self._attr_preset_modes = preset_modes
        self._attr_swing_modes = swing_modes
        self._attr_swing_horizontal_modes = swing_horizontal_modes
        self._attr_min_temp = min_temp
        self._attr_max_temp = max_temp
        self._attr_min_humidity = min_humidity
        self._attr_max_humidity = max_humidity
        self.calls: list[Any] = []

    async def async_set_hvac_mode(self, hvac_mode: HVACMode) -> None:
        self.calls.append(["set_hvac_mode", hvac_mode.value])

    async def async_set_temperature(self, **kwargs: Any) -> None:
        clean = {
            k: (v.value if hasattr(v, "value") else v) for k, v in kwargs.items()
        }
        self.calls.append(["set_temperature", clean])

    async def async_set_humidity(self, humidity: int) -> None:
        self.calls.append(["set_humidity", humidity])

    async def async_set_fan_mode(self, fan_mode: str) -> None:
        self.calls.append(["set_fan_mode", fan_mode])

    async def async_set_preset_mode(self, preset_mode: str) -> None:
        self.calls.append(["set_preset_mode", preset_mode])

    async def async_set_swing_mode(self, swing_mode: str) -> None:
        self.calls.append(["set_swing_mode", swing_mode])

    async def async_set_swing_horizontal_mode(self, swing_horizontal_mode: str) -> None:
        self.calls.append(["set_swing_horizontal_mode", swing_horizontal_mode])


class FakeNotify(NotifyEntity):
    def __init__(self, entity_id: str, features: NotifyEntityFeature) -> None:
        self.entity_id = entity_id
        self._attr_supported_features = features
        self.calls: list[Any] = []

    def send_message(self, message: str, title: str | None = None) -> None:
        self.calls.append(["send_message", message, title])


async def run_call(
    hass: HomeAssistant,
    domain: str,
    service: str,
    data: dict[str, Any],
    entities: list[Any],
) -> dict[str, Any]:
    for e in entities:
        e.calls.clear()
    try:
        await hass.services.async_call(domain, service, dict(data), blocking=True)
    except ServiceValidationError as err:
        return {
            "error": {
                "kind": "validation",
                "domain": err.translation_domain,
                "key": err.translation_key,
                "placeholders": err.translation_placeholders,
                "message": str(err),
            }
        }
    except vol.Invalid as err:
        return {"error": {"kind": "invalid", "message": str(err)}}
    except HomeAssistantError as err:
        return {
            "error": {
                "kind": "translated" if err.translation_key else "failed",
                "domain": err.translation_domain,
                "key": err.translation_key,
                "placeholders": err.translation_placeholders,
                "message": str(err),
            }
        }
    except Exception as err:  # noqa: BLE001
        return {
            "error": {
                "kind": "unknown",
                "type": type(err).__name__,
                "message": str(err),
            }
        }
    return {"calls": {e.entity_id: list(e.calls) for e in entities if e.calls}}


async def main() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        hass = HomeAssistant(tmpdir)
        hass.config.skip_pip = True
        await hass.config.async_load()
        loader.async_setup(hass)
        hass.config_entries = ConfigEntries(hass, {})
        await hass.config_entries.async_initialize()
        await bootstrap.async_load_base_functionality(hass)
        await translation.async_load_integrations(
            hass, {"homeassistant", "climate", "notify"}
        )
        await setup_climate(hass, {})
        await setup_notify(hass, {})

        all_climate = (
            ClimateEntityFeature.TARGET_TEMPERATURE
            | ClimateEntityFeature.TARGET_TEMPERATURE_RANGE
            | ClimateEntityFeature.TARGET_HUMIDITY
            | ClimateEntityFeature.FAN_MODE
            | ClimateEntityFeature.PRESET_MODE
            | ClimateEntityFeature.SWING_MODE
            | ClimateEntityFeature.SWING_HORIZONTAL_MODE
            | ClimateEntityFeature.TURN_OFF
            | ClimateEntityFeature.TURN_ON
        )
        clim_full = FakeClimate(
            "climate.living_room",
            all_climate,
            hvac_modes=[HVACMode.OFF, HVACMode.HEAT, HVACMode.COOL, HVACMode.AUTO],
            hvac_mode=HVACMode.OFF,
            fan_modes=["auto", "low", "high"],
            preset_modes=["eco", "boost", "away"],
            swing_modes=["off", "vertical"],
            swing_horizontal_modes=["off", "horizontal"],
            min_temp=16,
            max_temp=30,
            min_humidity=30,
            max_humidity=80,
        )
        clim_minimal = FakeClimate(
            "climate.minimal",
            ClimateEntityFeature(0),
            hvac_modes=[HVACMode.OFF, HVACMode.HEAT],
            hvac_mode=HVACMode.HEAT,
        )
        clim_entities = [clim_full, clim_minimal]
        clim_comp: EntityComponent[ClimateEntity] = hass.data["climate"]
        await clim_comp.async_add_entities(clim_entities)

        notify_with_title = FakeNotify("notify.telegram", NotifyEntityFeature.TITLE)
        notify_no_title = FakeNotify("notify.plain", NotifyEntityFeature(0))
        notify_entities = [notify_with_title, notify_no_title]
        notify_comp: EntityComponent[NotifyEntity] = hass.data["notify"]
        await notify_comp.async_add_entities(notify_entities)

        all_entities: list[Any] = clim_entities + notify_entities
        cases: list[dict[str, Any]] = []

        async def record(
            name: str, domain: str, service: str, data: dict[str, Any]
        ) -> None:
            result = await run_call(hass, domain, service, data, all_entities)
            cases.append(
                {
                    "name": name,
                    "domain": domain,
                    "service": service,
                    "data": data,
                    **result,
                }
            )

        # --- climate ---
        await record("climate_set_preset_mode_ok", "climate", "set_preset_mode", {"entity_id": "climate.living_room", "preset_mode": "eco"})
        await record("climate_set_preset_mode_invalid", "climate", "set_preset_mode", {"entity_id": "climate.living_room", "preset_mode": "turbo"})
        await record("climate_set_preset_mode_unsupported", "climate", "set_preset_mode", {"entity_id": "climate.minimal", "preset_mode": "eco"})
        await record("climate_set_humidity_ok", "climate", "set_humidity", {"entity_id": "climate.living_room", "humidity": 55})
        await record("climate_set_humidity_low", "climate", "set_humidity", {"entity_id": "climate.living_room", "humidity": 20})
        await record("climate_set_humidity_high", "climate", "set_humidity", {"entity_id": "climate.living_room", "humidity": 90})
        await record("climate_set_humidity_unsupported", "climate", "set_humidity", {"entity_id": "climate.minimal", "humidity": 50})
        await record("climate_set_swing_mode_ok", "climate", "set_swing_mode", {"entity_id": "climate.living_room", "swing_mode": "vertical"})
        await record("climate_set_swing_mode_invalid", "climate", "set_swing_mode", {"entity_id": "climate.living_room", "swing_mode": "both"})
        await record("climate_set_swing_mode_unsupported", "climate", "set_swing_mode", {"entity_id": "climate.minimal", "swing_mode": "vertical"})
        await record("climate_set_swing_horizontal_mode_ok", "climate", "set_swing_horizontal_mode", {"entity_id": "climate.living_room", "swing_horizontal_mode": "horizontal"})
        await record("climate_set_swing_horizontal_mode_invalid", "climate", "set_swing_horizontal_mode", {"entity_id": "climate.living_room", "swing_horizontal_mode": "wide"})
        await record("climate_set_swing_horizontal_mode_unsupported", "climate", "set_swing_horizontal_mode", {"entity_id": "climate.minimal", "swing_horizontal_mode": "horizontal"})
        await record("climate_set_temperature_range_ok", "climate", "set_temperature", {"entity_id": "climate.living_room", "target_temp_low": 19, "target_temp_high": 24, "hvac_mode": "auto"})
        await record("climate_set_temperature_range_inverted", "climate", "set_temperature", {"entity_id": "climate.living_room", "target_temp_low": 25, "target_temp_high": 20})
        await record("climate_set_hvac_mode_ok", "climate", "set_hvac_mode", {"entity_id": "climate.living_room", "hvac_mode": "heat"})
        await record("climate_set_fan_mode_ok", "climate", "set_fan_mode", {"entity_id": "climate.living_room", "fan_mode": "high"})
        await record("climate_turn_on", "climate", "turn_on", {"entity_id": "climate.living_room"})
        await record("climate_turn_off", "climate", "turn_off", {"entity_id": "climate.living_room"})
        await record("climate_toggle_off_turns_on", "climate", "toggle", {"entity_id": "climate.living_room"})

        # --- notify ---
        await record("notify_send_message_with_title_supported", "notify", "send_message", {"entity_id": "notify.telegram", "message": "Hello", "title": "Alert"})
        await record("notify_send_message_without_title", "notify", "send_message", {"entity_id": "notify.telegram", "message": "Hello"})
        await record("notify_send_message_strips_title_when_unsupported", "notify", "send_message", {"entity_id": "notify.plain", "message": "Hello", "title": "Ignored"})
        await record("notify_send_message_missing_message", "notify", "send_message", {"entity_id": "notify.telegram"})

        out = {
            "ha_version": HA_VERSION,
            "capability_attributes": {
                "climate_full": clim_full.capability_attributes,
                "notify_with_title": notify_with_title.capability_attributes,
            },
            "notify_state_is_timestamp": bool(
                (st := hass.states.get("notify.telegram")) is not None
                and st.state not in ("unknown", "unavailable")
                and "T" in st.state
            ),
            "cases": cases,
        }
        print(json.dumps(out, indent=2))
        await hass.async_stop()


if __name__ == "__main__":
    asyncio.run(main())

#!/usr/bin/env python3
"""Generate HA 2026.6.1 vectors for media_player and remote.

Run with a Python environment that has Home Assistant 2026.6.1 installed:

    /path/to/havenv/bin/python scripts/vectors/media_player_remote.py \
        > crates/ha-integrations/tests/vectors/media_player_remote.json
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

from homeassistant.components.media_player import (
    MediaPlayerEntity,
    MediaPlayerEntityFeature,
    MediaPlayerState,
    RepeatMode,
    async_setup as setup_media_player,
)
from homeassistant.components.remote import (
    RemoteEntity,
    RemoteEntityFeature,
    async_setup as setup_remote,
)
from homeassistant import bootstrap, loader
from homeassistant.config_entries import ConfigEntries
from homeassistant.const import __version__ as HA_VERSION
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError, ServiceValidationError
from homeassistant.helpers import translation
from homeassistant.helpers.entity_component import EntityComponent


class FakeMediaPlayer(MediaPlayerEntity):
    def __init__(
        self,
        entity_id: str,
        features: MediaPlayerEntityFeature,
        state: MediaPlayerState | None = MediaPlayerState.IDLE,
        volume_level: float | None = 0.5,
        volume_step: float = 0.1,
        source_list: list[str] | None = None,
        sound_mode_list: list[str] | None = None,
    ) -> None:
        self.entity_id = entity_id
        self._attr_supported_features = features
        self._attr_state = state
        self._attr_volume_level = volume_level
        self._attr_volume_step = volume_step
        self._attr_source_list = source_list
        self._attr_sound_mode_list = sound_mode_list
        self.calls: list[Any] = []

    async def async_turn_on(self) -> None:
        self.calls.append(["turn_on"])

    async def async_turn_off(self) -> None:
        self.calls.append(["turn_off"])

    async def async_mute_volume(self, mute: bool) -> None:
        self.calls.append(["mute_volume", mute])

    async def async_set_volume_level(self, volume: float) -> None:
        self.calls.append(["set_volume_level", round(volume, 6)])

    async def async_media_play(self) -> None:
        self.calls.append(["media_play"])

    async def async_media_pause(self) -> None:
        self.calls.append(["media_pause"])

    async def async_media_stop(self) -> None:
        self.calls.append(["media_stop"])

    async def async_media_previous_track(self) -> None:
        self.calls.append(["media_previous_track"])

    async def async_media_next_track(self) -> None:
        self.calls.append(["media_next_track"])

    async def async_media_seek(self, position: float) -> None:
        self.calls.append(["media_seek", position])

    async def async_play_media(
        self, media_type: str, media_id: str, **kwargs: Any
    ) -> None:
        clean = {
            k: (v.value if hasattr(v, "value") else v) for k, v in kwargs.items()
        }
        self.calls.append(["play_media", media_type, media_id, clean])

    async def async_select_source(self, source: str) -> None:
        self.calls.append(["select_source", source])

    async def async_select_sound_mode(self, sound_mode: str) -> None:
        self.calls.append(["select_sound_mode", sound_mode])

    async def async_clear_playlist(self) -> None:
        self.calls.append(["clear_playlist"])

    async def async_set_shuffle(self, shuffle: bool) -> None:
        self.calls.append(["set_shuffle", shuffle])

    async def async_set_repeat(self, repeat: RepeatMode) -> None:
        self.calls.append(["set_repeat", repeat.value])

    async def async_join_players(self, group_members: list[str]) -> None:
        self.calls.append(["join_players", list(group_members)])

    async def async_unjoin_player(self) -> None:
        self.calls.append(["unjoin_player"])


class StepOnlyMediaPlayer(FakeMediaPlayer):
    async def async_volume_up(self) -> None:
        self.calls.append(["volume_up"])

    async def async_volume_down(self) -> None:
        self.calls.append(["volume_down"])


class FakeRemote(RemoteEntity):
    def __init__(
        self,
        entity_id: str,
        is_on: bool = False,
        features: RemoteEntityFeature = RemoteEntityFeature(0),
        activity_list: list[str] | None = None,
        current_activity: str | None = None,
    ) -> None:
        self.entity_id = entity_id
        self._attr_is_on = is_on
        self._attr_supported_features = features
        self._attr_activity_list = activity_list
        self._attr_current_activity = current_activity
        self.calls: list[Any] = []

    async def async_turn_on(self, **kwargs: Any) -> None:
        self.calls.append(["turn_on", dict(kwargs)])

    async def async_turn_off(self, **kwargs: Any) -> None:
        self.calls.append(["turn_off", dict(kwargs)])

    async def async_send_command(self, command: Any, **kwargs: Any) -> None:
        self.calls.append(["send_command", list(command), dict(kwargs)])

    async def async_learn_command(self, **kwargs: Any) -> None:
        self.calls.append(["learn_command", dict(kwargs)])

    async def async_delete_command(self, **kwargs: Any) -> None:
        self.calls.append(["delete_command", dict(kwargs)])


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
            hass, {"homeassistant", "media_player", "remote"}
        )
        # Stub minimal http hook so media_player.async_setup runs standalone.
        hass.http = type("StubHttp", (), {"register_view": lambda self, v: None})()  # type: ignore[assignment]
        await setup_media_player(hass, {})
        await setup_remote(hass, {})

        all_mp = (
            MediaPlayerEntityFeature.PAUSE
            | MediaPlayerEntityFeature.SEEK
            | MediaPlayerEntityFeature.VOLUME_SET
            | MediaPlayerEntityFeature.VOLUME_MUTE
            | MediaPlayerEntityFeature.PREVIOUS_TRACK
            | MediaPlayerEntityFeature.NEXT_TRACK
            | MediaPlayerEntityFeature.TURN_ON
            | MediaPlayerEntityFeature.TURN_OFF
            | MediaPlayerEntityFeature.PLAY_MEDIA
            | MediaPlayerEntityFeature.SELECT_SOURCE
            | MediaPlayerEntityFeature.STOP
            | MediaPlayerEntityFeature.CLEAR_PLAYLIST
            | MediaPlayerEntityFeature.PLAY
            | MediaPlayerEntityFeature.SHUFFLE_SET
            | MediaPlayerEntityFeature.SELECT_SOUND_MODE
            | MediaPlayerEntityFeature.REPEAT_SET
            | MediaPlayerEntityFeature.GROUPING
        )
        mp_full = FakeMediaPlayer(
            "media_player.living_room",
            all_mp,
            state=MediaPlayerState.PLAYING,
            volume_level=0.5,
            volume_step=0.1,
            source_list=["HDMI 1", "Spotify"],
            sound_mode_list=["Movie", "Music"],
        )
        mp_off = FakeMediaPlayer(
            "media_player.bedroom",
            MediaPlayerEntityFeature.TURN_ON
            | MediaPlayerEntityFeature.TURN_OFF
            | MediaPlayerEntityFeature.PLAY
            | MediaPlayerEntityFeature.PAUSE
            | MediaPlayerEntityFeature.VOLUME_SET,
            state=MediaPlayerState.OFF,
            volume_level=0.95,
            volume_step=0.1,
        )
        mp_standby = FakeMediaPlayer(
            "media_player.den",
            MediaPlayerEntityFeature.TURN_ON
            | MediaPlayerEntityFeature.TURN_OFF
            | MediaPlayerEntityFeature.VOLUME_SET,
            state="standby",  # type: ignore[arg-type]
            volume_level=0.05,
            volume_step=0.1,
        )
        mp_turn_on_only = FakeMediaPlayer(
            "media_player.turn_on_only",
            MediaPlayerEntityFeature.TURN_ON | MediaPlayerEntityFeature.PLAY,
            state=MediaPlayerState.OFF,
            volume_level=None,
        )
        mp_step_only = StepOnlyMediaPlayer(
            "media_player.step_only",
            MediaPlayerEntityFeature.VOLUME_STEP,
            state=MediaPlayerState.IDLE,
            volume_level=None,
        )
        mp_none_vol = FakeMediaPlayer(
            "media_player.none_vol",
            MediaPlayerEntityFeature.VOLUME_SET,
            state=MediaPlayerState.IDLE,
            volume_level=None,
        )
        mp_edge_vol = FakeMediaPlayer(
            "media_player.edge_vol",
            MediaPlayerEntityFeature.VOLUME_SET,
            state=MediaPlayerState.IDLE,
            volume_level=1.0,
        )
        mp_zero_vol = FakeMediaPlayer(
            "media_player.zero_vol",
            MediaPlayerEntityFeature.VOLUME_SET,
            state=MediaPlayerState.IDLE,
            volume_level=0.0,
        )

        mp_entities = [
            mp_full,
            mp_off,
            mp_standby,
            mp_turn_on_only,
            mp_step_only,
            mp_none_vol,
            mp_edge_vol,
            mp_zero_vol,
        ]
        mp_comp: EntityComponent[MediaPlayerEntity] = hass.data["media_player"]
        await mp_comp.async_add_entities(mp_entities)

        rem_off = FakeRemote(
            "remote.living_room",
            is_on=False,
            features=RemoteEntityFeature.ACTIVITY | RemoteEntityFeature.LEARN_COMMAND,
            activity_list=["Watch TV", "Listen"],
            current_activity=None,
        )
        rem_on = FakeRemote(
            "remote.bedroom",
            is_on=True,
            features=RemoteEntityFeature(0),
        )
        rem_entities = [rem_off, rem_on]
        rem_comp: EntityComponent[RemoteEntity] = hass.data["remote"]
        await rem_comp.async_add_entities(rem_entities)

        all_entities: list[Any] = mp_entities + rem_entities
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

        # --- media_player ---
        await record("mp_turn_on", "media_player", "turn_on", {"entity_id": "media_player.living_room"})
        await record("mp_turn_off", "media_player", "turn_off", {"entity_id": "media_player.living_room"})
        await record("mp_toggle_playing_turns_off", "media_player", "toggle", {"entity_id": "media_player.living_room"})
        await record("mp_toggle_off_turns_on", "media_player", "toggle", {"entity_id": "media_player.bedroom"})
        await record("mp_toggle_standby_turns_on", "media_player", "toggle", {"entity_id": "media_player.den"})
        await record("mp_toggle_needs_both_turn_on_and_off", "media_player", "toggle", {"entity_id": "media_player.turn_on_only"})
        await record("mp_play_pause_playing_pauses", "media_player", "media_play_pause", {"entity_id": "media_player.living_room"})
        await record("mp_play_pause_off_plays", "media_player", "media_play_pause", {"entity_id": "media_player.bedroom"})
        await record("mp_play_pause_needs_both_play_and_pause", "media_player", "media_play_pause", {"entity_id": "media_player.turn_on_only"})
        await record("mp_media_play", "media_player", "media_play", {"entity_id": "media_player.living_room"})
        await record("mp_media_pause", "media_player", "media_pause", {"entity_id": "media_player.living_room"})
        await record("mp_media_stop", "media_player", "media_stop", {"entity_id": "media_player.living_room"})
        await record("mp_media_next_track", "media_player", "media_next_track", {"entity_id": "media_player.living_room"})
        await record("mp_media_previous_track", "media_player", "media_previous_track", {"entity_id": "media_player.living_room"})
        await record("mp_clear_playlist", "media_player", "clear_playlist", {"entity_id": "media_player.living_room"})
        await record("mp_volume_up_steps_volume_set", "media_player", "volume_up", {"entity_id": "media_player.living_room"})
        await record("mp_volume_up_clamps_to_1", "media_player", "volume_up", {"entity_id": "media_player.bedroom"})
        await record("mp_volume_up_noop_at_1", "media_player", "volume_up", {"entity_id": "media_player.edge_vol"})
        await record("mp_volume_up_noop_when_none", "media_player", "volume_up", {"entity_id": "media_player.none_vol"})
        await record("mp_volume_up_step_only", "media_player", "volume_up", {"entity_id": "media_player.step_only"})
        await record("mp_volume_up_unsupported", "media_player", "volume_up", {"entity_id": "media_player.turn_on_only"})
        await record("mp_volume_down_steps_volume_set", "media_player", "volume_down", {"entity_id": "media_player.living_room"})
        await record("mp_volume_down_clamps_to_0", "media_player", "volume_down", {"entity_id": "media_player.den"})
        await record("mp_volume_down_noop_at_0", "media_player", "volume_down", {"entity_id": "media_player.zero_vol"})
        await record("mp_volume_down_step_only", "media_player", "volume_down", {"entity_id": "media_player.step_only"})
        await record("mp_volume_set_valid", "media_player", "volume_set", {"entity_id": "media_player.living_room", "volume_level": "0.25"})
        await record("mp_volume_set_too_high", "media_player", "volume_set", {"entity_id": "media_player.living_room", "volume_level": 1.1})
        await record("mp_volume_set_negative", "media_player", "volume_set", {"entity_id": "media_player.living_room", "volume_level": -0.1})
        await record("mp_volume_mute", "media_player", "volume_mute", {"entity_id": "media_player.living_room", "is_volume_muted": "on"})
        await record("mp_media_seek_valid", "media_player", "media_seek", {"entity_id": "media_player.living_room", "seek_position": 42.5})
        await record("mp_media_seek_zero", "media_player", "media_seek", {"entity_id": "media_player.living_room", "seek_position": 0})
        await record("mp_media_seek_negative", "media_player", "media_seek", {"entity_id": "media_player.living_room", "seek_position": -1})
        await record("mp_select_source", "media_player", "select_source", {"entity_id": "media_player.living_room", "source": "Anything"})
        await record("mp_select_sound_mode", "media_player", "select_sound_mode", {"entity_id": "media_player.living_room", "sound_mode": "Movie"})
        await record("mp_shuffle_set", "media_player", "shuffle_set", {"entity_id": "media_player.living_room", "shuffle": True})
        await record("mp_repeat_set_valid", "media_player", "repeat_set", {"entity_id": "media_player.living_room", "repeat": "one"})
        await record("mp_repeat_set_invalid", "media_player", "repeat_set", {"entity_id": "media_player.living_room", "repeat": "twice"})
        await record("mp_join_single_string_lowercased", "media_player", "join", {"entity_id": "media_player.living_room", "group_members": "media_player.Bedroom"})
        await record("mp_join_list", "media_player", "join", {"entity_id": "media_player.living_room", "group_members": ["media_player.bedroom", "media_player.den"]})
        await record("mp_join_invalid_entity_id", "media_player", "join", {"entity_id": "media_player.living_room", "group_members": ["not_an_id"]})
        await record("mp_unjoin", "media_player", "unjoin", {"entity_id": "media_player.living_room"})
        await record("mp_play_media_basic", "media_player", "play_media", {"entity_id": "media_player.living_room", "media_content_type": "music", "media_content_id": "http://example.com/a.mp3"})
        await record("mp_play_media_promote_media_dict", "media_player", "play_media", {"entity_id": "media_player.living_room", "media": {"media_content_type": "music", "media_content_id": "http://example.com/b.mp3"}})
        await record("mp_play_media_conflict_media_and_id", "media_player", "play_media", {"entity_id": "media_player.living_room", "media": {"media_content_type": "music", "media_content_id": "a"}, "media_content_id": "b"})
        await record("mp_play_media_enqueue_true", "media_player", "play_media", {"entity_id": "media_player.living_room", "media_content_type": "music", "media_content_id": "a", "enqueue": True})
        await record("mp_play_media_enqueue_false", "media_player", "play_media", {"entity_id": "media_player.living_room", "media_content_type": "music", "media_content_id": "a", "enqueue": False})
        await record("mp_play_media_enqueue_enum", "media_player", "play_media", {"entity_id": "media_player.living_room", "media_content_type": "music", "media_content_id": "a", "enqueue": "next"})
        await record("mp_play_media_announce_and_extra", "media_player", "play_media", {"entity_id": "media_player.living_room", "media_content_type": "music", "media_content_id": "a", "announce": True, "extra": {"thumb": "x"}})
        await record("mp_play_media_enqueue_and_announce_exclusive", "media_player", "play_media", {"entity_id": "media_player.living_room", "media_content_type": "music", "media_content_id": "a", "enqueue": "add", "announce": True})
        await record("mp_play_media_bad_extra", "media_player", "play_media", {"entity_id": "media_player.living_room", "media_content_type": "music", "media_content_id": "a", "extra": "nope"})

        # --- remote ---
        await record("remote_turn_on_no_activity", "remote", "turn_on", {"entity_id": "remote.living_room"})
        await record("remote_turn_on_with_activity", "remote", "turn_on", {"entity_id": "remote.living_room", "activity": "Watch TV"})
        await record("remote_turn_off_with_activity", "remote", "turn_off", {"entity_id": "remote.bedroom", "activity": "Sleep"})
        await record("remote_toggle_off_turns_on", "remote", "toggle", {"entity_id": "remote.living_room", "activity": "Listen"})
        await record("remote_toggle_on_turns_off", "remote", "toggle", {"entity_id": "remote.bedroom"})
        await record("remote_send_command_single_string_defaults", "remote", "send_command", {"entity_id": "remote.living_room", "command": "Power"})
        await record("remote_send_command_list_all_opts", "remote", "send_command", {"entity_id": "remote.living_room", "command": ["VolUp", "VolUp"], "device": "tv", "num_repeats": 3, "delay_secs": 0.25, "hold_secs": 1.5})
        await record("remote_send_command_negative_repeats", "remote", "send_command", {"entity_id": "remote.living_room", "command": "Power", "num_repeats": -1})
        await record("remote_learn_command_no_feature_required", "remote", "learn_command", {"entity_id": "remote.bedroom", "device": "tv", "command": "Mute", "command_type": "ir", "alternative": True, "timeout": 30})
        await record("remote_learn_command_empty_opts", "remote", "learn_command", {"entity_id": "remote.bedroom"})
        await record("remote_delete_command_no_feature_required", "remote", "delete_command", {"entity_id": "remote.bedroom", "command": "Mute", "device": "tv"})
        await record("remote_delete_command_missing_command", "remote", "delete_command", {"entity_id": "remote.bedroom"})

        out = {
            "ha_version": HA_VERSION,
            "unrecorded_attributes": {
                "media_player": sorted(MediaPlayerEntity._entity_component_unrecorded_attributes),
                "remote": sorted(RemoteEntity._entity_component_unrecorded_attributes),
            },
            "capability_attributes": {
                "media_player_full": mp_full.capability_attributes,
                "remote_with_activity": rem_off.capability_attributes,
            },
            "cases": cases,
        }
        print(json.dumps(out, indent=2))
        await hass.async_stop()


if __name__ == "__main__":
    asyncio.run(main())

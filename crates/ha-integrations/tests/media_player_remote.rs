//! Replays Home Assistant 2026.6.1 vectors for `media_player` and `remote`
//! (`scripts/vectors/media_player_remote.py`).

use ha_core::{EntityId, Hass, ServiceError};
use ha_integrations::light::BoxFuture;
use ha_integrations::media_player::{MediaPlayer, MediaPlayers, PlayMedia, feature as mp_feat};
use ha_integrations::remote::{DeleteCommand, LearnCommand, Remote, Remotes, SendCommand};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::sync::Arc;

struct FakePlayer {
    id: EntityId,
    features: u32,
    state: Option<String>,
    volume_level: Option<f64>,
    volume_step: f64,
    step_override: bool,
    calls: Mutex<Vec<Value>>,
}

impl FakePlayer {
    fn new(
        id: &str,
        features: u32,
        state: Option<&str>,
        volume_level: Option<f64>,
        volume_step: f64,
        step_override: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            id: EntityId::parse(id).unwrap(),
            features,
            state: state.map(str::to_owned),
            volume_level,
            volume_step,
            step_override,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn push(&self, v: Value) -> BoxFuture<'_> {
        self.calls.lock().push(v);
        Box::pin(async { Ok(()) })
    }
}

impl MediaPlayer for FakePlayer {
    fn entity_id(&self) -> &EntityId {
        &self.id
    }
    fn available(&self) -> bool {
        true
    }
    fn supported_features(&self) -> u32 {
        self.features
    }
    fn state(&self) -> Option<String> {
        self.state.clone()
    }
    fn volume_info(&self) -> (Option<f64>, f64) {
        (self.volume_level, self.volume_step)
    }
    fn turn_on(&self) -> BoxFuture<'_> {
        self.push(json!(["turn_on"]))
    }
    fn turn_off(&self) -> BoxFuture<'_> {
        self.push(json!(["turn_off"]))
    }
    fn volume_up(&self) -> BoxFuture<'_> {
        if self.step_override {
            return self.push(json!(["volume_up"]));
        }
        let (level, step) = self.volume_info();
        if let Some(v) = level
            && v < 1.0
            && (self.features & mp_feat::VOLUME_SET) != 0
        {
            return self.set_volume_level((v + step).min(1.0));
        }
        Box::pin(async { Ok(()) })
    }
    fn volume_down(&self) -> BoxFuture<'_> {
        if self.step_override {
            return self.push(json!(["volume_down"]));
        }
        let (level, step) = self.volume_info();
        if let Some(v) = level
            && v > 0.0
            && (self.features & mp_feat::VOLUME_SET) != 0
        {
            return self.set_volume_level((v - step).max(0.0));
        }
        Box::pin(async { Ok(()) })
    }
    fn set_volume_level(&self, volume: f64) -> BoxFuture<'_> {
        let rounded = (volume * 1_000_000.0).round() / 1_000_000.0;
        let val = if (rounded - 1.0).abs() < 1e-9 {
            json!(1)
        } else if rounded.abs() < 1e-9 {
            json!(0)
        } else {
            json!(rounded)
        };
        self.push(json!(["set_volume_level", val]))
    }
    fn mute_volume(&self, mute: bool) -> BoxFuture<'_> {
        self.push(json!(["mute_volume", mute]))
    }
    fn media_play(&self) -> BoxFuture<'_> {
        self.push(json!(["media_play"]))
    }
    fn media_pause(&self) -> BoxFuture<'_> {
        self.push(json!(["media_pause"]))
    }
    fn media_stop(&self) -> BoxFuture<'_> {
        self.push(json!(["media_stop"]))
    }
    fn media_next_track(&self) -> BoxFuture<'_> {
        self.push(json!(["media_next_track"]))
    }
    fn media_previous_track(&self) -> BoxFuture<'_> {
        self.push(json!(["media_previous_track"]))
    }
    fn media_seek(&self, position: f64) -> BoxFuture<'_> {
        self.push(json!(["media_seek", position]))
    }
    fn play_media(&self, params: PlayMedia) -> BoxFuture<'_> {
        let mut kw = Map::new();
        if let Some(enq) = params.enqueue {
            kw.insert("enqueue".into(), json!(enq));
        }
        if let Some(ann) = params.announce {
            kw.insert("announce".into(), json!(ann));
        }
        kw.insert("extra".into(), Value::Object(params.extra));
        self.push(json!([
            "play_media",
            params.media_type,
            params.media_id,
            kw
        ]))
    }
    fn select_source(&self, source: &str) -> BoxFuture<'_> {
        self.push(json!(["select_source", source]))
    }
    fn select_sound_mode(&self, sound_mode: &str) -> BoxFuture<'_> {
        self.push(json!(["select_sound_mode", sound_mode]))
    }
    fn set_shuffle(&self, shuffle: bool) -> BoxFuture<'_> {
        self.push(json!(["set_shuffle", shuffle]))
    }
    fn set_repeat(&self, repeat: &str) -> BoxFuture<'_> {
        self.push(json!(["set_repeat", repeat]))
    }
    fn join_players(&self, group_members: &[String]) -> BoxFuture<'_> {
        self.push(json!(["join_players", group_members]))
    }
    fn unjoin_player(&self) -> BoxFuture<'_> {
        self.push(json!(["unjoin_player"]))
    }
    fn clear_playlist(&self) -> BoxFuture<'_> {
        self.push(json!(["clear_playlist"]))
    }
}

struct FakeRemote {
    id: EntityId,
    is_on: bool,
    calls: Mutex<Vec<Value>>,
}

impl FakeRemote {
    fn new(id: &str, is_on: bool) -> Arc<Self> {
        Arc::new(Self {
            id: EntityId::parse(id).unwrap(),
            is_on,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn push(&self, v: Value) -> BoxFuture<'_> {
        self.calls.lock().push(v);
        Box::pin(async { Ok(()) })
    }
}

impl Remote for FakeRemote {
    fn entity_id(&self) -> &EntityId {
        &self.id
    }
    fn available(&self) -> bool {
        true
    }
    fn is_on(&self) -> bool {
        self.is_on
    }
    fn turn_on(&self, activity: Option<&str>) -> BoxFuture<'_> {
        let mut kw = Map::new();
        if let Some(a) = activity {
            kw.insert("activity".into(), json!(a));
        }
        self.push(json!(["turn_on", kw]))
    }
    fn turn_off(&self, activity: Option<&str>) -> BoxFuture<'_> {
        let mut kw = Map::new();
        if let Some(a) = activity {
            kw.insert("activity".into(), json!(a));
        }
        self.push(json!(["turn_off", kw]))
    }
    fn send_command(&self, cmd: SendCommand) -> BoxFuture<'_> {
        let mut kw = Map::new();
        if let Some(d) = cmd.device {
            kw.insert("device".into(), json!(d));
        }
        kw.insert("num_repeats".into(), json!(cmd.num_repeats));
        if let Some(delay) = cmd.delay_secs {
            kw.insert("delay_secs".into(), json!(delay));
        }
        kw.insert("hold_secs".into(), json!(cmd.hold_secs));
        self.push(json!(["send_command", cmd.command, kw]))
    }
    fn learn_command(&self, cmd: LearnCommand) -> BoxFuture<'_> {
        let mut kw = Map::new();
        if let Some(d) = cmd.device {
            kw.insert("device".into(), json!(d));
        }
        if let Some(c) = cmd.command {
            kw.insert("command".into(), json!(c));
        }
        if let Some(ct) = cmd.command_type {
            kw.insert("command_type".into(), json!(ct));
        }
        if let Some(alt) = cmd.alternative {
            kw.insert("alternative".into(), json!(alt));
        }
        if let Some(t) = cmd.timeout {
            kw.insert("timeout".into(), json!(t));
        }
        self.push(json!(["learn_command", kw]))
    }
    fn delete_command(&self, cmd: DeleteCommand) -> BoxFuture<'_> {
        let mut kw = Map::new();
        kw.insert("command".into(), json!(cmd.command));
        if let Some(d) = cmd.device {
            kw.insert("device".into(), json!(d));
        }
        self.push(json!(["delete_command", kw]))
    }
}

#[tokio::test]
async fn matches_ha_2026_6_1_media_player_and_remote_vectors() {
    let raw = include_str!("vectors/media_player_remote.json");
    let doc: Value = serde_json::from_str(raw).unwrap();
    assert_eq!(doc["ha_version"], "2026.6.1");

    // Verify unrecorded attributes match HA 2026.6.1 exactly.
    let hass = Hass::new();
    for (domain, id) in [
        ("media_player", "media_player.living_room"),
        ("remote", "remote.living_room"),
    ] {
        let eid = EntityId::parse(id).unwrap();
        let mut got: Vec<&str> = hass.unrecorded(&eid).domain.to_vec();
        got.sort_unstable();
        let want: Vec<&str> = doc["unrecorded_attributes"][domain]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(got, want, "unrecorded_attributes for {domain}");
    }

    let reg = Arc::new(ha_registry::Registries::default());
    let players = Arc::new(MediaPlayers::default());
    players.register_services(&hass, reg.clone());
    let remotes = Arc::new(Remotes::default());
    remotes.register_services(&hass, reg);

    let all_mp = mp_feat::PAUSE
        | mp_feat::SEEK
        | mp_feat::VOLUME_SET
        | mp_feat::VOLUME_MUTE
        | mp_feat::PREVIOUS_TRACK
        | mp_feat::NEXT_TRACK
        | mp_feat::TURN_ON
        | mp_feat::TURN_OFF
        | mp_feat::PLAY_MEDIA
        | mp_feat::SELECT_SOURCE
        | mp_feat::STOP
        | mp_feat::CLEAR_PLAYLIST
        | mp_feat::PLAY
        | mp_feat::SHUFFLE_SET
        | mp_feat::SELECT_SOUND_MODE
        | mp_feat::REPEAT_SET
        | mp_feat::GROUPING;

    let mp_list = vec![
        FakePlayer::new(
            "media_player.living_room",
            all_mp,
            Some("playing"),
            Some(0.5),
            0.1,
            false,
        ),
        FakePlayer::new(
            "media_player.bedroom",
            mp_feat::TURN_ON
                | mp_feat::TURN_OFF
                | mp_feat::PLAY
                | mp_feat::PAUSE
                | mp_feat::VOLUME_SET,
            Some("off"),
            Some(0.95),
            0.1,
            false,
        ),
        FakePlayer::new(
            "media_player.den",
            mp_feat::TURN_ON | mp_feat::TURN_OFF | mp_feat::VOLUME_SET,
            Some("standby"),
            Some(0.05),
            0.1,
            false,
        ),
        FakePlayer::new(
            "media_player.turn_on_only",
            mp_feat::TURN_ON | mp_feat::PLAY,
            Some("off"),
            None,
            0.1,
            false,
        ),
        FakePlayer::new(
            "media_player.step_only",
            mp_feat::VOLUME_STEP,
            Some("idle"),
            None,
            0.1,
            true,
        ),
        FakePlayer::new(
            "media_player.none_vol",
            mp_feat::VOLUME_SET,
            Some("idle"),
            None,
            0.1,
            false,
        ),
        FakePlayer::new(
            "media_player.edge_vol",
            mp_feat::VOLUME_SET,
            Some("idle"),
            Some(1.0),
            0.1,
            false,
        ),
        FakePlayer::new(
            "media_player.zero_vol",
            mp_feat::VOLUME_SET,
            Some("idle"),
            Some(0.0),
            0.1,
            false,
        ),
    ];
    for p in &mp_list {
        players.add(p.clone());
    }

    let rem_list = vec![
        FakeRemote::new("remote.living_room", false),
        FakeRemote::new("remote.bedroom", true),
    ];
    for r in &rem_list {
        remotes.add(r.clone());
    }

    let cases = doc["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 62);

    for case in cases {
        let name = case["name"].as_str().unwrap();
        let domain = case["domain"].as_str().unwrap();
        let service = case["service"].as_str().unwrap();
        let data = case["data"].as_object().unwrap().clone();
        for p in &mp_list {
            p.calls.lock().clear();
        }
        for r in &rem_list {
            r.calls.lock().clear();
        }

        let res = hass
            .call_service(domain, service, data, None, false, None)
            .await;

        if let Some(err_obj) = case.get("error") {
            let err = res.expect_err(&format!("case {name}: expected error"));
            match err_obj["kind"].as_str().unwrap() {
                "invalid" => match err {
                    ServiceError::Invalid(msg) => {
                        assert_eq!(msg, err_obj["message"].as_str().unwrap(), "case {name}");
                    }
                    other => panic!("case {name}: expected Invalid, got {other:?}"),
                },
                "validation" => match err {
                    ServiceError::Validation(r) => {
                        assert_eq!(
                            r.domain.as_str(),
                            err_obj["domain"].as_str().unwrap(),
                            "case {name}"
                        );
                        assert_eq!(
                            r.key.as_str(),
                            err_obj["key"].as_str().unwrap(),
                            "case {name}"
                        );
                        assert_eq!(
                            json!(r.placeholders),
                            err_obj["placeholders"],
                            "case {name}"
                        );
                    }
                    other => panic!("case {name}: expected Validation, got {other:?}"),
                },
                other => panic!("case {name}: unexpected vector error kind {other}"),
            }
        } else {
            res.unwrap_or_else(|e| panic!("case {name}: unexpected error {e:?}"));
            let mut got = Map::new();
            for p in &mp_list {
                let c = p.calls.lock().clone();
                if !c.is_empty() {
                    got.insert(p.id.as_str().to_owned(), Value::Array(c));
                }
            }
            for r in &rem_list {
                let c = r.calls.lock().clone();
                if !c.is_empty() {
                    got.insert(r.id.as_str().to_owned(), Value::Array(c));
                }
            }
            assert_eq!(Value::Object(got), case["calls"], "case {name}");
        }
    }
}

//! The `media_player` entity component (`components/media_player/__init__.py`):
//! `turn_on`, `turn_off`, `toggle`, `volume_up`, `volume_down`, `volume_set`,
//! `volume_mute`, `media_play_pause`, `media_play`, `media_pause`,
//! `media_stop`, `media_next_track`, `media_previous_track`, `media_seek`,
//! `play_media`, `select_source`, `select_sound_mode`, `shuffle_set`,
//! `repeat_set`, `join`, `unjoin`, and `clear_playlist`.

use crate::light::BoxFuture;
use crate::switch::select;
use ha_core::{
    EntityId, EntityTarget, Hass, Kind, Range, Schema, ServiceData, ServiceError, SupportsResponse,
};
use parking_lot::RwLock;
use serde_json::{Map, Value};
use std::sync::Arc;

pub const DOMAIN: &str = "media_player";

/// `MediaPlayerEntityFeature` (`components/media_player/const.py`).
pub mod feature {
    pub const PAUSE: u32 = 1;
    pub const SEEK: u32 = 2;
    pub const VOLUME_SET: u32 = 4;
    pub const VOLUME_MUTE: u32 = 8;
    pub const PREVIOUS_TRACK: u32 = 16;
    pub const NEXT_TRACK: u32 = 32;
    pub const TURN_ON: u32 = 128;
    pub const TURN_OFF: u32 = 256;
    pub const PLAY_MEDIA: u32 = 512;
    pub const VOLUME_STEP: u32 = 1024;
    pub const SELECT_SOURCE: u32 = 2048;
    pub const STOP: u32 = 4096;
    pub const CLEAR_PLAYLIST: u32 = 8192;
    pub const PLAY: u32 = 16384;
    pub const SHUFFLE_SET: u32 = 32768;
    pub const SELECT_SOUND_MODE: u32 = 65536;
    pub const BROWSE_MEDIA: u32 = 131072;
    pub const REPEAT_SET: u32 = 262144;
    pub const GROUPING: u32 = 524288;
    pub const MEDIA_ANNOUNCE: u32 = 1048576;
    pub const MEDIA_ENQUEUE: u32 = 2097152;
    pub const SEARCH_MEDIA: u32 = 4194304;
}

/// `RepeatMode` values (`components/media_player/const.py`).
pub const REPEAT_MODES: &[&str] = &["all", "off", "one"];

/// `MediaPlayerEnqueue` values (`components/media_player/__init__.py`).
pub const ENQUEUE_MODES: &[&str] = &["add", "next", "play", "replace"];

/// Validated arguments for `media_player.play_media`.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayMedia {
    pub media_type: String,
    pub media_id: String,
    pub enqueue: Option<String>,
    pub announce: Option<bool>,
    pub extra: Map<String, Value>,
}

/// A method the entity doesn't implement (Python's `NotImplementedError`).
fn unsupported<'a>() -> BoxFuture<'a> {
    Box::pin(async { Err(ServiceError::Unknown(String::new())) })
}

/// A media player entity a platform runs (`MediaPlayerEntity`).
pub trait MediaPlayer: Send + Sync {
    fn entity_id(&self) -> &EntityId;
    fn available(&self) -> bool;
    fn supported_features(&self) -> u32;

    /// Current player state string (e.g. `"off"`, `"standby"`, `"idle"`,
    /// `"playing"`, `"paused"`, `"on"`, `"buffering"`), if known.
    fn state(&self) -> Option<String> {
        None
    }

    /// Atomic snapshot of `(volume_level, volume_step)` for default
    /// `volume_up`/`volume_down` stepping when `VOLUME_SET` is supported.
    fn volume_info(&self) -> (Option<f64>, f64) {
        (None, 0.1)
    }

    fn turn_on(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn turn_off(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn toggle(&self) -> BoxFuture<'_> {
        let is_off = matches!(self.state().as_deref(), Some("off" | "standby"));
        if is_off {
            self.turn_on()
        } else {
            self.turn_off()
        }
    }
    fn volume_up(&self) -> BoxFuture<'_> {
        let (level, step) = self.volume_info();
        if let Some(v) = level
            && v < 1.0
            && (self.supported_features() & feature::VOLUME_SET) != 0
        {
            return self.set_volume_level((v + step).min(1.0));
        }
        Box::pin(async { Ok(()) })
    }
    fn volume_down(&self) -> BoxFuture<'_> {
        let (level, step) = self.volume_info();
        if let Some(v) = level
            && v > 0.0
            && (self.supported_features() & feature::VOLUME_SET) != 0
        {
            return self.set_volume_level((v - step).max(0.0));
        }
        Box::pin(async { Ok(()) })
    }
    fn set_volume_level(&self, _volume: f64) -> BoxFuture<'_> {
        unsupported()
    }
    fn mute_volume(&self, _mute: bool) -> BoxFuture<'_> {
        unsupported()
    }
    fn media_play(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn media_pause(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn media_play_pause(&self) -> BoxFuture<'_> {
        if self.state().as_deref() == Some("playing") {
            self.media_pause()
        } else {
            self.media_play()
        }
    }
    fn media_stop(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn media_next_track(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn media_previous_track(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn media_seek(&self, _position: f64) -> BoxFuture<'_> {
        unsupported()
    }
    fn play_media(&self, _params: PlayMedia) -> BoxFuture<'_> {
        unsupported()
    }
    fn select_source(&self, _source: &str) -> BoxFuture<'_> {
        unsupported()
    }
    fn select_sound_mode(&self, _sound_mode: &str) -> BoxFuture<'_> {
        unsupported()
    }
    fn set_shuffle(&self, _shuffle: bool) -> BoxFuture<'_> {
        unsupported()
    }
    fn set_repeat(&self, _repeat: &str) -> BoxFuture<'_> {
        unsupported()
    }
    fn join_players(&self, _group_members: &[String]) -> BoxFuture<'_> {
        unsupported()
    }
    fn unjoin_player(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn clear_playlist(&self) -> BoxFuture<'_> {
        unsupported()
    }
}

/// The running media player entities, for the services.
#[derive(Default)]
pub struct MediaPlayers {
    items: RwLock<Vec<Arc<dyn MediaPlayer>>>,
}

/// HA's entity-service selection with required feature sets (matches any set;
/// every bit in a given set must be present).
fn chosen(
    items: &[Arc<dyn MediaPlayer>],
    target: &EntityTarget,
    indirect: &[String],
    required: &[u32],
    service: &str,
) -> Result<Vec<Arc<dyn MediaPlayer>>, ServiceError> {
    let mut out = Vec::new();
    for p in select(
        items,
        |p| p.entity_id(),
        |p| p.available(),
        target,
        indirect,
    ) {
        let feats = p.supported_features();
        if !required.is_empty() && !required.iter().any(|r| feats & r == *r) {
            if let EntityTarget::Ids(ids) = target
                && ids.contains(p.entity_id())
            {
                return Err(ServiceError::not_supported(
                    DOMAIN,
                    service,
                    p.entity_id().as_str(),
                ));
            }
            continue;
        }
        out.push(p);
    }
    Ok(out)
}

/// HA's `_promote_media_fields`: if `media` is a dict, promote
/// `media_content_type` and `media_content_id` to top level, rejecting calls
/// that supply both `media` and top-level `media_content_id`/`media_content_type`.
fn promote_media_fields(data: &ServiceData) -> Result<Option<ServiceData>, ServiceError> {
    let Some(Value::Object(media)) = data.get("media") else {
        return Ok(None);
    };
    if data.contains_key("media_content_type") || data.contains_key("media_content_id") {
        return Err(ServiceError::Invalid(
            "Play media cannot contain 'media' and 'media_content_id' or 'media_content_type'"
                .into(),
        ));
    }
    let mut out = data.clone();
    if let Some(v) = media.get("media_content_type") {
        out.insert("media_content_type".into(), v.clone());
    }
    if let Some(v) = media.get("media_content_id") {
        out.insert("media_content_id".into(), v.clone());
    }
    out.remove("media");
    Ok(Some(out))
}

impl MediaPlayers {
    pub fn add(&self, player: Arc<dyn MediaPlayer>) {
        let mut items = self.items.write();
        items.retain(|p| p.entity_id() != player.entity_id());
        items.push(player);
    }

    pub fn remove(&self, entity_id: &EntityId) {
        self.items.write().retain(|p| p.entity_id() != entity_id);
    }

    /// Register all 22 core `media_player.*` control services on `hass`.
    pub fn register_services(
        self: &Arc<Self>,
        hass: &Hass,
        registries: impl Into<ha_registry::Lookup>,
    ) {
        let registries = registries.into();
        let entity = Schema::entity_service;
        use feature::*;

        let services: [(&'static str, Schema, &'static [u32]); 22] = [
            ("turn_on", entity(), &[TURN_ON]),
            ("turn_off", entity(), &[TURN_OFF]),
            ("toggle", entity(), &[TURN_ON | TURN_OFF]),
            ("volume_up", entity(), &[VOLUME_SET, VOLUME_STEP]),
            ("volume_down", entity(), &[VOLUME_SET, VOLUME_STEP]),
            ("media_play_pause", entity(), &[PLAY | PAUSE]),
            ("media_play", entity(), &[PLAY]),
            ("media_pause", entity(), &[PAUSE]),
            ("media_stop", entity(), &[STOP]),
            ("media_next_track", entity(), &[NEXT_TRACK]),
            ("media_previous_track", entity(), &[PREVIOUS_TRACK]),
            ("clear_playlist", entity(), &[CLEAR_PLAYLIST]),
            ("unjoin", entity(), &[GROUPING]),
            (
                "volume_set",
                entity().required("volume_level", Kind::Float(Range::new(0.0, 1.0))),
                &[VOLUME_SET],
            ),
            (
                "volume_mute",
                entity().required("is_volume_muted", Kind::Boolean),
                &[VOLUME_MUTE],
            ),
            (
                "media_seek",
                entity().required(
                    "seek_position",
                    Kind::Float(Range {
                        min: Some(0.0),
                        max: None,
                    }),
                ),
                &[SEEK],
            ),
            (
                "play_media",
                entity()
                    .pre_validate(promote_media_fields)
                    .required("media_content_type", Kind::String)
                    .required("media_content_id", Kind::String)
                    .optional(
                        "enqueue",
                        Kind::AnyOf(vec![
                            Kind::Boolean,
                            Kind::Enum("MediaPlayerEnqueue", ENQUEUE_MODES),
                        ]),
                    )
                    .optional("announce", Kind::Boolean)
                    .optional("extra", Kind::Dict)
                    .exclusive("enqueue_announce", &["enqueue", "announce"]),
                &[PLAY_MEDIA],
            ),
            (
                "select_source",
                entity().required("source", Kind::String),
                &[SELECT_SOURCE],
            ),
            (
                "select_sound_mode",
                entity().required("sound_mode", Kind::String),
                &[SELECT_SOUND_MODE],
            ),
            (
                "shuffle_set",
                entity().required("shuffle", Kind::Boolean),
                &[SHUFFLE_SET],
            ),
            (
                "repeat_set",
                entity().required("repeat", Kind::Enum("RepeatMode", REPEAT_MODES)),
                &[REPEAT_SET],
            ),
            (
                "join",
                entity().required("group_members", Kind::List(Box::new(Kind::EntityId))),
                &[GROUPING],
            ),
        ];

        for (name, schema, required) in services {
            let weak = Arc::downgrade(self);
            let reg = registries.clone();
            hass.services().register_with_schema(
                DOMAIN,
                name,
                Some(schema),
                SupportsResponse::None,
                move |call| {
                    let weak = weak.clone();
                    let reg = reg.clone();
                    async move {
                        let Some(players) = weak.upgrade() else {
                            return Ok(None);
                        };
                        let target = call.entity_ids()?;
                        let indirect = reg.get().target_entities(&call.data);
                        let list =
                            chosen(&players.items.read(), &target, &indirect, required, name)?;
                        let data = &call.data;
                        for p in list {
                            match name {
                                "turn_on" => p.turn_on().await?,
                                "turn_off" => p.turn_off().await?,
                                "toggle" => p.toggle().await?,
                                "volume_up" => p.volume_up().await?,
                                "volume_down" => p.volume_down().await?,
                                "media_play_pause" => p.media_play_pause().await?,
                                "media_play" => p.media_play().await?,
                                "media_pause" => p.media_pause().await?,
                                "media_stop" => p.media_stop().await?,
                                "media_next_track" => p.media_next_track().await?,
                                "media_previous_track" => p.media_previous_track().await?,
                                "clear_playlist" => p.clear_playlist().await?,
                                "unjoin" => p.unjoin_player().await?,
                                "volume_set" => {
                                    let v = data
                                        .get("volume_level")
                                        .and_then(Value::as_f64)
                                        .unwrap_or(0.0);
                                    p.set_volume_level(v).await?;
                                }
                                "volume_mute" => {
                                    let m = data
                                        .get("is_volume_muted")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false);
                                    p.mute_volume(m).await?;
                                }
                                "media_seek" => {
                                    let pos = data
                                        .get("seek_position")
                                        .and_then(Value::as_f64)
                                        .unwrap_or(0.0);
                                    p.media_seek(pos).await?;
                                }
                                "play_media" => {
                                    let media_type = data
                                        .get("media_content_type")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_owned();
                                    let media_id = data
                                        .get("media_content_id")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_owned();
                                    // Mirror HA's `_rewrite_enqueue`: True -> "add", False -> "play".
                                    let enqueue = data.get("enqueue").and_then(|v| match v {
                                        Value::Bool(true) => Some("add".to_owned()),
                                        Value::Bool(false) => Some("play".to_owned()),
                                        Value::String(s) => Some(s.clone()),
                                        _ => None,
                                    });
                                    let announce = data.get("announce").and_then(Value::as_bool);
                                    let extra = data
                                        .get("extra")
                                        .and_then(Value::as_object)
                                        .cloned()
                                        .unwrap_or_default();
                                    p.play_media(PlayMedia {
                                        media_type,
                                        media_id,
                                        enqueue,
                                        announce,
                                        extra,
                                    })
                                    .await?;
                                }
                                "select_source" => {
                                    let src = data
                                        .get("source")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default();
                                    p.select_source(src).await?;
                                }
                                "select_sound_mode" => {
                                    let mode = data
                                        .get("sound_mode")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default();
                                    p.select_sound_mode(mode).await?;
                                }
                                "shuffle_set" => {
                                    let s = data
                                        .get("shuffle")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false);
                                    p.set_shuffle(s).await?;
                                }
                                "repeat_set" => {
                                    let r = data
                                        .get("repeat")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default();
                                    p.set_repeat(r).await?;
                                }
                                "join" => {
                                    let members: Vec<String> = data
                                        .get("group_members")
                                        .and_then(Value::as_array)
                                        .map(|arr| {
                                            arr.iter()
                                                .filter_map(|v| v.as_str().map(str::to_owned))
                                                .collect()
                                        })
                                        .unwrap_or_default();
                                    p.join_players(&members).await?;
                                }
                                _ => {}
                            }
                        }
                        Ok(None)
                    }
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use serde_json::json;

    struct DummyPlayer {
        id: EntityId,
        available: bool,
        features: u32,
        state: Option<String>,
        volume: Option<f64>,
        calls: Mutex<Vec<String>>,
    }

    impl MediaPlayer for DummyPlayer {
        fn entity_id(&self) -> &EntityId {
            &self.id
        }
        fn available(&self) -> bool {
            self.available
        }
        fn supported_features(&self) -> u32 {
            self.features
        }
        fn state(&self) -> Option<String> {
            self.state.clone()
        }
        fn volume_info(&self) -> (Option<f64>, f64) {
            (self.volume, 0.1)
        }
        fn turn_on(&self) -> BoxFuture<'_> {
            self.calls.lock().push("turn_on".into());
            Box::pin(async { Ok(()) })
        }
        fn turn_off(&self) -> BoxFuture<'_> {
            self.calls.lock().push("turn_off".into());
            Box::pin(async { Ok(()) })
        }
        fn set_volume_level(&self, volume: f64) -> BoxFuture<'_> {
            self.calls.lock().push(format!("vol:{volume:.2}"));
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn unavailable_player_is_skipped_and_no_cycle() {
        let hass = Hass::new();
        let players = Arc::new(MediaPlayers::default());
        players.register_services(&hass, Arc::new(ha_registry::Registries::default()));

        let offline = Arc::new(DummyPlayer {
            id: EntityId::parse("media_player.offline").unwrap(),
            available: false,
            features: feature::TURN_ON | feature::TURN_OFF | feature::VOLUME_SET,
            state: Some("off".into()),
            volume: Some(0.5),
            calls: Mutex::new(Vec::new()),
        });
        players.add(offline.clone());
        hass.call_service(
            "media_player",
            "turn_on",
            json!({"entity_id": "media_player.offline"})
                .as_object()
                .cloned()
                .unwrap(),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        assert!(offline.calls.lock().is_empty());

        let weak = Arc::downgrade(&players);
        drop((hass, players));
        assert!(weak.upgrade().is_none());
    }
}

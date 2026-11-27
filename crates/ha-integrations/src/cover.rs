//! The `cover` entity component (`components/cover/__init__.py` and
//! `components/cover/const.py`): `open_cover`, `close_cover`,
//! `set_cover_position`, `stop_cover`, `toggle`, `open_cover_tilt`,
//! `close_cover_tilt`, `stop_cover_tilt`, `set_cover_tilt_position`, and
//! `toggle_cover_tilt`.

use crate::light::BoxFuture;
use crate::switch::select;
use ha_core::{EntityId, EntityTarget, Hass, Kind, Range, Schema, ServiceError, SupportsResponse};
use parking_lot::RwLock;
use serde_json::Value;
use std::sync::Arc;

pub const DOMAIN: &str = "cover";

/// `CoverEntityFeature` (`components/cover/const.py`).
pub mod feature {
    pub const OPEN: u32 = 1;
    pub const CLOSE: u32 = 2;
    pub const SET_POSITION: u32 = 4;
    pub const STOP: u32 = 8;
    pub const OPEN_TILT: u32 = 16;
    pub const CLOSE_TILT: u32 = 32;
    pub const STOP_TILT: u32 = 64;
    pub const SET_TILT_POSITION: u32 = 128;
}

/// A method the entity doesn't implement (Python's `NotImplementedError`).
fn unsupported<'a>() -> BoxFuture<'a> {
    Box::pin(async { Err(ServiceError::Unknown(String::new())) })
}

/// A cover entity a platform runs (`CoverEntity`).
pub trait Cover: Send + Sync {
    fn entity_id(&self) -> &EntityId;
    fn available(&self) -> bool;
    fn supported_features(&self) -> u32;

    fn is_closed(&self) -> Option<bool> {
        None
    }
    fn is_opening(&self) -> Option<bool> {
        None
    }
    fn is_closing(&self) -> Option<bool> {
        None
    }
    fn current_position(&self) -> Option<u8> {
        None
    }
    fn current_tilt_position(&self) -> Option<u8> {
        None
    }
    /// Mirrors HA's `CoverEntity._cover_is_last_toggle_direction_open`
    /// (defaults to `true`; becomes `true` when opening, `false` when closing).
    fn last_toggle_direction_open(&self) -> bool {
        true
    }

    fn open_cover(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn close_cover(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn stop_cover(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn set_cover_position(&self, _position: u8) -> BoxFuture<'_> {
        unsupported()
    }
    fn open_cover_tilt(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn close_cover_tilt(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn stop_cover_tilt(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn set_cover_tilt_position(&self, _tilt_position: u8) -> BoxFuture<'_> {
        unsupported()
    }
}

/// The running cover entities, for the services.
#[derive(Default)]
pub struct Covers {
    items: RwLock<Vec<Arc<dyn Cover>>>,
}

/// HA's entity-service selection with required features.
fn chosen(
    items: &[Arc<dyn Cover>],
    target: &EntityTarget,
    indirect: &[String],
    required: u32,
    service: &str,
) -> Result<Vec<Arc<dyn Cover>>, ServiceError> {
    let mut out = Vec::new();
    for c in select(
        items,
        |c| c.entity_id(),
        |c| c.available(),
        target,
        indirect,
    ) {
        if required != 0 && (c.supported_features() & required) != required {
            if let EntityTarget::Ids(ids) = target
                && ids.contains(c.entity_id())
            {
                return Err(ServiceError::not_supported(
                    DOMAIN,
                    service,
                    c.entity_id().as_str(),
                ));
            }
            continue;
        }
        out.push(c);
    }
    Ok(out)
}

/// HA's `CoverEntity._get_toggle_function`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleAction {
    Open,
    Close,
    Stop,
}

pub fn toggle_action(c: &dyn Cover) -> ToggleAction {
    let is_opening = c.is_opening() == Some(true);
    let is_closing = c.is_closing() == Some(true);
    if (c.supported_features() & feature::STOP != 0) && (is_closing || is_opening) {
        return ToggleAction::Stop;
    }
    if c.is_closed() == Some(true) || is_closing {
        return ToggleAction::Open;
    }
    if c.current_position() == Some(100) || is_opening {
        return ToggleAction::Close;
    }
    if c.last_toggle_direction_open() {
        ToggleAction::Close
    } else {
        ToggleAction::Open
    }
}

impl Covers {
    pub fn add(&self, c: Arc<dyn Cover>) {
        self.items.write().push(c);
    }

    pub fn remove(&self, entity_id: &EntityId) {
        self.items.write().retain(|c| c.entity_id() != entity_id);
    }

    /// Register HA's ten `cover.*` entity services.
    pub fn register_services(
        self: &Arc<Self>,
        hass: &Hass,
        registries: impl Into<ha_registry::Lookup>,
    ) {
        let registries = registries.into();
        let entity = Schema::entity_service;
        let pct = || Kind::Int(Range::new(0.0, 100.0));
        use feature::*;
        let services: [(&'static str, Schema, u32); 10] = [
            ("open_cover", entity(), OPEN),
            ("close_cover", entity(), CLOSE),
            (
                "set_cover_position",
                entity().required("position", pct()),
                SET_POSITION,
            ),
            ("stop_cover", entity(), STOP),
            ("toggle", entity(), OPEN | CLOSE),
            ("open_cover_tilt", entity(), OPEN_TILT),
            ("close_cover_tilt", entity(), CLOSE_TILT),
            ("stop_cover_tilt", entity(), STOP_TILT),
            (
                "set_cover_tilt_position",
                entity().required("tilt_position", pct()),
                SET_TILT_POSITION,
            ),
            ("toggle_cover_tilt", entity(), OPEN_TILT | CLOSE_TILT),
        ];

        for (name, schema, required) in services {
            let covers = self.clone();
            let registries = registries.clone();
            hass.services().register_with_schema(
                DOMAIN,
                name,
                Some(schema),
                SupportsResponse::None,
                move |call| {
                    let covers = covers.clone();
                    let registries = registries.clone();
                    async move {
                        let target = call.entity_ids()?;
                        let indirect = registries.get().target_entities(&call.data);
                        let items = covers.items.read().clone();
                        let chosen = chosen(&items, &target, &indirect, required, name)?;
                        let data = &call.data;
                        let u8_of = |k: &str| {
                            data.get(k).and_then(Value::as_u64).unwrap_or(0).min(100) as u8
                        };
                        let calls = chosen.iter().map(|c| async move {
                            match name {
                                "open_cover" => c.open_cover().await,
                                "close_cover" => c.close_cover().await,
                                "stop_cover" => c.stop_cover().await,
                                "set_cover_position" => {
                                    c.set_cover_position(u8_of("position")).await
                                }
                                "toggle" => match toggle_action(c.as_ref()) {
                                    ToggleAction::Open => c.open_cover().await,
                                    ToggleAction::Close => c.close_cover().await,
                                    ToggleAction::Stop => c.stop_cover().await,
                                },
                                "open_cover_tilt" => c.open_cover_tilt().await,
                                "close_cover_tilt" => c.close_cover_tilt().await,
                                "stop_cover_tilt" => c.stop_cover_tilt().await,
                                "set_cover_tilt_position" => {
                                    c.set_cover_tilt_position(u8_of("tilt_position")).await
                                }
                                "toggle_cover_tilt" => {
                                    if c.current_tilt_position() == Some(0) {
                                        c.open_cover_tilt().await
                                    } else {
                                        c.close_cover_tilt().await
                                    }
                                }
                                _ => Err(ServiceError::Unknown(format!(
                                    "cover.{name} is not supported"
                                ))),
                            }
                        });
                        crate::light::join_all(calls)
                            .await
                            .into_iter()
                            .collect::<Result<Vec<()>, _>>()?;
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
    use serde_json::{Map, json};

    #[derive(Default)]
    struct State {
        is_closed: Option<bool>,
        is_opening: Option<bool>,
        is_closing: Option<bool>,
        pos: Option<u8>,
        tilt: Option<u8>,
        last_open: bool,
    }

    struct FakeCover {
        id: EntityId,
        available: bool,
        features: u32,
        state: Mutex<State>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeCover {
        fn new(id: &str, available: bool, features: u32) -> Arc<Self> {
            Arc::new(Self {
                id: EntityId::parse(id).unwrap(),
                available,
                features,
                state: Mutex::new(State {
                    last_open: true,
                    ..Default::default()
                }),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn record(&self, s: impl Into<String>) -> BoxFuture<'_> {
            self.calls.lock().push(s.into());
            Box::pin(async { Ok(()) })
        }
    }

    impl Cover for FakeCover {
        fn entity_id(&self) -> &EntityId {
            &self.id
        }
        fn available(&self) -> bool {
            self.available
        }
        fn supported_features(&self) -> u32 {
            self.features
        }
        fn is_closed(&self) -> Option<bool> {
            self.state.lock().is_closed
        }
        fn is_opening(&self) -> Option<bool> {
            self.state.lock().is_opening
        }
        fn is_closing(&self) -> Option<bool> {
            self.state.lock().is_closing
        }
        fn current_position(&self) -> Option<u8> {
            self.state.lock().pos
        }
        fn current_tilt_position(&self) -> Option<u8> {
            self.state.lock().tilt
        }
        fn last_toggle_direction_open(&self) -> bool {
            self.state.lock().last_open
        }
        fn open_cover(&self) -> BoxFuture<'_> {
            self.record("open")
        }
        fn close_cover(&self) -> BoxFuture<'_> {
            self.record("close")
        }
        fn stop_cover(&self) -> BoxFuture<'_> {
            self.record("stop")
        }
        fn set_cover_position(&self, pos: u8) -> BoxFuture<'_> {
            self.record(format!("pos:{pos}"))
        }
        fn open_cover_tilt(&self) -> BoxFuture<'_> {
            self.record("open_tilt")
        }
        fn close_cover_tilt(&self) -> BoxFuture<'_> {
            self.record("close_tilt")
        }
        fn stop_cover_tilt(&self) -> BoxFuture<'_> {
            self.record("stop_tilt")
        }
        fn set_cover_tilt_position(&self, pos: u8) -> BoxFuture<'_> {
            self.record(format!("tilt:{pos}"))
        }
    }

    fn map_of(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[tokio::test]
    async fn dispatches_all_cover_services_and_toggle_logic() {
        let hass = Hass::new();
        let covers = Arc::new(Covers::default());
        covers.register_services(&hass, Arc::new(ha_registry::Registries::default()));

        let all = feature::OPEN
            | feature::CLOSE
            | feature::SET_POSITION
            | feature::STOP
            | feature::OPEN_TILT
            | feature::CLOSE_TILT
            | feature::STOP_TILT
            | feature::SET_TILT_POSITION;
        let c = FakeCover::new("cover.blinds", true, all);
        let open_only = FakeCover::new("cover.open_only", true, feature::OPEN);
        let offline = FakeCover::new("cover.offline", false, all);
        covers.add(c.clone());
        covers.add(open_only.clone());
        covers.add(offline.clone());

        for svc in [
            "open_cover",
            "close_cover",
            "stop_cover",
            "open_cover_tilt",
            "close_cover_tilt",
            "stop_cover_tilt",
        ] {
            hass.call_service(
                "cover",
                svc,
                map_of(json!({"entity_id": "cover.blinds"})),
                None,
                false,
                None,
            )
            .await
            .unwrap();
        }
        hass.call_service(
            "cover",
            "set_cover_position",
            map_of(json!({"entity_id": "cover.blinds", "position": 55})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        hass.call_service(
            "cover",
            "set_cover_tilt_position",
            map_of(json!({"entity_id": "cover.blinds", "tilt_position": 25})),
            None,
            false,
            None,
        )
        .await
        .unwrap();

        // Toggle state machine:
        // 1. opening with STOP -> stop
        c.state.lock().is_opening = Some(true);
        hass.call_service(
            "cover",
            "toggle",
            map_of(json!({"entity_id": "cover.blinds"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        // 2. closed -> open
        *c.state.lock() = State {
            is_closed: Some(true),
            last_open: true,
            ..Default::default()
        };
        hass.call_service(
            "cover",
            "toggle",
            map_of(json!({"entity_id": "cover.blinds"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        // 3. partially open after closing -> open
        *c.state.lock() = State {
            is_closed: Some(false),
            pos: Some(50),
            last_open: false,
            ..Default::default()
        };
        hass.call_service(
            "cover",
            "toggle",
            map_of(json!({"entity_id": "cover.blinds"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        // 4. toggle_cover_tilt at 0 -> open_tilt; at 50 -> close_tilt
        c.state.lock().tilt = Some(0);
        hass.call_service(
            "cover",
            "toggle_cover_tilt",
            map_of(json!({"entity_id": "cover.blinds"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        c.state.lock().tilt = Some(50);
        hass.call_service(
            "cover",
            "toggle_cover_tilt",
            map_of(json!({"entity_id": "cover.blinds"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            *c.calls.lock(),
            vec![
                "open",
                "close",
                "stop",
                "open_tilt",
                "close_tilt",
                "stop_tilt",
                "pos:55",
                "tilt:25",
                "stop",
                "open",
                "open",
                "open_tilt",
                "close_tilt",
            ]
        );

        // Toggle requires BOTH OPEN and CLOSE; open-only cover fails with ServiceNotSupported.
        let err = hass
            .call_service(
                "cover",
                "toggle",
                map_of(json!({"entity_id": "cover.open_only"})),
                None,
                false,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ServiceError::not_supported("cover", "toggle", "cover.open_only")
        );

        // Out-of-range position rejected by schema.
        let bad = hass
            .call_service(
                "cover",
                "set_cover_position",
                map_of(json!({"entity_id": "cover.blinds", "position": 101})),
                None,
                false,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(bad, ServiceError::Invalid(_)));

        // `all` skips offline and unsupported entities.
        hass.call_service(
            "cover",
            "close_cover",
            map_of(json!({"entity_id": "all"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        assert!(open_only.calls.lock().is_empty());
        assert!(offline.calls.lock().is_empty());
    }

    #[test]
    fn services_hold_no_cycle() {
        let hass = Hass::new();
        let covers = Arc::new(Covers::default());
        covers.register_services(&hass, Arc::new(ha_registry::Registries::default()));
        let weak = Arc::downgrade(&covers);
        drop((hass, covers));
        assert!(weak.upgrade().is_none());
    }
}

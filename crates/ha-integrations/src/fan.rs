//! The `fan` entity component (`components/fan/__init__.py`): `turn_on`,
//! `turn_off`, `toggle`, `increase_speed`, `decrease_speed`, `oscillate`,
//! `set_direction`, `set_percentage`, and `set_preset_mode`.

use crate::light::BoxFuture;
use crate::switch::select;
use ha_core::{EntityId, EntityTarget, Hass, Kind, Range, Schema, ServiceError, SupportsResponse};
use parking_lot::RwLock;
use serde_json::{Map, Value, json};
use std::sync::Arc;

pub const DOMAIN: &str = "fan";

/// `FanEntityFeature` (`components/fan/__init__.py`).
pub mod feature {
    pub const SET_SPEED: u32 = 1;
    pub const OSCILLATE: u32 = 2;
    pub const DIRECTION: u32 = 4;
    pub const PRESET_MODE: u32 = 8;
    pub const TURN_OFF: u32 = 16;
    pub const TURN_ON: u32 = 32;
}

/// A method the entity doesn't implement (Python's `NotImplementedError`).
fn unsupported<'a>() -> BoxFuture<'a> {
    Box::pin(async { Err(ServiceError::Unknown(String::new())) })
}

/// A fan entity a platform runs (`FanEntity`).
pub trait Fan: Send + Sync {
    fn entity_id(&self) -> &EntityId;
    fn available(&self) -> bool;
    fn supported_features(&self) -> u32;
    fn is_on(&self) -> bool;

    fn percentage(&self) -> Option<u8> {
        None
    }
    fn speed_count(&self) -> u32 {
        100
    }
    fn preset_modes(&self) -> Vec<String> {
        Vec::new()
    }

    fn turn_on(&self, _percentage: Option<u8>, _preset_mode: Option<&str>) -> BoxFuture<'_> {
        unsupported()
    }
    fn turn_off(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn set_percentage(&self, _percentage: u8) -> BoxFuture<'_> {
        unsupported()
    }
    fn set_preset_mode(&self, _preset_mode: &str) -> BoxFuture<'_> {
        unsupported()
    }
    fn oscillate(&self, _oscillating: bool) -> BoxFuture<'_> {
        unsupported()
    }
    fn set_direction(&self, _direction: &str) -> BoxFuture<'_> {
        unsupported()
    }
}

/// The running fan entities, for the services.
#[derive(Default)]
pub struct Fans {
    items: RwLock<Vec<Arc<dyn Fan>>>,
}

/// HA's entity-service selection with required feature sets (matches any set).
fn chosen(
    items: &[Arc<dyn Fan>],
    target: &EntityTarget,
    indirect: &[String],
    required: &[u32],
    service: &str,
) -> Result<Vec<Arc<dyn Fan>>, ServiceError> {
    let mut out = Vec::new();
    for f in select(
        items,
        |f| f.entity_id(),
        |f| f.available(),
        target,
        indirect,
    ) {
        let feats = f.supported_features();
        if !required.is_empty() && !required.iter().any(|r| feats & r == *r) {
            if let EntityTarget::Ids(ids) = target
                && ids.contains(f.entity_id())
            {
                return Err(ServiceError::not_supported(
                    DOMAIN,
                    service,
                    f.entity_id().as_str(),
                ));
            }
            continue;
        }
        out.push(f);
    }
    Ok(out)
}

/// HA's `FanEntity._valid_preset_mode_or_raise`.
fn validate_preset_mode(preset_mode: &str, preset_modes: &[String]) -> Result<(), ServiceError> {
    if !preset_modes.is_empty() && preset_modes.iter().any(|m| m == preset_mode) {
        return Ok(());
    }
    let modes_str = preset_modes.join(", ");
    let mut p = Map::new();
    p.insert("preset_mode".into(), json!(preset_mode));
    p.insert("preset_modes".into(), json!(modes_str));
    Err(ServiceError::validation(
        DOMAIN,
        "not_valid_preset_mode",
        Some(p),
        format!("Preset mode {preset_mode} is not valid, valid preset modes are: {modes_str}"),
    ))
}

/// HA's `FanEntity._async_adjust_speed` calculation (matching
/// `util/percentage.py`'s `percentage_to_ranged_value` and
/// `ranged_value_to_percentage`).
pub fn adjust_speed(
    current_percentage: Option<u8>,
    speed_count: u32,
    modifier: i64,
    percentage_step: Option<u8>,
) -> u8 {
    let current = i64::from(current_percentage.unwrap_or(0));
    let new_pct = if let Some(step) = percentage_step {
        current + i64::from(step) * modifier
    } else {
        let sc = f64::from(speed_count.max(1));
        let speed_index = ((current as f64) * sc / 100.0).ceil() as i64;
        let target_index = speed_index + modifier;
        ((target_index as f64) * 100.0 / sc).floor() as i64
    };
    new_pct.clamp(0, 100) as u8
}

impl Fans {
    pub fn add(&self, f: Arc<dyn Fan>) {
        self.items.write().push(f);
    }

    pub fn remove(&self, entity_id: &EntityId) {
        self.items.write().retain(|f| f.entity_id() != entity_id);
    }

    /// Register HA's nine `fan.*` entity services.
    pub fn register_services(
        self: &Arc<Self>,
        hass: &Hass,
        registries: impl Into<ha_registry::Lookup>,
    ) {
        let registries = registries.into();
        let entity = Schema::entity_service;
        let pct = || Kind::Int(Range::new(0.0, 100.0));
        use feature::*;
        let services: [(&'static str, Schema, &'static [u32]); 9] = [
            (
                "turn_on",
                entity()
                    .optional("percentage", pct())
                    .optional("preset_mode", Kind::String),
                &[TURN_ON],
            ),
            ("turn_off", entity(), &[TURN_OFF]),
            ("toggle", entity(), &[TURN_OFF, TURN_ON]),
            (
                "increase_speed",
                entity().optional("percentage_step", pct()),
                &[SET_SPEED],
            ),
            (
                "decrease_speed",
                entity().optional("percentage_step", pct()),
                &[SET_SPEED],
            ),
            (
                "oscillate",
                entity().required("oscillating", Kind::Boolean),
                &[OSCILLATE],
            ),
            (
                "set_direction",
                entity().optional("direction", Kind::String),
                &[DIRECTION],
            ),
            (
                "set_percentage",
                entity().required("percentage", pct()),
                &[SET_SPEED],
            ),
            (
                "set_preset_mode",
                entity().required("preset_mode", Kind::String),
                &[SET_SPEED, PRESET_MODE],
            ),
        ];

        for (name, schema, required) in services {
            let fans = self.clone();
            let registries = registries.clone();
            hass.services().register_with_schema(
                DOMAIN,
                name,
                Some(schema),
                SupportsResponse::None,
                move |call| {
                    let fans = fans.clone();
                    let registries = registries.clone();
                    async move {
                        let target = call.entity_ids()?;
                        let indirect = registries.get().target_entities(&call.data);
                        let items = fans.items.read().clone();
                        let chosen = chosen(&items, &target, &indirect, required, name)?;
                        let data = &call.data;
                        let opt_u8 = |k: &str| {
                            data.get(k)
                                .and_then(Value::as_u64)
                                .map(|n| n.min(100) as u8)
                        };
                        let calls = chosen.iter().map(|f| async move {
                            match name {
                                "turn_on" => {
                                    let percentage = opt_u8("percentage");
                                    let preset_mode =
                                        data.get("preset_mode").and_then(Value::as_str);
                                    if let Some(pm) = preset_mode {
                                        validate_preset_mode(pm, &f.preset_modes())?;
                                    }
                                    f.turn_on(percentage, preset_mode).await
                                }
                                "turn_off" => f.turn_off().await,
                                "toggle" => {
                                    if f.is_on() {
                                        f.turn_off().await
                                    } else {
                                        f.turn_on(None, None).await
                                    }
                                }
                                "increase_speed" => {
                                    let pct = adjust_speed(
                                        f.percentage(),
                                        f.speed_count(),
                                        1,
                                        opt_u8("percentage_step"),
                                    );
                                    f.set_percentage(pct).await
                                }
                                "decrease_speed" => {
                                    let pct = adjust_speed(
                                        f.percentage(),
                                        f.speed_count(),
                                        -1,
                                        opt_u8("percentage_step"),
                                    );
                                    f.set_percentage(pct).await
                                }
                                "oscillate" => {
                                    let osc = data
                                        .get("oscillating")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false);
                                    f.oscillate(osc).await
                                }
                                "set_direction" => {
                                    // HA's schema marks `direction` optional, then
                                    // `FanEntity.async_set_direction(self, direction: str)` raises
                                    // TypeError (-> unknown_error) when omitted.
                                    let Some(dir) = data.get("direction").and_then(Value::as_str)
                                    else {
                                        return Err(ServiceError::Unknown("Unknown error".into()));
                                    };
                                    f.set_direction(dir).await
                                }
                                "set_percentage" => {
                                    let pct = opt_u8("percentage").unwrap_or(0);
                                    f.set_percentage(pct).await
                                }
                                "set_preset_mode" => {
                                    let pm = data
                                        .get("preset_mode")
                                        .and_then(Value::as_str)
                                        .unwrap_or("");
                                    validate_preset_mode(pm, &f.preset_modes())?;
                                    f.set_preset_mode(pm).await
                                }
                                _ => Err(ServiceError::Unknown(format!(
                                    "fan.{name} is not supported"
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

    struct FakeFan {
        id: EntityId,
        available: bool,
        features: u32,
        speed_count: u32,
        preset_modes: Vec<String>,
        on: Mutex<bool>,
        pct: Mutex<Option<u8>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeFan {
        fn new(id: &str, available: bool, features: u32, speed_count: u32) -> Arc<Self> {
            Arc::new(Self {
                id: EntityId::parse(id).unwrap(),
                available,
                features,
                speed_count,
                preset_modes: vec!["auto".into(), "sleep".into(), "breeze".into()],
                on: Mutex::new(false),
                pct: Mutex::new(Some(0)),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn record(&self, s: impl Into<String>) -> BoxFuture<'_> {
            self.calls.lock().push(s.into());
            Box::pin(async { Ok(()) })
        }
    }

    impl Fan for FakeFan {
        fn entity_id(&self) -> &EntityId {
            &self.id
        }
        fn available(&self) -> bool {
            self.available
        }
        fn supported_features(&self) -> u32 {
            self.features
        }
        fn is_on(&self) -> bool {
            *self.on.lock()
        }
        fn percentage(&self) -> Option<u8> {
            *self.pct.lock()
        }
        fn speed_count(&self) -> u32 {
            self.speed_count
        }
        fn preset_modes(&self) -> Vec<String> {
            self.preset_modes.clone()
        }
        fn turn_on(&self, percentage: Option<u8>, preset_mode: Option<&str>) -> BoxFuture<'_> {
            *self.on.lock() = true;
            if let Some(p) = percentage {
                *self.pct.lock() = Some(p);
            }
            self.record(format!("on:{percentage:?}:{preset_mode:?}"))
        }
        fn turn_off(&self) -> BoxFuture<'_> {
            *self.on.lock() = false;
            *self.pct.lock() = Some(0);
            self.record("off")
        }
        fn set_percentage(&self, percentage: u8) -> BoxFuture<'_> {
            *self.pct.lock() = Some(percentage);
            *self.on.lock() = percentage > 0;
            self.record(format!("pct:{percentage}"))
        }
        fn set_preset_mode(&self, preset_mode: &str) -> BoxFuture<'_> {
            self.record(format!("preset:{preset_mode}"))
        }
        fn oscillate(&self, oscillating: bool) -> BoxFuture<'_> {
            self.record(format!("osc:{oscillating}"))
        }
        fn set_direction(&self, direction: &str) -> BoxFuture<'_> {
            self.record(format!("dir:{direction}"))
        }
    }

    fn map_of(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn adjust_speed_matches_ha() {
        // 3-speed fan: 0 -> 33 -> 66 -> 100 -> 100; 100 -> 66 -> 33 -> 0
        assert_eq!(adjust_speed(Some(0), 3, 1, None), 33);
        assert_eq!(adjust_speed(Some(33), 3, 1, None), 66);
        assert_eq!(adjust_speed(Some(66), 3, 1, None), 100);
        assert_eq!(adjust_speed(Some(100), 3, 1, None), 100);
        assert_eq!(adjust_speed(Some(100), 3, -1, None), 66);
        assert_eq!(adjust_speed(Some(66), 3, -1, None), 33);
        assert_eq!(adjust_speed(Some(33), 3, -1, None), 0);
        assert_eq!(adjust_speed(Some(0), 3, -1, None), 0);
        // Explicit percentage_step overrides speed_count.
        assert_eq!(adjust_speed(Some(40), 3, 1, Some(25)), 65);
        assert_eq!(adjust_speed(Some(10), 3, -1, Some(25)), 0);
    }

    #[tokio::test]
    async fn fan_services_validation_and_features() {
        let hass = Hass::new();
        let fans = Arc::new(Fans::default());
        fans.register_services(&hass, Arc::new(ha_registry::Registries::default()));

        let all = feature::SET_SPEED
            | feature::OSCILLATE
            | feature::DIRECTION
            | feature::PRESET_MODE
            | feature::TURN_OFF
            | feature::TURN_ON;
        let f = FakeFan::new("fan.bedroom", true, all, 3);
        let basic = FakeFan::new("fan.basic", true, feature::TURN_ON | feature::TURN_OFF, 100);
        fans.add(f.clone());
        fans.add(basic.clone());

        hass.call_service(
            "fan",
            "turn_on",
            map_of(json!({"entity_id": "fan.bedroom", "percentage": 33, "preset_mode": "auto"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        hass.call_service(
            "fan",
            "increase_speed",
            map_of(json!({"entity_id": "fan.bedroom"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        hass.call_service(
            "fan",
            "decrease_speed",
            map_of(json!({"entity_id": "fan.bedroom", "percentage_step": 16})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        hass.call_service(
            "fan",
            "oscillate",
            map_of(json!({"entity_id": "fan.bedroom", "oscillating": "on"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        hass.call_service(
            "fan",
            "set_direction",
            map_of(json!({"entity_id": "fan.bedroom", "direction": "reverse"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        hass.call_service(
            "fan",
            "set_preset_mode",
            map_of(json!({"entity_id": "fan.bedroom", "preset_mode": "sleep"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        hass.call_service(
            "fan",
            "toggle",
            map_of(json!({"entity_id": "fan.bedroom"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        hass.call_service(
            "fan",
            "toggle",
            map_of(json!({"entity_id": "fan.bedroom"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            *f.calls.lock(),
            vec![
                r#"on:Some(33):Some("auto")"#,
                "pct:66",
                "pct:50",
                "osc:true",
                "dir:reverse",
                "preset:sleep",
                "off",
                "on:None:None",
            ]
        );

        // Invalid preset_mode rejected in both set_preset_mode and turn_on.
        for svc in ["set_preset_mode", "turn_on"] {
            let err = hass
                .call_service(
                    "fan",
                    svc,
                    map_of(json!({"entity_id": "fan.bedroom", "preset_mode": "turbo"})),
                    None,
                    false,
                    None,
                )
                .await
                .unwrap_err();
            match err {
                ServiceError::Validation(r) => {
                    assert_eq!(r.domain.as_str(), "fan");
                    assert_eq!(r.key.as_str(), "not_valid_preset_mode");
                    assert_eq!(
                        r.message,
                        "Preset mode turbo is not valid, valid preset modes are: auto, sleep, breeze"
                    );
                }
                other => panic!("expected validation error, got {other:?}"),
            }
        }

        // Feature gating: basic fan doesn't support oscillate.
        let err = hass
            .call_service(
                "fan",
                "oscillate",
                map_of(json!({"entity_id": "fan.basic", "oscillating": true})),
                None,
                false,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ServiceError::not_supported("fan", "oscillate", "fan.basic")
        );

        // Omitting direction fails before reaching the entity (HA raises TypeError -> Unknown).
        let before = f.calls.lock().len();
        let err = hass
            .call_service(
                "fan",
                "set_direction",
                map_of(json!({"entity_id": "fan.bedroom"})),
                None,
                false,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err, ServiceError::Unknown("Unknown error".into()));
        assert_eq!(f.calls.lock().len(), before);
    }

    #[test]
    fn services_hold_no_cycle() {
        let hass = Hass::new();
        let fans = Arc::new(Fans::default());
        fans.register_services(&hass, Arc::new(ha_registry::Registries::default()));
        let weak = Arc::downgrade(&fans);
        drop((hass, fans));
        assert!(weak.upgrade().is_none());
    }
}

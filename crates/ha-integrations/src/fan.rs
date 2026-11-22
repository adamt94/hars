//! The `fan` entity component (`components/fan/__init__.py`): every one of
//! HA's fan services, run against the fans platforms provide. The platforms
//! write the states themselves.

use crate::humidifier::{Call, chosen};
use crate::light::BoxFuture;
use ha_core::{EntityId, Hass, Kind, Range, Schema, ServiceError, SupportsResponse};
use parking_lot::RwLock;
use serde_json::{Map, Value, json};
use std::sync::Arc;

pub const DOMAIN: &str = "fan";

/// `FanEntityFeature`.
pub mod feature {
    pub const SET_SPEED: u32 = 1;
    pub const OSCILLATE: u32 = 2;
    pub const DIRECTION: u32 = 4;
    pub const PRESET_MODE: u32 = 8;
    pub const TURN_OFF: u32 = 16;
    pub const TURN_ON: u32 = 32;
}

/// A fan a platform runs.
pub trait Fan: Send + Sync {
    fn entity_id(&self) -> &EntityId;
    fn available(&self) -> bool;
    fn supported_features(&self) -> u32;
    fn is_on(&self) -> bool;
    /// `percentage` (HA's default is 0).
    fn percentage(&self) -> Option<i64> {
        Some(0)
    }
    /// `speed_count` (HA's default is 100).
    fn speed_count(&self) -> u32 {
        100
    }
    fn preset_modes(&self) -> Vec<String> {
        Vec::new()
    }
    fn turn_on(&self, percentage: Option<i64>, preset_mode: Option<String>) -> BoxFuture<'_>;
    fn turn_off(&self) -> BoxFuture<'_>;
    fn set_percentage(&self, percentage: i64) -> BoxFuture<'_>;
    fn set_preset_mode(&self, preset_mode: &str) -> BoxFuture<'_>;
    fn oscillate(&self, oscillating: bool) -> BoxFuture<'_>;
    fn set_direction(&self, direction: &str) -> BoxFuture<'_>;
}

#[derive(Default)]
pub struct Fans {
    items: RwLock<Vec<Arc<dyn Fan>>>,
}

/// `_valid_preset_mode_or_raise`.
fn valid_preset(f: &dyn Fan, preset_mode: &str) -> Result<(), ServiceError> {
    let modes = f.preset_modes();
    if modes.iter().any(|m| m == preset_mode) {
        return Ok(());
    }
    let joined = modes.join(", ");
    let mut p = Map::new();
    p.insert("preset_mode".into(), json!(preset_mode));
    p.insert("preset_modes".into(), json!(joined));
    Err(ServiceError::validation(
        DOMAIN,
        "not_valid_preset_mode",
        Some(p),
        format!("Preset mode {preset_mode} is not valid, valid preset modes are: {joined}"),
    ))
}

/// `util.scaling.states_in_range`.
fn states(low: f64, high: f64) -> f64 {
    high - low + 1.0
}

/// `_async_adjust_speed`'s new percentage.
pub fn adjusted(current: i64, speed_count: u32, step: Option<i64>, modifier: i64) -> i64 {
    let new = match step {
        Some(step) => current + step * modifier,
        None => {
            let count = f64::from(speed_count);
            // `percentage_to_ranged_value((1, count), current)`, ceiled.
            let index = (current as f64 * states(1.0, count) / states(1.0, 100.0)).ceil();
            // `ranged_value_to_percentage((1, count), index + modifier)`.
            let value = index + modifier as f64;
            (value * states(1.0, 100.0) / states(1.0, count)).floor() as i64
        }
    };
    new.clamp(0, 100)
}

impl Fans {
    pub fn add(&self, f: Arc<dyn Fan>) {
        self.items.write().push(f);
    }

    pub fn remove(&self, entity_id: &EntityId) {
        self.items.write().retain(|f| f.entity_id() != entity_id);
    }

    pub fn register_services(
        self: &Arc<Self>,
        hass: &Hass,
        registries: impl Into<ha_registry::Lookup>,
    ) {
        use feature::*;
        let registries = registries.into();
        let entity = Schema::entity_service;
        let pct = || Kind::Int(Range::new(0.0, 100.0));
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
            let me = self.clone();
            let registries = registries.clone();
            hass.services().register_with_schema(
                DOMAIN,
                name,
                Some(schema),
                SupportsResponse::None,
                move |call| {
                    let me = me.clone();
                    let registries = registries.clone();
                    async move {
                        let target = call.entity_ids()?;
                        let indirect = registries.get().target_entities(&call.data);
                        let items = me.items.read().clone();
                        let want = Call {
                            domain: DOMAIN,
                            service: name,
                            target: &target,
                            indirect: &indirect,
                            required,
                        };
                        let chosen = chosen(
                            &want,
                            &items,
                            |f| f.entity_id(),
                            |f| f.available(),
                            |f| f.supported_features(),
                        )?;
                        let data = &call.data;
                        let int = |k: &str| data.get(k).and_then(Value::as_i64);
                        let text = |k: &str| data.get(k).and_then(Value::as_str);
                        let calls = chosen.iter().map(|f| async move {
                            match name {
                                "turn_on" => {
                                    let preset = text("preset_mode");
                                    if let Some(p) = preset {
                                        valid_preset(f.as_ref(), p)?;
                                    }
                                    f.turn_on(int("percentage"), preset.map(str::to_owned))
                                        .await
                                }
                                "turn_off" => f.turn_off().await,
                                "toggle" if f.is_on() => f.turn_off().await,
                                "toggle" => f.turn_on(None, None).await,
                                "increase_speed" | "decrease_speed" => {
                                    let modifier = if name == "increase_speed" { 1 } else { -1 };
                                    let current = f.percentage().unwrap_or(0);
                                    let new = adjusted(
                                        current,
                                        f.speed_count(),
                                        int("percentage_step"),
                                        modifier,
                                    );
                                    f.set_percentage(new).await
                                }
                                "oscillate" => {
                                    let on = data
                                        .get("oscillating")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false);
                                    f.oscillate(on).await
                                }
                                "set_direction" => {
                                    f.set_direction(text("direction").unwrap_or("")).await
                                }
                                "set_percentage" => {
                                    f.set_percentage(int("percentage").unwrap_or(0)).await
                                }
                                _ => {
                                    let preset = text("preset_mode").unwrap_or("");
                                    valid_preset(f.as_ref(), preset)?;
                                    f.set_preset_mode(preset).await
                                }
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

    #[test]
    fn speed_steps_as_ha() {
        // A 3-speed fan at 33 % steps up to 66 %, and from 100 % down to 66 %.
        assert_eq!(adjusted(33, 3, None, 1), 66);
        assert_eq!(adjusted(100, 3, None, -1), 66);
        assert_eq!(adjusted(0, 3, None, 1), 33);
        assert_eq!(adjusted(90, 100, Some(20), 1), 100);
    }

    struct Fake {
        id: EntityId,
        features: u32,
        calls: Mutex<Vec<String>>,
    }

    impl Fan for Fake {
        fn entity_id(&self) -> &EntityId {
            &self.id
        }
        fn available(&self) -> bool {
            true
        }
        fn supported_features(&self) -> u32 {
            self.features
        }
        fn is_on(&self) -> bool {
            false
        }
        fn preset_modes(&self) -> Vec<String> {
            vec!["auto".into()]
        }
        fn turn_on(&self, p: Option<i64>, m: Option<String>) -> BoxFuture<'_> {
            self.calls.lock().push(format!("on {p:?} {m:?}"));
            Box::pin(async { Ok(()) })
        }
        fn turn_off(&self) -> BoxFuture<'_> {
            self.calls.lock().push("off".into());
            Box::pin(async { Ok(()) })
        }
        fn set_percentage(&self, p: i64) -> BoxFuture<'_> {
            self.calls.lock().push(format!("pct {p}"));
            Box::pin(async { Ok(()) })
        }
        fn set_preset_mode(&self, m: &str) -> BoxFuture<'_> {
            self.calls.lock().push(format!("preset {m}"));
            Box::pin(async { Ok(()) })
        }
        fn oscillate(&self, _: bool) -> BoxFuture<'_> {
            Box::pin(async { Ok(()) })
        }
        fn set_direction(&self, _: &str) -> BoxFuture<'_> {
            Box::pin(async { Ok(()) })
        }
    }

    async fn call(hass: &Hass, service: &str, data: Value) -> Result<(), ServiceError> {
        let mut data = data.as_object().cloned().unwrap();
        data.insert("entity_id".into(), json!("fan.f"));
        hass.call_service(DOMAIN, service, data, None, false, None)
            .await
            .map(|_| ())
    }

    fn setup(features: u32) -> (Hass, Arc<Fake>) {
        let hass = Hass::new();
        let c = Arc::new(Fans::default());
        c.register_services(&hass, Arc::new(ha_registry::Registries::default()));
        let f = Arc::new(Fake {
            id: EntityId::parse("fan.f").unwrap(),
            features,
            calls: Mutex::default(),
        });
        c.add(f.clone());
        (hass, f)
    }

    #[tokio::test]
    async fn features_and_presets_are_checked() {
        use feature::*;
        let (hass, f) = setup(TURN_ON | TURN_OFF);
        call(&hass, "toggle", json!({})).await.unwrap();
        let e = call(&hass, "set_percentage", json!({"percentage": 50}))
            .await
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "Entity fan.f does not support action fan.set_percentage"
        );
        let e = call(&hass, "turn_on", json!({"preset_mode": "eco"}))
            .await
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "Preset mode eco is not valid, valid preset modes are: auto"
        );
        call(&hass, "turn_on", json!({"preset_mode": "auto"}))
            .await
            .unwrap();
        assert_eq!(*f.calls.lock(), ["on None None", "on None Some(\"auto\")"]);
    }
}

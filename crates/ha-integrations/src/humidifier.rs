//! The `humidifier` entity component (`components/humidifier/__init__.py`):
//! its services, run against the (de)humidifiers platforms provide. The
//! platforms write the states themselves.

use crate::climate::Temp;
use crate::light::BoxFuture;
use crate::switch::select;
use ha_core::{EntityId, EntityTarget, Hass, Kind, Range, Schema, ServiceError, SupportsResponse};
use parking_lot::RwLock;
use serde_json::{Map, Value, json};
use std::sync::Arc;

pub const DOMAIN: &str = "humidifier";

/// `HumidifierEntityFeature`.
pub mod feature {
    pub const MODES: u32 = 1;
}

/// HA's `DEFAULT_MIN_HUMIDITY` / `DEFAULT_MAX_HUMIDITY`.
pub const DEFAULT_MIN_HUMIDITY: Temp = Temp::Int(0);
pub const DEFAULT_MAX_HUMIDITY: Temp = Temp::Int(100);

/// A (de)humidifier a platform runs.
pub trait Humidifier: Send + Sync {
    fn entity_id(&self) -> &EntityId;
    fn available(&self) -> bool;
    fn supported_features(&self) -> u32;
    /// `is_on`: `toggle` turns off only what is on.
    fn is_on(&self) -> bool;
    fn min_humidity(&self) -> Temp {
        DEFAULT_MIN_HUMIDITY
    }
    fn max_humidity(&self) -> Temp {
        DEFAULT_MAX_HUMIDITY
    }
    fn turn(&self, on: bool) -> BoxFuture<'_>;
    fn set_humidity(&self, humidity: i64) -> BoxFuture<'_>;
    fn set_mode(&self, mode: &str) -> BoxFuture<'_>;
}

#[derive(Default)]
pub struct Humidifiers {
    items: RwLock<Vec<Arc<dyn Humidifier>>>,
}

/// One service call's target and the features it needs.
pub(crate) struct Call<'a> {
    pub domain: &'a str,
    pub service: &'a str,
    pub target: &'a EntityTarget,
    pub indirect: &'a [String],
    pub required: &'a [u32],
}

/// HA's entity-service selection with required features: a targeted
/// entity lacking every feature set is skipped, or, when named directly,
/// refused with `ServiceNotSupported`.
pub(crate) fn chosen<T: ?Sized>(
    call: &Call<'_>,
    items: &[Arc<T>],
    id: impl Fn(&T) -> &EntityId + Copy,
    available: impl Fn(&T) -> bool,
    features: impl Fn(&T) -> u32,
) -> Result<Vec<Arc<T>>, ServiceError> {
    let mut out = Vec::new();
    for c in select(items, id, available, call.target, call.indirect) {
        let f = features(&c);
        if !call.required.is_empty() && !call.required.iter().any(|r| f & r == *r) {
            if let EntityTarget::Ids(ids) = call.target
                && ids.contains(id(&c))
            {
                return Err(ServiceError::not_supported(
                    call.domain,
                    call.service,
                    id(&c).as_str(),
                ));
            }
            continue;
        }
        out.push(c);
    }
    Ok(out)
}

/// `async_service_humidity_set`'s range check, then the entity's call.
async fn set_humidity(h: &dyn Humidifier, data: &Map<String, Value>) -> Result<(), ServiceError> {
    let humidity = data.get("humidity").and_then(Value::as_i64).unwrap_or(0);
    let (min, max) = (h.min_humidity(), h.max_humidity());
    if (humidity as f64) < min.get() || (humidity as f64) > max.get() {
        let mut p = Map::new();
        p.insert("humidity".into(), json!(humidity.to_string()));
        p.insert("min_humidity".into(), json!(min.py_str()));
        p.insert("max_humidity".into(), json!(max.py_str()));
        return Err(ServiceError::validation(
            DOMAIN,
            "humidity_out_of_range",
            Some(p),
            format!(
                "Provided humidity {humidity} is not valid. Accepted range is {} to {}",
                min.py_str(),
                max.py_str()
            ),
        ));
    }
    h.set_humidity(humidity).await
}

impl Humidifiers {
    pub fn add(&self, h: Arc<dyn Humidifier>) {
        self.items.write().push(h);
    }

    pub fn remove(&self, entity_id: &EntityId) {
        self.items.write().retain(|h| h.entity_id() != entity_id);
    }

    pub fn register_services(
        self: &Arc<Self>,
        hass: &Hass,
        registries: impl Into<ha_registry::Lookup>,
    ) {
        let registries = registries.into();
        let entity = Schema::entity_service;
        let services: [(&'static str, Schema, &'static [u32]); 5] = [
            ("turn_on", entity(), &[]),
            ("turn_off", entity(), &[]),
            ("toggle", entity(), &[]),
            (
                "set_mode",
                entity().required("mode", Kind::String),
                &[feature::MODES],
            ),
            (
                "set_humidity",
                entity().required("humidity", Kind::Int(Range::new(0.0, 100.0))),
                &[],
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
                            |h| h.entity_id(),
                            |h| h.available(),
                            |h| h.supported_features(),
                        )?;
                        let data = &call.data;
                        let calls = chosen.iter().map(|h| async move {
                            match name {
                                "turn_on" => h.turn(true).await,
                                "turn_off" => h.turn(false).await,
                                "toggle" => h.turn(!h.is_on()).await,
                                "set_mode" => {
                                    let mode =
                                        data.get("mode").and_then(Value::as_str).unwrap_or("");
                                    h.set_mode(mode).await
                                }
                                _ => set_humidity(h.as_ref(), data).await,
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

    struct Fake {
        id: EntityId,
        on: Mutex<bool>,
        calls: Mutex<Vec<String>>,
        features: u32,
    }

    impl Humidifier for Fake {
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
            *self.on.lock()
        }
        fn min_humidity(&self) -> Temp {
            Temp::Int(35)
        }
        fn max_humidity(&self) -> Temp {
            Temp::Int(70)
        }
        fn turn(&self, on: bool) -> BoxFuture<'_> {
            *self.on.lock() = on;
            self.calls.lock().push(format!("turn {on}"));
            Box::pin(async { Ok(()) })
        }
        fn set_humidity(&self, h: i64) -> BoxFuture<'_> {
            self.calls.lock().push(format!("humidity {h}"));
            Box::pin(async { Ok(()) })
        }
        fn set_mode(&self, m: &str) -> BoxFuture<'_> {
            self.calls.lock().push(format!("mode {m}"));
            Box::pin(async { Ok(()) })
        }
    }

    fn setup(features: u32) -> (Hass, Arc<Fake>) {
        let hass = Hass::new();
        let c = Arc::new(Humidifiers::default());
        c.register_services(&hass, Arc::new(ha_registry::Registries::default()));
        let fake = Arc::new(Fake {
            id: EntityId::parse("humidifier.dry").unwrap(),
            on: Mutex::new(false),
            calls: Mutex::default(),
            features,
        });
        c.add(fake.clone());
        (hass, fake)
    }

    async fn call(hass: &Hass, service: &str, data: Value) -> Result<(), ServiceError> {
        hass.call_service(
            DOMAIN,
            service,
            data.as_object().cloned().unwrap(),
            None,
            false,
            None,
        )
        .await
        .map(|_| ())
    }

    #[tokio::test]
    async fn toggles_and_sets_humidity_in_range() {
        let (hass, fake) = setup(0);
        call(&hass, "toggle", json!({"entity_id": "humidifier.dry"}))
            .await
            .unwrap();
        call(
            &hass,
            "set_humidity",
            json!({"entity_id": "humidifier.dry", "humidity": 50}),
        )
        .await
        .unwrap();
        assert_eq!(*fake.calls.lock(), ["turn true", "humidity 50"]);
        let e = call(
            &hass,
            "set_humidity",
            json!({"entity_id": "humidifier.dry", "humidity": 80}),
        )
        .await
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            "Provided humidity 80 is not valid. Accepted range is 35 to 70"
        );
    }

    #[tokio::test]
    async fn set_mode_needs_the_feature() {
        let (hass, _) = setup(0);
        let e = call(
            &hass,
            "set_mode",
            json!({"entity_id": "humidifier.dry", "mode": "auto"}),
        )
        .await
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            "Entity humidifier.dry does not support action humidifier.set_mode"
        );
        let (hass, fake) = setup(feature::MODES);
        call(
            &hass,
            "set_mode",
            json!({"entity_id": "humidifier.dry", "mode": "auto"}),
        )
        .await
        .unwrap();
        assert_eq!(*fake.calls.lock(), ["mode auto"]);
    }
}

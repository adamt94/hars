//! The `number` entity component (`components/number/__init__.py` and
//! `components/number/const.py`): `number.set_value` with HA's min/max range
//! validation and default step calculation.

use crate::light::BoxFuture;
use crate::switch::select;
use ha_core::{EntityId, Hass, Kind, Range, Schema, ServiceError, SupportsResponse};
use parking_lot::RwLock;
use serde_json::{Map, Value, json};
use std::sync::Arc;

pub const DOMAIN: &str = "number";

/// HA's `DEFAULT_MIN_VALUE`, `DEFAULT_MAX_VALUE`, and `DEFAULT_STEP`.
pub const DEFAULT_MIN_VALUE: f64 = 0.0;
pub const DEFAULT_MAX_VALUE: f64 = 100.0;
pub const DEFAULT_STEP: f64 = 1.0;

/// HA's `NumberEntity._calculate_step`.
pub fn calculate_step(min_value: f64, max_value: f64, native_step: Option<f64>) -> f64 {
    if let Some(step) = native_step {
        return step;
    }
    let mut step = DEFAULT_STEP;
    let value_range = (max_value - min_value).abs();
    if value_range != 0.0 && value_range.is_finite() {
        while value_range <= step && step > f64::MIN_POSITIVE {
            step /= 10.0;
        }
    }
    step
}

/// A number entity a platform runs (`NumberEntity`).
pub trait Number: Send + Sync {
    fn entity_id(&self) -> &EntityId;
    fn available(&self) -> bool;
    fn min_value(&self) -> f64 {
        DEFAULT_MIN_VALUE
    }
    fn max_value(&self) -> f64 {
        DEFAULT_MAX_VALUE
    }
    fn step(&self) -> f64 {
        calculate_step(self.min_value(), self.max_value(), None)
    }
    fn mode(&self) -> &str {
        "auto"
    }
    fn set_value(&self, value: f64) -> BoxFuture<'_>;
}

/// The running number entities, for the services.
#[derive(Default)]
pub struct Numbers {
    items: RwLock<Vec<Arc<dyn Number>>>,
}

impl Numbers {
    pub fn add(&self, n: Arc<dyn Number>) {
        self.items.write().push(n);
    }

    pub fn remove(&self, entity_id: &EntityId) {
        self.items.write().retain(|n| n.entity_id() != entity_id);
    }

    /// Register HA's `number.set_value` service.
    pub fn register_services(
        self: &Arc<Self>,
        hass: &Hass,
        registries: impl Into<ha_registry::Lookup>,
    ) {
        let registries = registries.into();
        let numbers = self.clone();
        hass.services().register_with_schema(
            DOMAIN,
            "set_value",
            Some(Schema::entity_service().required("value", Kind::Float(Range::ANY))),
            SupportsResponse::None,
            move |call| {
                let numbers = numbers.clone();
                let registries = registries.clone();
                async move {
                    let target = call.entity_ids()?;
                    let indirect = registries.get().target_entities(&call.data);
                    let items = numbers.items.read().clone();
                    let chosen = select(
                        &items,
                        |n| n.entity_id(),
                        |n| n.available(),
                        &target,
                        &indirect,
                    );
                    let value = call
                        .data
                        .get("value")
                        .and_then(Value::as_f64)
                        .unwrap_or(f64::NAN);
                    let calls = chosen.iter().map(|n| async move {
                        let min = n.min_value();
                        let max = n.max_value();
                        if !value.is_finite() || value < min || value > max {
                            let val_s = crate::sun::py_float(value);
                            let min_s = crate::sun::py_float(min);
                            let max_s = crate::sun::py_float(max);
                            let mut p = Map::new();
                            // HA passes the coerced float directly for `value`, and `str()`
                            // for `min_value` / `max_value` (components/number/__init__.py).
                            p.insert(
                                "value".into(),
                                if value.is_finite() {
                                    json!(value)
                                } else {
                                    json!(val_s)
                                },
                            );
                            p.insert("entity_id".into(), json!(n.entity_id().as_str()));
                            p.insert("min_value".into(), json!(min_s));
                            p.insert("max_value".into(), json!(max_s));
                            return Err(ServiceError::validation(
                                DOMAIN,
                                "out_of_range",
                                Some(p),
                                format!(
                                    "Value {val_s} for {} is outside valid range {min_s} - {max_s}",
                                    n.entity_id().as_str()
                                ),
                            ));
                        }
                        n.set_value(value).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    struct FakeNumber {
        id: EntityId,
        available: bool,
        min: f64,
        max: f64,
        calls: Mutex<Vec<f64>>,
    }

    impl FakeNumber {
        fn new(id: &str, available: bool, min: f64, max: f64) -> Arc<Self> {
            Arc::new(Self {
                id: EntityId::parse(id).unwrap(),
                available,
                min,
                max,
                calls: Mutex::new(Vec::new()),
            })
        }
    }

    impl Number for FakeNumber {
        fn entity_id(&self) -> &EntityId {
            &self.id
        }
        fn available(&self) -> bool {
            self.available
        }
        fn min_value(&self) -> f64 {
            self.min
        }
        fn max_value(&self) -> f64 {
            self.max
        }
        fn set_value(&self, value: f64) -> BoxFuture<'_> {
            self.calls.lock().push(value);
            Box::pin(async { Ok(()) })
        }
    }

    fn map_of(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn calculate_step_matches_ha() {
        assert_eq!(calculate_step(0.0, 100.0, None), 1.0);
        assert_eq!(calculate_step(0.0, 1.0, None), 0.1);
        assert_eq!(calculate_step(0.0, 0.1, None), 0.01);
        assert_eq!(calculate_step(0.0, 100.0, Some(0.5)), 0.5);
        assert_eq!(calculate_step(5.0, 5.0, None), 1.0);
    }

    #[tokio::test]
    async fn set_value_validates_range_and_dispatches() {
        let hass = Hass::new();
        let numbers = Arc::new(Numbers::default());
        numbers.register_services(&hass, Arc::new(ha_registry::Registries::default()));

        let n = FakeNumber::new("number.volume", true, 0.0, 10.0);
        let offline = FakeNumber::new("number.offline", false, 0.0, 100.0);
        numbers.add(n.clone());
        numbers.add(offline.clone());

        // Numeric and coerced string inputs at bounds succeed.
        hass.call_service(
            "number",
            "set_value",
            map_of(json!({"entity_id": "number.volume", "value": 0})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        hass.call_service(
            "number",
            "set_value",
            map_of(json!({"entity_id": "number.volume", "value": "7.5"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        hass.call_service(
            "number",
            "set_value",
            map_of(json!({"entity_id": "number.volume", "value": 10.0})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        assert_eq!(*n.calls.lock(), vec![0.0, 7.5, 10.0]);

        // Out of range raises HA's out_of_range ServiceValidationError.
        let err = hass
            .call_service(
                "number",
                "set_value",
                map_of(json!({"entity_id": "number.volume", "value": 11})),
                None,
                false,
                None,
            )
            .await
            .unwrap_err();
        match err {
            ServiceError::Validation(r) => {
                assert_eq!(r.domain.as_str(), "number");
                assert_eq!(r.key.as_str(), "out_of_range");
                assert_eq!(
                    r.message,
                    "Value 11.0 for number.volume is outside valid range 0.0 - 10.0"
                );
                assert_eq!(
                    json!(r.placeholders),
                    json!({
                        "value": 11.0,
                        "entity_id": "number.volume",
                        "min_value": "0.0",
                        "max_value": "10.0",
                    })
                );
            }
            other => panic!("expected validation error, got {other:?}"),
        }

        // Missing or non-numeric value fails schema validation.
        let bad = hass
            .call_service(
                "number",
                "set_value",
                map_of(json!({"entity_id": "number.volume", "value": "abc"})),
                None,
                false,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(bad, ServiceError::Invalid(_)));

        // Unavailable entity is skipped.
        hass.call_service(
            "number",
            "set_value",
            map_of(json!({"entity_id": "all", "value": 5})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        assert!(offline.calls.lock().is_empty());

        numbers.remove(&n.id);
        let before = n.calls.lock().len();
        hass.call_service(
            "number",
            "set_value",
            map_of(json!({"entity_id": "number.volume", "value": 3})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        assert_eq!(n.calls.lock().len(), before);
    }

    #[test]
    fn services_hold_no_cycle() {
        let hass = Hass::new();
        let numbers = Arc::new(Numbers::default());
        numbers.register_services(&hass, Arc::new(ha_registry::Registries::default()));
        let weak = Arc::downgrade(&numbers);
        drop((hass, numbers));
        assert!(weak.upgrade().is_none());
    }
}

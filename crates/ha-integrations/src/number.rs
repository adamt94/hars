//! The `number` entity component (`components/number/__init__.py`): its
//! `set_value` service, run against the numbers platforms provide. The
//! platforms write the states themselves.

use crate::light::BoxFuture;
use crate::switch::select;
use ha_core::{EntityId, Hass, Kind, Range, Schema, ServiceError, SupportsResponse};
use parking_lot::RwLock;
use serde_json::{Map, Value, json};
use std::sync::Arc;

pub const DOMAIN: &str = "number";

/// A number a platform runs. Values are in the state's unit.
pub trait Number: Send + Sync {
    fn entity_id(&self) -> &EntityId;
    fn available(&self) -> bool;
    fn min_value(&self) -> f64;
    fn max_value(&self) -> f64;
    fn set_value(&self, value: f64) -> BoxFuture<'_>;
}

#[derive(Default)]
pub struct Numbers {
    items: RwLock<Vec<Arc<dyn Number>>>,
}

/// `async_set_value`'s range check, then the entity's call.
async fn set_value(n: &dyn Number, value: f64) -> Result<(), ServiceError> {
    let (min, max) = (n.min_value(), n.max_value());
    if value < min || value > max {
        let py = crate::sun::py_float;
        let mut p = Map::new();
        p.insert("value".into(), json!(value));
        p.insert("entity_id".into(), json!(n.entity_id().as_str()));
        p.insert("min_value".into(), json!(py(min)));
        p.insert("max_value".into(), json!(py(max)));
        return Err(ServiceError::validation(
            DOMAIN,
            "out_of_range",
            Some(p),
            format!(
                "Value {} for {} is outside valid range {} - {}",
                py(value),
                n.entity_id(),
                py(min),
                py(max)
            ),
        ));
    }
    n.set_value(value).await
}

impl Numbers {
    pub fn add(&self, n: Arc<dyn Number>) {
        self.items.write().push(n);
    }

    pub fn remove(&self, entity_id: &EntityId) {
        self.items.write().retain(|n| n.entity_id() != entity_id);
    }

    pub fn register_services(
        self: &Arc<Self>,
        hass: &Hass,
        registries: impl Into<ha_registry::Lookup>,
    ) {
        let registries = registries.into();
        let me = self.clone();
        hass.services().register_with_schema(
            DOMAIN,
            "set_value",
            Some(Schema::entity_service().required("value", Kind::Float(Range::ANY))),
            SupportsResponse::None,
            move |call| {
                let me = me.clone();
                let registries = registries.clone();
                async move {
                    let target = call.entity_ids()?;
                    let indirect = registries.get().target_entities(&call.data);
                    let items = me.items.read().clone();
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
                        .unwrap_or(0.0);
                    let calls = chosen.iter().map(|n| set_value(n.as_ref(), value));
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

    struct Fake {
        id: EntityId,
        set: Mutex<Vec<f64>>,
    }

    impl Number for Fake {
        fn entity_id(&self) -> &EntityId {
            &self.id
        }
        fn available(&self) -> bool {
            true
        }
        fn min_value(&self) -> f64 {
            1.0
        }
        fn max_value(&self) -> f64 {
            20.0
        }
        fn set_value(&self, v: f64) -> BoxFuture<'_> {
            self.set.lock().push(v);
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn checks_the_range() {
        let hass = Hass::new();
        let c = Arc::new(Numbers::default());
        c.register_services(&hass, Arc::new(ha_registry::Registries::default()));
        let f = Arc::new(Fake {
            id: EntityId::parse("number.feed").unwrap(),
            set: Mutex::default(),
        });
        c.add(f.clone());
        let call = |v: Value| {
            let data = json!({"entity_id": "number.feed", "value": v});
            hass.call_service(
                DOMAIN,
                "set_value",
                data.as_object().cloned().unwrap(),
                None,
                false,
                None,
            )
        };
        call(json!("3")).await.unwrap();
        let e = call(json!(25)).await.unwrap_err();
        assert_eq!(
            e.to_string(),
            "Value 25.0 for number.feed is outside valid range 1.0 - 20.0"
        );
        assert_eq!(*f.set.lock(), [3.0]);
    }
}

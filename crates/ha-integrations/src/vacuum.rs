//! The `vacuum` entity component (`components/vacuum/__init__.py` and
//! `components/vacuum/const.py`): `vacuum.start`, `vacuum.pause`,
//! `vacuum.stop`, `vacuum.return_to_base`, `vacuum.clean_spot`,
//! `vacuum.locate`, `vacuum.set_fan_speed`, and `vacuum.send_command`.
//!
//! Deliberately omitted for now: `vacuum.clean_area` and the
//! `vacuum/get_segments` WebSocket command (which depend on entity-registry
//! `options` editing and repair flows that `hars` does not implement yet;
//! segment/room cleaning is available via `vacuum.send_command`).

use crate::light::BoxFuture;
use crate::switch::select;
use ha_core::{EntityId, EntityTarget, Hass, Kind, Schema, ServiceError, SupportsResponse};
use parking_lot::RwLock;
use serde_json::Value;
use std::sync::Arc;

pub const DOMAIN: &str = "vacuum";

/// `VacuumEntityFeature` (`components/vacuum/const.py`).
pub mod feature {
    pub const TURN_ON: u32 = 1;
    pub const TURN_OFF: u32 = 2;
    pub const PAUSE: u32 = 4;
    pub const STOP: u32 = 8;
    pub const RETURN_HOME: u32 = 16;
    pub const FAN_SPEED: u32 = 32;
    pub const BATTERY: u32 = 64;
    pub const STATUS: u32 = 128;
    pub const SEND_COMMAND: u32 = 256;
    pub const LOCATE: u32 = 512;
    pub const CLEAN_SPOT: u32 = 1024;
    pub const MAP: u32 = 2048;
    pub const STATE: u32 = 4096;
    pub const START: u32 = 8192;
    pub const CLEAN_AREA: u32 = 16384;
}

/// A method the entity doesn't implement (Python's `NotImplementedError`).
fn unsupported<'a>() -> BoxFuture<'a> {
    Box::pin(async { Err(ServiceError::Unknown(String::new())) })
}

/// A vacuum entity a platform runs (`StateVacuumEntity`).
pub trait Vacuum: Send + Sync {
    fn entity_id(&self) -> &EntityId;
    fn available(&self) -> bool;
    fn supported_features(&self) -> u32;

    fn start(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn pause(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn stop(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn return_to_base(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn clean_spot(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn locate(&self) -> BoxFuture<'_> {
        unsupported()
    }
    fn set_fan_speed(&self, _fan_speed: &str) -> BoxFuture<'_> {
        unsupported()
    }
    fn send_command(&self, _command: &str, _params: Option<Value>) -> BoxFuture<'_> {
        unsupported()
    }
}

/// The running vacuums, for the services.
#[derive(Default)]
pub struct Vacuums {
    items: RwLock<Vec<Arc<dyn Vacuum>>>,
}

/// HA's entity-service selection with required features: a targeted entity
/// lacking the required feature is skipped when targeted indirectly, or
/// refused with `ServiceNotSupported` when named directly.
fn chosen(
    items: &[Arc<dyn Vacuum>],
    target: &EntityTarget,
    indirect: &[String],
    required: u32,
    service: &str,
) -> Result<Vec<Arc<dyn Vacuum>>, ServiceError> {
    let mut out = Vec::new();
    for v in select(
        items,
        |v| v.entity_id(),
        |v| v.available(),
        target,
        indirect,
    ) {
        if required != 0 && (v.supported_features() & required) != required {
            if let EntityTarget::Ids(ids) = target
                && ids.contains(v.entity_id())
            {
                return Err(ServiceError::not_supported(
                    DOMAIN,
                    service,
                    v.entity_id().as_str(),
                ));
            }
            continue;
        }
        out.push(v);
    }
    Ok(out)
}

/// Normalize `vacuum.send_command`'s optional `params` field the way HA's
/// `vol.Any(dict, cv.ensure_list)` does: objects and arrays pass through,
/// `null` becomes `[]`, and any scalar is wrapped in a one-element list.
fn normalize_command_params(v: Option<&Value>) -> Option<Value> {
    let val = v?;
    Some(match val {
        Value::Object(_) | Value::Array(_) => val.clone(),
        Value::Null => Value::Array(Vec::new()),
        scalar => Value::Array(vec![scalar.clone()]),
    })
}

impl Vacuums {
    pub fn add(&self, v: Arc<dyn Vacuum>) {
        self.items.write().push(v);
    }

    pub fn remove(&self, entity_id: &EntityId) {
        self.items.write().retain(|v| v.entity_id() != entity_id);
    }

    /// Register HA's `vacuum.*` entity services.
    pub fn register_services(
        self: &Arc<Self>,
        hass: &Hass,
        registries: impl Into<ha_registry::Lookup>,
    ) {
        let registries = registries.into();
        let entity = Schema::entity_service;
        let services: [(&'static str, Schema, u32); 8] = [
            ("start", entity(), feature::START),
            ("pause", entity(), feature::PAUSE),
            ("return_to_base", entity(), feature::RETURN_HOME),
            ("clean_spot", entity(), feature::CLEAN_SPOT),
            ("locate", entity(), feature::LOCATE),
            ("stop", entity(), feature::STOP),
            (
                "set_fan_speed",
                entity().required("fan_speed", Kind::String),
                feature::FAN_SPEED,
            ),
            (
                "send_command",
                entity()
                    .required("command", Kind::String)
                    .optional("params", Kind::Any),
                feature::SEND_COMMAND,
            ),
        ];

        for (name, schema, required) in services {
            let vacuums = self.clone();
            let registries = registries.clone();
            hass.services().register_with_schema(
                DOMAIN,
                name,
                Some(schema),
                SupportsResponse::None,
                move |call| {
                    let vacuums = vacuums.clone();
                    let registries = registries.clone();
                    async move {
                        let target = call.entity_ids()?;
                        let indirect = registries.get().target_entities(&call.data);
                        let items = vacuums.items.read().clone();
                        let chosen = chosen(&items, &target, &indirect, required, name)?;
                        let data = &call.data;
                        let str_of = |k: &str| data.get(k).and_then(Value::as_str).unwrap_or("");
                        let calls = chosen.iter().map(|v| async move {
                            match name {
                                "start" => v.start().await,
                                "pause" => v.pause().await,
                                "stop" => v.stop().await,
                                "return_to_base" => v.return_to_base().await,
                                "clean_spot" => v.clean_spot().await,
                                "locate" => v.locate().await,
                                "set_fan_speed" => v.set_fan_speed(str_of("fan_speed")).await,
                                "send_command" => {
                                    let params = normalize_command_params(data.get("params"));
                                    v.send_command(str_of("command"), params).await
                                }
                                _ => Err(ServiceError::Unknown(format!(
                                    "vacuum.{name} is not supported"
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

    struct FakeVacuum {
        id: EntityId,
        available: bool,
        features: u32,
        calls: Mutex<Vec<String>>,
    }

    impl FakeVacuum {
        fn new(id: &str, available: bool, features: u32) -> Arc<Self> {
            Arc::new(Self {
                id: EntityId::parse(id).unwrap(),
                available,
                features,
                calls: Mutex::new(Vec::new()),
            })
        }

        fn record(&self, entry: impl Into<String>) -> BoxFuture<'_> {
            self.calls.lock().push(entry.into());
            Box::pin(async { Ok(()) })
        }
    }

    impl Vacuum for FakeVacuum {
        fn entity_id(&self) -> &EntityId {
            &self.id
        }
        fn available(&self) -> bool {
            self.available
        }
        fn supported_features(&self) -> u32 {
            self.features
        }
        fn start(&self) -> BoxFuture<'_> {
            self.record("start")
        }
        fn pause(&self) -> BoxFuture<'_> {
            self.record("pause")
        }
        fn stop(&self) -> BoxFuture<'_> {
            self.record("stop")
        }
        fn return_to_base(&self) -> BoxFuture<'_> {
            self.record("return_to_base")
        }
        fn clean_spot(&self) -> BoxFuture<'_> {
            self.record("clean_spot")
        }
        fn locate(&self) -> BoxFuture<'_> {
            self.record("locate")
        }
        fn set_fan_speed(&self, fan_speed: &str) -> BoxFuture<'_> {
            self.record(format!("set_fan_speed:{fan_speed}"))
        }
        fn send_command(&self, command: &str, params: Option<Value>) -> BoxFuture<'_> {
            self.record(format!("send_command:{command}:{params:?}"))
        }
    }

    fn map_of(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[tokio::test]
    async fn dispatches_all_services_and_checks_features() {
        let hass = Hass::new();
        let vacuums = Arc::new(Vacuums::default());
        vacuums.register_services(&hass, Arc::new(ha_registry::Registries::default()));

        let all_features = feature::START
            | feature::PAUSE
            | feature::STOP
            | feature::RETURN_HOME
            | feature::CLEAN_SPOT
            | feature::LOCATE
            | feature::FAN_SPEED
            | feature::SEND_COMMAND
            | feature::STATE;
        let v1 = FakeVacuum::new("vacuum.roborock", true, all_features);
        let v_minimal = FakeVacuum::new("vacuum.basic", true, feature::START | feature::STATE);
        let v_offline = FakeVacuum::new("vacuum.offline", false, all_features);
        vacuums.add(v1.clone());
        vacuums.add(v_minimal.clone());
        vacuums.add(v_offline.clone());

        for svc in [
            "start",
            "pause",
            "stop",
            "return_to_base",
            "clean_spot",
            "locate",
        ] {
            hass.call_service(
                "vacuum",
                svc,
                map_of(json!({"entity_id": "vacuum.roborock"})),
                None,
                false,
                None,
            )
            .await
            .unwrap();
        }
        hass.call_service(
            "vacuum",
            "set_fan_speed",
            map_of(json!({"entity_id": "vacuum.roborock", "fan_speed": "turbo"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        hass.call_service(
            "vacuum",
            "send_command",
            map_of(json!({
                "entity_id": "vacuum.roborock",
                "command": "app_segment_clean",
                "params": [{"segments": [16, 17]}],
            })),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        // Scalar params wrapped as a 1-element list (HA's cv.ensure_list).
        hass.call_service(
            "vacuum",
            "send_command",
            map_of(json!({
                "entity_id": "vacuum.roborock",
                "command": "find_me",
                "params": "loud",
            })),
            None,
            false,
            None,
        )
        .await
        .unwrap();

        let calls = v1.calls.lock().clone();
        assert_eq!(
            calls,
            vec![
                "start",
                "pause",
                "stop",
                "return_to_base",
                "clean_spot",
                "locate",
                "set_fan_speed:turbo",
                r#"send_command:app_segment_clean:Some(Array [Object {"segments": Array [Number(16), Number(17)]}])"#,
                r#"send_command:find_me:Some(Array [String("loud")])"#,
            ]
        );

        // Directly targeting an entity lacking a feature returns ServiceNotSupported.
        let err = hass
            .call_service(
                "vacuum",
                "locate",
                map_of(json!({"entity_id": "vacuum.basic"})),
                None,
                false,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ServiceError::not_supported("vacuum", "locate", "vacuum.basic")
        );

        // Targeting `all` skips entities lacking the feature and unavailable ones.
        hass.call_service(
            "vacuum",
            "locate",
            map_of(json!({"entity_id": "all"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        assert!(v_minimal.calls.lock().is_empty());
        assert!(v_offline.calls.lock().is_empty());

        // Schema rejects missing required arguments.
        let bad = hass
            .call_service(
                "vacuum",
                "set_fan_speed",
                map_of(json!({"entity_id": "vacuum.roborock"})),
                None,
                false,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(bad, ServiceError::Invalid(_)));

        // Removal stops further service delivery.
        vacuums.remove(&v1.id);
        let before = v1.calls.lock().len();
        hass.call_service(
            "vacuum",
            "start",
            map_of(json!({"entity_id": "vacuum.roborock"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        assert_eq!(v1.calls.lock().len(), before);
    }

    #[test]
    fn services_hold_no_cycle() {
        let hass = Hass::new();
        let vacuums = Arc::new(Vacuums::default());
        vacuums.register_services(&hass, Arc::new(ha_registry::Registries::default()));
        let weak = Arc::downgrade(&vacuums);
        drop((hass, vacuums));
        assert!(weak.upgrade().is_none());
    }
}

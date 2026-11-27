//! The `select` entity component (`components/select/__init__.py` and
//! `components/select/const.py`): `select.select_option`,
//! `select.select_first`, `select.select_last`, `select.select_next`, and
//! `select.select_previous`.

use crate::light::BoxFuture;
use crate::switch::select;
use ha_core::{EntityId, Hass, Kind, Schema, ServiceError, SupportsResponse};
use parking_lot::RwLock;
use serde_json::{Map, Value, json};
use std::sync::Arc;

pub const DOMAIN: &str = "select";

/// A select entity a platform runs (`SelectEntity`).
pub trait Select: Send + Sync {
    fn entity_id(&self) -> &EntityId;
    fn available(&self) -> bool;
    /// Selectable options in order.
    fn options(&self) -> Vec<String>;
    /// Currently selected option, or `None` when unknown.
    fn current_option(&self) -> Option<String>;
    /// Change the selected option (`async_select_option`).
    fn select_option(&self, option: &str) -> BoxFuture<'_>;
}

/// The running select entities, for the services.
#[derive(Default)]
pub struct Selects {
    items: RwLock<Vec<Arc<dyn Select>>>,
}

/// HA's `SelectEntity._valid_option_or_raise`.
fn validate_option(
    entity_id: &EntityId,
    option: &str,
    options: &[String],
) -> Result<(), ServiceError> {
    if !options.is_empty() && options.iter().any(|o| o == option) {
        return Ok(());
    }
    let friendly_options = options.join(", ");
    let mut p = Map::new();
    p.insert("entity_id".into(), json!(entity_id.as_str()));
    p.insert("option".into(), json!(option));
    p.insert("options".into(), json!(friendly_options));
    Err(ServiceError::validation(
        DOMAIN,
        "not_valid_option",
        Some(p),
        format!(
            "Option {option} is not valid for entity {}, valid options are: {friendly_options}",
            entity_id.as_str()
        ),
    ))
}

/// Pick the option at `idx` (Python modulo indexing) or fail cleanly when
/// `options` is empty (matching Python 3.14 `ZeroDivisionError: division by zero`).
fn pick_index(options: &[String], idx: isize) -> Result<&str, ServiceError> {
    let len = options.len() as isize;
    if len == 0 {
        return Err(ServiceError::Unknown("division by zero".into()));
    }
    let i = idx.rem_euclid(len) as usize;
    Ok(&options[i])
}

/// HA's `SelectEntity._async_offset_index`.
fn offset_option<'a>(
    options: &'a [String],
    current: Option<&str>,
    offset: isize,
    cycle: bool,
) -> Result<&'a str, ServiceError> {
    let len = options.len() as isize;
    if len == 0 {
        let msg = if cycle {
            "division by zero"
        } else {
            "list index out of range"
        };
        return Err(ServiceError::Unknown(msg.into()));
    }
    let current_index = current
        .and_then(|c| options.iter().position(|o| o == c))
        .unwrap_or(0) as isize;
    let new_index = current_index + offset;
    let idx = if cycle {
        new_index.rem_euclid(len) as usize
    } else if new_index < 0 {
        0
    } else if new_index >= len {
        (len - 1) as usize
    } else {
        new_index as usize
    };
    Ok(&options[idx])
}

impl Selects {
    pub fn add(&self, s: Arc<dyn Select>) {
        self.items.write().push(s);
    }

    pub fn remove(&self, entity_id: &EntityId) {
        self.items.write().retain(|s| s.entity_id() != entity_id);
    }

    /// Register HA's five `select.*` entity services.
    pub fn register_services(
        self: &Arc<Self>,
        hass: &Hass,
        registries: impl Into<ha_registry::Lookup>,
    ) {
        let registries = registries.into();
        let entity = Schema::entity_service;
        let services: [(&'static str, Schema); 5] = [
            ("select_first", entity()),
            ("select_last", entity()),
            ("select_next", entity().optional("cycle", Kind::StrictBool)),
            ("select_option", entity().required("option", Kind::String)),
            (
                "select_previous",
                entity().optional("cycle", Kind::StrictBool),
            ),
        ];

        for (name, schema) in services {
            let selects = self.clone();
            let registries = registries.clone();
            hass.services().register_with_schema(
                DOMAIN,
                name,
                Some(schema),
                SupportsResponse::None,
                move |call| {
                    let selects = selects.clone();
                    let registries = registries.clone();
                    async move {
                        let target = call.entity_ids()?;
                        let indirect = registries.get().target_entities(&call.data);
                        let items = selects.items.read().clone();
                        let chosen = select(
                            &items,
                            |s| s.entity_id(),
                            |s| s.available(),
                            &target,
                            &indirect,
                        );
                        let data = &call.data;
                        let calls = chosen.iter().map(|s| async move {
                            let options = s.options();
                            match name {
                                "select_option" => {
                                    let option =
                                        data.get("option").and_then(Value::as_str).unwrap_or("");
                                    validate_option(s.entity_id(), option, &options)?;
                                    s.select_option(option).await
                                }
                                "select_first" => {
                                    let target_opt = pick_index(&options, 0)?;
                                    s.select_option(target_opt).await
                                }
                                "select_last" => {
                                    let target_opt = pick_index(&options, -1)?;
                                    s.select_option(target_opt).await
                                }
                                "select_next" => {
                                    let cycle =
                                        data.get("cycle").and_then(Value::as_bool).unwrap_or(true);
                                    let current = s.current_option();
                                    let target_opt = match current.as_deref() {
                                        None => pick_index(&options, 0)?,
                                        Some(cur) => offset_option(&options, Some(cur), 1, cycle)?,
                                    };
                                    s.select_option(target_opt).await
                                }
                                "select_previous" => {
                                    let cycle =
                                        data.get("cycle").and_then(Value::as_bool).unwrap_or(true);
                                    let current = s.current_option();
                                    let target_opt = match current.as_deref() {
                                        None => pick_index(&options, -1)?,
                                        Some(cur) => offset_option(&options, Some(cur), -1, cycle)?,
                                    };
                                    s.select_option(target_opt).await
                                }
                                _ => Err(ServiceError::Unknown(format!(
                                    "select.{name} is not supported"
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

    struct FakeSelect {
        id: EntityId,
        available: bool,
        options: Vec<String>,
        current: Mutex<Option<String>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeSelect {
        fn new(id: &str, available: bool, options: &[&str], current: Option<&str>) -> Arc<Self> {
            Arc::new(Self {
                id: EntityId::parse(id).unwrap(),
                available,
                options: options.iter().map(|s| (*s).to_owned()).collect(),
                current: Mutex::new(current.map(str::to_owned)),
                calls: Mutex::new(Vec::new()),
            })
        }
    }

    impl Select for FakeSelect {
        fn entity_id(&self) -> &EntityId {
            &self.id
        }
        fn available(&self) -> bool {
            self.available
        }
        fn options(&self) -> Vec<String> {
            self.options.clone()
        }
        fn current_option(&self) -> Option<String> {
            self.current.lock().clone()
        }
        fn select_option(&self, option: &str) -> BoxFuture<'_> {
            *self.current.lock() = Some(option.to_owned());
            self.calls.lock().push(option.to_owned());
            Box::pin(async { Ok(()) })
        }
    }

    fn map_of(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[tokio::test]
    async fn select_services_and_validation_match_ha() {
        let hass = Hass::new();
        let selects = Arc::new(Selects::default());
        selects.register_services(&hass, Arc::new(ha_registry::Registries::default()));

        let s = FakeSelect::new(
            "select.mop_intensity",
            true,
            &["off", "mild", "moderate", "intense"],
            None,
        );
        let offline = FakeSelect::new("select.offline", false, &["a", "b"], Some("a"));
        selects.add(s.clone());
        selects.add(offline.clone());

        // With no current option, select_next picks first; select_previous picks last.
        hass.call_service(
            "select",
            "select_next",
            map_of(json!({"entity_id": "select.mop_intensity"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        *s.current.lock() = None;
        hass.call_service(
            "select",
            "select_previous",
            map_of(json!({"entity_id": "select.mop_intensity"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        // Cycle wraps at end by default.
        hass.call_service(
            "select",
            "select_next",
            map_of(json!({"entity_id": "select.mop_intensity"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        // cycle: false clamps at bounds.
        hass.call_service(
            "select",
            "select_previous",
            map_of(json!({"entity_id": "select.mop_intensity", "cycle": false})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        hass.call_service(
            "select",
            "select_last",
            map_of(json!({"entity_id": "select.mop_intensity"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        hass.call_service(
            "select",
            "select_next",
            map_of(json!({"entity_id": "select.mop_intensity", "cycle": false})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        hass.call_service(
            "select",
            "select_first",
            map_of(json!({"entity_id": "select.mop_intensity"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        hass.call_service(
            "select",
            "select_option",
            map_of(json!({"entity_id": "select.mop_intensity", "option": "moderate"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            *s.calls.lock(),
            vec![
                "off", "intense", "off", "off", "intense", "intense", "off", "moderate",
            ]
        );

        // Invalid option raises HA's translated ServiceValidationError.
        let err = hass
            .call_service(
                "select",
                "select_option",
                map_of(json!({"entity_id": "select.mop_intensity", "option": "turbo"})),
                None,
                false,
                None,
            )
            .await
            .unwrap_err();
        match err {
            ServiceError::Validation(r) => {
                assert_eq!(r.domain.as_str(), "select");
                assert_eq!(r.key.as_str(), "not_valid_option");
                assert_eq!(
                    r.message,
                    "Option turbo is not valid for entity select.mop_intensity, valid options are: off, mild, moderate, intense"
                );
                assert_eq!(
                    json!(r.placeholders),
                    json!({
                        "entity_id": "select.mop_intensity",
                        "option": "turbo",
                        "options": "off, mild, moderate, intense"
                    })
                );
            }
            other => panic!("expected validation error, got {other:?}"),
        }

        // StrictBool rejects string "true" for cycle (matching HA's `bool` validator).
        let bad_cycle = hass
            .call_service(
                "select",
                "select_next",
                map_of(json!({"entity_id": "select.mop_intensity", "cycle": "true"})),
                None,
                false,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(bad_cycle, ServiceError::Invalid(_)));

        // Unavailable entity is skipped for `all`.
        hass.call_service(
            "select",
            "select_first",
            map_of(json!({"entity_id": "all"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        assert!(offline.calls.lock().is_empty());

        // Empty options never panics.
        let empty = FakeSelect::new("select.empty", true, &[], None);
        selects.add(empty);
        let empty_err = hass
            .call_service(
                "select",
                "select_first",
                map_of(json!({"entity_id": "select.empty"})),
                None,
                false,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(empty_err, ServiceError::Unknown(_)));

        // Removal stops further calls.
        selects.remove(&s.id);
        let before = s.calls.lock().len();
        hass.call_service(
            "select",
            "select_first",
            map_of(json!({"entity_id": "select.mop_intensity"})),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        assert_eq!(s.calls.lock().len(), before);
    }

    #[test]
    fn services_hold_no_cycle() {
        let hass = Hass::new();
        let selects = Arc::new(Selects::default());
        selects.register_services(&hass, Arc::new(ha_registry::Registries::default()));
        let weak = Arc::downgrade(&selects);
        drop((hass, selects));
        assert!(weak.upgrade().is_none());
    }
}

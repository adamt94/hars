//! The `select` entity component (`components/select/__init__.py`): its
//! services, run against the selects platforms provide. The platforms
//! write the states themselves.

use crate::light::BoxFuture;
use crate::switch::select;
use ha_core::{EntityId, Hass, Kind, Schema, ServiceError, SupportsResponse};
use parking_lot::RwLock;
use serde_json::{Map, Value, json};
use std::sync::Arc;

pub const DOMAIN: &str = "select";

/// A select a platform runs.
pub trait Select: Send + Sync {
    fn entity_id(&self) -> &EntityId;
    fn available(&self) -> bool;
    fn options(&self) -> Vec<String>;
    fn current_option(&self) -> Option<String>;
    fn select_option(&self, option: &str) -> BoxFuture<'_>;
}

#[derive(Default)]
pub struct Selects {
    items: RwLock<Vec<Arc<dyn Select>>>,
}

/// `_valid_option_or_raise`.
fn valid_option(s: &dyn Select, option: &str, options: &[String]) -> Result<(), ServiceError> {
    if options.iter().any(|o| o == option) {
        return Ok(());
    }
    let joined = options.join(", ");
    let mut p = Map::new();
    p.insert("entity_id".into(), json!(s.entity_id().as_str()));
    p.insert("option".into(), json!(option));
    p.insert("options".into(), json!(joined));
    Err(ServiceError::validation(
        DOMAIN,
        "not_valid_option",
        Some(p),
        format!(
            "Option {option} is not valid for entity {}, valid options are: {joined}",
            s.entity_id()
        ),
    ))
}

/// The option `select_first`/`last`/`next`/`previous` picks (`_async_select_index`
/// and `_async_offset_index`), or `None` when there are no options.
pub fn pick(
    service: &str,
    options: &[String],
    current: Option<&str>,
    cycle: bool,
) -> Option<usize> {
    let n = options.len() as i64;
    if n == 0 {
        return None;
    }
    let index = |idx: i64| Some(idx.rem_euclid(n) as usize);
    let offset = |by: i64| {
        let at = current
            .and_then(|c| options.iter().position(|o| o == c))
            .unwrap_or(0) as i64;
        let new = at + by;
        Some(if cycle {
            new.rem_euclid(n)
        } else {
            new.clamp(0, n - 1)
        } as usize)
    };
    match (service, current) {
        ("select_first", _) | ("select_next", None) => index(0),
        ("select_last", _) | ("select_previous", None) => index(-1),
        ("select_next", Some(_)) => offset(1),
        ("select_previous", Some(_)) => offset(-1),
        _ => None,
    }
}

impl Selects {
    pub fn add(&self, s: Arc<dyn Select>) {
        self.items.write().push(s);
    }

    pub fn remove(&self, entity_id: &EntityId) {
        self.items.write().retain(|s| s.entity_id() != entity_id);
    }

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
                        let chosen = select(
                            &items,
                            |s| s.entity_id(),
                            |s| s.available(),
                            &target,
                            &indirect,
                        );
                        let data = &call.data;
                        // `vol.Optional(ATTR_CYCLE, default=True)`.
                        let cycle = data.get("cycle").and_then(Value::as_bool).unwrap_or(true);
                        let calls = chosen.iter().map(|s| async move {
                            let options = s.options();
                            if name == "select_option" {
                                let option =
                                    data.get("option").and_then(Value::as_str).unwrap_or("");
                                valid_option(s.as_ref(), option, &options)?;
                                return s.select_option(option).await;
                            }
                            let current = s.current_option();
                            match pick(name, &options, current.as_deref(), cycle) {
                                Some(i) => s.select_option(&options[i]).await,
                                None => Ok(()),
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

    #[test]
    fn picks_as_ha() {
        let o: Vec<String> = ["cancel", "1h", "2h"].map(String::from).to_vec();
        assert_eq!(pick("select_next", &o, Some("2h"), true), Some(0));
        assert_eq!(pick("select_next", &o, Some("2h"), false), Some(2));
        assert_eq!(pick("select_previous", &o, Some("cancel"), true), Some(2));
        assert_eq!(pick("select_previous", &o, None, true), Some(2));
        assert_eq!(pick("select_next", &o, None, false), Some(0));
        // A current option that isn't listed counts as the first.
        assert_eq!(pick("select_next", &o, Some("9h"), true), Some(1));
        assert_eq!(pick("select_last", &o, None, true), Some(2));
    }
}

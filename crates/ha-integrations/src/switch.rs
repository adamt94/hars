//! The `switch` entity component (`components/switch`): `switch.turn_on`,
//! `switch.turn_off` and `switch.toggle`, handed to the targeted switches
//! of a platform.

use crate::light::BoxFuture;
use ha_core::{EntityId, EntityTarget, Hass, Schema, SupportsResponse};
use parking_lot::RwLock;
use std::sync::Arc;

pub const DOMAIN: &str = "switch";

/// A switch a platform runs.
pub trait Switch: Send + Sync {
    fn entity_id(&self) -> &EntityId;
    fn available(&self) -> bool;
    fn is_on(&self) -> bool;
    fn turn(&self, on: bool) -> BoxFuture<'_>;
}

/// The running switches, for the services.
#[derive(Default)]
pub struct Switches {
    items: RwLock<Vec<Arc<dyn Switch>>>,
}

/// The targeted, available entities of a list, as HA's entity services
/// pick them.
pub(crate) fn select<T: ?Sized>(
    items: &[Arc<T>],
    id: impl Fn(&T) -> &EntityId,
    available: impl Fn(&T) -> bool,
    target: &EntityTarget,
    indirect: &[String],
) -> Vec<Arc<T>> {
    items
        .iter()
        .filter(|i| match target {
            EntityTarget::All => true,
            EntityTarget::Ids(ids) => {
                ids.contains(id(i)) || indirect.iter().any(|x| x == id(i).as_str())
            }
        })
        .filter(|i| available(i))
        .cloned()
        .collect()
}

impl Switches {
    pub fn add(&self, s: Arc<dyn Switch>) {
        self.items.write().push(s);
    }

    pub fn remove(&self, entity_id: &EntityId) {
        self.items.write().retain(|s| s.entity_id() != entity_id);
    }

    /// Register `switch.turn_on`, `switch.turn_off` and `switch.toggle`.
    pub fn register_services(
        self: &Arc<Self>,
        hass: &Hass,
        registries: impl Into<ha_registry::Lookup>,
    ) {
        let registries = registries.into();
        for name in ["turn_on", "turn_off", "toggle"] {
            let switches = self.clone();
            let registries = registries.clone();
            hass.services().register_with_schema(
                DOMAIN,
                name,
                Some(Schema::entity_service()),
                SupportsResponse::None,
                move |call| {
                    let switches = switches.clone();
                    let registries = registries.clone();
                    async move {
                        let target = call.entity_ids()?;
                        let indirect = registries.get().target_entities(&call.data);
                        let items = switches.items.read().clone();
                        let chosen = select(
                            &items,
                            |s| s.entity_id(),
                            |s| s.available(),
                            &target,
                            &indirect,
                        );
                        let calls = chosen.iter().map(|s| {
                            let on = match name {
                                "turn_on" => true,
                                "turn_off" => false,
                                _ => !s.is_on(),
                            };
                            s.turn(on)
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

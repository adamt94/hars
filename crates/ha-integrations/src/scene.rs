//! The `scene` entity component (`components/scene`): `scene.turn_on`,
//! handed to the targeted scenes of a platform.

use crate::light::BoxFuture;
use crate::switch::select;
use ha_core::{EntityId, Hass, Kind, Schema, SupportsResponse};
use parking_lot::RwLock;
use serde_json::Value;
use std::sync::Arc;

pub const DOMAIN: &str = "scene";

/// A scene a platform runs. Activating records HA's activation time (the
/// scene's state) and writes it before asking the device.
pub trait Scene: Send + Sync {
    fn entity_id(&self) -> &EntityId;
    fn available(&self) -> bool;
    fn activate(&self, transition: Option<f64>) -> BoxFuture<'_>;
}

#[derive(Default)]
pub struct Scenes {
    items: RwLock<Vec<Arc<dyn Scene>>>,
}

impl Scenes {
    pub fn add(&self, s: Arc<dyn Scene>) {
        self.items.write().push(s);
    }

    /// The ids a call reaches the scenes under (one per scene served).
    #[cfg(test)]
    pub(crate) fn ids(&self) -> Vec<EntityId> {
        self.items
            .read()
            .iter()
            .map(|s| s.entity_id().clone())
            .collect()
    }

    pub fn remove(&self, entity_id: &EntityId) {
        self.items.write().retain(|s| s.entity_id() != entity_id);
    }

    /// Register `scene.turn_on` (`transition`: clamped to 0..6553 s).
    pub fn register_services(
        self: &Arc<Self>,
        hass: &Hass,
        registries: impl Into<ha_registry::Lookup>,
    ) {
        let registries = registries.into();
        let scenes = self.clone();
        hass.services().register_with_schema(
            DOMAIN,
            "turn_on",
            Some(Schema::entity_service().optional("transition", Kind::FloatClamp(0.0, 6553.0))),
            SupportsResponse::None,
            move |call| {
                let scenes = scenes.clone();
                let registries = registries.clone();
                async move {
                    let target = call.entity_ids()?;
                    let indirect = registries.get().target_entities(&call.data);
                    let transition = call.data.get("transition").and_then(Value::as_f64);
                    let items = scenes.items.read().clone();
                    let chosen = select(
                        &items,
                        |s| s.entity_id(),
                        |s| s.available(),
                        &target,
                        &indirect,
                    );
                    crate::light::join_all(chosen.iter().map(|s| s.activate(transition)))
                        .await
                        .into_iter()
                        .collect::<Result<Vec<()>, _>>()?;
                    Ok(None)
                }
            },
        );
    }
}

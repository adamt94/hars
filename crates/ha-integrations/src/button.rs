//! The `button` entity component (`components/button`): `button.press`.
//! A button's state is the time of its last press as HA saw it, written
//! before the platform is asked, so it moves even when the press fails.

use crate::light::BoxFuture;
use crate::switch::select;
use ha_core::{Context, EntityId, Hass, Schema, SupportsResponse};
use parking_lot::RwLock;
use std::sync::Arc;

pub const DOMAIN: &str = "button";

/// A button a platform runs.
pub trait Button: Send + Sync {
    fn entity_id(&self) -> &EntityId;
    fn available(&self) -> bool;
    /// Record `at` (an isoformat UTC time) as the last press, and write it
    /// with the call's context.
    fn pressed(&self, at: String, context: &Context);
    fn press(&self) -> BoxFuture<'_>;
}

/// `ButtonEntity.async_internal_added_to_hass`: the last stored press,
/// unless it was `unavailable` (`unknown` is kept: it means never). The
/// entity is registered for restore from now on.
pub fn restore(hass: &Hass, entity_id: &EntityId) -> Option<String> {
    restored(hass, entity_id).flatten()
}

/// What [`restore`] takes, as an entity added again under another id
/// needs it: `None` if nothing was kept under the id (the entity keeps
/// its last press), else the press kept (`None` for `unknown`, which HA
/// takes as it is, and so as no press).
pub fn restored(hass: &Hass, entity_id: &EntityId) -> Option<Option<String>> {
    let restore = hass.restore();
    let last = restore
        .last_state(entity_id.as_str())
        .and_then(|s| s["state"].as_str().map(str::to_owned))
        .filter(|s| s != "unavailable")
        .map(|s| Some(s).filter(|s| s != "unknown"));
    restore.register(entity_id.as_str(), None);
    last
}

#[derive(Default)]
pub struct Buttons {
    items: RwLock<Vec<Arc<dyn Button>>>,
}

impl Buttons {
    pub fn add(&self, b: Arc<dyn Button>) {
        self.items.write().push(b);
    }

    pub fn remove(&self, entity_id: &EntityId) {
        self.items.write().retain(|b| b.entity_id() != entity_id);
    }

    /// Register `button.press`.
    pub fn register_services(
        self: &Arc<Self>,
        hass: &Hass,
        registries: impl Into<ha_registry::Lookup>,
    ) {
        let registries = registries.into();
        let buttons = self.clone();
        // The clock, not `hass`: the handler lives in `hass`'s registry.
        let clock = hass.clock().clone();
        hass.services().register_with_schema(
            DOMAIN,
            "press",
            Some(Schema::entity_service()),
            SupportsResponse::None,
            move |call| {
                let buttons = buttons.clone();
                let registries = registries.clone();
                let clock = clock.clone();
                async move {
                    let target = call.entity_ids()?;
                    let indirect = registries.get().target_entities(&call.data);
                    let items = buttons.items.read().clone();
                    let chosen = select(
                        &items,
                        |b| b.entity_id(),
                        |b| b.available(),
                        &target,
                        &indirect,
                    );
                    let presses = chosen.iter().map(|b| {
                        b.pressed(ha_core::isoformat(clock.now()), &call.context);
                        b.press()
                    });
                    crate::light::join_all(presses)
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
    use ha_core::ServiceError;
    use parking_lot::Mutex;
    use serde_json::json;

    struct Fake {
        id: EntityId,
        hass: Hass,
        fail: bool,
        presses: Mutex<u32>,
    }

    impl Button for Fake {
        fn entity_id(&self) -> &EntityId {
            &self.id
        }
        fn available(&self) -> bool {
            true
        }
        fn pressed(&self, at: String, context: &Context) {
            self.hass.set_state_with_context(
                self.id.clone(),
                at,
                ha_core::Attributes::new(),
                Some(context.clone()),
            );
        }
        fn press(&self) -> BoxFuture<'_> {
            *self.presses.lock() += 1;
            let fail = self.fail;
            Box::pin(async move {
                match fail {
                    true => Err(ServiceError::Unknown(String::new())),
                    false => Ok(()),
                }
            })
        }
    }

    #[test]
    fn restores_the_last_press_unless_unavailable() {
        let dir = std::env::temp_dir().join(format!("hars-button-restore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let row = |id: &str, state: &str| {
            json!({"state": {"entity_id": id, "state": state, "attributes": {}},
                "extra_data": null, "last_seen": "2099-01-01T00:00:00+00:00"})
        };
        ha_core::storage::Store::new(&dir, "core.restore_state", 1, 1, false)
            .save(&json!([
                row("button.a", "2026-11-14T09:15:02.123456+00:00"),
                row("button.b", "unavailable"),
                row("button.c", "unknown"),
            ]))
            .unwrap();
        let hass = Hass::new();
        hass.set_restore(ha_core::restore::RestoreState::load(&dir));
        let id = |s| EntityId::parse(s).unwrap();
        assert_eq!(
            restore(&hass, &id("button.a")).as_deref(),
            Some("2026-11-14T09:15:02.123456+00:00")
        );
        assert_eq!(restore(&hass, &id("button.b")), None);
        assert_eq!(restore(&hass, &id("button.c")), None);
        assert_eq!(restore(&hass, &id("button.d")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn press_writes_the_time_first_even_when_it_fails() {
        let hass = Hass::new();
        let buttons = Arc::new(Buttons::default());
        buttons.register_services(&hass, Arc::new(ha_registry::Registries::default()));
        let id = EntityId::parse("button.towel_switch").unwrap();
        let b = Arc::new(Fake {
            id: id.clone(),
            hass: hass.clone(),
            fail: true,
            presses: Mutex::new(0),
        });
        buttons.add(b.clone());
        let data = json!({"entity_id": "button.towel_switch"});
        let context = Context::at(hass.clock().now()).with_user("u1");
        let r = hass
            .call_service(
                "button",
                "press",
                data.as_object().cloned().unwrap(),
                None,
                false,
                Some(context.clone()),
            )
            .await;
        assert_eq!(r, Err(ServiceError::Unknown(String::new())));
        assert_eq!(*b.presses.lock(), 1);
        let state = hass.states().get(&id).unwrap();
        assert_eq!(state.context, context);
        assert!(
            state.state.ends_with("+00:00") && state.state.len() == 32,
            "{}",
            state.state
        );
        // Gone from the component: nothing is pressed.
        buttons.remove(&id);
        hass.call_service(
            "button",
            "press",
            data.as_object().cloned().unwrap(),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        assert_eq!(*b.presses.lock(), 1);
    }

    /// The registered handler doesn't keep `hass` (or the buttons) alive.
    #[test]
    fn services_hold_no_cycle() {
        let hass = Hass::new();
        let buttons = Arc::new(Buttons::default());
        buttons.register_services(&hass, Arc::new(ha_registry::Registries::default()));
        let weak = Arc::downgrade(&buttons);
        drop((hass, buttons));
        assert!(weak.upgrade().is_none());
    }
}

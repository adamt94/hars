//! The shared `notify` entity component (`components/notify/__init__.py`):
//! `notify.send_message` across built-in integrations (e.g. `mobile_app`) and
//! out-of-process plugins.

use crate::light::BoxFuture;
use ha_core::{
    Context, EntityId, EntityTarget, Hass, Kind, Schema, ServiceError, SupportsResponse,
};
use parking_lot::RwLock;
use serde_json::Value;
use std::sync::Arc;

pub const DOMAIN: &str = "notify";

/// `NotifyEntityFeature` (`components/notify/__init__.py`).
pub mod feature {
    pub const TITLE: u32 = 1;
}

/// A notify entity a platform or plugin runs (`NotifyEntity`).
pub trait Notify: Send + Sync {
    /// Current entity ID (`None` if disabled or not yet registered).
    fn entity_id(&self) -> Option<EntityId>;
    fn available(&self) -> bool;
    fn supported_features(&self) -> u32 {
        0
    }

    /// Deliver `message` (and optional `title`, already stripped when
    /// `feature::TITLE` is not set) and record the notification timestamp on
    /// `Hass` on success (`NotifyEntity._async_send_message`).
    fn send_message<'a>(
        &'a self,
        message: &'a str,
        title: Option<&'a str>,
        context: Context,
    ) -> BoxFuture<'a>;
}

/// The running notify entities, backing `notify.send_message`.
#[derive(Default)]
pub struct Notifiers {
    items: RwLock<Vec<Arc<dyn Notify>>>,
}

impl Notifiers {
    pub fn add(&self, entity: Arc<dyn Notify>) {
        let mut items = self.items.write();
        if let Some(id) = entity.entity_id() {
            items.retain(|n| n.entity_id().as_ref() != Some(&id));
        }
        items.push(entity);
    }

    pub fn remove(&self, entity_id: &EntityId) {
        self.items
            .write()
            .retain(|n| n.entity_id().as_ref() != Some(entity_id));
    }

    /// Remove an exact registered adapter instance (safe even if its registry
    /// `entity_id` was renamed or disabled since registration).
    pub fn remove_arc(&self, target: &Arc<dyn Notify>) {
        self.items.write().retain(|n| !Arc::ptr_eq(n, target));
    }

    /// Register `notify.send_message` on `hass`.
    pub fn register_services(
        self: &Arc<Self>,
        hass: &Hass,
        registries: impl Into<ha_registry::Lookup>,
    ) {
        let registries = registries.into();
        let weak = Arc::downgrade(self);
        hass.services().register_with_schema(
            DOMAIN,
            "send_message",
            Some(
                Schema::entity_service()
                    .required("message", Kind::String)
                    .optional("title", Kind::String),
            ),
            SupportsResponse::None,
            move |call| {
                let weak = weak.clone();
                let registries = registries.clone();
                async move {
                    let Some(notifiers) = weak.upgrade() else {
                        return Ok(None);
                    };
                    let target = call.entity_ids()?;
                    let indirect = registries.get().target_entities(&call.data);
                    let list: Vec<Arc<dyn Notify>> = {
                        let items = notifiers.items.read();
                        items
                            .iter()
                            .filter(|n| {
                                n.available()
                                    && n.entity_id().is_some_and(|id| match &target {
                                        EntityTarget::All => true,
                                        EntityTarget::Ids(ids) => {
                                            ids.contains(&id)
                                                || indirect.iter().any(|x| x == id.as_str())
                                        }
                                    })
                            })
                            .cloned()
                            .collect()
                    };
                    let message = call
                        .data
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let raw_title = call.data.get("title").and_then(Value::as_str);
                    let ctx = call.context;

                    if let [only] = list.as_slice() {
                        dispatch_one(&**only, message, raw_title, ctx).await?;
                    } else {
                        let mut first_err: Option<ServiceError> = None;
                        for chunk in list.chunks(32) {
                            for res in crate::light::join_all(
                                chunk
                                    .iter()
                                    .map(|n| dispatch_one(&**n, message, raw_title, ctx.clone())),
                            )
                            .await
                            {
                                if let Err(e) = res
                                    && first_err.is_none()
                                {
                                    first_err = Some(e);
                                }
                            }
                        }
                        if let Some(e) = first_err {
                            return Err(e);
                        }
                    }
                    Ok(None)
                }
            },
        );
    }
}

fn dispatch_one<'a>(
    n: &'a dyn Notify,
    message: &'a str,
    raw_title: Option<&'a str>,
    ctx: Context,
) -> BoxFuture<'a> {
    let title = (n.supported_features() & feature::TITLE != 0)
        .then_some(raw_title)
        .flatten();
    n.send_message(message, title, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use serde_json::json;

    struct DummyNotify {
        id: EntityId,
        available: bool,
        features: u32,
        fail: bool,
        calls: Mutex<Vec<(String, Option<String>)>>,
    }

    impl Notify for DummyNotify {
        fn entity_id(&self) -> Option<EntityId> {
            Some(self.id.clone())
        }
        fn available(&self) -> bool {
            self.available
        }
        fn supported_features(&self) -> u32 {
            self.features
        }
        fn send_message<'a>(
            &'a self,
            message: &'a str,
            title: Option<&'a str>,
            _context: Context,
        ) -> BoxFuture<'a> {
            self.calls
                .lock()
                .push((message.to_owned(), title.map(str::to_owned)));
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    Err(ServiceError::Failed("relay down".into()))
                } else {
                    Ok(())
                }
            })
        }
    }

    #[tokio::test]
    async fn title_feature_filtering_multi_target_and_no_cycle() {
        let hass = Hass::new();
        let notifiers = Arc::new(Notifiers::default());
        notifiers.register_services(&hass, Arc::new(ha_registry::Registries::default()));

        let with_title = Arc::new(DummyNotify {
            id: EntityId::parse("notify.with_title").unwrap(),
            available: true,
            features: feature::TITLE,
            fail: true,
            calls: Mutex::new(Vec::new()),
        });
        let plain = Arc::new(DummyNotify {
            id: EntityId::parse("notify.plain").unwrap(),
            available: true,
            features: 0,
            fail: false,
            calls: Mutex::new(Vec::new()),
        });
        let offline = Arc::new(DummyNotify {
            id: EntityId::parse("notify.offline").unwrap(),
            available: false,
            features: feature::TITLE,
            fail: false,
            calls: Mutex::new(Vec::new()),
        });
        notifiers.add(with_title.clone());
        notifiers.add(plain.clone());
        notifiers.add(offline.clone());

        let err = hass
            .call_service(
                "notify",
                "send_message",
                json!({
                    "entity_id": ["notify.with_title", "notify.plain", "notify.offline"],
                    "message": "hi",
                    "title": "Heads up"
                })
                .as_object()
                .cloned()
                .unwrap(),
                None,
                false,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err, ServiceError::Failed("relay down".into()));
        assert_eq!(
            *with_title.calls.lock(),
            vec![("hi".into(), Some("Heads up".into()))]
        );
        assert_eq!(*plain.calls.lock(), vec![("hi".into(), None)]);
        assert!(offline.calls.lock().is_empty());

        let weak = Arc::downgrade(&notifiers);
        drop((hass, notifiers));
        assert!(weak.upgrade().is_none());
    }
}

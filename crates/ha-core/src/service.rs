//! Service registry: `hass.services.call(domain, service, data)`.
//!
//! Mirrors Home Assistant's `ServiceRegistry` (`homeassistant/core.py`):
//! services are keyed by `domain` then `service`, `target` is merged into the
//! service data, and a `call_service` event is fired before the handler runs.

use crate::{Context, EntityId, Schema};
use parking_lot::RwLock;
use serde_json::Value;
use smol_str::SmolStr;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Service data, as received (after `target` has been merged in).
pub type ServiceData = serde_json::Map<String, Value>;

/// Response data returned by services that support responses.
pub type ServiceResponse = serde_json::Map<String, Value>;

type HandlerFuture =
    Pin<Box<dyn Future<Output = Result<Option<ServiceResponse>, ServiceError>> + Send>>;
type Handler = dyn Fn(ServiceCall) -> HandlerFuture + Send + Sync;

/// Whether a service returns response data. Same values as HA's `SupportsResponse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SupportsResponse {
    /// The service never returns response data (the default).
    #[default]
    None,
    /// The service returns response data when the caller asks for it.
    Optional,
    /// The caller must ask for response data.
    Only,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ServiceError {
    #[error("service {domain}.{service} not found")]
    NotFound { domain: SmolStr, service: SmolStr },
    /// The service data failed validation (HA's `vol.Invalid`).
    #[error("invalid service data: {0}")]
    Invalid(String),
    #[error("service does not support responses; remove return_response from the request")]
    ResponseNotSupported,
    #[error("service requires responses; add return_response to the request")]
    ResponseRequired,
    /// The handler ran and failed (HA's `HomeAssistantError`).
    #[error("{0}")]
    Failed(String),
    /// A `HomeAssistantError` with translation fields (WS
    /// `home_assistant_error`, carrying them).
    #[error("{}", .0.message)]
    Translated(Box<Raised>),
    /// HA's `ServiceValidationError` (WS `service_validation_error`).
    #[error("{}", .0.message)]
    Validation(Box<Raised>),
    /// Any other exception escaping the handler (WS `unknown_error`).
    #[error("{0}")]
    Unknown(String),
}

/// A raised HA exception with its translation: the English message
/// (formatted from the translation, its trailing `.` dropped, as HA's
/// `HomeAssistantError.__str__` does) and the fields the frontend uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raised {
    pub message: String,
    pub domain: SmolStr,
    pub key: SmolStr,
    pub placeholders: Option<serde_json::Map<String, Value>>,
}

impl ServiceError {
    fn raised(
        domain: &str,
        key: &str,
        placeholders: Option<serde_json::Map<String, Value>>,
        message: String,
    ) -> Box<Raised> {
        Box::new(Raised {
            message,
            domain: domain.into(),
            key: key.into(),
            placeholders,
        })
    }

    /// A `ServiceValidationError`. An empty `key` is one raised with a
    /// message only (no translation).
    pub fn validation(
        domain: &str,
        key: &str,
        placeholders: Option<serde_json::Map<String, Value>>,
        message: String,
    ) -> Self {
        Self::Validation(Self::raised(domain, key, placeholders, message))
    }

    /// A translated `HomeAssistantError`.
    pub fn translated(
        domain: &str,
        key: &str,
        placeholders: Option<serde_json::Map<String, Value>>,
        message: String,
    ) -> Self {
        Self::Translated(Self::raised(domain, key, placeholders, message))
    }

    /// HA's `ServiceNotSupported`: an entity named directly lacks the
    /// service's required features.
    pub fn not_supported(domain: &str, service: &str, entity_id: &str) -> Self {
        let mut p = serde_json::Map::new();
        p.insert("domain".into(), domain.into());
        p.insert("service".into(), service.into());
        p.insert("entity_id".into(), entity_id.into());
        Self::validation(
            "homeassistant",
            "service_not_supported",
            Some(p),
            format!("Entity {entity_id} does not support action {domain}.{service}"),
        )
    }
}

/// One invocation of a service, handed to its handler.
#[derive(Debug, Clone)]
pub struct ServiceCall {
    pub domain: SmolStr,
    pub service: SmolStr,
    /// Shared with the `call_service` event, so it is never deep-copied.
    pub data: Arc<ServiceData>,
    pub return_response: bool,
    /// The caller's context. Handlers pass it to
    /// [`crate::Hass::set_state_with_context`].
    pub context: Context,
}

/// Entities a call targets, resolved from its `entity_id` field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityTarget {
    /// `entity_id: all`.
    All,
    /// Explicit ids. Empty when `entity_id` is absent or `none`.
    Ids(Vec<EntityId>),
}

impl ServiceCall {
    /// Resolve `entity_id` the way HA's `cv.comp_entity_ids` does: a single
    /// id, a comma-separated string, a list of ids, or `all` / `none`.
    /// Ids are lower-cased before validation. An explicit `null` is invalid.
    pub fn entity_ids(&self) -> Result<EntityTarget, ServiceError> {
        resolve_entity_ids(self.data.get("entity_id"))
    }
}

impl ServiceCall {
    /// [`ServiceCall::entity_ids`] for a raw `entity_id` value.
    pub fn entity_ids_of(value: Option<&Value>) -> Result<EntityTarget, ServiceError> {
        resolve_entity_ids(value)
    }
}

/// [`ServiceCall::entity_ids`] for a raw `entity_id` value.
pub(crate) fn resolve_entity_ids(value: Option<&Value>) -> Result<EntityTarget, ServiceError> {
    let mut ids = Vec::new();
    match walk_entity_ids(value, |id| ids.push(id))? {
        true => Ok(EntityTarget::All),
        false => Ok(EntityTarget::Ids(ids)),
    }
}

/// Check an `entity_id` value without collecting the ids.
pub(crate) fn validate_entity_ids(value: &Value) -> Result<(), ServiceError> {
    walk_entity_ids(Some(value), |_| {}).map(|_| ())
}

/// Visit each id in an `entity_id` value; `Ok(true)` means `all`.
fn walk_entity_ids(
    value: Option<&Value>,
    mut visit: impl FnMut(EntityId),
) -> Result<bool, ServiceError> {
    let mut parse = |s: &str| {
        let s = s.trim();
        let id = if s.bytes().any(|b| b.is_ascii_uppercase()) {
            EntityId::parse(&s.to_ascii_lowercase())
        } else {
            EntityId::parse(s)
        };
        id.map(&mut visit)
            .map_err(|e| ServiceError::Invalid(e.to_string()))
    };
    match value {
        None => Ok(false),
        Some(Value::Null) => Err(ServiceError::Invalid("Entity IDs cannot be None".into())),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("all") => Ok(true),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("none") => Ok(false),
        Some(Value::String(s)) => s.split(',').try_for_each(parse).map(|_| false),
        Some(Value::Array(items)) => items
            .iter()
            .try_for_each(|v| match v {
                Value::String(s) => parse(s),
                other => Err(ServiceError::Invalid(format!(
                    "entity_id must be a string, got {other}"
                ))),
            })
            .map(|_| false),
        Some(other) => Err(ServiceError::Invalid(format!(
            "entity_id must be a string or list, got {other}"
        ))),
    }
}

pub(crate) struct Service {
    handler: Box<Handler>,
    pub(crate) supports_response: SupportsResponse,
    pub(crate) schema: Option<Schema>,
}

impl Service {
    pub(crate) fn call(&self, call: ServiceCall) -> HandlerFuture {
        (self.handler)(call)
    }
}

#[derive(Default)]
pub struct ServiceRegistry {
    services: RwLock<HashMap<SmolStr, HashMap<SmolStr, Arc<Service>>>>,
}

impl ServiceRegistry {
    /// Register (or replace) `domain.service` with no data schema. Names are
    /// lower-cased, as in HA.
    pub fn register<F, Fut>(
        &self,
        domain: impl Into<SmolStr>,
        service: impl Into<SmolStr>,
        supports_response: SupportsResponse,
        handler: F,
    ) where
        F: Fn(ServiceCall) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Option<ServiceResponse>, ServiceError>> + Send + 'static,
    {
        self.register_with_schema(domain, service, None, supports_response, handler);
    }

    /// Register (or replace) `domain.service`. Calls are validated against
    /// `schema`, if any, before the `call_service` event fires; the handler
    /// receives the coerced data.
    pub fn register_with_schema<F, Fut>(
        &self,
        domain: impl Into<SmolStr>,
        service: impl Into<SmolStr>,
        schema: Option<Schema>,
        supports_response: SupportsResponse,
        handler: F,
    ) where
        F: Fn(ServiceCall) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Option<ServiceResponse>, ServiceError>> + Send + 'static,
    {
        let entry = Arc::new(Service {
            handler: Box::new(move |call| Box::pin(handler(call))),
            supports_response,
            schema,
        });
        self.services
            .write()
            .entry(lower(&domain.into()))
            .or_default()
            .insert(lower(&service.into()), entry);
    }

    /// Remove `domain.service` (case-insensitive). Returns whether it existed.
    pub fn remove(&self, domain: &str, service: &str) -> bool {
        let (domain, service) = (lower(domain), lower(service));
        let mut services = self.services.write();
        let Some(domain_services) = services.get_mut(&domain) else {
            return false;
        };
        let removed = domain_services.remove(&service).is_some();
        if domain_services.is_empty() {
            services.remove(&domain);
        }
        removed
    }

    pub fn has_service(&self, domain: &str, service: &str) -> bool {
        self.lookup(domain, service).is_some()
    }

    pub fn supports_response(&self, domain: &str, service: &str) -> Option<SupportsResponse> {
        self.lookup(domain, service).map(|s| s.supports_response)
    }

    /// Every registered service with its response support, grouped by
    /// domain, both sorted by name.
    pub fn services(&self) -> Vec<(SmolStr, Vec<(SmolStr, SupportsResponse)>)> {
        let services = self.services.read();
        let mut out: Vec<_> = services
            .iter()
            .map(|(domain, s)| {
                let mut names: Vec<_> = s
                    .iter()
                    .map(|(name, s)| (name.clone(), s.supports_response))
                    .collect();
                names.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                (domain.clone(), names)
            })
            .collect();
        out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Find a service. Names are stored lower-cased; like HA, retry
    /// lower-cased only on a miss so the common path does not allocate.
    pub(crate) fn lookup(&self, domain: &str, service: &str) -> Option<Arc<Service>> {
        let services = self.services.read();
        if let Some(s) = services.get(domain).and_then(|d| d.get(service)) {
            return Some(s.clone());
        }
        services
            .get(domain.to_ascii_lowercase().as_str())
            .and_then(|d| d.get(service.to_ascii_lowercase().as_str()))
            .cloned()
    }
}

/// Lower-case `s`, without allocating when it already is (the common case).
pub(crate) fn lower(s: &str) -> SmolStr {
    if s.bytes().any(|b| b.is_ascii_uppercase()) {
        SmolStr::new(s.to_ascii_lowercase())
    } else {
        SmolStr::new(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EventKind, Hass};
    use serde_json::json;

    fn data(v: Value) -> ServiceData {
        v.as_object().unwrap().clone()
    }

    fn call_with(entity_id: Value) -> ServiceCall {
        ServiceCall {
            domain: "light".into(),
            service: "turn_on".into(),
            data: Arc::new(data(json!({ "entity_id": entity_id }))),
            return_response: false,
            context: Context::new(),
        }
    }

    fn ids(list: &[&str]) -> EntityTarget {
        EntityTarget::Ids(list.iter().map(|s| EntityId::parse(s).unwrap()).collect())
    }

    #[tokio::test]
    async fn call_merges_target_and_fires_call_service_first() {
        let hass = Hass::new();
        let mut rx = hass.bus().subscribe();
        let seen = Arc::new(parking_lot::Mutex::new(None));
        let seen2 = seen.clone();
        hass.services()
            .register("light", "turn_on", SupportsResponse::None, move |call| {
                let seen = seen2.clone();
                async move {
                    *seen.lock() = Some(call);
                    Ok(None)
                }
            });

        let ctx = Context::new().with_user("user1");
        let out = hass
            .call_service(
                "light",
                "turn_on",
                data(json!({ "brightness_pct": 30 })),
                Some(data(json!({ "entity_id": "light.hallway" }))),
                false,
                Some(ctx.clone()),
            )
            .await;
        assert_eq!(out, Ok(None));

        let call = seen.lock().take().expect("handler ran");
        assert_eq!(
            *call.data,
            data(json!({ "brightness_pct": 30, "entity_id": "light.hallway" }))
        );
        let event = rx.try_recv().expect("call_service fired");
        let EventKind::CallService {
            domain,
            service,
            service_data,
        } = &event.kind
        else {
            panic!("expected call_service, got {:?}", event.kind);
        };
        assert_eq!((domain.as_str(), service.as_str()), ("light", "turn_on"));
        assert!(
            Arc::ptr_eq(service_data, &call.data),
            "event shares the data"
        );
        assert_eq!(call.context, ctx, "handler gets the caller's context");
        assert_eq!(event.context, ctx, "so does the event");
        assert_eq!(event.context.user_id.as_deref(), Some("user1"));
        assert_eq!(
            serde_json::to_value(&*event).unwrap()["data"],
            json!({
                "domain": "light",
                "service": "turn_on",
                "service_data": { "brightness_pct": 30, "entity_id": "light.hallway" }
            })
        );
    }

    #[tokio::test]
    async fn unknown_service_is_not_found_and_fires_nothing() {
        let hass = Hass::new();
        let mut rx = hass.bus().subscribe();
        let err = hass
            .call_service("Light", "Turn_On", ServiceData::new(), None, false, None)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ServiceError::NotFound {
                domain: "light".into(),
                service: "turn_on".into()
            }
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn lookup_falls_back_to_lower_case() {
        let hass = Hass::new();
        hass.services()
            .register("light", "turn_on", SupportsResponse::None, |_| async {
                Ok(None)
            });
        assert!(hass.services().has_service("LIGHT", "Turn_On"));
        let out = hass
            .call_service("LIGHT", "Turn_On", ServiceData::new(), None, false, None)
            .await;
        assert_eq!(out, Ok(None));
    }

    #[tokio::test]
    async fn response_rules_match_ha() {
        let hass = Hass::new();
        let response = || Ok(Some(data(json!({ "forecast": [] }))));
        hass.services()
            .register("light", "turn_on", SupportsResponse::None, |_| async {
                Ok(None)
            });
        hass.services().register(
            "weather",
            "get_forecasts",
            SupportsResponse::Only,
            move |_| async move { response() },
        );
        hass.services().register(
            "demo",
            "optional",
            SupportsResponse::Optional,
            move |_| async move { response() },
        );
        hass.services()
            .register("demo", "broken", SupportsResponse::Optional, |_| async {
                Ok(None)
            });

        let call = |d: &'static str, s: &'static str, r: bool| {
            let hass = hass.clone();
            async move {
                hass.call_service(d, s, ServiceData::new(), None, r, None)
                    .await
            }
        };
        assert_eq!(
            call("light", "turn_on", true).await,
            Err(ServiceError::ResponseNotSupported)
        );
        assert_eq!(
            call("weather", "get_forecasts", false).await,
            Err(ServiceError::ResponseRequired)
        );
        assert_eq!(call("weather", "get_forecasts", true).await, response());
        assert_eq!(call("demo", "optional", true).await, response());
        assert_eq!(call("demo", "optional", false).await, Ok(None));
        assert!(matches!(
            call("demo", "broken", true).await,
            Err(ServiceError::Failed(_))
        ));
    }

    #[test]
    fn registry_lists_sorted_and_removes() {
        let reg = ServiceRegistry::default();
        for (d, s) in [
            ("light", "turn_on"),
            ("Light", "Toggle"),
            ("hue", "activate_scene"),
        ] {
            reg.register(d, s, SupportsResponse::None, |_| async { Ok(None) });
        }
        let none = SupportsResponse::None;
        assert_eq!(
            reg.services(),
            vec![
                ("hue".into(), vec![("activate_scene".into(), none)]),
                (
                    "light".into(),
                    vec![("toggle".into(), none), ("turn_on".into(), none)]
                ),
            ],
            "names are stored lower-cased"
        );
        assert!(reg.has_service("light", "toggle"));
        assert!(reg.remove("LIGHT", "TOGGLE"), "remove is case-insensitive");
        assert!(!reg.has_service("light", "toggle"));
        assert!(reg.remove("hue", "activate_scene"));
        assert!(!reg.remove("hue", "activate_scene"));
        assert_eq!(reg.services().len(), 1, "empty domains are dropped");
        assert_eq!(
            reg.supports_response("Light", "turn_on"),
            Some(SupportsResponse::None)
        );
        assert_eq!(reg.supports_response("light", "nope"), None);
    }

    #[test]
    fn entity_ids_resolve_like_comp_entity_ids() {
        assert_eq!(
            call_with(json!("light.a")).entity_ids(),
            Ok(ids(&["light.a"]))
        );
        assert_eq!(
            call_with(json!("light.a, Light.B")).entity_ids(),
            Ok(ids(&["light.a", "light.b"]))
        );
        assert_eq!(
            call_with(json!(["light.a", "switch.b"])).entity_ids(),
            Ok(ids(&["light.a", "switch.b"]))
        );
        assert_eq!(call_with(json!("ALL")).entity_ids(), Ok(EntityTarget::All));
        assert_eq!(call_with(json!("none")).entity_ids(), Ok(ids(&[])));
        let missing = ServiceCall {
            data: Arc::new(ServiceData::new()),
            ..call_with(json!("none"))
        };
        assert_eq!(missing.entity_ids(), Ok(ids(&[])));
        for bad in [json!("light"), json!(["light.a", 3]), json!(5), Value::Null] {
            assert!(
                matches!(
                    call_with(bad.clone()).entity_ids(),
                    Err(ServiceError::Invalid(_))
                ),
                "{bad} should be invalid"
            );
        }
    }

    #[tokio::test]
    async fn schema_rejects_before_the_event_and_coerces_for_the_handler() {
        use crate::{Kind, Range, Schema};
        let hass = Hass::new();
        let mut rx = hass.bus().subscribe();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        hass.services().register_with_schema(
            "light",
            "turn_on",
            Some(
                Schema::entity_service()
                    .optional("brightness_pct", Kind::Float(Range::new(0.0, 100.0))),
            ),
            SupportsResponse::None,
            move |call| {
                let seen = seen2.clone();
                async move {
                    seen.lock().push(call.data);
                    Ok(None)
                }
            },
        );
        let call = |v: Value| {
            let hass = hass.clone();
            async move {
                hass.call_service("light", "turn_on", data(v), None, false, None)
                    .await
            }
        };

        let bad = call(json!({ "entity_id": "light.a", "brightness_pct": 150 })).await;
        assert!(matches!(bad, Err(ServiceError::Invalid(_))), "{bad:?}");
        assert!(rx.try_recv().is_err(), "no call_service for invalid data");
        assert!(seen.lock().is_empty(), "handler not run");

        // Coerced: the handler sees 50.0, the event keeps "50" as sent.
        call(json!({ "entity_id": "light.a", "brightness_pct": "50" }))
            .await
            .unwrap();
        let event = rx.try_recv().unwrap();
        let EventKind::CallService { service_data, .. } = &event.kind else {
            panic!("expected call_service");
        };
        assert_eq!(service_data["brightness_pct"], json!("50"));
        assert_eq!(seen.lock()[0]["brightness_pct"], json!(50.0));

        // Already valid: handler and event share one allocation.
        call(json!({ "entity_id": "light.a", "brightness_pct": 50 }))
            .await
            .unwrap();
        let event = rx.try_recv().unwrap();
        let EventKind::CallService { service_data, .. } = &event.kind else {
            panic!("expected call_service");
        };
        assert!(Arc::ptr_eq(service_data, &seen.lock()[1]));
    }

    /// Shadow mode: the call is validated and its event fired, but a
    /// handler that acts on a device doesn't run; `hars`'s own domains do.
    #[tokio::test]
    async fn shadow_mode_fires_the_event_and_holds_back_device_services() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hass = Hass::new();
        let ran = Arc::new(AtomicUsize::new(0));
        for domain in ["light", "automation"] {
            let ran = ran.clone();
            hass.services()
                .register(domain, "turn_on", SupportsResponse::None, move |_| {
                    let ran = ran.clone();
                    async move {
                        ran.fetch_add(1, Ordering::SeqCst);
                        Ok(None)
                    }
                });
        }
        let mut rx = hass.bus().subscribe();
        hass.set_shadow();
        let call = |domain: &'static str| {
            hass.call_service(domain, "turn_on", ServiceData::new(), None, false, None)
        };
        assert_eq!(call("light").await, Ok(None));
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        assert!(matches!(
            rx.try_recv().unwrap().kind,
            EventKind::CallService { .. }
        ));
        assert_eq!(call("automation").await, Ok(None));
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        // Unknown services are still errors, shadow or not.
        assert!(matches!(
            hass.call_service("light", "nope", ServiceData::new(), None, false, None)
                .await,
            Err(ServiceError::NotFound { .. })
        ));
    }
}

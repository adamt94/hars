//! Core runtime for homeassistant-rust.
//!
//! Mirrors the three pillars of Home Assistant's `core.py`:
//! - [`EventBus`]: typed pub/sub for everything that happens in the system.
//! - [`StateMachine`]: the current [`State`] of every entity.
//! - [`ServiceRegistry`]: the actions integrations expose (`light.turn_on`, …).
//! - [`Hass`]: the handle that ties them together and is shared by integrations.
//!
//! Design rule: the hot path (state writes + event fan-out) must not allocate
//! more than necessary and must never block on I/O.

mod entity_id;
mod event;
mod service;
mod state;

pub use entity_id::{EntityId, EntityIdError};
pub use event::{Event, EventBus, EventKind};
pub use service::{
    EntityTarget, ServiceCall, ServiceData, ServiceError, ServiceRegistry, ServiceResponse,
    SupportsResponse,
};
pub use state::{Attributes, State, StateMachine};

use smol_str::SmolStr;

use std::sync::Arc;

/// Shared handle to the running instance. Cheap to clone.
#[derive(Clone, Default)]
pub struct Hass {
    inner: Arc<HassInner>,
}

#[derive(Default)]
struct HassInner {
    bus: EventBus,
    states: StateMachine,
    services: ServiceRegistry,
}

impl Hass {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bus(&self) -> &EventBus {
        &self.inner.bus
    }

    pub fn states(&self) -> &StateMachine {
        &self.inner.states
    }

    pub fn services(&self) -> &ServiceRegistry {
        &self.inner.services
    }

    /// Call `domain.service` and wait for it to finish, like HA's
    /// `hass.services.async_call(..., blocking=True)`.
    ///
    /// `target` is merged into `data`, a `call_service` event is fired, then
    /// the handler runs. Response data is returned only when
    /// `return_response` is set.
    pub async fn call_service(
        &self,
        domain: &str,
        service: &str,
        mut data: ServiceData,
        target: Option<ServiceData>,
        return_response: bool,
    ) -> Result<Option<ServiceResponse>, ServiceError> {
        let handler =
            self.inner
                .services
                .lookup(domain, service)
                .ok_or_else(|| ServiceError::NotFound {
                    domain: lower(domain),
                    service: lower(service),
                })?;
        match (handler.supports_response, return_response) {
            (SupportsResponse::None, true) => return Err(ServiceError::ResponseNotSupported),
            (SupportsResponse::Only, false) => return Err(ServiceError::ResponseRequired),
            _ => {}
        }
        if let Some(target) = target {
            data.extend(target);
        }

        let call = ServiceCall {
            domain: lower(domain),
            service: lower(service),
            data: Arc::new(data),
            return_response,
        };
        self.inner.bus.fire(Event::now(EventKind::CallService {
            domain: call.domain.clone(),
            service: call.service.clone(),
            service_data: call.data.clone(),
        }));

        let response = handler.call(call).await?;
        if !return_response {
            return Ok(None);
        }
        response
            .map(Some)
            .ok_or_else(|| ServiceError::Failed("service did not return response data".into()))
    }

    /// Set an entity's state and fire `state_changed` if it actually changed.
    pub fn set_state(
        &self,
        entity_id: EntityId,
        state: impl Into<smol_str::SmolStr>,
        attributes: Attributes,
    ) -> Arc<State> {
        let (new, old) = self
            .inner
            .states
            .set(entity_id.clone(), state.into(), attributes);
        let changed = old.as_ref().is_none_or(|o| !Arc::ptr_eq(o, &new));
        if changed {
            self.inner
                .bus
                .fire(Event::state_changed(entity_id, old, Some(new.clone())));
        }
        new
    }
}

/// Lower-case `s`, without allocating when it already is (the common case).
fn lower(s: &str) -> SmolStr {
    if s.bytes().any(|b| b.is_ascii_uppercase()) {
        SmolStr::new(s.to_ascii_lowercase())
    } else {
        SmolStr::new(s)
    }
}

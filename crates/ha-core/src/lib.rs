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

pub mod casefold;
mod clock;
pub mod config;
mod context;
mod entity_id;
pub mod entityfilter;
mod event;
mod lifecycle;
pub mod restore;
mod schema;
mod service;
mod slug;
mod state;
pub mod storage;
pub mod yaml;

pub use clock::{Clock, SimulatedClock, isoformat, serialize_iso};
pub use context::{Context, ContextId};
pub use entity_id::{EntityId, EntityIdError, decimal_value, valid_entity_id};
pub use event::{Event, EventBus, EventKind, Origin};
pub use lifecycle::{CoreState, SetupGuard};
pub use schema::{Kind, Range, Schema};
pub use service::{
    EntityTarget, Raised, ServiceCall, ServiceData, ServiceError, ServiceRegistry, ServiceResponse,
    SupportsResponse,
};
pub use slug::slugify;
pub use state::{Attributes, Reported, State, StateChanges, StateMachine};

use service::lower;

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// Shared handle to the running instance. Cheap to clone.
#[derive(Clone, Default)]
pub struct Hass {
    inner: Arc<HassInner>,
}

#[derive(Default)]
struct HassInner {
    clock: Clock,
    bus: EventBus,
    states: StateMachine,
    services: ServiceRegistry,
    core_state: AtomicU8,
    config: parking_lot::RwLock<Arc<config::CoreConfig>>,
    restore: parking_lot::RwLock<Arc<restore::RestoreState>>,
    setups: lifecycle::Setups,
}

impl Hass {
    pub fn new() -> Self {
        Self::default()
    }

    /// A `Hass` whose timestamps and timers run on `clock`.
    pub fn with_clock(clock: Clock) -> Self {
        Self {
            inner: Arc::new(HassInner {
                clock,
                ..HassInner::default()
            }),
        }
    }

    pub fn clock(&self) -> &Clock {
        &self.inner.clock
    }

    /// Location, units and locale (`hass.config`).
    pub fn config(&self) -> Arc<config::CoreConfig> {
        self.inner.config.read().clone()
    }

    /// Last states for entities that restore themselves.
    pub fn restore(&self) -> Arc<restore::RestoreState> {
        self.inner.restore.read().clone()
    }

    pub fn set_restore(&self, restore: restore::RestoreState) {
        *self.inner.restore.write() = Arc::new(restore);
    }

    pub fn set_config(&self, config: config::CoreConfig) {
        *self.inner.config.write() = Arc::new(config);
    }

    pub fn core_state(&self) -> CoreState {
        CoreState::from_u8(self.inner.core_state.load(Ordering::Acquire))
    }

    pub(crate) fn set_core_state(&self, state: CoreState) {
        self.inner.core_state.store(state as u8, Ordering::Release);
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
    /// `return_response` is set. The event and the handler's
    /// [`ServiceCall`] carry `context`, or a new one if it is `None`.
    pub async fn call_service(
        &self,
        domain: &str,
        service: &str,
        mut data: ServiceData,
        target: Option<ServiceData>,
        return_response: bool,
        context: Option<Context>,
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

        // HA validates before firing `call_service`; the event keeps the data
        // as sent and the handler gets the coerced copy, if one was needed.
        let coerced = match &handler.schema {
            Some(schema) => schema.validate(&data)?,
            None => None,
        };
        let data = Arc::new(data);
        let now = self.inner.clock.now();
        let call = ServiceCall {
            domain: lower(domain),
            service: lower(service),
            data: coerced.map_or_else(|| data.clone(), Arc::new),
            return_response,
            context: context.unwrap_or_else(|| Context::at(now)),
        };
        self.inner.bus.fire(Event::new(
            EventKind::CallService {
                domain: call.domain.clone(),
                service: call.service.clone(),
                service_data: data,
            },
            now,
            call.context.clone(),
        ));

        let response = handler.call(call).await?;
        if !return_response {
            return Ok(None);
        }
        response
            .map(Some)
            .ok_or_else(|| ServiceError::Failed("service did not return response data".into()))
    }

    /// Set an entity's state under a new [`Context`] and fire `state_changed`
    /// if it actually changed.
    pub fn set_state(
        &self,
        entity_id: EntityId,
        state: impl Into<smol_str::SmolStr>,
        attributes: Attributes,
    ) -> Arc<State> {
        self.set_state_with_context(entity_id, state, attributes, None)
    }

    /// Like [`Hass::set_state`], recording `context` (or a new one) as the
    /// cause. Service handlers pass their call's context, so the change is
    /// attributed to the call. A write that changes nothing keeps the old
    /// state, context included, as in HA.
    pub fn set_state_with_context(
        &self,
        entity_id: EntityId,
        state: impl Into<smol_str::SmolStr>,
        attributes: Attributes,
        context: Option<Context>,
    ) -> Arc<State> {
        let inner = &*self.inner;
        inner.states.set(
            entity_id,
            state.into(),
            attributes,
            context,
            &inner.clock,
            &inner.bus,
            false,
        )
    }

    /// Write a state only if the entity has none, in one step (HA's
    /// start-time placeholders can't overwrite a state written meanwhile).
    pub fn set_state_if_absent(
        &self,
        entity_id: EntityId,
        state: impl Into<smol_str::SmolStr>,
        attributes: Attributes,
    ) {
        let inner = &*self.inner;
        inner.states.set(
            entity_id,
            state.into(),
            attributes,
            None,
            &inner.clock,
            &inner.bus,
            true,
        );
    }

    /// Remove an entity's state (HA's `async_remove`): `state_changed`
    /// fires with no new state. `false` if it had none.
    pub fn remove_state(&self, entity_id: &EntityId) -> bool {
        let inner = &*self.inner;
        inner.states.remove(entity_id, &inner.clock, &inner.bus)
    }

    /// Lossless, ordered `state_changed` delivery for internal consumers
    /// (automations, the recorder), plus a snapshot of every state taken
    /// atomically with the subscription. API clients use the lossy
    /// [`Hass::bus`] instead and resync when they lag.
    pub fn subscribe_state_changes(&self) -> StateChanges {
        self.inner.states.subscribe()
    }
}

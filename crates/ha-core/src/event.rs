use crate::{Context, EntityId, ServiceData, State};
use jiff::Timestamp;
use serde::Serialize;
use smol_str::SmolStr;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Bounded so a slow subscriber can't grow memory without limit;
/// laggards get `RecvError::Lagged` and must resync from the state machine.
const BUS_CAPACITY: usize = 4096;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event_type", content = "data", rename_all = "snake_case")]
pub enum EventKind {
    StateChanged {
        entity_id: EntityId,
        old_state: Option<Arc<State>>,
        new_state: Option<Arc<State>>,
    },
    /// Fired before a service handler runs (HA's `call_service`).
    CallService {
        domain: SmolStr,
        service: SmolStr,
        service_data: Arc<ServiceData>,
    },
    /// Lifecycle events, each with empty `data` as in HA.
    HomeassistantStart {},
    HomeassistantStarted {},
    HomeassistantStop {},
    HomeassistantFinalWrite {},
    HomeassistantClose {},
    /// Escape hatch for integration-defined events.
    Custom {
        name: SmolStr,
        data: serde_json::Value,
    },
}

/// HA's `EventOrigin`: where an event came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Origin {
    #[default]
    Local,
    /// Fired on behalf of a remote client (e.g. the companion app).
    Remote,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "LOCAL",
            Self::Remote => "REMOTE",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    #[serde(flatten)]
    pub kind: EventKind,
    pub origin: Origin,
    #[serde(serialize_with = "crate::clock::serialize_iso")]
    pub time_fired: Timestamp,
    pub context: Context,
}

impl Event {
    pub fn new(kind: EventKind, time_fired: Timestamp, context: Context) -> Self {
        Self {
            kind,
            origin: Origin::Local,
            time_fired,
            context,
        }
    }

    /// The same event, from `origin`.
    pub fn with_origin(mut self, origin: Origin) -> Self {
        self.origin = origin;
        self
    }
}

/// Fan-out event bus. Events are wrapped in `Arc` so each subscriber
/// receives a pointer, not a deep copy.
pub struct EventBus {
    tx: broadcast::Sender<Arc<Event>>,
}

impl Default for EventBus {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(BUS_CAPACITY);
        Self { tx }
    }
}

impl EventBus {
    pub fn fire(&self, event: Event) {
        self.fire_shared(Arc::new(event));
    }

    pub(crate) fn fire_shared(&self, event: Arc<Event>) {
        // No subscribers is not an error.
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.tx.subscribe()
    }
}

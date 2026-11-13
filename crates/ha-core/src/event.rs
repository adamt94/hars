use crate::{EntityId, ServiceData, State};
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
    /// Escape hatch for integration-defined events.
    Custom {
        name: SmolStr,
        data: serde_json::Value,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    #[serde(flatten)]
    pub kind: EventKind,
    pub time_fired: Timestamp,
}

impl Event {
    pub fn state_changed(
        entity_id: EntityId,
        old_state: Option<Arc<State>>,
        new_state: Option<Arc<State>>,
    ) -> Self {
        Self::now(EventKind::StateChanged {
            entity_id,
            old_state,
            new_state,
        })
    }

    pub fn now(kind: EventKind) -> Self {
        Self {
            kind,
            time_fired: Timestamp::now(),
        }
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
        // No subscribers is not an error.
        let _ = self.tx.send(Arc::new(event));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.tx.subscribe()
    }
}

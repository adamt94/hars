use crate::{Context, EntityId, ServiceData, State};
use jiff::Timestamp;
use parking_lot::Mutex;
use serde::Serialize;
use smol_str::SmolStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{broadcast, mpsc};

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

/// A lossless subscriber: its filter, its queue, and how many events were
/// ever queued to it.
type Lossless = (
    EventFilter,
    mpsc::UnboundedSender<Arc<Event>>,
    Arc<AtomicU64>,
);

/// Picks the events a lossless subscriber wants. It runs on the firing
/// thread with the bus locked (and, for a state change, the states map),
/// so it must look only at the event it is given: reading `Hass` or firing
/// from it deadlocks.
pub type EventFilter = fn(&Event) -> bool;

/// Fan-out event bus. Events are wrapped in `Arc` so each subscriber
/// receives a pointer, not a deep copy.
pub struct EventBus {
    tx: broadcast::Sender<Arc<Event>>,
    /// Lossless subscribers ([`EventBus::subscribe_lossless`]). Held while
    /// firing, broadcast included, so they and the bus see one order.
    lossless: Mutex<Vec<Lossless>>,
}

impl Default for EventBus {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(BUS_CAPACITY);
        Self {
            tx,
            lossless: Mutex::new(Vec::new()),
        }
    }
}

impl EventBus {
    pub fn fire(&self, event: Event) {
        self.fire_shared(Arc::new(event));
    }

    pub(crate) fn fire_shared(&self, event: Arc<Event>) {
        let mut lossless = self.lossless.lock();
        if !lossless.is_empty() {
            // A dropped feed goes before its filter runs, matching or not.
            lossless.retain(|(wants, tx, queued)| {
                !tx.is_closed()
                    && (!wants(&event) || {
                        let sent = tx.send(event.clone()).is_ok();
                        if sent {
                            queued.fetch_add(1, Ordering::Release);
                        }
                        sent
                    })
            });
        }
        // No subscribers is not an error.
        let _ = self.tx.send(event);
    }

    /// Lossy: a subscriber that falls `BUS_CAPACITY` behind gets
    /// `RecvError::Lagged`. API clients use this and resync.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.tx.subscribe()
    }

    /// Every later event `wants` picks, never dropped and in the order the
    /// bus fires them (the recorder). The queue grows while the consumer is
    /// behind, so this is only for internal consumers that keep up.
    /// Dropping the feed unsubscribes.
    pub fn subscribe_lossless(&self, wants: EventFilter) -> EventFeed {
        let (tx, rx) = mpsc::unbounded_channel();
        let queued = Arc::new(AtomicU64::new(0));
        let mut lossless = self.lossless.lock();
        lossless.push((wants, tx, queued.clone()));
        EventFeed { rx, queued }
    }
}

/// A lossless event feed, from [`EventBus::subscribe_lossless`].
pub struct EventFeed {
    rx: mpsc::UnboundedReceiver<Arc<Event>>,
    queued: Arc<AtomicU64>,
}

impl EventFeed {
    /// The next event. `None` once the bus is gone.
    pub async fn recv(&mut self) -> Option<Arc<Event>> {
        self.rx.recv().await
    }

    /// The next event if one is queued.
    pub fn try_recv(&mut self) -> Option<Arc<Event>> {
        self.rx.try_recv().ok()
    }

    /// Events queued and not yet received.
    pub fn backlog(&self) -> usize {
        self.rx.len()
    }

    /// How many events the bus has ever queued to this feed. A consumer
    /// that counts what it has handled can tell when it has caught up with
    /// everything fired before a moment: read this, then wait for its own
    /// count to reach it.
    pub fn queued(&self) -> Arc<AtomicU64> {
        self.queued.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Context, Hass};

    fn custom(name: &str, i: u64) -> Event {
        Event::new(
            EventKind::Custom {
                name: name.into(),
                data: serde_json::json!(i),
            },
            Timestamp::UNIX_EPOCH,
            Context::at(Timestamp::UNIX_EPOCH),
        )
    }

    fn number(e: &Event) -> u64 {
        match &e.kind {
            EventKind::Custom { data, .. } => data.as_u64().unwrap(),
            _ => u64::MAX,
        }
    }

    fn wanted(e: &Event) -> bool {
        matches!(&e.kind, EventKind::Custom { name, .. } if name == "kept")
            || matches!(e.kind, EventKind::StateChanged { .. })
    }

    /// A 10,000-event burst arrives complete and in order while the
    /// broadcast bus lags, and only the picked events.
    #[test]
    fn bursts_arrive_complete_while_the_bus_lags() {
        let hass = Hass::new();
        let mut lossy = hass.bus().subscribe();
        let mut feed = hass.bus().subscribe_lossless(wanted);
        let id = crate::EntityId::parse("sensor.x").unwrap();
        for i in 0..10_000u64 {
            match i % 3 {
                0 => hass.bus().fire(custom("kept", i)),
                1 => hass.bus().fire(custom("skipped", i)),
                _ => {
                    hass.set_state(id.clone(), i.to_string(), Default::default());
                }
            }
        }
        assert!(matches!(
            lossy.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
        let mut got = Vec::new();
        while let Some(e) = feed.try_recv() {
            got.push(match &e.kind {
                EventKind::StateChanged { new_state, .. } => {
                    new_state.as_ref().unwrap().state.parse().unwrap()
                }
                _ => number(&e),
            });
        }
        let want: Vec<u64> = (0..10_000).filter(|i| i % 3 != 1).collect();
        assert_eq!(got, want);
    }

    /// The feed counts what was queued to it: filtered-out events don't count.
    #[test]
    fn the_feed_counts_what_was_queued() {
        let bus = EventBus::default();
        let feed = bus.subscribe_lossless(
            |e| matches!(&e.kind, EventKind::Custom { name, .. } if name == "kept"),
        );
        let queued = feed.queued();
        bus.fire(custom("kept", 1));
        bus.fire(custom("other", 2));
        bus.fire(custom("kept", 3));
        assert_eq!(queued.load(Ordering::Acquire), 2);
        assert_eq!(feed.backlog(), 2);
    }

    /// Concurrent firers: the feed's order is the bus's order.
    #[test]
    fn the_feed_and_the_bus_agree_on_order() {
        let bus = EventBus::default();
        let mut lossy = bus.subscribe();
        let mut feed = bus.subscribe_lossless(|_| true);
        std::thread::scope(|s| {
            for t in 0..4u64 {
                let bus = &bus;
                s.spawn(move || {
                    for i in 0..500 {
                        bus.fire(custom("kept", t * 1000 + i));
                    }
                });
            }
        });
        let mut from_bus = Vec::new();
        while let Ok(e) = lossy.try_recv() {
            from_bus.push(number(&e));
        }
        let mut from_feed = Vec::new();
        while let Some(e) = feed.try_recv() {
            from_feed.push(number(&e));
        }
        assert_eq!(from_bus.len(), 2000);
        assert_eq!(from_feed, from_bus);
    }

    #[test]
    fn dropped_feeds_unsubscribe() {
        let bus = EventBus::default();
        let _kept = bus.subscribe_lossless(|_| false);
        for _ in 0..10 {
            drop(bus.subscribe_lossless(|_| false));
        }
        assert_eq!(bus.lossless.lock().len(), 11);
        // An event none of them wants still prunes the dropped ones.
        bus.fire(custom("unwanted", 0));
        assert_eq!(bus.lossless.lock().len(), 1);
    }
}

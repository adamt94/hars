use crate::{Clock, Context, EntityId, Event, EventBus, EventKind};
use jiff::Timestamp;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use smol_str::SmolStr;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::sync::mpsc;

pub type Attributes = serde_json::Map<String, serde_json::Value>;

/// Immutable snapshot of one entity (but for `last_reported`). Shared via
/// `Arc` between the state machine, events, and API responses so it is
/// never deep-copied. Serializes as HA's `State.as_dict`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct State {
    pub entity_id: EntityId,
    pub state: SmolStr,
    pub attributes: Attributes,
    #[serde(serialize_with = "crate::clock::serialize_iso")]
    pub last_changed: Timestamp,
    pub last_reported: Reported,
    #[serde(serialize_with = "crate::clock::serialize_iso")]
    pub last_updated: Timestamp,
    pub context: Context,
}

/// `last_reported`: when the state was last written, changed or not. A
/// write that changes nothing moves it on the existing state, as HA
/// mutates its `State` in place, so it is atomic (microseconds since the
/// epoch) and needs no copy of the attributes.
///
/// HA caches a state's dict and JSON the first time they're made, and a
/// report doesn't clear that cache: the `last_reported` its JSON shows is
/// the one at the state's first serialization. `shown` keeps that, and
/// [`Reported::get`] the live time.
#[derive(Debug)]
pub struct Reported {
    live: AtomicI64,
    /// `i64::MIN` until first serialized.
    shown: AtomicI64,
}

const UNSET: i64 = i64::MIN;

impl Reported {
    pub fn new(t: Timestamp) -> Self {
        Self {
            live: AtomicI64::new(t.as_microsecond()),
            shown: AtomicI64::new(UNSET),
        }
    }

    /// When the state was last written.
    pub fn get(&self) -> Timestamp {
        micros(self.live.load(Ordering::Relaxed))
    }

    fn set(&self, t: Timestamp) {
        self.live.store(t.as_microsecond(), Ordering::Relaxed);
    }

    /// What the state's JSON shows: fixed by the first serialization.
    fn shown(&self) -> Timestamp {
        let live = self.live.load(Ordering::Relaxed);
        match self
            .shown
            .compare_exchange(UNSET, live, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => micros(live),
            Err(shown) => micros(shown),
        }
    }
}

fn micros(us: i64) -> Timestamp {
    Timestamp::from_microsecond(us).unwrap_or_default()
}

impl Clone for Reported {
    fn clone(&self) -> Self {
        Self {
            live: AtomicI64::new(self.live.load(Ordering::Relaxed)),
            shown: AtomicI64::new(self.shown.load(Ordering::Relaxed)),
        }
    }
}

impl PartialEq for Reported {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

impl Serialize for Reported {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        crate::clock::serialize_iso(&self.shown(), s)
    }
}

#[derive(Default)]
pub struct StateMachine {
    states: RwLock<HashMap<EntityId, Arc<State>>>,
    /// Lossless `state_changed` consumers. Only touched while `states` is
    /// locked, so every consumer sees changes in the order they were applied.
    listeners: Mutex<Vec<mpsc::UnboundedSender<Arc<Event>>>>,
}

/// A lossless, ordered feed of `state_changed` events, from
/// [`crate::Hass::subscribe_state_changes`].
///
/// Unlike the [`EventBus`], nothing is ever dropped: the queue grows while the
/// consumer is behind, so it is only for internal consumers that keep up
/// (automations, the recorder). Dropping it unsubscribes.
pub struct StateChanges {
    /// Every state at the moment of subscribing. Each later change is in the
    /// feed, and none before it is.
    pub snapshot: Vec<Arc<State>>,
    rx: mpsc::UnboundedReceiver<Arc<Event>>,
}

impl StateChanges {
    /// The next `state_changed` event. `None` once the `Hass` is gone.
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
}

impl StateMachine {
    pub fn get(&self, entity_id: &EntityId) -> Option<Arc<State>> {
        self.states.read().get(entity_id).cloned()
    }

    pub fn all(&self) -> Vec<Arc<State>> {
        self.states.read().values().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.states.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Subscribe to every later change, with a snapshot taken atomically
    /// with the subscription.
    pub(crate) fn subscribe(&self) -> StateChanges {
        let (tx, rx) = mpsc::unbounded_channel();
        // Writers notify while holding the write lock, so holding the read
        // lock here means no change can fall between snapshot and feed.
        let states = self.states.read();
        let mut listeners = self.listeners.lock();
        // Writes also prune, but an idle system must not pile up dead feeds.
        listeners.retain(|tx| !tx.is_closed());
        listeners.push(tx);
        drop(listeners);
        StateChanges {
            snapshot: states.values().cloned().collect(),
            rx,
        }
    }

    /// Write a state and, if it changed, fire `state_changed` to the
    /// lossless listeners and the bus before the lock is released, so every
    /// consumer sees one order. The timestamp is read under the lock for the
    /// same reason. An unchanged write returns the existing `Arc`, moves
    /// only its `last_reported`, and fires no `state_changed`.
    pub(crate) fn set(
        &self,
        entity_id: EntityId,
        state: SmolStr,
        attributes: Attributes,
        context: Option<Context>,
        clock: &Clock,
        bus: &EventBus,
    ) -> Arc<State> {
        let mut states = self.states.write();
        let old = states.get(&entity_id).cloned();

        if let Some(o) = &old
            && o.state == state
            && o.attributes == attributes
        {
            o.last_reported.set(clock.now());
            return o.clone();
        }

        let now = clock.now();
        let last_changed = match &old {
            Some(o) if o.state == state => o.last_changed,
            _ => now,
        };
        let context = context.unwrap_or_else(|| Context::at(now));
        let new = Arc::new(State {
            entity_id: entity_id.clone(),
            state,
            attributes,
            last_changed,
            last_reported: Reported::new(now),
            last_updated: now,
            context: context.clone(),
        });
        states.insert(entity_id.clone(), new.clone());

        let event = Arc::new(Event::new(
            EventKind::StateChanged {
                entity_id,
                old_state: old,
                new_state: Some(new.clone()),
            },
            now,
            context,
        ));
        self.listeners
            .lock()
            .retain(|tx| tx.send(event.clone()).is_ok());
        bus.fire_shared(event);
        new
    }
}

#[cfg(test)]
mod tests {
    use crate::{EventKind, Hass};

    use super::*;

    #[tokio::test]
    async fn set_state_fires_only_on_change() {
        let hass = Hass::new();
        let mut rx = hass.bus().subscribe();
        let id = EntityId::parse("light.kitchen").unwrap();

        hass.set_state(id.clone(), "on", Attributes::new());
        hass.set_state(id.clone(), "on", Attributes::new()); // no-op
        hass.set_state(id.clone(), "off", Attributes::new());

        let first = rx.recv().await.unwrap();
        let second = rx.recv().await.unwrap();
        assert!(rx.try_recv().is_err(), "unchanged write must not fire");

        let EventKind::StateChanged {
            old_state,
            new_state,
            ..
        } = &second.kind
        else {
            panic!("expected state_changed");
        };
        assert_eq!(old_state.as_ref().unwrap().state, "on");
        assert_eq!(new_state.as_ref().unwrap().state, "off");
        assert!(matches!(
            first.kind,
            EventKind::StateChanged {
                old_state: None,
                ..
            }
        ));
        assert_eq!(hass.states().get(&id).unwrap().state, "off");
    }

    #[tokio::test(start_paused = true)]
    async fn timestamps_come_from_the_injected_clock() {
        use crate::SimulatedClock;
        use std::time::Duration;

        let t0: Timestamp = "2026-10-25T00:30:00Z".parse().unwrap();
        let hass = Hass::with_clock(SimulatedClock::new(t0).clock());
        let mut rx = hass.bus().subscribe();
        let id = EntityId::parse("sensor.temp").unwrap();

        let first = hass.set_state(id.clone(), "20", Attributes::new());
        assert_eq!((first.last_changed, first.last_updated), (t0, t0));

        tokio::time::advance(Duration::from_secs(60)).await;
        let mut attrs = Attributes::new();
        attrs.insert("unit_of_measurement".into(), "°C".into());
        let second = hass.set_state(id, "20", attrs);
        let t1 = t0 + jiff::SignedDuration::from_secs(60);
        assert_eq!(second.last_updated, t1);
        assert_eq!(
            second.last_changed, t0,
            "attribute-only change keeps last_changed"
        );

        assert_eq!(rx.recv().await.unwrap().time_fired, t0);
        assert_eq!(rx.recv().await.unwrap().time_fired, t1);
    }

    /// Another write lands between the call and the lock (here: while the
    /// caller's state value is converted). The later write must not get an
    /// older `last_updated` than the one it follows.
    #[tokio::test(start_paused = true)]
    async fn timestamp_is_read_under_the_lock() {
        use crate::SimulatedClock;

        struct Interleave<'a>(&'a Hass, &'a SimulatedClock, &'a EntityId);
        impl From<Interleave<'_>> for SmolStr {
            fn from(Interleave(hass, sim, id): Interleave<'_>) -> Self {
                sim.jump_to("2026-01-01T00:00:10Z".parse().unwrap());
                hass.set_state(id.clone(), "on", Attributes::new());
                "on".into()
            }
        }

        let sim = SimulatedClock::new("2026-01-01T00:00:00Z".parse().unwrap());
        let hass = Hass::with_clock(sim.clock());
        let id = EntityId::parse("light.a").unwrap();
        let mut attrs = Attributes::new();
        attrs.insert("brightness".into(), 10.into());
        let state = hass.set_state(id.clone(), Interleave(&hass, &sim, &id), attrs);
        assert!(
            state.last_updated >= state.last_changed,
            "last_updated {} < last_changed {}",
            state.last_updated,
            state.last_changed
        );
    }

    #[tokio::test]
    async fn state_and_event_carry_the_context() {
        let hass = Hass::new();
        let mut rx = hass.bus().subscribe();
        let id = EntityId::parse("light.kitchen").unwrap();
        let ctx = Context::new().with_user("user1");

        let on =
            hass.set_state_with_context(id.clone(), "on", Attributes::new(), Some(ctx.clone()));
        assert_eq!(on.context, ctx);
        assert_eq!(rx.recv().await.unwrap().context, ctx);

        // An unchanged write keeps the old state and its context.
        let same = hass.set_state(id.clone(), "on", Attributes::new());
        assert!(Arc::ptr_eq(&same, &on));
        assert_eq!(same.context, ctx);

        let off = hass.set_state(id, "off", Attributes::new());
        assert_ne!(off.context, ctx, "no context given: a fresh one");
        assert_eq!(off.context.user_id, None);
        let json = serde_json::to_value(&*off).unwrap();
        assert_eq!(json["context"]["id"], off.context.id.to_string());
        assert_eq!(json["context"]["parent_id"], serde_json::Value::Null);
    }

    fn change(event: &Event) -> (&EntityId, Option<&Arc<State>>, &Arc<State>) {
        match &event.kind {
            EventKind::StateChanged {
                entity_id,
                old_state,
                new_state: Some(new),
            } => (entity_id, old_state.as_ref(), new),
            other => panic!("expected state_changed, got {other:?}"),
        }
    }

    #[test]
    fn listeners_miss_nothing_when_the_bus_overflows() {
        const N: usize = 10_000; // > the 4,096-event bus capacity
        let hass = Hass::new();
        let mut bus = hass.bus().subscribe();
        let mut feed = hass.subscribe_state_changes();
        assert!(feed.snapshot.is_empty());
        let id = EntityId::parse("sensor.counter").unwrap();
        for i in 0..N {
            hass.set_state(id.clone(), i.to_string(), Attributes::new());
        }
        assert_eq!(feed.backlog(), N);
        for i in 0..N {
            let event = feed.try_recv().expect("lossless");
            assert_eq!(change(&event).2.state, i.to_string());
        }
        assert!(feed.try_recv().is_none());
        assert!(matches!(
            bus.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
        ));
    }

    /// Concurrent writers to one entity: every consumer must see a single
    /// chain where each event's `old_state` is the previous `new_state`.
    #[test]
    fn concurrent_writes_arrive_in_one_order() {
        const THREADS: usize = 4;
        const WRITES: usize = 1_000; // 4,000 fits in the bus too
        let hass = Hass::new();
        let mut bus = hass.bus().subscribe();
        let mut feed = hass.subscribe_state_changes();
        let id = EntityId::parse("sensor.contended").unwrap();
        std::thread::scope(|s| {
            for t in 0..THREADS {
                let (hass, id) = (&hass, &id);
                s.spawn(move || {
                    for i in 0..WRITES {
                        hass.set_state(id.clone(), format!("{t}-{i}"), Attributes::new());
                    }
                });
            }
        });

        let mut feed_events = Vec::new();
        while let Some(e) = feed.try_recv() {
            feed_events.push(e);
        }
        let mut bus_events = Vec::new();
        while let Ok(e) = bus.try_recv() {
            bus_events.push(e);
        }
        for events in [&feed_events, &bus_events] {
            assert_eq!(events.len(), THREADS * WRITES);
            let mut prev: Option<&Arc<State>> = None;
            for event in events {
                let (_, old, new) = change(event);
                match (prev, old) {
                    (None, None) => {}
                    (Some(p), Some(o)) => assert!(Arc::ptr_eq(p, o), "chain broken"),
                    _ => panic!("chain broken at the first event"),
                }
                assert!(new.last_updated >= old.map_or(new.last_updated, |o| o.last_updated));
                prev = Some(new);
            }
            assert!(Arc::ptr_eq(prev.unwrap(), &hass.states().get(&id).unwrap()));
        }
        for (a, b) in feed_events.iter().zip(&bus_events) {
            assert!(
                Arc::ptr_eq(a, b),
                "feed and bus share one order and one Arc"
            );
        }
    }

    /// Subscribing while another thread writes: the snapshot plus the feed
    /// must account for every change exactly once.
    #[test]
    fn snapshot_and_feed_have_no_gap_or_overlap() {
        let hass = Hass::new();
        let id = EntityId::parse("sensor.busy").unwrap();
        hass.set_state(id.clone(), "start", Attributes::new());
        let subscribed = std::sync::atomic::AtomicBool::new(false);
        let feed = std::thread::scope(|s| {
            // Write until the subscription exists, then 1,000 more: so it is
            // always taken while writes are in flight.
            let writer = s.spawn(|| {
                let mut i = 0;
                let mut after = 0;
                while after < 1_000 {
                    hass.set_state(id.clone(), i.to_string(), Attributes::new());
                    i += 1;
                    if subscribed.load(std::sync::atomic::Ordering::Acquire) {
                        after += 1;
                    }
                }
                i
            });
            while hass.states().get(&id).unwrap().state == "start" {
                std::hint::spin_loop();
            }
            let feed = hass.subscribe_state_changes();
            subscribed.store(true, std::sync::atomic::Ordering::Release);
            (feed, writer.join().unwrap())
        });
        let (mut feed, writes) = feed;
        // The write that raced the flag may predate the subscription.
        assert!(feed.backlog() >= 999, "subscribed mid-write");
        let mut prev = feed.snapshot[0].clone();
        while let Some(event) = feed.try_recv() {
            let (_, old, new) = change(&event);
            assert!(Arc::ptr_eq(old.unwrap(), &prev), "gap or overlap");
            prev = new.clone();
        }
        assert_eq!(prev.state, (writes - 1).to_string());
    }

    #[test]
    fn dropped_feed_unsubscribes() {
        let hass = Hass::new();
        let feed = hass.subscribe_state_changes();
        let _kept = hass.subscribe_state_changes();
        drop(feed);
        let id = EntityId::parse("light.a").unwrap();
        hass.set_state(id, "on", Attributes::new());
        assert_eq!(hass.states().listeners.lock().len(), 1);
    }

    #[test]
    fn dropped_feeds_do_not_pile_up_while_idle() {
        let hass = Hass::new();
        for _ in 0..100 {
            drop(hass.subscribe_state_changes());
        }
        assert_eq!(hass.states().listeners.lock().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn serializes_and_reports_like_has_state() {
        let sim = crate::SimulatedClock::new("2026-11-14T13:29:29.778252Z".parse().unwrap());
        let hass = Hass::with_clock(sim.clock());
        let id = EntityId::parse("sensor.t").unwrap();
        let mut attrs = Attributes::new();
        attrs.insert("unit_of_measurement".into(), "°C".into());
        let first = hass.set_state(id.clone(), "20", attrs.clone());
        let json = serde_json::to_value(&*first).unwrap();
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "attributes",
                "context",
                "entity_id",
                "last_changed",
                "last_reported",
                "last_updated",
                "state"
            ]
        );
        for k in ["last_changed", "last_reported", "last_updated"] {
            assert_eq!(json[k], "2026-11-14T13:29:29.778252+00:00", "{k}");
        }

        // The same write later: same state object, only `last_reported`
        // moves, and nothing fires. Its JSON keeps the time it was first
        // serialized with, as HA's cached dict.
        let mut rx = hass.bus().subscribe();
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        let again = hass.set_state(id.clone(), "20", attrs.clone());
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(
            again.last_reported.get().to_string(),
            "2026-11-14T13:29:31.778252Z"
        );
        let json = serde_json::to_value(&*again).unwrap();
        assert_eq!(json["last_reported"], "2026-11-14T13:29:29.778252+00:00");
        assert!(rx.try_recv().is_err());

        // Not yet serialized when reported: it shows the report.
        let other = EntityId::parse("sensor.u").unwrap();
        hass.set_state(other.clone(), "1", Attributes::new());
        tokio::time::advance(std::time::Duration::from_secs(3)).await;
        let reported = hass.set_state(other, "1", Attributes::new());
        let json = serde_json::to_value(&*reported).unwrap();
        assert_eq!(json["last_reported"], "2026-11-14T13:29:34.778252+00:00");
        assert_eq!(json["last_updated"], "2026-11-14T13:29:31.778252+00:00");
    }
}

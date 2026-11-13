use crate::{Clock, EntityId};
use jiff::Timestamp;
use parking_lot::RwLock;
use serde::Serialize;
use smol_str::SmolStr;
use std::collections::HashMap;
use std::sync::Arc;

pub type Attributes = serde_json::Map<String, serde_json::Value>;

/// Immutable snapshot of one entity. Shared via `Arc` between the state
/// machine, events, and API responses so it is never deep-copied.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct State {
    pub entity_id: EntityId,
    pub state: SmolStr,
    pub attributes: Attributes,
    pub last_changed: Timestamp,
    pub last_updated: Timestamp,
}

#[derive(Default)]
pub struct StateMachine {
    states: RwLock<HashMap<EntityId, Arc<State>>>,
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

    /// Returns `(current, previous)`. If nothing changed, `current` is the
    /// same `Arc` as `previous` and no new allocation is made. The time is
    /// read under the lock, so a write never gets an older `last_updated`
    /// than the write it follows.
    pub(crate) fn set(
        &self,
        entity_id: EntityId,
        state: SmolStr,
        attributes: Attributes,
        clock: &Clock,
    ) -> (Arc<State>, Option<Arc<State>>) {
        let mut states = self.states.write();
        let old = states.get(&entity_id).cloned();

        if let Some(o) = &old
            && o.state == state
            && o.attributes == attributes
        {
            return (o.clone(), old);
        }

        let now = clock.now();
        let last_changed = match &old {
            Some(o) if o.state == state => o.last_changed,
            _ => now,
        };
        let new = Arc::new(State {
            entity_id: entity_id.clone(),
            state,
            attributes,
            last_changed,
            last_updated: now,
        });
        states.insert(entity_id, new.clone());
        (new, old)
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
}

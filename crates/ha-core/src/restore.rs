//! `core.restore_state` (`helpers/restore_state.py`): the last states of
//! entities that restore themselves (scenes, assumed-state climates, ...)
//! across restarts. Dumped as HA dumps it: at start, every 15 minutes and
//! at stop; entries no entity has claimed for 7 days are dropped.

use crate::storage::Store;
use crate::{Hass, isoformat};
use jiff::{SignedDuration, Timestamp};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const KEY: &str = "core.restore_state";

/// `STATE_DUMP_INTERVAL`.
pub const DUMP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// `STATE_EXPIRATION`.
const EXPIRATION: SignedDuration = SignedDuration::from_hours(7 * 24);

/// An entity's extra restore data (`extra_restore_state_data`), read at
/// each dump.
pub type ExtraData = Arc<dyn Fn() -> Option<Value> + Send + Sync>;

#[derive(Default)]
pub struct RestoreState {
    dir: Option<PathBuf>,
    /// The previous run's entries, by entity id (`{state, extra_data, last_seen}`).
    last: Mutex<HashMap<String, Value>>,
    /// Entities that restore themselves.
    entities: Mutex<HashMap<String, Option<ExtraData>>>,
    /// Dumps write one at a time (HA's Store lock), each with its own
    /// fresh snapshot, so the last one taken is the one left on disk.
    save_turn: tokio::sync::Mutex<()>,
}

fn store(dir: &Path) -> Store {
    Store::new(dir, KEY, 1, 1, false)
}

impl RestoreState {
    /// Read `.storage/core.restore_state` (blocking; at start-up). One that
    /// can't be read is logged and treated as empty, as HA does.
    pub fn load(config_dir: &Path) -> Self {
        let items: Vec<Value> = match store(config_dir).load() {
            Ok(Some(v)) => v,
            Ok(None) => Vec::new(),
            Err(e) => {
                tracing::error!("Error loading last states: {e}");
                Vec::new()
            }
        };
        let last = items
            .into_iter()
            .filter_map(|item| {
                let id = item["state"]["entity_id"].as_str()?.to_owned();
                crate::EntityId::parse(&id).ok()?;
                Some((id, item))
            })
            .collect();
        Self {
            dir: Some(config_dir.to_owned()),
            last: Mutex::new(last),
            entities: Mutex::default(),
            save_turn: tokio::sync::Mutex::new(()),
        }
    }

    /// The state an entity had at the last dump (HA's `State.as_dict`).
    pub fn last_state(&self, entity_id: &str) -> Option<Value> {
        self.last.lock().get(entity_id).map(|i| i["state"].clone())
    }

    /// Its extra restore data, if it had any.
    pub fn last_extra_data(&self, entity_id: &str) -> Option<Value> {
        self.last
            .lock()
            .get(entity_id)
            .map(|i| i["extra_data"].clone())
            .filter(|v| !v.is_null())
    }

    /// What an entity's extra restore data is now (what a dump would write).
    pub fn extra_data_now(&self, entity_id: &str) -> Option<Value> {
        let extra = self.entities.lock().get(entity_id).cloned().flatten()?;
        extra()
    }

    /// Mark an entity as one that restores itself, so its state is dumped.
    pub fn register(&self, entity_id: &str, extra: Option<ExtraData>) {
        self.entities.lock().insert(entity_id.to_owned(), extra);
    }

    /// An entity going away (`async_restore_entity_removed`): its current
    /// state and extra data are kept as an entry, so they survive it.
    /// Call before its state is removed.
    pub fn unregister(&self, hass: &Hass, entity_id: &str) {
        let Some(extra) = self.entities.lock().remove(entity_id) else {
            return;
        };
        let Some(state) = crate::EntityId::parse(entity_id)
            .ok()
            .and_then(|id| hass.states().get(&id))
        else {
            return;
        };
        let item = json!({
            "state": &*state,
            "extra_data": extra.as_ref().and_then(|f| f()),
            "last_seen": isoformat(hass.clock().now()),
        });
        self.last.lock().insert(entity_id.to_owned(), item);
    }

    /// HA's `async_get_stored_states`: each registered entity's current
    /// state (not a `restored` placeholder) seen now, then the previous
    /// run's entries no entity wrote this run, unless expired.
    pub fn stored(&self, hass: &Hass, now: Timestamp) -> Vec<Value> {
        let mut out = Vec::new();
        // Every current state but a placeholder, restorable or not.
        let current = |id: &str| {
            crate::EntityId::parse(id)
                .ok()
                .and_then(|eid| hass.states().get(&eid))
                .filter(|s| !s.attributes.get("restored").is_some_and(|r| r == true))
        };
        for (id, extra) in self.entities.lock().iter() {
            let Some(state) = current(id) else {
                continue;
            };
            out.push(json!({
                "state": &*state,
                "extra_data": extra.as_ref().and_then(|f| f()),
                "last_seen": isoformat(now),
            }));
        }
        let expired = now - EXPIRATION;
        for (id, item) in self.last.lock().iter() {
            if current(id).is_some() {
                continue;
            }
            let seen: Option<Timestamp> = item["last_seen"].as_str().and_then(|s| s.parse().ok());
            if seen.is_some_and(|s| s < expired) {
                continue;
            }
            out.push(item.clone());
        }
        out
    }

    /// Save the stored states (`async_dump_states`). Failures are logged.
    pub async fn dump(&self, hass: &Hass) {
        let Some(dir) = self.dir.clone() else {
            return;
        };
        let _turn = self.save_turn.lock().await;
        let items = self.stored(hass, hass.clock().now());
        let saved = tokio::task::spawn_blocking(move || store(&dir).save(&items)).await;
        match saved {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!("Error saving current states: {e}"),
            Err(e) => tracing::error!("Unexpected error saving current states: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Attributes, EntityId};

    #[tokio::test]
    async fn dumps_registered_entities_and_unexpired_old_ones() {
        let dir = std::env::temp_dir().join(format!("hars-restore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let now: Timestamp = "2026-11-14T15:00:00Z".parse().unwrap();
        let old = |id: &str, seen: &str| json!({"state": {"entity_id": id, "state": "x", "attributes": {}}, "extra_data": {"k": 1}, "last_seen": seen});
        store(&dir)
            .save(&vec![
                old("scene.kept", "2026-11-10T00:00:00+00:00"),
                old("scene.expired", "2026-11-01T00:00:00+00:00"),
                old("scene.live", "2026-11-14T00:00:00+00:00"),
                old("bad id", "2026-11-14T00:00:00+00:00"),
            ])
            .unwrap();
        let r = RestoreState::load(&dir);
        assert_eq!(r.last_state("scene.kept").unwrap()["state"], "x");
        assert_eq!(r.last_extra_data("scene.kept"), Some(json!({"k": 1})));
        assert!(r.last_state("bad id").is_none());

        let hass = Hass::new();
        hass.set_state(
            EntityId::parse("scene.live").unwrap(),
            "2026-11-14T14:00:00+00:00",
            Attributes::new(),
        );
        // A placeholder isn't a state worth keeping.
        let mut restored = Attributes::new();
        restored.insert("restored".into(), json!(true));
        hass.set_state(
            EntityId::parse("scene.placeholder").unwrap(),
            "unavailable",
            restored,
        );
        r.register("scene.live", Some(Arc::new(|| Some(json!({"e": 2})))));
        r.register("scene.placeholder", None);
        let stored = r.stored(&hass, now);
        let ids: Vec<&str> = stored
            .iter()
            .map(|i| i["state"]["entity_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["scene.live", "scene.kept"]);
        assert_eq!(stored[0]["extra_data"], json!({"e": 2}));
        assert_eq!(stored[0]["last_seen"], "2026-11-14T15:00:00+00:00");

        r.dump(&hass).await;
        let again = RestoreState::load(&dir);
        assert_eq!(
            again.last_state("scene.live").unwrap()["state"],
            "2026-11-14T14:00:00+00:00"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An old entry is dropped once its entity has a real state, even one
    /// that doesn't restore; an entity that goes keeps its last state.
    #[tokio::test]
    async fn current_states_replace_entries_and_removal_keeps_one() {
        let hass = Hass::new();
        let r = RestoreState::default();
        r.last.lock().insert(
            "light.kitchen".into(),
            json!({"state": {"entity_id": "light.kitchen", "state": "on"}, "extra_data": null,
                "last_seen": "2099-01-01T00:00:00+00:00"}),
        );
        hass.set_state(
            EntityId::parse("light.kitchen").unwrap(),
            "off",
            Attributes::new(),
        );
        let now = hass.clock().now();
        assert!(r.stored(&hass, now).is_empty());

        let id = EntityId::parse("scene.gone").unwrap();
        hass.set_state(id.clone(), "2026-11-14T10:00:00+00:00", Attributes::new());
        r.register("scene.gone", None);
        r.unregister(&hass, "scene.gone");
        hass.remove_state(&id);
        let stored = r.stored(&hass, now);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0]["state"]["state"], "2026-11-14T10:00:00+00:00");
    }

    /// Dumps at once write one after the other, so the file stays whole.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_dumps_leave_valid_json() {
        let dir = std::env::temp_dir().join(format!("hars-restore-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let r = Arc::new(RestoreState {
            dir: Some(dir.clone()),
            ..Default::default()
        });
        let hass = Hass::new();
        for i in 0..200 {
            let id = format!("scene.s{i}");
            hass.set_state(EntityId::parse(&id).unwrap(), "x", Attributes::new());
            r.register(&id, None);
        }
        let dumps: Vec<_> = (0..16)
            .map(|_| {
                let (r, hass) = (r.clone(), hass.clone());
                tokio::spawn(async move { r.dump(&hass).await })
            })
            .collect();
        for d in dumps {
            d.await.unwrap();
        }
        let loaded = RestoreState::load(&dir);
        assert_eq!(loaded.last.lock().len(), 200);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! The frontend's per-user and system settings (`components/frontend/storage.py`):
//! `.storage/frontend.user_data_<user_id>` and `.storage/frontend.system_data`,
//! free-form `{key: value}` maps the frontend reads and writes over the
//! WebSocket (default panel, sidebar order, theme, language, ...).

use ha_core::storage::{Store, StoreError};
use parking_lot::Mutex;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::sync::broadcast;

const USER_PREFIX: &str = "frontend.user_data_";

/// Whose data a key belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Scope {
    User(String),
    System,
}

/// A key that was set, for subscribers.
#[derive(Debug, Clone)]
pub(crate) struct Change {
    pub scope: Scope,
    pub key: String,
}

pub struct FrontendData {
    /// `None` keeps everything in memory (tests, no config dir).
    dir: Option<PathBuf>,
    users: Mutex<HashMap<String, Map<String, Value>>>,
    system: Mutex<Map<String, Value>>,
    changes: broadcast::Sender<Change>,
    /// Serializes saves so an older snapshot never lands last.
    save_lock: Mutex<()>,
}

impl Default for FrontendData {
    fn default() -> Self {
        Self::new(None, HashMap::new(), Map::new())
    }
}

impl FrontendData {
    fn new(
        dir: Option<PathBuf>,
        users: HashMap<String, Map<String, Value>>,
        system: Map<String, Value>,
    ) -> Self {
        Self {
            dir,
            users: Mutex::new(users),
            system: Mutex::new(system),
            changes: broadcast::channel(64).0,
            save_lock: Mutex::new(()),
        }
    }

    /// Read every user's file and the system file (blocking; at start-up).
    pub fn load(config_dir: &Path) -> Result<Self, StoreError> {
        let mut users = HashMap::new();
        let storage = config_dir.join(".storage");
        if let Ok(entries) = std::fs::read_dir(&storage) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(user_id) = name.strip_prefix(USER_PREFIX) else {
                    continue;
                };
                if user_id.is_empty() || user_id.contains('.') {
                    continue; // temporary files
                }
                if let Some(data) = user_store(config_dir, user_id).load::<Map<String, Value>>()? {
                    users.insert(user_id.to_owned(), data);
                }
            }
        }
        let system = system_store(config_dir).load()?.unwrap_or_default();
        Ok(Self::new(Some(config_dir.to_owned()), users, system))
    }

    /// `data[key]`, or the whole map when `key` is `None` (users only).
    pub(crate) fn get(&self, scope: &Scope, key: Option<&str>) -> Value {
        let read = |map: &Map<String, Value>| match key {
            Some(k) => map.get(k).cloned().unwrap_or(Value::Null),
            None => Value::Object(map.clone()),
        };
        match scope {
            Scope::User(id) => self.users.lock().get(id).map_or_else(
                || {
                    if key.is_some() {
                        Value::Null
                    } else {
                        Value::Object(Map::new())
                    }
                },
                read,
            ),
            Scope::System => read(&self.system.lock()),
        }
    }

    /// Set a key and tell subscribers. Call [`FrontendData::save`] after.
    pub(crate) fn set(&self, scope: &Scope, key: &str, value: Value) {
        match scope {
            Scope::User(id) => {
                self.users
                    .lock()
                    .entry(id.clone())
                    .or_default()
                    .insert(key.to_owned(), value);
            }
            Scope::System => {
                self.system.lock().insert(key.to_owned(), value);
            }
        }
        let _ = self.changes.send(Change {
            scope: scope.clone(),
            key: key.to_owned(),
        });
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.changes.subscribe()
    }

    /// Write one scope's file (blocking).
    pub(crate) fn save(&self, scope: &Scope) -> std::io::Result<()> {
        let Some(dir) = &self.dir else {
            return Ok(());
        };
        let _order = self.save_lock.lock();
        match scope {
            Scope::User(id) => {
                let data = self.users.lock().get(id).cloned().unwrap_or_default();
                user_store(dir, id).save(&data)
            }
            Scope::System => {
                let data = self.system.lock().clone();
                system_store(dir).save(&data)
            }
        }
    }
}

fn user_store(dir: &Path, user_id: &str) -> Store {
    Store::new(dir, format!("{USER_PREFIX}{user_id}"), 1, 1, false)
}

fn system_store(dir: &Path) -> Store {
    Store::new(dir, "frontend.system_data", 1, 1, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_through_has_files() {
        let dir = std::env::temp_dir().join(format!("hars-frontend-data-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let data = FrontendData::load(&dir).unwrap();
        let user = Scope::User("u1".into());
        assert_eq!(data.get(&user, Some("core")), Value::Null);
        assert_eq!(data.get(&user, None), json!({}));

        let mut changes = data.subscribe();
        data.set(&user, "core", json!({"default_panel": "home"}));
        data.set(
            &Scope::System,
            "core",
            json!({"onboarded_version": "2026.6.1"}),
        );
        assert_eq!(changes.try_recv().unwrap().key, "core");
        data.save(&user).unwrap();
        data.save(&Scope::System).unwrap();

        let again = FrontendData::load(&dir).unwrap();
        assert_eq!(
            again.get(&user, Some("core")),
            json!({"default_panel": "home"})
        );
        assert_eq!(
            again.get(&user, None),
            json!({"core": {"default_panel": "home"}})
        );
        assert_eq!(
            again.get(&Scope::System, Some("core"))["onboarded_version"],
            "2026.6.1"
        );
        let raw: Value = serde_json::from_slice(
            &std::fs::read(dir.join(".storage/frontend.user_data_u1")).unwrap(),
        )
        .unwrap();
        assert_eq!(raw["key"], "frontend.user_data_u1");
        assert_eq!(
            (raw["version"].clone(), raw["minor_version"].clone()),
            (json!(1), json!(1))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}

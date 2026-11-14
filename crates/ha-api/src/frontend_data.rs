//! The frontend's per-user and system settings (`components/frontend/storage.py`):
//! `.storage/frontend.user_data_<user_id>` and `.storage/frontend.system_data`,
//! free-form `{key: value}` maps the frontend reads and writes over the
//! WebSocket (default panel, sidebar order, theme, language, ...).

use ha_core::storage::{Store, StoreError};
use parking_lot::Mutex;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::broadcast;

const USER_PREFIX: &str = "frontend.user_data_";

/// Whose data a key belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    /// Per scope: (version of the data, version on disk). Every set bumps
    /// the first; a save that finds the second caught up has nothing to do.
    versions: Mutex<HashMap<Scope, (u64, u64)>>,
    /// Saves take turns here: an async lock, so waiting writers don't hold
    /// blocking threads, and an older snapshot never lands last.
    save_turn: tokio::sync::Mutex<()>,
    #[cfg(test)]
    writes: std::sync::atomic::AtomicUsize,
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
            versions: Mutex::new(HashMap::new()),
            save_turn: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            writes: Default::default(),
        }
    }

    /// Read every user's file and the system file (blocking; at start-up).
    /// A file that can't be read doesn't stop the server: it is logged, and
    /// a corrupt one is set aside as `<file>.corrupt.<time>`, as HA does.
    pub fn load(config_dir: &Path) -> Self {
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
                if let Some(data) = load_or_quarantine(&user_store(config_dir, user_id)) {
                    users.insert(user_id.to_owned(), data);
                }
            }
        }
        let system = load_or_quarantine(&system_store(config_dir)).unwrap_or_default();
        Self::new(Some(config_dir.to_owned()), users, system)
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
        self.versions.lock().entry(scope.clone()).or_default().0 += 1;
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

    /// Write a scope's file, if it changed since the last write. Saves
    /// queue on an async lock and coalesce: a burst of sets costs one write
    /// per turn, and only the write itself runs on a blocking thread.
    pub(crate) async fn save(self: &Arc<Self>, scope: &Scope) -> std::io::Result<()> {
        let Some(dir) = self.dir.clone() else {
            return Ok(());
        };
        let _turn = self.save_turn.lock().await;
        // The data and its version, read together.
        let (version, data) = {
            let versions = self.versions.lock();
            let &(version, saved) = versions.get(scope).unwrap_or(&(0, 0));
            if saved >= version {
                return Ok(()); // an earlier turn already wrote this
            }
            let data = match scope {
                Scope::User(id) => self.users.lock().get(id).cloned().unwrap_or_default(),
                Scope::System => self.system.lock().clone(),
            };
            (version, data)
        };
        let store = match scope {
            Scope::User(id) => user_store(&dir, id),
            Scope::System => system_store(&dir),
        };
        #[cfg(test)]
        self.writes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tokio::task::spawn_blocking(move || store.save(&data))
            .await
            .map_err(std::io::Error::other)??;
        let mut versions = self.versions.lock();
        let entry = versions.entry(scope.clone()).or_default();
        entry.1 = entry.1.max(version);
        Ok(())
    }
}

/// `Store.async_load`'s fallback: corrupt JSON is renamed out of the way and
/// the store starts empty; other errors are logged and the data skipped.
fn load_or_quarantine(store: &Store) -> Option<Map<String, Value>> {
    match store.load::<Map<String, Value>>() {
        Ok(data) => data,
        Err(StoreError::Json { path, source }) => {
            let aside = format!(
                "{}.corrupt.{}",
                path.display(),
                jiff::Timestamp::now().as_second()
            );
            tracing::error!(
                "{} is not valid ({source}); moved to {aside}",
                path.display()
            );
            if let Err(e) = std::fs::rename(&path, &aside) {
                tracing::error!("could not move {}: {e}", path.display());
            }
            None
        }
        Err(e) => {
            tracing::error!("{e}; starting without it");
            None
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

    #[tokio::test]
    async fn round_trips_through_has_files() {
        let dir = std::env::temp_dir().join(format!("hars-frontend-data-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let data = Arc::new(FrontendData::load(&dir));
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
        data.save(&user).await.unwrap();
        data.save(&Scope::System).await.unwrap();

        let again = FrontendData::load(&dir);
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

    #[tokio::test]
    async fn a_corrupt_file_is_set_aside_not_fatal() {
        let dir =
            std::env::temp_dir().join(format!("hars-frontend-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let storage = dir.join(".storage");
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(storage.join("frontend.user_data_bad"), "{not json").unwrap();
        std::fs::write(storage.join("frontend.system_data"), "[").unwrap();
        std::fs::write(
            storage.join("frontend.user_data_good"),
            json!({"version": 1, "minor_version": 1, "key": "frontend.user_data_good", "data": {"k": 1}}).to_string(),
        )
        .unwrap();

        let data = FrontendData::load(&dir);
        assert_eq!(data.get(&Scope::User("good".into()), Some("k")), json!(1));
        assert_eq!(data.get(&Scope::User("bad".into()), None), json!({}));
        assert_eq!(data.get(&Scope::System, Some("core")), Value::Null);
        let names: Vec<String> = std::fs::read_dir(&storage)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("frontend.user_data_bad.corrupt.")),
            "{names:?}"
        );
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("frontend.system_data.corrupt.")),
            "{names:?}"
        );
        assert!(!names.contains(&"frontend.user_data_bad".to_string()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_burst_of_saves_coalesces() {
        let dir = std::env::temp_dir().join(format!("hars-frontend-burst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let data = Arc::new(FrontendData::load(&dir));
        let user = Scope::User("u1".into());
        let mut saves = Vec::new();
        for i in 0..200 {
            data.set(&user, "n", json!(i));
            let (data, user) = (data.clone(), user.clone());
            saves.push(tokio::spawn(async move { data.save(&user).await }));
        }
        for s in saves {
            s.await.unwrap().unwrap();
        }
        let writes = data.writes.load(std::sync::atomic::Ordering::Relaxed);
        assert!((1..=2).contains(&writes), "{writes} writes for 200 saves");
        assert_eq!(FrontendData::load(&dir).get(&user, Some("n")), json!(199));
        // Nothing changed: nothing written.
        data.save(&user).await.unwrap();
        assert_eq!(
            data.writes.load(std::sync::atomic::Ordering::Relaxed),
            writes
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}

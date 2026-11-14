//! Lovelace dashboards (`components/lovelace`): HA's `.storage/lovelace_dashboards`
//! (the dashboards list), `lovelace.<id>` (each storage dashboard's config),
//! `lovelace` (the default dashboard) and `lovelace_resources`.
//!
//! Storage mode only: YAML dashboards arrive with `configuration.yaml`.

use ha_core::storage::{Store, StoreError};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const DASHBOARDS_KEY: &str = "lovelace_dashboards";
pub const RESOURCES_KEY: &str = "lovelace_resources";
pub const DEFAULT_KEY: &str = "lovelace";
const DASHBOARD_PREFIX: &str = "lovelace.";
/// `DEFAULT_ICON` for a dashboard without one.
const DEFAULT_ICON: &str = "mdi:view-dashboard";

/// A dashboard's store key and its stored data.
type Dashboard = (String, DashboardConfig);

#[derive(Debug, Default, Serialize, Deserialize)]
struct Collection {
    #[serde(default)]
    items: Vec<Map<String, Value>>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct DashboardConfig {
    #[serde(default)]
    config: Option<Value>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// Why a config request failed, as HA's `_handle_errors` reports it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ConfigError {
    /// `config_not_found`, "Unknown config specified: <url_path>".
    UnknownDashboard(Option<String>),
    /// `config_not_found`, "No config found.".
    NoConfig,
    /// `error`: the file couldn't be written or removed.
    Save(String),
}

pub struct Lovelace {
    dir: Option<PathBuf>,
    /// `lovelace_dashboards` items, in order.
    dashboards: Vec<Map<String, Value>>,
    resources: Vec<Map<String, Value>>,
    /// By `url_path` (`None` is the default dashboard): its store key and
    /// its config, `None` while it has none.
    configs: Mutex<HashMap<Option<String>, Dashboard>>,
    save_turn: tokio::sync::Mutex<()>,
}

/// A fresh install: only the default dashboard, with no config.
impl Default for Lovelace {
    fn default() -> Self {
        Self {
            dir: None,
            dashboards: Vec::new(),
            resources: Vec::new(),
            configs: Mutex::new(HashMap::from([(
                None,
                (DEFAULT_KEY.to_owned(), DashboardConfig::default()),
            )])),
            save_turn: Default::default(),
        }
    }
}

impl Lovelace {
    /// Read the dashboards for the server (blocking; at start-up). A file
    /// that can't be read doesn't stop the server: corrupt JSON is moved to
    /// `<file>.corrupt.<time>` (as HA's `Store` does) and that dashboard
    /// starts without a config; a dashboard with an unsafe `id` is skipped.
    pub fn load(config_dir: &Path) -> Result<Self, StoreError> {
        Self::load_with(config_dir, false)
    }

    /// Any problem is an error and nothing is moved: for the importer.
    pub fn load_strict(config_dir: &Path) -> Result<Self, StoreError> {
        Self::load_with(config_dir, true)
    }

    fn load_with(config_dir: &Path, strict: bool) -> Result<Self, StoreError> {
        let read = |store: Store| -> Result<Option<Value>, StoreError> {
            match store.load::<Value>() {
                Ok(v) => Ok(v),
                Err(e) if strict => Err(e),
                Err(StoreError::Json { path, .. }) => {
                    let aside = format!(
                        "{}.corrupt.{}",
                        path.display(),
                        jiff::Timestamp::now().as_second()
                    );
                    tracing::error!("{} is not valid JSON; moved to {aside}", path.display());
                    if let Err(e) = std::fs::rename(&path, &aside) {
                        tracing::error!("could not move {}: {e}", path.display());
                    }
                    Ok(None)
                }
                Err(e) => {
                    tracing::error!("{e}; starting without it");
                    Ok(None)
                }
            }
        };
        // The data's shape: an error when strict, else logged and skipped.
        fn typed<T: serde::de::DeserializeOwned + Default>(
            v: Option<Value>,
            store: &Store,
            strict: bool,
        ) -> Result<T, StoreError> {
            let Some(v) = v else {
                return Ok(T::default());
            };
            match serde_json::from_value(v) {
                Ok(t) => Ok(t),
                Err(source) if strict => Err(StoreError::Json {
                    path: store.path().to_owned(),
                    source,
                }),
                Err(e) => {
                    tracing::error!(
                        "{} has an unexpected shape ({e}); ignoring it",
                        store.path().display()
                    );
                    Ok(T::default())
                }
            }
        }
        let collection = |key: &'static str| -> Result<Collection, StoreError> {
            let store = collection_store(config_dir, key);
            typed(read(store.clone())?, &store, strict)
        };
        let dashboard = |key: &str| -> Result<DashboardConfig, StoreError> {
            let store = config_store(config_dir, key);
            typed(read(store.clone())?, &store, strict)
        };

        let dashboards = collection(DASHBOARDS_KEY)?;
        let resources = collection(RESOURCES_KEY)?;
        let mut configs = HashMap::new();
        configs.insert(None, (DEFAULT_KEY.to_owned(), dashboard(DEFAULT_KEY)?));
        let mut kept = Vec::new();
        for item in dashboards.items {
            let (Some(id), Some(url_path)) = (str_field(&item, "id"), str_field(&item, "url_path"))
            else {
                continue;
            };
            // HA's ids are slugs. Anything else could leave `.storage`.
            if !is_slug(id) {
                if strict {
                    return Err(StoreError::Io {
                        path: config_dir.join(".storage").join(DASHBOARDS_KEY),
                        source: std::io::Error::other(format!("dashboard id {id:?} is not a slug")),
                    });
                }
                tracing::error!("skipping dashboard with unsafe id {id:?}");
                continue;
            }
            let key = format!("{DASHBOARD_PREFIX}{id}");
            configs.insert(Some(url_path.to_owned()), (key.clone(), dashboard(&key)?));
            kept.push(item);
        }
        Ok(Self {
            dir: Some(config_dir.to_owned()),
            dashboards: kept,
            resources: resources.items,
            configs: Mutex::new(configs),
            save_turn: Default::default(),
        })
    }

    /// The store keys a config directory's dashboards use, for the importer.
    pub fn is_store(key: &str) -> bool {
        key == DASHBOARDS_KEY
            || key == RESOURCES_KEY
            || key == DEFAULT_KEY
            || key.starts_with(DASHBOARD_PREFIX)
    }

    /// `_handle_errors`' lookup: `None` prefers a dashboard at `lovelace`,
    /// else the default one.
    fn resolve(&self, url_path: Option<&str>) -> Result<Option<String>, ConfigError> {
        let configs = self.configs.lock();
        let key = match url_path {
            None if configs.contains_key(&Some(DEFAULT_KEY.to_owned())) => {
                Some(DEFAULT_KEY.to_owned())
            }
            None => None,
            Some(p) => Some(p.to_owned()),
        };
        if configs.contains_key(&key) {
            Ok(key)
        } else {
            Err(ConfigError::UnknownDashboard(url_path.map(str::to_owned)))
        }
    }

    /// `lovelace/config`.
    pub(crate) fn config(&self, url_path: Option<&str>) -> Result<Value, ConfigError> {
        let key = self.resolve(url_path)?;
        self.configs
            .lock()
            .get(&key)
            .and_then(|(_, d)| d.config.clone())
            .ok_or(ConfigError::NoConfig)
    }

    /// `lovelace/config/save` (`config: None` is `/delete`). Returns the
    /// `url_path` for the `lovelace_updated` event.
    pub(crate) async fn save(
        self: &Arc<Self>,
        url_path: Option<&str>,
        config: Option<Value>,
    ) -> Result<Option<String>, ConfigError> {
        let _turn = self.save_turn.lock().await;
        let key = self.resolve(url_path)?;
        let (store_key, mut data) = self.configs.lock().get(&key).cloned().expect("resolved");
        let delete = config.is_none();
        // Only `config` changes; other fields in the file are kept.
        data.config = config;
        // The file first: the cache (and the event) only change once it's done.
        if let Some(dir) = self.dir.clone() {
            let to_write = data.clone();
            let written = tokio::task::spawn_blocking(move || {
                let store = config_store(&dir, &store_key);
                if delete {
                    match std::fs::remove_file(store.path()) {
                        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                        _ => Ok(()),
                    }
                } else {
                    store.save(&to_write)
                }
            })
            .await;
            match written {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(ConfigError::Save(e.to_string())),
                Err(e) => return Err(ConfigError::Save(e.to_string())),
            }
        }
        if delete {
            data = DashboardConfig::default();
        }
        if let Some(entry) = self.configs.lock().get_mut(&key) {
            entry.1 = data;
        }
        Ok(key)
    }

    /// `lovelace/dashboards/list`: the stored dashboards.
    pub(crate) fn dashboards_json(&self) -> Value {
        Value::Array(self.dashboards.iter().cloned().map(Value::Object).collect())
    }

    /// `lovelace/resources`.
    pub(crate) fn resources_json(&self) -> Value {
        Value::Array(self.resources.iter().cloned().map(Value::Object).collect())
    }

    /// The panels `_register_panel` adds, one per dashboard.
    pub(crate) fn panels(&self) -> Vec<(String, Value)> {
        self.dashboards
            .iter()
            .filter_map(|item| {
                let url = str_field(item, "url_path")?;
                let mode = str_field(item, "mode").unwrap_or("storage");
                Some((
                    url.to_owned(),
                    json!({
                        "component_name": "lovelace",
                        "icon": item.get("icon").filter(|v| !v.is_null()).cloned().unwrap_or(json!(DEFAULT_ICON)),
                        "title": item.get("title").cloned().unwrap_or(Value::Null),
                        "default_visible": true,
                        "config": {"mode": mode},
                        "url_path": url,
                        "require_admin": item.get("require_admin").cloned().unwrap_or(json!(false)),
                        "config_panel_domain": null,
                        "show_in_sidebar": item.get("show_in_sidebar").cloned().unwrap_or(json!(true)),
                    }),
                ))
            })
            .collect()
    }

    /// Whether `url_path` is one of the dashboards (for the index page).
    pub(crate) fn has_dashboard(&self, url_path: &str) -> bool {
        self.dashboards
            .iter()
            .any(|i| str_field(i, "url_path") == Some(url_path))
    }
}

/// `slugify` output: what HA's dashboard ids look like.
fn is_slug(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn str_field<'a>(item: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    item.get(key).and_then(Value::as_str)
}

fn collection_store(dir: &Path, key: &'static str) -> Store {
    Store::new(dir, key, 1, 1, false)
}

fn config_store(dir: &Path, key: &str) -> Store {
    Store::new(dir, key.to_owned(), 1, 1, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, key: &str, data: Value) {
        std::fs::create_dir_all(dir.join(".storage")).unwrap();
        let v = json!({"version": 1, "minor_version": 1, "key": key, "data": data});
        std::fs::write(dir.join(".storage").join(key), v.to_string()).unwrap();
    }

    fn ha_dashboards(dir: &Path) {
        write(
            dir,
            DASHBOARDS_KEY,
            json!({"items": [
                {"id": "map", "icon": "mdi:map", "title": "Map", "url_path": "map", "require_admin": false, "show_in_sidebar": true, "mode": "storage"},
                {"id": "lovelace", "icon": null, "title": "Overview", "url_path": "lovelace", "require_admin": false, "show_in_sidebar": true, "mode": "storage"},
                {"id": "dashboard_energy", "title": "Energy", "url_path": "dashboard-energy", "require_admin": true, "show_in_sidebar": false, "mode": "storage"}
            ]}),
        );
        write(
            dir,
            "lovelace.map",
            json!({"config": {"strategy": {"type": "map"}}}),
        );
        write(
            dir,
            "lovelace.lovelace",
            json!({"config": {"views": [{"title": "Home"}]}}),
        );
        write(
            dir,
            RESOURCES_KEY,
            json!({"items": [{"id": "abc", "type": "module", "url": "/local/card.js"}]}),
        );
    }

    #[tokio::test]
    async fn serves_and_saves_has_dashboards() {
        let dir = std::env::temp_dir().join(format!("hars-lovelace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        ha_dashboards(&dir);
        let l = Arc::new(Lovelace::load(&dir).unwrap());

        // `url_path: null` prefers the dashboard at `lovelace`.
        assert_eq!(
            l.config(None).unwrap(),
            json!({"views": [{"title": "Home"}]})
        );
        assert_eq!(
            l.config(Some("map")).unwrap(),
            json!({"strategy": {"type": "map"}})
        );
        assert_eq!(
            l.config(Some("dashboard-energy")),
            Err(ConfigError::NoConfig)
        );
        assert_eq!(
            l.config(Some("nope")),
            Err(ConfigError::UnknownDashboard(Some("nope".into())))
        );
        assert_eq!(l.resources_json()[0]["url"], "/local/card.js");
        assert_eq!(l.dashboards_json().as_array().unwrap().len(), 3);
        assert!(l.has_dashboard("dashboard-energy") && !l.has_dashboard("home"));

        let panels: HashMap<String, Value> = l.panels().into_iter().collect();
        assert_eq!(panels["map"]["config"], json!({"mode": "storage"}));
        assert_eq!(panels["lovelace"]["icon"], DEFAULT_ICON);
        assert_eq!(panels["dashboard-energy"]["require_admin"], true);

        // Saved in HA's format, and read back after a restart.
        let new = json!({"views": [{"title": "Kitchen"}]});
        assert_eq!(
            l.save(Some("dashboard-energy"), Some(new.clone())).await,
            Ok(Some("dashboard-energy".into()))
        );
        let raw: Value = serde_json::from_slice(
            &std::fs::read(dir.join(".storage/lovelace.dashboard_energy")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            raw,
            json!({"version": 1, "minor_version": 1, "key": "lovelace.dashboard_energy", "data": {"config": new}})
        );
        assert_eq!(
            Lovelace::load(&dir)
                .unwrap()
                .config(Some("dashboard-energy"))
                .unwrap(),
            new
        );
        // Delete removes the file.
        l.save(Some("map"), None).await.unwrap();
        assert!(!dir.join(".storage/lovelace.map").exists());
        assert_eq!(l.config(Some("map")), Err(ConfigError::NoConfig));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_fresh_install_has_only_the_default_dashboard_without_config() {
        let l = Lovelace::load(&std::env::temp_dir().join("hars-lovelace-none")).unwrap();
        assert_eq!(l.config(None), Err(ConfigError::NoConfig));
        assert_eq!(
            l.config(Some("lovelace")),
            Err(ConfigError::UnknownDashboard(Some("lovelace".into())))
        );
        assert!(l.panels().is_empty());
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hars-lovelace-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A dashboard id is part of a file name: one that isn't a slug can't be
    /// used to reach outside `.storage`.
    #[test]
    fn unsafe_dashboard_ids_are_refused() {
        let dir = temp("traversal");
        write(
            &dir,
            DASHBOARDS_KEY,
            json!({"items": [
                {"id": "/../../victim", "url_path": "evil", "mode": "storage"},
                {"id": "ok_one", "url_path": "fine", "mode": "storage"}
            ]}),
        );
        assert!(Lovelace::load_strict(&dir).is_err());
        let l = Lovelace::load(&dir).unwrap();
        assert!(!l.has_dashboard("evil") && l.has_dashboard("fine"));
        assert_eq!(
            l.config(Some("evil")),
            Err(ConfigError::UnknownDashboard(Some("evil".into())))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn saving_keeps_the_rest_of_the_file() {
        let dir = temp("extra");
        ha_dashboards(&dir);
        write(
            &dir,
            "lovelace.map",
            json!({"config": {"a": 1}, "future_field": [1, 2]}),
        );
        let l = Arc::new(Lovelace::load(&dir).unwrap());
        l.save(Some("map"), Some(json!({"a": 2}))).await.unwrap();
        let raw: Value =
            serde_json::from_slice(&std::fs::read(dir.join(".storage/lovelace.map")).unwrap())
                .unwrap();
        assert_eq!(
            raw["data"],
            json!({"config": {"a": 2}, "future_field": [1, 2]})
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A delete that can't remove the file fails, and the dashboard stays.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_delete_is_an_error_and_changes_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("readonly");
        ha_dashboards(&dir);
        let l = Arc::new(Lovelace::load(&dir).unwrap());
        let storage = dir.join(".storage");
        std::fs::set_permissions(&storage, std::fs::Permissions::from_mode(0o555)).unwrap();
        let res = l.save(Some("map"), None).await;
        std::fs::set_permissions(&storage, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(res, Err(ConfigError::Save(_))), "{res:?}");
        assert_eq!(
            l.config(Some("map")).unwrap(),
            json!({"strategy": {"type": "map"}})
        );
        assert!(storage.join("lovelace.map").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_corrupt_dashboard_is_set_aside_not_fatal() {
        let dir = temp("corrupt");
        ha_dashboards(&dir);
        std::fs::write(dir.join(".storage/lovelace.map"), "{oops").unwrap();
        assert!(Lovelace::load_strict(&dir).is_err());
        assert!(
            dir.join(".storage/lovelace.map").exists(),
            "strict loading moves nothing"
        );
        let l = Lovelace::load(&dir).unwrap();
        assert_eq!(l.config(Some("map")), Err(ConfigError::NoConfig));
        assert_eq!(
            l.config(None).unwrap(),
            json!({"views": [{"title": "Home"}]})
        );
        assert!(!dir.join(".storage/lovelace.map").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

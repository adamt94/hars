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

/// A dashboard's store key and its config (`None` while it has none).
type Dashboard = (String, Option<Value>);

#[derive(Debug, Default, Serialize, Deserialize)]
struct Collection {
    #[serde(default)]
    items: Vec<Map<String, Value>>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
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
            configs: Mutex::new(HashMap::from([(None, (DEFAULT_KEY.to_owned(), None))])),
            save_turn: Default::default(),
        }
    }
}

impl Lovelace {
    /// Read the dashboards (blocking; at start-up). Any problem is an error:
    /// these are the user's dashboards, so they are never silently dropped.
    pub fn load(config_dir: &Path) -> Result<Self, StoreError> {
        let dashboards: Collection = collection_store(config_dir, DASHBOARDS_KEY)
            .load()?
            .unwrap_or_default();
        let resources: Collection = collection_store(config_dir, RESOURCES_KEY)
            .load()?
            .unwrap_or_default();
        let mut configs = HashMap::new();
        let default: DashboardConfig = config_store(config_dir, DEFAULT_KEY)
            .load()?
            .unwrap_or_default();
        configs.insert(None, (DEFAULT_KEY.to_owned(), default.config));
        for item in &dashboards.items {
            let (Some(id), Some(url_path)) = (str_field(item, "id"), str_field(item, "url_path"))
            else {
                continue;
            };
            let key = format!("{DASHBOARD_PREFIX}{id}");
            let data: DashboardConfig = config_store(config_dir, &key).load()?.unwrap_or_default();
            configs.insert(Some(url_path.to_owned()), (key, data.config));
        }
        Ok(Self {
            dir: Some(config_dir.to_owned()),
            dashboards: dashboards.items,
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
            .and_then(|(_, c)| c.clone())
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
        let store_key = {
            let mut configs = self.configs.lock();
            let entry = configs.get_mut(&key).expect("resolved");
            entry.1 = config.clone();
            entry.0.clone()
        };
        if let Some(dir) = self.dir.clone() {
            let written = tokio::task::spawn_blocking(move || {
                let store = config_store(&dir, &store_key);
                match config {
                    Some(config) => store.save(&json!({ "config": config })),
                    None => match std::fs::remove_file(store.path()) {
                        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                        _ => Ok(()),
                    },
                }
            })
            .await;
            match written {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::error!("saving the dashboard: {e}"),
                Err(e) => tracing::error!("saving the dashboard: {e}"),
            }
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
}

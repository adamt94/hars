//! Integrations: what turns devices and services into entities. Each one is
//! started for its config entries (HA's `core.config_entries`).

pub mod sun;

use ha_core::Hass;
use ha_registry::Registries;
use serde_json::{Map, Value};
use std::sync::Arc;

/// `.storage/core.config_entries` (1.5): every integration's entries, or
/// `None` if the store doesn't exist (a fresh install). One that can't be
/// read is logged and treated as no entries (nothing starts), so a bad file
/// can't stop the server.
pub fn load_entries(config_dir: &std::path::Path) -> Option<Vec<Map<String, Value>>> {
    #[derive(serde::Deserialize)]
    struct Entries {
        #[serde(default)]
        entries: Vec<Map<String, Value>>,
    }
    let store = ha_core::storage::Store::new(config_dir, "core.config_entries", 1, 5, true);
    match store.load::<Entries>() {
        Ok(data) => data.map(|d| d.entries),
        Err(e) => {
            tracing::error!("{e}; starting no integrations");
            Some(Vec::new())
        }
    }
}

/// The integrations `hars` can run, by HA domain.
pub const SUPPORTED: &[&str] = &[sun::DOMAIN];

/// Start the integrations for these config entries (each a
/// `core.config_entries` item). With no config entries at all (a fresh
/// install), `sun` is started as HA's default configuration does.
/// Returns the domains running, for `get_config`'s `components`.
pub fn start(
    hass: &Hass,
    registries: Arc<Registries>,
    entries: Option<&[Map<String, Value>]>,
) -> Vec<&'static str> {
    let field =
        |e: &Map<String, Value>, k: &str| e.get(k).and_then(Value::as_str).map(str::to_owned);
    let sun_entry = match entries {
        None => Some("hars-sun".to_owned()),
        Some(entries) => entries
            .iter()
            .filter(|e| field(e, "domain").as_deref() == Some(sun::DOMAIN))
            .find(|e| e.get("disabled_by").is_none_or(Value::is_null))
            .and_then(|e| field(e, "entry_id")),
    };
    let mut running = Vec::new();
    if let Some(entry_id) = sun_entry {
        tokio::spawn(sun::run(hass.clone(), registries, entry_id));
        running.extend(["sun", "sensor", "binary_sensor"]);
    }
    running
}

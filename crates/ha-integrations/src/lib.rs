//! Integrations: what turns devices and services into entities. Each one is
//! started for its config entries (HA's `core.config_entries`).

pub mod button;
pub mod climate;
pub mod components;
pub mod hue;
pub mod light;
pub mod met;
pub mod num;
pub mod scene;
pub mod sun;
pub mod switch;
pub mod weather;

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
pub const SUPPORTED: &[&str] = &[sun::DOMAIN, met::DOMAIN, hue::DOMAIN];

/// Who `hars` says it is to the services it calls.
pub const USER_AGENT: &str = concat!(
    "hars/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/adamt94/homeassistant-rust)"
);

/// What [`start`] started.
pub struct Started {
    /// The domains running, for `get_config`'s `components`.
    pub running: Vec<&'static str>,
    /// The weather entities, for the forecast APIs.
    pub weather: Arc<weather::Weather>,
}

/// Start the integrations for these config entries (each a
/// `core.config_entries` item). With no config entries at all (a fresh
/// install), `sun` is started as HA's default configuration does.
pub fn start(
    hass: &Hass,
    registries: Arc<Registries>,
    entries: Option<&[Map<String, Value>]>,
    profiles: light::Profiles,
) -> Started {
    let field =
        |e: &Map<String, Value>, k: &str| e.get(k).and_then(Value::as_str).map(str::to_owned);
    let enabled = |domain: &'static str| {
        entries
            .unwrap_or_default()
            .iter()
            .filter(move |e| field(e, "domain").as_deref() == Some(domain))
            .filter(|e| e.get("disabled_by").is_none_or(Value::is_null))
    };
    let sun_entry = match entries {
        None => Some("hars-sun".to_owned()),
        Some(_) => enabled(sun::DOMAIN)
            .next()
            .and_then(|e| field(e, "entry_id")),
    };
    let mut running = Vec::new();
    if let Some(entry_id) = sun_entry {
        tokio::spawn(sun::run(hass.clone(), registries.clone(), entry_id));
        running.extend(["sun", "sensor", "binary_sensor"]);
    }

    let weather = Arc::new(weather::Weather::default());
    let met: Vec<Map<String, Value>> = enabled(met::DOMAIN).cloned().collect();
    if !met.is_empty() {
        for entry in met {
            tokio::spawn(met::run(
                hass.clone(),
                registries.clone(),
                weather.clone(),
                USER_AGENT,
                met::URL.to_owned(),
                entry,
            ));
        }
        weather.register_services(hass, registries.clone());
        running.extend(["met", "weather"]);
    }
    let components = components::Components::new(hass, registries.clone(), profiles);
    let hue: Vec<Map<String, Value>> = enabled(hue::DOMAIN).cloned().collect();
    if !hue.is_empty() {
        let platforms = hue::Platforms::new(&components);
        for entry in hue {
            tokio::spawn(hue::run(platforms.clone(), entry));
        }
        running.extend(["hue", "light", "sensor", "binary_sensor", "switch", "scene"]);
    }
    Started { running, weather }
}

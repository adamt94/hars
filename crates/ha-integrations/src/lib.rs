//! Integrations: what turns devices and services into entities. Each one is
//! started for its config entries (HA's `core.config_entries`).

pub mod button;
pub mod climate;
pub mod components;
pub mod config_entries;
pub mod hue;
pub mod lifx;
pub mod light;
pub mod met;
pub mod mobile_app;
pub mod naming;
pub mod num;
pub mod onboarding;
pub mod scene;
pub mod sun;
pub mod switch;
pub mod switchbot_cloud;
pub mod weather;

use ha_core::Hass;
use ha_registry::Registries;
use serde_json::{Map, Value};
use std::sync::Arc;

/// The integrations `hars` can run, by HA domain.
pub const SUPPORTED: &[&str] = &[
    sun::DOMAIN,
    met::DOMAIN,
    hue::DOMAIN,
    switchbot_cloud::DOMAIN,
    lifx::DOMAIN,
];

/// The config flow `VERSION` of every integration `hars` runs (and of
/// `mobile_app`), at the pinned Home Assistant: 1.
pub const ENTRY_VERSION: u64 = 1;

/// Whether a config entry was written by a newer version of its config
/// flow than `hars` runs. Later Home Assistant releases migrate entries
/// (2026.10 `lifx` and 2026.11 `met` to version 2, with other `data`);
/// `ConfigEntry.async_migrate` doesn't set such an entry up
/// (`migration_error`), and neither does `hars`. A newer `minor_version`
/// is compatible by Home Assistant's rule, and runs.
pub fn entry_too_new(entry: &Map<String, Value>) -> bool {
    entry
        .get("version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v > ENTRY_VERSION)
}

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

/// A fresh install (no `core.config_entries` file) gets the entries HA's
/// default configuration imports at its first start: `sun`'s (`sun`'s
/// `async_setup` runs its import flow when it has no entry). Saved like
/// any new entry.
pub fn import_default_entries(entries: &config_entries::ConfigEntries) {
    if entries.list().is_none() {
        let sun = entries.new_entry(sun::DOMAIN, "import", "Sun", None, Map::new());
        entries.add(sun);
    }
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
            .filter(|e| !entry_too_new(e))
    };
    // `ConfigEntry.async_migrate`'s error for an entry no migration reaches.
    for e in entries
        .unwrap_or_default()
        .iter()
        .filter(|e| entry_too_new(e))
    {
        let domain = field(e, "domain").unwrap_or_default();
        if SUPPORTED.contains(&domain.as_str()) {
            tracing::error!(
                "Migration handler not found for entry {} for {domain}",
                field(e, "title").unwrap_or_default()
            );
        }
    }
    let sun_entry = match entries {
        None => Some("hars-sun".to_owned()),
        Some(_) => enabled(sun::DOMAIN)
            .next()
            .and_then(|e| field(e, "entry_id")),
    };
    let mut running = Vec::new();
    if let Some(entry_id) = sun_entry {
        tokio::spawn(sun::run(
            hass.clone(),
            registries.clone(),
            entry_id,
            hass.setup_guard(),
        ));
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
                hass.setup_guard(),
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
            tokio::spawn(hue::run(platforms.clone(), entry, hass.setup_guard()));
        }
        running.extend(["hue", "light", "sensor", "binary_sensor", "switch", "scene"]);
    }
    let switchbot: Vec<Map<String, Value>> = enabled(switchbot_cloud::DOMAIN).cloned().collect();
    if !switchbot.is_empty() {
        // The components exist from startup, as HA sets them up before
        // the entries.
        let _ = (
            components.buttons(),
            components.climates(),
            components.switches(),
        );
        for entry in switchbot {
            tokio::spawn(switchbot_cloud::run(
                components.clone(),
                entry,
                switchbot_cloud::api::HOST.to_owned(),
                hass.setup_guard(),
            ));
        }
        running.extend(["switchbot_cloud", "button", "climate", "sensor", "switch"]);
    }
    let lifx: Vec<Map<String, Value>> = enabled(lifx::DOMAIN).cloned().collect();
    if !lifx.is_empty() {
        let _ = (components.lights(), components.buttons());
        for entry in lifx {
            tokio::spawn(lifx::run(
                components.clone(),
                entry,
                lifx::conn::PORT,
                hass.setup_guard(),
            ));
        }
        running.extend(["lifx", "light", "button"]);
    }
    running.sort_unstable();
    running.dedup();
    Started { running, weather }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(domain: &str, version: u64, minor: u64) -> Map<String, Value> {
        json!({"entry_id": format!("{domain}-{version}"), "domain": domain, "title": "T",
               "disabled_by": null, "version": version, "minor_version": minor, "data": {}})
        .as_object()
        .unwrap()
        .clone()
    }

    /// `ConfigEntry.async_migrate` (2026.6.1): an entry from a newer major
    /// version than the flow's is not set up; a newer minor version is.
    /// The entries are what `scripts/vectors/import_releases.py` shows
    /// Home Assistant 2026.11.2 stores: `met` and `lifx` at 2.1, `hue` at
    /// 1.2.
    #[tokio::test]
    async fn entries_from_a_newer_version_are_not_started() {
        assert!(entry_too_new(&entry("met", 2, 1)));
        assert!(entry_too_new(&entry("lifx", 2, 1)));
        assert!(!entry_too_new(&entry("hue", 1, 2)));
        assert!(!entry_too_new(&entry("sun", 1, 1)));
        // Written before entries had versions.
        let mut old = entry("sun", 1, 1);
        old.remove("version");
        assert!(!entry_too_new(&old));

        let hass = Hass::new();
        let registries = Arc::new(Registries::default());
        let entries = [entry("met", 2, 1), entry("lifx", 2, 1)];
        let started = start(
            &hass,
            registries,
            Some(&entries),
            light::Profiles::default(),
        );
        assert!(started.running.is_empty(), "{:?}", started.running);
    }
}

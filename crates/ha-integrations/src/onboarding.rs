//! The config flows HA's onboarding starts when the core config step is
//! done (`components/onboarding/views.py`, `CoreConfigOnboardingView`):
//! `google_translate`, `met`, `radio_browser` and `shopping_list`, each with
//! source `onboarding`, so a fresh install gets a weather forecast for its
//! home.
//!
//! `hars` runs `met`'s (`components/met/config_flow.py`,
//! `async_step_onboarding`): an entry titled "Home" with
//! `{"track_home": true}`, unless no home is set. The other three
//! integrations don't exist in `hars`; HA runs each flow in its own task and
//! only logs a flow that fails, so leaving them out changes nothing else.

use crate::config_entries::ConfigEntries;
use crate::weather::Weather;
use crate::{USER_AGENT, met};
use ha_core::Hass;
use ha_registry::Registry;
use parking_lot::Mutex;
use serde_json::{Map, Value};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// `const.py`: met's onboarding default, which doesn't count as a home.
const DEFAULT_HOME: (f64, f64) = (52.3731339, 4.8903147);

/// What onboarding's flows need, and what they started.
pub struct Onboarding {
    hass: Hass,
    registry: Arc<Registry>,
    weather: Arc<Weather>,
    entries: Arc<ConfigEntries>,
    met_url: String,
    /// `weather.get_forecasts` is registered (at start-up, or here).
    weather_services: AtomicBool,
    /// Domains started here, for `get_config`'s `components`.
    started: Mutex<Vec<&'static str>>,
}

impl std::fmt::Debug for Onboarding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Onboarding").finish_non_exhaustive()
    }
}

impl Onboarding {
    /// `met_url` is met.no's forecast URL ([`met::URL`]; a local server in
    /// tests). `weather_services`: whether start-up already registered the
    /// weather services (a `met` entry was running).
    pub fn new(
        hass: &Hass,
        registry: Arc<Registry>,
        weather: Arc<Weather>,
        entries: Arc<ConfigEntries>,
        met_url: String,
        weather_services: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            hass: hass.clone(),
            registry,
            weather,
            entries,
            met_url,
            weather_services: AtomicBool::new(weather_services),
            started: Mutex::new(Vec::new()),
        })
    }

    /// The domains these flows started.
    pub fn started(&self) -> Vec<&'static str> {
        self.started.lock().clone()
    }

    /// The config entries, as they are now.
    pub fn entries(&self) -> &Arc<ConfigEntries> {
        &self.entries
    }

    /// `CoreConfigOnboardingView`'s flows, run once the step is done. They
    /// read the location `config/core/update` has just set.
    pub fn core_config_done(&self) {
        if let Err(reason) = self.met() {
            tracing::debug!("met's onboarding flow aborted: {reason}");
        }
    }

    /// `MetConfigFlowHandler.async_step_onboarding`, then the entry's
    /// setup.
    fn met(&self) -> Result<(), &'static str> {
        let config = self.hass.config();
        if (config.latitude == 0.0 && config.longitude == 0.0)
            || (config.latitude, config.longitude) == DEFAULT_HOME
        {
            return Err("no_home");
        }
        let mut data = Map::new();
        data.insert("track_home".into(), Value::Bool(true));
        let entry = self
            .entries
            .new_entry(met::DOMAIN, "onboarding", "Home", None, data);
        self.entries.add(entry.clone());
        let registries = self.registry.snapshot();
        if !self.weather_services.swap(true, Ordering::AcqRel) {
            self.weather
                .register_services(&self.hass, registries.clone());
        }
        tokio::spawn(met::run(
            self.hass.clone(),
            registries,
            self.weather.clone(),
            USER_AGENT,
            self.met_url.clone(),
            entry,
            self.hass.setup_guard(),
        ));
        let mut started = self.started.lock();
        for domain in [met::DOMAIN, "weather"] {
            if !started.contains(&domain) {
                started.push(domain);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ha_core::config::CoreConfig;
    use ha_registry::Registries;

    fn onboarding(hass: &Hass) -> Arc<Onboarding> {
        let registry = Registry::in_memory(hass, Registries::default());
        let entries = ConfigEntries::in_memory(hass, Vec::new());
        // Nothing listens there: the fetch fails, the entry stays.
        Onboarding::new(
            hass,
            registry,
            Arc::new(Weather::default()),
            entries,
            "http://127.0.0.1:9/".into(),
            false,
        )
    }

    /// Met's onboarding step: an entry tracking the home, unless no home is
    /// set (0, 0 or met's default).
    #[tokio::test(start_paused = true)]
    async fn met_tracks_the_home_once_one_is_set() {
        let hass = Hass::new();
        for (lat, lon) in [(0.0, 0.0), DEFAULT_HOME] {
            hass.set_config(CoreConfig {
                latitude: lat,
                longitude: lon,
                ..Default::default()
            });
            let o = onboarding(&hass);
            o.core_config_done();
            assert_eq!(o.entries().list(), Some(Vec::new()));
            assert!(o.started().is_empty());
        }
        hass.set_config(CoreConfig {
            latitude: 51.5,
            longitude: -0.12,
            ..Default::default()
        });
        let o = onboarding(&hass);
        o.core_config_done();
        let entries = o.entries().list().unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e["domain"], "met");
        assert_eq!(e["source"], "onboarding");
        assert_eq!(e["title"], "Home");
        assert_eq!(e["data"], serde_json::json!({"track_home": true}));
        assert_eq!(e["unique_id"], Value::Null);
        assert_eq!(o.started(), ["met", "weather"]);
        assert!(hass.services().has_service("weather", "get_forecasts"));
    }
}

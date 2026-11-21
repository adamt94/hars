//! Integrations: what turns devices and services into entities. Each one is
//! started for its config entries (HA's `core.config_entries`).

pub mod button;
pub mod climate;
pub mod components;
pub mod config_entries;
pub mod fan;
pub mod flow;
pub mod hue;
pub mod humidifier;
pub mod lifx;
pub mod light;
pub mod met;
pub mod mobile_app;
pub mod naming;
pub mod num;
pub mod number;
pub mod onboarding;
pub mod scene;
pub mod select;
pub(crate) mod sensor;
pub mod sun;
pub mod switch;
pub mod switchbot_cloud;
pub mod tuya;
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
    tuya::DOMAIN,
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
    " (+https://github.com/adamt94/hars)"
);

/// Where the integrations' services are: the real hosts, or stand-ins on
/// 127.0.0.1 in tests.
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// The SwitchBot API's base URL ([`switchbot_cloud::api::HOST`]).
    pub switchbot: String,
    /// Where Hue bridges announce themselves
    /// ([`hue::config_flow::DISCOVERY`]).
    pub hue_discovery: String,
    /// How long that service is given to answer (HA's 5 s).
    pub hue_discovery_timeout: std::time::Duration,
    /// `None`: a bridge is at its host (`http://<host>` and
    /// `https://<host>`). A base URL: every bridge is there instead, for
    /// tests and a debug build's browser run.
    pub hue_bridge: Option<String>,
    /// Where LIFX bulbs are reached, and where discovery broadcasts.
    pub lifx: lifx::Where,
    /// `None`: Tuya's API is at the address its entry names. A base URL:
    /// every Tuya entry talks to it instead (tests).
    pub tuya: Option<String>,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            switchbot: switchbot_cloud::api::HOST.to_owned(),
            hue_discovery: hue::config_flow::DISCOVERY.to_owned(),
            hue_discovery_timeout: std::time::Duration::from_secs(5),
            hue_bridge: None,
            lifx: lifx::Where::default(),
            tuya: None,
        }
    }
}

/// What [`start`] started.
pub struct Started {
    /// The domains running, for `get_config`'s `components`.
    pub running: Vec<&'static str>,
    /// The weather entities, for the forecast APIs.
    pub weather: Arc<weather::Weather>,
    /// The entity components, which entries added later feed too.
    pub components: Arc<components::Components>,
    /// The entries that can be stopped (`switchbot_cloud`'s and `hue`'s),
    /// for [`flow::Manager`].
    pub tasks: Vec<Running>,
}

/// An entry whose integration can be stopped.
pub struct Running {
    pub entry_id: String,
    pub task: tokio::task::JoinHandle<()>,
    /// HA's `entry.setup_lock`: the task holds it through its first setup
    /// attempt ([`config_entries::FirstAttempt`]), whoever started the
    /// entry and whatever became of them; a removal waits on it.
    pub setup: Arc<tokio::sync::Mutex<()>>,
}

impl Running {
    /// Start `entry`'s integration task with `status`, its first attempt
    /// under a new setup lock.
    pub(crate) fn start(
        entry_id: &str,
        status: config_entries::Status,
        spawn: impl FnOnce(config_entries::Status) -> tokio::task::JoinHandle<()>,
    ) -> Self {
        let (setup, first) = config_entries::FirstAttempt::start();
        Self {
            entry_id: entry_id.to_owned(),
            task: spawn(status.with_first(first)),
            setup,
        }
    }

    /// [`Running::start`], with `attempted` told where the first attempt
    /// left the entry.
    pub(crate) fn start_told(
        entry_id: &str,
        status: config_entries::Status,
        attempted: tokio::sync::oneshot::Sender<Option<config_entries::Runtime>>,
        spawn: impl FnOnce(config_entries::Status) -> tokio::task::JoinHandle<()>,
    ) -> Self {
        let (setup, first) = config_entries::FirstAttempt::start();
        Self {
            entry_id: entry_id.to_owned(),
            task: spawn(status.with_first(first.tell(attempted))),
            setup,
        }
    }
}

/// Start a `hue` entry: at start-up, or when a config flow has made it.
pub(crate) fn spawn_hue(
    components: &Arc<components::Components>,
    entry: Map<String, Value>,
    endpoints: &Endpoints,
    status: config_entries::Status,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(hue::run(
        components.clone(),
        entry,
        endpoints.hue_bridge.clone(),
        components.hass().setup_guard(),
        status,
    ))
}

/// Start a `lifx` entry: at start-up, or when a config flow has made it.
/// The components exist before the entry's setup.
pub(crate) fn spawn_lifx(
    components: &Arc<components::Components>,
    entry: Map<String, Value>,
    endpoints: &Endpoints,
    status: config_entries::Status,
) -> tokio::task::JoinHandle<()> {
    let _ = (components.lights(), components.buttons());
    tokio::spawn(lifx::run(
        components.clone(),
        entry,
        endpoints.lifx.clone(),
        components.hass().setup_guard(),
        status,
    ))
}

/// Start a `switchbot_cloud` entry: at start-up, or when a config flow
/// has made it. The components exist before the entry's setup, as HA sets
/// them up before the entries.
pub(crate) fn spawn_switchbot(
    components: &Arc<components::Components>,
    entry: Map<String, Value>,
    endpoints: &Endpoints,
    status: config_entries::Status,
) -> tokio::task::JoinHandle<()> {
    let _ = (
        components.buttons(),
        components.climates(),
        components.switches(),
    );
    tokio::spawn(switchbot_cloud::run(
        components.clone(),
        entry,
        endpoints.switchbot.clone(),
        components.hass().setup_guard(),
        status,
    ))
}

/// Start a `tuya` entry. Its components exist before its setup, so their
/// services do even while Tuya's cloud can't be reached.
pub(crate) fn spawn_tuya(
    components: &Arc<components::Components>,
    entry: Map<String, Value>,
    endpoints: &Endpoints,
    store: Option<Arc<config_entries::ConfigEntries>>,
    status: config_entries::Status,
) -> tokio::task::JoinHandle<()> {
    let _ = (
        components.switches(),
        components.humidifiers(),
        components.fans(),
        components.numbers(),
        components.selects(),
    );
    tokio::spawn(tuya::run(
        components.clone(),
        entry,
        store,
        endpoints.tuya.clone(),
        components.hass().setup_guard(),
        status,
    ))
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
///
/// `lookup` is the live registry in `hars` (entries set up later, and the
/// services' targets, see rows made after start-up); a fixed set in tests.
/// Not generic: a generic `start` is compiled into the binary crate, and
/// takes a second copy of the HTTP client with it (150 KiB).
pub fn start(
    hass: &Hass,
    lookup: ha_registry::Lookup,
    entries: Option<&[Map<String, Value>]>,
    profiles: light::Profiles,
    endpoints: &Endpoints,
    store: Option<&Arc<config_entries::ConfigEntries>>,
) -> Started {
    let registries: Arc<Registries> = lookup.get();
    let field =
        |e: &Map<String, Value>, k: &str| e.get(k).and_then(Value::as_str).map(str::to_owned);
    let enabled = |domain: &'static str| {
        entries
            .unwrap_or_default()
            .iter()
            .filter(move |e| field(e, "domain").as_deref() == Some(domain))
            // Not disabled, ignored or of a newer flow version.
            .filter(|e| config_entries::eligible(e))
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
    let components = components::Components::new(hass, lookup, profiles);
    let mut tasks = Vec::new();
    for entry in enabled(hue::DOMAIN).cloned().collect::<Vec<_>>() {
        let entry_id = field(&entry, "entry_id").unwrap_or_default();
        // Where its set-up is goes to the entries (a bridge that can't
        // be reached waits for a retry, one that refuses its key failed).
        let status = match store {
            Some(store) => config_entries::Status::new(store.clone(), &entry_id),
            None => config_entries::Status::none(),
        };
        tasks.push(Running::start(&entry_id, status, |status| {
            spawn_hue(&components, entry, endpoints, status)
        }));
        running.extend(hue::COMPONENTS);
    }
    let switchbot: Vec<Map<String, Value>> = enabled(switchbot_cloud::DOMAIN).cloned().collect();
    for entry in switchbot {
        let entry_id = field(&entry, "entry_id").unwrap_or_default();
        tasks.push(Running::start(
            &entry_id,
            config_entries::Status::none(),
            |status| spawn_switchbot(&components, entry, endpoints, status),
        ));
        running.extend(switchbot_cloud::COMPONENTS);
    }
    for entry in enabled(lifx::DOMAIN).cloned().collect::<Vec<_>>() {
        let entry_id = field(&entry, "entry_id").unwrap_or_default();
        // Where its set-up is goes to the entries (a bulb that can't be
        // reached waits for a retry).
        let status = match store {
            Some(store) => config_entries::Status::new(store.clone(), &entry_id),
            None => config_entries::Status::none(),
        };
        tasks.push(Running::start(&entry_id, status, |status| {
            spawn_lifx(&components, entry, endpoints, status)
        }));
        running.extend(lifx::COMPONENTS);
    }
    for entry in enabled(tuya::DOMAIN).cloned().collect::<Vec<_>>() {
        let entry_id = field(&entry, "entry_id").unwrap_or_default();
        let status = match store {
            Some(store) => config_entries::Status::new(store.clone(), &entry_id),
            None => config_entries::Status::none(),
        };
        tasks.push(Running::start(&entry_id, status, |status| {
            spawn_tuya(&components, entry, endpoints, store.cloned(), status)
        }));
        running.extend(tuya::COMPONENTS);
    }
    running.sort_unstable();
    running.dedup();
    Started {
        running,
        weather,
        components,
        tasks,
    }
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
            registries.into(),
            Some(&entries),
            light::Profiles::default(),
            &Endpoints::default(),
            None,
        );
        assert!(started.running.is_empty(), "{:?}", started.running);
    }
}

//! The `met` integration (`components/met`): a weather entity fed by the
//! Norwegian Meteorological Institute's forecast, fetched on HA's schedule.
//!
//! Where `hars` differs from HA 2026.6, on purpose:
//! - It asks the public `api.met.no` with its own User-Agent, not the
//!   hostname met.no gave Home Assistant with HA's.
//! - It sends `If-Modified-Since`, as met.no asks; on `304` the kept
//!   forecast is worked out again at the new time, which gives what HA
//!   gets from the same unchanged body.
//! - Legacy `*-hourly` registry entries are left alone (the registries are
//!   read-only here).

pub mod forecast;

use crate::sun::astral::Observer;
use crate::weather::{self, Forecasts, Units, Weather};
use forecast::{Computed, Forecast};
use ha_core::config::CoreConfig;
use ha_core::{EntityId, Hass};
use ha_registry::Registries;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde_json::{Map, Value};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

pub const DOMAIN: &str = "met";

/// `locationforecast/2.0/complete` on met.no's public host.
pub const URL: &str = "https://api.met.no/weatherapi/locationforecast/2.0/complete";

const ATTRIBUTION: &str =
    "Weather forecast from met.no, delivered by the Norwegian Meteorological Institute.";

/// HA's onboarding default location: a home still there isn't set.
const DEFAULT_HOME: (f64, f64) = (52.3731339, 4.8903147);

/// Where an entry forecasts for, and the entity's identity.
#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    pub latitude: f64,
    pub longitude: f64,
    pub elevation: f64,
    /// The weather entity's unique id: `home`, or `"{lat}-{lon}"`.
    pub unique_id: String,
    /// The entity's name (its device is "Forecast").
    pub name: String,
}

impl Location {
    /// From a config entry's `data`; `None` (HA logs and fails setup) for a
    /// home-tracking entry while no home is set.
    pub fn new(data: &Map<String, Value>, config: &CoreConfig) -> Option<Self> {
        if data.get("track_home").and_then(Value::as_bool) == Some(true) {
            let (lat, lon) = (config.latitude, config.longitude);
            if (lat == 0.0 && lon == 0.0) || (lat, lon) == DEFAULT_HOME {
                return None;
            }
            return Some(Self {
                latitude: lat,
                longitude: lon,
                elevation: config.elevation as f64,
                unique_id: "home".into(),
                name: config.location_name.clone(),
            });
        }
        let num = |k: &str| data.get(k).and_then(Value::as_f64);
        let (latitude, longitude) = (num("latitude")?, num("longitude")?);
        // Python's `f"{lat}-{lon}"` of what the entry holds.
        let repr = |k: &str| match data.get(k) {
            Some(Value::Number(n)) if n.is_f64() => {
                crate::sun::py_float(n.as_f64().unwrap_or_default())
            }
            Some(v) => v.to_string(),
            None => String::new(),
        };
        Some(Self {
            latitude,
            longitude,
            elevation: num("elevation").unwrap_or_default(),
            unique_id: format!("{}-{}", repr("latitude"), repr("longitude")),
            name: data
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("Met.no")
                .to_owned(),
        })
    }
}

/// The weather entity's id, friendly name and display-unit options, or
/// `None` if it's disabled.
fn entity(r: &Registries, loc: &Location) -> Option<Entity> {
    let entry = r.entities.iter().find(|e| {
        e.platform == DOMAIN
            && e.entity_id.starts_with("weather.")
            && e.unique_id.as_ref().and_then(Value::as_str) == Some(loc.unique_id.as_str())
    });
    if entry.is_some_and(|e| e.disabled_by.is_some()) {
        return None;
    }
    let device = entry
        .and_then(|e| e.device_id.as_deref())
        .and_then(|id| r.devices.iter().find(|d| d.id == id))
        .and_then(|d| d.display_name())
        .unwrap_or("Forecast");
    let default_id = || {
        format!(
            "weather.{}",
            ha_components::slugify(&format!("Forecast {}", loc.name))
        )
    };
    let entity_id = entry.map_or_else(default_id, |e| e.entity_id.clone());
    let name = match entry.and_then(|e| e.name.clone()) {
        Some(name) => name,
        None if loc.name.is_empty() => device.to_owned(),
        None => format!("{device} {}", loc.name),
    };
    Some(Entity {
        id: EntityId::parse(&entity_id).ok()?,
        name,
        options: entry.map(|e| e.options.object()),
    })
}

/// What one refresh leaves: the parsed body (kept for `304`s) and when it
/// was last modified.
struct Fetched {
    forecast: Forecast,
    last_modified: Option<String>,
}

#[derive(Debug, thiserror::Error)]
enum FetchError {
    #[error("{0}")]
    Http(#[from] ha_http::Error),
    #[error("HTTP {0}")]
    Status(u16),
    #[error("invalid forecast: {0}")]
    Parse(#[from] serde_json::Error),
}

/// [`fetch`] with a client made for it, so between fetches (an hour
/// apart) `hars` holds no TLS state at all.
async fn fetch_once(
    user_agent: &'static str,
    url: &str,
    prev: Option<Fetched>,
) -> Result<Fetched, (FetchError, Option<Fetched>)> {
    match ha_http::Client::new(user_agent) {
        Ok(client) => fetch(&client, url, prev).await,
        Err(e) => Err((e.into(), prev)),
    }
}

/// One `GET`. A status of 400 or more, a transport error or a body that
/// doesn't parse is a failure, as in pyMetno; `304` keeps `prev`.
async fn fetch(
    client: &ha_http::Client,
    url: &str,
    prev: Option<Fetched>,
) -> Result<Fetched, (FetchError, Option<Fetched>)> {
    let mut headers = Vec::new();
    if let Some(lm) = prev.as_ref().and_then(|p| p.last_modified.clone()) {
        headers.push((ha_http::header::IF_MODIFIED_SINCE, lm));
    }
    let res = match client.get(url, &headers).await {
        Ok(res) => res,
        Err(e) => return Err((e.into(), prev)),
    };
    if res.status == ha_http::StatusCode::NOT_MODIFIED {
        if let Some(prev) = prev {
            return Ok(prev);
        }
    }
    if res.status.as_u16() >= 400 || res.status == ha_http::StatusCode::NOT_MODIFIED {
        return Err((FetchError::Status(res.status.as_u16()), prev));
    }
    let forecast = match Forecast::parse(&res.body) {
        Ok(f) => f,
        Err(e) => return Err((e.into(), prev)),
    };
    let last_modified = res
        .headers
        .get(ha_http::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    Ok(Fetched {
        forecast,
        last_modified,
    })
}

/// A random number in `lo..hi`, from the standard library's per-process
/// random hashing keys (good enough for jitter).
fn random(lo: u64, hi: u64) -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let r = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    lo + r % (hi - lo).max(1)
}

/// The entity, as written after each refresh.
struct Entity {
    id: EntityId,
    name: String,
    options: Option<Map<String, Value>>,
}

impl Entity {
    fn units(&self, config: &CoreConfig) -> Units {
        Units::new(
            config.unit_system_v2 == "us_customary",
            self.options.as_ref(),
        )
    }

    /// Write the state (`unavailable` if the last refresh failed).
    fn write(&self, hass: &Hass, computed: &Computed, available: bool, now: Timestamp) {
        let config = hass.config();
        let units = self.units(&config);
        let current = computed.current.as_ref();
        let state = state(current, available, || sun_is_up(&config, now));
        let attrs = weather::attributes(
            current,
            &units,
            available,
            ATTRIBUTION,
            &self.name,
            weather::FORECAST_DAILY | weather::FORECAST_HOURLY,
        );
        hass.set_state(self.id.clone(), state, attrs);
    }

    fn forecasts(&self, hass: &Hass, computed: &Computed) -> Forecasts {
        let units = self.units(&hass.config());
        let list = |items: &[forecast::Item]| {
            Arc::new(Value::Array(
                items
                    .iter()
                    .map(|i| weather::forecast_item(i, &units))
                    .collect(),
            ))
        };
        Forecasts {
            daily: list(&computed.daily),
            hourly: list(&computed.hourly),
        }
    }
}

/// The entity's state: the current condition, except that the state alone
/// (not forecasts) turns `sunny` into `clear-night` while the sun is down.
fn state(
    current: Option<&forecast::Item>,
    available: bool,
    sun_up: impl FnOnce() -> bool,
) -> &'static str {
    if !available {
        return "unavailable";
    }
    match current.and_then(|c| c.condition) {
        Some("sunny") if !sun_up() => "clear-night",
        Some(c) => c,
        None => "unknown",
    }
}

/// HA's `sun.is_up`: the next sunrise comes after the next sunset.
fn sun_is_up(config: &CoreConfig, now: Timestamp) -> bool {
    let o = Observer {
        latitude: config.latitude,
        longitude: config.longitude,
        elevation: config.elevation as f64,
    };
    let tz = TimeZone::get(&config.time_zone).unwrap_or(TimeZone::UTC);
    crate::sun::is_up(&o, &tz, now)
}

/// HA's zone, and the zone pyMetno splits days in (the process's own).
fn zones(config: &CoreConfig) -> (TimeZone, TimeZone) {
    let tz = TimeZone::get(&config.time_zone).unwrap_or(TimeZone::UTC);
    (tz, TimeZone::system())
}

/// Run the integration for one config entry until the task is dropped.
/// Setup retries on HA's ladder (5 s doubling to 10 min); after that a
/// refresh every 55–64 minutes (drawn once) plus jitter, re-armed after
/// each one finishes. A failed refresh makes the entity unavailable until
/// the next success; forecast readers keep the last good forecast. An
/// entry with `pref_disable_polling` is fetched once, at setup, as HA.
pub async fn run(
    hass: Hass,
    registries: Arc<Registries>,
    weather: Arc<Weather>,
    user_agent: &'static str,
    base_url: String,
    entry: Map<String, Value>,
    ready: ha_core::SetupGuard,
) {
    let empty = Map::new();
    let data = entry
        .get("data")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let polling = entry.get("pref_disable_polling").and_then(Value::as_bool) != Some(true);
    let config = hass.config();
    let Some(loc) = Location::new(data, &config) else {
        tracing::warn!("Skip setting up met.no integration; No Home location has been set");
        return;
    };
    let Some(entity) = entity(&registries, &loc) else {
        return;
    };
    let url = format!(
        "{base_url}?{}",
        forecast::query(loc.latitude, loc.longitude, loc.elevation)
    );
    schedule(&hass, &weather, &entity, polling, ready, |prev| {
        fetch_once(user_agent, &url, prev)
    })
    .await;
}

/// HA's coordinator schedule around `fetch`, which is handed the last good
/// fetch (for `If-Modified-Since`) and gives it back on failure.
async fn schedule<F, Fut>(
    hass: &Hass,
    weather: &Weather,
    entity: &Entity,
    polling: bool,
    ready: ha_core::SetupGuard,
    mut fetch: F,
) where
    F: FnMut(Option<Fetched>) -> Fut,
    Fut: Future<Output = Result<Fetched, (FetchError, Option<Fetched>)>>,
{
    let jitter = || Duration::from_micros(random(50_000, 500_001));
    let clock = hass.clock().clone();
    let features = weather::FORECAST_DAILY | weather::FORECAST_HOURLY;

    // The first refresh: `ConfigEntryNotReady` until it works. Setup is
    // done after the first try either way (HA retries in the background).
    let mut tries = 0u32;
    let mut ready = Some(ready);
    let mut last = loop {
        match fetch(None).await {
            Ok(f) => break f,
            Err((e, _)) => {
                if let Some(r) = ready.take() {
                    r.done();
                }
                let wait = (5u64 << tries.min(7)).min(600);
                tracing::warn!("met.no: {e}; retrying setup in {wait} s");
                tries += 1;
                hass.setup_retry_wait(Duration::from_secs(wait) + jitter())
                    .await;
            }
        }
    };
    let every = Duration::from_secs(60 * random(55, 65)) + jitter();
    let mut available = true;
    loop {
        let now = clock.now();
        let (tz, bucket) = zones(&hass.config());
        let computed = last.forecast.compute(now, &tz, &bucket);
        entity.write(hass, &computed, true, now);
        weather.publish(
            &entity.id,
            features,
            true,
            entity.forecasts(hass, &computed),
        );
        if let Some(r) = ready.take() {
            r.done();
        }
        if !polling {
            return std::future::pending().await;
        }
        if !available {
            tracing::info!("Fetching met data recovered");
        }
        available = true;
        let mut kept = Some(last);
        // Refresh until one works; only the first failure is announced.
        last = loop {
            tokio::time::sleep(every).await;
            match fetch(kept.take()).await {
                Ok(f) => break f,
                Err((e, prev)) => {
                    kept = prev;
                    if available {
                        tracing::error!("Error fetching met data: {e}");
                        available = false;
                        entity.write(hass, &computed, false, clock.now());
                        // Subscribers hear of it, with the old forecast.
                        let forecasts = entity.forecasts(hass, &computed);
                        weather.publish(&entity.id, features, false, forecasts);
                    }
                }
            }
        };
    }
}

#[cfg(test)]
mod tests;

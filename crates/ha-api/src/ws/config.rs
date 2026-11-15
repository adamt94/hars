//! `config/core/update` and `config/core/detect`
//! (`components/config/core.py`), both admin-only: the location, units and
//! locale settings, as onboarding's core config step and the general
//! settings page change them.
//!
//! An update is validated with HA's schema and messages (every error, one
//! per line), applied (`CoreConfig._async_update`), written to
//! `.storage/core.config` and announced with `core_config_updated`, whose
//! data is what was sent, coerced (`unit_system: "imperial"` arrives as
//! `"us_customary"`). What follows the home (`zone.home`, `sun`, a `met`
//! entry that tracks the home) wakes on [`ha_core::Hass::watch_config`];
//! everything else reads the settings when it next needs them (time
//! triggers and the recorder's nightly purge at their next scheduling, as
//! in HA). Where `hars` differs:
//! - `update_units` is accepted and does nothing: HA re-suggests sensor
//!   units in the entity registry, which `hars` doesn't keep.
//! - An unknown time zone changes nothing. HA has by then applied the
//!   other fields in memory (not saved, no event) before refusing it.
//! - The repairs HA raises for a historic currency or a missing country
//!   are not raised (`hars` has no issue registry).

use super::{Conn, Invalid, only_keys};
use crate::onboarding_data::{COUNTRIES, CURRENCIES, LANGUAGES};
use ha_auth::Permission;
use ha_core::config::{ConfigSource, CoreConfig};
use ha_core::{Context, Event, EventKind};
use serde_json::{Map, Value, json};
use std::time::Duration;

/// `util/location.py::WHOAMI_URL`: HA's location service.
pub const WHOAMI_URL: &str = "https://services.home-assistant.io/whoami/v1";
/// `aiohttp.ClientTimeout(total=30)`.
const WHOAMI_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn dispatch(
    conn: &mut Conn,
    id: u64,
    ty: &str,
    msg: &Map<String, Value>,
) -> Option<Result<(), Invalid>> {
    Some(match ty {
        "config/core/update" => update(conn, id, msg),
        "config/core/detect" => only_keys(msg, &[]).map(|()| detect(conn, id)),
        _ => return None,
    })
}

/// `@require_admin`, checked after the schema as in HA.
fn admin(conn: &Conn, id: u64) -> bool {
    let ok = conn.principal.permission >= Permission::Admin;
    if !ok {
        conn.out.error(json!(id), "unauthorized", "Unauthorized");
    }
    ok
}

// ----- config/core/update -----

/// What an update sets, as HA's schema leaves it.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Update {
    pub country: Option<String>,
    pub currency: Option<String>,
    pub elevation: Option<i64>,
    pub external_url: Option<Option<String>>,
    pub internal_url: Option<Option<String>>,
    pub language: Option<String>,
    pub latitude: Option<f64>,
    pub location_name: Option<String>,
    pub longitude: Option<f64>,
    pub radius: Option<i64>,
    pub time_zone: Option<String>,
    pub unit_system: Option<&'static str>,
    pub update_units: bool,
    /// The event's data: each field sent (but `update_units`), coerced.
    pub event: Map<String, Value>,
}

/// The `websocket_update_config` schema. Every error is reported, one per
/// line, as voluptuous's `MultipleInvalid` and `humanize_error` do.
pub(crate) fn validate(msg: &Map<String, Value>) -> Result<Update, Invalid> {
    let mut u = Update::default();
    let mut errors = Vec::new();
    for (key, value) in msg {
        let mut bad =
            |why: &str| errors.push(format!("{why} for dictionary value @ data['{key}']"));
        let coerced: Value = match key.as_str() {
            "id" | "type" => continue,
            "country" => match one_of(value, COUNTRIES) {
                Some(c) => {
                    u.country = Some(c.to_owned());
                    json!(c)
                }
                None => {
                    bad("invalid ISO 3166 formatted country");
                    continue;
                }
            },
            "currency" => match one_of(value, CURRENCIES) {
                Some(c) => {
                    u.currency = Some(c.to_owned());
                    json!(c)
                }
                None => {
                    bad("invalid ISO 4217 formatted currency");
                    continue;
                }
            },
            "language" => match one_of(value, LANGUAGES) {
                Some(c) => {
                    u.language = Some(c.to_owned());
                    json!(c)
                }
                None => {
                    bad("invalid RFC 5646 formatted language");
                    continue;
                }
            },
            // `int`: Python's bools are ints too.
            "elevation" => match value {
                Value::Number(n) if n.is_i64() => {
                    u.elevation = n.as_i64();
                    value.clone()
                }
                Value::Bool(b) => {
                    u.elevation = Some(i64::from(*b));
                    value.clone()
                }
                _ => {
                    bad("expected int");
                    continue;
                }
            },
            "latitude" | "longitude" => {
                let limit = if key == "latitude" { 90.0 } else { 180.0 };
                match py_float(value).filter(|f| (-limit..=limit).contains(f)) {
                    Some(f) => {
                        if key == "latitude" {
                            u.latitude = Some(f);
                        } else {
                            u.longitude = Some(f);
                        }
                        json!(f)
                    }
                    None => {
                        bad(&format!("invalid {key}"));
                        continue;
                    }
                }
            }
            "radius" => match py_int(value) {
                Some(r) if r >= 0 => {
                    u.radius = Some(r);
                    json!(r)
                }
                Some(_) => {
                    bad("value must be at least 0");
                    continue;
                }
                None => {
                    bad("expected int");
                    continue;
                }
            },
            "location_name" | "time_zone" => match value {
                Value::String(s) => {
                    if key == "location_name" {
                        u.location_name = Some(s.clone());
                    } else {
                        u.time_zone = Some(s.clone());
                    }
                    value.clone()
                }
                _ => {
                    bad("expected str");
                    continue;
                }
            },
            "update_units" => match value {
                Value::Bool(b) => {
                    u.update_units = *b;
                    continue;
                }
                _ => {
                    bad("expected bool");
                    continue;
                }
            },
            "unit_system" => match unit_system(value) {
                Some(system) => {
                    u.unit_system = Some(system);
                    json!(system)
                }
                None => {
                    bad("not a valid value");
                    continue;
                }
            },
            "external_url" | "internal_url" => match url_no_path(value) {
                Ok(url) => {
                    let url = url.map(str::to_owned);
                    if key == "external_url" {
                        u.external_url = Some(url.clone());
                    } else {
                        u.internal_url = Some(url.clone());
                    }
                    json!(url)
                }
                Err(why) => {
                    bad(why);
                    continue;
                }
            },
            _ => {
                errors.push(format!("extra keys not allowed @ data['{key}']"));
                continue;
            }
        };
        u.event.insert(key.clone(), coerced);
    }
    if errors.is_empty() {
        Ok(u)
    } else {
        Err(errors.join("\n"))
    }
}

/// `vol.In(container)`: the value, if it is one of them.
fn one_of(value: &Value, container: &[&'static str]) -> Option<&'static str> {
    let s = value.as_str()?;
    container.binary_search(&s).ok().map(|i| container[i])
}

/// `vol.Coerce(float)`: Python's `float()` of a number, bool or numeric
/// string; `nan` and the infinities fail the range check after.
fn py_float(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|f: &f64| !f.is_nan())
}

/// `vol.Coerce(int)`: Python's `int()`: floats truncate, strings must be
/// whole numbers.
fn py_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite())
                .map(|f| f.trunc() as i64)
        }),
        Value::Bool(b) => Some(i64::from(*b)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `validate_unit_system`: `str(value).lower()`, `imperial` meaning
/// `us_customary`.
fn unit_system(value: &Value) -> Option<&'static str> {
    let s = match value {
        Value::String(s) => s.to_lowercase(),
        other => crate::cv::string(other).ok()?.to_lowercase(),
    };
    match s.as_str() {
        "metric" => Some("metric"),
        "us_customary" | "imperial" => Some("us_customary"),
        _ => None,
    }
}

/// `vol.Any(cv.url_no_path, None)`: `None`, or an `http`/`https` URL with a
/// host, a valid port and no path but `/`. Other values are `str()`-ed
/// first, so they fail as URLs.
fn url_no_path(value: &Value) -> Result<Option<&str>, &'static str> {
    let s = match value {
        Value::Null => return Ok(None),
        Value::String(s) => s.as_str(),
        _ => return Err("invalid url"),
    };
    let parts = urlsplit(s);
    if !matches!(parts.scheme.as_deref(), Some("http" | "https")) {
        return Err("invalid url");
    }
    if !parts.port_ok {
        return Err("invalid url");
    }
    // `vol.Url()`.
    if parts.netloc.is_empty() {
        return Err("expected a URL");
    }
    if !matches!(parts.path, "" | "/") {
        return Err("url is not allowed to have a path component");
    }
    Ok(Some(s))
}

/// The parts of Python's `urllib.parse.urlsplit` these checks use.
struct Split<'a> {
    scheme: Option<String>,
    netloc: &'a str,
    path: &'a str,
    /// `.port` doesn't raise.
    port_ok: bool,
}

fn urlsplit(url: &str) -> Split<'_> {
    let mut rest = url;
    let mut scheme = None;
    if let Some(i) = url.find(':') {
        let candidate = &url[..i];
        let valid = candidate
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
            && candidate
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        if valid {
            scheme = Some(candidate.to_ascii_lowercase());
            rest = &url[i + 1..];
        }
    }
    let mut netloc = "";
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = &after[..end];
        rest = &after[end..];
    }
    let path_end = rest.find(['?', '#']).unwrap_or(rest.len());
    let path = &rest[..path_end];
    // `_hostinfo` then `.port`: the digits after the host's last `:`.
    let host = netloc.rsplit_once('@').map_or(netloc, |(_, h)| h);
    let port = match host.find(']') {
        Some(close) => host[close..].split_once(':').map(|(_, p)| p),
        None => host.rsplit_once(':').map(|(_, p)| p),
    };
    let port_ok = match port {
        None | Some("") => true,
        Some(p) => {
            p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u32>().is_ok_and(|n| n <= 65535)
        }
    };
    Split {
        scheme,
        netloc,
        path,
        port_ok,
    }
}

/// `CoreConfig._async_update`: the fields that were sent.
fn apply(c: &mut CoreConfig, u: &Update) {
    c.source = ConfigSource::Storage;
    if let Some(v) = &u.country {
        c.country = Some(v.clone());
    }
    if let Some(v) = &u.currency {
        c.currency = v.clone();
    }
    if let Some(v) = u.elevation {
        c.elevation = v;
    }
    if let Some(v) = &u.external_url {
        c.external_url = v.clone();
    }
    if let Some(v) = &u.internal_url {
        c.internal_url = v.clone();
    }
    if let Some(v) = &u.language {
        c.language = v.clone();
    }
    if let Some(v) = u.latitude {
        c.latitude = v;
    }
    if let Some(v) = &u.location_name {
        c.location_name = v.clone();
    }
    if let Some(v) = u.longitude {
        c.longitude = v;
    }
    if let Some(v) = u.radius {
        c.radius = v;
    }
    if let Some(v) = &u.time_zone {
        c.time_zone = v.clone();
    }
    if let Some(v) = u.unit_system {
        c.unit_system_v2 = v.into();
    }
}

fn update(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    let u = validate(msg)?;
    if !admin(conn, id) {
        return Ok(());
    }
    let (shared, out) = (conn.shared.clone(), conn.out.clone());
    tokio::spawn(async move {
        // One update at a time: each reads what the last one wrote.
        let _turn = shared.core_config.lock().await;
        if let Some(tz) = &u.time_zone {
            if !valid_time_zone(tz) {
                out.error(
                    json!(id),
                    "invalid_info",
                    &format!("Received invalid time zone {tz}"),
                );
                return;
            }
        }
        let hass = &shared.hass;
        let mut config = (*hass.config()).clone();
        apply(&mut config, &u);
        hass.set_config(config.clone());
        if let Some(dir) = shared.config.storage.clone() {
            let saved = tokio::task::spawn_blocking(move || config.save(&dir)).await;
            if let Err(e) = saved.map_err(std::io::Error::other).and_then(|r| r) {
                tracing::error!("saving core.config: {e}");
                out.error(json!(id), "unknown_error", "Unknown error");
                return;
            }
        }
        let now = hass.clock().now();
        hass.bus().fire(Event::new(
            EventKind::Custom {
                name: "core_config_updated".into(),
                data: Value::Object(u.event),
            },
            now,
            Context::at(now),
        ));
        out.result(id, Value::Null);
    });
    Ok(())
}

/// `dt_util.async_get_time_zone`: a name in the time zone database, as
/// written there (HA's `zoneinfo` on Linux is case-sensitive; jiff's lookup
/// isn't).
fn valid_time_zone(name: &str) -> bool {
    jiff::tz::TimeZone::get(name).is_ok_and(|tz| tz.iana_name() == Some(name))
}

// ----- config/core/detect -----

/// `websocket_detect_config`: ask HA's location service where this
/// address is, as HA does (the frontend asks first, saying it will make a
/// one-time request to an external service). Any failure to reach it or
/// read its JSON is `{}`, as in HA; a reply without usable coordinates is
/// HA's `unknown_error`.
fn detect(conn: &mut Conn, id: u64) {
    if !admin(conn, id) {
        return;
    }
    let url = conn
        .shared
        .config
        .whoami_url
        .clone()
        .unwrap_or_else(|| WHOAMI_URL.to_owned());
    let out = conn.out.clone();
    tokio::spawn(async move {
        match detected(&url).await {
            Ok(info) => out.result(id, info),
            Err(()) => out.error(json!(id), "unknown_error", "Unknown error"),
        };
    });
}

/// `async_detect_location_info` and the answer built from it. `Err` where
/// HA's handler raises.
async fn detected(url: &str) -> Result<Value, ()> {
    let Some(raw) = whoami(url).await else {
        return Ok(json!({}));
    };
    let Value::Object(raw) = raw else {
        return Err(()); // `raw_info.get` on a list
    };
    // `float(raw_info.get(...))`: both must be there and numeric.
    let float = |key: &str| match raw.get(key) {
        Some(Value::String(s)) => s.trim().parse::<f64>().map_err(|_| ()),
        Some(v) => py_float(v).ok_or(()),
        None => Err(()),
    };
    let (latitude, longitude) = (float("latitude")?, float("longitude")?);
    let country = raw.get("country").cloned().unwrap_or(Value::Null);
    let use_metric = !matches!(country.as_str(), Some("US" | "MM" | "LR"));
    let mut info = Map::new();
    info.insert(
        "unit_system".into(),
        json!(if use_metric { "metric" } else { "us_customary" }),
    );
    if latitude != 0.0 {
        info.insert("latitude".into(), json!(latitude));
    }
    if longitude != 0.0 {
        info.insert("longitude".into(), json!(longitude));
    }
    for (from, to) in [
        ("timezone", "time_zone"),
        ("currency", "currency"),
        ("country", "country"),
    ] {
        if let Some(v) = raw.get(from).filter(|v| truthy(v)) {
            info.insert(to.into(), v.clone());
        }
    }
    Ok(Value::Object(info))
}

/// Python's truthiness of a JSON value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `_get_whoami`'s request: `None` when it can't be reached in 30 s or its
/// body isn't JSON served as JSON (aiohttp's `resp.json()`); the status
/// isn't checked.
async fn whoami(url: &str) -> Option<Value> {
    let client = ha_http::Client::new(ha_integrations::USER_AGENT)
        .ok()?
        .with_timeouts(WHOAMI_TIMEOUT, WHOAMI_TIMEOUT);
    let res = client.get(url, &[]).await.ok()?;
    let mime = res
        .headers
        .get(ha_http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    // aiohttp's `^application/(?:[\w.+-]+?\+)?json`.
    let sub = mime.strip_prefix("application/")?;
    let json_type = sub == "json"
        || sub.strip_suffix("+json").is_some_and(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '+' | '-'))
        });
    if !json_type {
        return None;
    }
    serde_json::from_slice(&res.body).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invalid(msg: Value) -> String {
        validate(msg.as_object().unwrap()).unwrap_err()
    }

    /// The generated tables are searched by bisection.
    #[test]
    fn generated_tables_are_sorted() {
        for table in [COUNTRIES, CURRENCIES, LANGUAGES] {
            assert!(table.windows(2).all(|w| w[0] < w[1]));
        }
        let names = crate::onboarding_data::AREA_NAMES;
        assert!(names.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(COUNTRIES.contains(&"GB") && CURRENCIES.contains(&"GBP"));
        assert!(LANGUAGES.contains(&"en-GB") && LANGUAGES.contains(&"zh-Hans"));
    }

    /// Each from voluptuous 0.15.2 with HA 2026.6.1's validators (the
    /// messages before `humanize_error` adds the value).
    #[test]
    fn validators_match_has() {
        let ok = |msg: Value| validate(msg.as_object().unwrap()).unwrap();
        assert_eq!(ok(json!({"radius": 12.7})).radius, Some(12));
        assert_eq!(ok(json!({"radius": -0.5})).radius, Some(0));
        assert_eq!(ok(json!({"radius": true})).radius, Some(1));
        assert_eq!(ok(json!({"radius": " 7 "})).radius, Some(7));
        for bad in [json!(null), json!("1.5"), json!([1])] {
            assert_eq!(
                invalid(json!({"radius": bad})),
                "expected int for dictionary value @ data['radius']"
            );
        }
        assert_eq!(
            invalid(json!({"external_url": "http://"})),
            "expected a URL for dictionary value @ data['external_url']"
        );
        assert_eq!(
            invalid(json!({"external_url": "http://x:99999"})),
            "invalid url for dictionary value @ data['external_url']"
        );
        assert_eq!(
            invalid(json!({"external_url": 5})),
            "invalid url for dictionary value @ data['external_url']"
        );
        for good in [
            "http://exa mple.com",
            "HTTP://Example.com",
            "https://x.y:8123",
            "http://x/?a=1",
            "http://x#frag",
        ] {
            assert_eq!(
                ok(json!({"external_url": good})).external_url,
                Some(Some(good.into()))
            );
        }
        assert_eq!(ok(json!({"latitude": true})).latitude, Some(1.0));
        assert_eq!(ok(json!({"latitude": " 12.5 "})).latitude, Some(12.5));
        for bad in [json!(null), json!("nan"), json!([1])] {
            assert_eq!(
                invalid(json!({"latitude": bad})),
                "invalid latitude for dictionary value @ data['latitude']"
            );
        }
        assert_eq!(
            invalid(json!({"unit_system": null})),
            "not a valid value for dictionary value @ data['unit_system']"
        );
        assert_eq!(
            ok(json!({"unit_system": "METRIC"})).unit_system,
            Some("metric")
        );
        assert_eq!(
            ok(json!({"unit_system": "imperial"})).unit_system,
            Some("us_customary")
        );
        for bad in [json!(null), json!(["GB"])] {
            assert_eq!(
                invalid(json!({"country": bad})),
                "invalid ISO 3166 formatted country for dictionary value @ data['country']"
            );
        }
        assert_eq!(
            invalid(json!({"location_name": null})),
            "expected str for dictionary value @ data['location_name']"
        );
        assert!(valid_time_zone("Europe/London"));
        assert!(valid_time_zone("UTC"));
        assert!(valid_time_zone("US/Eastern"));
        assert!(!valid_time_zone("europe/london"));
        assert!(!valid_time_zone("Mars/Phobos"));
        assert!(!valid_time_zone(""));
        // Every error, one per line.
        assert_eq!(
            invalid(json!({"latitude": 91, "country": "XX"})),
            "invalid ISO 3166 formatted country for dictionary value @ data['country']\n\
             invalid latitude for dictionary value @ data['latitude']"
        );
    }
}

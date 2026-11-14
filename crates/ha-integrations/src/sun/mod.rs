//! The `sun` integration (`components/sun`): `sun.sun`, its event sensors
//! and the solar position, recomputed on HA's schedule.

pub mod astral;

use astral::Observer;
use ha_core::{EntityId, Hass};
use ha_registry::Registries;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde_json::{Map, Value, json};

pub const DOMAIN: &str = "sun";

/// HA's phases and how often the position is refreshed in each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Night,
    AstronomicalTwilight,
    NauticalTwilight,
    Twilight,
    SmallDay,
    Day,
}

impl Phase {
    /// `_PHASE_UPDATES`.
    fn update_every(self) -> SignedDuration {
        SignedDuration::from_mins(match self {
            Self::Night => 20,
            Self::AstronomicalTwilight => 8,
            Self::NauticalTwilight => 4,
            Self::Twilight | Self::SmallDay => 2,
            Self::Day => 4,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Night => "night",
            Self::AstronomicalTwilight => "astronomical_twilight",
            Self::NauticalTwilight => "nautical_twilight",
            Self::Twilight => "twilight",
            Self::SmallDay => "small_day",
            Self::Day => "day",
        }
    }

    fn from_elevation(e: f64) -> Self {
        if e >= 10.0 {
            Self::Day
        } else if e >= 0.0 {
            Self::SmallDay
        } else if e >= -6.0 {
            Self::Twilight
        } else if e >= -12.0 {
            Self::NauticalTwilight
        } else if e >= -18.0 {
            Self::AstronomicalTwilight
        } else {
            Self::Night
        }
    }
}

/// Everything `Sun.update_events` works out at one instant.
#[derive(Debug, Clone, PartialEq)]
pub struct Events {
    pub next_dawn: Timestamp,
    pub next_dusk: Timestamp,
    pub next_midnight: Timestamp,
    pub next_noon: Timestamp,
    pub next_rising: Timestamp,
    pub next_setting: Timestamp,
    pub rising: bool,
    pub phase: Phase,
    /// When the phase changes and everything is worked out again.
    pub next_change: Timestamp,
}

/// `get_location_astral_event_next`: the first event after `now`, trying
/// local dates from yesterday on, as HA does.
fn next(
    now: Timestamp,
    tz: &TimeZone,
    event: impl Fn(jiff::civil::Date) -> Option<Timestamp>,
) -> Timestamp {
    let today = now.to_zoned(tz.clone()).date();
    for days in -1..367 {
        let Ok(date) = today.checked_add(jiff::ToSpan::days(days)) else {
            continue;
        };
        if let Some(t) = event(date).filter(|t| *t > now) {
            return t;
        }
    }
    // HA raises here; the sun always rises or sets within a year somewhere.
    now + SignedDuration::from_hours(400 * 24)
}

/// `Sun.update_events` at `now`.
pub fn events(o: &Observer, tz: &TimeZone, now: Timestamp) -> Events {
    let mut next_change = now + SignedDuration::from_hours(400 * 24);
    let mut phase = None;
    let mut check = |t: Timestamp, before: Option<Phase>| {
        if t < next_change {
            next_change = t;
            phase = before;
        }
        t
    };
    let at_elevation = Observer {
        elevation: o.elevation,
        ..*o
    };
    let flat = Observer {
        elevation: 0.0,
        ..*o
    };
    use Phase::*;
    check(
        next(now, tz, |d| astral::dawn(&at_elevation, d, 18.0)),
        Some(Night),
    );
    check(
        next(now, tz, |d| astral::dawn(&at_elevation, d, 12.0)),
        Some(AstronomicalTwilight),
    );
    let next_dawn = check(
        next(now, tz, |d| astral::dawn(&at_elevation, d, 6.0)),
        Some(NauticalTwilight),
    );
    let next_rising = check(
        next(now, tz, |d| astral::sunrise(&at_elevation, d)),
        Some(Twilight),
    );
    check(
        next(now, tz, |d| astral::dawn(&at_elevation, d, -10.0)),
        Some(SmallDay),
    );
    let next_noon = check(next(now, tz, |d| Some(astral::noon(&flat, d))), None);
    check(
        next(now, tz, |d| astral::dusk(&at_elevation, d, -10.0)),
        Some(Day),
    );
    let next_setting = check(
        next(now, tz, |d| astral::sunset(&at_elevation, d)),
        Some(SmallDay),
    );
    let next_dusk = check(
        next(now, tz, |d| astral::dusk(&at_elevation, d, 6.0)),
        Some(Twilight),
    );
    check(
        next(now, tz, |d| astral::dusk(&at_elevation, d, 12.0)),
        Some(NauticalTwilight),
    );
    check(
        next(now, tz, |d| astral::dusk(&at_elevation, d, 18.0)),
        Some(AstronomicalTwilight),
    );
    let next_midnight = check(next(now, tz, |d| Some(astral::midnight(&flat, d))), None);
    let phase = phase.unwrap_or_else(|| Phase::from_elevation(astral::elevation(o, next_change)));
    Events {
        next_dawn,
        next_dusk,
        next_midnight,
        next_noon,
        next_rising,
        next_setting,
        rising: next_noon < next_midnight,
        phase,
        next_change,
    }
}

/// HA's `helpers.sun.is_up`: the next sunrise comes after the next sunset.
pub fn is_up(o: &Observer, tz: &TimeZone, now: Timestamp) -> bool {
    next(now, tz, |d| astral::sunrise(o, d)) > next(now, tz, |d| astral::sunset(o, d))
}

/// `round(x, 2)`.
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// The solar elevation and azimuth at `now`, rounded as HA rounds them.
pub fn position(o: &Observer, now: Timestamp) -> (f64, f64) {
    (
        round2(astral::elevation(o, now)),
        round2(astral::azimuth(o, now)),
    )
}

/// Python's `datetime.isoformat()` of a UTC time: microseconds only when
/// there are any.
pub fn iso(t: Timestamp) -> String {
    ha_core::isoformat(t)
}

/// A timestamp sensor's state: `isoformat(timespec="seconds")` in UTC.
pub fn iso_seconds(t: Timestamp) -> String {
    t.strftime("%Y-%m-%dT%H:%M:%S+00:00").to_string()
}

/// Python's `str(float)`: `60.0`, not `60`.
pub fn py_float(x: f64) -> String {
    if x.is_finite() && x.fract() == 0.0 && x.abs() < 1e16 {
        format!("{x:.1}")
    } else {
        x.to_string()
    }
}

/// One sensor: its key (`<entry_id>-<key>` is its unique id), default
/// entity id, name, and whether HA enables it by default.
struct SensorDesc {
    key: &'static str,
    domain: &'static str,
    name: &'static str,
    enabled_default: bool,
}

const SENSORS: &[SensorDesc] = &[
    SensorDesc {
        key: "next_dawn",
        domain: "sensor",
        name: "Next dawn",
        enabled_default: true,
    },
    SensorDesc {
        key: "next_dusk",
        domain: "sensor",
        name: "Next dusk",
        enabled_default: true,
    },
    SensorDesc {
        key: "next_midnight",
        domain: "sensor",
        name: "Next midnight",
        enabled_default: true,
    },
    SensorDesc {
        key: "next_noon",
        domain: "sensor",
        name: "Next noon",
        enabled_default: true,
    },
    SensorDesc {
        key: "next_rising",
        domain: "sensor",
        name: "Next rising",
        enabled_default: true,
    },
    SensorDesc {
        key: "next_setting",
        domain: "sensor",
        name: "Next setting",
        enabled_default: true,
    },
    SensorDesc {
        key: "solar_elevation",
        domain: "sensor",
        name: "Solar elevation",
        enabled_default: false,
    },
    SensorDesc {
        key: "solar_azimuth",
        domain: "sensor",
        name: "Solar azimuth",
        enabled_default: false,
    },
    SensorDesc {
        key: "solar_rising",
        domain: "binary_sensor",
        name: "Solar rising",
        enabled_default: false,
    },
];

/// A sensor's entity id and friendly name, or `None` if it's disabled.
fn sensor_entity(r: &Registries, entry_id: &str, s: &SensorDesc) -> Option<(EntityId, String)> {
    let unique_id = format!("{entry_id}-{}", s.key);
    let entry = r.entities.iter().find(|e| {
        e.platform == DOMAIN
            && e.unique_id.as_ref().and_then(Value::as_str) == Some(unique_id.as_str())
    });
    let enabled = match entry {
        Some(e) => e.disabled_by.is_none(),
        None => s.enabled_default,
    };
    if !enabled {
        return None;
    }
    let entity_id = entry.map_or_else(
        || format!("{}.sun_{}", s.domain, s.key),
        |e| e.entity_id.clone(),
    );
    // `has_entity_name`: the device's name, then the entity's.
    let device = entry
        .and_then(|e| e.device_id.as_deref())
        .and_then(|id| r.devices.iter().find(|d| d.id == id))
        .and_then(|d| d.display_name())
        .unwrap_or("Sun");
    // A name set in the registry is used as is (`use_legacy_naming`).
    if let Some(name) = entry.and_then(|e| e.name.clone()) {
        return Some((EntityId::parse(&entity_id).ok()?, name));
    }
    let name = s.name;
    Some((
        EntityId::parse(&entity_id).ok()?,
        format!("{device} {name}"),
    ))
}

/// The running integration's entities.
struct Entities {
    sensors: Vec<(&'static SensorDesc, EntityId, String)>,
}

impl Entities {
    fn new(r: &Registries, entry_id: &str) -> Self {
        Self {
            sensors: SENSORS
                .iter()
                .filter_map(|s| sensor_entity(r, entry_id, s).map(|(id, name)| (s, id, name)))
                .collect(),
        }
    }

    /// Write `sun.sun` and the sensors. `events_changed` also writes the
    /// event sensors (HA's `SIGNAL_EVENTS_CHANGED` vs `..._POSITION_...`).
    fn write(
        &self,
        hass: &Hass,
        ev: &Events,
        (elevation, azimuth): (f64, f64),
        events_changed: bool,
    ) {
        let attrs = json!({
            "next_dawn": iso(ev.next_dawn),
            "next_dusk": iso(ev.next_dusk),
            "next_midnight": iso(ev.next_midnight),
            "next_noon": iso(ev.next_noon),
            "next_rising": iso(ev.next_rising),
            "next_setting": iso(ev.next_setting),
            "elevation": elevation,
            "azimuth": azimuth,
            "rising": ev.rising,
            "friendly_name": "Sun",
        });
        let state = if elevation > -0.833 {
            "above_horizon"
        } else {
            "below_horizon"
        };
        hass.set_state(
            EntityId::parse("sun.sun").expect("valid"),
            state,
            object(attrs),
        );

        for (desc, id, name) in &self.sensors {
            let (state, attrs) = match desc.key {
                "solar_elevation" => (py_float(elevation), position_attrs(name)),
                "solar_azimuth" => (py_float(azimuth), position_attrs(name)),
                _ if !events_changed => continue,
                "solar_rising" => (
                    if ev.rising { "on" } else { "off" }.to_owned(),
                    json!({ "friendly_name": name }),
                ),
                key => {
                    let t = match key {
                        "next_dawn" => ev.next_dawn,
                        "next_dusk" => ev.next_dusk,
                        "next_midnight" => ev.next_midnight,
                        "next_noon" => ev.next_noon,
                        "next_rising" => ev.next_rising,
                        _ => ev.next_setting,
                    };
                    (
                        iso_seconds(t),
                        json!({ "device_class": "timestamp", "friendly_name": name }),
                    )
                }
            };
            hass.set_state(id.clone(), state.as_str(), object(attrs));
        }
    }
}

fn position_attrs(name: &str) -> Value {
    json!({ "state_class": "measurement", "unit_of_measurement": "°", "friendly_name": name })
}

fn object(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// Run the integration for config entry `entry_id` until the task is
/// dropped: work out the events, refresh the position on the phase's
/// schedule, and start again when the phase changes. Each step sleeps until
/// its time on the hass clock; nothing polls.
pub async fn run(hass: Hass, registries: std::sync::Arc<Registries>, entry_id: String) {
    let config = hass.config();
    let observer = Observer {
        latitude: config.latitude,
        longitude: config.longitude,
        elevation: config.elevation as f64,
    };
    let tz = TimeZone::get(&config.time_zone).unwrap_or_else(|e| {
        tracing::warn!(
            "time zone {:?}: {e}; using UTC for the sun",
            config.time_zone
        );
        TimeZone::UTC
    });
    let entities = Entities::new(&registries, &entry_id);
    let clock = hass.clock().clone();
    // Until the wall clock reads `at`: after each wake the remaining time is
    // checked again, so a clock stepped back doesn't fire early (HA's
    // `async_track_point_in_utc_time` does the same).
    let sleep_until = |at: Timestamp| {
        let clock = clock.clone();
        async move {
            loop {
                let wait = at.duration_since(clock.now());
                if wait <= SignedDuration::ZERO {
                    return;
                }
                let wait = std::time::Duration::try_from(wait).unwrap_or_default();
                tokio::time::sleep_until(clock.instant() + wait).await;
            }
        }
    };
    loop {
        let now = clock.now();
        let ev = events(&observer, &tz, now);
        entities.write(&hass, &ev, position(&observer, now), true);
        // `update_sun_position`'s schedule until the phase changes.
        let mut at = now;
        loop {
            let every = ev.phase.update_every();
            if at + every.mul_f64(1.25) > ev.next_change {
                break;
            }
            sleep_until(at + every).await;
            at = clock.now();
            entities.write(&hass, &ev, position(&observer, at), false);
        }
        sleep_until(ev.next_change).await;
    }
}

#[cfg(test)]
mod tests;

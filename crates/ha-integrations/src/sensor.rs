//! What HA's `sensor` component does with a temperature that an
//! integration reports in °C: which unit it is shown in, where that is
//! remembered, and the conversion (`SensorEntity.add_to_platform_start`,
//! `_async_read_entity_options`, `unit_of_measurement`, `state`, and
//! `TemperatureConverter`).
//!
//! The unit is the user's (the row's `sensor` option
//! `unit_of_measurement`), else the one HA kept for the row
//! (`sensor.private`'s `suggested_unit_of_measurement`), else the unit
//! system's. HA keeps a unit for a row when the unit system changes under
//! it: a sensor that showed °C goes on showing °C on an install switched
//! to US customary units, and the row says so from the next start on.
//!
//! Checked against the rows and states HA 2026.6.1 made on a metric and a
//! US-customary install (`hue/vectors/us_customary.json`,
//! `hue/vectors/imported/` and `switchbot_cloud/vectors/us_customary.json`).

use ha_core::Hass;
use ha_registry::Entity;
use serde_json::{Map, Value, json};

/// `TemperatureConverter.VALID_UNITS`.
const UNITS: [&str; 3] = ["°C", "°F", "K"];

fn valid(unit: Option<&str>) -> Option<&'static str> {
    UNITS.into_iter().find(|u| Some(*u) == unit)
}

/// The unit system's temperature unit.
pub(crate) fn system_unit(hass: &Hass) -> &'static str {
    match hass.config().unit_system_v2 == "us_customary" {
        true => "°F",
        false => "°C",
    }
}

/// `_async_read_entity_options`: the unit the row's options name.
fn option_unit(options: &Map<String, Value>) -> Option<&'static str> {
    let of = |domain: &str, key: &str| valid(options.get(domain)?.get(key)?.as_str());
    of("sensor", "unit_of_measurement")
        .or_else(|| of("sensor.private", "suggested_unit_of_measurement"))
}

/// The unit of a running sensor's state: what its row's options name,
/// else the unit system's. `row` is the entity's row once it is
/// registered (none for an entity without one).
pub(crate) fn state_unit(hass: &Hass, row: Option<&Entity>) -> &'static str {
    row.and_then(|row| option_unit(&row.options.object()))
        .unwrap_or_else(|| system_unit(hass))
}

/// What registering a temperature sensor settles about its unit.
pub(crate) struct Registered {
    /// The row's `unit_of_measurement`.
    pub unit: &'static str,
    /// The row's options, if they change first: HA keeps the unit the row
    /// had (`sensor.private`) when the unit system would now give another.
    pub options: Option<Value>,
}

/// `add_to_platform_start`, then the unit the entity is registered with.
/// `row` is the row there is before (not one that is only among the
/// deleted: that one comes back with the unit system's unit, whatever its
/// options say, until the next start).
pub(crate) fn when_registered(hass: &Hass, row: Option<&Entity>) -> Registered {
    let system = system_unit(hass);
    let Some(row) = row else {
        return Registered {
            unit: system,
            options: None,
        };
    };
    let mut options = row.options.object();
    let unit = option_unit(&options).unwrap_or(system);
    let users = options
        .get("sensor")
        .is_some_and(|sensor| sensor.get("unit_of_measurement").is_some());
    let kept = options.contains_key("sensor.private");
    let was = row.unit_of_measurement.as_deref();
    match valid(was) {
        Some(was) if !users && !kept && was != unit => {
            let keep = json!({"suggested_unit_of_measurement": was});
            options.insert("sensor.private".into(), keep);
            Registered {
                unit: was,
                options: Some(Value::Object(options)),
            }
        }
        _ => Registered {
            unit,
            options: None,
        },
    }
}

/// `TemperatureConverter` from °C.
pub(crate) fn from_celsius(celsius: f64, unit: &str) -> f64 {
    match unit {
        "°F" => celsius * 1.8 + 32.0,
        "K" => celsius + 273.15,
        _ => celsius,
    }
}

/// A row's options with the sensor's suggested precision
/// (`_update_suggested_precision`), if that changes them. HA does this
/// only for an entity that runs: a disabled one's row is left as it is.
pub(crate) fn with_precision(row: &Entity, precision: i64) -> Option<Value> {
    let mut options = row.options.object();
    let mut sensor = match options.get("sensor") {
        Some(Value::Object(sensor)) => sensor.clone(),
        _ => Map::new(),
    };
    if sensor.get("suggested_display_precision") == Some(&json!(precision)) {
        return None;
    }
    // Taken out and put back last, as HA's dict has it.
    sensor.remove("suggested_display_precision");
    sensor.insert("suggested_display_precision".into(), json!(precision));
    options.insert("sensor".into(), Value::Object(sensor));
    Some(Value::Object(options))
}

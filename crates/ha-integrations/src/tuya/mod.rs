//! The `tuya` integration (`components/tuya` at HA 2026.6.1, on
//! `tuya-device-sharing-sdk` 0.2.8 and `tuya-device-handlers` 0.0.22), for
//! an imported Smart Life / Tuya entry: the cloud's device list, then state
//! pushed over Tuya's MQTT channel, and commands sent to the cloud.
//!
//! Covered: the dehumidifier (`cs`) and pet feeder (`cwwsq`) categories,
//! with HA's entity descriptions for them. Their entities go to the shared
//! `humidifier`, `fan`, `number`, `select` and `switch` components; sensors
//! and binary sensors only write states. Other categories are logged and
//! skipped.
//!
//! Where `hars` differs from HA, on purpose:
//! - There is no config flow (the QR-code login): an entry only arrives by
//!   import.
//! - A device renamed in the Smart Life app (`nameUpdate`) keeps its old
//!   name until the entry is set up again; HA renames it at once.
//! - The fan has on/off only (no speed, oscillation or direction).
//! - The SDK's version report, and its `unload` when the entry is removed,
//!   are not sent.

pub mod api;
pub mod mqtt;

#[cfg(test)]
mod tests;

use crate::climate::Temp;
use crate::components::Components;
use crate::config_entries::{ConfigEntries, EntryState, Status};
use crate::light::BoxFuture;
use api::{Api, ApiError, Token};
use ha_core::{Attributes, EntityId, Hass, ServiceError};
use ha_registry::{DeviceInfo, EntityPatch, Registry};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub const DOMAIN: &str = "tuya";

/// What running an entry loads, for `get_config`'s `components`.
pub const COMPONENTS: &[&str] = &[
    "tuya",
    "binary_sensor",
    "fan",
    "humidifier",
    "number",
    "select",
    "sensor",
    "switch",
];

/// `const.CELSIUS_ALIASES` / `FAHRENHEIT_ALIASES`.
const CELSIUS: [&str; 4] = ["°c", "c", "celsius", "℃"];
const FAHRENHEIT: [&str; 4] = ["°f", "f", "fahrenheit", "℉"];

/// A data point's type and range (`DeviceFunction` / `DeviceStatusRange`).
#[derive(Debug, Clone, Default)]
struct Spec {
    kind: String,
    values: Value,
    /// `dp-report-types`: `sum`, `minux` or `un_known`.
    report_type: Option<String>,
}

impl Spec {
    fn num(&self, k: &str) -> Option<f64> {
        self.values.get(k).and_then(Value::as_f64)
    }
    fn scale(&self) -> i32 {
        self.num("scale").unwrap_or(0.0) as i32
    }
    fn unit(&self) -> Option<String> {
        self.values
            .get("unit")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }
    fn strings(&self, k: &str) -> Vec<String> {
        self.values
            .get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }
    fn range(&self) -> Vec<String> {
        self.strings("range")
    }
    fn labels(&self) -> Vec<String> {
        self.strings("label")
    }
    /// A raw integer as the device means it.
    fn scaled(&self, raw: f64) -> f64 {
        raw / 10f64.powi(self.scale())
    }
    /// A value to send, back in the device's integer.
    fn unscaled(&self, v: f64) -> i64 {
        (v * 10f64.powi(self.scale())).round() as i64
    }
}

/// `CustomerDevice`, the parts used.
#[derive(Debug, Clone, Default)]
struct Dev {
    id: String,
    name: String,
    category: String,
    product_name: String,
    product_id: String,
    online: bool,
    status: Map<String, Value>,
    function: HashMap<String, Spec>,
    status_range: HashMap<String, Spec>,
    support_local: bool,
    /// dpId → status code, for local-format pushes.
    local: HashMap<i64, String>,
}

impl Dev {
    fn has(&self, code: &str) -> bool {
        self.function.contains_key(code)
            || self.status.contains_key(code)
            || self.status_range.contains_key(code)
    }
    /// `find_dpcode(..., prefer_function=True)`.
    fn spec(&self, code: &str) -> Option<&Spec> {
        self.function
            .get(code)
            .or_else(|| self.status_range.get(code))
    }
    fn first<'a>(&self, codes: &[&'a str]) -> Option<&'a str> {
        codes.iter().copied().find(|c| self.spec(c).is_some())
    }
}

/// What an entity reads and writes.
#[derive(Debug, Clone)]
enum Kind {
    Humidifier {
        switch: Option<&'static str>,
        current: Option<&'static str>,
        target: Option<&'static str>,
        mode: Option<&'static str>,
    },
    Fan {
        switch: &'static str,
    },
    Switch {
        code: &'static str,
    },
    Select {
        code: &'static str,
    },
    Number {
        code: &'static str,
    },
    Sensor {
        code: &'static str,
    },
    /// On when the code's value is `on` (a bool true when `None`).
    Binary {
        code: &'static str,
        on: Option<&'static str>,
    },
    /// On when bit `label` of a bitmap is set.
    Bitmap {
        code: &'static str,
        label: &'static str,
    },
}

/// An entity HA's description makes for a device.
#[derive(Debug, Clone)]
struct Desc {
    domain: &'static str,
    /// The unique id's suffix after `tuya.<device id>`.
    key: &'static str,
    name: Option<&'static str>,
    translation_key: Option<&'static str>,
    device_class: Option<&'static str>,
    entity_category: Option<&'static str>,
    icon: Option<&'static str>,
    measurement: bool,
    /// The description's own unit, else the data point's.
    unit: Option<&'static str>,
    kind: Kind,
}

fn desc(domain: &'static str, key: &'static str, kind: Kind) -> Desc {
    Desc {
        domain,
        key,
        name: None,
        translation_key: None,
        device_class: None,
        entity_category: None,
        icon: None,
        measurement: false,
        unit: None,
        kind,
    }
}

impl Desc {
    fn named(mut self, tk: &'static str, name: &'static str) -> Self {
        self.translation_key = Some(tk);
        self.name = Some(name);
        self
    }
    fn class(mut self, c: &'static str) -> Self {
        self.device_class = Some(c);
        self
    }
    fn cat(mut self, c: &'static str) -> Self {
        self.entity_category = Some(c);
        self
    }
    fn icon(mut self, i: &'static str) -> Self {
        self.icon = Some(i);
        self
    }
    fn measured(mut self) -> Self {
        self.measurement = true;
        self
    }
    fn unit(mut self, u: &'static str) -> Self {
        self.unit = Some(u);
        self
    }
}

/// HA's descriptions for a device's category, kept only where the device
/// has the data points they need (each platform's `async_discover_device`).
fn descriptions(d: &Dev) -> Vec<Desc> {
    let mut out = Vec::new();
    let mut add = |x: Desc, needs: &str| {
        if d.spec(needs).is_some() {
            out.push(x);
        }
    };
    match d.category.as_str() {
        "cs" => {
            // `humidifier.get_default_definition`: any of its codes.
            let switch = ["switch", "switch_spray"];
            let codes = [
                "switch",
                "switch_spray",
                "humidity_indoor",
                "dehumidify_set_value",
            ];
            if let Some(any) = codes.into_iter().find(|c| d.has(c)) {
                let mode = d
                    .first(&["mode"])
                    .filter(|m| d.spec(m).is_some_and(|s| s.kind == "Enum"));
                let hum = desc(
                    "humidifier",
                    "switch",
                    Kind::Humidifier {
                        switch: d.first(&switch),
                        current: d.first(&["humidity_indoor"]),
                        target: d.first(&["dehumidify_set_value"]),
                        mode,
                    },
                )
                .class("dehumidifier");
                if d.has(any) {
                    out.push(hum);
                }
            }
            let mut add = |x: Desc, needs: &str| {
                if d.spec(needs).is_some() {
                    out.push(x);
                }
            };
            // `fan.get_default_definition`: on/off only in hars.
            if let Some(switch) = d.first(&["switch_fan", "fan_switch", "switch"]) {
                add(desc("fan", "", Kind::Fan { switch }), switch);
            }
            add(
                desc(
                    "select",
                    "countdown_set",
                    Kind::Select {
                        code: "countdown_set",
                    },
                )
                .named("countdown", "Countdown")
                .cat("config"),
                "countdown_set",
            );
            add(
                desc(
                    "select",
                    "dehumidify_set_enum",
                    Kind::Select {
                        code: "dehumidify_set_enum",
                    },
                )
                .named("target_humidity", "Target humidity")
                .cat("config"),
                "dehumidify_set_enum",
            );
            for (code, tk, name, icon) in [
                ("anion", "ionizer", "Ionizer", "mdi:atom"),
                ("child_lock", "child_lock", "Child lock", "mdi:account-lock"),
                ("filter_reset", "filter_reset", "Filter reset", "mdi:filter"),
            ] {
                add(
                    desc("switch", code, Kind::Switch { code })
                        .named(tk, name)
                        .icon(icon)
                        .cat("config"),
                    code,
                );
            }
            add(
                desc(
                    "sensor",
                    "temp_indoor",
                    Kind::Sensor {
                        code: "temp_indoor",
                    },
                )
                .named("temperature", "Temperature")
                .class("temperature")
                .measured(),
                "temp_indoor",
            );
            add(
                desc(
                    "sensor",
                    "humidity_indoor",
                    Kind::Sensor {
                        code: "humidity_indoor",
                    },
                )
                .named("humidity", "Humidity")
                .class("humidity")
                .measured(),
                "humidity_indoor",
            );
            let labels = d.spec("fault").map(Spec::labels).unwrap_or_default();
            for (key, label, tk, name) in [
                ("fault_water_full", "water_full", "tankfull", "Tank full"),
                ("tankfull", "tankfull", "tankfull", "Tank full"),
                ("fault_FULL", "FULL", "tankfull", "Tank full"),
                ("defrost", "defrost", "defrost", "Defrost"),
                ("fault_COIL", "COIL", "coil_freeze", "Coil freeze/defrost"),
                ("wet", "wet", "wet", "Wet"),
                (
                    "fault_Cleaning",
                    "Cleaning",
                    "filter_cleaning",
                    "Filter cleaning",
                ),
                ("fault_E1", "E1", "temp_error", "Temperature error"),
                ("fault_CL", "CL", "low_temp", "Low temperature"),
                ("fault_CH", "CH", "high_temp", "High temperature"),
                ("fault_LO", "LO", "low_humidity", "Low humidity"),
                ("fault_MOTOR", "MOTOR", "motor_fault", "Motor fault"),
            ] {
                if labels.iter().any(|l| l == label) {
                    out.push(
                        desc(
                            "binary_sensor",
                            key,
                            Kind::Bitmap {
                                code: "fault",
                                label,
                            },
                        )
                        .named(tk, name)
                        .class("problem")
                        .cat("diagnostic"),
                    );
                }
            }
        }
        "cwwsq" => {
            add(
                desc(
                    "number",
                    "manual_feed",
                    Kind::Number {
                        code: "manual_feed",
                    },
                )
                .named("feed", "Feed"),
                "manual_feed",
            );
            add(
                desc(
                    "number",
                    "voice_times",
                    Kind::Number {
                        code: "voice_times",
                    },
                )
                .named("voice_times", "Voice times"),
                "voice_times",
            );
            add(
                desc("switch", "slow_feed", Kind::Switch { code: "slow_feed" })
                    .named("slow_feed", "Slow feed")
                    .cat("config"),
                "slow_feed",
            );
            for code in [
                "battery_percentage",
                "battery",
                "battery_value",
                "va_battery",
            ] {
                let mut s = desc("sensor", code, Kind::Sensor { code })
                    .named("battery", "Battery")
                    .class("battery")
                    .cat("diagnostic")
                    .measured();
                if matches!(code, "battery_percentage" | "battery") {
                    s = s.unit("%");
                }
                add(s, code);
            }
            add(
                desc(
                    "sensor",
                    "battery_state",
                    Kind::Sensor {
                        code: "battery_state",
                    },
                )
                .named("battery_state", "Battery state")
                .cat("diagnostic"),
                "battery_state",
            );
            add(
                desc(
                    "sensor",
                    "feed_report",
                    Kind::Sensor {
                        code: "feed_report",
                    },
                )
                .named("last_amount", "Last amount")
                .measured(),
                "feed_report",
            );
            add(
                desc(
                    "binary_sensor",
                    "feed_state",
                    Kind::Binary {
                        code: "feed_state",
                        on: Some("feeding"),
                    },
                )
                .named("feeding", "Feeding"),
                "feed_state",
            );
            let mut charging = desc(
                "binary_sensor",
                "charge_state",
                Kind::Binary {
                    code: "charge_state",
                    on: None,
                },
            )
            .class("battery_charging")
            .cat("diagnostic");
            charging.name = Some("Charging");
            add(charging, "charge_state");
        }
        other => tracing::info!(
            "Tuya device {} (category {other}) isn't supported by hars yet",
            d.name
        ),
    }
    out
}

/// What a sensor shows: its state's unit, device class, state class and
/// options, and whether a temperature is converted. Settled at set-up
/// (`TuyaSensorEntity.__init__` and `_validate_device_class_unit`).
#[derive(Debug, Clone, Default)]
struct Shown {
    unit: Option<String>,
    device_class: Option<&'static str>,
    options: Option<Vec<String>>,
    state_class: Option<&'static str>,
    /// A temperature's native unit, when it is shown in another.
    native_temp: Option<&'static str>,
}

/// `util.get_temperature_unit`, plus `K`, which HA's temperature sensor
/// takes as it comes.
fn temperature_unit(d: &Dev, uom: Option<&str>) -> Option<&'static str> {
    match uom.filter(|u| !u.is_empty()) {
        None => match d.status.get("temp_unit_convert").and_then(Value::as_str) {
            Some("c") => Some("°C"),
            Some("f") => Some("°F"),
            _ => None,
        },
        Some(u) => {
            let u = u.to_lowercase();
            if CELSIUS.contains(&u.as_str()) {
                Some("°C")
            } else if FAHRENHEIT.contains(&u.as_str()) {
                Some("°F")
            } else if u == "k" {
                Some("K")
            } else {
                None
            }
        }
    }
}

/// A sensor's [`Shown`]. `row` is its registry row, for the unit the
/// sensor component shows a temperature in.
fn shown(hass: &Hass, d: &Dev, x: &Desc, row: Option<&ha_registry::Entity>) -> Shown {
    let Kind::Sensor { code } = x.kind else {
        return Shown::default();
    };
    let spec = d.spec(code).cloned().unwrap_or_default();
    let mut s = Shown {
        device_class: x.device_class,
        state_class: x.measurement.then_some("measurement"),
        ..Default::default()
    };
    // An enum data point without a device class is an ENUM sensor.
    if spec.kind == "Enum" && x.device_class.is_none() {
        s.device_class = Some("enum");
        s.options = Some(spec.range());
        return s;
    }
    // An integer the cloud sums is a total that only grows.
    if spec.kind == "Integer" && !x.measurement && spec.report_type.as_deref() == Some("sum") {
        s.state_class = Some("total_increasing");
    }
    let native = spec.unit();
    s.unit = match x.device_class {
        Some("temperature") => match temperature_unit(d, native.as_deref()) {
            Some(native) => {
                let unit = crate::sensor::state_unit(hass, row);
                s.native_temp = (native != unit).then_some(native);
                Some(unit.to_owned())
            }
            None => x.unit.map(str::to_owned),
        },
        _ => x.unit.map(str::to_owned).or(native),
    };
    s
}

/// `TemperatureConverter` between two temperature units.
fn convert_temp(v: f64, from: &str, to: &str) -> f64 {
    let celsius = match from {
        "°F" => (v - 32.0) / 1.8,
        "K" => v - 273.15,
        _ => v,
    };
    crate::sensor::from_celsius(celsius, to)
}

/// A (de)humidifier's `min_humidity` and `max_humidity`: its target's
/// range, rounded, else HA's defaults (0 and 100).
fn min_max_humidity(x: &Desc, dev: &Dev) -> (Temp, Temp) {
    use crate::humidifier::{DEFAULT_MAX_HUMIDITY, DEFAULT_MIN_HUMIDITY};
    let Kind::Humidifier {
        target: Some(t), ..
    } = x.kind
    else {
        return (DEFAULT_MIN_HUMIDITY, DEFAULT_MAX_HUMIDITY);
    };
    let s = dev.spec(t).cloned().unwrap_or_default();
    let get = |k: &str, default: Temp| {
        s.num(k)
            .map(|v| Temp::Int(s.scaled(v).round() as i64))
            .unwrap_or(default)
    };
    (
        get("min", DEFAULT_MIN_HUMIDITY),
        get("max", DEFAULT_MAX_HUMIDITY),
    )
}

/// The capability attributes: kept while unavailable, and stored in the
/// registry row.
fn capabilities(x: &Desc, shown: &Shown, dev: &Dev) -> Attributes {
    let mut cap = Attributes::new();
    let spec = |code: &str| dev.spec(code).cloned().unwrap_or_default();
    match &x.kind {
        Kind::Humidifier { mode, .. } => {
            let (min, max) = min_max_humidity(x, dev);
            let num = |t: Temp| match t {
                Temp::Int(i) => json!(i),
                Temp::Float(f) => json!(f),
            };
            cap.insert("min_humidity".into(), num(min));
            cap.insert("max_humidity".into(), num(max));
            if let Some(m) = mode {
                cap.insert("available_modes".into(), json!(spec(m).range()));
            }
        }
        Kind::Select { code } => {
            cap.insert("options".into(), json!(spec(code).range()));
        }
        Kind::Number { code } => {
            let s = spec(code);
            cap.insert("min".into(), json!(s.scaled(s.num("min").unwrap_or(0.0))));
            cap.insert("max".into(), json!(s.scaled(s.num("max").unwrap_or(100.0))));
            cap.insert("step".into(), json!(s.scaled(s.num("step").unwrap_or(1.0))));
            cap.insert("mode".into(), json!("auto"));
        }
        Kind::Sensor { .. } => {
            if let Some(c) = shown.state_class {
                cap.insert("state_class".into(), json!(c));
            }
            if let Some(o) = &shown.options {
                cap.insert("options".into(), json!(o));
            }
        }
        _ => {}
    }
    cap
}

fn supported_features(x: &Desc) -> u32 {
    match &x.kind {
        Kind::Humidifier { mode: Some(_), .. } => crate::humidifier::feature::MODES,
        Kind::Fan { .. } => crate::fan::feature::TURN_ON | crate::fan::feature::TURN_OFF,
        _ => 0,
    }
}

/// The unit an entity shows, if it has one.
fn unit_of(x: &Desc, shown: &Shown, dev: &Dev) -> Option<String> {
    match &x.kind {
        Kind::Sensor { .. } => shown.unit.clone(),
        Kind::Number { code } => x
            .unit
            .map(str::to_owned)
            .or_else(|| dev.spec(code).and_then(Spec::unit)),
        _ => None,
    }
}

/// What an entry's entities share.
pub struct Shared {
    hass: Hass,
    api: Arc<Api>,
    devices: Mutex<HashMap<String, Dev>>,
    alive: Mutex<bool>,
    /// The SDK's `Filter`: the same commands to a device within 10 s are
    /// sent once. Only a command that was sent counts.
    last_command: Mutex<HashMap<String, (Value, tokio::time::Instant)>>,
}

impl Shared {
    async fn send(&self, device: &str, commands: Value) -> Result<(), ServiceError> {
        let clock = self.hass.clock();
        if let Some((v, at)) = self.last_command.lock().get(device)
            && *v == commands
            && clock.instant().saturating_duration_since(*at) < Duration::from_secs(10)
        {
            return Ok(());
        }
        self.api
            .post(
                &format!("/v1.1/m/thing/{device}/commands"),
                json!({"commands": commands}),
            )
            .await
            .map_err(|e| ServiceError::Failed(format!("Tuya command failed: {e}")))?;
        self.last_command
            .lock()
            .insert(device.to_owned(), (commands, clock.instant()));
        Ok(())
    }

    fn spec(&self, device: &str, code: &str) -> Spec {
        self.devices
            .lock()
            .get(device)
            .and_then(|d| d.spec(code).cloned())
            .unwrap_or_default()
    }

    fn value(&self, device: &str, code: &str) -> Option<Value> {
        self.devices
            .lock()
            .get(device)
            .and_then(|d| d.status.get(code).cloned())
    }
}

/// One entity: its id and friendly name as the registry had them at
/// set-up, and what its description reads.
pub struct Ent {
    shared: Arc<Shared>,
    device: String,
    entity_id: EntityId,
    friendly: String,
    d: Desc,
    shown: Shown,
}

fn num_state(v: f64) -> String {
    crate::num::py_format(v, 15)
}

/// `DPCodeRoundedIntegerWrapper`: a scaled value rounded to an int.
fn rounded(spec: &Spec, v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_f64)
        .map(|v| spec.scaled(v).round() as i64)
}

impl Ent {
    /// The state and the attributes only written while available.
    fn compute(&self, dev: &Dev) -> (String, Attributes) {
        let mut st = Attributes::new();
        let raw = |code: &str| dev.status.get(code);
        let spec = |code: &str| dev.spec(code).cloned().unwrap_or_default();
        let bool_state = |b: Option<bool>| match b {
            Some(true) => "on".to_owned(),
            Some(false) => "off".to_owned(),
            None => "unknown".to_owned(),
        };
        let state = match &self.d.kind {
            Kind::Humidifier {
                switch,
                current,
                target,
                mode,
            } => {
                if let Some(c) = current.and_then(|c| rounded(&spec(c), raw(c))) {
                    st.insert("current_humidity".into(), json!(c));
                }
                if let Some(t) = target.and_then(|t| rounded(&spec(t), raw(t))) {
                    st.insert("humidity".into(), json!(t));
                }
                if let Some(m) = mode {
                    st.insert("mode".into(), raw(m).cloned().unwrap_or(Value::Null));
                }
                bool_state(switch.and_then(raw).and_then(Value::as_bool))
            }
            Kind::Fan { switch } => bool_state(raw(switch).and_then(Value::as_bool)),
            Kind::Switch { code } => bool_state(raw(code).and_then(Value::as_bool)),
            Kind::Binary { code, on } => {
                let v = raw(code);
                bool_state(match on {
                    Some(on) => v.and_then(Value::as_str).map(|s| s == *on),
                    None => v.and_then(Value::as_bool),
                })
            }
            Kind::Bitmap { code, label } => {
                let bit = spec(code).labels().iter().position(|l| l == label);
                bool_state(match (bit, raw(code).and_then(Value::as_i64)) {
                    (Some(bit), Some(v)) => Some((v >> bit) & 1 == 1),
                    _ => None,
                })
            }
            Kind::Select { code } => raw(code)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| "unknown".into()),
            Kind::Number { code } => {
                let s = spec(code);
                raw(code)
                    .and_then(Value::as_f64)
                    .map(|v| num_state(s.scaled(v)))
                    .unwrap_or_else(|| "unknown".into())
            }
            Kind::Sensor { code } => {
                let s = spec(code);
                match (s.kind.as_str(), raw(code)) {
                    ("Integer", Some(v)) => v.as_f64().map(|v| {
                        let v = s.scaled(v);
                        match (self.shown.native_temp, &self.shown.unit) {
                            (Some(from), Some(to)) => num_state(convert_temp(v, from, to)),
                            _ => num_state(v),
                        }
                    }),
                    // An enum's value outside its range reads as unknown.
                    ("Enum", Some(Value::String(v))) => s.range().contains(v).then(|| v.clone()),
                    (_, Some(Value::String(v))) => Some(v.clone()),
                    (_, Some(Value::Number(n))) => n.as_f64().map(num_state),
                    _ => None,
                }
                .unwrap_or_else(|| "unknown".into())
            }
        };
        (state, st)
    }

    fn write(&self) {
        let devices = self.shared.devices.lock();
        let Some(dev) = devices.get(&self.device) else {
            return;
        };
        let available = dev.online;
        let mut attrs = capabilities(&self.d, &self.shown, dev);
        let (state, st) = self.compute(dev);
        if available {
            attrs.extend(st);
        }
        if let Some(u) = unit_of(&self.d, &self.shown, dev) {
            attrs.insert("unit_of_measurement".into(), json!(u));
        }
        let device_class = match &self.d.kind {
            Kind::Sensor { .. } => self.shown.device_class,
            _ => self.d.device_class,
        };
        if let Some(c) = device_class {
            attrs.insert("device_class".into(), json!(c));
        }
        if let Some(i) = self.d.icon {
            attrs.insert("icon".into(), json!(i));
        }
        attrs.insert("friendly_name".into(), json!(self.friendly));
        if matches!(self.d.kind, Kind::Humidifier { .. } | Kind::Fan { .. }) {
            attrs.insert(
                "supported_features".into(),
                json!(supported_features(&self.d)),
            );
        }
        drop(devices);
        if *self.shared.alive.lock() {
            let state = if available {
                state
            } else {
                "unavailable".into()
            };
            tracing::debug!("tuya: {} = {state}", self.entity_id);
            self.shared
                .hass
                .set_state(self.entity_id.clone(), state, attrs);
        }
    }

    fn available(&self) -> bool {
        self.shared
            .devices
            .lock()
            .get(&self.device)
            .is_some_and(|d| d.online)
    }

    fn switch_code(&self) -> Option<&'static str> {
        match &self.d.kind {
            Kind::Humidifier { switch, .. } => *switch,
            Kind::Fan { switch } => Some(switch),
            Kind::Switch { code } => Some(code),
            _ => None,
        }
    }

    fn is_on(&self) -> bool {
        self.switch_code()
            .and_then(|c| self.shared.value(&self.device, c))
            .and_then(|v| v.as_bool())
            == Some(true)
    }

    /// `ActionDPCodeNotFoundError`: the device has no function for this.
    fn no_dpcode(&self, expected: &[&str]) -> ServiceError {
        let mut available: Vec<String> = self
            .shared
            .devices
            .lock()
            .get(&self.device)
            .map(|d| d.function.keys().cloned().collect())
            .unwrap_or_default();
        available.sort();
        let mut expected: Vec<String> = expected.iter().map(|s| (*s).to_owned()).collect();
        expected.sort();
        // Python's `str()` of a list of strings.
        let list = |v: &[String]| {
            let items: Vec<String> = v.iter().map(|s| format!("'{s}'")).collect();
            format!("[{}]", items.join(", "))
        };
        let (expected, available) = (list(&expected), list(&available));
        let mut p = Map::new();
        p.insert("expected".into(), json!(expected));
        p.insert("available".into(), json!(available));
        ServiceError::validation(
            DOMAIN,
            "action_dpcode_not_found",
            Some(p),
            format!(
                "Unable to process action as the device does not provide a corresponding function code (expected one of {expected}, got {available})."
            ),
        )
    }

    async fn turn(&self, on: bool) -> Result<(), ServiceError> {
        let Some(code) = self.switch_code() else {
            return Err(self.no_dpcode(&["switch", "switch_spray"]));
        };
        self.shared
            .send(&self.device, json!([{"code": code, "value": on}]))
            .await
    }

    async fn send_int(&self, code: &str, v: f64) -> Result<(), ServiceError> {
        let spec = self.shared.spec(&self.device, code);
        self.shared
            .send(
                &self.device,
                json!([{"code": code, "value": spec.unscaled(v)}]),
            )
            .await
    }

    async fn send_enum(&self, code: &str, v: &str) -> Result<(), ServiceError> {
        self.shared
            .send(&self.device, json!([{"code": code, "value": v}]))
            .await
    }

    fn not_supported(&self, service: &str) -> BoxFuture<'_> {
        let e = ServiceError::not_supported(self.d.domain, service, self.entity_id.as_str());
        Box::pin(async move { Err(e) })
    }

    fn with_dev<T>(&self, f: impl FnOnce(&Dev) -> T) -> Option<T> {
        self.shared.devices.lock().get(&self.device).map(f)
    }
}

/// The entity as each component sees it.
struct As(Arc<Ent>);

impl crate::switch::Switch for As {
    fn entity_id(&self) -> &EntityId {
        &self.0.entity_id
    }
    fn available(&self) -> bool {
        self.0.available()
    }
    fn is_on(&self) -> bool {
        self.0.is_on()
    }
    fn turn(&self, on: bool) -> BoxFuture<'_> {
        Box::pin(self.0.turn(on))
    }
}

impl crate::humidifier::Humidifier for As {
    fn entity_id(&self) -> &EntityId {
        &self.0.entity_id
    }
    fn available(&self) -> bool {
        self.0.available()
    }
    fn supported_features(&self) -> u32 {
        supported_features(&self.0.d)
    }
    fn is_on(&self) -> bool {
        self.0.is_on()
    }
    fn min_humidity(&self) -> Temp {
        self.0
            .with_dev(|d| min_max_humidity(&self.0.d, d).0)
            .unwrap_or(crate::humidifier::DEFAULT_MIN_HUMIDITY)
    }
    fn max_humidity(&self) -> Temp {
        self.0
            .with_dev(|d| min_max_humidity(&self.0.d, d).1)
            .unwrap_or(crate::humidifier::DEFAULT_MAX_HUMIDITY)
    }
    fn turn(&self, on: bool) -> BoxFuture<'_> {
        Box::pin(self.0.turn(on))
    }
    fn set_humidity(&self, humidity: i64) -> BoxFuture<'_> {
        Box::pin(async move {
            match &self.0.d.kind {
                Kind::Humidifier {
                    target: Some(t), ..
                } => self.0.send_int(t, humidity as f64).await,
                _ => Err(self.0.no_dpcode(&["dehumidify_set_value"])),
            }
        })
    }
    fn set_mode(&self, mode: &str) -> BoxFuture<'_> {
        let mode = mode.to_owned();
        Box::pin(async move {
            match &self.0.d.kind {
                Kind::Humidifier { mode: Some(m), .. } => self.0.send_enum(m, &mode).await,
                _ => Err(self.0.no_dpcode(&["mode"])),
            }
        })
    }
}

impl crate::fan::Fan for As {
    fn entity_id(&self) -> &EntityId {
        &self.0.entity_id
    }
    fn available(&self) -> bool {
        self.0.available()
    }
    fn supported_features(&self) -> u32 {
        supported_features(&self.0.d)
    }
    fn is_on(&self) -> bool {
        self.0.is_on()
    }
    fn turn_on(&self, _percentage: Option<i64>, _preset: Option<String>) -> BoxFuture<'_> {
        Box::pin(self.0.turn(true))
    }
    fn turn_off(&self) -> BoxFuture<'_> {
        Box::pin(self.0.turn(false))
    }
    // The fan advertises on/off only, so the component refuses these
    // before they get here.
    fn set_percentage(&self, _: i64) -> BoxFuture<'_> {
        self.0.not_supported("set_percentage")
    }
    fn set_preset_mode(&self, _: &str) -> BoxFuture<'_> {
        self.0.not_supported("set_preset_mode")
    }
    fn oscillate(&self, _: bool) -> BoxFuture<'_> {
        self.0.not_supported("oscillate")
    }
    fn set_direction(&self, _: &str) -> BoxFuture<'_> {
        self.0.not_supported("set_direction")
    }
}

impl crate::number::Number for As {
    fn entity_id(&self) -> &EntityId {
        &self.0.entity_id
    }
    fn available(&self) -> bool {
        self.0.available()
    }
    fn min_value(&self) -> f64 {
        match &self.0.d.kind {
            Kind::Number { code } => {
                let s = self.0.shared.spec(&self.0.device, code);
                s.scaled(s.num("min").unwrap_or(0.0))
            }
            _ => 0.0,
        }
    }
    fn max_value(&self) -> f64 {
        match &self.0.d.kind {
            Kind::Number { code } => {
                let s = self.0.shared.spec(&self.0.device, code);
                s.scaled(s.num("max").unwrap_or(100.0))
            }
            _ => 100.0,
        }
    }
    fn set_value(&self, value: f64) -> BoxFuture<'_> {
        Box::pin(async move {
            match &self.0.d.kind {
                Kind::Number { code } => self.0.send_int(code, value).await,
                _ => Err(self.0.no_dpcode(&[])),
            }
        })
    }
}

impl crate::select::Select for As {
    fn entity_id(&self) -> &EntityId {
        &self.0.entity_id
    }
    fn available(&self) -> bool {
        self.0.available()
    }
    fn options(&self) -> Vec<String> {
        match &self.0.d.kind {
            Kind::Select { code } => self.0.shared.spec(&self.0.device, code).range(),
            _ => Vec::new(),
        }
    }
    fn current_option(&self) -> Option<String> {
        match &self.0.d.kind {
            Kind::Select { code } => self
                .0
                .shared
                .value(&self.0.device, code)
                .and_then(|v| v.as_str().map(str::to_owned)),
            _ => None,
        }
    }
    fn select_option(&self, option: &str) -> BoxFuture<'_> {
        let option = option.to_owned();
        Box::pin(async move {
            match &self.0.d.kind {
                Kind::Select { code } => self.0.send_enum(code, &option).await,
                _ => Err(self.0.no_dpcode(&[])),
            }
        })
    }
}

fn s(v: &Value, k: &str) -> String {
    match v.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn specs(list: Option<&Value>) -> HashMap<String, Spec> {
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|f| {
            let code = f.get("code")?.as_str()?.to_owned();
            let values = match f.get("values") {
                Some(Value::String(v)) => serde_json::from_str(v).unwrap_or(Value::Null),
                Some(v) => v.clone(),
                None => Value::Null,
            };
            Some((
                code,
                Spec {
                    kind: s(f, "type"),
                    values,
                    report_type: None,
                },
            ))
        })
        .collect()
}

/// `Manager.update_device_cache` (SDK 0.2.8): every home's devices with
/// their specifications, push format and report types. Also the home ids,
/// for the owner topic.
async fn fetch(api: &Api) -> Result<(Vec<String>, Vec<Dev>), ApiError> {
    let homes = api.get("/v1.0/m/life/users/homes", None).await?;
    let home_ids: Vec<String> = homes["result"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|h| s(h, "ownerId"))
        .filter(|h| !h.is_empty())
        .collect();
    let mut devices = Vec::new();
    for home in &home_ids {
        let r = api
            .get(
                "/v1.0/m/life/ha/home/devices",
                Some(json!({"homeId": home})),
            )
            .await?;
        for item in r["result"].as_array().into_iter().flatten() {
            let mut d = Dev {
                id: s(item, "id"),
                name: s(item, "name"),
                category: s(item, "category"),
                product_name: s(item, "product_name"),
                product_id: s(item, "product_id"),
                online: item.get("online").and_then(Value::as_bool).unwrap_or(false),
                ..Default::default()
            };
            for st in item
                .get("status")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let (Some(c), Some(v)) =
                    (st.get("code").and_then(Value::as_str), st.get("value"))
                {
                    d.status.insert(c.to_owned(), v.clone());
                }
            }
            let spec = api
                .get(&format!("/v1.1/m/life/{}/specifications", d.id), None)
                .await?;
            d.function = specs(spec["result"].get("functions"));
            d.status_range = specs(spec["result"].get("status"));
            // `update_device_strategy_info`: local-format pushes only when
            // every data point supports them, and the device lists some.
            let rel = api
                .get(&format!("/v1.0/m/life/devices/{}/status", d.id), None)
                .await?;
            let mut local = true;
            for x in rel["result"]["dpStatusRelationDTOS"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if x.get("supportLocal").and_then(Value::as_bool) != Some(true) {
                    local = false;
                    break;
                }
                if s(x, "valueConvert") != "default" {
                    tracing::debug!(
                        "tuya: {} dp {} uses conversion {}, read as it comes",
                        d.id,
                        s(x, "dpId"),
                        s(x, "valueConvert")
                    );
                }
                if let Some(id) = x.get("dpId").and_then(Value::as_i64) {
                    d.local.insert(id, s(x, "statusCode"));
                }
            }
            d.support_local = local && !d.local.is_empty();
            if !d.support_local {
                d.local.clear();
            }
            // `update_device_report_type`.
            let types = api
                .get(&format!("/v1.0/m/life/ha/{}/dp-report-types", d.id), None)
                .await?;
            for t in types["result"].as_array().into_iter().flatten() {
                if let Some(spec) = t
                    .get("dp_code")
                    .and_then(Value::as_str)
                    .and_then(|c| d.status_range.get_mut(c))
                {
                    spec.report_type = t
                        .get("report_type")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
            }
            devices.push(d);
        }
    }
    Ok((home_ids, devices))
}

/// The devices' and entities' registry rows, as HA's platforms make them:
/// made if missing, changed where HA's values differ.
fn register(
    hass: &Hass,
    registry: &Registry,
    entry_id: &str,
    disable_new: bool,
    found: &[(Dev, Vec<Desc>)],
) {
    // Nothing is registered when the stores can't be written (an install
    // imported from an older Home Assistant).
    if registry.read_only().is_some() {
        return;
    }
    for (d, descs) in found {
        if descs.is_empty() {
            continue;
        }
        let device = registry.get_or_create_device(DeviceInfo {
            config_entry_id: entry_id.to_owned(),
            identifiers: vec![json!([DOMAIN, d.id])],
            manufacturer: Some("Tuya".into()),
            model: Some(d.product_name.clone()),
            model_id: Some(d.product_id.clone()),
            name: Some(d.name.clone()),
            ..Default::default()
        });
        for x in descs {
            let unique_id = format!("tuya.{}{}", d.id, x.key);
            let row = registry.entity_by_unique_id(x.domain, DOMAIN, &unique_id);
            let shown = shown(hass, d, x, row.as_deref());
            // A temperature's unit is the sensor component's to settle,
            // which may first keep the old one in the row's options.
            let unit = if shown.native_temp.is_some()
                || (x.device_class == Some("temperature") && shown.unit.is_some())
            {
                let settled = crate::sensor::when_registered(hass, row.as_deref());
                if let (Some(row), Some(options)) = (&row, settled.options) {
                    let options = EntityPatch {
                        options: Some(options),
                        ..Default::default()
                    };
                    registry.update_entity(&row.entity_id, options);
                }
                Some(settled.unit.to_owned())
            } else {
                unit_of(x, &shown, d)
            };
            let cap = capabilities(x, &shown, d);
            let device_class = match &x.kind {
                Kind::Sensor { .. } => shown.device_class,
                _ => x.device_class,
            };
            let text = |s: Option<&str>| Some(s.map(str::to_owned));
            let patch = EntityPatch {
                // A fan keeps an empty mapping; the others none.
                capabilities: Some(
                    (!cap.is_empty() || x.domain == "fan").then_some(Value::Object(cap)),
                ),
                config_entry_id: Some(Some(entry_id.to_owned())),
                device_id: Some(Some(device.id.clone())),
                entity_category: Some(x.entity_category.map(str::to_owned)),
                has_entity_name: Some(true),
                object_id_base: text(x.name),
                original_device_class: text(device_class),
                original_icon: text(x.icon),
                original_name: text(x.name),
                supported_features: Some(i64::from(supported_features(x))),
                translation_key: text(x.translation_key),
                unit_of_measurement: Some(unit),
                pref_disable_new: disable_new,
                ..Default::default()
            };
            registry.get_or_create_entity(x.domain, DOMAIN, &unique_id, None, patch);
        }
    }
}

/// Apply one push message (`Manager.on_message`); the device it was for.
/// A rename (`nameUpdate`) is not followed: see the module's notes.
fn on_message(devices: &mut HashMap<String, Dev>, msg: &Value) -> Option<String> {
    let data = msg.get("data")?;
    match msg.get("protocol").and_then(Value::as_i64) {
        Some(4) => {
            let id = data.get("devId")?.as_str()?;
            let dev = devices.get_mut(id)?;
            for item in data.get("status")?.as_array()? {
                let Some(value) = item.get("value") else {
                    continue;
                };
                let code = if dev.support_local {
                    match item
                        .get("dpId")
                        .and_then(Value::as_i64)
                        .and_then(|i| dev.local.get(&i))
                    {
                        Some(c) => c.clone(),
                        None => continue,
                    }
                } else {
                    match item.get("code").and_then(Value::as_str) {
                        Some(c) => c.to_owned(),
                        None => continue,
                    }
                };
                dev.status.insert(code, value.clone());
            }
            Some(id.to_owned())
        }
        Some(20) => {
            let id = data.get("bizData")?.get("devId")?.as_str()?;
            let dev = devices.get_mut(id)?;
            match data.get("bizCode")?.as_str()? {
                "online" => dev.online = true,
                "offline" => dev.online = false,
                _ => return None,
            }
            Some(id.to_owned())
        }
        _ => None,
    }
}

/// `coordinator.py`'s check for a login Tuya refuses for good
/// (`ConfigEntryAuthFailed`): no retry will fix it.
fn auth_failed(e: &ApiError) -> bool {
    match e {
        ApiError::Api { code, msg } => code == "1010" || msg.contains("sign invalid"),
        _ => false,
    }
}

/// Set up an entry and keep its devices' states current from the push
/// channel. `store` keeps the refreshed tokens; `endpoint` replaces the
/// entry's API address (tests).
pub async fn run(
    components: Arc<Components>,
    entry: Map<String, Value>,
    store: Option<Arc<ConfigEntries>>,
    endpoint: Option<String>,
    ready: ha_core::SetupGuard,
    status: Status,
) {
    let mut ready = Some(ready);
    let hass = components.hass().clone();
    let data = entry
        .get("data")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let entry_id = entry
        .get("entry_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let endpoint = endpoint.unwrap_or_else(|| s(&Value::Object(data.clone()), "endpoint"));
    let Some(token) = data
        .get("token_info")
        .map(Token::from_value)
        .filter(|t| !t.refresh_token.is_empty())
    else {
        tracing::error!("tuya: the entry has no token");
        status.set(EntryState::SetupError, None);
        return;
    };
    let client = match ha_http::Client::builder(crate::USER_AGENT).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("tuya: {e}");
            status.set(EntryState::SetupError, None);
            return;
        }
    };
    let api = Api::new(client, hass.clock().clone(), endpoint, token);
    {
        // `TokenListener.update_token`: the entry keeps the new pair.
        let store = store.clone();
        let entry_id = entry_id.clone();
        let data = Mutex::new(data);
        api.on_token(move |t| {
            let mut d = data.lock();
            d.insert("token_info".into(), t.to_value());
            if let Some(store) = &store {
                store.update_data(&entry_id, d.clone());
            }
        });
    }
    let mut tries = 0u32;
    let (home_ids, devices) = loop {
        if tries > 0 {
            status.set(EntryState::SetupInProgress, None);
        }
        match fetch(&api).await {
            Ok(x) => break x,
            Err(e) if auth_failed(&e) => {
                tracing::error!(
                    "tuya: the login was refused ({e}); add Tuya again in Home Assistant and import again"
                );
                status.set(EntryState::SetupError, None);
                return;
            }
            Err(e) => {
                let why = e.to_string();
                status.set(EntryState::SetupRetry, Some(why.clone()));
                if let Some(r) = ready.take() {
                    r.done();
                }
                let wait = (5u64 << tries.min(7)).min(600);
                tracing::info!(
                    "Config entry for tuya not ready yet: {why}; Retrying in {wait} seconds"
                );
                tries += 1;
                hass.setup_retry_wait(Duration::from_secs(wait)).await;
            }
        }
    };
    // Each device's descriptions, worked out once.
    let found: Vec<(Dev, Vec<Desc>)> = devices
        .into_iter()
        .map(|d| {
            let descs = descriptions(&d);
            (d, descs)
        })
        .collect();
    let shared = Arc::new(Shared {
        hass: hass.clone(),
        api: api.clone(),
        devices: Mutex::new(
            found
                .iter()
                .map(|(d, _)| (d.id.clone(), d.clone()))
                .collect(),
        ),
        alive: Mutex::new(true),
        last_command: Mutex::default(),
    });
    let ents: Vec<Arc<Ent>> = {
        // One entry at a time, and none removed meanwhile.
        let _one = crate::switchbot_cloud::SETUP.lock();
        if let Some(registry) = components.registry() {
            let disable_new = entry
                .get("pref_disable_new_entities")
                .and_then(Value::as_bool)
                == Some(true);
            register(&hass, registry, &entry_id, disable_new, &found);
        }
        let registries = components.registries();
        let names = crate::naming::Names::new(&hass, &registries, DOMAIN);
        let mut ents = Vec::new();
        for (d, descs) in &found {
            for x in descs {
                let unique_id = format!("tuya.{}{}", d.id, x.key);
                let Some((entity_id, friendly)) =
                    names.entity(x.domain, &unique_id, &d.id, &d.name, x.name)
                else {
                    continue;
                };
                let row = registries.entities.iter().find(|e| {
                    e.platform == DOMAIN
                        && e.entity_id.split('.').next() == Some(x.domain)
                        && e.unique_id.as_ref().and_then(Value::as_str) == Some(&unique_id)
                });
                ents.push(Arc::new(Ent {
                    shared: shared.clone(),
                    device: d.id.clone(),
                    entity_id,
                    friendly,
                    shown: shown(&hass, d, x, row.map(|r| &**r)),
                    d: x.clone(),
                }));
            }
        }
        for e in &ents {
            let a = || Arc::new(As(e.clone()));
            match e.d.domain {
                "switch" => components.switches().add(a()),
                "humidifier" => components.humidifiers().add(a()),
                "fan" => components.fans().add(a()),
                "number" => components.numbers().add(a()),
                "select" => components.selects().add(a()),
                _ => {}
            }
            e.write();
        }
        ents
    };
    tracing::info!("tuya: {} devices, {} entities", found.len(), ents.len());
    struct Stopped(Arc<Shared>);
    impl Drop for Stopped {
        fn drop(&mut self) {
            *self.0.alive.lock() = false;
        }
    }
    let _stopped = Stopped(shared.clone());
    status.set(EntryState::Loaded, None);
    drop(ready);
    // `refresh_mq`: only devices with an entity are subscribed.
    let subscribed: Vec<&Dev> = found
        .iter()
        .map(|(d, _)| d)
        .filter(|d| ents.iter().any(|e| e.device == d.id))
        .collect();
    if subscribed.is_empty() {
        return;
    }
    // `SharingMQ.run`: fetch credentials, connect, renew a minute before
    // they run out; back off on failures.
    let mut backoff = 1u64;
    loop {
        let link = format!(
            "tuya-device-sharing-sdk-python.{}",
            crate::switchbot_cloud::api::uuid4()
        );
        let cfg = match api
            .post("/v1.0/m/life/ha/access/config", json!({"linkId": link}))
            .await
        {
            Ok(c) => c["result"].clone(),
            Err(e) => {
                tracing::error!("tuya: mqtt config failed: {e}; retrying in {backoff} s");
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
                continue;
            }
        };
        let expire = cfg
            .get("expireTime")
            .and_then(Value::as_u64)
            .unwrap_or(7200);
        let owner = s(&cfg["topic"]["ownerId"], "sub");
        let dev_topic = s(&cfg["topic"]["devId"], "sub");
        let mut topics: Vec<String> = home_ids
            .iter()
            .map(|h| owner.replace("{ownerId}", h))
            .collect();
        for d in &subscribed {
            let t = dev_topic.replace("{devId}", &d.id);
            topics.push(format!(
                "{t}{}",
                if d.support_local { "/pen" } else { "/sta" }
            ));
        }
        let renew = tokio::time::sleep(Duration::from_secs(expire.saturating_sub(60).max(60)));
        let started = tokio::time::Instant::now();
        let result = mqtt::run(
            &s(&cfg, "url"),
            &s(&cfg, "clientId"),
            &s(&cfg, "username"),
            &s(&cfg, "password"),
            &topics,
            renew,
            |_topic, payload| {
                let Ok(msg) = serde_json::from_slice::<Value>(payload) else {
                    return;
                };
                let changed = on_message(&mut shared.devices.lock(), &msg);
                if let Some(id) = changed {
                    for e in ents.iter().filter(|e| e.device == id) {
                        e.write();
                    }
                }
            },
        )
        .await;
        match result {
            Ok(()) => backoff = 1,
            Err(e) => {
                tracing::warn!("tuya: mqtt link ended: {e}; reconnecting in {backoff} s");
                if started.elapsed() > Duration::from_secs(300) {
                    backoff = 1;
                }
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

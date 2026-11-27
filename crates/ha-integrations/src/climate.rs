//! The `climate` entity component (`components/climate`): its ten
//! services with HA's schemas, required features and validation, and the
//! attributes HA derives from a climate entity.
//!
//! Scope: the features an entity in the inventory uses (target
//! temperature, fan mode, turn on/off). Entities claiming the others are
//! told so (`unknown_error`) rather than half-served, and their
//! attributes aren't rendered. Temperatures are taken to be in the
//! system's unit (°C), so no conversion is done.

use crate::light::BoxFuture;
use crate::switch::select;
use ha_core::{
    Attributes, EntityId, EntityTarget, Hass, Kind, Range, Schema, ServiceError, SupportsResponse,
};
use parking_lot::RwLock;
use serde_json::{Map, Number, Value, json};
use std::sync::Arc;

pub const DOMAIN: &str = "climate";

/// `ClimateEntityFeature`.
pub mod feature {
    pub const TARGET_TEMPERATURE: u32 = 1;
    pub const TARGET_TEMPERATURE_RANGE: u32 = 2;
    pub const TARGET_HUMIDITY: u32 = 4;
    pub const FAN_MODE: u32 = 8;
    pub const PRESET_MODE: u32 = 16;
    pub const SWING_MODE: u32 = 32;
    pub const TURN_OFF: u32 = 128;
    pub const TURN_ON: u32 = 256;
    pub const SWING_HORIZONTAL_MODE: u32 = 512;
}

/// `HVACMode`'s values, in definition order (its coercion error lists them).
pub const HVAC_MODES: &[&str] = &[
    "off",
    "heat",
    "cool",
    "heat_cool",
    "auto",
    "dry",
    "fan_only",
];

/// HA's `DEFAULT_MIN_TEMP` / `DEFAULT_MAX_TEMP` (ints, in °C).
pub const DEFAULT_MIN_TEMP: i64 = 7;
pub const DEFAULT_MAX_TEMP: i64 = 35;

/// HA's `DEFAULT_MIN_HUMIDITY` / `DEFAULT_MAX_HUMIDITY` (`components/climate/const.py`).
pub const DEFAULT_MIN_HUMIDITY: i64 = 30;
pub const DEFAULT_MAX_HUMIDITY: i64 = 99;

/// A temperature as HA holds it: Python keeps ints and floats apart, and
/// so does the attribute (`21` against `21.0`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Temp {
    Int(i64),
    Float(f64),
}

impl Temp {
    pub fn get(self) -> f64 {
        match self {
            Temp::Int(i) => i as f64,
            Temp::Float(f) => f,
        }
    }

    /// Python's `str()`.
    pub fn py_str(self) -> String {
        match self {
            Temp::Int(i) => i.to_string(),
            Temp::Float(f) => crate::sun::py_float(f),
        }
    }
}

/// `PRECISION_WHOLE`, `PRECISION_HALVES` and `PRECISION_TENTHS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precision {
    Whole,
    Halves,
    Tenths,
}

/// `display_temp` (`helpers/temperature.py`), in the entity's own unit.
pub fn display_temp(t: Option<Temp>, precision: Precision) -> Value {
    let Some(t) = t else {
        return Value::Null;
    };
    let float = |f: f64| Number::from_f64(f).map_or(Value::Null, Value::Number);
    match (precision, t) {
        (Precision::Halves, _) => float(crate::num::round_int(t.get() * 2.0) as f64 / 2.0),
        // `round(x, 1)` keeps an int an int.
        (Precision::Tenths, Temp::Int(i)) => json!(i),
        (Precision::Tenths, Temp::Float(f)) => float(crate::num::round(f, 1)),
        (Precision::Whole, _) => json!(crate::num::round_int(t.get())),
    }
}

/// `climate.set_temperature`'s arguments, after validation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SetTemperature {
    pub temperature: Option<f64>,
    pub target_temp_low: Option<f64>,
    pub target_temp_high: Option<f64>,
    /// Passed through to the entity, as HA does.
    pub hvac_mode: Option<String>,
}

/// A climate entity a platform runs.
pub trait Climate: Send + Sync {
    fn entity_id(&self) -> &EntityId;
    fn available(&self) -> bool;
    fn supported_features(&self) -> u32;
    /// The state; `None` is `unknown`.
    fn hvac_mode(&self) -> Option<String>;
    fn hvac_modes(&self) -> &[&'static str];
    fn hvac_modes_vec(&self) -> Vec<String> {
        self.hvac_modes().iter().map(|s| (*s).to_owned()).collect()
    }
    fn fan_mode(&self) -> Option<String> {
        None
    }
    fn fan_modes(&self) -> &[&'static str] {
        &[]
    }
    fn fan_modes_vec(&self) -> Vec<String> {
        self.fan_modes().iter().map(|s| (*s).to_owned()).collect()
    }
    fn preset_modes_vec(&self) -> Vec<String> {
        Vec::new()
    }
    fn swing_modes_vec(&self) -> Vec<String> {
        Vec::new()
    }
    fn swing_horizontal_modes_vec(&self) -> Vec<String> {
        Vec::new()
    }
    fn precision(&self) -> Precision {
        // HA's default for a system in °C.
        Precision::Tenths
    }
    fn min_temp(&self) -> Temp {
        Temp::Int(DEFAULT_MIN_TEMP)
    }
    fn max_temp(&self) -> Temp {
        Temp::Int(DEFAULT_MAX_TEMP)
    }
    /// Atomic snapshot of `(min_temp, max_temp)`.
    fn temp_bounds(&self) -> (Temp, Temp) {
        (self.min_temp(), self.max_temp())
    }
    /// Atomic snapshot of `(min_humidity, max_humidity)`.
    fn humidity_bounds(&self) -> (Temp, Temp) {
        (
            Temp::Int(DEFAULT_MIN_HUMIDITY),
            Temp::Int(DEFAULT_MAX_HUMIDITY),
        )
    }
    fn target_temperature_step(&self) -> Option<Temp> {
        None
    }
    fn current_temperature(&self) -> Option<Temp> {
        None
    }
    fn target_temperature(&self) -> Option<Temp> {
        None
    }
    fn hvac_action(&self) -> Option<&str> {
        None
    }

    fn set_hvac_mode(&self, mode: &str) -> BoxFuture<'_>;
    fn set_fan_mode(&self, _mode: &str) -> BoxFuture<'_> {
        unsupported()
    }
    fn set_preset_mode(&self, _mode: &str) -> BoxFuture<'_> {
        unsupported()
    }
    fn set_humidity(&self, _humidity: i64) -> BoxFuture<'_> {
        unsupported()
    }
    fn set_swing_mode(&self, _mode: &str) -> BoxFuture<'_> {
        unsupported()
    }
    fn set_swing_horizontal_mode(&self, _mode: &str) -> BoxFuture<'_> {
        unsupported()
    }
    fn set_temperature(&self, _args: SetTemperature) -> BoxFuture<'_> {
        unsupported()
    }
    /// The base `async_turn_on`: the only other mode when there are two
    /// and one is `off`, else the first of heat_cool, heat, cool.
    fn turn_on(&self) -> BoxFuture<'_> {
        let modes = self.hvac_modes_vec();
        let has_off = modes.iter().any(|m| m == "off");
        let only = (modes.len() == 2 && has_off)
            .then(|| modes.iter().find(|m| m.as_str() != "off"))
            .flatten()
            .cloned();
        let fake = || {
            ["heat_cool", "heat", "cool"]
                .into_iter()
                .find(|m| modes.iter().any(|x| x == m))
                .map(str::to_owned)
        };
        match only.or_else(fake) {
            Some(mode) => Box::pin(async move { self.set_hvac_mode(&mode).await }),
            None => unsupported(),
        }
    }
    /// The base `async_turn_off`: `off`, if it's a mode.
    fn turn_off(&self) -> BoxFuture<'_> {
        match self.hvac_modes_vec().iter().any(|m| m == "off") {
            true => self.set_hvac_mode("off"),
            false => unsupported(),
        }
    }
    /// The base `async_toggle`: `off` -> `turn_on`, anything else -> `turn_off`.
    /// Platforms and plugins may override this to handle `climate.toggle` directly.
    fn toggle(&self) -> BoxFuture<'_> {
        if self.hvac_mode().as_deref() == Some("off") {
            self.turn_on()
        } else {
            self.turn_off()
        }
    }
}

/// A method the entity doesn't implement (Python's `NotImplementedError`).
fn unsupported<'a>() -> BoxFuture<'a> {
    Box::pin(async { Err(ServiceError::Unknown(String::new())) })
}

/// The entity's `capability_attributes` and `state_attributes`, for the
/// features in scope. The platform adds the standard ones.
pub fn attributes(c: &dyn Climate) -> Attributes {
    let features = c.supported_features();
    let precision = c.precision();
    let mut a = Map::new();
    a.insert("hvac_modes".into(), json!(c.hvac_modes()));
    a.insert(
        "min_temp".into(),
        display_temp(Some(c.min_temp()), precision),
    );
    a.insert(
        "max_temp".into(),
        display_temp(Some(c.max_temp()), precision),
    );
    if let Some(step) = c.target_temperature_step().filter(|s| s.get() != 0.0) {
        a.insert(
            "target_temp_step".into(),
            match step {
                Temp::Int(i) => json!(i),
                Temp::Float(f) => json!(f),
            },
        );
    }
    if features & feature::FAN_MODE != 0 {
        a.insert("fan_modes".into(), json!(c.fan_modes()));
    }
    a.insert(
        "current_temperature".into(),
        display_temp(c.current_temperature(), precision),
    );
    if features & feature::TARGET_TEMPERATURE != 0 {
        a.insert(
            "temperature".into(),
            display_temp(c.target_temperature(), precision),
        );
    }
    if features & feature::FAN_MODE != 0 {
        a.insert("fan_mode".into(), json!(c.fan_mode()));
    }
    if let Some(action) = c.hvac_action() {
        a.insert("hvac_action".into(), json!(action));
    }
    a
}

/// `_valid_mode_or_raise`; `label` names the mode in the message (`HVAC`,
/// `fan`, `preset`, `swing`, or `horizontal swing`).
fn valid_mode(kind: &str, label: &str, mode: &str, modes: &[String]) -> Result<(), ServiceError> {
    if modes.iter().any(|m| m == mode) {
        return Ok(());
    }
    let modes = modes.join(", ");
    let mut p = Map::new();
    p.insert("mode".into(), json!(mode));
    p.insert("modes".into(), json!(modes));
    Err(ServiceError::validation(
        DOMAIN,
        &format!("not_valid_{kind}_mode"),
        Some(p),
        format!(
            "{}{} mode {mode} is not valid. Valid {label} modes are: {modes}",
            label[..1].to_ascii_uppercase(),
            &label[1..]
        ),
    ))
}

/// `async_service_humidity_set`'s range check, then the entity's call.
async fn set_humidity(c: &dyn Climate, data: &Map<String, Value>) -> Result<(), ServiceError> {
    let (min, max) = c.humidity_bounds();
    let val = data.get("humidity");
    let (h_f, shown, parsed_i64): (f64, String, Option<i64>) = match val {
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                (i as f64, i.to_string(), Some(i))
            } else if let Some(u) = n.as_u64() {
                (u as f64, u.to_string(), i64::try_from(u).ok())
            } else {
                (0.0, "0".into(), Some(0))
            }
        }
        Some(Value::String(s)) => {
            // Canonical base-10 integer string outside 64-bit JSON range (from `Kind::BigInt`).
            let f = s.parse::<f64>().unwrap_or(if s.starts_with('-') {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            });
            (f, s.clone(), None)
        }
        _ => (0.0, "0".into(), Some(0)),
    };
    if h_f < min.get() || h_f > max.get() {
        let mut p = Map::new();
        p.insert("humidity".into(), json!(shown));
        p.insert("min_humidity".into(), json!(min.py_str()));
        p.insert("max_humidity".into(), json!(max.py_str()));
        return Err(ServiceError::validation(
            DOMAIN,
            "humidity_out_of_range",
            Some(p),
            format!(
                "Provided humidity {shown} is not valid. Accepted range is {} to {}",
                min.py_str(),
                max.py_str()
            ),
        ));
    }
    let humidity = parsed_i64.unwrap_or(i64::MAX);
    c.set_humidity(humidity).await
}

/// `async_service_temperature_set`'s checks, then the entity's call.
async fn set_temperature(c: &dyn Climate, data: &Map<String, Value>) -> Result<(), ServiceError> {
    let features = c.supported_features();
    let get = |k: &str| data.get(k).and_then(Value::as_f64);
    let invalid =
        |key: &str, message: &str| Err(ServiceError::validation(DOMAIN, key, None, message.into()));
    if data.contains_key("temperature") && features & feature::TARGET_TEMPERATURE == 0 {
        return invalid(
            "missing_target_temperature_entity_feature",
            "Set temperature action was used with the 'Target temperature' parameter but the entity does not support it",
        );
    }
    if data.contains_key("target_temp_low") && features & feature::TARGET_TEMPERATURE_RANGE == 0 {
        return invalid(
            "missing_target_temperature_range_entity_feature",
            "Set temperature action was used with the 'Lower/Upper target temperature' parameter but the entity does not support it",
        );
    }
    if let (Some(low), Some(high)) = (get("target_temp_low"), get("target_temp_high"))
        && low != 0.0
        && high != 0.0
        && low > high
    {
        return invalid(
            "low_temp_higher_than_high_temp",
            "'Lower target temperature' cannot be higher than 'Upper target temperature'",
        );
    }
    let (min, max) = c.temp_bounds();
    // HA checks in the caller's key order; the data here is in key order
    // (serde_json's map), so with two out of range the one named can
    // differ.
    for key in ["target_temp_high", "target_temp_low", "temperature"] {
        let Some(t) = get(key) else { continue };
        if t < min.get() || t > max.get() {
            let mut p = Map::new();
            p.insert("check_temp".into(), json!(crate::sun::py_float(t)));
            p.insert("min_temp".into(), json!(min.py_str()));
            p.insert("max_temp".into(), json!(max.py_str()));
            return Err(ServiceError::validation(
                DOMAIN,
                "temp_out_of_range",
                Some(p),
                format!(
                    "Provided temperature {} is not valid. Accepted range is {} to {}",
                    crate::sun::py_float(t),
                    min.py_str(),
                    max.py_str()
                ),
            ));
        }
    }
    c.set_temperature(SetTemperature {
        temperature: get("temperature"),
        target_temp_low: get("target_temp_low"),
        target_temp_high: get("target_temp_high"),
        hvac_mode: data
            .get("hvac_mode")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
    .await
}

#[derive(Default)]
pub struct Climates {
    items: RwLock<Vec<Arc<dyn Climate>>>,
}

/// HA's entity-service selection with required features: a targeted entity
/// lacking every feature set is skipped, or, when named directly, refused
/// with `ServiceNotSupported`.
fn chosen(
    items: &[Arc<dyn Climate>],
    target: &EntityTarget,
    indirect: &[String],
    required: &[u32],
    service: &str,
) -> Result<Vec<Arc<dyn Climate>>, ServiceError> {
    let mut out = Vec::new();
    for c in select(
        items,
        |c| c.entity_id(),
        |c| c.available(),
        target,
        indirect,
    ) {
        let f = c.supported_features();
        if !required.is_empty() && !required.iter().any(|r| f & r == *r) {
            if let EntityTarget::Ids(ids) = target
                && ids.contains(c.entity_id())
            {
                return Err(ServiceError::not_supported(
                    DOMAIN,
                    service,
                    c.entity_id().as_str(),
                ));
            }
            continue;
        }
        out.push(c);
    }
    Ok(out)
}

fn dispatch_one<'a>(
    c: &'a dyn Climate,
    name: &'static str,
    data: &'a Map<String, Value>,
) -> BoxFuture<'a> {
    let str_of = |k: &str| data.get(k).and_then(Value::as_str).unwrap_or("");
    match name {
        "turn_on" => c.turn_on(),
        "turn_off" => c.turn_off(),
        "toggle" => c.toggle(),
        "set_hvac_mode" => {
            let mode = str_of("hvac_mode");
            if let Err(e) = valid_mode("hvac", "HVAC", mode, &c.hvac_modes_vec()) {
                return Box::pin(async move { Err(e) });
            }
            c.set_hvac_mode(mode)
        }
        "set_fan_mode" => {
            let mode = str_of("fan_mode");
            if let Err(e) = valid_mode("fan", "fan", mode, &c.fan_modes_vec()) {
                return Box::pin(async move { Err(e) });
            }
            c.set_fan_mode(mode)
        }
        "set_preset_mode" => {
            let mode = str_of("preset_mode");
            if let Err(e) = valid_mode("preset", "preset", mode, &c.preset_modes_vec()) {
                return Box::pin(async move { Err(e) });
            }
            c.set_preset_mode(mode)
        }
        "set_swing_mode" => {
            let mode = str_of("swing_mode");
            if let Err(e) = valid_mode("swing", "swing", mode, &c.swing_modes_vec()) {
                return Box::pin(async move { Err(e) });
            }
            c.set_swing_mode(mode)
        }
        "set_swing_horizontal_mode" => {
            let mode = str_of("swing_horizontal_mode");
            if let Err(e) = valid_mode(
                "horizontal_swing",
                "horizontal swing",
                mode,
                &c.swing_horizontal_modes_vec(),
            ) {
                return Box::pin(async move { Err(e) });
            }
            c.set_swing_horizontal_mode(mode)
        }
        "set_humidity" => Box::pin(set_humidity(c, data)),
        "set_temperature" => Box::pin(set_temperature(c, data)),
        _ => Box::pin(async { Ok(()) }),
    }
}

impl Climates {
    pub fn add(&self, c: Arc<dyn Climate>) {
        let mut items = self.items.write();
        items.retain(|existing| existing.entity_id() != c.entity_id());
        items.push(c);
    }

    pub fn remove(&self, entity_id: &EntityId) {
        self.items.write().retain(|c| c.entity_id() != entity_id);
    }

    /// Register the ten `climate.*` services, in HA's order.
    pub fn register_services(
        self: &Arc<Self>,
        hass: &Hass,
        registries: impl Into<ha_registry::Lookup>,
    ) {
        let registries = registries.into();
        use feature::*;
        let entity = Schema::entity_service;
        let services: [(&'static str, Schema, &'static [u32]); 10] = [
            ("turn_on", entity(), &[TURN_ON]),
            ("turn_off", entity(), &[TURN_OFF]),
            ("toggle", entity(), &[TURN_OFF, TURN_ON]),
            (
                "set_hvac_mode",
                entity().required("hvac_mode", Kind::Enum("HVACMode", HVAC_MODES)),
                &[],
            ),
            (
                "set_preset_mode",
                entity().required("preset_mode", Kind::String),
                &[PRESET_MODE],
            ),
            (
                "set_temperature",
                entity()
                    .at_least_one(&["temperature", "target_temp_high", "target_temp_low"])
                    .optional("temperature", Kind::Float(Range::ANY))
                    .optional("target_temp_high", Kind::Float(Range::ANY))
                    .optional("target_temp_low", Kind::Float(Range::ANY))
                    .optional("hvac_mode", Kind::Enum("HVACMode", HVAC_MODES))
                    .inclusive("temperature", &["target_temp_high", "target_temp_low"]),
                &[TARGET_TEMPERATURE, TARGET_TEMPERATURE_RANGE],
            ),
            (
                "set_humidity",
                entity().required("humidity", Kind::BigInt),
                &[TARGET_HUMIDITY],
            ),
            (
                "set_fan_mode",
                entity().required("fan_mode", Kind::String),
                &[FAN_MODE],
            ),
            (
                "set_swing_mode",
                entity().required("swing_mode", Kind::String),
                &[SWING_MODE],
            ),
            (
                "set_swing_horizontal_mode",
                entity().required("swing_horizontal_mode", Kind::String),
                &[SWING_HORIZONTAL_MODE],
            ),
        ];
        for (name, schema, required) in services {
            let climates = self.clone();
            let registries = registries.clone();
            hass.services().register_with_schema(
                DOMAIN,
                name,
                Some(schema),
                SupportsResponse::None,
                move |call| {
                    let climates = climates.clone();
                    let registries = registries.clone();
                    async move {
                        let target = call.entity_ids()?;
                        let indirect = registries.get().target_entities(&call.data);
                        let chosen =
                            chosen(&climates.items.read(), &target, &indirect, required, name)?;
                        let data = &call.data;
                        if let [only] = chosen.as_slice() {
                            dispatch_one(&**only, name, data).await?;
                        } else if !chosen.is_empty() {
                            let mut first_err: Option<ServiceError> = None;
                            for res in crate::light::join_all(
                                chosen.iter().map(|c| dispatch_one(&**c, name, data)),
                            )
                            .await
                            {
                                if let Err(e) = res
                                    && first_err.is_none()
                                {
                                    first_err = Some(e);
                                }
                            }
                            if let Some(e) = first_err {
                                return Err(e);
                            }
                        }
                        Ok(None)
                    }
                },
            );
        }
    }
}

#[cfg(test)]
mod tests;

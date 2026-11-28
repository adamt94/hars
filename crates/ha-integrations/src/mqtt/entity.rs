//! A discovered `sensor` or `binary_sensor`: its configuration, checked as
//! HA's discovery schemas check it (`schemas.py`, `config.py`, the
//! platforms' `DISCOVERY_SCHEMA`), and what it does with a message
//! (`entity.py`'s availability and JSON attributes mixins, `sensor.py`,
//! `binary_sensor.py`) and how its state is written (the `sensor` and
//! `binary_sensor` components and `helpers/entity.py`).
//!
//! Checked against what HA 2026.6.1 made of Zigbee2MQTT's, Tasmota's and
//! a device-based discovery message (`vectors/v5.json`).

use super::tables;
use ha_core::template::Template;
use serde_json::{Map, Value};
use std::sync::Arc;

/// The platforms `hars` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    Sensor,
    BinarySensor,
}

impl Domain {
    pub fn of(component: &str) -> Option<Domain> {
        match component {
            "sensor" => Some(Domain::Sensor),
            "binary_sensor" => Some(Domain::BinarySensor),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Domain::Sensor => "sensor",
            Domain::BinarySensor => "binary_sensor",
        }
    }

    /// `DEFAULT_NAME`.
    fn default_name(self) -> &'static str {
        match self {
            Domain::Sensor => "MQTT Sensor",
            Domain::BinarySensor => "MQTT Binary sensor",
        }
    }
}

/// `availability_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Latest,
    All,
    Any,
}

/// One availability topic.
#[derive(Clone)]
pub struct Availability {
    pub topic: String,
    pub available: String,
    pub not_available: String,
    pub template: Option<Arc<Template>>,
}

/// The `device` block (`MQTT_ENTITY_DEVICE_INFO_SCHEMA`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Device {
    pub identifiers: Vec<String>,
    pub connections: Vec<(String, String)>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub model_id: Option<String>,
    pub name: Option<String>,
    pub hw_version: Option<String>,
    pub serial_number: Option<String>,
    pub sw_version: Option<String>,
    pub via_device: Option<String>,
    pub suggested_area: Option<String>,
    pub configuration_url: Option<String>,
}

/// `name`: not given, given as `null`, or a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Name {
    Unset,
    Null,
    Some(String),
}

/// What each platform has of its own.
#[derive(Debug, Clone)]
pub enum Platform {
    Sensor {
        device_class: Option<String>,
        state_class: Option<String>,
        unit: Option<String>,
        precision: Option<u64>,
        options: Option<Vec<String>>,
    },
    BinarySensor {
        device_class: Option<String>,
        payload_on: String,
        payload_off: String,
        off_delay: Option<u64>,
    },
}

/// A discovered entity's configuration.
#[derive(Clone)]
pub struct Config {
    pub domain: Domain,
    pub name: Name,
    pub unique_id: Option<String>,
    pub default_entity_id: Option<String>,
    pub device: Option<Device>,
    pub availability: Vec<Availability>,
    pub mode: Mode,
    pub attributes_topic: Option<String>,
    pub attributes_template: Option<Arc<Template>>,
    pub enabled_by_default: bool,
    pub entity_category: Option<String>,
    pub icon: Option<String>,
    pub entity_picture: Option<String>,
    pub qos: u8,
    pub encoding: String,
    pub state_topic: String,
    pub value_template: Option<Arc<Template>>,
    pub expire_after: Option<u64>,
    pub force_update: bool,
    pub platform: Platform,
}

/// `cv.string`: a string, or a number or boolean as Python's `str()`.
pub fn cv_string(v: &Value) -> Result<String, String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Bool(true) => Ok("True".into()),
        Value::Bool(false) => Ok("False".into()),
        Value::Number(n) => Ok(match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => i.to_string(),
            (_, Some(u), _) => u.to_string(),
            (_, _, Some(f)) => ha_core::py::repr_float(f),
            _ => n.to_string(),
        }),
        Value::Null => Err("string value is None".into()),
        _ => Err("expected str".into()),
    }
}

/// `cv.boolean`.
fn cv_boolean(v: &Value) -> Result<bool, String> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::Number(n) => Ok(n.as_f64() != Some(0.0)),
        Value::String(s) => match s.to_lowercase().as_str() {
            "1" | "true" | "yes" | "on" | "enable" => Ok(true),
            "0" | "false" | "no" | "off" | "disable" => Ok(false),
            _ => Err(format!("invalid boolean value {s}")),
        },
        _ => Err(format!("invalid boolean value {v}")),
    }
}

/// `cv.positive_int`.
fn positive_int(v: &Value) -> Result<u64, String> {
    match v {
        Value::Number(n) if n.as_u64().is_some() => Ok(n.as_u64().unwrap_or_default()),
        Value::Number(n) if n.as_f64().is_some_and(|f| f >= 0.0 && f.fract() == 0.0) => {
            Ok(n.as_f64().unwrap_or_default() as u64)
        }
        Value::String(s) => s
            .trim()
            .parse::<u64>()
            .map_err(|_| "expected int".to_owned()),
        Value::Number(_) => Err("value must be at least 0".into()),
        _ => Err("expected int".into()),
    }
}

/// `cv.ensure_list`.
fn list(v: &Value) -> Vec<&Value> {
    match v {
        Value::Array(items) => items.iter().collect(),
        Value::Null => Vec::new(),
        other => vec![other],
    }
}

/// `valid_subscribe_topic`.
pub fn valid_subscribe_topic(topic: &str) -> Result<(), String> {
    valid_topic(topic)?;
    let bytes = topic.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        if c == b'+'
            && ((i > 0 && bytes[i - 1] != b'/') || (i + 1 < bytes.len() && bytes[i + 1] != b'/'))
        {
            return Err("Single-level wildcard must occupy an entire level of the filter".into());
        }
    }
    if let Some(i) = topic.find('#') {
        if i != topic.len() - 1 {
            return Err(
                "Multi-level wildcard must be the last character in the topic filter.".into(),
            );
        }
        if topic.len() > 1 && bytes[i - 1] != b'/' {
            return Err("Multi-level wildcard must be after a topic level separator.".into());
        }
    }
    Ok(())
}

/// `valid_topic`.
pub fn valid_topic(topic: &str) -> Result<(), String> {
    if topic.is_empty() {
        return Err("MQTT topic name/filter must not be empty.".into());
    }
    if topic.len() > 65535 {
        return Err("MQTT topic name/filter must not be longer than 65535 encoded bytes.".into());
    }
    for c in topic.chars() {
        if c == '\0' {
            return Err("MQTT topic name/filter must not contain null character.".into());
        }
        if c <= '\u{1f}' || ('\u{7f}'..='\u{9f}').contains(&c) {
            return Err("MQTT topic name/filter must not contain control characters.".into());
        }
        if ('\u{fdd0}'..='\u{fdef}').contains(&c)
            || matches!(u32::from(c) & 0xFFFF, 0xFFFE | 0xFFFF)
        {
            return Err("MQTT topic name/filter must not contain non-characters.".into());
        }
    }
    Ok(())
}

/// `valid_publish_topic`.
pub fn valid_publish_topic(topic: &str) -> Result<(), String> {
    valid_topic(topic)?;
    if topic.contains(['+', '#']) {
        return Err("Wildcards cannot be used in topic names".into());
    }
    Ok(())
}

/// `cv.template`: compiled now, so a template that doesn't is a schema
/// error, as HA's `ensure_valid`.
fn template(v: &Value) -> Result<Arc<Template>, String> {
    let source = cv_string(v).map_err(|_| "template value should be a string".to_owned())?;
    let t = Template::new(&source);
    if !t.is_static() {
        t.compile(false)
            .map_err(|e| format!("invalid template ({e})"))?;
    }
    Ok(Arc::new(t))
}

/// One error, as voluptuous names where it is.
fn at(key: &str, why: String) -> String {
    format!("{why} for dictionary value @ data['{key}']")
}

fn device(v: &Value) -> Result<Device, String> {
    let Value::Object(m) = v else {
        return Err("expected a dictionary".into());
    };
    let s = |k: &str| -> Result<Option<String>, String> {
        m.get(k)
            .map(|v| cv_string(v).map_err(|e| format!("{e} @ data['{k}']")))
            .transpose()
    };
    let mut d = Device {
        identifiers: m
            .get("identifiers")
            .map(list)
            .unwrap_or_default()
            .into_iter()
            .map(cv_string)
            .collect::<Result<_, _>>()?,
        manufacturer: s("manufacturer")?,
        model: s("model")?,
        model_id: s("model_id")?,
        name: s("name")?,
        hw_version: s("hw_version")?,
        serial_number: s("serial_number")?,
        sw_version: s("sw_version")?,
        // `cv.deprecated(via_hub, via_device)`.
        via_device: s("via_device")?.or(s("via_hub")?),
        suggested_area: s("suggested_area")?,
        configuration_url: s("configuration_url")?,
        connections: Vec::new(),
    };
    for c in m.get("connections").map(list).unwrap_or_default() {
        let pair = list(c);
        if pair.len() != 2 {
            return Err("length of value must be at least 2 @ data['connections']".into());
        }
        d.connections
            .push((cv_string(pair[0])?, cv_string(pair[1])?));
    }
    if let Some(url) = &d.configuration_url
        && !(url.starts_with("http://")
            || url.starts_with("https://")
            || url.starts_with("homeassistant://"))
    {
        return Err("invalid url @ data['configuration_url']".into());
    }
    if d.identifiers.is_empty() && d.connections.is_empty() {
        return Err(
            "Device must have at least one identifying value in 'identifiers' and/or \
                    'connections'"
                .into(),
        );
    }
    Ok(d)
}

/// The class a platform names, lower-cased and checked (`vol.Lower`,
/// `vol.Coerce(SensorDeviceClass)`), or `None` (`vol.Any(…, None)`).
fn device_class(v: Option<&Value>, classes: &[&str]) -> Result<Option<String>, String> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let s = cv_string(v)?.to_lowercase();
            match classes.binary_search(&s.as_str()) {
                Ok(_) => Ok(Some(s)),
                Err(_) => Err(at(
                    "device_class",
                    format!("expected {s} to be a valid class"),
                )),
            }
        }
    }
}

impl Config {
    /// A discovery payload (abbreviations expanded) for `domain`, as its
    /// `DISCOVERY_SCHEMA` takes it. Keys the schema doesn't know are
    /// dropped (`vol.REMOVE_EXTRA`).
    pub fn parse(domain: Domain, p: &Map<String, Value>) -> Result<Config, String> {
        let get = |k: &str| p.get(k);
        let opt_str = |k: &str| -> Result<Option<String>, String> {
            get(k)
                .map(|v| cv_string(v).map_err(|e| at(k, e)))
                .transpose()
        };
        let opt_tpl = |k: &str| -> Result<Option<Arc<Template>>, String> {
            get(k)
                .map(|v| template(v).map_err(|e| at(k, e)))
                .transpose()
        };
        let bool_or = |k: &str, default: bool| -> Result<bool, String> {
            get(k).map_or(Ok(default), |v| cv_boolean(v).map_err(|e| at(k, e)))
        };
        let opt_int = |k: &str| -> Result<Option<u64>, String> {
            get(k)
                .map(|v| positive_int(v).map_err(|e| at(k, e)))
                .transpose()
        };
        let qos = match get("qos") {
            None => 0,
            Some(v) => match v
                .as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            {
                Some(q @ 0..=2) => q as u8,
                _ => return Err(at("qos", "value must be one of [0, 1, 2]".into())),
            },
        };
        let state_topic = match get("state_topic") {
            None => return Err("required key not provided @ data['state_topic']".into()),
            Some(v) => cv_string(v).map_err(|e| at("state_topic", e))?,
        };
        valid_subscribe_topic(&state_topic).map_err(|e| at("state_topic", e))?;
        // `availability_topic` or an `availability` list, not both.
        if get("availability_topic").is_some() && get("availability").is_some() {
            return Err(
                "two or more values in the same group of exclusion 'availability' \
                        @ data[<availability>]"
                    .into(),
            );
        }
        let mut availability = Vec::new();
        if let Some(topic) = opt_str("availability_topic")? {
            valid_subscribe_topic(&topic).map_err(|e| at("availability_topic", e))?;
            availability.push(Availability {
                topic,
                available: opt_str("payload_available")?.unwrap_or_else(|| "online".into()),
                not_available: opt_str("payload_not_available")?
                    .unwrap_or_else(|| "offline".into()),
                template: opt_tpl("availability_template")?,
            });
        }
        if let Some(v) = get("availability") {
            for item in list(v) {
                let Value::Object(a) = item else {
                    return Err(at("availability", "expected a dictionary".into()));
                };
                let s = |k: &str| a.get(k).map(cv_string).transpose();
                let Some(topic) = s("topic")? else {
                    return Err(
                        "required key not provided @ data['availability'][0]['topic']".into(),
                    );
                };
                valid_subscribe_topic(&topic).map_err(|e| at("availability", e))?;
                // A later entry for a topic replaces an earlier one, as
                // HA's dict by topic.
                availability.retain(|x: &Availability| x.topic != topic);
                availability.push(Availability {
                    topic,
                    available: s("payload_available")?.unwrap_or_else(|| "online".into()),
                    not_available: s("payload_not_available")?.unwrap_or_else(|| "offline".into()),
                    template: a.get("value_template").map(template).transpose()?,
                });
            }
        }
        let mode = match opt_str("availability_mode")?.as_deref() {
            None | Some("latest") => Mode::Latest,
            Some("all") => Mode::All,
            Some("any") => Mode::Any,
            Some(other) => {
                return Err(at(
                    "availability_mode",
                    format!("value must be one of ['all', 'any', 'latest'], got {other}"),
                ));
            }
        };
        let attributes_topic = opt_str("json_attributes_topic")?;
        if let Some(t) = &attributes_topic {
            valid_subscribe_topic(t).map_err(|e| at("json_attributes_topic", e))?;
        }
        let entity_category = opt_str("entity_category")?;
        if let Some(c) = &entity_category
            && c != "config"
            && c != "diagnostic"
        {
            return Err(at(
                "entity_category",
                "expected EntityCategory or one of 'config', 'diagnostic'".into(),
            ));
        }
        let icon = opt_str("icon")?;
        if let Some(i) = &icon
            && !i.contains(':')
        {
            return Err(at(
                "icon",
                "Icons should be specified in the form \"prefix:name\"".into(),
            ));
        }
        let name = match get("name") {
            None => Name::Unset,
            Some(Value::Null) => Name::Null,
            Some(v) => Name::Some(cv_string(v).map_err(|e| at("name", e))?),
        };
        let platform = match domain {
            Domain::Sensor => sensor(p)?,
            Domain::BinarySensor => Platform::BinarySensor {
                device_class: device_class(get("device_class"), tables::BINARY_SENSOR_CLASSES)?,
                payload_on: opt_str("payload_on")?.unwrap_or_else(|| "ON".into()),
                payload_off: opt_str("payload_off")?.unwrap_or_else(|| "OFF".into()),
                off_delay: opt_int("off_delay")?,
            },
        };
        Ok(Config {
            domain,
            name,
            unique_id: opt_str("unique_id")?,
            default_entity_id: opt_str("default_entity_id")?,
            device: get("device")
                .map(|d| {
                    device(d).map_err(|e| format!("{e} for dictionary value @ data['device']"))
                })
                .transpose()?,
            availability,
            mode,
            attributes_topic,
            attributes_template: opt_tpl("json_attributes_template")?,
            enabled_by_default: bool_or("enabled_by_default", true)?,
            entity_category,
            icon,
            entity_picture: opt_str("entity_picture")?,
            qos,
            encoding: opt_str("encoding")?.unwrap_or_else(|| "utf-8".into()),
            state_topic,
            value_template: opt_tpl("value_template")?,
            expire_after: opt_int("expire_after")?,
            force_update: bool_or("force_update", false)?,
            platform,
        })
    }

    pub fn device_class(&self) -> Option<&str> {
        match &self.platform {
            Platform::Sensor { device_class, .. } | Platform::BinarySensor { device_class, .. } => {
                device_class.as_deref()
            }
        }
    }

    /// `Entity.name`: the configured name (`None` for `name: null`), else
    /// the device class's name, else the platform's default.
    pub fn entity_name(&self) -> Option<String> {
        match &self.name {
            Name::Some(n) => Some(n.clone()),
            Name::Null => None,
            Name::Unset => {
                let table = match self.domain {
                    Domain::Sensor => tables::SENSOR_NAMES,
                    Domain::BinarySensor => tables::BINARY_SENSOR_NAMES,
                };
                // `_default_to_device_class_name`: a sensor's `enum` class
                // doesn't name it.
                match self.device_class().filter(|dc| *dc != "enum") {
                    Some(dc) => table
                        .binary_search_by(|(k, _)| k.cmp(&dc))
                        .ok()
                        .map(|i| table[i].1.to_owned()),
                    None => Some(self.domain.default_name().to_owned()),
                }
            }
        }
    }

    /// `capability_attributes`: a sensor's state class and options.
    pub fn capabilities(&self) -> Option<Map<String, Value>> {
        let Platform::Sensor {
            state_class,
            options,
            ..
        } = &self.platform
        else {
            return None;
        };
        let mut m = Map::new();
        if let Some(sc) = state_class {
            m.insert("state_class".into(), Value::from(sc.as_str()));
        }
        if let Some(o) = options {
            m.insert("options".into(), Value::from(o.clone()));
        }
        (!m.is_empty()).then_some(m)
    }
}

/// `sensor.py`'s schema and `validate_sensor_state_and_device_class_config`.
fn sensor(p: &Map<String, Value>) -> Result<Platform, String> {
    let device_class = device_class(p.get("device_class"), tables::SENSOR_CLASSES)?;
    let state_class = match p.get("state_class") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let s = cv_string(v)
                .map_err(|e| at("state_class", e))?
                .to_lowercase();
            if tables::STATE_CLASSES.binary_search(&s.as_str()).is_err() {
                return Err(at(
                    "state_class",
                    format!("expected {s} to be a valid class"),
                ));
            }
            Some(s)
        }
    };
    if p.contains_key("last_reset_value_template") && state_class.as_deref() != Some("total") {
        return Err(format!(
            "The option `last_reset_value_template` cannot be used together with state class \
             `{}`",
            state_class.as_deref().unwrap_or("None")
        ));
    }
    let mut unit = match p.get("unit_of_measurement") {
        None | Some(Value::Null) => None,
        Some(v) => Some(cv_string(v).map_err(|e| at("unit_of_measurement", e))?),
    };
    if unit.as_deref().is_some_and(|u| u.trim().is_empty()) {
        unit = None;
    }
    let options = match p.get("options") {
        None => None,
        Some(v) => Some(
            list(v)
                .into_iter()
                .map(cv_string)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| at("options", e))?,
        ),
    };
    if let Some(o) = &options {
        if o.is_empty() {
            return Err("An empty options list is not allowed".into());
        }
        if state_class.is_some() || unit.is_some() {
            return Err(
                "Specifying `options` is not allowed together with the `state_class` or \
                        `unit_of_measurement` option"
                    .into(),
            );
        }
        if device_class.as_deref() != Some("enum") {
            return Err(format!(
                "The option `options` must be used together with device class `enum`, got \
                 `device_class` '{}'",
                device_class.as_deref().unwrap_or("None")
            ));
        }
    }
    if let Some(sc) = &state_class
        && let Some((_, units)) = tables::STATE_CLASS_UNITS.iter().find(|(c, _)| c == sc)
        && !units.contains(&unit.as_deref().unwrap_or(""))
    {
        return Err(format!(
            "The unit of measurement '{}' is not valid together with state class '{sc}'",
            unit.as_deref().unwrap_or("None")
        ));
    }
    if let Some(u) = &unit {
        let mapped = tables::AMBIGUOUS_UNITS
            .iter()
            .find(|(from, _)| from == u)
            .map(|(_, to)| (*to).to_owned());
        if let Some(m) = mapped {
            unit = Some(m);
        }
    }
    if let (Some(u), Some(dc)) = (&unit, &device_class)
        && let Some((_, units)) = tables::DEVICE_CLASS_UNITS.iter().find(|(c, _)| c == dc)
        && !units.contains(&u.as_str())
    {
        return Err(format!(
            "The unit of measurement `{u}` is not valid together with device class `{dc}`"
        ));
    }
    let precision = match p.get("suggested_display_precision") {
        None => None,
        Some(v) => Some(positive_int(v).map_err(|e| at("suggested_display_precision", e))?),
    };
    Ok(Platform::Sensor {
        device_class,
        state_class,
        unit,
        precision,
        options,
    })
}

/// `_numeric_state_expected`.
pub fn numeric_expected(
    device_class: Option<&str>,
    state_class: Option<&str>,
    unit: Option<&str>,
    precision: Option<u64>,
) -> bool {
    if device_class.is_some_and(|dc| tables::NON_NUMERIC_SENSOR_CLASSES.contains(&dc)) {
        return false;
    }
    if state_class.is_some() || unit.is_some() || precision.is_some() {
        return true;
    }
    device_class.is_some()
}

/// `_get_adjusted_display_precision` for a sensor that shows its own
/// unit: the integration's precision, else the device class's.
pub fn display_precision(
    device_class: Option<&str>,
    unit: Option<&str>,
    given: Option<u64>,
) -> Option<u64> {
    let (Some(dc), Some(unit), None) = (device_class, unit, given) else {
        return given;
    };
    tables::UNIT_PRECISION
        .iter()
        .find(|(c, u, _)| *c == dc && *u == unit)
        .map(|(_, _, p)| u64::from(*p))
}

/// HA's `MQTT_ATTRIBUTES_BLOCKED`, and the sensor's own
/// (`last_reset`, `state_class`).
pub fn attribute_blocked(domain: Domain, key: &str) -> bool {
    const BLOCKED: [&str; 18] = [
        "assumed_state",
        "available",
        "device_class",
        "device_info",
        "entity_category",
        "entity_id",
        "entity_picture",
        "entity_registry_enabled_default",
        "extra_state_attributes",
        "force_update",
        "friendly_name",
        "group_entities",
        "icon",
        "should_poll",
        "state",
        "supported_features",
        "unique_id",
        "unit_of_measurement",
    ];
    BLOCKED.contains(&key)
        || (domain == Domain::Sensor && matches!(key, "last_reset" | "state_class"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(domain: Domain, v: Value) -> Result<Config, String> {
        Config::parse(domain, v.as_object().unwrap())
    }

    #[test]
    fn strings_are_pythons() {
        assert_eq!(cv_string(&json!(false)).unwrap(), "False");
        assert_eq!(cv_string(&json!(21.5)).unwrap(), "21.5");
        assert_eq!(cv_string(&json!(3)).unwrap(), "3");
        assert_eq!(cv_string(&json!(1e16)).unwrap(), "1e+16");
    }

    #[test]
    fn names_come_from_the_device_class_or_the_platform() {
        let c = cfg(
            Domain::Sensor,
            json!({"state_topic": "t", "device_class": "atmospheric_pressure",
                                           "unit_of_measurement": "hPa"}),
        )
        .unwrap();
        assert_eq!(c.entity_name().as_deref(), Some("Atmospheric pressure"));
        let c = cfg(Domain::Sensor, json!({"state_topic": "t"})).unwrap();
        assert_eq!(c.entity_name().as_deref(), Some("MQTT Sensor"));
        let c = cfg(
            Domain::BinarySensor,
            json!({"state_topic": "t", "name": null}),
        )
        .unwrap();
        assert_eq!(c.entity_name(), None);
        let c = cfg(
            Domain::BinarySensor,
            json!({"state_topic": "t", "device_class": "door"}),
        )
        .unwrap();
        assert_eq!(c.entity_name().as_deref(), Some("Door"));
    }

    #[test]
    fn the_schema_refuses_what_has_does() {
        assert!(cfg(Domain::Sensor, json!({})).is_err());
        assert!(cfg(Domain::Sensor, json!({"state_topic": "a/+b"})).is_err());
        assert!(
            cfg(
                Domain::Sensor,
                json!({"state_topic": "t", "device_class": "nope"})
            )
            .is_err()
        );
        assert!(
            cfg(
                Domain::Sensor,
                json!({"state_topic": "t", "device_class": "temperature",
                                           "unit_of_measurement": "hPa"})
            )
            .is_err()
        );
        assert!(
            cfg(
                Domain::Sensor,
                json!({"state_topic": "t", "options": ["a"]})
            )
            .is_err()
        );
        assert!(
            cfg(
                Domain::Sensor,
                json!({"state_topic": "t", "device": {"name": "x"}})
            )
            .is_err()
        );
        assert!(
            cfg(
                Domain::Sensor,
                json!({"state_topic": "t", "value_template": "{{ x"})
            )
            .is_err()
        );
        // Unknown keys are dropped, not refused.
        assert!(cfg(Domain::Sensor, json!({"state_topic": "t", "whatever": 1})).is_ok());
        // An empty unit is no unit.
        let c = cfg(
            Domain::Sensor,
            json!({"state_topic": "t", "unit_of_measurement": " "}),
        )
        .unwrap();
        assert!(matches!(c.platform, Platform::Sensor { unit: None, .. }));
    }

    #[test]
    fn precision_follows_the_device_class_unit() {
        assert_eq!(
            display_precision(Some("temperature"), Some("°C"), None),
            Some(1)
        );
        assert_eq!(
            display_precision(Some("atmospheric_pressure"), Some("hPa"), None),
            Some(2)
        );
        assert_eq!(display_precision(Some("humidity"), Some("%"), None), None);
        assert_eq!(
            display_precision(Some("humidity"), Some("%"), Some(3)),
            Some(3)
        );
        assert_eq!(display_precision(None, Some("%"), Some(3)), Some(3));
        assert_eq!(display_precision(None, Some("%"), None), None);
    }
}

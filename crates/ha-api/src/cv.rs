//! The `homeassistant.helpers.config_validation` validators hars needs.

use serde_json::Value;

/// `cv.string`: anything but `None`, a list or a dict, as Python's `str`.
pub(crate) fn string(v: &Value) -> Result<String, &'static str> {
    match v {
        Value::Null => Err("string value is None"),
        Value::String(s) => Ok(s.clone()),
        Value::Array(_) | Value::Object(_) => Err("value should be a string"),
        Value::Bool(b) => Ok(if *b { "True" } else { "False" }.into()),
        Value::Number(n) => Ok(match n.as_f64().filter(|_| n.is_f64()) {
            Some(f) => ha_integrations::sun::py_float(f),
            None => n.to_string(),
        }),
    }
}

/// `cv.boolean`: a bool, a number (non-zero is true) or one of HA's words.
pub(crate) fn boolean(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_f64().map(|f| f != 0.0),
        Value::String(s) => match s.trim().to_lowercase().as_str() {
            "1" | "true" | "yes" | "on" | "enable" => Some(true),
            "0" | "false" | "no" | "off" | "disable" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

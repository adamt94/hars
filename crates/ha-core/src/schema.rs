//! Service data schemas: a small, declarative stand-in for the voluptuous
//! schemas HA attaches to services.
//!
//! [`crate::Hass::call_service`] validates the data (with `target` merged in)
//! before the `call_service` event fires, so a bad call is rejected with
//! [`ServiceError::Invalid`] (HTTP 400) and never reaches the handler. Values
//! are coerced the way HA's validators do (`"50"` → `50.0` for a float,
//! `"on"` → `true` for a boolean); the handler sees the coerced data, while
//! the `call_service` event keeps the data as sent, as in HA.

use crate::service::{ServiceData, ServiceError, resolve_entity_ids};
use serde_json::{Number, Value};

/// What a field accepts, mirroring the HA validator named on each variant.
#[derive(Debug, Clone)]
pub enum Kind {
    /// Anything.
    Any,
    /// `cv.boolean`: a bool, a number (non-zero is true) or one of
    /// `1/true/yes/on/enable`, `0/false/no/off/disable`.
    Boolean,
    /// `vol.All(vol.Coerce(int), vol.Range(...))`.
    Int(Range),
    /// `vol.All(vol.Coerce(float), vol.Range(...))`.
    Float(Range),
    /// `cv.string`: a string, or a number turned into one.
    String,
    /// `vol.In([...])`.
    OneOf(&'static [&'static str]),
    /// `cv.comp_entity_ids`: see [`crate::ServiceCall::entity_ids`].
    EntityIds,
    /// `vol.All(cv.ensure_list, [kind])`: a single value becomes a list.
    List(Box<Kind>),
}

/// Inclusive bounds, as `vol.Range`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Range {
    pub min: Option<f64>,
    pub max: Option<f64>,
}

impl Range {
    pub const ANY: Range = Range {
        min: None,
        max: None,
    };

    pub const fn new(min: f64, max: f64) -> Self {
        Self {
            min: Some(min),
            max: Some(max),
        }
    }
}

#[derive(Debug, Clone)]
struct Field {
    key: &'static str,
    required: bool,
    kind: Kind,
}

/// A service's data schema. Like `vol.Schema`, unknown keys are rejected
/// unless [`Schema::allow_extra`] is set.
#[derive(Debug, Clone, Default)]
pub struct Schema {
    fields: Vec<Field>,
    allow_extra: bool,
    entity_service: bool,
}

/// HA's `ENTITY_SERVICE_FIELDS`.
const TARGET_KEYS: [&str; 5] = ["entity_id", "device_id", "area_id", "floor_id", "label_id"];

impl Schema {
    pub fn new() -> Self {
        Self::default()
    }

    /// HA's `cv.make_entity_service_schema`: the target keys are accepted,
    /// at least one of them must be present, and the frontend's `metadata`
    /// key is dropped.
    pub fn entity_service() -> Self {
        Self {
            entity_service: true,
            ..Self::default()
        }
    }

    pub fn required(mut self, key: &'static str, kind: Kind) -> Self {
        self.fields.push(Field {
            key,
            required: true,
            kind,
        });
        self
    }

    pub fn optional(mut self, key: &'static str, kind: Kind) -> Self {
        self.fields.push(Field {
            key,
            required: false,
            kind,
        });
        self
    }

    /// Keep unknown keys instead of rejecting them (`extra=vol.ALLOW_EXTRA`).
    pub fn allow_extra(mut self) -> Self {
        self.allow_extra = true;
        self
    }

    /// Validate `data`. Returns `Ok(None)` when it is valid as it is (the
    /// common case, which allocates nothing) or `Ok(Some(coerced))` when a
    /// value had to be coerced. Errors use voluptuous's wording.
    pub fn validate(&self, data: &ServiceData) -> Result<Option<ServiceData>, ServiceError> {
        let invalid = |msg: String| Err(ServiceError::Invalid(msg));
        let mut coerced: Vec<(&str, Value)> = Vec::new();
        let mut drop_metadata = false;

        for (key, value) in data {
            let key = key.as_str();
            if let Some(field) = self.fields.iter().find(|f| f.key == key) {
                match check(&field.kind, value) {
                    Ok(Some(v)) => coerced.push((field.key, v)),
                    Ok(None) => {}
                    Err((msg, None)) => {
                        return invalid(format!("{msg} for dictionary value @ data['{key}']"));
                    }
                    Err((msg, Some(i))) => return invalid(format!("{msg} @ data['{key}'][{i}]")),
                }
            } else if self.entity_service && TARGET_KEYS.contains(&key) {
                if let Err((msg, _)) = check_target(key, value) {
                    return invalid(format!("{msg} for dictionary value @ data['{key}']"));
                }
            } else if self.entity_service && key == "metadata" {
                drop_metadata = true;
            } else if !self.allow_extra {
                return invalid(format!("extra keys not allowed @ data['{key}']"));
            }
        }
        if let Some(missing) = self
            .fields
            .iter()
            .find(|f| f.required && !data.contains_key(f.key))
        {
            return invalid(format!(
                "required key not provided @ data['{}']",
                missing.key
            ));
        }
        if self.entity_service && !TARGET_KEYS.iter().any(|k| data.contains_key(*k)) {
            return invalid(format!(
                "must contain at least one of {}.",
                TARGET_KEYS.join(", ")
            ));
        }

        if coerced.is_empty() && !drop_metadata {
            return Ok(None);
        }
        let mut out = data.clone();
        if drop_metadata {
            out.remove("metadata");
        }
        for (key, value) in coerced {
            out.insert(key.to_owned(), value);
        }
        Ok(Some(out))
    }
}

/// `Err((message, list index))`.
type Checked = Result<Option<Value>, (String, Option<usize>)>;

fn check(kind: &Kind, value: &Value) -> Checked {
    let err = |msg: String| Err((msg, None));
    match kind {
        Kind::Any => Ok(None),
        Kind::Boolean => match value {
            Value::Bool(_) => Ok(None),
            Value::Number(n) => Ok(Some(Value::Bool(n.as_f64() != Some(0.0)))),
            Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" | "enable" => Ok(Some(Value::Bool(true))),
                "0" | "false" | "no" | "off" | "disable" => Ok(Some(Value::Bool(false))),
                _ => err(format!("invalid boolean value {s}")),
            },
            other => err(format!("invalid boolean value {other}")),
        },
        Kind::Int(range) => {
            let (n, changed) = match value {
                Value::Number(n) if n.is_i64() || n.is_u64() => (n.as_f64().unwrap_or(0.0), false),
                // Python's int() truncates a float.
                Value::Number(n) => (n.as_f64().unwrap_or(0.0).trunc(), true),
                Value::Bool(b) => (f64::from(u8::from(*b)), true),
                Value::String(s) => match s.trim().parse::<i64>() {
                    Ok(i) => (i as f64, true),
                    Err(_) => return err("expected int".into()),
                },
                _ => return err("expected int".into()),
            };
            in_range(n, range).map_err(|m| (m, None))?;
            Ok(changed.then(|| Value::from(n as i64)))
        }
        Kind::Float(range) => {
            let (n, changed) = match value {
                Value::Number(n) => (n.as_f64().unwrap_or(0.0), false),
                Value::Bool(b) => (f64::from(u8::from(*b)), true),
                Value::String(s) => match s.trim().parse::<f64>() {
                    Ok(f) => (f, true),
                    Err(_) => return err("expected float".into()),
                },
                _ => return err("expected float".into()),
            };
            in_range(n, range).map_err(|m| (m, None))?;
            match changed {
                false => Ok(None),
                true => Number::from_f64(n)
                    .map(|n| Some(Value::Number(n)))
                    .ok_or_else(|| ("expected float".into(), None)),
            }
        }
        Kind::String => match value {
            Value::String(_) => Ok(None),
            Value::Number(n) => Ok(Some(Value::String(n.to_string()))),
            Value::Bool(b) => Ok(Some(Value::String(
                if *b { "True" } else { "False" }.into(),
            ))),
            Value::Null => err("string value is None".into()),
            _ => err("value should be a string".into()),
        },
        Kind::OneOf(options) => match value {
            Value::String(s) if options.contains(&s.as_str()) => Ok(None),
            _ => {
                let list: Vec<String> = options.iter().map(|o| format!("'{o}'")).collect();
                err(format!("value must be one of [{}]", list.join(", ")))
            }
        },
        Kind::EntityIds => check_target("entity_id", value),
        Kind::List(item) => {
            let Value::Array(items) = value else {
                // `ensure_list`: wrap a single value, then validate it.
                let one = check(item, value).map_err(|(m, _)| (m, Some(0)))?;
                return Ok(Some(Value::Array(vec![
                    one.unwrap_or_else(|| value.clone()),
                ])));
            };
            let mut out: Option<Vec<Value>> = None;
            for (i, v) in items.iter().enumerate() {
                if let Some(new) = check(item, v).map_err(|(m, _)| (m, Some(i)))? {
                    out.get_or_insert_with(|| items.clone())[i] = new;
                }
            }
            Ok(out.map(Value::Array))
        }
    }
}

/// Target keys are validated but kept as sent: handlers resolve them.
fn check_target(key: &str, value: &Value) -> Checked {
    if key == "entity_id" {
        return match resolve_entity_ids(Some(value)) {
            Ok(_) => Ok(None),
            Err(ServiceError::Invalid(msg)) => Err((msg, None)),
            Err(e) => Err((e.to_string(), None)),
        };
    }
    match value {
        Value::String(_) => Ok(None),
        Value::Array(items) if items.iter().all(Value::is_string) => Ok(None),
        _ => Err(("value should be a string".into(), None)),
    }
}

fn in_range(n: f64, range: &Range) -> Result<(), String> {
    let fmt = |b: f64| {
        if b.fract() == 0.0 {
            format!("{}", b as i64)
        } else {
            b.to_string()
        }
    };
    if let Some(min) = range.min
        && n < min
    {
        return Err(format!("value must be at least {}", fmt(min)));
    }
    if let Some(max) = range.max
        && n > max
    {
        return Err(format!("value must be at most {}", fmt(max)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn data(v: Value) -> ServiceData {
        v.as_object().unwrap().clone()
    }

    /// The part of HA's `light.turn_on` schema the MVP automations use.
    fn light_turn_on() -> Schema {
        Schema::entity_service()
            .optional("brightness_pct", Kind::Float(Range::new(0.0, 100.0)))
            .optional("color_temp_kelvin", Kind::Int(Range::ANY))
            .optional("transition", Kind::Float(Range::new(0.0, 6553.0)))
            .optional("flash", Kind::OneOf(&["short", "long"]))
    }

    fn err(schema: &Schema, v: Value) -> String {
        match schema.validate(&data(v)) {
            Err(ServiceError::Invalid(msg)) => msg,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn valid_data_is_not_copied() {
        let ok = data(json!({
            "entity_id": ["light.a", "light.b"],
            "brightness_pct": 30,
            "color_temp_kelvin": 2700,
            "transition": 1.5,
            "flash": "short",
        }));
        assert_eq!(light_turn_on().validate(&ok), Ok(None));
    }

    #[test]
    fn coerces_like_voluptuous() {
        let out = light_turn_on()
            .validate(&data(json!({
                "area_id": "bedroom",
                "brightness_pct": "50",
                "color_temp_kelvin": 2700.9,
                "metadata": { "entity_id": {} },
            })))
            .unwrap()
            .expect("coerced");
        assert_eq!(
            Value::Object(out),
            json!({ "area_id": "bedroom", "brightness_pct": 50.0, "color_temp_kelvin": 2700 }),
            "metadata is dropped; Coerce(int) truncates"
        );

        let s = Schema::new()
            .optional("b", Kind::Boolean)
            .optional("s", Kind::String)
            .optional("l", Kind::List(Box::new(Kind::Int(Range::ANY))));
        let out = s
            .validate(&data(json!({ "b": "On", "s": 12, "l": "3" })))
            .unwrap()
            .unwrap();
        assert_eq!(
            Value::Object(out),
            json!({ "b": true, "s": "12", "l": [3] })
        );
        let out = s
            .validate(&data(json!({ "b": 0, "l": [1, "2"] })))
            .unwrap()
            .unwrap();
        assert_eq!(Value::Object(out), json!({ "b": false, "l": [1, 2] }));
    }

    #[test]
    fn errors_use_voluptuous_wording() {
        let light = light_turn_on();
        for (v, msg) in [
            (
                json!({ "entity_id": "light.a", "brightness_pct": 101 }),
                "value must be at most 100 for dictionary value @ data['brightness_pct']",
            ),
            (
                json!({ "entity_id": "light.a", "transition": -1 }),
                "value must be at least 0 for dictionary value @ data['transition']",
            ),
            (
                json!({ "entity_id": "light.a", "brightness_pct": "bright" }),
                "expected float for dictionary value @ data['brightness_pct']",
            ),
            (
                json!({ "entity_id": "light.a", "color_temp_kelvin": "2700.5" }),
                "expected int for dictionary value @ data['color_temp_kelvin']",
            ),
            (
                json!({ "entity_id": "light.a", "flash": "medium" }),
                "value must be one of ['short', 'long'] for dictionary value @ data['flash']",
            ),
            (
                json!({ "entity_id": "light.a", "brightnes_pct": 5 }),
                "extra keys not allowed @ data['brightnes_pct']",
            ),
            (
                json!({ "brightness_pct": 5 }),
                "must contain at least one of entity_id, device_id, area_id, floor_id, label_id.",
            ),
            (
                json!({ "entity_id": "kitchen" }),
                "entity id must be `domain.object_id`: \"kitchen\" for dictionary value @ data['entity_id']",
            ),
        ] {
            assert_eq!(err(&light, v.clone()), msg, "{v}");
        }

        let s = Schema::new()
            .required("message", Kind::String)
            .optional("l", Kind::List(Box::new(Kind::Int(Range::ANY))));
        assert_eq!(
            err(&s, json!({})),
            "required key not provided @ data['message']"
        );
        assert_eq!(
            err(&s, json!({ "message": "hi", "l": [1, "x"] })),
            "expected int @ data['l'][1]"
        );
        assert_eq!(
            err(&s, json!({ "message": null })),
            "string value is None for dictionary value @ data['message']"
        );
        assert_eq!(
            s.clone()
                .allow_extra()
                .validate(&data(json!({ "message": "hi", "x": 1 }))),
            Ok(None)
        );
    }
}

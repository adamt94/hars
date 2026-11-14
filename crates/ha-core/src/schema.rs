//! Service data schemas: a small, declarative stand-in for the voluptuous
//! schemas HA attaches to services.
//!
//! [`crate::Hass::call_service`] validates the data (with `target` merged in)
//! before the `call_service` event fires, so a bad call is rejected with
//! [`ServiceError::Invalid`] (HTTP 400) and never reaches the handler. Values
//! are coerced the way HA's validators do (`"50"` → `50.0` for a float,
//! `"on"` → `true` for a boolean); the handler sees the coerced data, while
//! the `call_service` event keeps the data as sent, as in HA.

use crate::service::{ServiceData, ServiceError, validate_entity_ids};
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
    /// `vol.All(vol.Coerce(int), vol.Clamp(min, max))`.
    IntClamp(i64, i64),
    /// `vol.All(vol.Coerce(float), vol.Clamp(min, max))`.
    FloatClamp(f64, f64),
    /// `vol.All(vol.Coerce(tuple), vol.ExactSequence(kinds))`: a list (or a
    /// string, as its characters) of exactly these items. Becomes a list.
    Tuple(Vec<Kind>),
    /// `vol.Any(True, kind)`.
    TrueOr(Box<Kind>),
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
    /// `vol.Exclusive` groups: (group name, keys).
    exclusive: Vec<(&'static str, &'static [&'static str])>,
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

    /// `vol.Exclusive(key, group)` for each of `keys`: at most one may be
    /// given. The keys still need their own field.
    pub fn exclusive(mut self, group: &'static str, keys: &'static [&'static str]) -> Self {
        self.exclusive.push((group, keys));
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
        for (group, keys) in &self.exclusive {
            if keys.iter().filter(|k| data.contains_key(**k)).count() > 1 {
                return invalid(format!(
                    "two or more values in the same group of exclusion '{group}' @ data[<{group}>]"
                ));
            }
        }

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
                match check_target(key, value) {
                    Ok(Some(v)) => coerced.push((key, v)),
                    Ok(None) => {}
                    Err((msg, _)) => {
                        return invalid(format!("{msg} for dictionary value @ data['{key}']"));
                    }
                }
            } else if self.entity_service && key == "metadata" && value.is_object() {
                // `vol.Remove("metadata"): dict`; anything else is an extra key.
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
            Value::String(s) => match s.trim().to_lowercase().as_str() {
                "1" | "true" | "yes" | "on" | "enable" => Ok(Some(Value::Bool(true))),
                "0" | "false" | "no" | "off" | "disable" => Ok(Some(Value::Bool(false))),
                other => err(format!("invalid boolean value {other}")),
            },
            other => err(format!("invalid boolean value {other}")),
        },
        Kind::Int(range) => {
            let expected = || ("expected int".to_owned(), None);
            let (n, coerced): (i128, bool) = match value {
                Value::Number(n) => match (n.as_i64(), n.as_u64()) {
                    (Some(i), _) => (i.into(), false),
                    (_, Some(u)) => (u.into(), false),
                    // Python's int() truncates a float.
                    _ => {
                        let t = n.as_f64().unwrap_or(f64::NAN).trunc();
                        if !(t >= i64::MIN as f64 && t < u64::MAX as f64) {
                            return Err(expected());
                        }
                        (t as i128, true)
                    }
                },
                Value::Bool(b) => (i128::from(*b), true),
                Value::String(s) => (parse_python_int(s).ok_or_else(expected)?, true),
                _ => return Err(expected()),
            };
            int_in_range(n, range).map_err(|m| (m, None))?;
            if !coerced {
                return Ok(None);
            }
            match (i64::try_from(n), u64::try_from(n)) {
                (Ok(i), _) => Ok(Some(Value::from(i))),
                (_, Ok(u)) => Ok(Some(Value::from(u))),
                _ => Err(expected()),
            }
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
                // voluptuous sorts the choices in its message.
                let mut sorted = options.to_vec();
                sorted.sort_unstable();
                let list: Vec<String> = sorted.iter().map(|o| format!("'{o}'")).collect();
                err(format!("value must be one of [{}]", list.join(", ")))
            }
        },
        Kind::EntityIds => check_target("entity_id", value),
        Kind::IntClamp(min, max) => {
            let n = match check(&Kind::Int(Range::ANY), value)? {
                Some(v) => v.as_i64(),
                None => value.as_i64(),
            };
            // Beyond i64, the clamp decides.
            let n = n.unwrap_or(if value.as_f64().unwrap_or(0.0) < 0.0 {
                *min
            } else {
                *max
            });
            let clamped = n.clamp(*min, *max);
            Ok((clamped != n || !value.is_i64()).then(|| Value::from(clamped)))
        }
        Kind::FloatClamp(min, max) => {
            let n = match check(&Kind::Float(Range::ANY), value)? {
                Some(v) => v.as_f64(),
                None => value.as_f64(),
            }
            .unwrap_or(0.0);
            let clamped = n.clamp(*min, *max);
            // `vol.Coerce(float)` makes a float of an int too.
            Ok((clamped != n || !value.is_f64())
                .then(|| Number::from_f64(clamped).map(Value::Number))
                .flatten())
        }
        Kind::Tuple(kinds) => {
            let items: Vec<Value> = match value {
                Value::Array(items) => items.clone(),
                Value::String(s) => s.chars().map(|c| Value::String(c.into())).collect(),
                _ => return err("expected tuple".into()),
            };
            if items.len() != kinds.len() {
                // voluptuous's own (odd) message for a wrong length.
                return err("None".into());
            }
            let mut out = Vec::with_capacity(items.len());
            for (kind, item) in kinds.iter().zip(&items) {
                let new = check(kind, item).map_err(|(m, _)| (m, None))?;
                out.push(new.unwrap_or_else(|| item.clone()));
            }
            Ok((Value::Array(out.clone()) != *value).then_some(Value::Array(out)))
        }
        Kind::TrueOr(kind) => match value {
            Value::Bool(true) => Ok(None),
            _ => check(kind, value).map_err(|_| ("not a valid value".into(), None)),
        },
        Kind::List(item) => {
            if value.is_null() {
                return Ok(Some(Value::Array(Vec::new()))); // ensure_list(None) == []
            }
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
        return match validate_entity_ids(value) {
            Ok(()) => Ok(None),
            Err(ServiceError::Invalid(msg)) => Err((msg, None)),
            Err(e) => Err((e.to_string(), None)),
        };
    }
    // `vol.Any("none", vol.All(cv.ensure_list, [str]))`: `"none"` is kept,
    // null becomes `[]` and a single id a one-item list.
    match value {
        Value::String(s) if s == "none" => Ok(None),
        Value::Null => Ok(Some(Value::Array(Vec::new()))),
        Value::String(_) => Ok(Some(Value::Array(vec![value.clone()]))),
        Value::Array(items) if items.iter().all(Value::is_string) => Ok(None),
        _ => Err(("value should be a string".into(), None)),
    }
}

/// Python's `int(str)`: surrounding whitespace, an optional sign, and
/// digits with single `_` separators.
fn parse_python_int(s: &str) -> Option<i128> {
    let t = s.trim();
    let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
    let ok = !digits.is_empty()
        && !digits.starts_with('_')
        && !digits.ends_with('_')
        && !digits.contains("__")
        && digits.bytes().all(|b| b.is_ascii_digit() || b == b'_');
    if !ok {
        return None;
    }
    t.replace('_', "").parse().ok()
}

fn int_in_range(n: i128, range: &Range) -> Result<(), String> {
    // Exact for integral bounds; a fractional bound compares as Python does.
    let below = |b: f64| match b.fract() == 0.0 && b.abs() < 1e38 {
        true => n < b as i128,
        false => (n as f64) < b,
    };
    let above = |b: f64| match b.fract() == 0.0 && b.abs() < 1e38 {
        true => n > b as i128,
        false => (n as f64) > b,
    };
    if let Some(min) = range.min
        && below(min)
    {
        return Err(format!("value must be at least {}", fmt_bound(min)));
    }
    if let Some(max) = range.max
        && above(max)
    {
        return Err(format!("value must be at most {}", fmt_bound(max)));
    }
    Ok(())
}

fn fmt_bound(b: f64) -> String {
    if b.fract() == 0.0 {
        format!("{}", b as i64)
    } else {
        b.to_string()
    }
}

fn in_range(n: f64, range: &Range) -> Result<(), String> {
    // NaN fails the bound check, as in voluptuous.
    if let Some(min) = range.min
        && (n.is_nan() || n < min)
    {
        return Err(format!("value must be at least {}", fmt_bound(min)));
    }
    if let Some(max) = range.max
        && (n.is_nan() || n > max)
    {
        return Err(format!("value must be at most {}", fmt_bound(max)));
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
            json!({ "area_id": ["bedroom"], "brightness_pct": 50.0, "color_temp_kelvin": 2700 }),
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
                "value must be one of ['long', 'short'] for dictionary value @ data['flash']",
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

    #[test]
    fn ints_stay_exact() {
        let s = Schema::new().optional("i", Kind::Int(Range::ANY));
        let out = |v: Value| {
            s.validate(&data(json!({ "i": v })))
                .map(|o| o.map(|d| d["i"].clone()))
        };
        assert_eq!(
            out(json!("9007199254740993")),
            Ok(Some(json!(9007199254740993_i64)))
        );
        assert_eq!(
            out(json!("18446744073709551615")),
            Ok(Some(json!(u64::MAX)))
        );
        assert_eq!(out(json!(" -1_000 ")), Ok(Some(json!(-1000))));
        assert_eq!(out(json!(true)), Ok(Some(json!(1))));
        assert_eq!(out(json!(u64::MAX)), Ok(None));
        for bad in [
            json!("1__0"),
            json!("_1"),
            json!("1.0"),
            json!(1e20),
            json!(""),
        ] {
            assert!(out(bad.clone()).is_err(), "{bad}");
        }
        let capped = Schema::new().optional("i", Kind::Int(Range::new(0.0, 9007199254740992.0)));
        assert!(
            capped
                .validate(&data(json!({ "i": 9007199254740993_i64 })))
                .is_err()
        );
    }

    #[test]
    fn nulls_and_metadata_follow_ha() {
        let s = Schema::entity_service().optional("l", Kind::List(Box::new(Kind::Int(Range::ANY))));
        let out = s
            .validate(&data(
                json!({ "entity_id": "light.a", "area_id": null, "l": null }),
            ))
            .unwrap()
            .unwrap();
        assert_eq!(
            Value::Object(out),
            json!({ "entity_id": "light.a", "area_id": [], "l": [] })
        );
        let out = s
            .validate(&data(
                json!({ "device_id": "abc", "label_id": "none", "floor_id": ["f"] }),
            ))
            .unwrap()
            .unwrap();
        assert_eq!(
            Value::Object(out),
            json!({ "device_id": ["abc"], "label_id": "none", "floor_id": ["f"] })
        );
        assert_eq!(
            err(&s, json!({ "entity_id": "light.a", "metadata": 42 })),
            "extra keys not allowed @ data['metadata']"
        );
    }

    #[test]
    fn nan_and_boolean_messages() {
        let f = Schema::new().optional("f", Kind::Float(Range::new(0.0, 100.0)));
        assert_eq!(
            err(&f, json!({ "f": "NaN" })),
            "value must be at least 0 for dictionary value @ data['f']"
        );
        let b = Schema::new().optional("b", Kind::Boolean);
        assert_eq!(
            err(&b, json!({ "b": " BAD " })),
            "invalid boolean value bad for dictionary value @ data['b']"
        );
    }

    /// Each against voluptuous 0.15.2 with HA's light schema.
    #[test]
    fn clamps_tuples_any_and_exclusive_groups() {
        let schema = Schema::new()
            .optional("transition", Kind::FloatClamp(0.0, 6553.0))
            .optional("brightness", Kind::IntClamp(0, 255))
            .optional("brightness_pct", Kind::Float(Range::new(0.0, 100.0)))
            .optional(
                "rgb_color",
                Kind::Tuple(vec![Kind::Int(Range::new(0.0, 255.0)); 3]),
            )
            .optional(
                "hs_color",
                Kind::Tuple(vec![
                    Kind::Float(Range::new(0.0, 360.0)),
                    Kind::Float(Range::new(0.0, 100.0)),
                ]),
            )
            .optional(
                "xy_color",
                Kind::Tuple(vec![Kind::Float(Range::new(0.0, 1.0)); 2]),
            )
            .optional("white", Kind::TrueOr(Box::new(Kind::IntClamp(0, 255))))
            .exclusive("brightness", &["brightness", "brightness_pct"])
            .exclusive(
                "Color descriptors",
                &["rgb_color", "hs_color", "xy_color", "white"],
            );
        let ok = |v: Value| schema.validate(&data(v)).unwrap().map(Value::Object);
        let bad = |v: Value| match schema.validate(&data(v)) {
            Err(ServiceError::Invalid(m)) => m,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            ok(json!({"brightness": 300})),
            Some(json!({"brightness": 255}))
        );
        assert_eq!(
            ok(json!({"brightness": 12.7})),
            Some(json!({"brightness": 12}))
        );
        assert_eq!(
            ok(json!({"brightness": true})),
            Some(json!({"brightness": 1}))
        );
        assert_eq!(ok(json!({"brightness": 12})), None);
        assert_eq!(
            ok(json!({"transition": 9000})),
            Some(json!({"transition": 6553.0}))
        );
        assert_eq!(ok(json!({"white": true})), None);
        assert_eq!(ok(json!({"white": 300})), Some(json!({"white": 255})));
        assert_eq!(
            ok(json!({"rgb_color": ["1", 2, 3]})),
            Some(json!({"rgb_color": [1, 2, 3]}))
        );
        for (v, msg) in [
            (
                json!({"brightness": "12.5"}),
                "expected int for dictionary value @ data['brightness']",
            ),
            (
                json!({"transition": "x"}),
                "expected float for dictionary value @ data['transition']",
            ),
            (
                json!({"brightness_pct": 101}),
                "value must be at most 100 for dictionary value @ data['brightness_pct']",
            ),
            (
                json!({"brightness": 1, "brightness_pct": 2}),
                "two or more values in the same group of exclusion 'brightness' @ data[<brightness>]",
            ),
            (
                json!({"rgb_color": [1, 2]}),
                "None for dictionary value @ data['rgb_color']",
            ),
            (
                json!({"rgb_color": [1, 2, 300]}),
                "value must be at most 255 for dictionary value @ data['rgb_color']",
            ),
            (
                json!({"rgb_color": "abc"}),
                "expected int for dictionary value @ data['rgb_color']",
            ),
            (
                json!({"rgb_color": 5}),
                "expected tuple for dictionary value @ data['rgb_color']",
            ),
            (
                json!({"hs_color": [400, 1]}),
                "value must be at most 360 for dictionary value @ data['hs_color']",
            ),
            (
                json!({"xy_color": [0.5, "a"]}),
                "expected float for dictionary value @ data['xy_color']",
            ),
            (
                json!({"white": "x"}),
                "not a valid value for dictionary value @ data['white']",
            ),
            (
                json!({"rgb_color": [1, 2, 3], "xy_color": [0.1, 0.2]}),
                "two or more values in the same group of exclusion 'Color descriptors' @ data[<Color descriptors>]",
            ),
        ] {
            assert_eq!(bad(v.clone()), msg, "{v}");
        }
    }
}

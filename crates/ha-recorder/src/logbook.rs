//! The logbook's query and rows: HA's `logbook/processor.py` (`_humanify`),
//! `queries/` and `helpers.py` over the segment store.
//!
//! What a row is (`websocket_api` asks for `timestamp=True`,
//! `include_entity_name=False`):
//!
//! - **State**: `{"state", "entity_id", "icon"?, "when", <context_*>}`, for
//!   a [`VISIBLE`] row (its state changed, and its previous recorded state
//!   differed) of an entity that isn't a continuous sensor. `icon` is the
//!   recorded attributes' `icon`, when it is a non-empty string.
//! - **`automation_triggered`**: the describer's `{"name", "message",
//!   "source", "entity_id", "context_id"}` (absent keys are `null`), then
//!   `"domain": "automation"`, `"when"`, `<context_*>`.
//! - **`homeassistant_start` / `_stop`**: `{"name": "Home Assistant",
//!   "message": "started" | "stopped", "icon": "mdi:home-assistant",
//!   "domain": "homeassistant", "when", <context_*>}`.
//!
//! `<context_*>` is the row's stored `aug` (decision D4). `call_service`
//! rows exist only to link contexts, which the intake did at write time, so
//! none is a row here. `when` is the row's time as epoch seconds.
//!
//! The selection is HA's: with entities and/or devices, those entities'
//! state rows and the `automation_triggered` events whose `data` names them
//! (`entity_id` / `device_id`, a string equal to one asked for); with
//! neither, every row, plus the start and stop events; with a context id,
//! the rows of that context. Times are open at both ends.

use crate::format::{HAS_UNIT, REMOVED, VISIBLE};
use crate::history::seconds;
use crate::{EventRec, Rec, StateRec, Store};
use ha_core::{Attributes, ContextId};
use serde::Deserialize;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

/// `logbook/const.py`'s `ALWAYS_CONTINUOUS_DOMAINS`.
pub const ALWAYS_CONTINUOUS_DOMAINS: [&str; 3] = ["counter", "image", "proximity"];
/// `sensor/const.py`'s `NON_NUMERIC_DEVICE_CLASSES` (2026.6.1).
const NON_NUMERIC_DEVICE_CLASSES: [&str; 4] = ["date", "enum", "timestamp", "uptime"];

pub fn domain(entity_id: &str) -> &str {
    entity_id.split_once('.').map_or(entity_id, |(d, _)| d)
}

/// `_device_class_is_numeric`.
pub fn device_class_is_numeric(device_class: Option<&Value>) -> bool {
    match device_class {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !NON_NUMERIC_DEVICE_CLASSES.contains(&s.as_str()),
        Some(_) => true,
    }
}

/// A sensor's attributes say it is continuous: a unit, a state class or a
/// numeric device class (`is_sensor_continuous` on a live state).
pub fn sensor_attributes_continuous(attributes: &Attributes) -> bool {
    attributes.contains_key("unit_of_measurement")
        || attributes.contains_key("state_class")
        || device_class_is_numeric(attributes.get("device_class"))
}

/// Whether the logbook never shows this state's changes: an always
/// continuous domain, or a sensor that is continuous by its attributes.
pub fn continuous(domain: &str, attributes: &Attributes) -> bool {
    ALWAYS_CONTINUOUS_DOMAINS.contains(&domain)
        || (domain == "sensor" && sensor_attributes_continuous(attributes))
}

/// Which rows a request wants.
pub struct Filter {
    entities: HashSet<String>,
    devices: HashSet<String>,
    context: Option<[u8; 16]>,
}

impl Filter {
    /// `entities` are the ones left after HA's `async_filter_entities`.
    pub fn new(entities: &[String], devices: &[String], context: Option<[u8; 16]>) -> Self {
        Self {
            entities: entities.iter().cloned().collect(),
            devices: devices.iter().cloned().collect(),
            context,
        }
    }

    /// `EventProcessor.limited_select` without the context.
    fn limited(&self) -> bool {
        !self.entities.is_empty() || !self.devices.is_empty()
    }

    /// No entities and no devices: the logbook's every row.
    pub fn is_unlimited(&self) -> bool {
        !self.limited()
    }

    /// Whether a stored row is one the query selects (the SQL `WHERE`s).
    pub fn selects(&self, rec: &Rec<'_>) -> bool {
        match rec {
            Rec::State(s) => {
                self.context.is_none_or(|c| s.context == c)
                    && (!self.limited()
                        || (!self.entities.is_empty() && self.entities.contains(s.entity_id)))
            }
            Rec::Event(e) => {
                let kind = matches!(
                    e.event_type,
                    "automation_triggered" | "homeassistant_start" | "homeassistant_stop"
                );
                if let Some(c) = self.context {
                    return kind && e.context == c;
                }
                // With entities or devices only the events that can name
                // them (HA's `async_determine_event_types`).
                if self.limited() {
                    return e.event_type == "automation_triggered" && self.names(e.data, false);
                }
                kind
            }
        }
    }

    /// Whether a live row is one the stream follows
    /// (`event_forwarder_filtered`, `async_subscribe_events`).
    pub fn follows(&self, rec: &Rec<'_>) -> bool {
        match rec {
            // Devices alone: no state changes are subscribed.
            Rec::State(s) => {
                !self.limited()
                    || (!self.entities.is_empty() && self.entities.contains(s.entity_id))
            }
            Rec::Event(e) => {
                if !self.limited() {
                    return true;
                }
                e.event_type == "automation_triggered" && self.names(e.data, true)
            }
        }
    }

    /// Whether an event's `data` names an entity or device asked for. The
    /// stored query compares the JSON string; the live forwarder
    /// (`extract_attr`) also takes lists and comma separated text.
    fn names(&self, data: Option<&str>, live: bool) -> bool {
        let Some(Ok(Value::Object(data))) = data.map(serde_json::from_str::<Value>) else {
            return false;
        };
        let hit = |key: &str, wanted: &HashSet<String>| match data.get(key) {
            Some(Value::String(s)) if live => s.split(',').any(|p| wanted.contains(p)),
            Some(Value::String(s)) => wanted.contains(s.as_str()),
            Some(Value::Array(items)) if live => items
                .iter()
                .any(|v| v.as_str().is_some_and(|s| wanted.contains(s))),
            Some(v @ (Value::Number(_) | Value::Bool(_))) if live => {
                v.to_string().split(',').any(|p| wanted.contains(p))
            }
            _ => false,
        };
        hit("entity_id", &self.entities) || hit("device_id", &self.devices)
    }
}

/// Turns rows into the logbook's JSON, skipping those it never shows.
pub struct Humanify<'a> {
    /// `is_sensor_continuous`, for sensor rows: the entity's current state
    /// and registry entry decide.
    continuous: &'a dyn Fn(&str) -> bool,
    /// Per entity, as `_humanify`'s `continuous_sensors`.
    cache: HashMap<String, bool>,
}

impl<'a> Humanify<'a> {
    pub fn new(continuous: &'a dyn Fn(&str) -> bool) -> Self {
        Self {
            continuous,
            cache: HashMap::new(),
        }
    }

    /// Append `rec`'s row to `out`; `false` (and nothing appended) when the
    /// logbook doesn't show it.
    pub fn row(&mut self, out: &mut String, rec: &Rec<'_>) -> bool {
        match rec {
            Rec::State(s) => self.state(out, s),
            Rec::Event(e) => event(out, e),
        }
    }

    fn state(&mut self, out: &mut String, s: &StateRec<'_>) -> bool {
        if s.flags & VISIBLE == 0 || s.flags & REMOVED != 0 {
            return false;
        }
        let domain = domain(s.entity_id);
        if ALWAYS_CONTINUOUS_DOMAINS.contains(&domain) {
            return false;
        }
        if domain == "sensor" {
            // The recorded attributes had a unit (the query's `LIKE`), or
            // the entity is one by its state or registry entry.
            if s.flags & HAS_UNIT != 0 {
                return false;
            }
            let continuous = match self.cache.get(s.entity_id) {
                Some(c) => *c,
                None => {
                    let c = (self.continuous)(s.entity_id);
                    self.cache.insert(s.entity_id.to_owned(), c);
                    c
                }
            };
            if continuous {
                return false;
            }
        }
        out.push_str("{\"state\":");
        push_json(out, s.state);
        out.push_str(",\"entity_id\":");
        push_json(out, s.entity_id);
        if let Some(icon) = icon(s.attributes) {
            out.push_str(",\"icon\":");
            push_json(out, &icon);
        }
        finish(out, s.ts, s.aug);
        true
    }
}

/// The recorded attributes' `icon`, when it is a non-empty string.
fn icon(attributes: &str) -> Option<Cow<'_, str>> {
    #[derive(Deserialize)]
    struct Icon<'a> {
        #[serde(borrow)]
        icon: Option<Cow<'a, str>>,
    }
    // Most rows have none: look before parsing.
    if !attributes.contains("\"icon\"") {
        return None;
    }
    serde_json::from_str::<Icon<'_>>(attributes)
        .ok()?
        .icon
        .filter(|i| !i.is_empty())
}

fn event(out: &mut String, e: &EventRec<'_>) -> bool {
    match e.event_type {
        "automation_triggered" => {
            // `automation/logbook.py`'s describer.
            let data: Value = e
                .data
                .and_then(|d| serde_json::from_str(d).ok())
                .unwrap_or(Value::Null);
            out.push_str("{\"name\":");
            push_value(out, data.get("name"));
            out.push_str(",\"message\":");
            match data.get("source") {
                Some(source) => push_json(out, &format!("triggered by {}", py_str(source))),
                None => out.push_str("\"triggered\""),
            }
            out.push_str(",\"source\":");
            push_value(out, data.get("source"));
            out.push_str(",\"entity_id\":");
            push_value(out, data.get("entity_id"));
            let _ = write!(
                out,
                ",\"context_id\":\"{}\",\"domain\":\"automation\"",
                ContextId::from_bytes(e.context)
            );
        }
        // `homeassistant/logbook.py`'s describer.
        "homeassistant_start" | "homeassistant_stop" => {
            let message = if e.event_type == "homeassistant_start" {
                "started"
            } else {
                "stopped"
            };
            let _ = write!(
                out,
                "{{\"name\":\"Home Assistant\",\"message\":\"{message}\",\
                 \"icon\":\"mdi:home-assistant\",\"domain\":\"homeassistant\""
            );
        }
        _ => return false,
    }
    finish(out, e.ts, e.aug);
    true
}

/// `when`, the stored `context_*` fields, and the closing brace.
fn finish(out: &mut String, ts: i64, aug: Option<&str>) {
    let _ = write!(out, ",\"when\":{:?}", seconds(ts));
    if let Some(fields) = aug
        .and_then(|a| a.strip_prefix('{'))
        .and_then(|a| a.strip_suffix('}'))
        .filter(|f| !f.is_empty())
    {
        out.push(',');
        out.push_str(fields);
    }
    out.push('}');
}

/// Python's `str()` of a JSON value, for the message.
fn py_str(v: &Value) -> Cow<'_, str> {
    match v {
        Value::String(s) => Cow::Borrowed(s),
        Value::Null => Cow::Borrowed("None"),
        Value::Bool(true) => Cow::Borrowed("True"),
        Value::Bool(false) => Cow::Borrowed("False"),
        other => Cow::Owned(other.to_string()),
    }
}

fn push_value(out: &mut String, v: Option<&Value>) {
    match v {
        None | Some(Value::Null) => out.push_str("null"),
        Some(Value::String(s)) => push_json(out, s),
        Some(other) => {
            let _ = write!(out, "{other}");
        }
    }
}

/// A JSON string, escaped as `serde_json` does, without an allocation.
fn push_json(out: &mut String, s: &str) {
    out.push('"');
    let mut from = 0;
    for (i, b) in s.bytes().enumerate() {
        let escape = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0x08 => "\\b",
            0x0c => "\\f",
            0..0x20 => "",
            _ => continue,
        };
        // Escapes are ASCII, so `i` is on a character boundary.
        out.push_str(&s[from..i]);
        if escape.is_empty() {
            let _ = write!(out, "\\u{b:04x}");
        } else {
            out.push_str(escape);
        }
        from = i + 1;
    }
    out.push_str(&s[from..]);
    out.push('"');
}

pub struct Query<'a> {
    /// µs since the epoch; rows strictly between `start` and `end`.
    pub start: i64,
    pub end: i64,
    pub filter: &'a Filter,
    /// `is_sensor_continuous`, see [`Humanify::new`].
    pub continuous: &'a dyn Fn(&str) -> bool,
}

pub struct Answer {
    /// A JSON array of rows, oldest first.
    pub json: String,
    /// The newest row's time (µs), if there is any row.
    pub last: Option<i64>,
}

/// `EventProcessor.get_events`. Blocking: reads segment files.
pub fn events(store: &Store, q: &Query<'_>) -> Answer {
    let mut humanify = Humanify::new(q.continuous);
    // Every row's text in one buffer; `(time, from, to)` per row.
    let mut buf = String::new();
    let mut rows: Vec<(i64, usize, usize)> = Vec::new();
    store.scan(q.start, q.end, |rec| {
        let ts = rec.ts();
        if ts <= q.start || ts >= q.end || !q.filter.selects(&rec) {
            return;
        }
        let from = buf.len();
        if humanify.row(&mut buf, &rec) {
            rows.push((ts, from, buf.len()));
        }
    });
    // `ORDER BY time_fired_ts`; the write order breaks ties.
    rows.sort_by_key(|r| r.0);
    let mut json = String::with_capacity(buf.len() + rows.len() + 2);
    json.push('[');
    for (i, &(_, from, to)) in rows.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        json.push_str(&buf[from..to]);
    }
    json.push(']');
    Answer {
        json,
        last: rows.last().map(|r| r.0),
    }
}

#[cfg(test)]
mod tests;

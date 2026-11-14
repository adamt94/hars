//! An automation's config, validated into what the engine runs. The
//! subset the inventory uses (state, numeric_state and time triggers;
//! numeric_state, state, time and trigger conditions; service calls,
//! choose and wait_for_trigger; single and restart modes). Anything else,
//! templates included, fails validation: the automation is then
//! `unavailable`, as HA makes an invalid one.

use serde_json::{Map, Value};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Single,
    Restart,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Single => "single",
            Mode::Restart => "restart",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TriggerKind {
    State {
        entity_ids: Vec<String>,
        to: Option<Vec<String>>,
        for_: Option<Duration>,
    },
    NumericState {
        entity_ids: Vec<String>,
        below: Option<f64>,
        above: Option<f64>,
    },
    /// One `at` each (HA attaches one tracker per time).
    Time { at: Vec<jiff::civil::Time> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Trigger {
    /// `id`, or the index as a string.
    pub id: String,
    pub idx: usize,
    pub kind: TriggerKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    NumericState {
        entity_ids: Vec<String>,
        below: Option<f64>,
        above: Option<f64>,
    },
    State {
        entity_ids: Vec<String>,
        states: Vec<String>,
    },
    Time {
        after: Option<jiff::civil::Time>,
        before: Option<jiff::civil::Time>,
    },
    Trigger {
        ids: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Call {
        domain: String,
        service: String,
        data: Map<String, Value>,
        /// The target, `entity_id` as a list.
        target: Map<String, Value>,
    },
    Choose {
        options: Vec<(Vec<Condition>, Vec<Action>)>,
        default: Vec<Action>,
    },
    WaitForTrigger {
        triggers: Vec<Trigger>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub id: Option<String>,
    pub alias: Option<String>,
    pub mode: Mode,
    pub initial_state: Option<bool>,
    pub triggers: Vec<Trigger>,
    pub conditions: Vec<Condition>,
    pub actions: Vec<Action>,
}

type R<T> = Result<T, String>;

fn path(p: &str, k: &str) -> String {
    format!("{p}['{k}']")
}

fn reject_templates(v: &Value) -> R<()> {
    match v {
        Value::String(s) if s.contains("{{") || s.contains("{%") => {
            Err(format!("templates are not supported by hars: {s}"))
        }
        Value::Array(a) => a.iter().try_for_each(reject_templates),
        Value::Object(o) => o.values().try_for_each(reject_templates),
        _ => Ok(()),
    }
}

/// `cv.entity_ids`: a string (comma-separated) or a list of strings,
/// lower-cased.
fn entity_ids(v: Option<&Value>, at: &str) -> R<Vec<String>> {
    let one = |s: &str| -> R<String> {
        let s = s.trim().to_lowercase();
        ha_core::EntityId::parse(&s)
            .map(|_| s.clone())
            .map_err(|_| {
                format!("Entity ID {s} is an invalid entity ID for dictionary value @ data{at}")
            })
    };
    match v {
        Some(Value::String(s)) => s.split(',').map(one).collect(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| match i {
                Value::String(s) => one(s),
                _ => Err(format!("expected str for dictionary value @ data{at}")),
            })
            .collect(),
        Some(_) => Err(format!("Entity IDs should be a list @ data{at}")),
        None => Err(format!("required key not provided @ data{at}")),
    }
}

/// `vol.Coerce(float)`.
fn float(v: Option<&Value>, at: &str) -> R<Option<f64>> {
    match v {
        None => Ok(None),
        Some(Value::Number(n)) => Ok(n.as_f64()),
        Some(Value::String(s)) => crate::py_float(s)
            .map(Some)
            .ok_or_else(|| format!("expected float for dictionary value @ data{at}")),
        Some(Value::Bool(b)) => Ok(Some(f64::from(u8::from(*b)))),
        Some(_) => Err(format!("expected float for dictionary value @ data{at}")),
    }
}

/// `cv.time`: `HH:MM` or `HH:MM:SS` (a string; YAML's unquoted
/// `23:00:00` is an int and fails, as in HA).
pub fn time(v: &Value) -> Option<jiff::civil::Time> {
    let s = v.as_str()?;
    let parts: Vec<&str> = s.split(':').collect();
    let num = |p: &str| p.parse::<i8>().ok();
    let (h, m, sec) = match parts.as_slice() {
        [h, m] => (num(h)?, num(m)?, 0),
        [h, m, s] => (num(h)?, num(m)?, num(s)?),
        _ => return None,
    };
    jiff::civil::Time::new(h, m, sec, 0).ok()
}

/// `cv.positive_time_period`: `{hours, minutes, seconds, milliseconds,
/// days}`, `"HH:MM[:SS]"`, or seconds.
fn duration(v: &Value, at: &str) -> R<Duration> {
    let bad = || format!("expected a time period @ data{at}");
    let secs = match v {
        Value::Number(n) => n.as_f64().ok_or_else(bad)?,
        Value::String(s) => {
            let parts: Vec<f64> = s
                .split(':')
                .map(|p| p.trim().parse::<f64>())
                .collect::<Result<_, _>>()
                .map_err(|_| bad())?;
            match parts.as_slice() {
                [s] => *s,
                [h, m] => h * 3600.0 + m * 60.0,
                [h, m, s] => h * 3600.0 + m * 60.0 + s,
                _ => return Err(bad()),
            }
        }
        Value::Object(o) => {
            let mut t = 0.0;
            for (k, v) in o {
                let n = v.as_f64().ok_or_else(bad)?;
                t += n * match k.as_str() {
                    "days" => 86400.0,
                    "hours" => 3600.0,
                    "minutes" => 60.0,
                    "seconds" => 1.0,
                    "milliseconds" => 0.001,
                    _ => return Err(format!("extra keys not allowed @ data{at}['{k}']")),
                };
            }
            t
        }
        _ => return Err(bad()),
    };
    if !(0.0..1e10).contains(&secs) {
        return Err(format!("Time period must be positive @ data{at}"));
    }
    Ok(Duration::from_secs_f64(secs))
}

fn only_keys(o: &Map<String, Value>, allowed: &[&str], at: &str) -> R<()> {
    match o.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(format!("extra keys not allowed @ data{at}['{k}']")),
        None => Ok(()),
    }
}

/// `enabled`: a bool, default true (templates aren't supported).
fn is_enabled(v: &Value, at: &str) -> R<bool> {
    match v.get("enabled") {
        None => Ok(true),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("expected boolean @ data{at}['enabled']")),
    }
}

/// The items not `enabled: false` (with their original index).
fn enabled_items(list: &[Value], at: &str) -> R<Vec<(usize, Value)>> {
    let mut out = Vec::new();
    for (i, v) in list.iter().enumerate() {
        if is_enabled(v, &format!("{at}[{i}]"))? {
            out.push((i, v.clone()));
        }
    }
    Ok(out)
}

fn triggers(v: Option<&Value>, at: &str) -> R<Vec<Trigger>> {
    let list = match v {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::Object(o)) => vec![Value::Object(o.clone())],
        _ => return Err(format!("required key not provided @ data{at}")),
    };
    // A disabled trigger isn't attached; the others keep their index.
    enabled_items(&list, at)?
        .iter()
        .map(|(idx, t)| trigger(t, *idx, &format!("{at}[{idx}]")))
        .collect()
}

fn trigger(v: &Value, idx: usize, at: &str) -> R<Trigger> {
    let o = v
        .as_object()
        .ok_or_else(|| format!("expected a dictionary @ data{at}"))?;
    let platform = o
        .get("trigger")
        .or_else(|| o.get("platform"))
        .and_then(Value::as_str)
        .ok_or_else(|| format!("required key not provided @ data{at}['trigger']"))?;
    let id = match o.get("id") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(_) => {
            return Err(format!(
                "expected str for dictionary value @ data{at}['id']"
            ));
        }
        None => idx.to_string(),
    };
    let common = ["trigger", "platform", "id", "enabled", "alias"];
    let kind = match platform {
        "state" => {
            only_keys(o, &[&common[..], &["entity_id", "to", "for"]].concat(), at)?;
            let to = match o.get("to") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(vec![s.clone()]),
                Some(Value::Array(a)) => Some(
                    a.iter()
                        .map(|s| s.as_str().map(str::to_owned))
                        .collect::<Option<_>>()
                        .ok_or_else(|| {
                            format!("expected str for dictionary value @ data{at}['to']")
                        })?,
                ),
                Some(_) => {
                    return Err(format!(
                        "expected str for dictionary value @ data{at}['to']"
                    ));
                }
            };
            TriggerKind::State {
                entity_ids: entity_ids(o.get("entity_id"), &path(at, "entity_id"))?,
                to,
                for_: o
                    .get("for")
                    .map(|f| duration(f, &path(at, "for")))
                    .transpose()?,
            }
        }
        "numeric_state" => {
            only_keys(
                o,
                &[&common[..], &["entity_id", "below", "above"]].concat(),
                at,
            )?;
            let below = float(o.get("below"), &path(at, "below"))?;
            let above = float(o.get("above"), &path(at, "above"))?;
            if below.is_none() && above.is_none() {
                return Err(format!(
                    "must contain at least one of below, above. @ data{at}"
                ));
            }
            TriggerKind::NumericState {
                entity_ids: entity_ids(o.get("entity_id"), &path(at, "entity_id"))?,
                below,
                above,
            }
        }
        "time" => {
            only_keys(o, &[&common[..], &["at"]].concat(), at)?;
            let list = match o.get("at") {
                Some(Value::Array(a)) => a.clone(),
                Some(v) => vec![v.clone()],
                None => return Err(format!("required key not provided @ data{at}['at']")),
            };
            let at_times = list
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    time(v).ok_or_else(|| {
                        format!("Expected HH:MM, HH:MM:SS, an Entity ID with domain 'input_datetime' or 'sensor', a combination of a timestamp sensor entity and an offset, or Limited Template @ data{at}['at'][{i}]")
                    })
                })
                .collect::<R<_>>()?;
            TriggerKind::Time { at: at_times }
        }
        other => return Err(format!("Invalid trigger '{other}' specified")),
    };
    Ok(Trigger { id, idx, kind })
}

fn conditions(v: Option<&Value>, at: &str) -> R<Vec<Condition>> {
    let list = match v {
        None | Some(Value::Null) => return Ok(vec![]),
        Some(Value::Array(a)) => a.clone(),
        Some(o @ Value::Object(_)) => vec![o.clone()],
        Some(_) => return Err(format!("expected a list @ data{at}")),
    };
    // A disabled condition passes: it's left out.
    enabled_items(&list, at)?
        .iter()
        .map(|(i, c)| condition(c, &format!("{at}[{i}]")))
        .collect()
}

fn condition(v: &Value, at: &str) -> R<Condition> {
    let o = v
        .as_object()
        .ok_or_else(|| format!("Unexpected value for condition: '{v}' @ data{at}"))?;
    let kind = o
        .get("condition")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("required key not provided @ data{at}['condition']"))?;
    let common = ["condition", "alias", "enabled"];
    Ok(match kind {
        "numeric_state" => {
            only_keys(
                o,
                &[&common[..], &["entity_id", "below", "above"]].concat(),
                at,
            )?;
            let below = float(o.get("below"), &path(at, "below"))?;
            let above = float(o.get("above"), &path(at, "above"))?;
            if below.is_none() && above.is_none() {
                return Err(format!(
                    "must contain at least one of below, above. @ data{at}"
                ));
            }
            Condition::NumericState {
                entity_ids: entity_ids(o.get("entity_id"), &path(at, "entity_id"))?,
                below,
                above,
            }
        }
        "state" => {
            only_keys(o, &[&common[..], &["entity_id", "state"]].concat(), at)?;
            let states = match o.get("state") {
                Some(Value::String(s)) => vec![s.clone()],
                Some(Value::Array(a)) => a
                    .iter()
                    .map(|s| s.as_str().map(str::to_owned))
                    .collect::<Option<_>>()
                    .ok_or_else(|| format!("expected str @ data{at}['state']"))?,
                _ => {
                    return Err(format!(
                        "expected str for dictionary value @ data{at}['state']"
                    ));
                }
            };
            Condition::State {
                entity_ids: entity_ids(o.get("entity_id"), &path(at, "entity_id"))?,
                states,
            }
        }
        "time" => {
            only_keys(o, &[&common[..], &["after", "before"]].concat(), at)?;
            let t = |k: &str| -> R<Option<jiff::civil::Time>> {
                o.get(k)
                    .map(|v| {
                        time(v)
                            .ok_or_else(|| format!("Invalid time specified: {v} @ data{at}['{k}']"))
                    })
                    .transpose()
            };
            let (after, before) = (t("after")?, t("before")?);
            if after.is_none() && before.is_none() {
                return Err(format!(
                    "must contain at least one of before, after, weekday. @ data{at}"
                ));
            }
            Condition::Time { after, before }
        }
        "trigger" => {
            only_keys(o, &[&common[..], &["id"]].concat(), at)?;
            let ids = match o.get("id") {
                Some(Value::String(s)) => vec![s.clone()],
                Some(Value::Number(n)) => vec![n.to_string()],
                Some(Value::Array(a)) => a
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => Some(s.clone()),
                        Value::Number(n) => Some(n.to_string()),
                        _ => None,
                    })
                    .collect::<Option<_>>()
                    .ok_or_else(|| format!("expected str @ data{at}['id']"))?,
                _ => return Err(format!("required key not provided @ data{at}['id']")),
            };
            Condition::Trigger { ids }
        }
        other => {
            return Err(format!(
                "Invalid condition \"{other}\" specified @ data{at}"
            ));
        }
    })
}

fn actions(v: Option<&Value>, at: &str) -> R<Vec<Action>> {
    let list = match v {
        Some(Value::Array(a)) => a.clone(),
        Some(o @ Value::Object(_)) => vec![o.clone()],
        _ => return Err(format!("required key not provided @ data{at}")),
    };
    // A disabled action is skipped.
    enabled_items(&list, at)?
        .iter()
        .map(|(i, a)| action(a, &format!("{at}[{i}]")))
        .collect()
}

fn action(v: &Value, at: &str) -> R<Action> {
    let o = v
        .as_object()
        .ok_or_else(|| format!("expected a dictionary @ data{at}"))?;
    if let Some(name) = o.get("action").or_else(|| o.get("service")) {
        only_keys(
            o,
            &["action", "service", "target", "data", "alias", "enabled"],
            at,
        )?;
        let name = name.as_str().unwrap_or_default();
        let (domain, service) = name
            .split_once('.')
            .filter(|(d, s)| !d.is_empty() && !s.is_empty())
            .ok_or_else(|| {
                format!("Service {name} does not match format <domain>.<name> @ data{at}['action']")
            })?;
        let data = match o.get("data") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(d)) => d.clone(),
            Some(_) => return Err(format!("expected a dictionary @ data{at}['data']")),
        };
        let mut target = Map::new();
        match o.get("target") {
            None | Some(Value::Null) => {}
            Some(Value::Object(t)) => {
                for (k, v) in t {
                    // `cv.comp_entity_ids`: always a list.
                    let list = match v {
                        Value::String(s) => Value::Array(
                            s.split(',')
                                .map(|x| Value::String(x.trim().to_owned()))
                                .collect(),
                        ),
                        other => other.clone(),
                    };
                    target.insert(k.clone(), list);
                }
            }
            Some(_) => return Err(format!("expected a dictionary @ data{at}['target']")),
        }
        return Ok(Action::Call {
            domain: domain.to_lowercase(),
            service: service.to_lowercase(),
            data,
            target,
        });
    }
    if let Some(options) = o.get("choose") {
        only_keys(o, &["choose", "default", "alias", "enabled"], at)?;
        let list = match options {
            Value::Array(a) => a.clone(),
            o @ Value::Object(_) => vec![o.clone()],
            _ => return Err(format!("expected a list @ data{at}['choose']")),
        };
        let options = list
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let p = format!("{at}['choose'][{i}]");
                let c = c
                    .as_object()
                    .ok_or_else(|| format!("expected a dictionary @ data{p}"))?;
                only_keys(c, &["conditions", "sequence", "alias"], &p)?;
                Ok((
                    conditions(c.get("conditions"), &path(&p, "conditions"))?,
                    actions(c.get("sequence"), &path(&p, "sequence"))?,
                ))
            })
            .collect::<R<_>>()?;
        let default = match o.get("default") {
            None | Some(Value::Null) => vec![],
            d => actions(d, &path(at, "default"))?,
        };
        return Ok(Action::Choose { options, default });
    }
    if let Some(t) = o.get("wait_for_trigger") {
        only_keys(o, &["wait_for_trigger", "alias", "enabled"], at)?;
        let triggers = triggers(Some(t), &path(at, "wait_for_trigger"))?;
        if triggers
            .iter()
            .any(|t| !matches!(t.kind, TriggerKind::State { .. }))
        {
            return Err(format!(
                "wait_for_trigger supports state triggers only in hars @ data{at}"
            ));
        }
        return Ok(Action::WaitForTrigger { triggers });
    }
    Err(format!("Unable to determine action @ data{at}"))
}

/// Validate one automation's config (an item of `automations.yaml`).
pub fn parse(v: &Value) -> Result<Config, String> {
    reject_templates(v)?;
    let o = v.as_object().ok_or("expected a dictionary")?;
    only_keys(
        o,
        &[
            "id",
            "alias",
            "description",
            "mode",
            "max",
            "max_exceeded",
            "initial_state",
            "trace",
            "variables",
            "triggers",
            "trigger",
            "conditions",
            "condition",
            "actions",
            "action",
        ],
        "",
    )?;
    if o.contains_key("variables") {
        return Err("variables are not supported by hars yet".into());
    }
    let id = match o.get("id") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(_) => return Err("expected str for dictionary value @ data['id']".into()),
    };
    let alias = match o.get("alias") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(v) => Some(v.to_string()),
    };
    let mode = match o.get("mode").and_then(Value::as_str) {
        None | Some("single") => Mode::Single,
        Some("restart") => Mode::Restart,
        Some(m) => return Err(format!("mode {m} is not supported by hars yet")),
    };
    let initial_state = match o.get("initial_state") {
        None => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return Err("expected boolean @ data['initial_state']".into()),
    };
    let tkey = if o.contains_key("triggers") {
        "triggers"
    } else {
        "trigger"
    };
    let ckey = if o.contains_key("conditions") {
        "conditions"
    } else {
        "condition"
    };
    let akey = if o.contains_key("actions") {
        "actions"
    } else {
        "action"
    };
    Ok(Config {
        id,
        alias,
        mode,
        initial_state,
        triggers: triggers(o.get(tkey), &format!("['{tkey}']"))?,
        conditions: conditions(o.get(ckey), &format!("['{ckey}']"))?,
        actions: actions(o.get(akey), &format!("['{akey}']"))?,
    })
}

//! An automation's config, validated into what the engine runs. The
//! subset the inventory uses: state, numeric_state, time and template
//! triggers; numeric_state, state, time, trigger and template conditions;
//! service calls, choose and wait_for_trigger; single and restart modes;
//! `variables` and `trigger_variables`.
//!
//! Templates are HA's: a string with `{{`, `{%` or `{#` where HA's schema
//! takes a template (`cv.template`, `cv.dynamic_template`,
//! `cv.template_complex`) is compiled here, once, and a template that
//! doesn't compile makes the automation invalid, as HA's validation does.
//! Elsewhere such a string is plain text, as in HA (a state trigger's
//! `to`). Anything hars can't run yet fails validation, so the automation
//! is `unavailable`, as HA makes an invalid one: templates in `for`, `at`
//! and `enabled`, a trigger's own `variables`, and the actions other than
//! those above (`delay`, `wait_template`, `if`, `repeat`, …).
//!
//! Compiling recurses as deep as the template nests, so [`parse`] runs on
//! a render thread ([`ha_core::template::run`]) when an engine loads.

use ha_core::template::{Mode as Env, Template};
use serde_json::{Map, Value};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

/// The run mode.
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

/// A compiled template from the config.
#[derive(Clone)]
pub struct Tpl(pub Arc<Template>);

impl fmt::Debug for Tpl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Template({:?})", self.0.source())
    }
}

impl PartialEq for Tpl {
    fn eq(&self, other: &Self) -> bool {
        self.0.source() == other.0.source()
    }
}

impl std::ops::Deref for Tpl {
    type Target = Template;
    fn deref(&self) -> &Template {
        &self.0
    }
}

/// A value that may hold templates (`cv.template_complex`): rendered with
/// `render_complex`.
#[derive(Debug, Clone, PartialEq)]
pub enum Complex {
    /// No template anywhere in it.
    Static(Value),
    Template(Tpl),
    List(Vec<Complex>),
    /// Keys may be templates too.
    Map(Vec<(Complex, Complex)>),
}

impl Complex {
    pub fn is_static(&self) -> bool {
        matches!(self, Complex::Static(_))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TriggerKind {
    State {
        entity_ids: Vec<String>,
        to: Option<Vec<String>>,
        for_: Option<Duration>,
        /// No `to` key at all: any change fires, an attribute's too.
        match_all: bool,
    },
    NumericState {
        entity_ids: Vec<String>,
        below: Option<f64>,
        above: Option<f64>,
        value_template: Option<Tpl>,
    },
    /// One `at` each (HA attaches one tracker per time).
    Time { at: Vec<jiff::civil::Time> },
    /// The `template` integration's trigger.
    Template {
        value_template: Tpl,
        for_: Option<Duration>,
        /// How often the template has rendered (HA's `Template._renders`,
        /// which its error message shows).
        renders: Renders,
    },
}

/// A render count, shared by every attachment of a trigger.
#[derive(Debug, Clone, Default)]
pub struct Renders(pub Arc<std::sync::atomic::AtomicU64>);

impl PartialEq for Renders {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Trigger {
    /// `id`, or the index as a string.
    pub id: String,
    pub idx: usize,
    pub alias: Option<String>,
    pub kind: TriggerKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    NumericState {
        entity_ids: Vec<String>,
        below: Option<f64>,
        above: Option<f64>,
        value_template: Option<Tpl>,
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
    /// `condition: template`, or a template string (the shorthand).
    Template(Tpl),
}

impl Condition {
    pub fn has_template(&self) -> bool {
        matches!(
            self,
            Condition::Template(_)
                | Condition::NumericState {
                    value_template: Some(_),
                    ..
                }
        )
    }
}

/// What a service call action calls: `domain.service`, or a template
/// rendering it.
#[derive(Debug, Clone, PartialEq)]
pub enum Service {
    Static { domain: String, service: String },
    Template(Tpl),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Call {
        service: Service,
        /// `data`, then `data_template`, each a dict (or one template).
        data: Vec<Complex>,
        /// The target (a static one has `entity_id` as a list), or one
        /// template.
        target: Option<Complex>,
    },
    Choose {
        options: Vec<(Vec<Condition>, Vec<Action>)>,
        default: Vec<Action>,
    },
    WaitForTrigger {
        triggers: Vec<Trigger>,
    },
}

impl Action {
    /// A call with nothing to render.
    pub fn is_static_call(&self) -> bool {
        matches!(self, Action::Call { service: Service::Static { .. }, data, target }
            if data.iter().all(Complex::is_static) && target.as_ref().is_none_or(Complex::is_static))
    }
}

/// A variable of `variables` or `trigger_variables`: its name, value, and
/// whether it renders limited (`trigger_variables`' do, always: HA
/// compiles them limited when the triggers attach).
#[derive(Debug, Clone, PartialEq)]
pub struct Variable {
    pub name: String,
    pub value: Complex,
    pub limited: bool,
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
    /// `trigger_variables` (rendered when the triggers attach).
    pub trigger_variables: Vec<Variable>,
    /// `trigger_variables` then `variables` (a name in both: `variables`'
    /// value, in `trigger_variables`' place), rendered for each run.
    pub variables: Vec<Variable>,
    /// Whether any template or variable is used: the runs then carry
    /// HA's `this` and `trigger` variables.
    pub templated: bool,
}

impl Config {
    /// Whether a trigger needs a render before its run starts (variables or
    /// conditions with templates).
    pub fn renders_before_run(&self) -> bool {
        self.variables.iter().any(|v| !v.value.is_static())
            || self.conditions.iter().any(Condition::has_template)
    }
}

type R<T> = Result<T, String>;

fn path(p: &str, k: &str) -> String {
    format!("{p}['{k}']")
}

/// `is_template_string`.
pub fn is_template_string(s: &str) -> bool {
    s.contains('{') && (s.contains("{%") || s.contains("{{") || s.contains("{#"))
}

/// Parsing context: whether a template was seen.
#[derive(Default)]
struct Ctx {
    templated: bool,
}

impl Ctx {
    /// `cv.template`: a string (static or not), compiled now.
    fn template(&mut self, v: &Value, mode: Env, at: &str) -> R<Tpl> {
        let s = match v {
            Value::Null => return Err(format!("template value is None @ data{at}")),
            Value::Array(_) | Value::Object(_) => {
                return Err(format!("template value should be a string @ data{at}"));
            }
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let t = Template::new(&s);
        t.compile_in(mode)
            .map_err(|e| format!("invalid template ({e}) @ data{at}"))?;
        self.templated = true;
        Ok(Tpl(Arc::new(t)))
    }

    /// `cv.dynamic_template`.
    fn dynamic_template(&mut self, v: &Value, mode: Env, at: &str) -> R<Tpl> {
        if let Value::String(s) = v
            && !is_template_string(s)
        {
            return Err(format!(
                "template value does not contain a dynamic template @ data{at}"
            ));
        }
        self.template(v, mode, at)
    }

    /// `cv.template_complex`.
    fn complex(&mut self, v: &Value, mode: Env, at: &str) -> R<Complex> {
        Ok(match v {
            Value::String(s) if is_template_string(s) => {
                Complex::Template(self.template(v, mode, at)?)
            }
            Value::Array(items) => {
                let items = items
                    .iter()
                    .enumerate()
                    .map(|(i, x)| self.complex(x, mode, &format!("{at}[{i}]")))
                    .collect::<R<Vec<_>>>()?;
                if items.iter().all(Complex::is_static) {
                    Complex::Static(v.clone())
                } else {
                    Complex::List(items)
                }
            }
            Value::Object(m) => {
                let pairs = m
                    .iter()
                    .map(|(k, x)| {
                        let key = self.complex(&Value::String(k.clone()), mode, at)?;
                        Ok((key, self.complex(x, mode, &path(at, k))?))
                    })
                    .collect::<R<Vec<_>>>()?;
                if pairs.iter().all(|(k, x)| k.is_static() && x.is_static()) {
                    Complex::Static(v.clone())
                } else {
                    Complex::Map(pairs)
                }
            }
            other => Complex::Static(other.clone()),
        })
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

/// A template where hars doesn't render one yet.
fn no_template_here(v: &Value, what: &str, at: &str) -> R<()> {
    let templated = match v {
        Value::String(s) => is_template_string(s),
        Value::Object(o) => o
            .iter()
            .any(|(k, x)| is_template_string(k) || x.as_str().is_some_and(is_template_string)),
        _ => false,
    };
    if templated {
        return Err(format!(
            "templates in '{what}' are not supported by hars yet @ data{at}"
        ));
    }
    Ok(())
}

/// `cv.positive_time_period`: `{hours, minutes, seconds, milliseconds,
/// days}`, `"HH:MM[:SS]"`, or seconds.
fn duration(v: &Value, at: &str) -> R<Duration> {
    no_template_here(v, "for", at)?;
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

/// `enabled`: a bool, default true (templates aren't supported yet).
fn is_enabled(v: &Value, at: &str) -> R<bool> {
    match v.get("enabled") {
        None => Ok(true),
        Some(Value::Bool(b)) => Ok(*b),
        Some(e) => {
            no_template_here(e, "enabled", &path(at, "enabled"))?;
            Err(format!("expected boolean @ data{at}['enabled']"))
        }
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

/// HA's `_MINIMAL_PLATFORM_SCHEMA`: an `id` is a string (`str`, on
/// purpose), an alias and a description are text (`cv.string`: a number
/// will do, nothing, a list or a mapping won't). HA drops an automation
/// that fails this altogether; one that only fails the rest is shown
/// `unavailable`.
pub fn minimal(item: &Value) -> Result<(), String> {
    let Some(o) = item.as_object() else {
        return Ok(());
    };
    if o.get("id").is_some_and(|id| !id.is_string()) {
        return Err("expected str for dictionary value @ data['id']".into());
    }
    for key in ["alias", "description"] {
        match o.get(key) {
            Some(Value::Null) => {
                return Err(format!(
                    "string value is None for dictionary value @ data['{key}']"
                ));
            }
            Some(Value::Array(_) | Value::Object(_)) => {
                return Err(format!(
                    "value should be a string for dictionary value @ data['{key}']"
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn alias(o: &Map<String, Value>) -> Option<String> {
    match o.get("alias") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(v) => Some(v.to_string()),
    }
}

impl Ctx {
    fn triggers(&mut self, v: Option<&Value>, at: &str) -> R<Vec<Trigger>> {
        let list = match v {
            Some(Value::Array(a)) => a.clone(),
            Some(Value::Object(o)) => vec![Value::Object(o.clone())],
            _ => return Err(format!("required key not provided @ data{at}")),
        };
        // A disabled trigger isn't attached; the others keep their index.
        enabled_items(&list, at)?
            .iter()
            .map(|(idx, t)| self.trigger(t, *idx, &format!("{at}[{idx}]")))
            .collect()
    }

    fn trigger(&mut self, v: &Value, idx: usize, at: &str) -> R<Trigger> {
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
        if o.contains_key("variables") {
            return Err(format!(
                "a trigger's 'variables' are not supported by hars yet @ data{at}"
            ));
        }
        // From here it is the trigger platform's own schema, which HA
        // validates one trigger at a time: its errors name the place in
        // the trigger, not in the automation.
        let at = "";
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
                    match_all: !o.contains_key("to"),
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
                    &[
                        &common[..],
                        &["entity_id", "below", "above", "value_template"],
                    ]
                    .concat(),
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
                    value_template: o
                        .get("value_template")
                        .map(|t| self.template(t, Env::Lenient, &path(at, "value_template")))
                        .transpose()?,
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
                        let here = format!("{at}['at'][{i}]");
                        no_template_here(v, "at", &here)?;
                        time(v).ok_or_else(|| {
                            format!("Expected HH:MM, HH:MM:SS, an Entity ID with domain 'input_datetime' or 'sensor', a combination of a timestamp sensor entity and an offset, or Limited Template @ data{here}")
                        })
                    })
                    .collect::<R<_>>()?;
                TriggerKind::Time { at: at_times }
            }
            "template" => {
                only_keys(o, &[&common[..], &["value_template", "for"]].concat(), at)?;
                let value_template = match o.get("value_template") {
                    Some(t) => self.template(t, Env::Lenient, &path(at, "value_template"))?,
                    None => {
                        return Err(format!(
                            "required key not provided @ data{at}['value_template']"
                        ));
                    }
                };
                TriggerKind::Template {
                    value_template,
                    renders: Renders::default(),
                    for_: o
                        .get("for")
                        .map(|f| duration(f, &path(at, "for")))
                        .transpose()?,
                }
            }
            other => return Err(format!("Invalid trigger '{other}' specified")),
        };
        Ok(Trigger {
            id,
            idx,
            alias: alias(o),
            kind,
        })
    }

    fn conditions(&mut self, v: Option<&Value>, at: &str) -> R<Vec<Condition>> {
        let list = match v {
            None | Some(Value::Null) => return Ok(vec![]),
            Some(Value::Array(a)) => a.clone(),
            Some(c @ (Value::Object(_) | Value::String(_))) => vec![c.clone()],
            Some(_) => return Err(format!("expected a list @ data{at}")),
        };
        // A disabled condition passes: it's left out.
        enabled_items(&list, at)?
            .iter()
            .map(|(i, c)| self.condition(c, &format!("{at}[{i}]")))
            .collect()
    }

    fn condition(&mut self, v: &Value, at: &str) -> R<Condition> {
        // The shorthand: a template string is a template condition.
        if let Value::String(s) = v {
            if !is_template_string(s) {
                return Err(format!("Expected a dictionary @ data{at}. Got '{s}'"));
            }
            return Ok(Condition::Template(self.dynamic_template(
                v,
                Env::Lenient,
                at,
            )?));
        }
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
                    &[
                        &common[..],
                        &["entity_id", "below", "above", "value_template"],
                    ]
                    .concat(),
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
                    value_template: o
                        .get("value_template")
                        .map(|t| self.template(t, Env::Lenient, &path(at, "value_template")))
                        .transpose()?,
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
                            time(v).ok_or_else(|| {
                                format!("Invalid time specified: {v} @ data{at}['{k}']")
                            })
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
            "template" => {
                only_keys(o, &[&common[..], &["value_template"]].concat(), at)?;
                match o.get("value_template") {
                    Some(t) => Condition::Template(self.template(
                        t,
                        Env::Lenient,
                        &path(at, "value_template"),
                    )?),
                    None => {
                        return Err(format!(
                            "required key not provided @ data{at}['value_template']"
                        ));
                    }
                }
            }
            other => {
                return Err(format!(
                    "Invalid condition \"{other}\" specified @ data{at}"
                ));
            }
        })
    }

    fn actions(&mut self, v: Option<&Value>, at: &str) -> R<Vec<Action>> {
        let list = match v {
            Some(Value::Array(a)) => a.clone(),
            Some(o @ Value::Object(_)) => vec![o.clone()],
            _ => return Err(format!("required key not provided @ data{at}")),
        };
        // A disabled action is skipped.
        enabled_items(&list, at)?
            .iter()
            .map(|(i, a)| self.action(a, &format!("{at}[{i}]")))
            .collect()
    }

    fn action(&mut self, v: &Value, at: &str) -> R<Action> {
        let o = v
            .as_object()
            .ok_or_else(|| format!("expected a dictionary @ data{at}"))?;
        if o.contains_key("action") && o.contains_key("service") {
            return Err(format!(
                "Cannot specify both 'service' and 'action'. Please use 'action' only. @ data{at}"
            ));
        }
        if let Some(name) = o.get("action").or_else(|| o.get("service")) {
            only_keys(
                o,
                &[
                    "action",
                    "service",
                    "target",
                    "data",
                    "data_template",
                    "alias",
                    "enabled",
                    "metadata",
                ],
                at,
            )?;
            // "The frontend stores data here. Don't use in core."
            // (`vol.Remove("metadata"): dict`).
            if o.get("metadata").is_some_and(|m| !m.is_object()) {
                return Err(format!(
                    "expected dict for dictionary value @ data{at}['metadata']"
                ));
            }
            let service = match name.as_str() {
                Some(s) if is_template_string(s) => Service::Template(self.dynamic_template(
                    name,
                    Env::Lenient,
                    &path(at, "action"),
                )?),
                _ => {
                    let name = name.as_str().unwrap_or_default();
                    let (domain, service) = name
                        .split_once('.')
                        .filter(|(d, s)| !d.is_empty() && !s.is_empty())
                        .ok_or_else(|| {
                            format!("Service {name} does not match format <domain>.<name> for dictionary value @ data{at}['action']")
                        })?;
                    Service::Static {
                        domain: domain.to_lowercase(),
                        service: service.to_lowercase(),
                    }
                }
            };
            let mut data = Vec::new();
            for key in ["data", "data_template"] {
                match o.get(key) {
                    None | Some(Value::Null) => {}
                    // `vol.Any(template, vol.All(dict, template_complex))`.
                    Some(d @ Value::Object(_)) => {
                        data.push(self.complex(d, Env::Lenient, &path(at, key))?);
                    }
                    Some(d @ Value::String(_)) => {
                        data.push(Complex::Template(self.template(
                            d,
                            Env::Lenient,
                            &path(at, key),
                        )?));
                    }
                    Some(_) => return Err(format!("expected a dictionary @ data{at}['{key}']")),
                }
            }
            let target = match o.get("target") {
                None | Some(Value::Null) => None,
                Some(t @ Value::String(_)) => Some(Complex::Template(self.dynamic_template(
                    t,
                    Env::Lenient,
                    &path(at, "target"),
                )?)),
                Some(Value::Object(t)) => {
                    let mut out = Map::new();
                    let mut templated = false;
                    for (k, v) in t {
                        let c = self.complex(v, Env::Lenient, &path(&path(at, "target"), k))?;
                        templated |= !c.is_static();
                        // `cv.comp_entity_ids`: always a list.
                        let list = match v {
                            Value::String(s) if c.is_static() => Value::Array(
                                s.split(',')
                                    .map(|x| Value::String(x.trim().to_owned()))
                                    .collect(),
                            ),
                            other => other.clone(),
                        };
                        out.insert(k.clone(), list);
                    }
                    Some(if templated {
                        self.complex(&Value::Object(t.clone()), Env::Lenient, &path(at, "target"))?
                    } else {
                        Complex::Static(Value::Object(out))
                    })
                }
                Some(_) => return Err(format!("expected a dictionary @ data{at}['target']")),
            };
            return Ok(Action::Call {
                service,
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
                        self.conditions(c.get("conditions"), &path(&p, "conditions"))?,
                        self.actions(c.get("sequence"), &path(&p, "sequence"))?,
                    ))
                })
                .collect::<R<_>>()?;
            let default = match o.get("default") {
                None | Some(Value::Null) => vec![],
                d => self.actions(d, &path(at, "default"))?,
            };
            return Ok(Action::Choose { options, default });
        }
        if let Some(t) = o.get("wait_for_trigger") {
            only_keys(o, &["wait_for_trigger", "alias", "enabled"], at)?;
            let triggers = self.triggers(Some(t), &path(at, "wait_for_trigger"))?;
            if triggers.iter().any(|t| {
                !matches!(
                    t.kind,
                    TriggerKind::State { .. } | TriggerKind::Template { .. }
                )
            }) {
                return Err(format!(
                    "wait_for_trigger supports state and template triggers only in hars @ data{at}"
                ));
            }
            return Ok(Action::WaitForTrigger { triggers });
        }
        // HA's other actions (`cv.determine_script_action`).
        for (key, name) in [
            ("delay", "delay"),
            ("wait_template", "wait_template"),
            ("event", "event"),
            ("condition", "condition"),
            ("if", "if"),
            ("repeat", "repeat"),
            ("parallel", "parallel"),
            ("sequence", "sequence"),
            ("variables", "variables"),
            ("stop", "stop"),
            ("scene", "scene"),
            ("device_id", "device"),
            ("set_conversation_response", "set_conversation_response"),
        ] {
            if o.contains_key(key) {
                return Err(format!(
                    "the '{name}' action is not supported by hars yet @ data{at}"
                ));
            }
        }
        Err(format!("Unable to determine action @ data{at}"))
    }

    /// `variables` or `trigger_variables`: a dict (`cv.SCRIPT_VARIABLES_SCHEMA`),
    /// in the order written (`order`), any other names after, sorted.
    fn variables(
        &mut self,
        v: &Value,
        limited: bool,
        key: &str,
        order: &[String],
    ) -> R<Vec<Variable>> {
        let Value::Object(m) = v else {
            return Err(format!("expected a dictionary @ data['{key}']"));
        };
        let mode = if limited { Env::Limited } else { Env::Lenient };
        let names = order
            .iter()
            .filter(|n| m.contains_key(n.as_str()))
            .chain(m.keys().filter(|k| !order.contains(k)));
        let out = names
            .map(|name| {
                Ok(Variable {
                    name: name.clone(),
                    value: self.complex(&m[name], mode, &path(&format!("['{key}']"), name))?,
                    limited,
                })
            })
            .collect::<R<Vec<_>>>()?;
        self.templated = true;
        Ok(out)
    }
}

/// The names of `trigger_variables` and `variables` in the order written:
/// HA renders them in that order, each seeing the ones before it, and a
/// JSON map here keeps its keys sorted.
#[derive(Debug, Clone, Default)]
pub struct Order {
    pub trigger_variables: Vec<String>,
    pub variables: Vec<String>,
}

impl Order {
    /// The order in an item as loaded.
    pub fn of(item: &ha_core::yaml::Yaml) -> Order {
        let names = |key: &str| match item.get(key) {
            Some(ha_core::yaml::Yaml::Map(pairs)) => pairs
                .iter()
                .filter_map(|(k, _)| k.as_str().map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        };
        Order {
            trigger_variables: names("trigger_variables"),
            variables: names("variables"),
        }
    }
}

/// Validate one automation's config (an item of `automations.yaml`), its
/// variables in sorted order.
pub fn parse(v: &Value) -> Result<Config, String> {
    parse_ordered(v, &Order::default())
}

/// [`parse`], with the variables in the order written.
pub fn parse_ordered(v: &Value, order: &Order) -> Result<Config, String> {
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
            "trigger_variables",
            "triggers",
            "trigger",
            "conditions",
            "condition",
            "actions",
            "action",
        ],
        "",
    )?;
    minimal(v)?;
    let mut ctx = Ctx::default();
    let id = o.get("id").and_then(Value::as_str).map(str::to_owned);
    let alias = alias(o);
    let mode = match o.get("mode") {
        None => Mode::Single,
        Some(Value::String(m)) => match m.as_str() {
            "single" => Mode::Single,
            "restart" => Mode::Restart,
            "queued" | "parallel" => {
                return Err(format!("mode {m} is not supported by hars yet"));
            }
            _ => {
                return Err("value must be one of ['parallel', 'queued', 'restart', 'single'] for dictionary value @ data['mode']".into());
            }
        },
        Some(_) => {
            return Err("value must be one of ['parallel', 'queued', 'restart', 'single'] for dictionary value @ data['mode']".into());
        }
    };
    let initial_state = match o.get("initial_state") {
        None => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return Err("expected boolean @ data['initial_state']".into()),
    };
    let trigger_variables = match o.get("trigger_variables") {
        None => vec![],
        Some(v) => ctx.variables(v, true, "trigger_variables", &order.trigger_variables)?,
    };
    let own = match o.get("variables") {
        None => vec![],
        Some(v) => ctx.variables(v, false, "variables", &order.variables)?,
    };
    // `ScriptVariables(trigger_variables)` updated with `variables`.
    let mut variables = trigger_variables.clone();
    for v in own {
        match variables.iter_mut().find(|x| x.name == v.name) {
            Some(slot) => *slot = v,
            None => variables.push(v),
        }
    }
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
    // HA renames the old keys first, so its errors name the new ones.
    let triggers = ctx.triggers(o.get(tkey), "['triggers']")?;
    let conditions = ctx.conditions(o.get(ckey), "['conditions']")?;
    let actions = ctx.actions(o.get(akey), "['actions']")?;
    Ok(Config {
        id,
        alias,
        mode,
        initial_state,
        triggers,
        conditions,
        actions,
        trigger_variables,
        variables,
        templated: ctx.templated,
    })
}

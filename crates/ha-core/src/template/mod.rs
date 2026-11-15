//! Jinja templates: Home Assistant's `helpers/template`, on minijinja.
//!
//! This is the first slice: the template language with HA's environment
//! settings, and the result handling the WebSocket `render_template`
//! command needs. What matches HA 2026.6.1:
//!
//! - **Source and output.** The source is stripped before compiling and
//!   the output after rendering; a source without `{{`, `{%` or `{#` is
//!   static and returned as is. Output over 256 KiB is an error. The
//!   result goes through [`parse_result`] (`literal_eval`), so `{{ 1 + 1 }}`
//!   renders the number 2.
//! - **Undefined.** Lenient mode prints an undefined value as an empty
//!   string and logs a warning (`LoggingUndefined`); using it any other way
//!   is an error, logged first. Strict mode (`StrictUndefined`) makes any
//!   use of it an error. Messages are Jinja's (`'user' is undefined`,
//!   `'dict object' has no attribute 'x'`), and errors read as
//!   `str(TemplateError)` does (`UndefinedError: …`).
//! - **Environment.** No autoescaping, Jinja's default whitespace handling,
//!   `break`/`continue` and `do`, Python's `str()` for printed values
//!   (`True`, `None`, `1e+20`, `['a']`), `range` capped at 100000 items.
//!   Jinja's own filters and tests are minijinja's where they give Jinja's
//!   answers (each checked by a vector), and only those.
//! - **Compiling once.** A [`Template`] compiles on first use with the
//!   strictness asked for then, and keeps it, as HA's `_ensure_compiled`
//!   does; compile errors are not kept, so the next use tries again.
//!
//! Not here yet: **HA's own functions, filters and tests** (`states`,
//! `is_state`, `now`, `as_timestamp`, `float`, `round`, …, every name in
//! [`names`] that hars doesn't serve). A template that uses one fails with
//! `NotImplementedError: hars does not support …` instead of rendering:
//! never a wrong value. Templates therefore read no states and no clock,
//! so a render is a pure function of the template and its variables.
//!
//! Deliberate differences, with Jinja's behaviour in brackets: an
//! undefined value tested by `if` or looped over in lenient mode logs no
//! warning (it does); dict keys iterate sorted (in insertion order); a
//! number joined with `~` or `join` prints in minijinja's style for very
//! large or small floats (Python's `1e+20`); syntax errors other than an
//! unknown tag or an unclosed block keep minijinja's wording in Jinja's
//! vocabulary; other runtime errors read `TemplateError: <minijinja's
//! message>` (Python's exception); calling a method on a value
//! (`'a'.upper()`) is not supported yet; an unknown filter or test in a
//! branch that doesn't run is not reported (Jinja fails at compile).

mod literal;
mod names;
mod syntax;
#[cfg(test)]
mod tests;

pub use literal::{Rendered, Unserializable, parse_result};

use crate::py;
use minijinja::value::{Enumerator, Object, ObjectExt, ObjectRepr, Tuple, ValueKind};
use minijinja::{AutoEscape, Environment, ErrorKind, State, UndefinedBehavior, Value};
use serde_json::Map;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, LazyLock, OnceLock};

/// `MAX_TEMPLATE_OUTPUT`, in characters.
const MAX_OUTPUT: usize = 256 * 1024;

/// `str(TemplateError)`: what HA shows for a template that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateError(pub String);

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TemplateError {}

/// A logging level HA's template `log_fn` receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Warning,
    Error,
}

impl Level {
    /// `logging.getLevelName(level)`.
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Warning => "WARNING",
            Level::Error => "ERROR",
        }
    }
}

/// What a render told HA's `log_fn`, in order.
pub type Log = Vec<(Level, String)>;

/// One render: the result or error, and what was logged on the way.
#[derive(Debug)]
pub struct Render {
    pub result: Result<Rendered, TemplateError>,
    pub log: Log,
}

/// HA's `Template`: one template string, compiled on first render.
pub struct Template {
    source: Arc<str>,
    is_static: bool,
    compiled: OnceLock<Bound>,
}

/// A compiled template and the strictness it was compiled with. The
/// template lives in its own copy of the environment, whose filter, test
/// and global tables are shared (`Arc`) with every other copy.
struct Bound {
    strict: bool,
    env: Environment<'static>,
    /// HA functions it names that hars doesn't have.
    unsupported: Box<[&'static str]>,
}

/// The name a template has in its environment (never shown).
const NAME: &str = "t";

impl Template {
    /// `Template(template, hass)`.
    pub fn new(source: &str) -> Template {
        Template {
            source: py::strip(source).into(),
            // `is_template_string`, on the unstripped string.
            is_static: !(source.contains('{')
                && (source.contains("{%") || source.contains("{{") || source.contains("{#"))),
            compiled: OnceLock::new(),
        }
    }

    /// The source, stripped.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// No `{{`, `{%` or `{#`: rendered as the (stripped) source.
    pub fn is_static(&self) -> bool {
        self.is_static
    }

    /// `ensure_valid` + `_ensure_compiled(strict)`: compile with `strict`
    /// unless already compiled (the first strictness stays).
    pub fn compile(&self, strict: bool) -> Result<(), TemplateError> {
        self.bound(strict).map(|_| ())
    }

    fn bound(&self, strict: bool) -> Result<&Bound, TemplateError> {
        if let Some(bound) = self.compiled.get() {
            return Ok(bound);
        }
        let mut env = if strict { &ENVS.strict } else { &ENVS.lenient }.clone();
        env.add_template_owned(NAME, self.source.to_string())
            .map_err(|e| {
                let detail = e.detail().unwrap_or_default();
                TemplateError(format!(
                    "TemplateSyntaxError: {}",
                    syntax::message(&self.source, detail)
                ))
            })?;
        let used = env
            .get_template(NAME)
            .map(|t| t.undeclared_variables(false))
            .unwrap_or_default();
        let unsupported = names::GLOBALS
            .iter()
            .copied()
            .filter(|n| !SERVED_GLOBALS.contains(n) && used.contains(*n))
            .collect();
        let _ = self.compiled.set(Bound {
            strict,
            env,
            unsupported,
        });
        Ok(self.compiled.get().expect("just set"))
    }

    /// `async_render_to_info(variables, strict=strict).result()`, as
    /// `render_template` uses it: the source for a static template,
    /// otherwise the stripped, parsed output.
    pub fn render(
        &self,
        variables: Option<&Map<String, serde_json::Value>>,
        strict: bool,
    ) -> Render {
        let mut log = Log::new();
        if self.is_static {
            return Render {
                result: Ok(Rendered::text(self.source.to_string())),
                log,
            };
        }
        let result = self
            .bound(strict)
            .and_then(|bound| self.render_bound(bound, variables, &mut log))
            .map(|out| {
                let stripped = py::strip(&out);
                parse_result(if stripped.len() == out.len() {
                    out
                } else {
                    stripped.to_owned()
                })
            });
        Render { result, log }
    }

    /// `async_render_will_timeout`'s render: compiled first (an error
    /// there is returned without rendering), then rendered for its time
    /// and its log only.
    pub fn render_discarding(
        &self,
        variables: Option<&Map<String, serde_json::Value>>,
        strict: bool,
    ) -> Render {
        let mut log = Log::new();
        let result = self.bound(strict).and_then(|bound| {
            self.render_bound(bound, variables, &mut log)
                .map(|_| Rendered::text(String::new()))
        });
        Render { result, log }
    }

    fn render_bound(
        &self,
        bound: &Bound,
        variables: Option<&Map<String, serde_json::Value>>,
        log: &mut Log,
    ) -> Result<String, TemplateError> {
        if let Some(name) = bound
            .unsupported
            .iter()
            .find(|n| variables.is_none_or(|v| !v.contains_key(**n)))
        {
            return Err(unsupported("function", name));
        }
        let root = Value::from_object(Dict {
            entries: variables.map(convert_map).unwrap_or_default(),
            root: true,
        });
        SCRATCH.with_borrow_mut(|s| {
            s.miss = None;
            s.log.clear();
        });
        let mut out = Limited::default();
        let rendered = bound
            .env
            .get_template(NAME)
            .and_then(|t| t.render_captured_to(root, &mut out).map(|_| ()));
        let (miss, logged) =
            SCRATCH.with_borrow_mut(|s| (s.miss.take(), std::mem::take(&mut s.log)));
        log.extend(logged);
        match rendered {
            Ok(_) => Ok(String::from_utf8(out.buf).expect("minijinja writes UTF-8")),
            Err(e) => Err(runtime_error(&e, bound.strict, miss, log)),
        }
    }
}

/// HA's `TemplateError` for a failed render.
fn runtime_error(
    e: &minijinja::Error,
    strict: bool,
    miss: Option<String>,
    log: &mut Log,
) -> TemplateError {
    let detail = e.detail().unwrap_or_default();
    // `LoggingUndefined._fail_with_undefined_error` logs before raising;
    // `StrictUndefined` doesn't.
    let mut undefined = |message: String| {
        if !strict {
            log.push((Level::Error, message.clone()));
        }
        TemplateError(format!("UndefinedError: {message}"))
    };
    match e.kind() {
        ErrorKind::WriteFailure => TemplateError(format!(
            "Template output exceeded maximum size of {MAX_OUTPUT} characters"
        )),
        ErrorKind::UndefinedError => undefined(miss.unwrap_or_else(|| "None is undefined".into())),
        ErrorKind::UnknownFunction => {
            let name = detail.strip_suffix(" is unknown").unwrap_or(detail);
            if let Some(n) = names::GLOBALS.iter().find(|n| **n == name) {
                unsupported("function", n)
            } else {
                let mut message = String::new();
                py::repr_str(&mut message, name);
                message.push_str(" is undefined");
                undefined(message)
            }
        }
        ErrorKind::UnknownFilter => {
            let name = between(detail, "filter ", " is unknown");
            match names::FILTERS.iter().find(|n| **n == name) {
                Some(n) => unsupported("filter", n),
                None => TemplateError(format!("TemplateAssertionError: No filter named '{name}'.")),
            }
        }
        ErrorKind::UnknownTest => {
            let name = between(detail, "test ", " is unknown");
            match names::TESTS.iter().find(|n| **n == name) {
                Some(n) => unsupported("test", n),
                None => TemplateError(format!("TemplateAssertionError: No test named '{name}'.")),
            }
        }
        ErrorKind::UnknownMethod => {
            let name = detail.rsplit(' ').next().unwrap_or(detail);
            TemplateError(format!(
                "NotImplementedError: hars does not support calling the method '{name}' in templates yet"
            ))
        }
        ErrorKind::InvalidOperation if detail == "range has too many elements" => TemplateError(
            "OverflowError: Range too big. The sandbox blocks ranges larger than MAX_RANGE (100000)."
                .into(),
        ),
        // Arithmetic or a comparison on an undefined value.
        ErrorKind::InvalidOperation if detail.contains("undefined") && miss.is_some() => {
            undefined(miss.unwrap_or_default())
        }
        _ => TemplateError(format!("TemplateError: {detail}")),
    }
}

fn between<'a>(s: &'a str, prefix: &str, suffix: &str) -> &'a str {
    let s = s.strip_prefix(prefix).unwrap_or(s);
    s.strip_suffix(suffix).unwrap_or(s)
}

/// A name HA has and hars doesn't serve yet.
fn unsupported(kind: &str, name: &str) -> TemplateError {
    TemplateError(format!(
        "NotImplementedError: hars does not support Home Assistant's template {kind} '{name}' yet"
    ))
}

/// The globals hars serves: Jinja's, which HA keeps.
const SERVED_GLOBALS: &[&str] = &["range", "dict", "namespace"];

/// minijinja's filters that Jinja hasn't got, HA replaces (`round`, `int`,
/// `float`, `bool`), or whose output isn't Jinja's (`pprint`).
const REMOVED_FILTERS: &[&str] = &[
    "split", "lines", "round", "int", "float", "bool", "chain", "zip", "pprint",
];

/// minijinja's tests that Jinja hasn't got, or that would answer about
/// hars's environment rather than HA's (`filter`, `test`).
const REMOVED_TESTS: &[&str] = &[
    "safe",
    "int",
    "startingwith",
    "endingwith",
    "filter",
    "test",
];

/// The two environments (`_ENVIRONMENT`, `_ENVIRONMENT_STRICT`), built on
/// first use and shared by every template.
struct Envs {
    lenient: Environment<'static>,
    strict: Environment<'static>,
}

static ENVS: LazyLock<Envs> = LazyLock::new(|| Envs {
    lenient: environment(UndefinedBehavior::Lenient),
    strict: environment(UndefinedBehavior::Strict),
});

fn environment(undefined: UndefinedBehavior) -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(undefined);
    // `TemplateEnvironment` keeps Jinja's default: no autoescaping.
    env.set_auto_escape_callback(|_| AutoEscape::None);
    env.set_formatter(print_value);
    env.remove_global("debug");
    for f in REMOVED_FILTERS {
        env.remove_filter(f);
    }
    for t in REMOVED_TESTS {
        env.remove_test(t);
    }
    env.add_filter("string", |v: &Value| {
        let mut s = String::new();
        let _ = py_str(&mut s, v);
        Value::from(s)
    });
    let escape = |v: &Value| {
        if v.is_safe() {
            return v.clone();
        }
        let mut s = String::new();
        let _ = py_str(&mut s, v);
        Value::from_safe_string(markupsafe_escape(&s))
    };
    env.add_filter("escape", escape);
    env.add_filter("e", escape);
    env
}

/// What `{{ value }}` writes: Python's `str(value)`. An undefined value
/// only gets here in lenient mode, where it prints nothing and warns.
fn print_value(
    out: &mut minijinja::Output,
    _state: &mut State,
    value: &Value,
) -> Result<(), minijinja::Error> {
    if value.is_undefined() {
        // Without a missing name it is the silent undefined of an inline
        // `if` with no `else`, which Jinja doesn't log either.
        SCRATCH.with_borrow_mut(|s| {
            if let Some(message) = s.miss.take() {
                s.log.push((Level::Warning, message));
            }
        });
        return Ok(());
    }
    // Whatever was missing before this didn't end up printed.
    SCRATCH.with_borrow_mut(|s| s.miss = None);
    py_str(out, value).map_err(|_| minijinja::Error::new(ErrorKind::WriteFailure, "output limit"))
}

/// Python's `str()`.
fn py_str(out: &mut impl fmt::Write, v: &Value) -> fmt::Result {
    match v.as_str() {
        Some(s) if v.kind() == ValueKind::String => out.write_str(s),
        _ => py_repr(out, v),
    }
}

/// Python's `repr()`.
fn py_repr(out: &mut impl fmt::Write, v: &Value) -> fmt::Result {
    match v.kind() {
        ValueKind::Undefined => out.write_str("Undefined"),
        ValueKind::None => out.write_str("None"),
        ValueKind::Bool => out.write_str(if v.is_true() { "True" } else { "False" }),
        ValueKind::Number if !v.is_integer() => match f64::try_from(v.clone()) {
            Ok(f) => out.write_str(&py::repr_float(f)),
            Err(_) => write!(out, "{v}"),
        },
        ValueKind::String => {
            let mut s = String::new();
            py::repr_str(&mut s, v.as_str().unwrap_or_default());
            out.write_str(&s)
        }
        ValueKind::Seq => {
            let tuple = v.downcast_object_ref::<Tuple>().is_some();
            out.write_char(if tuple { '(' } else { '[' })?;
            let mut n = 0;
            if let Ok(items) = v.try_iter() {
                for item in items {
                    if n > 0 {
                        out.write_str(", ")?;
                    }
                    py_repr(out, &item)?;
                    n += 1;
                }
            }
            if tuple && n == 1 {
                out.write_char(',')?;
            }
            out.write_char(if tuple { ')' } else { ']' })
        }
        ValueKind::Map => {
            out.write_char('{')?;
            if let Ok(keys) = v.try_iter() {
                for (i, k) in keys.enumerate() {
                    if i > 0 {
                        out.write_str(", ")?;
                    }
                    py_repr(out, &k)?;
                    out.write_str(": ")?;
                    py_repr(out, &v.get_item(&k).unwrap_or_default())?;
                }
            }
            out.write_char('}')
        }
        _ => write!(out, "{v}"),
    }
}

/// `markupsafe.escape`.
fn markupsafe_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&#39;"),
            '"' => out.push_str("&#34;"),
            c => out.push(c),
        }
    }
    out
}

/// Per render, on the rendering thread: the last missing name or key
/// (for Jinja's undefined messages) and what was logged.
#[derive(Default)]
struct Scratch {
    miss: Option<String>,
    log: Log,
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = RefCell::default();
}

fn note_miss(message: String) {
    SCRATCH.with_borrow_mut(|s| s.miss = Some(message));
}

/// A dict from the variables, or the variables themselves (`root`): a
/// missing key is remembered for Jinja's message about it.
#[derive(Debug)]
struct Dict {
    entries: BTreeMap<String, Value>,
    root: bool,
}

impl Object for Dict {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Map
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        if let Some(found) = key.as_str().and_then(|k| self.entries.get(k)) {
            return Some(found.clone());
        }
        let mut message = String::new();
        match key.as_str() {
            // A global is looked up here first; it isn't missing.
            Some(name) if self.root && SERVED_GLOBALS.contains(&name) => return None,
            Some(name) if self.root => {
                py::repr_str(&mut message, name);
                message.push_str(" is undefined");
            }
            Some(name) => {
                message.push_str("'dict object' has no attribute ");
                py::repr_str(&mut message, name);
            }
            None => {
                message.push_str("dict object has no element ");
                let _ = py_repr(&mut message, key);
            }
        }
        note_miss(message);
        None
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        self.mapped_enumerator(|this| {
            Box::new(this.entries.keys().map(|k| Value::from(k.as_str())))
        })
    }

    fn enumerator_len(self: &Arc<Self>) -> Option<usize> {
        Some(self.entries.len())
    }
}

fn convert_map(m: &Map<String, serde_json::Value>) -> BTreeMap<String, Value> {
    m.iter().map(|(k, v)| (k.clone(), convert(v))).collect()
}

/// JSON (what `variables` arrive as) to a template value.
fn convert(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::from(()),
        serde_json::Value::Bool(b) => Value::from(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::from(i)
            } else if let Some(u) = n.as_u64() {
                Value::from(u)
            } else {
                Value::from(n.as_f64().unwrap_or_default())
            }
        }
        serde_json::Value::String(s) => Value::from(s.as_str()),
        serde_json::Value::Array(items) => {
            Value::from(items.iter().map(convert).collect::<Vec<_>>())
        }
        serde_json::Value::Object(m) => Value::from_object(Dict {
            entries: convert_map(m),
            root: false,
        }),
    }
}

/// The render's output, refusing to grow past `MAX_TEMPLATE_OUTPUT`.
#[derive(Default)]
struct Limited {
    buf: Vec<u8>,
    chars: usize,
}

impl std::io::Write for Limited {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        // UTF-8 characters: bytes that don't continue one.
        self.chars += b.iter().filter(|c| (**c as i8) >= -0x40).count();
        if self.chars > MAX_OUTPUT {
            return Err(std::io::Error::other("template output too long"));
        }
        self.buf.extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

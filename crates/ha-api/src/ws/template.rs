//! `render_template` (`websocket_api/commands.py::handle_render_template`
//! with `helpers/event.py::async_track_template_result`).
//!
//! The reply is a result, then one event: `{result, listeners}`, or with
//! `report_errors` the template's errors and warnings as `{error, level}`
//! events, in HA's order. Any authenticated user may call it (HA has no
//! admin check). Templates are compiled once and kept, 128 at most, as
//! HA's `_cached_template` LRU does; a `report_errors` call compiles its
//! own. A `timeout` renders once on a blocking thread first and answers
//! `template_error` if that takes longer.
//!
//! Templates can't read states or the clock yet ([`ha_core::template`]),
//! so `listeners` is always empty and nothing re-renders: a subscription
//! costs no timer, no listener and no wakeup until it is unsubscribed.
//! A timed-out render keeps its blocking thread until it ends (HA raises
//! an exception into its thread; minijinja can't be interrupted).

use super::commands::{Bad, cv_entity_ids};
use super::{Conn, Invalid, Outbox, Subscription};
use ha_core::template::{Level, Log, Render, Template, TemplateError};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// `functools.lru_cache`'s default `maxsize`.
const CACHE_SIZE: usize = 128;

pub(super) fn dispatch(
    conn: &mut Conn,
    id: u64,
    ty: &str,
    msg: &Map<String, Value>,
) -> Option<Result<(), Invalid>> {
    match ty {
        "render_template" => Some(render_template(conn, id, msg)),
        _ => None,
    }
}

/// `_cached_template`: compiled templates by their exact source.
#[derive(Default)]
pub(crate) struct Cache {
    inner: Mutex<Lru>,
}

#[derive(Default)]
struct Lru {
    templates: HashMap<Box<str>, (Arc<Template>, u64)>,
    clock: u64,
}

impl Cache {
    fn get(&self, source: &str) -> Arc<Template> {
        let mut lru = self.inner.lock();
        lru.clock += 1;
        let now = lru.clock;
        if let Some((template, used)) = lru.templates.get_mut(source) {
            *used = now;
            return template.clone();
        }
        if lru.templates.len() >= CACHE_SIZE
            && let Some(oldest) = lru
                .templates
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone())
        {
            lru.templates.remove(&oldest);
        }
        let template = Arc::new(Template::new(source));
        lru.templates.insert(source.into(), (template.clone(), now));
        template
    }
}

/// The command's arguments.
struct Args {
    template: String,
    variables: Option<Map<String, Value>>,
    timeout: Option<f64>,
    strict: bool,
    report_errors: bool,
}

/// The command's schema. Every error is collected, as voluptuous does; a
/// `TypeError` from `cv.entity_ids` wins.
fn parse(msg: &Map<String, Value>) -> Result<Args, Bad> {
    let mut errors = Vec::new();
    let mut type_error = false;
    let mut args = Args {
        template: String::new(),
        variables: None,
        timeout: None,
        strict: false,
        report_errors: false,
    };
    let mut template = None;
    for (key, value) in msg {
        let expected = |what: &str| format!("expected {what} for dictionary value @ data['{key}']");
        match key.as_str() {
            "id" | "type" => {}
            "template" => match value {
                Value::String(s) => template = Some(s.clone()),
                _ => errors.push(expected("str")),
            },
            // Validated, then unused (as in HA).
            "entity_ids" => match cv_entity_ids(value, "['entity_ids']") {
                Ok(_) => {}
                Err(Bad::Invalid(e)) => errors.extend(e),
                Err(Bad::TypeError) => type_error = true,
            },
            "variables" => match value {
                Value::Object(m) => args.variables = Some(m.clone()),
                _ => errors.push(expected("dict")),
            },
            // `vol.Coerce(float)`.
            "timeout" => match value {
                Value::Number(n) => args.timeout = n.as_f64(),
                Value::Bool(b) => args.timeout = Some(f64::from(u8::from(*b))),
                Value::String(s) => match ha_automation::py_float(s) {
                    Some(f) => args.timeout = Some(f),
                    None => errors.push(expected("float")),
                },
                _ => errors.push(expected("float")),
            },
            "strict" | "report_errors" => match value {
                Value::Bool(b) if key == "strict" => args.strict = *b,
                Value::Bool(b) => args.report_errors = *b,
                _ => errors.push(expected("bool")),
            },
            _ => errors.push(format!("extra keys not allowed @ data['{key}']")),
        }
    }
    if type_error {
        return Err(Bad::TypeError);
    }
    match template {
        Some(t) => args.template = t,
        None if !msg.contains_key("template") => {
            errors.push("required key not provided @ data['template']".into());
        }
        None => {}
    }
    if errors.is_empty() {
        Ok(args)
    } else {
        Err(Bad::Invalid(errors))
    }
}

fn render_template(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    let args = match parse(msg) {
        Ok(args) => args,
        Err(Bad::Invalid(errors)) => return Err(errors.join("\n")),
        Err(Bad::TypeError) => {
            conn.out.error(json!(id), "unknown_error", "Unknown error");
            return Ok(());
        }
    };
    let template = if args.report_errors {
        Arc::new(Template::new(&args.template))
    } else {
        conn.shared.templates.get(&args.template)
    };
    // `if timeout:` and `async_render_will_timeout`'s static shortcut.
    let timeout = args.timeout.filter(|t| *t != 0.0 && !template.is_static());
    let Some(timeout) = timeout else {
        // No awaiting in HA either: everything is queued now, in order.
        let reply = Reply {
            out: &conn.out,
            id,
            report: args.report_errors,
            deferred: false,
        };
        reply.subscribe(&template, args.variables.as_ref(), args.strict);
        conn.subs.insert(id, Subscription::Idle);
        return Ok(());
    };

    // The check runs in the background, as HA's `async_response` does, so
    // the connection keeps serving other commands meanwhile. Until it ends
    // the id isn't subscribed (`unsubscribe_events` says not found), and
    // after a timeout it never is.
    let state = Arc::new(Mutex::new(Later::Pending));
    conn.subs.insert(id, Subscription::Later(state.clone()));
    let out = conn.out.clone();
    tokio::spawn(async move {
        let variables = args.variables.map(Arc::new);
        let strict = args.strict;
        let mut deferred = false;
        // Compiled on the loop; a template that doesn't compile skips the
        // check (`except TemplateError: timed_out = False`).
        if template.compile(strict).is_ok() {
            let (t, v) = (template.clone(), variables.clone());
            let job =
                tokio::task::spawn_blocking(move || t.render_discarding(v.as_deref(), strict));
            let finished = match Duration::try_from_secs_f64(timeout) {
                Ok(limit) => tokio::time::timeout(limit, job).await.ok(),
                // Negative: already over. Infinite or NaN: never.
                Err(_) if timeout < 0.0 => None,
                Err(_) => Some(job.await),
            };
            let Some(finished) = finished else {
                *state.lock() = Later::Never;
                let message = format!(
                    "Exceeded maximum execution time of {}s",
                    ha_core::py::repr_float(timeout)
                );
                out.error(json!(id), "template_error", &message);
                return;
            };
            // The template now logs through the thread-safe listener it
            // was compiled with: what it logs arrives a step late.
            deferred = args.report_errors;
            if let (true, Ok(Render { log, .. })) = (args.report_errors, finished) {
                report(&out, id, &log);
            }
        }
        let mut state = state.lock();
        if *state == Later::Never {
            return;
        }
        *state = Later::Live;
        let reply = Reply {
            out: &out,
            id,
            report: args.report_errors,
            deferred,
        };
        reply.subscribe(&template, variables.as_deref(), strict);
    });
    Ok(())
}

/// A subscription whose start is decided in the background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Later {
    /// Not subscribed yet.
    Pending,
    /// Subscribed: `unsubscribe_events` ends it.
    Live,
    /// Ended, or never started.
    Never,
}

/// Where the replies for one `render_template` go.
struct Reply<'a> {
    out: &'a Outbox,
    id: u64,
    report: bool,
    /// The template logs through `call_soon_threadsafe`: each render's log
    /// is sent after what the render's caller sends straight away.
    deferred: bool,
}

/// `listeners` of a template that reads no states and no clock.
fn listeners() -> Value {
    json!({"all": false, "entities": [], "domains": [], "time": false})
}

impl Reply<'_> {
    /// `async_track_template_result`'s setup render, the result, and the
    /// first refresh's render and event.
    fn subscribe(&self, template: &Template, variables: Option<&Map<String, Value>>, strict: bool) {
        let setup = template.render(variables, strict);
        if !self.report {
            // Nothing is reported: HA logs, and its refresh renders the
            // same thing again (a template can't read anything that
            // changes yet), so render once.
            if let Err(e) = &setup.result {
                tracing::error!(
                    "Error while processing template: {}: {e}",
                    template.source()
                );
            }
            // `make_logging_undefined`'s `_log_with_logger`.
            for (level, message) in &setup.log {
                let source = template.source();
                match level {
                    Level::Warning => tracing::warn!(
                        "Template variable warning: {message} when rendering '{source}'"
                    ),
                    Level::Error => tracing::error!(
                        "Template variable error: {message} when rendering '{source}'"
                    ),
                }
            }
            self.out.result(self.id, Value::Null);
            self.send_result(setup);
            return;
        }
        let late = if self.deferred {
            setup.log
        } else {
            report(self.out, self.id, &setup.log);
            Log::new()
        };
        if let Err(e) = &setup.result {
            self.error(e);
        }
        self.out.result(self.id, Value::Null);
        report(self.out, self.id, &late);
        let refresh = template.render(variables, strict);
        let late = if self.deferred {
            refresh.log.clone()
        } else {
            report(self.out, self.id, &refresh.log);
            Log::new()
        };
        self.send_result(refresh);
        report(self.out, self.id, &late);
    }

    /// `_template_listener`.
    fn send_result(&self, render: Render) {
        match render.result {
            Err(e) if self.report => self.error(&e),
            Err(_) => {}
            Ok(rendered) => match rendered.to_json() {
                Ok(result) => {
                    self.out.event(
                        self.id,
                        &json!({"result": result, "listeners": listeners()}),
                    );
                }
                // `message_to_json_bytes`'s fallback.
                Err(_) => {
                    self.out
                        .error(json!(self.id), "unknown_error", "Invalid JSON in response");
                }
            },
        }
    }

    fn error(&self, e: &TemplateError) {
        self.out.event(
            self.id,
            &json!({"error": e.0, "level": Level::Error.as_str()}),
        );
    }
}

/// `_error_listener` for each logged message.
fn report(out: &Outbox, id: u64, log: &Log) {
    for (level, message) in log {
        out.event(id, &json!({"error": message, "level": level.as_str()}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// At most 128 templates, the least recently used going first.
    #[test]
    fn the_cache_is_bounded() {
        let cache = Cache::default();
        let first = cache.get("{{ 0 }}");
        for i in 1..CACHE_SIZE {
            cache.get(&format!("{{{{ {i} }}}}"));
        }
        // Used again: now the most recent.
        assert!(Arc::ptr_eq(&first, &cache.get("{{ 0 }}")));
        cache.get("{{ 'new' }}");
        let lru = cache.inner.lock();
        assert_eq!(lru.templates.len(), CACHE_SIZE);
        assert!(lru.templates.contains_key("{{ 0 }}"));
        assert!(!lru.templates.contains_key("{{ 1 }}"), "the oldest went");
    }
}

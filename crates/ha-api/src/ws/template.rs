//! `render_template` (`websocket_api/commands.py::handle_render_template`
//! with `helpers/event.py::async_track_template_result`).
//!
//! The reply is a result, then one event: `{result, listeners}`, or with
//! `report_errors` the template's errors and warnings as `{error, level}`
//! events, in HA's order. Any authenticated user may call it (HA has no
//! admin check). Templates are compiled once and kept, 128 at most, as
//! HA's `_cached_template` LRU does (up to 8 KiB each; longer ones and a
//! `report_errors` call compile their own). A `timeout` renders once first
//! and answers `template_error` if that takes longer.
//!
//! Template input is hostile: any logged-in user can send it. Compiling
//! and rendering run on threads of their own, with a fixed stack
//! ([`ha_core::template::STACK`]), never on the async runtime, at most
//! `ha_core::template::run::RENDERS_AT_ONCE` at a time across every
//! connection and automation (`ha_core::template::run`), each render
//! within its fuel budget ([`ha_core::template::FUEL`]). A connection
//! handles its messages in order, so what follows a `render_template` waits
//! for its replies (in a bounded queue; the connection keeps reading, so a
//! close or a heartbeat's pong is seen at once): HA does the same for a render without `timeout`, but
//! serves other commands while a `timeout` check runs. A timed-out render
//! keeps its permit and thread until it reaches its budget (HA raises an
//! exception into its thread; minijinja can't be interrupted). Closing the
//! connection drops a render that hasn't started.
//!
//! Templates read the live instance ([`ha_core::template::Home`]): its
//! states, clock and time zone, and the entity registry's sensor display
//! precision. `listeners` reports what the render read, as HA's does. Not
//! yet: re-rendering when one of those changes (HA's state and time
//! listeners and rate limits), so a subscription still costs no timer, no
//! listener and no wakeup until it is unsubscribed.

use super::commands::{Bad, cv_entity_ids};
use super::{Conn, Invalid, Outbox};
use ha_core::template::{Home, Level, Log, Render, Template, TemplateError};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

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
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().templates.len()
    }

    pub(crate) fn get(&self, source: &str) -> Arc<Template> {
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
    let template = if args.report_errors || args.template.len() > CACHE_MAX_SOURCE {
        Arc::new(Template::new(&args.template))
    } else {
        conn.shared.templates.get(&args.template)
    };
    // `if timeout:` and `async_render_will_timeout`'s static shortcut.
    let timeout = args.timeout.filter(|t| *t != 0.0 && !template.is_static());
    let out = conn.out.clone();
    let home = Arc::new(home(&conn.shared));
    #[cfg(test)]
    let source = args.template.clone();
    #[cfg(test)]
    let started = source.clone();
    let task = tokio::spawn(async move {
        let variables = args.variables.map(Arc::new);
        let (strict, report) = (args.strict, args.report_errors);
        let mut deferred = false;
        if let Some(timeout) = timeout {
            match timed(&template, &home, &variables, strict, timeout).await {
                Timed::Over => {
                    let message = format!(
                        "Exceeded maximum execution time of {}s",
                        ha_core::py::repr_float(timeout)
                    );
                    out.error(json!(id), "template_error", &message);
                    return None;
                }
                // A template that doesn't compile skips the check
                // (`except TemplateError: timed_out = False`).
                Timed::NotCompiled => {}
                Timed::Done(log) => {
                    // The template now logs through the thread-safe
                    // listener it was compiled with: what it logs arrives
                    // a step late.
                    deferred = report;
                    if report {
                        send_log(&out, id, &log);
                    }
                }
            }
        }
        // `async_track_template_result`'s setup render and its first
        // refresh. Nothing a template reads changes yet, so without
        // `report_errors` (where only the second is seen) it renders once.
        let t = template.clone();
        let (setup, refresh) = blocking(move || {
            #[cfg(test)]
            tests::note("started", &started);
            let setup = t.render_in(Some(&home), variables.as_deref(), strict);
            let refresh = report.then(|| t.render_in(Some(&home), variables.as_deref(), strict));
            (setup, refresh)
        })
        .await?;
        // Too deep to compile: Jinja's `RecursionError` escapes HA's
        // handler, which answers with its catch-all.
        if matches!(&setup.result, Err(e) if e.0 == ha_core::template::TOO_DEEP) {
            out.error(json!(id), "unknown_error", "Unknown error");
            return None;
        }
        let reply = Reply {
            out: &out,
            id,
            report,
            deferred,
        };
        reply.send(&template, setup, refresh);
        Some(id)
    });
    conn.busy = Some(Busy {
        task,
        #[cfg(test)]
        source,
    });
    Ok(())
}

/// What templates read: the instance, and the entity registry's sensor
/// display precision (`sensor.async_rounded_state`'s `_display_precision`).
fn home(shared: &crate::Shared) -> Home {
    Home {
        hass: shared.hass.clone(),
        display_precision: shared
            .config
            .registry
            .as_ref()
            .map(|r| r.display_precision()),
    }
}

/// Templates longer than this aren't kept: 128 of them stay under 1 MiB.
const CACHE_MAX_SOURCE: usize = 8 * 1024;

/// Run template work on a render thread (`ha_core::template::run`, shared
/// with automations), holding a render permit until it returns. When they
/// are all taken, the next render waits for one, and its connection reads
/// nothing more until then: at most one waiting render per connection.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    ha_core::template::run::run(work).await
}

#[cfg(test)]
pub(crate) use ha_core::template::run::{RENDERS_AT_ONCE, renders_free, take_all_renders};

enum Timed {
    Over,
    NotCompiled,
    Done(Log),
}

/// `async_render_will_timeout`: one render on a render thread, timed
/// from when it starts (a wait for a permit doesn't count).
async fn timed(
    template: &Arc<Template>,
    home: &Arc<Home>,
    variables: &Option<Arc<Map<String, Value>>>,
    strict: bool,
    timeout: f64,
) -> Timed {
    let Some(permit) = ha_core::template::run::permit().await else {
        return Timed::NotCompiled;
    };
    let (t, h, v) = (template.clone(), home.clone(), variables.clone());
    let Some(job) = ha_core::template::run::on_render_thread(permit, move || {
        t.compile(strict)
            .ok()
            .map(|()| t.render_discarding(Some(&h), v.as_deref(), strict).log)
    }) else {
        return Timed::NotCompiled;
    };
    let finished = match Duration::try_from_secs_f64(timeout) {
        Ok(limit) => tokio::time::timeout(limit, job).await.ok(),
        // Negative: already over. Infinite or NaN: never.
        Err(_) if timeout < 0.0 => None,
        Err(_) => Some(job.await),
    };
    match finished {
        None => Timed::Over,
        Some(Ok(Some(log))) => Timed::Done(log),
        Some(_) => Timed::NotCompiled,
    }
}

/// The `render_template` a connection is waiting on.
pub(crate) struct Busy {
    task: JoinHandle<Option<u64>>,
    /// Its template, for tests.
    #[cfg(test)]
    source: String,
}

impl Busy {
    /// The connection is gone (`why`: what ended it): stop sending, and
    /// stop any render not yet started.
    pub(crate) fn abort(self, why: &'static str) {
        self.task.abort();
        // A test learns why, once the task has stopped.
        #[cfg(test)]
        {
            let (task, source) = (self.task, self.source);
            tokio::spawn(async move {
                let _ = task.await;
                tests::note(&format!("cancelled:{why}"), &source);
            });
        }
        #[cfg(not(test))]
        let _ = why;
    }
}

/// When the busy command ends: the id it subscribed, if any. Never, while
/// nothing is busy.
pub(crate) async fn finished(busy: &mut Option<Busy>) -> Option<u64> {
    match busy {
        Some(b) => (&mut b.task).await.ok().flatten(),
        None => std::future::pending().await,
    }
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

impl Reply<'_> {
    /// The setup render's log and error, the result, then the refresh's
    /// log and event (`async_track_template_result`).
    fn send(&self, template: &Template, setup: Render, refresh: Option<Render>) {
        let Some(refresh) = refresh.filter(|_| self.report) else {
            // Nothing is reported: HA logs.
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
        };
        if !self.deferred {
            send_log(self.out, self.id, &setup.log);
        }
        if let Err(e) = &setup.result {
            self.error(e);
        }
        self.out.result(self.id, Value::Null);
        if self.deferred {
            send_log(self.out, self.id, &setup.log);
        } else {
            send_log(self.out, self.id, &refresh.log);
        }
        let late = self.deferred.then(|| refresh.log.clone());
        self.send_result(refresh);
        if let Some(late) = late {
            send_log(self.out, self.id, &late);
        }
    }

    /// `_template_listener`.
    fn send_result(&self, render: Render) {
        let listeners = render.info.listeners();
        match render.result {
            Err(e) if self.report => self.error(&e),
            Err(_) => {}
            Ok(rendered) => match rendered.to_json() {
                Ok(result) => {
                    self.out
                        .event(self.id, &json!({"result": result, "listeners": listeners}));
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
fn send_log(out: &Outbox, id: u64, log: &Log) {
    for (level, message) in log {
        out.event(id, &json!({"error": message, "level": level.as_str()}));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// What happened to each `render_template` in this process, by its
    /// template: `started` (its render began on a render thread) and
    /// `cancelled` (its connection went and it was stopped). A test names
    /// its template uniquely and looks for its own.
    static EVENTS: parking_lot::Mutex<Vec<(String, String)>> = parking_lot::Mutex::new(Vec::new());

    pub(crate) fn note(what: &str, source: &str) {
        EVENTS.lock().push((what.to_owned(), source.to_owned()));
    }

    /// Whether `what` happened to the render of `source`.
    pub(crate) fn happened(what: &str, source: &str) -> bool {
        EVENTS.lock().iter().any(|(w, s)| *w == what && s == source)
    }

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

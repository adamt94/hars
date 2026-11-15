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
//! and rendering run on blocking threads, never on the async runtime, at
//! most [`RENDERS_AT_ONCE`] at a time across every connection, each render
//! within its fuel budget ([`ha_core::template::FUEL`]). A connection
//! handles its messages in order, so what follows a `render_template` waits
//! for its replies: HA does the same for a render without `timeout`, but
//! serves other commands while a `timeout` check runs. A timed-out render
//! keeps its permit and thread until it reaches its budget (HA raises an
//! exception into its thread; minijinja can't be interrupted). Closing the
//! connection drops a render that hasn't started.
//!
//! Templates can't read states or the clock yet ([`ha_core::template`]),
//! so `listeners` is always empty and nothing re-renders: a subscription
//! costs no timer, no listener and no wakeup until it is unsubscribed.

use super::commands::{Bad, cv_entity_ids};
use super::{Conn, Invalid, Outbox};
use ha_core::template::{Level, Log, Render, Template, TemplateError};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::Semaphore;
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
    let task = tokio::spawn(async move {
        let variables = args.variables.map(Arc::new);
        let (strict, report) = (args.strict, args.report_errors);
        let mut deferred = false;
        if let Some(timeout) = timeout {
            match timed(&template, &variables, strict, timeout).await {
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
            let setup = t.render(variables.as_deref(), strict);
            let refresh = report.then(|| t.render(variables.as_deref(), strict));
            (setup, refresh)
        })
        .await?;
        let reply = Reply {
            out: &out,
            id,
            report,
            deferred,
        };
        reply.send(&template, setup, refresh);
        Some(id)
    });
    conn.busy = Some(Busy { task });
    Ok(())
}

/// Templates longer than this aren't kept: 128 of them stay under 1 MiB.
const CACHE_MAX_SOURCE: usize = 8 * 1024;

/// Renders running at once, across every connection. A permit is taken
/// before a render starts and given back when its thread is done with it,
/// even if nobody waits for the answer any more (a timed-out render runs
/// on to its fuel budget). When they are all taken, the next render waits
/// for one, and its connection reads nothing more until then: at most one
/// waiting render per connection.
pub(crate) const RENDERS_AT_ONCE: usize = 2;

static RENDERS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(RENDERS_AT_ONCE)));

/// Run template work on a blocking thread, holding a render permit until
/// it returns. `None` if it panicked.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let permit = RENDERS.clone().acquire_owned().await.ok()?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .ok()
}

/// Render permits free now.
#[cfg(test)]
pub(crate) fn renders_free() -> usize {
    RENDERS.available_permits()
}

/// Take every render permit (until dropped).
#[cfg(test)]
pub(crate) async fn take_all_renders() -> tokio::sync::OwnedSemaphorePermit {
    RENDERS
        .clone()
        .acquire_many_owned(RENDERS_AT_ONCE as u32)
        .await
        .expect("open")
}

enum Timed {
    Over,
    NotCompiled,
    Done(Log),
}

/// `async_render_will_timeout`: one render on a blocking thread, timed
/// from when it starts (a wait for a permit doesn't count).
async fn timed(
    template: &Arc<Template>,
    variables: &Option<Arc<Map<String, Value>>>,
    strict: bool,
    timeout: f64,
) -> Timed {
    let Ok(permit) = RENDERS.clone().acquire_owned().await else {
        return Timed::NotCompiled;
    };
    let (t, v) = (template.clone(), variables.clone());
    let job = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        t.compile(strict)
            .ok()
            .map(|()| t.render_discarding(v.as_deref(), strict).log)
    });
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
}

impl Busy {
    /// The connection is gone: stop sending, and stop any render not yet
    /// started.
    pub(crate) fn abort(self) {
        self.task.abort();
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

/// `listeners` of a template that reads no states and no clock.
fn listeners() -> Value {
    json!({"all": false, "entities": [], "domains": [], "time": false})
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
fn send_log(out: &Outbox, id: u64, log: &Log) {
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

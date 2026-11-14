//! `system_log` (`components/system_log`): the last 50 warnings and errors,
//! deduplicated by logger, source and root cause, for `system_log/list`;
//! `system_log.write` (the frontend reports its own errors there) and
//! `system_log.clear`. A [`layer`] feeds the store from `tracing`.

use ha_core::{Hass, Kind, Schema, SupportsResponse};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::VecDeque;

/// `DEFAULT_MAX_ENTRIES`.
const MAX_ENTRIES: usize = 50;
/// The messages kept per entry (`deque(maxlen=5)`).
const MAX_MESSAGES: usize = 5;

/// One deduplicated record (`LogEntry`).
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    name: String,
    level: &'static str,
    source: (String, u32),
    messages: VecDeque<String>,
    timestamp: f64,
    first_occurred: f64,
    count: u64,
}

impl Entry {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "message": self.messages,
            "level": self.level,
            "source": [self.source.0, self.source.1],
            "timestamp": self.timestamp,
            "exception": "",
            "count": self.count,
            "first_occurred": self.first_occurred,
        })
    }
}

/// `DedupStore`, oldest first. Process-wide, as HA's handler on the root
/// logger is.
static STORE: Mutex<VecDeque<Entry>> = Mutex::new(VecDeque::new());

/// `DedupStore.add_entry`: a repeat (same logger and source) counts up,
/// keeps a new message and moves to the end; the oldest of more than 50
/// goes.
fn add(name: &str, level: &'static str, source: (String, u32), message: String, now: f64) {
    let mut store = STORE.lock();
    if let Some(i) = store
        .iter()
        .position(|e| e.name == name && e.source == source)
    {
        let mut e = store.remove(i).unwrap_or_else(|| unreachable!());
        e.count += 1;
        e.timestamp = now;
        if !e.messages.contains(&message) {
            e.messages.push_back(message);
            if e.messages.len() > MAX_MESSAGES {
                e.messages.pop_front();
            }
        }
        store.push_back(e);
    } else {
        store.push_back(Entry {
            name: name.to_owned(),
            level,
            source,
            messages: VecDeque::from([message]),
            timestamp: now,
            first_occurred: now,
            count: 1,
        });
    }
    while store.len() > MAX_ENTRIES {
        store.pop_front();
    }
}

/// `system_log/list`: newest first.
pub fn list() -> Value {
    Value::Array(STORE.lock().iter().rev().map(Entry::to_json).collect())
}

fn clear() {
    STORE.lock().clear();
}

/// A `tracing` layer keeping warnings and errors (HA's handler is at
/// WARNING). The logger is the event's `logger` field if it has one (as
/// `system_log.write` sets), else its target.
pub fn layer<S>() -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    use tracing_subscriber::Layer;
    // Its own filter, so DEBUG/TRACE call sites stay disabled.
    SystemLogLayer.with_filter(tracing_subscriber::filter::LevelFilter::WARN)
}

/// Where HA's `system_log.write` logs from (`_figure_out_source` finds
/// the service handler's call), so every write of a logger is one entry.
const WRITE_SOURCE: (&str, u32) = ("components/system_log/__init__.py", 329);

struct SystemLogLayer;

#[derive(Default)]
struct Fields {
    message: String,
    logger: Option<String>,
    /// `system_log.write`'s own entry: already stored.
    stored: bool,
}

impl tracing::field::Visit for Fields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "logger" => self.logger = Some(value.to_owned()),
            "message" => self.message = value.to_owned(),
            _ => {}
        }
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        if field.name() == "stored" {
            self.stored = value;
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "message" => self.message = format!("{value:?}"),
            "logger" => self.logger = Some(format!("{value:?}")),
            _ => {}
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for SystemLogLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let meta = event.metadata();
        let level = match *meta.level() {
            tracing::Level::ERROR => "ERROR",
            tracing::Level::WARN => "WARNING",
            _ => return,
        };
        let mut f = Fields::default();
        event.record(&mut f);
        if f.stored {
            return;
        }
        let name = f.logger.unwrap_or_else(|| meta.target().replace("::", "."));
        let source = (
            meta.file().unwrap_or_default().to_owned(),
            meta.line().unwrap_or_default(),
        );
        let now = jiff::Timestamp::now().as_microsecond() as f64 / 1e6;
        add(&name, level, source, f.message, now);
    }
}

/// Register `system_log.write` and `system_log.clear`.
pub fn register(hass: &Hass) {
    let write = Schema::new()
        .required("message", Kind::String)
        .optional(
            "level",
            Kind::OneOf(&["debug", "info", "warning", "error", "critical"]),
        )
        .optional("logger", Kind::String);
    hass.services().register_with_schema(
        "system_log",
        "write",
        Some(write),
        SupportsResponse::None,
        |call| async move {
            let text = |k: &str| call.data.get(k).and_then(Value::as_str);
            let message = text("message").unwrap_or_default();
            let logger = text("logger").unwrap_or("homeassistant.components.system_log.external");
            let level = text("level").unwrap_or("error");
            // Stored here (one source for every write, HA's level names),
            // then only printed.
            let name = match level {
                "warning" => Some("WARNING"),
                "error" => Some("ERROR"),
                "critical" => Some("CRITICAL"),
                _ => None,
            };
            if let Some(name) = name {
                let now = jiff::Timestamp::now().as_microsecond() as f64 / 1e6;
                let (file, line) = WRITE_SOURCE;
                add(
                    logger,
                    name,
                    (file.to_owned(), line),
                    message.to_owned(),
                    now,
                );
            }
            match level {
                "debug" => tracing::debug!(logger, "{message}"),
                "info" => tracing::info!(logger, "{message}"),
                "warning" => tracing::warn!(logger, stored = true, "{message}"),
                // `error` and `critical`.
                _ => tracing::error!(logger, stored = true, "{message}"),
            }
            Ok(None)
        },
    );
    hass.services().register_with_schema(
        "system_log",
        "clear",
        Some(Schema::new()),
        SupportsResponse::None,
        |_| async {
            clear();
            Ok(None)
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // The guard only serializes tests; nothing else takes it.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn write_checks_its_data_as_has_schema() {
        let _guard = SERIAL.lock();
        let hass = Hass::new();
        register(&hass);
        let call = |data: Value| {
            let hass = hass.clone();
            async move {
                let Value::Object(data) = data else { panic!() };
                hass.call_service("system_log", "write", data, None, false, None)
                    .await
            }
        };
        assert_eq!(
            call(json!({"message": "boom", "level": "warning"})).await,
            Ok(None)
        );
        assert_eq!(
            call(json!({"message": "boom", "logger": "frontend.js.latest"})).await,
            Ok(None)
        );
        assert!(
            call(json!({"message": "boom", "level": "loud"}))
                .await
                .is_err()
        );
        assert!(call(json!({"level": "error"})).await.is_err());
        assert!(call(json!({"message": "boom", "extra": 1})).await.is_err());
        let clear = hass
            .call_service("system_log", "clear", Default::default(), None, false, None)
            .await;
        assert_eq!(clear, Ok(None));
    }

    /// The tests share the process-wide store.
    static SERIAL: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[test]
    fn dedups_as_has_store() {
        let _guard = SERIAL.lock();
        use tracing_subscriber::layer::SubscriberExt;
        clear();
        let subscriber = tracing_subscriber::registry().with(layer());
        tracing::subscriber::with_default(subscriber, || {
            for i in 0..3 {
                // One call site: one entry, counted.
                tracing::warn!("boom {}", i % 2);
            }
            tracing::info!("not kept");
            // Its own filter: lower levels aren't even evaluated.
            assert!(!tracing::enabled!(tracing::Level::DEBUG));
            assert!(tracing::enabled!(tracing::Level::WARN));
            tracing::error!(logger = "frontend.js.latest", "bad");
            for i in 0..60 {
                add("x", "ERROR", ("f".into(), i), "m".into(), 0.0);
            }
        });
        let all = list();
        assert_eq!(all.as_array().unwrap().len(), 50);
        // Newest first; the two real ones were pushed out by the 60.
        assert_eq!(all[0]["source"], json!(["f", 59]));
        clear();
        let subscriber = tracing_subscriber::registry().with(layer());
        tracing::subscriber::with_default(subscriber, || {
            for i in 0..3 {
                tracing::warn!("boom {}", i % 2);
            }
            tracing::error!(logger = "frontend.js.latest", "bad");
        });
        let all = list();
        assert_eq!(all[0]["name"], "frontend.js.latest");
        assert_eq!(all[0]["level"], "ERROR");
        assert_eq!(all[1]["name"], "ha_api.system_log.tests");
        assert_eq!(all[1]["level"], "WARNING");
        assert_eq!(all[1]["count"], 3);
        assert_eq!(all[1]["message"], json!(["boom 0", "boom 1"]));
        assert_eq!(all[1]["exception"], "");
        assert!(
            all[1]["source"][0]
                .as_str()
                .unwrap()
                .ends_with("system_log.rs")
        );
        clear();
        assert_eq!(list(), json!([]));
    }

    /// Writes through the service: one entry per logger whatever the
    /// level (HA logs them from one place), the first level kept, and
    /// `critical` named as such.
    // The guard only serializes tests; nothing else takes it.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn service_writes_as_ha() {
        let _guard = SERIAL.lock();
        clear();
        let hass = Hass::new();
        register(&hass);
        let write = |data: Value| {
            let hass = hass.clone();
            async move {
                let Value::Object(data) = data else { panic!() };
                hass.call_service("system_log", "write", data, None, false, None)
                    .await
                    .unwrap();
            }
        };
        write(json!({"message": "warning", "level": "warning", "logger": "test.logger"})).await;
        write(json!({"message": "error", "level": "error", "logger": "test.logger"})).await;
        write(json!({"message": "boom", "level": "critical", "logger": "other"})).await;
        write(json!({"message": "quiet", "level": "info", "logger": "other"})).await;
        let all = list();
        assert_eq!(all.as_array().unwrap().len(), 2, "{all}");
        assert_eq!(all[0]["level"], "CRITICAL");
        assert_eq!(all[1]["count"], 2);
        assert_eq!(all[1]["level"], "WARNING");
        assert_eq!(all[1]["message"], json!(["warning", "error"]));
        assert_eq!(
            all[1]["source"],
            json!(["components/system_log/__init__.py", 329])
        );
        clear();
    }
}

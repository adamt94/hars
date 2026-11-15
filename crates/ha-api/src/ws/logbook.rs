//! `logbook/get_events` and `logbook/event_stream`
//! (`components/logbook/websocket_api.py`), answered from the recorder's
//! segment store; the stream then follows the recorder's live rows.

use super::recorder::{parse_datetime, str_list, string};
use super::{Conn, Invalid, Subscription, TaskSub, only_keys};
use ha_core::{ContextId, EntityId, Hass};
use ha_recorder::history::seconds;
use ha_recorder::logbook::{
    ALWAYS_CONTINUOUS_DOMAINS, Answer, Filter, Humanify, Query, device_class_is_numeric, events,
    sensor_attributes_continuous,
};
use ha_recorder::{LiveFeed, Recorder};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::Duration;

/// `EVENT_COALESCE_TIME`.
const COALESCE: Duration = Duration::from_millis(350);
/// `MAX_PENDING_LOGBOOK_EVENTS`: a stream whose queue grows past this is
/// dropped.
const MAX_PENDING_LOGBOOK_EVENTS: usize = 2048;
/// `BIG_QUERY_HOURS`: the smallest unfiltered window that is split in two.
const BIG_QUERY_US: i64 = 25 * 3_600_000_000;
/// `BIG_QUERY_RECENT_HOURS`: how much of it is sent first.
const BIG_QUERY_RECENT_US: i64 = 24 * 3_600_000_000;

/// `is_sensor_continuous`, which also reads the registry.
type Continuous = Arc<dyn Fn(&str) -> bool + Send + Sync>;

pub(super) fn dispatch(
    conn: &mut Conn,
    id: u64,
    ty: &str,
    msg: &Map<String, Value>,
) -> Option<Result<(), Invalid>> {
    let stream = match ty {
        "logbook/get_events" => false,
        "logbook/event_stream" => true,
        _ => return None,
    };
    let recorder = conn.shared.config.recorder.clone()?;
    Some(Args::parse(msg, stream).map(|args| {
        if stream {
            start_stream(conn, id, &recorder, args);
        } else {
            get_events(conn, id, &recorder, args);
        }
    }))
}

struct Args {
    start_time: String,
    end_time: Option<String>,
    entity_ids: Vec<String>,
    device_ids: Vec<String>,
    /// `get_events` only.
    context_id: Option<String>,
}

impl Args {
    fn parse(msg: &Map<String, Value>, stream: bool) -> Result<Self, Invalid> {
        only_keys(
            msg,
            if stream {
                &["start_time", "end_time", "entity_ids", "device_ids"]
            } else {
                &[
                    "start_time",
                    "end_time",
                    "entity_ids",
                    "device_ids",
                    "context_id",
                ]
            },
        )?;
        let start_time = string(msg, "start_time", true)?.unwrap_or_default();
        let end_time = string(msg, "end_time", false)?;
        str_list(msg, "entity_ids", false)?;
        str_list(msg, "device_ids", false)?;
        let context_id = if stream {
            None
        } else {
            string(msg, "context_id", false)?
        };
        let list = |key: &str| -> Vec<String> {
            msg.get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        };
        Ok(Self {
            start_time: start_time.to_owned(),
            end_time: end_time.map(str::to_owned),
            entity_ids: list("entity_ids"),
            device_ids: list("device_ids"),
            context_id: context_id.map(str::to_owned),
        })
    }
}

/// `is_sensor_continuous`: a sensor with a unit, a state class or a numeric
/// device class. Its state decides while it has one with attributes,
/// otherwise its registry entry.
fn sensor_continuous(hass: &Hass, registries: &ha_registry::Registries, entity_id: &str) -> bool {
    if let Some(state) = EntityId::parse(entity_id)
        .ok()
        .and_then(|id| hass.states().get(&id))
        && !state.attributes.is_empty()
    {
        return sensor_attributes_continuous(&state.attributes);
    }
    registries
        .entities
        .iter()
        .find(|e| e.entity_id == entity_id)
        .is_some_and(|entry| {
            let state_class = entry
                .capabilities
                .as_ref()
                .and_then(|c| c.object().remove("state_class"));
            state_class.is_some_and(|v| truthy(&v))
                || device_class_is_numeric(entry.device_class.as_deref().map(Value::from).as_ref())
        })
}

/// Python's truthiness of a JSON value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// A request's entities and devices once HA's `async_filter_entities` has
/// had its say, as `ws_get_events` and `ws_event_stream` go about it.
enum Targets {
    /// Everything was filtered away: HA answers without reading.
    Nothing,
    /// An id HA can't split (`split_entity_id` raises: `unknown_error`).
    Invalid,
    Some {
        entities: Vec<String>,
        devices: Vec<String>,
        continuous: Continuous,
    },
}

fn targets(conn: &Conn, args: &Args) -> Targets {
    let hass = conn.shared.hass.clone();
    let registries = conn.shared.config.registries();
    let continuous: Continuous = {
        let (hass, registries) = (hass.clone(), registries.clone());
        Arc::new(move |e| sensor_continuous(&hass, &registries, e))
    };
    let mut entities = Vec::new();
    for e in &args.entity_ids {
        let split = e
            .split_once('.')
            .filter(|(d, o)| !d.is_empty() && !o.is_empty());
        let Some((d, _)) = split else {
            return Targets::Invalid;
        };
        if !ALWAYS_CONTINUOUS_DOMAINS.contains(&d) && !(d == "sensor" && continuous(e)) {
            entities.push(e.clone());
        }
    }
    if entities.is_empty() && args.device_ids.is_empty() && !args.entity_ids.is_empty() {
        return Targets::Nothing;
    }
    Targets::Some {
        entities,
        devices: args.device_ids.clone(),
        continuous,
    }
}

/// What a read needs, owned so it can move to the blocking pool.
struct Reader {
    recorder: Arc<Recorder>,
    filter: Arc<Filter>,
    continuous: Continuous,
    /// The stream's second read finds nothing (see [`start_stream`]).
    known_empty: bool,
}

impl Reader {
    /// The rows strictly between `start` and `end` (µs). `None` if the
    /// read task failed.
    async fn fetch(&self, start: i64, end: i64) -> Option<Answer> {
        if self.known_empty {
            return Some(Answer {
                json: "[]".into(),
                last: None,
            });
        }
        let (recorder, filter, continuous) = (
            self.recorder.clone(),
            self.filter.clone(),
            self.continuous.clone(),
        );
        tokio::task::spawn_blocking(move || {
            events(
                recorder.store(),
                &Query {
                    start,
                    end,
                    filter: &filter,
                    continuous: &*continuous,
                },
            )
        })
        .await
        .ok()
    }
}

fn get_events(conn: &mut Conn, id: u64, recorder: &Arc<Recorder>, args: Args) {
    let hass = conn.shared.hass.clone();
    let tz = &hass.config().time_zone;
    let now = hass.clock().now().as_microsecond();
    let err = |code: &str, message: &str| {
        conn.out.error(json!(id), code, message);
    };
    let Some(start) = parse_datetime(&args.start_time, tz).map(|t| t.as_microsecond()) else {
        return err("invalid_start_time", "Invalid start_time");
    };
    let end = match args.end_time.as_deref().filter(|e| !e.is_empty()) {
        None => now,
        Some(e) => match parse_datetime(e, tz) {
            Some(t) => t.as_microsecond(),
            None => return err("invalid_end_time", "Invalid end_time"),
        },
    };
    if start > now {
        conn.out.result(id, json!([]));
        return;
    }
    let (entities, devices, continuous) = match targets(conn, &args) {
        Targets::Nothing => {
            conn.out.result(id, json!([]));
            return;
        }
        Targets::Invalid => return err("unknown_error", "Unknown error"),
        Targets::Some {
            entities,
            devices,
            continuous,
        } => (entities, devices, continuous),
    };
    // `EventProcessor` asserts it isn't given both.
    let context = args.context_id.as_deref().filter(|c| !c.is_empty());
    if context.is_some() && (!entities.is_empty() || !devices.is_empty()) {
        return err("unknown_error", "Unknown error");
    }
    // An id that isn't a ULID is no filter (`ulid_to_bytes_or_none`).
    let context = context.and_then(|c| c.parse::<ContextId>().ok().map(ContextId::to_bytes));
    let reader = Reader {
        recorder: recorder.clone(),
        filter: Arc::new(Filter::new(&entities, &devices, context)),
        continuous,
        known_empty: false,
    };
    let out = conn.out.clone();
    tokio::spawn(async move {
        // D5: reads see everything fired before them.
        reader.recorder.sync().await;
        if let Some(answer) = reader.fetch(start, end).await {
            out.result_raw(id, &answer.json);
        }
    });
}

/// One message of the stream's past.
fn historical_event(id: u64, events: &str, start: i64, end: i64, partial: bool) -> String {
    let partial = if partial { r#","partial":true"# } else { "" };
    format!(
        r#"{{"id":{id},"type":"event","event":{{"events":{events},"start_time":{:?},"end_time":{:?}{partial}}}}}"#,
        seconds(start),
        seconds(end)
    )
}

/// `_async_send_historical_events`: the rows between `start` and `end`, in
/// one message, or for a big unfiltered window the recent day first and then
/// the rest. A message with no rows is sent only when it is the last one
/// (`!partial`) or `force`, so a client knows its request was answered.
/// Returns the time of the newest row sent, or `None` when no read ran.
async fn send_historical(
    send: &impl Fn(String) -> bool,
    id: u64,
    reader: &Reader,
    window: (i64, i64),
    partial: bool,
    force: bool,
) -> Option<Option<i64>> {
    let (start, end) = window;
    let big = reader.filter.is_unlimited() && end - start > BIG_QUERY_US;
    if !big {
        let answer = reader.fetch(start, end).await?;
        if answer.last.is_some() || !partial || force {
            send(historical_event(id, &answer.json, start, end, partial));
        }
        return Some(answer.last);
    }
    // The recent day arrives while the long read is still going on.
    let recent_start = end - BIG_QUERY_RECENT_US;
    let recent = reader.fetch(recent_start, end).await?;
    if recent.last.is_some() {
        send(historical_event(id, &recent.json, recent_start, end, true));
    }
    let older = reader.fetch(start, recent_start).await?;
    if older.last.is_some() || !partial || force {
        send(historical_event(
            id,
            &older.json,
            start,
            recent_start,
            partial,
        ));
    }
    Some(recent.last.or(older.last))
}

fn start_stream(conn: &mut Conn, id: u64, recorder: &Arc<Recorder>, args: Args) {
    let hass = conn.shared.hass.clone();
    let tz = &hass.config().time_zone;
    let now = hass.clock().now().as_microsecond();
    let out = conn.out.clone();
    let err = |code: &str, message: &str| {
        out.error(json!(id), code, message);
    };
    let start = match parse_datetime(&args.start_time, tz).map(|t| t.as_microsecond()) {
        Some(start) if start <= now => start,
        _ => return err("invalid_start_time", "Invalid start_time"),
    };
    let end = match args.end_time.as_deref().filter(|e| !e.is_empty()) {
        None => None,
        Some(e) => match parse_datetime(e, tz).map(|t| t.as_microsecond()) {
            Some(end) if end >= start => Some(end),
            _ => return err("invalid_end_time", "Invalid end_time"),
        },
    };
    let (entities, devices, continuous) = match targets(conn, &args) {
        // Every entity asked for is continuous: an empty answer.
        Targets::Nothing => {
            out.result(id, Value::Null);
            out.send(historical_event(id, "[]", start, end.unwrap_or(now), false));
            return;
        }
        Targets::Invalid => return err("unknown_error", "Unknown error"),
        Targets::Some {
            entities,
            devices,
            continuous,
        } => (entities, devices, continuous),
    };
    let filter = Arc::new(Filter::new(&entities, &devices, None));
    let mut reader = Reader {
        recorder: recorder.clone(),
        filter: filter.clone(),
        continuous: continuous.clone(),
        known_empty: false,
    };

    // A finished window: one answer, nothing to follow.
    if let Some(end) = end.filter(|&end| end <= now) {
        conn.subs.insert(id, Subscription::Idle);
        out.result(id, Value::Null);
        tokio::spawn(async move {
            reader.recorder.sync().await;
            let send = |msg: String| out.send(msg);
            send_historical(&send, id, &reader, (start, end), false, false).await;
        });
        return;
    }

    // Live: follow first, so nothing between the read and the feed is lost.
    let mut feed = recorder.subscribe_live(filter.clone());
    let setup = hass.clock().now().as_microsecond();
    // A write that timestamped its state before `setup` may still be
    // publishing it: wait, so the read below finds it (the live side
    // drops everything at or before `setup`).
    hass.states().quiesce();
    out.result(id, Value::Null);
    let live = Arc::new(parking_lot::Mutex::new(true));
    let task_live = live.clone();
    let task = tokio::spawn(async move {
        let emit = |msg: String| {
            let live = task_live.lock();
            *live && out.send(msg)
        };
        // Everything fired before `setup` is in the store once this
        // returns, so one read is the past. (HA reads again after its
        // commit, for what its first read missed; there is none here, so
        // the second step only sends the closing message it sends when
        // that finds nothing.)
        reader.recorder.sync().await;
        // Whatever the store says, the client gets a first message.
        let Some(last) = send_historical(&emit, id, &reader, (start, setup), true, true).await
        else {
            return;
        };
        reader.known_empty = true;
        let from = last.unwrap_or(start) + 1;
        if send_historical(&emit, id, &reader, (from, setup), false, false)
            .await
            .is_none()
        {
            return;
        }
        // Past `end`, nothing more is sent, whatever is still being grouped.
        let deadline = end.map(|end| {
            let wait = u64::try_from(end - hass.clock().now().as_microsecond()).unwrap_or(0);
            tokio::time::Instant::now() + Duration::from_micros(wait)
        });
        let follow = follow(&mut feed, &filter, &continuous, setup, id, &emit, deadline);
        match deadline {
            Some(at) => {
                let _ = tokio::time::timeout_at(at, follow).await;
            }
            None => follow.await,
        }
    });
    conn.subs.insert(
        id,
        Subscription::Task(TaskSub {
            abort: task.abort_handle(),
            live,
        }),
    );
}

/// `_async_events_consumer`: the rows after `setup`, grouped for 350 ms
/// after the first, one message per group that has a row to show.
async fn follow(
    feed: &mut LiveFeed,
    filter: &Filter,
    continuous: &Continuous,
    setup: i64,
    id: u64,
    emit: &impl Fn(String) -> bool,
    deadline: Option<tokio::time::Instant>,
) {
    let wanted = |row: &ha_recorder::Row| row.ts() > setup && filter.follows(&row.as_rec());
    let mut batch: Vec<Arc<ha_recorder::Row>> = Vec::new();
    loop {
        // Filtered as they arrive: the batch holds only what is shown.
        while batch.is_empty() {
            let Some(row) = feed.recv().await else { return };
            if wanted(&row) {
                batch.push(row);
            }
        }
        // Group an event storm into one message.
        let group = tokio::time::sleep(COALESCE);
        tokio::pin!(group);
        loop {
            tokio::select! {
                () = &mut group => break,
                row = feed.recv() => {
                    let Some(row) = row else { return };
                    if wanted(&row) {
                        batch.push(row);
                        if batch.len() > MAX_PENDING_LOGBOOK_EVENTS {
                            return;
                        }
                    }
                }
            }
        }
        let msg = live_message(id, &batch, &**continuous);
        // `timeout_at` polls the loop first, and building the message
        // takes time: check the deadline last, so nothing is sent
        // after `end`.
        if deadline.is_some_and(|at| tokio::time::Instant::now() >= at) {
            return;
        }
        if let Some(msg) = msg
            && !emit(msg)
        {
            return;
        }
        batch.clear();
    }
}

/// `{"events": [...]}` for the rows the logbook shows, none if it shows none.
fn live_message(
    id: u64,
    rows: &[Arc<ha_recorder::Row>],
    continuous: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let mut humanify = Humanify::new(continuous);
    let mut text = format!(r#"{{"id":{id},"type":"event","event":{{"events":["#);
    let mut shown = 0;
    for row in rows {
        let mark = text.len();
        if shown > 0 {
            text.push(',');
        }
        if humanify.row(&mut text, &row.as_rec()) {
            shown += 1;
        } else {
            text.truncate(mark);
        }
    }
    text.push_str("]}}");
    (shown > 0).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `is_sensor_continuous`: the state's attributes while it has them,
    /// else the registry entry's state class or device class.
    #[test]
    fn continuity_reads_the_state_then_the_registry() {
        let hass = Hass::new();
        let set = |id: &str, attrs: Value| {
            let Value::Object(attrs) = attrs else {
                panic!()
            };
            hass.set_state(EntityId::parse(id).unwrap(), "1", attrs);
        };
        set("sensor.unit", json!({"unit_of_measurement": "W"}));
        set("sensor.class", json!({"state_class": "total"}));
        set("sensor.numeric", json!({"device_class": "battery"}));
        set("sensor.plain", json!({"friendly_name": "Plain"}));
        set("sensor.enum", json!({"device_class": "enum"}));
        // A state with no attributes defers to the registry, as HA's
        // truthiness check on them does.
        set("sensor.bare", json!({}));
        let entity = |id: &str, extra: Value| -> Arc<ha_registry::Entity> {
            let mut row = json!({"entity_id": id, "platform": "demo"});
            row.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            Arc::new(serde_json::from_value(row).unwrap())
        };
        let registries = ha_registry::Registries {
            entities: vec![
                entity(
                    "sensor.bare",
                    json!({"capabilities": {"state_class": "measurement"}}),
                ),
                entity(
                    "sensor.gone_class",
                    json!({"capabilities": {"state_class": "total"}}),
                ),
                entity("sensor.gone_device", json!({"device_class": "energy"})),
                entity(
                    "sensor.gone_empty",
                    json!({"capabilities": {"state_class": null}}),
                ),
                entity("sensor.gone_enum", json!({"device_class": "timestamp"})),
            ],
            ..Default::default()
        };
        let continuous = |id: &str| sensor_continuous(&hass, &registries, id);
        for (id, expected) in [
            ("sensor.unit", true),
            ("sensor.class", true),
            ("sensor.numeric", true),
            ("sensor.plain", false),
            ("sensor.enum", false),
            ("sensor.bare", true),
            ("sensor.gone_class", true),
            ("sensor.gone_device", true),
            ("sensor.gone_empty", false),
            ("sensor.gone_enum", false),
            ("sensor.unknown", false),
        ] {
            assert_eq!(continuous(id), expected, "{id}");
        }
    }
}

//! `history/history_during_period` and `history/stream`
//! (`components/history/websocket_api.py`), answered from the recorder's
//! segment store.

use super::recorder::{parse_datetime, str_list, string};
use super::{Conn, Invalid, Subscription, TaskSub, only_keys};
use ha_core::{EntityId, EventKind, Hass, State};
use ha_recorder::Recorder;
use ha_recorder::history::{
    NEED_ATTRIBUTE_DOMAINS, Query, SIGNIFICANT_DOMAINS, seconds, significant_states,
};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

/// `history/const.py`'s `EVENT_COALESCE_TIME`.
const COALESCE: Duration = Duration::from_millis(350);
/// `history/const.py`'s `MAX_PENDING_HISTORY_STATES`: a stream whose queue
/// grows past this is dropped.
const MAX_PENDING_HISTORY_STATES: usize = 2048;

pub(super) fn dispatch(
    conn: &mut Conn,
    id: u64,
    ty: &str,
    msg: &Map<String, Value>,
) -> Option<Result<(), Invalid>> {
    let stream = match ty {
        "history/history_during_period" => false,
        "history/stream" => true,
        _ => return None,
    };
    let recorder = conn.shared.config.recorder.clone()?;
    Some(Args::parse(msg).map(|args| {
        if stream {
            start_stream(conn, id, &recorder, args);
        } else {
            during_period(conn, id, &recorder, args);
        }
    }))
}

/// The two commands' shared schema.
struct Args {
    start_time: String,
    end_time: Option<String>,
    entity_ids: Vec<String>,
    include_start_time_state: bool,
    significant_changes_only: bool,
    minimal_response: bool,
    no_attributes: bool,
}

impl Args {
    fn parse(msg: &Map<String, Value>) -> Result<Self, Invalid> {
        only_keys(
            msg,
            &[
                "start_time",
                "end_time",
                "entity_ids",
                "include_start_time_state",
                "significant_changes_only",
                "minimal_response",
                "no_attributes",
            ],
        )?;
        let start_time = string(msg, "start_time", true)?.unwrap_or_default();
        let end_time = string(msg, "end_time", false)?;
        str_list(msg, "entity_ids", true)?;
        let flag = |key: &str, default: bool| match msg.get(key) {
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(format!(
                "expected bool for dictionary value @ data['{key}']"
            )),
            None => Ok(default),
        };
        Ok(Self {
            start_time: start_time.to_owned(),
            end_time: end_time.map(str::to_owned),
            entity_ids: msg["entity_ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            include_start_time_state: flag("include_start_time_state", true)?,
            significant_changes_only: flag("significant_changes_only", true)?,
            minimal_response: flag("minimal_response", false)?,
            no_attributes: flag("no_attributes", false)?,
        })
    }

    /// `valid_entity_id` (an entity that exists is valid too).
    fn entities_valid(&self) -> bool {
        self.entity_ids.iter().all(|e| EntityId::parse(e).is_ok())
    }

    /// `entities_may_have_state_changes_after`.
    fn may_have_changes_after(&self, hass: &Hass, start: i64) -> bool {
        self.entity_ids.iter().any(|e| {
            let Some(state) = EntityId::parse(e)
                .ok()
                .and_then(|id| hass.states().get(&id))
            else {
                return true;
            };
            let time = if self.no_attributes {
                state.last_changed
            } else {
                state.last_updated
            };
            time.as_microsecond() > start
        })
    }

    fn query<'a>(&'a self, start: i64, end: Option<i64>, include_start: bool) -> Query<'a> {
        Query {
            start,
            end,
            entity_ids: &self.entity_ids,
            include_start_state: include_start,
            significant_only: self.significant_changes_only,
            minimal: self.minimal_response,
            no_attributes: self.no_attributes,
        }
    }
}

/// The blocking read of a query, off the runtime.
async fn read(
    recorder: Arc<Recorder>,
    args: Arc<Args>,
    start: i64,
    end: Option<i64>,
    include_start: bool,
) -> Option<ha_recorder::history::Answer> {
    tokio::task::spawn_blocking(move || {
        significant_states(recorder.store(), &args.query(start, end, include_start))
    })
    .await
    .ok()
}

/// The times, and the answers HA gives before it reads anything.
enum Times {
    Ok { start: i64, end: Option<i64> },
    Replied,
}

fn times(conn: &Conn, id: u64, args: &Args, stream: bool) -> Times {
    let tz = &conn.shared.hass.config().time_zone;
    let now = conn.shared.hass.clock().now().as_microsecond();
    let err = |code: &str, message: &str| {
        conn.out.error(json!(id), code, message);
        Times::Replied
    };
    let Some(start) = parse_datetime(&args.start_time, tz).map(|t| t.as_microsecond()) else {
        return err("invalid_start_time", "Invalid start_time");
    };
    if stream && start > now {
        return err("invalid_start_time", "Invalid start_time");
    }
    let end = match args.end_time.as_deref().filter(|e| !e.is_empty()) {
        Some(e) => match parse_datetime(e, tz) {
            Some(t) => Some(t.as_microsecond()),
            None => return err("invalid_end_time", "Invalid end_time"),
        },
        None => None,
    };
    if stream && end.is_some_and(|end| end < start) {
        return err("invalid_end_time", "Invalid end_time");
    }
    if !stream && start > now {
        conn.out.result(id, json!({}));
        return Times::Replied;
    }
    Times::Ok { start, end }
}

fn during_period(conn: &mut Conn, id: u64, recorder: &Arc<Recorder>, args: Args) {
    let Times::Ok { start, end } = times(conn, id, &args, false) else {
        return;
    };
    let hass = &conn.shared.hass;
    if !args.entities_valid() {
        conn.out
            .error(json!(id), "invalid_entity_ids", "Invalid entity_ids");
        return;
    }
    if args.entity_ids.is_empty()
        || (!args.include_start_time_state && !args.may_have_changes_after(hass, start))
    {
        conn.out.result(id, json!({}));
        return;
    }
    let out = conn.out.clone();
    let recorder = recorder.clone();
    let include = args.include_start_time_state;
    let args = Arc::new(args);
    tokio::spawn(async move {
        // D5: reads see everything fired before them.
        recorder.sync().await;
        // `has_states_before`: nothing recorded as old as `end`.
        if end.is_some_and(|end| recorder.store().oldest_ts().is_none_or(|o| end < o)) {
            out.result(id, json!({}));
            return;
        }
        if let Some(answer) = read(recorder, args, start, end, include).await {
            out.result_raw(id, &answer.json);
        }
    });
}

/// An event message carrying a historical answer.
fn historical_event(id: u64, answer: &str, start: i64, end: i64) -> String {
    format!(
        r#"{{"id":{id},"type":"event","event":{{"states":{answer},"start_time":{:?},"end_time":{:?}}}}}"#,
        seconds(start),
        seconds(end)
    )
}

fn start_stream(conn: &mut Conn, id: u64, recorder: &Arc<Recorder>, args: Args) {
    let Times::Ok { start, end } = times(conn, id, &args, true) else {
        return;
    };
    let hass = conn.shared.hass.clone();
    if !args.entities_valid() {
        conn.out
            .error(json!(id), "invalid_entity_ids", "Invalid entity_ids");
        return;
    }
    let now = hass.clock().now().as_microsecond();
    let out = conn.out.clone();
    if args.entity_ids.is_empty() {
        out.result(id, Value::Null);
        out.send(historical_event(id, "{}", start, end.unwrap_or(now)));
        return;
    }
    let recorder = recorder.clone();
    let include = args.include_start_time_state;

    // A finished window: one answer, nothing to follow.
    if let Some(end) = end.filter(|&end| end <= now) {
        if !include && !args.may_have_changes_after(&hass, start) {
            out.result(id, Value::Null);
            out.send(historical_event(id, "{}", start, end));
            return;
        }
        conn.subs.insert(id, Subscription::Idle);
        out.result(id, Value::Null);
        let args = Arc::new(args);
        tokio::spawn(async move {
            recorder.sync().await;
            if let Some(answer) = read(recorder, args, start, Some(end), include).await {
                let last = answer.last_updated.unwrap_or(end);
                out.send(historical_event(id, &answer.json, start, last));
            }
        });
        return;
    }

    // Live: subscribe first, so nothing between the read and the feed is lost.
    let mut feed = hass.subscribe_state_changes();
    feed.snapshot = Vec::new();
    let setup = hass.clock().now().as_microsecond();
    // A write that timestamped its state before `setup` may still be
    // publishing it: wait, so the read below finds it (the live side
    // drops everything at or before `setup`).
    hass.states().quiesce();
    out.result(id, Value::Null);
    let live = Arc::new(parking_lot::Mutex::new(true));
    let task_live = live.clone();
    let wanted: HashSet<EntityId> = args
        .entity_ids
        .iter()
        .filter_map(|e| EntityId::parse(e).ok())
        .collect();
    let args = Arc::new(args);
    let task = tokio::spawn(async move {
        let emit = |msg: String| {
            let live = task_live.lock();
            *live && out.send(msg)
        };
        // Everything fired before `setup` is in the store once this
        // returns, so one read is the past. (HA reads again after its
        // commit; its first read can miss rows the writer holds.)
        recorder.sync().await;
        let Some(answer) = read(recorder, args.clone(), start, Some(setup), include).await else {
            return;
        };
        let msg = match answer.last_updated {
            Some(last) => historical_event(id, &answer.json, start, last),
            None => historical_event(id, "{}", start, setup),
        };
        if !emit(msg) {
            return;
        }
        // What queued during the read: a queue that grew past the limit is
        // dropped, as HA drops a client that falls behind.
        if feed.backlog() > MAX_PENDING_HISTORY_STATES {
            return;
        }
        // Past `end`, nothing more is sent, whatever is still being grouped.
        let deadline = end.map(|end| {
            let wait = u64::try_from(end - hass.clock().now().as_microsecond()).unwrap_or(0);
            tokio::time::Instant::now() + Duration::from_micros(wait)
        });
        let follow = async {
            let mut batch: Vec<Arc<ha_core::Event>> = Vec::new();
            loop {
                // Filtered as they arrive: the batch holds only what is sent.
                while batch.is_empty() {
                    let Some(e) = feed.recv().await else { return };
                    if feed.backlog() > MAX_PENDING_HISTORY_STATES {
                        return;
                    }
                    if forward(&e, &wanted, &args) && e.time_fired.as_microsecond() > setup {
                        batch.push(e);
                    }
                }
                // Group an event storm into one message.
                let group = tokio::time::sleep(COALESCE);
                tokio::pin!(group);
                loop {
                    tokio::select! {
                        () = &mut group => break,
                        e = feed.recv() => {
                            let Some(e) = e else { return };
                            if feed.backlog() > MAX_PENDING_HISTORY_STATES {
                                return;
                            }
                            if forward(&e, &wanted, &args) {
                                batch.push(e);
                                if batch.len() > MAX_PENDING_HISTORY_STATES {
                                    return;
                                }
                            }
                        }
                    }
                }
                let msg = live_states(&batch, args.no_attributes).map(|states| {
                    format!(r#"{{"id":{id},"type":"event","event":{{"states":{states}}}}}"#)
                });
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
        };
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

/// `_forward_state_events_filtered`.
fn forward(event: &ha_core::Event, wanted: &HashSet<EntityId>, args: &Args) -> bool {
    let EventKind::StateChanged {
        entity_id,
        old_state: Some(old),
        new_state: Some(new),
    } = &event.kind
    else {
        return false;
    };
    wanted.contains(entity_id)
        && !((args.significant_changes_only || args.minimal_response)
            && new.state == old.state
            && !SIGNIFICANT_DOMAINS.contains(&entity_id.domain()))
}

/// `_events_to_compressed_states`: `{entity_id: [state, …]}`, entities in
/// the order they first appear.
fn live_states(events: &[Arc<ha_core::Event>], no_attributes: bool) -> Option<String> {
    let mut order: Vec<&EntityId> = Vec::new();
    for e in events {
        if let EventKind::StateChanged { entity_id, .. } = &e.kind
            && !order.contains(&entity_id)
        {
            order.push(entity_id);
        }
    }
    if order.is_empty() {
        return None;
    }
    let mut out = String::from("{");
    for (i, entity) in order.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{}:[", json!(entity.as_str()));
        let mut first = true;
        for e in events {
            let EventKind::StateChanged {
                entity_id,
                new_state: Some(new),
                ..
            } = &e.kind
            else {
                continue;
            };
            if entity_id != *entity {
                continue;
            }
            if !std::mem::take(&mut first) {
                out.push(',');
            }
            live_state(&mut out, new, no_attributes);
        }
        out.push(']');
    }
    out.push('}');
    Some(out)
}

/// `_history_compressed_state`.
fn live_state(out: &mut String, state: &State, no_attributes: bool) {
    let _ = write!(out, r#"{{"s":{}"#, json!(state.state.as_str()));
    if !no_attributes || NEED_ATTRIBUTE_DOMAINS.contains(&state.entity_id.domain()) {
        out.push_str(",\"a\":");
        out.push_str(&serde_json::to_string(&state.attributes).unwrap_or_else(|_| "{}".into()));
    }
    let _ = write!(
        out,
        r#","lu":{:?}"#,
        seconds(state.last_updated.as_microsecond())
    );
    if state.last_changed != state.last_updated {
        let _ = write!(
            out,
            r#","lc":{:?}"#,
            seconds(state.last_changed.as_microsecond())
        );
    }
    out.push('}');
}

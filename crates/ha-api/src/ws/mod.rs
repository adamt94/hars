//! `/api/websocket`: HA's WebSocket API (`components/websocket_api`).
//!
//! Two tasks per connection: one reads client frames and runs commands, the
//! other writes what commands and subscriptions queue in a bounded outbox. A client
//! that falls 4096 messages behind is disconnected, as in HA; the frontend
//! reconnects and gets a fresh snapshot. Once the client sends
//! `supported_features` with `coalesce_messages`, queued messages go out
//! together as one JSON array per frame.

mod analytics;
mod automation;
mod commands;
mod config;
mod config_entries;
mod entities;
mod frontend;
mod history;
mod logbook;
mod manifest;
mod mobile_app;
mod recorder;
mod sensor;
mod weather;

pub use config::WHOAMI_URL;

use crate::Shared;
use crate::auth::RemoteIp;
use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
    routing::get,
};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use ha_auth::{Permission, Principal, User};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{Notify, broadcast, mpsc};
use tokio::task::AbortHandle;

/// `ws/http.py`: the client has this long to authenticate.
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
/// `MAX_PENDING_MSG`: queued messages before a client is cut off.
const MAX_PENDING: usize = 4096;
/// `PENDING_MSG_MAX_FORCE_READY`: most messages coalesced into one frame.
const MAX_BATCH: usize = 256;
/// aiohttp's default `max_msg_size`, which HA keeps.
const MAX_MESSAGE: usize = 4 * 1024 * 1024;
/// `WebSocketResponse(heartbeat=55)`: a WebSocket ping this often. A client
/// that hasn't answered the previous ping by the next one is gone (a phone
/// that lost its network), and its subscriptions are dropped.
const HEARTBEAT: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(55)
};

pub(crate) fn routes(shared: Shared) -> Router {
    Router::new()
        .route("/api/websocket", get(upgrade))
        .with_state(shared)
}

async fn upgrade(
    ws: WebSocketUpgrade,
    State(shared): State<Shared>,
    RemoteIp(ip): RemoteIp,
) -> Response {
    ws.max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| run(socket, shared, ip))
}

/// What the writer task sends.
enum Frame {
    Json(String),
    Ping,
}

/// The connection's outgoing queue, shared with its subscription tasks.
#[derive(Clone)]
pub(crate) struct Outbox {
    tx: mpsc::Sender<Frame>,
    overflow: Arc<Notify>,
}

impl Outbox {
    fn queue(&self, frame: Frame) -> bool {
        match self.tx.try_send(frame) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.overflow.notify_one();
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Queue a message; `false` once the connection is gone or too far behind.
    pub(crate) fn send(&self, msg: String) -> bool {
        self.queue(Frame::Json(msg))
    }

    pub(crate) fn result(&self, id: u64, result: Value) -> bool {
        self.send(
            json!({"id": id, "type": "result", "success": true, "result": result}).to_string(),
        )
    }

    /// A result whose JSON is already written.
    pub(crate) fn result_raw(&self, id: u64, result: &str) -> bool {
        self.send(format!(
            r#"{{"id":{id},"type":"result","success":true,"result":{result}}}"#
        ))
    }

    pub(crate) fn error(&self, id: Value, code: &str, message: &str) -> bool {
        self.error_with(id, json!({"code": code, "message": message}))
    }

    pub(crate) fn error_with(&self, id: Value, error: Value) -> bool {
        self.send(json!({"id": id, "type": "result", "success": false, "error": error}).to_string())
    }

    pub(crate) fn event(&self, id: u64, event: &Value) -> bool {
        self.send(json!({"id": id, "type": "event", "event": event}).to_string())
    }

    /// Tell the connection to close (a subscription can't keep up).
    pub(crate) fn overflowed(&self) {
        self.overflow.notify_one();
    }
}

/// A subscription served by its own task. `live` is held while the task
/// queues an event and while unsubscribing flips it and queues the reply, so
/// no event can follow the unsubscribe result.
pub(crate) struct TaskSub {
    pub abort: AbortHandle,
    pub live: Arc<parking_lot::Mutex<bool>>,
}

/// What a subscription id refers to, for `unsubscribe_events`.
pub(crate) enum Subscription {
    /// Served by the connection's shared bus listener.
    Events,
    Task(TaskSub),
    /// Fed by a listener elsewhere, which checks `live` (held while it
    /// queues an event, as [`TaskSub`]'s) and lets go once it's false.
    Listener(Arc<parking_lot::Mutex<bool>>),
    /// Registered so it can be unsubscribed, but nothing ever fires it
    /// (`persistent_notification/subscribe` while there are none).
    Idle,
}

/// `subscribe_events` ids by event type. The bus listener holds the lock
/// while it queues an event, and (un)subscribing holds it while it changes
/// the map and queues its result, so results and events are ordered.
pub(crate) type EventSubs = Arc<parking_lot::Mutex<HashMap<String, Vec<u64>>>>;

pub(crate) struct Conn {
    shared: Shared,
    principal: Principal,
    user: User,
    out: Outbox,
    coalesce: Arc<AtomicBool>,
    last_id: u64,
    subs: HashMap<u64, Subscription>,
    /// Served by one bus listener, so each event wakes the connection once
    /// however many types it wants. It runs only while the map is non-empty.
    events: EventSubs,
    event_listener: Option<AbortHandle>,
}

async fn run(mut socket: WebSocket, shared: Shared, ip: Option<IpAddr>) {
    let hello = json!({"type": "auth_required", "ha_version": ha_auth::HA_VERSION});
    if socket
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    // Before the token is checked, so a revocation in between isn't missed.
    let mut revocations = shared.auth.subscribe_revocations();
    let Some((principal, user)) = authenticate(&mut socket, &shared, ip).await else {
        let _ = socket.send(Message::Close(None)).await;
        return;
    };
    let ok = json!({"type": "auth_ok", "ha_version": ha_auth::HA_VERSION});
    if socket
        .send(Message::Text(ok.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    // Writing has its own task, so a client that stops reading can't stall
    // the overflow, heartbeat and revocation checks below.
    let (sink, mut stream) = socket.split();
    let (tx, rx) = mpsc::channel(MAX_PENDING);
    let coalesce = Arc::new(AtomicBool::new(false));
    let closing = Arc::new(Notify::new());
    let mut writer = tokio::spawn(write(sink, rx, coalesce.clone(), closing.clone()));
    let overflow = Arc::new(Notify::new());
    let mut conn = Conn {
        shared,
        principal,
        user,
        out: Outbox {
            tx,
            overflow: overflow.clone(),
        },
        coalesce,
        last_id: 0,
        subs: HashMap::new(),
        events: Default::default(),
        event_listener: None,
    };

    let mut heartbeat =
        tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut awaiting_pong = false;
    let writer_done = loop {
        tokio::select! {
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if !conn.handle_frame(&text) {
                        break false;
                    }
                }
                Some(Ok(Message::Pong(_))) => awaiting_pong = false,
                Some(Ok(Message::Ping(_))) => {}
                // Close, an error, or a binary frame (no binary handlers exist).
                _ => break false,
            },
            _ = heartbeat.tick() => {
                if awaiting_pong {
                    tracing::debug!("websocket client missed a heartbeat; disconnecting");
                    break false;
                }
                if !conn.out.queue(Frame::Ping) {
                    break false;
                }
                awaiting_pong = true;
            }
            _ = overflow.notified() => {
                tracing::error!("websocket client fell {MAX_PENDING} messages behind; disconnecting");
                break false;
            }
            revoked = revocations.recv() => match revoked {
                Ok(id) if id == conn.principal.refresh_token_id => break false,
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if !conn.shared.auth.is_token_live(&conn.principal.refresh_token_id) {
                        break false;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break false,
            },
            _ = &mut writer => break true,
        }
    };
    conn.close();
    if !writer_done {
        // Say goodbye if the writer can, but never wait on a stuck client. The
        // signal is outside the queue, so a full outbox can't swallow it.
        closing.notify_one();
        if tokio::time::timeout(Duration::from_secs(1), &mut writer)
            .await
            .is_err()
        {
            writer.abort();
        }
    }
}

/// The writer task. With coalescing on, everything already queued (up to
/// 256 messages) goes out as one JSON array.
async fn write(
    mut sink: SplitSink<WebSocket, Message>,
    mut rx: mpsc::Receiver<Frame>,
    coalesce: Arc<AtomicBool>,
    closing: Arc<Notify>,
) {
    let mut next = None;
    loop {
        let frame = match next.take() {
            Some(f) => f,
            None => tokio::select! {
                // Closing wins over what's still queued.
                biased;
                _ = closing.notified() => {
                    let _ = sink.send(Message::Close(None)).await;
                    return;
                }
                f = rx.recv() => match f {
                    Some(f) => f,
                    None => return,
                },
            },
        };
        let msg = match frame {
            Frame::Ping => Message::Ping(Default::default()),
            Frame::Json(first) if !coalesce.load(Ordering::Relaxed) => Message::Text(first.into()),
            Frame::Json(first) => {
                let mut batch = vec![first];
                while batch.len() < MAX_BATCH {
                    match rx.try_recv() {
                        Ok(Frame::Json(m)) => batch.push(m),
                        Ok(other) => {
                            next = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                let text = if batch.len() == 1 {
                    batch.pop().expect("one message")
                } else {
                    format!("[{}]", batch.join(","))
                };
                Message::Text(text.into())
            }
        };
        if sink.send(msg).await.is_err() {
            return;
        }
    }
}

/// `ws/auth.py`: `{"type": "auth", "access_token": ...}` within 10 s.
async fn authenticate(
    socket: &mut WebSocket,
    shared: &Shared,
    ip: Option<IpAddr>,
) -> Option<(Principal, User)> {
    let msg = tokio::time::timeout(AUTH_TIMEOUT, socket.recv())
        .await
        .ok()??
        .ok()?;
    let Message::Text(text) = msg else {
        return None;
    };
    // Invalid JSON closes the connection without a message.
    let msg: Value = serde_json::from_str(&text).ok()?;
    let invalid = |message: String| json!({"type": "auth_invalid", "message": message}).to_string();

    let token = match auth_token(&msg) {
        Ok(token) => token,
        Err(why) => {
            let why = humanize(&msg, &why);
            let reply = invalid(format!("Auth message incorrectly formatted: {why}"));
            let _ = socket.send(Message::Text(reply.into())).await;
            return None;
        }
    };
    let found = shared
        .auth
        .validate_access_token(token, ip)
        .filter(|p| p.permission > Permission::None)
        .and_then(|p| shared.auth.user(&p.user_id).map(|u| (p, u)));
    if found.is_none() {
        crate::auth::log_invalid(ip, "websocket");
        let reply = invalid("Invalid access token or password".into());
        let _ = socket.send(Message::Text(reply.into())).await;
    }
    found
}

fn auth_token(msg: &Value) -> Result<&str, String> {
    let Value::Object(m) = msg else {
        return Err("expected a dictionary".into());
    };
    if m.get("type").and_then(Value::as_str) != Some("auth") {
        return Err("value must be one of ['auth'] for dictionary value @ data['type']".into());
    }
    if let Some(k) = m
        .keys()
        .find(|k| !matches!(k.as_str(), "type" | "access_token" | "api_password"))
    {
        return Err(format!("extra keys not allowed @ data['{k}']"));
    }
    match m.get("access_token") {
        Some(Value::String(t)) => Ok(t),
        Some(_) => Err("expected str for dictionary value @ data['access_token']".into()),
        // `api_password` auth was removed from HA; nothing accepts it.
        None => Ok(""),
    }
}

impl Conn {
    /// One client frame: a message, or (clients may batch) an array of them.
    /// `false` means disconnect: invalid JSON gets no reply in HA either.
    fn handle_frame(&mut self, text: &str) -> bool {
        match serde_json::from_str::<Value>(text) {
            Ok(Value::Array(msgs)) => msgs.into_iter().for_each(|m| self.handle(m)),
            Ok(msg) => self.handle(msg),
            Err(_) => return false,
        }
        true
    }

    /// `ActiveConnection.async_handle`.
    fn handle(&mut self, msg: Value) {
        let parsed = match &msg {
            Value::Object(m) => {
                let id = m.get("id").and_then(Value::as_u64).filter(|&id| id > 0);
                let ty = m
                    .get("type")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty());
                id.zip(ty).map(|(id, ty)| (id, ty, m))
            }
            _ => None,
        };
        let Some((id, ty, m)) = parsed else {
            let id = msg.get("id").cloned().unwrap_or(json!(0));
            self.out
                .error(id, "invalid_format", "Message incorrectly formatted.");
            return;
        };
        if id <= self.last_id {
            self.out
                .error(json!(id), "id_reuse", "Identifier values have to increase.");
            return;
        }
        if commands::dispatch(self, id, ty, m) {
            self.last_id = id;
        } else {
            self.out
                .error(json!(id), "unknown_command", "Unknown command.");
        }
    }

    /// Start the shared bus listener if it isn't running.
    pub(crate) fn ensure_event_listener(&mut self) {
        if self.event_listener.is_some() {
            return;
        }
        let mut rx = self.shared.hass.bus().subscribe();
        let events = self.events.clone();
        let out = self.out.clone();
        let task = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let event_type = commands::event_type(&event.kind);
                        let subs = events.lock();
                        let all = subs.get("*").into_iter().flatten();
                        let typed = subs.get(event_type).into_iter().flatten();
                        let mut ids = all.chain(typed).peekable();
                        if ids.peek().is_none() {
                            continue;
                        }
                        let json = commands::event_json(&event);
                        for &id in ids {
                            if !out.event(id, &json) {
                                return;
                            }
                        }
                    }
                    // HA disconnects a client that falls behind; so do we.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        out.overflowed();
                        return;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        self.event_listener = Some(task.abort_handle());
    }

    /// End subscription `id`, running `reply` (the unsubscribe result) at the
    /// point after which no more of its events can be queued.
    pub(crate) fn unsubscribe(&mut self, id: u64, reply: impl FnOnce(&Outbox)) -> bool {
        match self.subs.remove(&id) {
            Some(Subscription::Task(task)) => {
                let mut live = task.live.lock();
                *live = false;
                task.abort.abort();
                reply(&self.out);
            }
            Some(Subscription::Events) => {
                let mut events = self.events.lock();
                for ids in events.values_mut() {
                    ids.retain(|&i| i != id);
                }
                events.retain(|_, ids| !ids.is_empty());
                reply(&self.out);
                if events.is_empty() {
                    // Nothing left to listen for: stop waking on every event.
                    if let Some(listener) = self.event_listener.take() {
                        listener.abort();
                    }
                }
            }
            Some(Subscription::Listener(live)) => {
                let mut live = live.lock();
                *live = false;
                reply(&self.out);
            }
            Some(Subscription::Idle) => reply(&self.out),
            None => return false,
        }
        true
    }

    fn close(&mut self) {
        let ids: Vec<u64> = self.subs.keys().copied().collect();
        for id in ids {
            self.unsubscribe(id, |_| {});
        }
        if let Some(listener) = self.event_listener.take() {
            listener.abort();
        }
    }
}

/// A command's arguments failed validation (`vol.Invalid`).
pub(crate) type Invalid = String;

/// Reject keys beyond `id`, `type` and `allowed`, as voluptuous does.
/// voluptuous's `humanize_error`, as HA sends a schema error: the message,
/// `. Got `, and Python's `repr` of the value at the error's path (the
/// whole message when it names none; `None` when nothing is there), cut to
/// 500 characters.
pub(crate) fn humanize(msg: &Value, why: &str) -> String {
    // Several errors (voluptuous's `MultipleInvalid`): one line each,
    // sorted as `humanize_error` sorts them.
    if why.contains('\n') {
        let mut lines: Vec<String> = why.lines().map(|line| humanize(msg, line)).collect();
        lines.sort_unstable();
        return lines.join("\n");
    }
    let at = match why.rfind(" @ data") {
        Some(i) => path_value(msg, &why[i + " @ data".len()..]),
        None => Some(msg),
    };
    let mut got = String::new();
    match at {
        Some(v) => py_repr(&mut got, v),
        None => got.push_str("None"),
    }
    if got.chars().count() > 500 {
        got = got.chars().take(497).chain("...".chars()).collect();
    }
    format!("{why}. Got {got}")
}

/// The value at a voluptuous path such as `['integration'][0]`.
fn path_value<'a>(mut v: &'a Value, mut path: &str) -> Option<&'a Value> {
    while let Some(rest) = path.strip_prefix('[') {
        let (segment, after) = if let Some(quoted) = rest.strip_prefix('\'') {
            let end = quoted.find("']")?;
            (Some(&quoted[..end]), &quoted[end + 2..])
        } else {
            let end = rest.find(']')?;
            let i: usize = rest[..end].parse().ok()?;
            v = v.as_array()?.get(i)?;
            (None, &rest[end + 1..])
        };
        if let Some(key) = segment {
            v = v.as_object()?.get(key)?;
        }
        path = after;
    }
    Some(v)
}

/// Python's `repr` of a JSON value.
pub(crate) fn py_repr(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("None"),
        Value::Bool(true) => out.push_str("True"),
        Value::Bool(false) => out.push_str("False"),
        Value::Number(n) => match n.as_f64().filter(|_| n.is_f64()) {
            Some(f) => out.push_str(&ha_integrations::sun::py_float(f)),
            None => out.push_str(&n.to_string()),
        },
        Value::String(s) => py_repr_str(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                py_repr(out, item);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            for (i, (k, item)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                py_repr_str(out, k);
                out.push_str(": ");
                py_repr(out, item);
            }
            out.push('}');
        }
    }
}

/// Python's `repr` of a `str`: single quotes unless only `'` is inside,
/// escapes for `\\`, the quote and control characters.
pub(crate) fn py_repr_str(out: &mut String, s: &str) {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x100 && c.is_control() => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
}

pub(crate) fn only_keys(msg: &Map<String, Value>, allowed: &[&str]) -> Result<(), Invalid> {
    let extra = msg
        .keys()
        .find(|k| !matches!(k.as_str(), "id" | "type") && !allowed.contains(&k.as_str()));
    match extra {
        // HA's message for commands without a schema has no path.
        Some(_) if allowed.is_empty() => Err("extra keys not allowed".into()),
        Some(k) => Err(format!("extra keys not allowed @ data['{k}']")),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod humanize_tests {
    use super::humanize;
    use serde_json::json;

    /// Each from voluptuous 0.15.2's `humanize_error` in CPython 3.14.
    #[test]
    fn like_voluptuous() {
        let m = json!({"id": 1, "type": "t", "a": {"b": [1, 2.0, "it's", "say \"hi\"", "both ' \"", null, true, "tab\there\n", "é€", "ctl\u{1}"]}});
        assert_eq!(
            humanize(&m, "bad @ data['a']"),
            r#"bad @ data['a']. Got {'b': [1, 2.0, "it's", 'say "hi"', 'both \' "', None, True, 'tab\there\n', 'é€', 'ctl\x01']}"#
        );
        let m = json!({"id": 1, "type": "t", "a": [{"k": 1e20}, {"k": 1e-7}]});
        assert_eq!(
            humanize(&m, "bad @ data['a'][1]['k']"),
            "bad @ data['a'][1]['k']. Got 1e-07"
        );
        let m = json!({"id": 1, "type": "t"});
        assert_eq!(
            humanize(&m, "bad @ data['missing'][0]"),
            "bad @ data['missing'][0]. Got None"
        );
        // No path: the whole message.
        assert_eq!(
            humanize(&m, "extra keys not allowed"),
            "extra keys not allowed. Got {'id': 1, 'type': 't'}"
        );
        // Cut to 500 characters.
        let m = json!({"s": "x".repeat(600)});
        let got = humanize(&m, "long @ data['s']");
        assert_eq!(
            got,
            format!("long @ data['s']. Got '{}...", "x".repeat(496))
        );
    }
}

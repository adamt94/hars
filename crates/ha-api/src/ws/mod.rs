//! `/api/websocket`: HA's WebSocket API (`components/websocket_api`).
//!
//! One task per connection. It reads client frames, and writes what the
//! command handlers and subscriptions queue in a bounded outbox. A client
//! that falls 4096 messages behind is disconnected, as in HA; the frontend
//! reconnects and gets a fresh snapshot. Once the client sends
//! `supported_features` with `coalesce_messages`, queued messages go out
//! together as one JSON array per frame.

mod commands;
mod entities;

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
use ha_auth::{Permission, Principal, User};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
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

/// The connection's outgoing queue, shared with its subscription tasks.
#[derive(Clone)]
pub(crate) struct Outbox {
    tx: mpsc::Sender<String>,
    overflow: Arc<Notify>,
}

impl Outbox {
    /// Queue a message; `false` once the connection is gone or too far behind.
    pub(crate) fn send(&self, msg: String) -> bool {
        match self.tx.try_send(msg) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.overflow.notify_one();
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    pub(crate) fn result(&self, id: u64, result: Value) -> bool {
        self.send(
            json!({"id": id, "type": "result", "success": true, "result": result}).to_string(),
        )
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

/// What a subscription id refers to, for `unsubscribe_events`.
enum Subscription {
    /// Served by the connection's shared bus listener.
    Events,
    Task(AbortHandle),
}

pub(crate) struct Conn {
    shared: Shared,
    principal: Principal,
    user: User,
    out: Outbox,
    coalesce: bool,
    last_id: u64,
    subs: HashMap<u64, Subscription>,
    /// `subscribe_events` ids by event type, served by one bus listener so
    /// each event wakes the connection once, however many types it wants.
    events: Arc<parking_lot::Mutex<HashMap<String, Vec<u64>>>>,
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

    let mut revocations = shared.auth.subscribe_revocations();
    let (tx, mut rx) = mpsc::channel(MAX_PENDING);
    let overflow = Arc::new(Notify::new());
    let mut conn = Conn {
        shared,
        principal,
        user,
        out: Outbox {
            tx,
            overflow: overflow.clone(),
        },
        coalesce: false,
        last_id: 0,
        subs: HashMap::new(),
        events: Default::default(),
        event_listener: None,
    };

    let mut heartbeat =
        tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut awaiting_pong = false;
    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if !conn.handle_frame(&text) {
                        break;
                    }
                }
                Some(Ok(Message::Pong(_))) => awaiting_pong = false,
                Some(Ok(Message::Ping(_))) => {}
                // Close, an error, or a binary frame (no binary handlers exist).
                _ => break,
            },
            Some(first) = rx.recv() => {
                if flush(&mut socket, &mut rx, first, conn.coalesce).await.is_err() {
                    break;
                }
            }
            _ = heartbeat.tick() => {
                if awaiting_pong {
                    tracing::debug!("websocket client missed a heartbeat; disconnecting");
                    break;
                }
                if socket.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
                awaiting_pong = true;
            }
            _ = overflow.notified() => {
                tracing::error!("websocket client fell {MAX_PENDING} messages behind; disconnecting");
                break;
            }
            revoked = revocations.recv() => match revoked {
                Ok(id) if id == conn.principal.refresh_token_id => break,
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if !conn.shared.auth.is_token_live(&conn.principal.refresh_token_id) {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    conn.close();
    let _ = socket.send(Message::Close(None)).await;
}

/// Send the first queued message, and with coalescing everything else that
/// is already queued, as one array.
async fn flush(
    socket: &mut WebSocket,
    rx: &mut mpsc::Receiver<String>,
    first: String,
    coalesce: bool,
) -> Result<(), axum::Error> {
    if !coalesce {
        return socket.send(Message::Text(first.into())).await;
    }
    let mut batch = vec![first];
    while batch.len() < MAX_BATCH {
        match rx.try_recv() {
            Ok(m) => batch.push(m),
            Err(_) => break,
        }
    }
    let text = if batch.len() == 1 {
        batch.pop().expect("one message")
    } else {
        format!("[{}]", batch.join(","))
    };
    socket.send(Message::Text(text.into())).await
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
    fn ensure_event_listener(&mut self) {
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
                        let ids: Vec<u64> = {
                            let subs = events.lock();
                            let all = subs.get("*").into_iter().flatten();
                            let typed = subs.get(event_type).into_iter().flatten();
                            all.chain(typed).copied().collect()
                        };
                        if ids.is_empty() {
                            continue;
                        }
                        let json = commands::event_json(&event);
                        for id in ids {
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

    /// `unsubscribe_events` and connection close.
    fn unsubscribe(&mut self, id: u64) -> bool {
        match self.subs.remove(&id) {
            Some(Subscription::Task(task)) => task.abort(),
            Some(Subscription::Events) => {
                for ids in self.events.lock().values_mut() {
                    ids.retain(|&i| i != id);
                }
            }
            None => return false,
        }
        true
    }

    fn close(&mut self) {
        for (_, sub) in self.subs.drain() {
            if let Subscription::Task(task) = sub {
                task.abort();
            }
        }
        if let Some(task) = self.event_listener.take() {
            task.abort();
        }
    }
}

/// A command's arguments failed validation (`vol.Invalid`).
pub(crate) type Invalid = String;

/// Reject keys beyond `id`, `type` and `allowed`, as voluptuous does.
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

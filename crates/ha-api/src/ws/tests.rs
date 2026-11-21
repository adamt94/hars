//! A real server on a loopback port, driven by a real WebSocket client.

use futures_util::{SinkExt, StreamExt};
use ha_auth::Auth;
use ha_core::{Attributes, EntityId, Hass, ServiceResponse, SupportsResponse};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub(crate) struct Server {
    pub(crate) url: String,
    pub(crate) hass: Hass,
    pub(crate) auth: Auth,
}

async fn server() -> Server {
    server_with(crate::ApiConfig {
        config_dir: "/config".into(),
        ..Default::default()
    })
    .await
}

async fn server_with(config: crate::ApiConfig) -> Server {
    server_on(Hass::new(), config).await
}

pub(crate) async fn server_on(hass: Hass, config: crate::ApiConfig) -> Server {
    hass.start();
    let auth = crate::test_util::auth(true);
    let app = crate::router(hass.clone(), auth.clone(), &config)
        .into_make_service_with_connect_info::<std::net::SocketAddr>();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/api/websocket", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { url, hass, auth }
}

/// A [`Client`] that a task of its own keeps reading, so the server's
/// heartbeat is answered however long a test leaves the connection alone.
/// A `Client` answers a ping only while the test reads it: one left idle
/// for two heartbeats (400 ms in test builds, which a slow machine spends
/// on a few steps of a replay) is dropped by the server, and its next use
/// fails with a broken pipe.
pub(crate) struct Kept {
    out: tokio::sync::mpsc::UnboundedSender<Value>,
    inbox: tokio::sync::mpsc::UnboundedReceiver<Result<Value, String>>,
}

impl Kept {
    /// Send; `at` says where the test is, should the connection be gone.
    pub(crate) fn send(&self, msg: Value, at: &str) {
        if self.out.send(msg).is_err() {
            panic!("{at}: the WebSocket is closed");
        }
    }

    /// The next message, or `None` after `wait` without one. A connection
    /// that failed panics, saying how and `at`.
    pub(crate) async fn recv_within(&mut self, wait: Duration, at: &str) -> Option<Value> {
        match tokio::time::timeout(wait, self.inbox.recv()).await {
            Ok(Some(Ok(msg))) => Some(msg),
            Ok(Some(Err(why))) => panic!("{at}: the WebSocket failed: {why}"),
            Ok(None) => panic!("{at}: the WebSocket is closed"),
            Err(_) => None,
        }
    }

    /// The next message; none within 5 s is a failure.
    pub(crate) async fn recv(&mut self, at: &str) -> Value {
        match self.recv_within(Duration::from_secs(5), at).await {
            Some(msg) => msg,
            None => panic!("{at}: no message within 5 s"),
        }
    }
}

pub(crate) struct Client {
    ws: Ws,
    /// Messages from coalesced frames not yet returned.
    pending: VecDeque<Value>,
    frames: Vec<bool>,
}

impl Client {
    async fn connect(url: &str) -> Self {
        let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        Self {
            ws,
            pending: VecDeque::new(),
            frames: Vec::new(),
        }
    }

    pub(crate) async fn send(&mut self, msg: Value) {
        self.ws
            .send(Message::Text(msg.to_string().into()))
            .await
            .unwrap();
    }

    /// The next message, unpacking coalesced arrays. Records whether each
    /// frame was an array.
    pub(crate) async fn recv(&mut self) -> Value {
        if let Some(v) = self.pending.pop_front() {
            return v;
        }
        // Heartbeat pings (200 ms in test builds) can arrive between
        // messages on a slow machine; tungstenite answers them itself.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let text = loop {
            let frame = tokio::time::timeout_at(deadline, self.ws.next())
                .await
                .expect("no message within 5 s")
                .expect("stream ended")
                .unwrap();
            match frame {
                Message::Text(text) => break text,
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("unexpected frame {other:?}"),
            }
        };
        match serde_json::from_str(&text).unwrap() {
            Value::Array(msgs) => {
                self.frames.push(true);
                self.pending.extend(msgs);
                self.pending.pop_front().unwrap()
            }
            v => {
                self.frames.push(false);
                v
            }
        }
    }

    /// Whether the server closes the connection within 2 s. Reading answers
    /// heartbeat pings, so a live connection stays open.
    async fn closed(&mut self) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            match tokio::time::timeout_at(deadline, self.ws.next()).await {
                Err(_) => return false,
                Ok(None | Some(Err(_)) | Some(Ok(Message::Close(_)))) => return true,
                Ok(Some(Ok(_))) => {}
            }
        }
    }

    /// Hand the connection to a task that reads it all the time: see
    /// [`Kept`].
    pub(crate) fn kept(self) -> Kept {
        assert!(self.pending.is_empty(), "messages not yet read");
        let (out, mut to_send) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let (got, inbox) = tokio::sync::mpsc::unbounded_channel();
        let mut ws = self.ws;
        tokio::spawn(async move {
            // What ends the connection is told to whoever reads next.
            let end = loop {
                tokio::select! {
                    msg = to_send.recv() => {
                        let Some(msg) = msg else { return };
                        if let Err(e) = ws.send(Message::Text(msg.to_string().into())).await {
                            break format!("sending failed: {e}");
                        }
                    }
                    frame = ws.next() => match frame {
                        Some(Ok(Message::Text(text))) => {
                            let msgs = match serde_json::from_str(&text).unwrap() {
                                Value::Array(msgs) => msgs,
                                msg => vec![msg],
                            };
                            for msg in msgs {
                                let _ = got.send(Ok(msg));
                            }
                        }
                        // Reading is what answers a ping.
                        Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                        Some(Ok(other)) => break format!("unexpected frame {other:?}"),
                        Some(Err(e)) => break format!("reading failed: {e}"),
                        None => break "the server closed the connection".to_owned(),
                    },
                }
            };
            let _ = got.send(Err(end));
        });
        Kept { out, inbox }
    }

    pub(crate) async fn authed(server: &Server, token: &str) -> Self {
        let mut c = Self::connect(&server.url).await;
        assert_eq!(
            c.recv().await,
            json!({"type": "auth_required", "ha_version": "2026.6.1"})
        );
        c.send(json!({"type": "auth", "access_token": token})).await;
        assert_eq!(
            c.recv().await,
            json!({"type": "auth_ok", "ha_version": "2026.6.1"})
        );
        c
    }

    async fn call(&mut self, msg: Value) -> Value {
        self.send(msg).await;
        self.recv().await
    }
}

/// A login for the owner, through the same code paths the HTTP views use.
pub(crate) fn token(auth: &Auth) -> (String, String) {
    const CLIENT: &str = "http://127.0.0.1:8123/";
    let flow = auth.start_login_flow(CLIENT, None);
    let user = auth.check_password("alex", "secret").unwrap();
    let code = auth.finish_login_flow(&flow, CLIENT, &user, None).unwrap();
    let t = auth.exchange_code(Some(CLIENT), Some(&code), None).unwrap();
    (t.access_token, t.refresh_token.unwrap())
}

fn attrs(v: Value) -> Attributes {
    let Value::Object(m) = v else { panic!() };
    m
}

#[tokio::test]
async fn handshake() {
    let s = server().await;
    let (access, _) = token(&s.auth);

    let mut bad = Client::connect(&s.url).await;
    bad.recv().await;
    bad.send(json!({"type": "auth", "access_token": "nope"}))
        .await;
    assert_eq!(
        bad.recv().await,
        json!({"type": "auth_invalid", "message": "Invalid access token or password"})
    );
    assert!(bad.closed().await);

    let mut malformed = Client::connect(&s.url).await;
    malformed.recv().await;
    malformed
        .send(json!({"type": "auth", "access_token": access, "x": 1}))
        .await;
    let reply = malformed.recv().await;
    assert_eq!(reply["type"], "auth_invalid");
    assert!(
        reply["message"]
            .as_str()
            .unwrap()
            .starts_with("Auth message incorrectly formatted: ")
    );
    assert!(malformed.closed().await);

    let mut garbage = Client::connect(&s.url).await;
    garbage.recv().await;
    garbage.ws.send(Message::Text("{".into())).await.unwrap();
    assert!(garbage.closed().await);

    Client::authed(&s, &access).await;
}

#[tokio::test]
async fn message_ids_and_errors() {
    let s = server().await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;

    let err = |id: Value, code: &str, msg: &str| json!({"id": id, "type": "result", "success": false, "error": {"code": code, "message": msg}});
    assert_eq!(
        c.call(json!({"id": 0, "type": "ping"})).await,
        err(json!(0), "invalid_format", "Message incorrectly formatted.")
    );
    assert_eq!(
        c.call(json!({"id": 1})).await,
        err(json!(1), "invalid_format", "Message incorrectly formatted.")
    );
    assert_eq!(
        c.call(json!({"id": 5, "type": "recorder/info"})).await,
        err(json!(5), "unknown_command", "Unknown command.")
    );
    // An unknown command doesn't use up its id.
    assert_eq!(
        c.call(json!({"id": 5, "type": "ping"})).await,
        json!({"id": 5, "type": "pong"})
    );
    assert_eq!(
        c.call(json!({"id": 5, "type": "ping"})).await,
        err(json!(5), "id_reuse", "Identifier values have to increase.")
    );
    assert_eq!(
        c.call(json!({"id": 6, "type": "get_states", "x": 1})).await,
        err(
            json!(6),
            "invalid_format",
            "extra keys not allowed. Got {'id': 6, 'type': 'get_states', 'x': 1}"
        )
    );
    // A failed command still uses its id.
    assert_eq!(
        c.call(json!({"id": 6, "type": "ping"})).await["error"]["code"],
        "id_reuse"
    );
    // Invalid JSON in the command phase disconnects without a reply.
    c.ws.send(Message::Text("nope".into())).await.unwrap();
    assert!(c.closed().await);
}

#[tokio::test]
async fn coalescing_only_after_supported_features() {
    let s = server().await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;

    // Before: every message is its own frame, even when sent together.
    c.ws.send(Message::Text(
        json!([{"id": 2, "type": "ping"}, {"id": 3, "type": "ping"}])
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    assert_eq!(c.recv().await["id"], 2);
    assert_eq!(c.recv().await["id"], 3);
    assert_eq!(c.frames, [false; 4]); // auth_required, auth_ok, two pongs

    let r = c
        .call(json!({"id": 4, "type": "supported_features", "features": {"coalesce_messages": 1}}))
        .await;
    assert_eq!(
        r,
        json!({"id": 4, "type": "result", "success": true, "result": null})
    );
    c.frames.clear();
    let batch: Vec<Value> = (5..15)
        .map(|id| json!({"id": id, "type": "ping"}))
        .collect();
    c.ws.send(Message::Text(Value::Array(batch).to_string().into()))
        .await
        .unwrap();
    for id in 5..15 {
        assert_eq!(c.recv().await, json!({"id": id, "type": "pong"}));
    }
    assert!(
        c.frames.contains(&true),
        "replies should be coalesced: {:?}",
        c.frames
    );
}

#[tokio::test]
async fn startup_commands_have_has_shapes() {
    let s = server().await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;

    let config = c.call(json!({"id": 2, "type": "get_config"})).await["result"].clone();
    assert_eq!(config["state"], "RUNNING");
    assert_eq!(config["version"], "2026.6.1");
    assert_eq!(config["config_dir"], "/config");
    assert_eq!(config["unit_system"]["temperature"], "°C");
    assert_eq!(config["time_zone"], "UTC");
    assert!(
        config["components"]
            .as_array()
            .unwrap()
            .contains(&json!("websocket_api"))
    );

    let panels = c.call(json!({"id": 3, "type": "get_panels"})).await["result"].clone();
    for p in ["home", "notfound", "profile", "lovelace"] {
        assert_eq!(panels[p]["url_path"], p, "{p}");
    }
    assert_eq!(panels["lovelace"]["config"], Value::Null);
    assert_eq!(panels["home"]["show_in_sidebar"], false);
    // Settings, as `config` registers it: for admins.
    assert_eq!(
        panels["config"],
        json!({
            "component_name": "config", "icon": "mdi:cog", "title": "config",
            "default_visible": true, "config": null, "url_path": "config",
            "require_admin": true, "config_panel_domain": null,
            "show_in_sidebar": true,
        })
    );
    // Nothing records here, so neither the components nor their panels.
    for p in ["recorder", "history", "logbook"] {
        assert!(
            !config["components"].as_array().unwrap().contains(&json!(p)),
            "{p}"
        );
        assert_eq!(panels[p], Value::Null, "{p}");
    }

    let me = c.call(json!({"id": 4, "type": "auth/current_user"})).await["result"].clone();
    assert_eq!(me["name"], "alex");
    assert_eq!(
        (me["is_owner"].clone(), me["is_admin"].clone()),
        (json!(true), json!(true))
    );
    assert_eq!(
        me["credentials"],
        json!([{"auth_provider_type": "homeassistant", "auth_provider_id": null}])
    );

    s.hass
        .services()
        .register("light", "turn_on", SupportsResponse::None, |_| async {
            Ok(None)
        });
    let services = c.call(json!({"id": 5, "type": "get_services"})).await["result"].clone();
    assert_eq!(services, json!({"light": {"turn_on": {"fields": {}}}}));

    s.hass.set_state(
        EntityId::parse("sun.sun").unwrap(),
        "above_horizon",
        attrs(json!({"elevation": 41})),
    );
    let states = c.call(json!({"id": 6, "type": "get_states"})).await["result"].clone();
    assert_eq!(states[0]["entity_id"], "sun.sun");
    assert_eq!(states[0]["attributes"]["elevation"], 41);
}

#[tokio::test]
async fn subscribe_entities_streams_compressed_diffs() {
    let s = server().await;
    let light = EntityId::parse("light.kitchen").unwrap();
    s.hass
        .set_state(light.clone(), "off", attrs(json!({"brightness": 0})));
    let mut c = Client::authed(&s, &token(&s.auth).0).await;

    assert_eq!(
        c.call(json!({"id": 3, "type": "subscribe_entities"})).await["result"],
        Value::Null
    );
    let snapshot = c.recv().await;
    assert_eq!(snapshot["id"], 3);
    assert_eq!(snapshot["event"]["a"]["light.kitchen"]["s"], "off");

    s.hass
        .set_state(light.clone(), "on", attrs(json!({"brightness": 200})));
    let change = c.recv().await;
    assert_eq!(change["type"], "event");
    let plus = &change["event"]["c"]["light.kitchen"]["+"];
    assert_eq!(plus["s"], "on");
    assert_eq!(plus["a"], json!({"brightness": 200}));
    assert!(plus["lc"].is_f64());

    s.hass.set_state(
        EntityId::parse("sensor.new").unwrap(),
        "1",
        Attributes::new(),
    );
    assert_eq!(c.recv().await["event"]["a"]["sensor.new"]["s"], "1");

    // After unsubscribing, nothing more arrives for that id.
    let r = c
        .call(json!({"id": 4, "type": "unsubscribe_events", "subscription": 3}))
        .await;
    assert_eq!(r["success"], true);
    s.hass.set_state(light, "off", Attributes::new());
    assert_eq!(
        c.call(json!({"id": 5, "type": "ping"})).await,
        json!({"id": 5, "type": "pong"})
    );
    let r = c
        .call(json!({"id": 6, "type": "unsubscribe_events", "subscription": 3}))
        .await;
    assert_eq!(
        r["error"],
        json!({"code": "not_found", "message": "Subscription not found."})
    );
}

#[tokio::test]
async fn subscribe_events_forwards_has_event_shape() {
    let s = server().await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    assert_eq!(
        c.call(json!({"id": 2, "type": "subscribe_events", "event_type": "state_changed"}))
            .await["success"],
        true
    );
    assert_eq!(
        c.call(json!({"id": 3, "type": "subscribe_events", "event_type": "core_config_updated"}))
            .await["success"],
        true
    );

    s.hass.set_state(
        EntityId::parse("switch.a").unwrap(),
        "on",
        Attributes::new(),
    );
    let e = c.recv().await;
    assert_eq!(
        (e["id"].clone(), e["type"].clone()),
        (json!(2), json!("event"))
    );
    let event = &e["event"];
    assert_eq!(event["event_type"], "state_changed");
    assert_eq!(event["origin"], "LOCAL");
    assert_eq!(event["data"]["entity_id"], "switch.a");
    assert_eq!(event["data"]["old_state"], Value::Null);
    assert_eq!(event["data"]["new_state"]["state"], "on");
    assert!(event["context"]["id"].is_string());
    // HA's `isoformat()`: UTC as `+00:00`, at most microseconds.
    let ha_time = |v: &Value| {
        let s = v.as_str().unwrap_or_default();
        let (main, frac) = s
            .strip_suffix("+00:00")
            .map_or((s, ""), |m| m.split_at(19.min(m.len())));
        s.ends_with("+00:00")
            && main.len() == 19
            && (frac.is_empty() || (frac.len() == 7 && frac.starts_with('.')))
    };
    assert!(ha_time(&event["time_fired"]), "{}", event["time_fired"]);
    let new = &event["data"]["new_state"];
    for k in ["last_changed", "last_reported", "last_updated"] {
        assert!(ha_time(&new[k]), "{k}: {}", new[k]);
    }
}

#[tokio::test]
async fn call_service_runs_with_the_users_context() {
    let s = server().await;
    s.hass.services().register(
        "test",
        "echo",
        SupportsResponse::Optional,
        |call| async move {
            let mut r = ServiceResponse::new();
            r.insert("user".into(), json!(call.context.user_id));
            Ok(Some(r))
        },
    );
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    let me = c.call(json!({"id": 2, "type": "auth/current_user"})).await["result"]["id"].clone();

    let r = c.call(json!({"id": 3, "type": "call_service", "domain": "test", "service": "echo", "return_response": true})).await;
    assert_eq!(r["success"], true, "{r}");
    assert_eq!(r["result"]["context"]["user_id"], me);
    assert_eq!(r["result"]["response"], json!({"user": me}));

    let r = c
        .call(json!({"id": 4, "type": "call_service", "domain": "test", "service": "nope"}))
        .await;
    assert_eq!(r["error"]["code"], "not_found");
    assert_eq!(r["error"]["message"], "Service test.nope not found.");
    assert_eq!(r["error"]["translation_key"], "service_not_found");

    let r = c
        .call(json!({"id": 5, "type": "call_service", "domain": "test"}))
        .await;
    assert_eq!(r["error"]["code"], "invalid_format");
}

/// HA's `ServiceValidationError`, translated `HomeAssistantError` and
/// other exceptions, as `call_service` reports them.
#[tokio::test]
async fn call_service_error_shapes() {
    let s = server().await;
    for (name, err) in [
        (
            "validation",
            ha_core::ServiceError::not_supported("climate", "set_preset_mode", "climate.ac"),
        ),
        (
            "translated",
            ha_core::ServiceError::translated(
                "switchbot_cloud",
                "device_offline",
                None,
                "SwitchBot Cloud device is offline".into(),
            ),
        ),
        ("unknown", ha_core::ServiceError::Unknown(String::new())),
        (
            "untranslated",
            ha_core::ServiceError::validation(
                "",
                "",
                None,
                "Failed to load configuration: no".into(),
            ),
        ),
    ] {
        s.hass
            .services()
            .register("test", name, SupportsResponse::None, move |_| {
                let err = err.clone();
                async move { Err(err) }
            });
    }
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    let call = |id: u64, service: &str| json!({"id": id, "type": "call_service", "domain": "test", "service": service});
    let r = c.call(call(2, "validation")).await;
    assert_eq!(
        r["error"],
        json!({"code": "service_validation_error",
            "message": "Validation error: Entity climate.ac does not support action climate.set_preset_mode",
            "translation_key": "service_not_supported", "translation_domain": "homeassistant",
            "translation_placeholders": {"domain": "climate", "service": "set_preset_mode", "entity_id": "climate.ac"}})
    );
    let r = c.call(call(3, "translated")).await;
    assert_eq!(
        r["error"],
        json!({"code": "home_assistant_error", "message": "SwitchBot Cloud device is offline",
            "translation_key": "device_offline", "translation_domain": "switchbot_cloud",
            "translation_placeholders": null})
    );
    let r = c.call(call(4, "unknown")).await;
    assert_eq!(r["error"], json!({"code": "unknown_error", "message": ""}));
    // `error_message`: with no translation key, only the code and message.
    let r = c.call(call(5, "untranslated")).await;
    assert_eq!(
        r["error"],
        json!({"code": "service_validation_error",
            "message": "Validation error: Failed to load configuration: no"})
    );
}

#[tokio::test]
async fn read_only_users_are_limited() {
    let s = server().await;
    s.auth.add_user("reader", "pw").unwrap();
    let flow = s.auth.start_login_flow("http://127.0.0.1:8123/", None);
    let code = s
        .auth
        .finish_login_flow(
            &flow,
            "http://127.0.0.1:8123/",
            &s.auth.check_password("reader", "pw").unwrap(),
            None,
        )
        .unwrap();
    let t = s
        .auth
        .exchange_code(Some("http://127.0.0.1:8123/"), Some(&code), None)
        .unwrap();
    let id = s
        .auth
        .validate_access_token(&t.access_token, None)
        .unwrap()
        .user_id;
    s.auth.set_user_groups(&id, &[ha_auth::GROUP_ID_READ_ONLY]);

    let mut c = Client::authed(&s, &t.access_token).await;
    let unauthorized = json!({"code": "unauthorized", "message": "Unauthorized"});
    let r = c
        .call(json!({"id": 2, "type": "call_service", "domain": "light", "service": "turn_on"}))
        .await;
    assert_eq!(r["error"], unauthorized);
    let r = c
        .call(json!({"id": 3, "type": "subscribe_events", "event_type": "call_service"}))
        .await;
    assert_eq!(r["error"], unauthorized);
    let r = c
        .call(json!({"id": 4, "type": "subscribe_events", "event_type": "state_changed"}))
        .await;
    assert_eq!(r["success"], true);
    assert_eq!(
        c.call(json!({"id": 5, "type": "get_states"})).await["success"],
        true
    );
    // Labs is admin-only.
    let r = c.call(json!({"id": 50, "type": "labs/list"})).await;
    assert_eq!(r["error"], unauthorized);
}

#[tokio::test]
async fn revoking_the_token_closes_the_connection() {
    let s = server().await;
    let (access, refresh) = token(&s.auth);
    let mut c = Client::authed(&s, &access).await;
    assert_eq!(
        c.call(json!({"id": 2, "type": "ping"})).await["type"],
        "pong"
    );
    s.auth.revoke(&refresh);
    assert!(c.closed().await);
}

#[tokio::test]
async fn rest_config_matches_get_config() {
    let s = server().await;
    let (access, _) = token(&s.auth);
    let http = s
        .url
        .replace("ws://", "http://")
        .replace("/api/websocket", "/api/config");
    let body = reqwest_get(&http, &access).await;
    let mut c = Client::authed(&s, &access).await;
    let ws = c.call(json!({"id": 2, "type": "get_config"})).await["result"].clone();
    assert_eq!(body, ws);
}

/// A minimal HTTP/1.1 GET, to avoid an HTTP client dependency.
async fn reqwest_get(url: &str, token: &str) -> Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let rest = url.strip_prefix("http://").unwrap();
    let (host, path) = rest.split_once('/').unwrap();
    let mut stream = tokio::net::TcpStream::connect(host).await.unwrap();
    let req = format!(
        "GET /{path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).await.unwrap();
    assert!(buf.starts_with("HTTP/1.1 200"), "{buf}");
    serde_json::from_str(buf.split_once("\r\n\r\n").unwrap().1).unwrap()
}

/// A connection handed to [`Client::kept`] outlives any number of
/// heartbeats without the test touching it, where a plain [`Client`] left
/// alone that long is dropped (the test below): what the replays of HA's
/// conversations need, which open several sockets and use each only now
/// and then.
#[tokio::test]
async fn a_kept_client_answers_heartbeats_while_the_test_is_elsewhere() {
    let s = server().await;
    let (token, _) = token(&s.auth);
    let mut kept = Client::authed(&s, &token).await.kept();
    tokio::time::sleep(super::HEARTBEAT * 4).await;
    kept.send(json!({"id": 1, "type": "ping"}), "after four heartbeats");
    assert_eq!(
        kept.recv("after four heartbeats").await,
        json!({"id": 1, "type": "pong"})
    );
}

/// A client that stops answering (no pong for a whole heartbeat) is dropped;
/// one that answers stays.
/// Real time: a paused clock jumps ahead while pongs are still in flight.
/// The heartbeat is 200 ms in test builds.
#[tokio::test]
async fn silent_clients_are_dropped_after_a_missed_heartbeat() {
    let s = server().await;
    let (access, _) = token(&s.auth);
    let mut alive = Client::authed(&s, &access).await;
    let mut silent = Client::authed(&s, &access).await;

    // `alive` keeps reading, so tungstenite answers each ping.
    let keep_reading = tokio::spawn(async move {
        while let Some(Ok(_)) = alive.ws.next().await {}
        alive
    });
    tokio::time::sleep(super::HEARTBEAT * 2 + Duration::from_secs(1)).await;
    assert!(silent.closed().await, "the silent client should be closed");
    assert!(
        !keep_reading.is_finished(),
        "the responsive client should stay"
    );
    keep_reading.abort();
}

/// A connection without a socket, to look at its internals.
fn bare_conn(hass: &Hass) -> (super::Conn, tokio::sync::mpsc::Receiver<super::Frame>) {
    let auth = crate::test_util::auth(true);
    let (access, _) = token(&auth);
    let principal = auth.validate_access_token(&access, None).unwrap();
    let user = auth.user(&principal.user_id).unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(super::MAX_PENDING);
    let conn = super::Conn {
        shared: crate::Shared {
            hass: hass.clone(),
            auth,
            config: Default::default(),
            templates: Default::default(),
            core_config: Default::default(),
        },
        principal,
        user,
        out: super::Outbox {
            tx,
            overflow: Default::default(),
        },
        coalesce: Default::default(),
        last_id: 0,
        subs: Default::default(),
        events: Default::default(),
        event_listener: None,
        busy: None,
        queue: Default::default(),
        queued_bytes: 0,
    };
    (conn, rx)
}

fn handle(conn: &mut super::Conn, msg: Value) {
    conn.handle(msg);
}

/// Wait for the command the connection is busy with, as its read loop does.
async fn settle(conn: &mut super::Conn) {
    let mut busy = conn.busy.take();
    if busy.is_some() {
        let done = super::template::finished(&mut busy).await;
        conn.done(done);
        conn.drain();
    }
}

fn sent(rx: &mut tokio::sync::mpsc::Receiver<super::Frame>) -> Vec<Value> {
    let mut sent = Vec::new();
    while let Ok(super::Frame::Json(m)) = rx.try_recv() {
        sent.push(serde_json::from_str::<Value>(&m).unwrap());
    }
    sent
}

#[tokio::test]
async fn the_bus_listener_runs_only_while_events_are_subscribed() {
    let hass = Hass::new();
    let (mut conn, _rx) = bare_conn(&hass);
    handle(
        &mut conn,
        json!({"id": 1, "type": "subscribe_events", "event_type": "a"}),
    );
    handle(
        &mut conn,
        json!({"id": 2, "type": "subscribe_events", "event_type": "b"}),
    );
    assert!(conn.event_listener.is_some());
    handle(
        &mut conn,
        json!({"id": 3, "type": "unsubscribe_events", "subscription": 1}),
    );
    assert!(conn.event_listener.is_some());
    assert!(
        !conn.events.lock().contains_key("a"),
        "empty types are pruned"
    );
    handle(
        &mut conn,
        json!({"id": 4, "type": "unsubscribe_events", "subscription": 2}),
    );
    assert!(conn.event_listener.is_none());
    assert!(conn.events.lock().is_empty());
}

#[tokio::test]
async fn the_entity_snapshot_is_not_kept() {
    let hass = Hass::new();
    let id = EntityId::parse("light.a").unwrap();
    hass.set_state(id.clone(), "on", Attributes::new());
    let (mut conn, _rx) = bare_conn(&hass);
    handle(&mut conn, json!({"id": 1, "type": "subscribe_entities"}));
    tokio::task::yield_now().await;
    // The state machine's reference and ours; none held by the subscription.
    assert_eq!(
        std::sync::Arc::strong_count(&hass.states().get(&id).unwrap()),
        2
    );
}

#[tokio::test]
async fn a_runaway_entity_feed_disconnects() {
    let hass = Hass::new();
    let (mut conn, _rx) = bare_conn(&hass);
    handle(&mut conn, json!({"id": 1, "type": "subscribe_entities"}));
    // More changes than the outbox holds, before the task gets to run.
    for i in 0..=super::MAX_PENDING + 1 {
        hass.set_state(
            EntityId::parse("sensor.x").unwrap(),
            i.to_string().as_str(),
            Attributes::new(),
        );
    }
    let notified = conn.out.overflow.clone();
    tokio::time::timeout(Duration::from_secs(2), notified.notified())
        .await
        .expect("the subscription should report overflow");
}

/// A client that fell 4096 messages behind still gets a Close frame when it
/// reads again, promptly, even though the outbox was full.
#[tokio::test]
async fn overflowed_clients_get_a_close_frame() {
    let s = server().await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    assert_eq!(
        c.call(json!({"id": 2, "type": "subscribe_events", "event_type": "state_changed"}))
            .await["success"],
        true
    );
    // Stop reading while far more events than the outbox holds are fired.
    let big = "x".repeat(2048);
    for i in 0..(super::MAX_PENDING * 3) {
        s.hass.set_state(
            EntityId::parse("sensor.flood").unwrap(),
            i.to_string().as_str(),
            attrs(json!({"pad": big})),
        );
    }
    let start = std::time::Instant::now();
    let mut saw_close = false;
    while let Ok(Some(frame)) = tokio::time::timeout(Duration::from_secs(5), c.ws.next()).await {
        match frame {
            Ok(Message::Close(_)) => {
                saw_close = true;
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(saw_close, "expected a Close frame");
    assert!(start.elapsed() < Duration::from_secs(5));
}

/// The frontend's cold load on "/" (20260527.4, admin user), in its order.
/// The splash screen goes away only if `subscribe_entities`, `get_config`
/// and `get_services` succeed, `recorder/info` succeeds or is
/// `unknown_command`, and `frontend/subscribe_{user,system}_data` send their
/// event; the panel needs `home` in `get_panels`.
#[tokio::test]
async fn the_frontends_cold_load_gets_every_answer_it_waits_for() {
    let s = server().await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    let commands: Vec<Value> = vec![
        json!({"type": "supported_features", "features": {"coalesce_messages": 1}}),
        json!({"type": "subscribe_entities"}),
        json!({"type": "subscribe_events", "event_type": "entity_registry_updated"}),
        json!({"type": "config/entity_registry/list_for_display"}),
        json!({"type": "subscribe_events", "event_type": "device_registry_updated"}),
        json!({"type": "config/device_registry/list"}),
        json!({"type": "subscribe_events", "event_type": "area_registry_updated"}),
        json!({"type": "config/area_registry/list"}),
        json!({"type": "subscribe_events", "event_type": "floor_registry_updated"}),
        json!({"type": "config/floor_registry/list"}),
        json!({"type": "subscribe_events", "event_type": "component_loaded"}),
        json!({"type": "subscribe_events", "event_type": "core_config_updated"}),
        json!({"type": "get_config"}),
        json!({"type": "subscribe_events", "event_type": "service_registered"}),
        json!({"type": "subscribe_events", "event_type": "service_removed"}),
        json!({"type": "get_services"}),
        json!({"type": "subscribe_events", "event_type": "panels_updated"}),
        json!({"type": "get_panels"}),
        json!({"type": "subscribe_events", "event_type": "themes_updated"}),
        json!({"type": "frontend/get_themes"}),
        json!({"type": "auth/current_user"}),
        json!({"type": "frontend/subscribe_user_data", "key": "core"}),
        json!({"type": "subscribe_events", "event_type": "repairs_issue_registry_updated"}),
        json!({"type": "repairs/list_issues"}),
        json!({"type": "recorder/info"}),
        json!({"type": "lovelace/config", "url_path": null, "force": false}),
        json!({"type": "lovelace/resources"}),
        json!({"type": "frontend/subscribe_user_data", "key": "core"}),
        json!({"type": "frontend/subscribe_system_data", "key": "core"}),
        json!({"type": "frontend/subscribe_user_data", "key": "language"}),
        json!({"type": "frontend/subscribe_user_data", "key": "theme"}),
        json!({"type": "frontend/get_translations", "language": "en", "category": "entity_component"}),
        json!({"type": "frontend/get_translations", "language": "en", "category": "entity"}),
        json!({"type": "frontend/subscribe_user_data", "key": "sidebar"}),
        json!({"type": "persistent_notification/subscribe"}),
        json!({"type": "frontend/get_system_data", "key": "home"}),
        json!({"type": "lovelace/info"}),
        json!({"type": "frontend/get_icons", "category": "entity_component"}),
        json!({"type": "ping"}),
    ];
    // The frontend's ids: 1, then 3 onwards.
    let ids: Vec<u64> = std::iter::once(1).chain(3..).take(commands.len()).collect();
    let batch: Vec<Value> = commands
        .iter()
        .zip(&ids)
        .map(|(cmd, id)| {
            let mut m = cmd.clone();
            m["id"] = json!(id);
            m
        })
        .collect();
    c.ws.send(Message::Text(Value::Array(batch).to_string().into()))
        .await
        .unwrap();

    // Every message, by id, in arrival order.
    let mut by_id: std::collections::HashMap<u64, Vec<Value>> = Default::default();
    let mut answered = 0;
    while answered < ids.len() {
        let m = c.recv().await;
        let id = m["id"].as_u64().unwrap();
        if m["type"] == "result" || m["type"] == "pong" {
            answered += 1;
        }
        by_id.entry(id).or_default().push(m);
    }
    let reply = |i: usize| -> &Value {
        by_id[&ids[i]]
            .iter()
            .find(|m| m["type"] == "result" || m["type"] == "pong")
            .unwrap()
    };
    let events = |i: usize| -> Vec<&Value> {
        by_id[&ids[i]]
            .iter()
            .filter(|m| m["type"] == "event")
            .collect()
    };

    for (i, cmd) in commands.iter().enumerate() {
        let r = reply(i);
        match cmd["type"].as_str().unwrap() {
            "recorder/info" => assert_eq!(r["error"]["code"], "unknown_command"),
            "lovelace/config" => assert_eq!(r["error"]["code"], "config_not_found"),
            "ping" => assert_eq!(r["type"], "pong"),
            t => assert_eq!(r["success"], true, "{t}: {r}"),
        }
    }
    assert_eq!(reply(12)["result"]["state"], "RUNNING");
    assert!(reply(17)["result"].get("home").is_some());
    assert_eq!(events(1)[0]["event"], json!({"a": {}}));
    // User and system data: the event comes before the result.
    for i in [21, 27, 28, 29, 30, 33] {
        let msgs = &by_id[&ids[i]];
        assert_eq!(msgs[0]["type"], "event", "{:?}", commands[i]);
        assert_eq!(msgs[0]["event"], json!({"value": null}));
        assert_eq!(msgs[1]["type"], "result");
    }
    // Notifications: result first, then the (empty) current set.
    let msgs = &by_id[&ids[34]];
    assert_eq!(msgs[0]["type"], "result");
    assert_eq!(
        msgs[1]["event"],
        json!({"type": "current", "notifications": {}})
    );
    assert_eq!(reply(35)["result"], json!({"value": null}));
}

#[tokio::test]
async fn user_data_is_shared_between_a_users_connections() {
    let s = server().await;
    let (access, _) = token(&s.auth);
    let mut tab1 = Client::authed(&s, &access).await;
    let mut tab2 = Client::authed(&s, &access).await;
    let sub = json!({"id": 2, "type": "frontend/subscribe_user_data", "key": "sidebar"});
    tab2.send(sub).await;
    assert_eq!(tab2.recv().await["event"], json!({"value": null}));
    tab2.recv().await;

    let set = json!({"id": 2, "type": "frontend/set_user_data", "key": "sidebar", "value": {"panelOrder": ["home"]}});
    assert_eq!(tab1.call(set).await["success"], true);
    assert_eq!(
        tab2.recv().await["event"],
        json!({"value": {"panelOrder": ["home"]}})
    );
    let get = json!({"id": 3, "type": "frontend/get_user_data"});
    assert_eq!(
        tab1.call(get).await["result"],
        json!({"value": {"sidebar": {"panelOrder": ["home"]}}})
    );
    // Another key doesn't notify a keyed subscription.
    let set = json!({"id": 4, "type": "frontend/set_user_data", "key": "theme", "value": "dark"});
    tab1.call(set).await;
    assert_eq!(
        tab2.call(json!({"id": 3, "type": "ping"})).await["type"],
        "pong"
    );
}

#[tokio::test]
async fn registry_lists_come_from_the_registries() {
    let hass = Hass::new();
    hass.start();
    let auth = crate::test_util::auth(true);
    let registries = ha_registry::Registries {
        areas: vec![ha_registry::Area {
            id: "kitchen".into(),
            name: "Kitchen".into(),
            ..Default::default()
        }],
        entities: ha_registry::rows![ha_registry::Entity {
            entity_id: "light.kitchen".into(),
            platform: "hue".into(),
            area_id: Some("kitchen".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let config = crate::ApiConfig {
        registry: Some(ha_registry::Registry::in_memory(&hass, registries)),
        ..Default::default()
    };
    let app = crate::router(hass.clone(), auth.clone(), &config)
        .into_make_service_with_connect_info::<std::net::SocketAddr>();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/api/websocket", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let s = Server { url, hass, auth };
    let mut c = Client::authed(&s, &token(&s.auth).0).await;

    let areas = c
        .call(json!({"id": 2, "type": "config/area_registry/list"}))
        .await;
    assert_eq!(areas["result"][0]["area_id"], "kitchen");
    let entities = c
        .call(json!({"id": 3, "type": "config/entity_registry/list_for_display"}))
        .await;
    assert_eq!(
        entities["result"]["entities"],
        json!([{"ei": "light.kitchen", "pl": "hue", "ai": "kitchen", "lb": []}])
    );
    for (id, ty) in [
        (4, "config/device_registry/list"),
        (5, "config/floor_registry/list"),
        (6, "config/label_registry/list"),
    ] {
        assert_eq!(
            c.call(json!({"id": id, "type": ty})).await["result"],
            json!([]),
            "{ty}"
        );
    }
}

#[tokio::test]
async fn dashboards_are_served_saved_and_announced() {
    let dir = std::env::temp_dir().join(format!("hars-ws-lovelace-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".storage")).unwrap();
    let store = |key: &str, data: Value| {
        let v = json!({"version": 1, "minor_version": 1, "key": key, "data": data});
        std::fs::write(dir.join(".storage").join(key), v.to_string()).unwrap();
    };
    store(
        "lovelace_dashboards",
        json!({"items": [
            {"id": "lovelace", "title": "Overview", "url_path": "lovelace", "require_admin": false, "show_in_sidebar": true, "mode": "storage"}
        ]}),
    );
    store(
        "lovelace.lovelace",
        json!({"config": {"views": [{"title": "Home"}]}}),
    );

    let hass = Hass::new();
    hass.start();
    let auth = crate::test_util::auth(true);
    let config = crate::ApiConfig {
        lovelace: std::sync::Arc::new(crate::Lovelace::load(&dir).unwrap()),
        ..Default::default()
    };
    let app = crate::router(hass.clone(), auth.clone(), &config)
        .into_make_service_with_connect_info::<std::net::SocketAddr>();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/api/websocket", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let s = Server { url, hass, auth };
    let mut c = Client::authed(&s, &token(&s.auth).0).await;

    // The dashboard at `lovelace` replaces the default panel, with a config,
    // so the frontend shows it instead of redirecting to /home.
    let panels = c.call(json!({"id": 2, "type": "get_panels"})).await["result"].clone();
    assert_eq!(panels["lovelace"]["config"], json!({"mode": "storage"}));
    assert_eq!(panels["lovelace"]["title"], "Overview");
    let cfg = c
        .call(json!({"id": 3, "type": "lovelace/config", "url_path": null, "force": false}))
        .await;
    assert_eq!(cfg["result"], json!({"views": [{"title": "Home"}]}));
    let cfg = c
        .call(json!({"id": 4, "type": "lovelace/config", "url_path": "nope"}))
        .await;
    assert_eq!(
        cfg["error"],
        json!({"code": "config_not_found", "message": "Unknown config specified: nope"})
    );

    let sub = c
        .call(json!({"id": 5, "type": "subscribe_events", "event_type": "lovelace_updated"}))
        .await;
    assert_eq!(sub["success"], true);
    let new = json!({"views": [{"title": "Kitchen"}]});
    let first = c
        .call(
            json!({"id": 6, "type": "lovelace/config/save", "url_path": "lovelace", "config": new}),
        )
        .await;
    let second = c.recv().await;
    let (event, result) = if first["type"] == "event" {
        (first, second)
    } else {
        (second, first)
    };
    assert_eq!(result["success"], true, "{result}");
    assert_eq!(event["event"]["event_type"], "lovelace_updated");
    assert_eq!(event["event"]["data"], json!({"url_path": "lovelace"}));
    let cfg = c
        .call(json!({"id": 7, "type": "lovelace/config", "url_path": "lovelace"}))
        .await;
    assert_eq!(cfg["result"], new);
    let raw: Value =
        serde_json::from_slice(&std::fs::read(dir.join(".storage/lovelace.lovelace")).unwrap())
            .unwrap();
    assert_eq!(raw["data"]["config"], new);

    // Saving is admin-only.
    s.auth.add_user("reader", "pw").unwrap();
    let flow = s.auth.start_login_flow("http://127.0.0.1:8123/", None);
    let code = s
        .auth
        .finish_login_flow(
            &flow,
            "http://127.0.0.1:8123/",
            &s.auth.check_password("reader", "pw").unwrap(),
            None,
        )
        .unwrap();
    let t = s
        .auth
        .exchange_code(Some("http://127.0.0.1:8123/"), Some(&code), None)
        .unwrap();
    let uid = s
        .auth
        .validate_access_token(&t.access_token, None)
        .unwrap()
        .user_id;
    s.auth.set_user_groups(&uid, &[ha_auth::GROUP_ID_USER]);
    let mut user = Client::authed(&s, &t.access_token).await;
    let r = user
        .call(json!({"id": 2, "type": "lovelace/config/save", "config": {}}))
        .await;
    assert_eq!(r["error"]["code"], "unauthorized");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn zone_and_person_lists() {
    let s = server().await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    assert_eq!(
        c.call(json!({"id": 2, "type": "zone/list"})).await["result"],
        json!([])
    );
    assert_eq!(
        c.call(json!({"id": 3, "type": "person/list"})).await["result"],
        json!({"storage": [], "config": []})
    );
}

#[tokio::test]
async fn weather_forecasts_are_served_and_pushed() {
    use ha_integrations::weather::{Forecasts, Weather};
    let hass = Hass::new();
    hass.start();
    let auth = crate::test_util::auth(true);
    let weather = std::sync::Arc::new(Weather::default());
    let registries = ha_registry::Registries {
        entities: ha_registry::rows![ha_registry::Entity {
            entity_id: "weather.forecast_home".into(),
            platform: "met".into(),
            device_id: Some("dev1".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    weather.register_services(&hass, std::sync::Arc::new(registries));
    let id = EntityId::parse("weather.forecast_home").unwrap();
    let daily = |t: f64| {
        std::sync::Arc::new(json!([{"datetime": "2026-11-14T12:00:00+00:00", "temperature": t}]))
    };
    let publish_as = |t: f64, available: bool| {
        weather.publish(
            &id,
            3,
            available,
            Forecasts {
                daily: daily(t),
                hourly: std::sync::Arc::new(json!([])),
            },
        )
    };
    let publish = |t: f64| publish_as(t, true);
    publish(10.5);
    let config = crate::ApiConfig {
        weather: weather.clone(),
        ..Default::default()
    };
    let app = crate::router(hass.clone(), auth.clone(), &config)
        .into_make_service_with_connect_info::<std::net::SocketAddr>();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/api/websocket", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let s = Server { url, hass, auth };
    let mut c = Client::authed(&s, &token(&s.auth).0).await;

    let units = c
        .call(json!({"id": 2, "type": "weather/convertible_units"}))
        .await;
    assert_eq!(
        units["result"],
        json!({"units": {"pressure_unit": ["hPa", "inHg", "mbar", "mmHg"], "temperature_unit": ["°C", "°F"], "visibility_unit": ["km", "mi"], "precipitation_unit": ["in", "mm"], "wind_speed_unit": ["Beaufort", "ft/s", "km/h", "kn", "m/s", "mph"]}})
    );

    // Errors, as HA words them.
    let sub = |id: u64, entity: &str, kind: &str| json!({"id": id, "type": "weather/subscribe_forecast", "entity_id": entity, "forecast_type": kind});
    let e = c.call(sub(3, "weather.nope", "daily")).await;
    assert_eq!(
        e["error"],
        json!({"code": "invalid_entity_id", "message": "Weather entity not found: weather.nope"})
    );
    let e = c.call(sub(4, "weather.forecast_home", "twice_daily")).await;
    assert_eq!(e["error"]["code"], "forecast_not_supported");
    assert_eq!(
        e["error"]["message"],
        "The weather entity does not support forecast type: twice_daily"
    );
    let e = c.call(sub(5, "light.x", "daily")).await;
    assert_eq!(e["error"]["code"], "invalid_format");
    assert_eq!(
        e["error"]["message"],
        "Entity ID 'light.x' does not belong to domain 'weather' for dictionary value @ data['entity_id']. Got 'light.x'"
    );
    let e = c.call(sub(6, "weather.forecast_home", "weekly")).await;
    assert_eq!(e["error"]["code"], "invalid_format");

    // The result, the current forecast, then every publish.
    let r = c.call(sub(7, "Weather.Forecast_Home", "daily")).await;
    assert_eq!(
        r,
        json!({"id": 7, "type": "result", "success": true, "result": null})
    );
    let ev = c.recv().await;
    assert_eq!(
        ev,
        json!({"id": 7, "type": "event", "event": {"type": "daily", "forecast": *daily(10.5)}})
    );
    publish(11.0);
    assert_eq!(c.recv().await["event"]["forecast"][0]["temperature"], 11.0);

    // `weather.get_forecasts`, keyed by entity.
    let r = c
        .call(json!({"id": 8, "type": "call_service", "domain": "weather", "service": "get_forecasts",
            "service_data": {"type": "daily"}, "target": {"entity_id": "weather.forecast_home"}, "return_response": true}))
        .await;
    assert_eq!(
        r["result"]["response"],
        json!({"weather.forecast_home": {"forecast": *daily(11.0)}})
    );
    let r = c
        .call(
            json!({"id": 9, "type": "call_service", "domain": "weather", "service": "get_forecasts",
            "service_data": {"type": "twice_daily", "entity_id": "all"}, "return_response": true}),
        )
        .await;
    assert_eq!(
        r["error"],
        json!({"code": "home_assistant_error", "message": "Weather entity 'weather.forecast_home' does not support 'twice_daily' forecast"})
    );
    let r = c
        .call(json!({"id": 10, "type": "call_service", "domain": "weather", "service": "get_forecasts",
            "service_data": {"type": "daily", "entity_id": "weather.other"}, "return_response": true}))
        .await;
    assert_eq!(
        r["error"]["message"],
        "Service call requested response data but did not match any entities"
    );
    // A device target reaches its weather entity.
    let by_device = |id: u64| {
        json!({"id": id, "type": "call_service", "domain": "weather", "service": "get_forecasts",
            "service_data": {"type": "daily"}, "target": {"device_id": "dev1"}, "return_response": true})
    };
    let r = c.call(by_device(20)).await;
    assert_eq!(
        r["result"]["response"]["weather.forecast_home"]["forecast"][0]["temperature"],
        11.0
    );
    // Unavailable: the service skips it; subscribers still get the forecast.
    publish_as(11.0, false);
    assert_eq!(c.recv().await["event"]["forecast"][0]["temperature"], 11.0);
    let r = c.call(by_device(21)).await;
    assert_eq!(
        r["error"]["message"],
        "Service call requested response data but did not match any entities"
    );

    // Unsubscribed: no more pushes.
    let r = c
        .call(json!({"id": 31, "type": "unsubscribe_events", "subscription": 7}))
        .await;
    assert_eq!(r["success"], true, "{r}");
    publish(12.0);
    let r = c.call(json!({"id": 32, "type": "ping"})).await;
    assert_eq!(r["type"], "pong");
}

#[tokio::test]
async fn translations_and_icons_come_from_has_files() {
    let s = server().await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    // Loaded components by default: `person` is one, `weather` isn't.
    let r = c
        .call(json!({"id": 2, "type": "frontend/get_translations", "language": "en", "category": "entity_component"}))
        .await;
    let res = &r["result"]["resources"];
    assert_eq!(
        res["component.person.entity_component._.state.home"],
        "Home"
    );
    assert!(
        res.get("component.weather.entity_component._.name")
            .is_none()
    );
    // Asked for by name (a string or a list); other languages get English.
    let r = c
        .call(json!({"id": 3, "type": "frontend/get_translations", "language": "de", "category": "entity_component", "integration": "weather"}))
        .await;
    assert_eq!(
        r["result"]["resources"]["component.weather.entity_component._.state.clear-night"],
        "Clear, night"
    );
    let r = c
        .call(json!({"id": 4, "type": "frontend/get_translations", "language": "en", "category": "config", "config_flow": true}))
        .await;
    assert_eq!(r["result"], json!({"resources": {}}));
    let r = c
        .call(json!({"id": 5, "type": "frontend/get_translations", "language": "en"}))
        .await;
    assert_eq!(
        r["error"]["message"],
        "required key not provided @ data['category']. Got None"
    );

    let r = c
        .call(json!({"id": 6, "type": "frontend/get_icons", "category": "entity_component", "integration": ["weather", "sun"]}))
        .await;
    let res = &r["result"]["resources"];
    assert_eq!(res["weather"]["_"]["state"]["sunny"], "mdi:weather-sunny");
    // `sun` has only `entity` icons.
    assert!(res.get("sun").is_none());
    let r = c
        .call(json!({"id": 7, "type": "frontend/get_icons", "category": "state"}))
        .await;
    assert_eq!(r["error"]["code"], "invalid_format");
}

#[tokio::test]
async fn labs_features_list_subscribe_and_update() {
    let s = server().await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    // Only features of loaded integrations (`frontend` isn't loaded here).
    let r = c.call(json!({"id": 2, "type": "labs/list"})).await;
    assert_eq!(r["result"], json!({"features": []}));

    let r = c
        .call(json!({"id": 3, "type": "labs/subscribe", "domain": "frontend", "preview_feature": "nope"}))
        .await;
    assert_eq!(
        r["error"],
        json!({"code": "not_found", "message": "Preview feature frontend.nope not found"})
    );
    let r = c
        .call(json!({"id": 4, "type": "labs/subscribe", "domain": "frontend", "preview_feature": "winter_mode"}))
        .await;
    assert_eq!(r["success"], true);
    let ev = c.recv().await;
    assert_eq!(ev["event"]["enabled"], false);
    assert_eq!(ev["event"]["preview_feature"], "winter_mode");

    let r = c
        .call(json!({"id": 5, "type": "labs/update", "domain": "frontend", "preview_feature": "winter_mode", "enabled": true, "create_backup": true}))
        .await;
    assert_eq!(r["error"]["code"], "unknown_error");
    let r = c
        .call(json!({"id": 6, "type": "labs/update", "domain": "frontend", "preview_feature": "winter_mode", "enabled": true}))
        .await;
    // The subscriber hears of it before the result, as in HA.
    assert_eq!(r["id"], 4);
    assert_eq!(r["event"]["enabled"], true);
    let result = c.recv().await;
    assert_eq!(
        (result["id"].clone(), result["success"].clone()),
        (json!(6), json!(true))
    );
    let r = c
        .call(json!({"id": 7, "type": "labs/update", "domain": "frontend", "preview_feature": "winter_mode"}))
        .await;
    assert_eq!(
        r["error"]["message"],
        "required key not provided @ data['enabled']. Got None"
    );
}

#[tokio::test]
async fn sensor_unit_tables_are_has() {
    let s = server().await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    let r = c
        .call(json!({"id": 2, "type": "sensor/numeric_device_classes"}))
        .await;
    let classes = r["result"]["numeric_device_classes"].as_array().unwrap();
    assert_eq!(classes.len(), 57);
    assert!(classes.contains(&json!("temperature")) && !classes.contains(&json!("enum")));
    let r = c
        .call(json!({"id": 3, "type": "sensor/device_class_convertible_units", "device_class": "temperature"}))
        .await;
    assert_eq!(r["result"], json!({"units": ["K", "°C", "°F"]}));
    let r = c
        .call(json!({"id": 4, "type": "sensor/device_class_convertible_units", "device_class": "enum"}))
        .await;
    assert_eq!(r["result"], json!({"units": []}));
    // voluptuous's errors, every one, humanized.
    let r = c
        .call(json!({"id": 5, "type": "sensor/numeric_device_classes", "extra": true}))
        .await;
    assert_eq!(
        r["error"],
        json!({"code": "invalid_format", "message": "extra keys not allowed @ data['extra']. Got True"})
    );
    let r = c
        .call(json!({"id": 6, "type": "sensor/device_class_convertible_units", "extra": true}))
        .await;
    assert_eq!(
        r["error"]["message"],
        "extra keys not allowed @ data['extra']. Got True\nrequired key not provided @ data['device_class']. Got None"
    );
}

#[tokio::test]
async fn brands_access_token() {
    let dir = std::env::temp_dir().join(format!("hars-ws-brands-{}", std::process::id()));
    let brands = crate::brands::Brands::new(&dir, "http://127.0.0.1:9").unwrap();
    let s = server_with(crate::ApiConfig {
        brands: Some(brands.clone()),
        ..Default::default()
    })
    .await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    let r = c
        .call(json!({"id": 2, "type": "brands/access_token"}))
        .await;
    assert_eq!(r["result"], json!({"token": brands.token()}));
    let r = c.call(json!({"id": 3, "type": "get_config"})).await;
    assert!(
        r["result"]["components"]
            .as_array()
            .unwrap()
            .contains(&json!("brands"))
    );
    // Without brands (no cache dir), the command doesn't exist.
    let s = server().await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    let r = c
        .call(json!({"id": 2, "type": "brands/access_token"}))
        .await;
    assert_eq!(r["error"]["code"], "unknown_command");
}

#[tokio::test]
async fn manifests_as_ha() {
    let s = server().await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    let r = c
        .call(json!({"id": 2, "type": "manifest/list", "integrations": ["hue", "nope", "hue", "sun"]}))
        .await;
    let list = r["result"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["name"], "Philips Hue");
    assert_eq!(list[1]["domain"], "sun");
    // Every loaded integration with a manifest.
    let r = c.call(json!({"id": 3, "type": "manifest/list"})).await;
    let names: Vec<&str> = r["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["domain"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"websocket_api") && names.contains(&"zone"),
        "{names:?}"
    );
    let r = c
        .call(json!({"id": 4, "type": "manifest/get", "integration": "lifx"}))
        .await;
    assert_eq!(r["result"]["iot_class"], "local_polling");
    let r = c
        .call(json!({"id": 5, "type": "manifest/get", "integration": "nope"}))
        .await;
    assert_eq!(
        r["error"],
        json!({"code": "not_found", "message": "Integration not found"})
    );
    // Loaded ones first (`zone` is), then the rest, each as asked.
    let r = c
        .call(json!({"id": 7, "type": "manifest/list", "integrations": ["hue", "zone"]}))
        .await;
    assert_eq!(r["result"][0]["domain"], "zone");
    // voluptuous's errors, sorted.
    let r = c
        .call(json!({"id": 8, "type": "manifest/list", "a": 1, "integrations": 3}))
        .await;
    assert_eq!(
        r["error"]["message"],
        "expected a list for dictionary value @ data['integrations']. Got 3\nextra keys not allowed @ data['a']. Got 1"
    );
    let r = c
        .call(json!({"id": 9, "type": "manifest/list", "integrations": "hue"}))
        .await;
    assert_eq!(
        r["error"]["message"],
        "expected a list for dictionary value @ data['integrations']. Got 'hue'"
    );
}

#[tokio::test]
async fn system_log_list_is_admin_only() {
    let s = server().await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    let r = c.call(json!({"id": 2, "type": "system_log/list"})).await;
    assert!(r["result"].is_array(), "{r}");
    let r = c
        .call(json!({"id": 3, "type": "system_log/list", "x": 1}))
        .await;
    assert_eq!(
        r["error"]["message"],
        "extra keys not allowed @ data['x']. Got 1"
    );
}

#[tokio::test]
async fn automation_config_is_the_raw_entry() {
    let hass = Hass::new();
    let items = ha_core::yaml::parse(
        "- id: b_first\n  alias: Z\n  triggers: [{trigger: time, at: '10:00'}]\n  actions: [{action: light.turn_on}]\n",
    )
    .unwrap();
    let ha_core::yaml::Yaml::Seq(items) = items else {
        panic!()
    };
    let autos = ha_automation::Automations::setup_yaml(&hass, &Default::default(), &items).await;
    let s = server_with(crate::ApiConfig {
        automations: Some(autos),
        ..Default::default()
    })
    .await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    let r = c
        .call(json!({"id": 2, "type": "automation/config", "entity_id": "automation.z"}))
        .await;
    assert_eq!(
        r["result"],
        json!({"config": {"id": "b_first", "alias": "Z", "triggers": [{"trigger": "time", "at": "10:00"}], "actions": [{"action": "light.turn_on"}]}})
    );
    let r = c
        .call(json!({"id": 3, "type": "automation/config", "entity_id": "automation.nope"}))
        .await;
    assert_eq!(
        r["error"],
        json!({"code": "not_found", "message": "Entity not found"})
    );
    let r = c.call(json!({"id": 4, "type": "automation/config"})).await;
    assert_eq!(
        r["error"],
        json!({"code": "unknown_error", "message": "Unknown error"})
    );
}

/// HA's raw config shows its validators' nested changes (`service` →
/// `action`, last), and what its encoder can't write is an error.
#[tokio::test]
async fn automation_config_quirks() {
    let hass = Hass::new();
    let items = ha_core::yaml::parse(
        "- alias: Legacy no id\n  trigger: {platform: time, at: '10:00'}\n  action: {service: light.turn_on, target: {entity_id: light.hallway}}\n- {id: bin, alias: Bin, value: !!binary aGk=, triggers: [], actions: []}\n",
    )
    .unwrap();
    let ha_core::yaml::Yaml::Seq(items) = items else {
        panic!()
    };
    let autos = ha_automation::Automations::setup_yaml(&hass, &Default::default(), &items).await;
    let s = server_with(crate::ApiConfig {
        automations: Some(autos),
        ..Default::default()
    })
    .await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    let r = c
        .call(json!({"id": 2, "type": "automation/config", "entity_id": "automation.legacy_no_id"}))
        .await;
    assert_eq!(
        r["result"]["config"]["action"],
        json!({"target": {"entity_id": "light.hallway"}, "action": "light.turn_on"})
    );
    let r = c
        .call(json!({"id": 3, "type": "automation/config", "entity_id": "automation.bin"}))
        .await;
    assert_eq!(
        r["error"],
        json!({"code": "unknown_error", "message": "Invalid JSON in response"})
    );
}

/// HA's `INCLUDE_EXCLUDE_BASE_FILTER_SCHEMA` on `subscribe_entities`: the
/// iOS app asks for zones and persons only. Cases from HA 2026.6.1's
/// `ws.json` (mobile_app vectors) and its schema run on bad input.
#[tokio::test]
async fn subscribe_entities_include_exclude_filters() {
    let s = server().await;
    for id in [
        "zone.home",
        "person.alex",
        "sensor.phone_battery_level",
        "sensor.x",
        "binary_sensor.phone_kiosk_mode",
    ] {
        s.hass
            .set_state(EntityId::parse(id).unwrap(), "0", Attributes::new());
    }
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    let keys = |v: &Value| {
        let mut k: Vec<String> = v["event"]["a"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        k.sort();
        k
    };

    let r = c
        .call(json!({"id": 6, "type": "subscribe_entities",
                     "include": {"domains": ["zone", "person"]}}))
        .await;
    assert_eq!(r["success"], true);
    assert_eq!(keys(&c.recv().await), ["person.alex", "zone.home"]);

    let r = c
        .call(json!({"id": 7, "type": "subscribe_entities", "include": {"domains": "zone"}}))
        .await;
    assert_eq!(r["success"], true);
    assert_eq!(keys(&c.recv().await), ["zone.home"]);

    // Case 5: excluded by glob unless included by id.
    let r = c
        .call(json!({"id": 8, "type": "subscribe_entities",
                     "exclude": {"entity_globs": ["sensor.*"]},
                     "include": {"entities": ["sensor.phone_battery_level"]}}))
        .await;
    assert_eq!(r["success"], true);
    assert_eq!(
        keys(&c.recv().await),
        [
            "binary_sensor.phone_kiosk_mode",
            "person.alex",
            "sensor.phone_battery_level",
            "zone.home"
        ]
    );

    // `entity_ids` as a comma-separated string, lowercased, AND the filter.
    let r = c
        .call(json!({"id": 9, "type": "subscribe_entities",
                     "entity_ids": "Zone.Home, sensor.x",
                     "include": {"domains": ["sensor"]}}))
        .await;
    assert_eq!(r["success"], true);
    assert_eq!(keys(&c.recv().await), ["sensor.x"]);

    // HA's validator, not `EntityId`'s: Unicode digits and a trailing newline pass.
    let r = c
        .call(json!({"id": 10, "type": "subscribe_entities",
                     "entity_ids": ["sensor.\u{661}", "sensor.a\n"]}))
        .await;
    assert_eq!(r["success"], true);
    assert_eq!(c.recv().await["event"]["a"], json!({}));

    // Changes are filtered the same way: only 8 sees the sensor, 7 and 6 the zone.
    s.hass.set_state(
        EntityId::parse("sensor.phone_battery_level").unwrap(),
        "1",
        Attributes::new(),
    );
    let ev = c.recv().await;
    assert_eq!(ev["id"], 8);
    assert!(ev["event"]["c"]["sensor.phone_battery_level"].is_object());
    s.hass.set_state(
        EntityId::parse("zone.home").unwrap(),
        "1",
        Attributes::new(),
    );
    let mut ids = vec![
        c.recv().await["id"].clone(),
        c.recv().await["id"].clone(),
        c.recv().await["id"].clone(),
    ];
    ids.sort_by_key(|v| v.as_u64());
    assert_eq!(ids, [json!(6), json!(7), json!(8)]);

    let cases = [
        (
            json!({"entity_ids": "light.a,"}),
            "invalid_format",
            "Entity ID  is an invalid entity ID for dictionary value @ data['entity_ids']. Got 'light.a,'",
        ),
        (
            json!({"entity_ids": ["nope"]}),
            "invalid_format",
            "Entity ID nope is an invalid entity ID for dictionary value @ data['entity_ids']. Got ['nope']",
        ),
        (
            json!({"entity_ids": [1]}),
            "invalid_format",
            "Entity ID 1 is an invalid entity ID for dictionary value @ data['entity_ids']. Got [1]",
        ),
        (
            json!({"entity_ids": null}),
            "invalid_format",
            "Entity IDs cannot be None for dictionary value @ data['entity_ids']. Got None",
        ),
        (json!({"entity_ids": 5}), "unknown_error", "Unknown error"),
        (
            json!({"entity_ids": {"a": 1}}),
            "invalid_format",
            "Entity ID a is an invalid entity ID for dictionary value @ data['entity_ids']. Got {'a': 1}",
        ),
        (
            json!({"include": null}),
            "invalid_format",
            "expected a dictionary for dictionary value @ data['include']. Got None",
        ),
        (
            json!({"include": "zone"}),
            "invalid_format",
            "expected a dictionary for dictionary value @ data['include']. Got 'zone'",
        ),
        (
            json!({"include": []}),
            "invalid_format",
            "expected a dictionary for dictionary value @ data['include']. Got []",
        ),
        (
            json!({"include": {"domains": [null]}}),
            "invalid_format",
            "string value is None @ data['include']['domains'][0]. Got None",
        ),
        (
            json!({"include": {"domains": [[1]]}}),
            "invalid_format",
            "value should be a string @ data['include']['domains'][0]. Got [1]",
        ),
        (
            json!({"include": {"domains": [{"a": 1}]}}),
            "invalid_format",
            "value should be a string @ data['include']['domains'][0]. Got {'a': 1}",
        ),
        (
            json!({"include": {"entities": ["bad"]}}),
            "invalid_format",
            "Entity ID bad is an invalid entity ID for dictionary value @ data['include']['entities']. Got ['bad']",
        ),
        (
            json!({"include": {"entities": null}}),
            "invalid_format",
            "Entity IDs cannot be None for dictionary value @ data['include']['entities']. Got None",
        ),
        (
            json!({"include": {"foo": 1}}),
            "invalid_format",
            "extra keys not allowed @ data['include']['foo']. Got 1",
        ),
        (
            json!({"exclude": {"domains": {"a": 1}}}),
            "invalid_format",
            "value should be a string @ data['exclude']['domains'][0]. Got None",
        ),
        (
            json!({"include": {"domains": ["x"], "foo": 1}, "exclude": {"bar": 2}}),
            "invalid_format",
            "extra keys not allowed @ data['exclude']['bar']. Got 2\nextra keys not allowed @ data['include']['foo']. Got 1",
        ),
        (
            json!({"foo": 1}),
            "invalid_format",
            "extra keys not allowed @ data['foo']. Got 1",
        ),
        (
            json!({"entity_ids": true}),
            "unknown_error",
            "Unknown error",
        ),
        (json!({"entity_ids": 1.5}), "unknown_error", "Unknown error"),
        (
            json!({"entity_ids": ""}),
            "invalid_format",
            "Entity ID  is an invalid entity ID for dictionary value @ data['entity_ids']. Got ''",
        ),
        (
            json!({"entity_ids": " , "}),
            "invalid_format",
            "Entity ID  is an invalid entity ID for dictionary value @ data['entity_ids']. Got ' , '",
        ),
        (
            json!({"include": {"entities": 5}}),
            "unknown_error",
            "Unknown error",
        ),
        (
            json!({"include": {"entities": [null]}}),
            "invalid_format",
            "string value is None for dictionary value @ data['include']['entities']. Got [None]",
        ),
        (
            json!({"include": {"entities": [5]}}),
            "invalid_format",
            "Entity ID 5 is an invalid entity ID for dictionary value @ data['include']['entities']. Got [5]",
        ),
        (
            json!({"include": {"entity_globs": [null]}}),
            "invalid_format",
            "string value is None @ data['include']['entity_globs'][0]. Got None",
        ),
    ];
    for (n, (args, code, message)) in cases.into_iter().enumerate() {
        let mut msg = json!({"id": 100 + n, "type": "subscribe_entities"});
        msg.as_object_mut()
            .unwrap()
            .extend(args.as_object().unwrap().clone());
        let r = c.call(msg).await;
        assert_eq!(
            r["error"],
            json!({"code": code, "message": message}),
            "{args}"
        );
    }
}

/// HA's `ws.json`: the push channel's access checks, a silent channel,
/// its unsubscribe, and a confirm with nothing pending.
#[tokio::test]
async fn mobile_app_push_channel_as_ha() {
    let reg = |webhook: &str, user: &str| {
        json!({"domain": "mobile_app", "entry_id": webhook, "disabled_by": null,
               "data": {"webhook_id": webhook, "user_id": user, "secret": "00",
                        "supports_encryption": false}})
        .as_object()
        .unwrap()
        .clone()
    };
    // `server_with`, but the registration belongs to this server's owner.
    let hass = Hass::new();
    hass.start();
    let auth = crate::test_util::auth(true);
    let (tok, _) = token(&auth);
    let owner = auth.validate_access_token(&tok, None).unwrap().user_id;
    let entries = [reg("mine", &owner), reg("theirs", "someone-else")];
    let app = ha_integrations::mobile_app::MobileApp::new(&entries, Vec::new());
    app.setup(
        &hass,
        ha_registry::Registry::in_memory(&hass, Default::default()),
        ha_integrations::config_entries::ConfigEntries::in_memory(&hass, entries.to_vec()),
        None,
    );
    let config = crate::ApiConfig {
        config_dir: "/config".into(),
        mobile_app: Some(std::sync::Arc::new(app)),
        ..Default::default()
    };
    let router = crate::router(hass.clone(), auth.clone(), &config)
        .into_make_service_with_connect_info::<std::net::SocketAddr>();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/api/websocket", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let s = Server { url, hass, auth };
    let mut c = Client::authed(&s, &tok).await;
    let err = |r: Value| r["error"].clone();
    assert_eq!(
        err(c
            .call(
                json!({"id": 1, "type": "mobile_app/push_notification_channel",
                          "webhook_id": "theirs", "support_confirm": true})
            )
            .await),
        json!({"code": "unauthorized", "message": "User not linked to this webhook ID"})
    );
    assert_eq!(
        err(c
            .call(
                json!({"id": 2, "type": "mobile_app/push_notification_channel",
                          "webhook_id": "nope"})
            )
            .await),
        json!({"code": "not_found", "message": "Webhook ID not found"})
    );
    let r = c
        .call(
            json!({"id": 3, "type": "mobile_app/push_notification_channel",
                     "webhook_id": "mine", "support_confirm": true}),
        )
        .await;
    assert_eq!(
        (r["success"].clone(), r["result"].clone()),
        (json!(true), Value::Null)
    );
    assert_eq!(
        err(c
            .call(
                json!({"id": 4, "type": "mobile_app/push_notification_confirm",
                          "webhook_id": "mine", "confirm_id": "nope"})
            )
            .await),
        json!({"code": "not_found", "message": "Push notification channel not found"})
    );
    let r = c
        .call(json!({"id": 5, "type": "unsubscribe_events", "subscription": 3}))
        .await;
    assert_eq!(r["success"], true);
    // HA's schema: strict types, no extra keys.
    let r = c
        .call(
            json!({"id": 6, "type": "mobile_app/push_notification_channel",
                     "webhook_id": 5, "x": 1}),
        )
        .await;
    assert_eq!(r["error"]["code"], "invalid_format");
}

/// With the recorder running: `recorder/info` in HA's shape, and the
/// statistics calls answer empty after HA's schema and time checks.
#[tokio::test]
async fn recorder_info_and_empty_statistics() {
    let dir = std::env::temp_dir().join(format!("hars-ws-recorder-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let recorder =
        ha_recorder::Recorder::start(&Hass::new(), dir.clone(), &ha_recorder::Settings::default())
            .await
            .unwrap();
    let s = server_with(crate::ApiConfig {
        recorder: Some(std::sync::Arc::new(recorder)),
        ..Default::default()
    })
    .await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    let ok = |id: u64, result: Value| json!({"id": id, "type": "result", "success": true, "result": result});
    let err = |id: u64, code: &str, message: &str| json!({"id": id, "type": "result", "success": false, "error": {"code": code, "message": message}});
    // The frontend draws history graphs, the Map's trail and the logbook
    // only when `get_config` lists these components.
    let mut c2 = Client::authed(&s, &token(&s.auth).0).await;
    let config = c2.call(json!({"id": 1, "type": "get_config"})).await["result"].clone();
    for p in ["recorder", "history", "logbook"] {
        assert!(
            config["components"].as_array().unwrap().contains(&json!(p)),
            "{p}"
        );
    }
    // The sidebar's History and Logbook entries, as `history` and `logbook`
    // register them.
    let panels = c2.call(json!({"id": 2, "type": "get_panels"})).await["result"].clone();
    for (p, icon) in [
        ("history", "mdi:chart-box"),
        ("logbook", "mdi:format-list-bulleted-type"),
    ] {
        assert_eq!(
            panels[p],
            json!({
                "component_name": p, "icon": icon, "title": p,
                "default_visible": true, "config": null, "url_path": p,
                "require_admin": false, "config_panel_domain": null,
                "show_in_sidebar": true,
            }),
        );
    }
    assert_eq!(
        c.call(json!({"id": 1, "type": "recorder/info"})).await,
        ok(
            1,
            json!({"backlog": 0, "db_in_default_location": true, "max_backlog": 10000,
                   "migration_in_progress": false, "migration_is_live": false,
                   "recording": true, "thread_running": true})
        )
    );
    assert_eq!(
        c.call(json!({"id": 2, "type": "recorder/get_statistics_metadata",
                      "statistic_ids": ["sensor.hall_temperature"]}))
            .await,
        ok(2, json!([]))
    );
    assert_eq!(
        c.call(json!({"id": 3, "type": "recorder/list_statistic_ids", "statistic_type": "mean"}))
            .await,
        ok(3, json!([]))
    );
    let period = |id: u64, extra: Value| {
        let mut m = json!({"id": id, "type": "recorder/statistics_during_period",
                           "start_time": "2026-01-15T08:30:00.000Z",
                           "statistic_ids": ["sensor.hall_temperature"], "period": "5minute",
                           "types": ["mean", "min", "max"], "units": {"temperature": "°C"}});
        for (k, v) in extra.as_object().unwrap() {
            m[k] = v.clone();
        }
        m
    };
    assert_eq!(c.call(period(4, json!({}))).await, ok(4, json!({})));
    assert_eq!(
        c.call(period(5, json!({"start_time": "soon"}))).await,
        err(5, "invalid_start_time", "Invalid start_time")
    );
    assert_eq!(
        c.call(period(6, json!({"end_time": "later"}))).await,
        err(6, "invalid_end_time", "Invalid end_time")
    );
    let r = c.call(period(7, json!({"statistic_ids": []}))).await;
    assert_eq!(r["error"]["code"], "invalid_format");
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("length of value must be at least 1"),
        "{r}"
    );
    let r = c.call(period(8, json!({"period": "decade"}))).await;
    assert_eq!(r["error"]["code"], "invalid_format", "{r}");
    // HA's unit schema: known converters, their units.
    let r = c
        .call(period(9, json!({"units": {"temperature": "bananas"}})))
        .await;
    assert_eq!(r["error"]["code"], "invalid_format", "{r}");
    assert!(
        r["error"]["message"].as_str().unwrap().starts_with(
            "value must be one of [<UnitOfTemperature.KELVIN: 'K'>, <UnitOfTemperature.CELSIUS: '°C'>"
        ),
        "{r}"
    );
    let r = c
        .call(period(10, json!({"units": {"colour": "red"}})))
        .await;
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("extra keys not allowed @ data['units']['colour']"),
        "{r}"
    );
    assert_eq!(
        c.call(period(
            11,
            json!({"units": {"unitless": null, "energy": "kWh"}})
        ))
        .await,
        ok(11, json!({}))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A server whose recorder records the server's own states.
async fn recording_server(name: &str) -> (Server, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("hars-ws-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let hass = Hass::new();
    let recorder =
        ha_recorder::Recorder::start(&hass, dir.clone(), &ha_recorder::Settings::default())
            .await
            .unwrap();
    let s = server_on(
        hass,
        crate::ApiConfig {
            recorder: Some(std::sync::Arc::new(recorder)),
            ..Default::default()
        },
    )
    .await;
    (s, dir)
}

fn minutes_ago(n: i64) -> String {
    (jiff::Timestamp::now() - jiff::SignedDuration::from_mins(n)).to_string()
}

/// `history/history_during_period`: recorded states in HA's compressed
/// shape, and HA's checks before it reads anything.
#[tokio::test]
async fn history_during_period_answers_from_the_recorder() {
    let (s, dir) = recording_server("history-period").await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    let light = EntityId::parse("light.a").unwrap();
    s.hass.set_state(light.clone(), "on", Attributes::new());
    s.hass.set_state(light.clone(), "off", Attributes::new());

    let ask = |id: u64, extra: Value| {
        let mut m = json!({"id": id, "type": "history/history_during_period",
                           "start_time": minutes_ago(5), "entity_ids": ["light.a"]});
        for (k, v) in extra.as_object().unwrap() {
            m[k] = v.clone();
        }
        m
    };
    // The recorder takes the feed on its own task: ask until it has both.
    let mut id = 1;
    let rows = loop {
        let r = c.call(ask(id, json!({}))).await;
        assert_eq!(r["success"], true, "{r}");
        if r["result"]["light.a"]
            .as_array()
            .is_some_and(|a| a.len() == 2)
        {
            break r["result"]["light.a"].clone();
        }
        id += 1;
        assert!(id < 100, "never recorded: {r}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(rows[0]["s"], "on");
    assert_eq!(rows[1]["s"], "off");
    assert_eq!(rows[1]["a"], json!({}));
    assert!(rows[0]["lu"].as_f64().unwrap() < rows[1]["lu"].as_f64().unwrap());

    id += 1;
    let r = c
        .call(ask(
            id,
            json!({"minimal_response": true, "no_attributes": true}),
        ))
        .await;
    assert_eq!(
        r["result"]["light.a"][1].as_object().unwrap().len(),
        2,
        "{r}"
    );

    id += 1;
    let r = c.call(ask(id, json!({"start_time": "soon"}))).await;
    assert_eq!(r["error"]["code"], "invalid_start_time");
    id += 1;
    let r = c.call(ask(id, json!({"end_time": "later"}))).await;
    assert_eq!(r["error"]["code"], "invalid_end_time");
    id += 1;
    let r = c
        .call(ask(id, json!({"entity_ids": ["not an entity"]})))
        .await;
    assert_eq!(r["error"]["code"], "invalid_entity_ids");
    // A start in the future, or an unknown entity: empty.
    id += 1;
    let r = c
        .call(ask(id, json!({"start_time": minutes_ago(-5)})))
        .await;
    assert_eq!(r["result"], json!({}));
    id += 1;
    let r = c
        .call(ask(id, json!({"entity_ids": ["light.nobody"]})))
        .await;
    assert_eq!(r["result"], json!({}));
    id += 1;
    let r = c.call(ask(id, json!({"entity_ids": []}))).await;
    assert_eq!(r["result"], json!({}));
    id += 1;
    let r = c
        .call(json!({"id": id, "type": "history/history_during_period",
                          "start_time": minutes_ago(5)}))
        .await;
    assert_eq!(r["error"]["code"], "invalid_format");
    let _ = std::fs::remove_dir_all(dir);
}

/// `history/stream`: the result, the recorded states, then live changes
/// grouped into one message.
#[tokio::test]
async fn history_stream_sends_the_past_then_follows() {
    let (s, dir) = recording_server("history-stream").await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    let light = EntityId::parse("light.a").unwrap();
    s.hass.set_state(light.clone(), "on", Attributes::new());
    // Wait for it to be recorded.
    for id in 1.. {
        let r = c
            .call(json!({"id": id, "type": "history/history_during_period",
                              "start_time": minutes_ago(5), "entity_ids": ["light.a"]}))
            .await;
        if r["result"].as_object().is_some_and(|m| !m.is_empty()) {
            break;
        }
        assert!(id < 100, "never recorded");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    c.send(
        json!({"id": 200, "type": "history/stream", "start_time": minutes_ago(5),
                  "entity_ids": ["light.a"]}),
    )
    .await;
    let r = c.recv().await;
    assert_eq!(
        r,
        json!({"id": 200, "type": "result", "success": true, "result": null})
    );
    let past = c.recv().await;
    assert_eq!(past["type"], "event");
    assert_eq!(past["event"]["states"]["light.a"][0]["s"], "on");
    assert!(past["event"]["start_time"].is_f64() && past["event"]["end_time"].is_f64());

    // Live: two quick changes arrive as one message.
    s.hass.set_state(light.clone(), "off", Attributes::new());
    s.hass.set_state(light.clone(), "on", Attributes::new());
    let live = c.recv().await;
    let states = live["event"]["states"]["light.a"].as_array().unwrap();
    assert_eq!(
        states
            .iter()
            .map(|x| x["s"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["off", "on"],
        "{live}"
    );
    assert!(live["event"].get("start_time").is_none());
    let r = c
        .call(json!({"id": 201, "type": "unsubscribe_events", "subscription": 200}))
        .await;
    assert_eq!(r["success"], true);

    // After `end_time`, nothing more is sent, even a change that was
    // still being grouped.
    c.send(json!({"id": 202, "type": "history/stream", "start_time": minutes_ago(5),
                  "end_time": (jiff::Timestamp::now() + jiff::SignedDuration::from_millis(900)).to_string(),
                  "entity_ids": ["light.a"]})).await;
    assert_eq!(c.recv().await["result"], Value::Null);
    assert_eq!(c.recv().await["type"], "event");
    s.hass.set_state(light.clone(), "off", Attributes::new());
    assert_eq!(c.recv().await["event"]["states"]["light.a"][0]["s"], "off");
    // (Read while waiting, so the heartbeat is answered.)
    assert!(
        tokio::time::timeout(Duration::from_millis(900), c.recv())
            .await
            .is_err()
    );
    s.hass.set_state(light.clone(), "on", Attributes::new());
    assert!(
        tokio::time::timeout(Duration::from_millis(800), c.recv())
            .await
            .is_err()
    );

    // A group still open when the window closes is dropped.
    c.send(json!({"id": 203, "type": "history/stream", "start_time": minutes_ago(5),
                  "end_time": (jiff::Timestamp::now() + jiff::SignedDuration::from_millis(250)).to_string(),
                  "entity_ids": ["light.a"]})).await;
    assert_eq!(c.recv().await["result"], Value::Null);
    assert_eq!(c.recv().await["type"], "event");
    s.hass.set_state(light.clone(), "off", Attributes::new());
    assert!(
        tokio::time::timeout(Duration::from_millis(900), c.recv())
            .await
            .is_err()
    );

    // A finished window answers once, even when empty.
    c.send(
        json!({"id": 204, "type": "history/stream", "start_time": minutes_ago(120),
                  "end_time": minutes_ago(60), "entity_ids": ["light.a"]}),
    )
    .await;
    assert_eq!(c.recv().await["result"], Value::Null);
    let r = c.recv().await;
    assert_eq!(r["event"]["states"], json!({}));
    // A start in the future is an error for a stream.
    let r = c
        .call(
            json!({"id": 205, "type": "history/stream", "start_time": minutes_ago(-5),
                     "entity_ids": ["light.a"]}),
        )
        .await;
    assert_eq!(r["error"]["code"], "invalid_start_time");
    let _ = std::fs::remove_dir_all(dir);
}

/// An `automation_triggered` event, as the automation engine fires it.
fn triggered(hass: &Hass, data: Value) -> ha_core::Context {
    let context = ha_core::Context::new();
    hass.bus().fire(ha_core::Event::new(
        ha_core::EventKind::Custom {
            name: "automation_triggered".into(),
            data,
        },
        hass.clock().now(),
        context.clone(),
    ));
    context
}

/// A logbook row without its time (which is checked to be a number).
fn bare(row: &Value) -> Value {
    let mut row = row.clone();
    assert!(row["when"].is_f64(), "{row}");
    row.as_object_mut().unwrap().remove("when");
    row
}

fn bare_all(rows: &Value) -> Vec<Value> {
    rows.as_array().unwrap().iter().map(bare).collect()
}

const USER: &str = "0a0a0a0a0a0a4a0a8a0a0a0a0a0a0a01";

/// What the logbook tests record: start, a lamp switched by a user, a
/// washer, an automation run, and the continuous things the logbook skips.
fn logbook_history(hass: &Hass) -> ha_core::Context {
    let id = |s: &str| EntityId::parse(s).unwrap();
    let lamp = attrs(json!({"icon": "mdi:lamp"}));
    // The first row of an entity is never in the logbook.
    hass.set_state(id("light.a"), "on", lamp.clone());
    let user = ha_core::Context::new().with_user(USER);
    hass.set_state_with_context(id("light.a"), "off", lamp, Some(user));
    let unit = attrs(json!({"unit_of_measurement": "W"}));
    hass.set_state(id("sensor.power"), "1", unit.clone());
    hass.set_state(id("sensor.power"), "2", unit);
    hass.set_state(id("counter.visits"), "1", Attributes::new());
    hass.set_state(id("counter.visits"), "2", Attributes::new());
    hass.set_state(id("sensor.washer"), "idle", Attributes::new());
    hass.set_state(id("sensor.washer"), "done", Attributes::new());
    triggered(
        hass,
        json!({"name": "Washer done", "entity_id": "automation.washer_done",
               "source": "state of sensor.washer"}),
    )
}

/// `logbook/get_events`: HA's rows over the recorded history, its
/// filters, and its checks before it reads anything.
#[tokio::test]
async fn logbook_get_events_answers_from_the_recorder() {
    let (s, dir) = recording_server("logbook-events").await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    let run = logbook_history(&s.hass);

    let ask = |id: u64, extra: Value| {
        let mut m = json!({"id": id, "type": "logbook/get_events", "start_time": minutes_ago(5)});
        for (k, v) in extra.as_object().unwrap() {
            m[k] = v.clone();
        }
        m
    };
    let started = json!({"name": "Home Assistant", "message": "started",
                         "icon": "mdi:home-assistant", "domain": "homeassistant"});
    let lamp = json!({"state": "off", "entity_id": "light.a", "icon": "mdi:lamp",
                      "context_user_id": USER});
    let washer = json!({"state": "done", "entity_id": "sensor.washer"});
    let automation = json!({"name": "Washer done", "message": "triggered by state of sensor.washer",
                            "source": "state of sensor.washer",
                            "entity_id": "automation.washer_done",
                            "context_id": run.id.to_string(), "domain": "automation"});

    // Everything shown, oldest first; no first rows, no units or counters.
    let r = c.call(ask(1, json!({}))).await;
    assert_eq!(r["success"], true, "{r}");
    assert_eq!(
        bare_all(&r["result"]),
        [
            started.clone(),
            lamp.clone(),
            washer.clone(),
            automation.clone()
        ]
    );
    let whens: Vec<f64> = r["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["when"].as_f64().unwrap())
        .collect();
    assert!(whens.windows(2).all(|w| w[0] <= w[1]), "{whens:?}");

    // Entities: their rows and the automation runs that name them.
    let r = c
        .call(ask(
            2,
            json!({"entity_ids": ["light.a", "automation.washer_done"]}),
        ))
        .await;
    assert_eq!(bare_all(&r["result"]), [lamp.clone(), automation.clone()]);
    // Devices: events that name them, no states.
    let r = c.call(ask(3, json!({"device_ids": ["dev1"]}))).await;
    assert_eq!(r["result"], json!([]));
    let r = c
        .call(ask(
            4,
            json!({"device_ids": ["dev1"], "entity_ids": ["sensor.washer"]}),
        ))
        .await;
    assert_eq!(bare_all(&r["result"]), [washer]);
    // Every entity is a continuous sensor or counter: nothing to read.
    let r = c
        .call(ask(
            5,
            json!({"entity_ids": ["sensor.power", "counter.visits"]}),
        ))
        .await;
    assert_eq!(r["result"], json!([]));
    // A context: the automation's rows.
    let r = c
        .call(ask(6, json!({"context_id": run.id.to_string()})))
        .await;
    assert_eq!(bare_all(&r["result"]), [automation]);
    // Not a ULID: no filter at all.
    let r = c.call(ask(7, json!({"context_id": "nope"}))).await;
    assert_eq!(r["result"].as_array().unwrap().len(), 4, "{r}");
    // The window is open at both ends and defaults to now.
    let r = c.call(ask(8, json!({"end_time": minutes_ago(1)}))).await;
    assert_eq!(r["result"], json!([]));

    // The checks.
    let r = c.call(ask(9, json!({"start_time": "soon"}))).await;
    assert_eq!(
        r["error"],
        json!({"code": "invalid_start_time", "message": "Invalid start_time"})
    );
    let r = c.call(ask(10, json!({"end_time": "later"}))).await;
    assert_eq!(r["error"]["code"], "invalid_end_time");
    let r = c
        .call(ask(11, json!({"start_time": minutes_ago(-5)})))
        .await;
    assert_eq!(r["result"], json!([]));
    // `EventProcessor` asserts on a context with entities.
    let r = c
        .call(ask(
            12,
            json!({"context_id": run.id.to_string(), "entity_ids": ["light.a"]}),
        ))
        .await;
    assert_eq!(
        r["error"],
        json!({"code": "unknown_error", "message": "Unknown error"})
    );
    let r = c.call(ask(13, json!({"entity_ids": ["light"]}))).await;
    assert_eq!(r["error"]["code"], "unknown_error");
    let r = c.call(ask(14, json!({"entity_ids": "light.a"}))).await;
    assert_eq!(
        r["error"],
        json!({"code": "invalid_format",
               "message": "expected list for dictionary value @ data['entity_ids']. Got 'light.a'"})
    );
    let r = c.call(ask(15, json!({"entity_ids": [1]}))).await;
    assert_eq!(r["error"]["code"], "invalid_format");
    let r = c
        .call(json!({"id": 16, "type": "logbook/get_events"}))
        .await;
    assert_eq!(
        r["error"]["message"],
        "required key not provided @ data['start_time']. Got None"
    );
    let r = c.call(ask(17, json!({"surprise": 1}))).await;
    assert_eq!(r["error"]["code"], "invalid_format");
    let _ = std::fs::remove_dir_all(dir);
}

/// `logbook/event_stream`: the result, the past as a partial message and
/// the closing one, then live rows in 350 ms groups.
#[tokio::test]
async fn logbook_stream_sends_the_past_then_follows() {
    let (s, dir) = recording_server("logbook-stream").await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    let run = logbook_history(&s.hass);
    let id = |s: &str| EntityId::parse(s).unwrap();

    c.send(json!({"id": 200, "type": "logbook/event_stream", "start_time": minutes_ago(5)}))
        .await;
    assert_eq!(
        c.recv().await,
        json!({"id": 200, "type": "result", "success": true, "result": null})
    );
    let past = c.recv().await;
    assert_eq!(
        (past["id"].clone(), past["type"].clone()),
        (json!(200), json!("event"))
    );
    assert_eq!(past["event"]["partial"], true, "{past}");
    assert_eq!(
        bare_all(&past["event"]["events"])
            .iter()
            .map(|r| r.get("entity_id").or(r.get("name")).unwrap().clone())
            .collect::<Vec<_>>(),
        [
            "Home Assistant",
            "light.a",
            "sensor.washer",
            "automation.washer_done"
        ]
    );
    let (start, end) = (
        past["event"]["start_time"].as_f64().unwrap(),
        past["event"]["end_time"].as_f64().unwrap(),
    );
    assert!(start < end);
    // The closing message: nothing newer, and not partial.
    let closing = c.recv().await;
    assert_eq!(closing["event"]["events"], json!([]));
    assert!(closing["event"].get("partial").is_none(), "{closing}");
    let newest = past["event"]["events"].as_array().unwrap().last().unwrap()["when"]
        .as_f64()
        .unwrap();
    assert!((closing["event"]["start_time"].as_f64().unwrap() - newest).abs() < 1e-3);
    assert_eq!(closing["event"]["end_time"], past["event"]["end_time"]);

    // Live: the changes within one group arrive as one message, in order;
    // continuous sensors and counters don't.
    s.hass
        .set_state(id("light.a"), "on", attrs(json!({"icon": "mdi:lamp"})));
    s.hass.set_state(
        id("sensor.power"),
        "3",
        attrs(json!({"unit_of_measurement": "W"})),
    );
    s.hass
        .set_state(id("counter.visits"), "3", Attributes::new());
    s.hass
        .set_state(id("sensor.washer"), "running", Attributes::new());
    let live = c.recv().await;
    assert_eq!(live["id"], 200);
    assert!(live["event"].get("start_time").is_none(), "{live}");
    assert_eq!(
        bare_all(&live["event"]["events"]),
        [
            json!({"state": "on", "entity_id": "light.a", "icon": "mdi:lamp"}),
            json!({"state": "running", "entity_id": "sensor.washer"}),
        ]
    );
    // An event, with the context it was fired in.
    let next = triggered(&s.hass, json!({"name": "Again", "source": "time"}));
    let live = c.recv().await;
    assert_eq!(
        bare_all(&live["event"]["events"]),
        [
            json!({"name": "Again", "message": "triggered by time", "source": "time",
                "entity_id": null, "context_id": next.id.to_string(), "domain": "automation"})
        ]
    );
    // A change caused by that automation names it.
    s.hass.set_state_with_context(
        id("light.a"),
        "off",
        attrs(json!({"icon": "mdi:lamp"})),
        Some(ha_core::Context::new().with_parent(&next)),
    );
    let live = c.recv().await;
    assert_eq!(
        bare_all(&live["event"]["events"]),
        [
            json!({"state": "off", "entity_id": "light.a", "icon": "mdi:lamp",
                "context_event_type": "automation_triggered", "context_domain": "automation",
                "context_name": "Again", "context_message": "triggered by time",
                "context_source": "time"})
        ]
    );
    assert_eq!(
        c.call(json!({"id": 201, "type": "unsubscribe_events", "subscription": 200}))
            .await["success"],
        true
    );
    let _ = run;

    // Filtered to an entity: its past, and only its live rows.
    c.send(
        json!({"id": 202, "type": "logbook/event_stream", "start_time": minutes_ago(5),
                  "entity_ids": ["light.a"]}),
    )
    .await;
    assert_eq!(c.recv().await["result"], Value::Null);
    let past = c.recv().await;
    assert_eq!(past["event"]["partial"], true);
    assert_eq!(
        past["event"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["entity_id"].clone())
            .collect::<Vec<_>>(),
        ["light.a", "light.a", "light.a"]
    );
    assert_eq!(c.recv().await["event"]["events"], json!([]));
    s.hass
        .set_state(id("sensor.washer"), "idle", Attributes::new());
    s.hass
        .set_state(id("light.a"), "on", attrs(json!({"icon": "mdi:lamp"})));
    let live = c.recv().await;
    assert_eq!(
        live["event"]["events"].as_array().unwrap().len(),
        1,
        "{live}"
    );
    assert_eq!(live["event"]["events"][0]["entity_id"], "light.a");
    assert_eq!(
        c.call(json!({"id": 203, "type": "unsubscribe_events", "subscription": 202}))
            .await["success"],
        true
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// The stream's other shapes: windows that are over, big ones split in
/// two, empty answers, and HA's checks.
#[tokio::test]
async fn logbook_stream_windows_and_checks() {
    let (s, dir) = recording_server("logbook-windows").await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    logbook_history(&s.hass);
    let days_ago = |d: i64| minutes_ago(d * 24 * 60);
    let stream = |id: u64, extra: Value| {
        let mut m = json!({"id": id, "type": "logbook/event_stream"});
        for (k, v) in extra.as_object().unwrap() {
            m[k] = v.clone();
        }
        m
    };

    // A window that is over: one message, empty is still an answer.
    c.send(stream(
        1,
        json!({"start_time": minutes_ago(120), "end_time": minutes_ago(60)}),
    ))
    .await;
    assert_eq!(c.recv().await["result"], Value::Null);
    let r = c.recv().await;
    assert_eq!(r["event"]["events"], json!([]));
    assert!(r["event"].get("partial").is_none());
    let (a, b) = (
        r["event"]["start_time"].as_f64().unwrap(),
        r["event"]["end_time"].as_f64().unwrap(),
    );
    assert!((b - a - 3600.0).abs() < 1.0);
    // Its subscription exists, so it can be cancelled.
    assert_eq!(
        c.call(json!({"id": 2, "type": "unsubscribe_events", "subscription": 1}))
            .await["success"],
        true
    );

    // Over, with rows.
    c.send(stream(
        3,
        json!({"start_time": minutes_ago(5),
        "end_time": (jiff::Timestamp::now() - jiff::SignedDuration::from_millis(1)).to_string()}),
    ))
    .await;
    assert_eq!(c.recv().await["result"], Value::Null);
    let r = c.recv().await;
    assert_eq!(r["event"]["events"].as_array().unwrap().len(), 4, "{r}");
    assert!(r["event"].get("partial").is_none());

    // Over and big (more than 25 h, no filter): the recent day is sent
    // only if it has rows, then the rest, not partial.
    c.send(stream(
        4,
        json!({"start_time": days_ago(3), "end_time": minutes_ago(1)}),
    ))
    .await;
    assert_eq!(c.recv().await["result"], Value::Null);
    let r = c.recv().await;
    assert_eq!(r["event"]["events"], json!([]), "{r}");
    assert!(r["event"].get("partial").is_none());
    let (a, b) = (
        r["event"]["start_time"].as_f64().unwrap(),
        r["event"]["end_time"].as_f64().unwrap(),
    );
    // The older part: from the start to a day before the end.
    assert!(
        (b - a - (2.0 * 24.0 - 1.0 / 60.0) * 3600.0).abs() < 2.0,
        "{r}"
    );
    // A filtered window is never split.
    c.send(stream(
        5,
        json!({"start_time": days_ago(3), "end_time": minutes_ago(1),
                            "entity_ids": ["light.a"]}),
    ))
    .await;
    assert_eq!(c.recv().await["result"], Value::Null);
    let r = c.recv().await;
    let (a, b) = (
        r["event"]["start_time"].as_f64().unwrap(),
        r["event"]["end_time"].as_f64().unwrap(),
    );
    assert!(
        (b - a - (3.0 * 24.0 - 1.0 / 60.0) * 3600.0).abs() < 2.0,
        "{r}"
    );

    // Live and big: the recent day (partial, it has rows), the rest
    // (partial, sent though empty), then the closing message.
    c.send(stream(6, json!({"start_time": days_ago(3)}))).await;
    assert_eq!(c.recv().await["result"], Value::Null);
    let recent = c.recv().await;
    assert_eq!(recent["event"]["partial"], true);
    assert_eq!(
        recent["event"]["events"].as_array().unwrap().len(),
        4,
        "{recent}"
    );
    let (a, b) = (
        recent["event"]["start_time"].as_f64().unwrap(),
        recent["event"]["end_time"].as_f64().unwrap(),
    );
    assert!((b - a - 24.0 * 3600.0).abs() < 1e-3);
    let older = c.recv().await;
    assert_eq!(older["event"]["events"], json!([]));
    assert_eq!(older["event"]["partial"], true);
    assert_eq!(older["event"]["end_time"], recent["event"]["start_time"]);
    let closing = c.recv().await;
    assert_eq!(closing["event"]["events"], json!([]));
    assert!(closing["event"].get("partial").is_none());
    assert_eq!(
        c.call(json!({"id": 7, "type": "unsubscribe_events", "subscription": 6}))
            .await["success"],
        true
    );

    // Nothing to show for a continuous sensor: the result, then an empty
    // message that is not registered for unsubscribing.
    c.send(stream(
        8,
        json!({"start_time": minutes_ago(5), "entity_ids": ["sensor.power"]}),
    ))
    .await;
    assert_eq!(c.recv().await["result"], Value::Null);
    let r = c.recv().await;
    assert_eq!(r["event"]["events"], json!([]));
    assert!(r["event"].get("partial").is_none());
    let r = c
        .call(json!({"id": 9, "type": "unsubscribe_events", "subscription": 8}))
        .await;
    assert_eq!(r["error"]["code"], "not_found");
    // Unless a device is asked for too.
    c.send(stream(
        10,
        json!({"start_time": minutes_ago(5), "end_time": minutes_ago(1),
                             "entity_ids": ["sensor.power"], "device_ids": ["d"]}),
    ))
    .await;
    assert_eq!(c.recv().await["result"], Value::Null);
    assert_eq!(c.recv().await["event"]["events"], json!([]));

    // After `end_time`, nothing more is sent.
    c.send(stream(
        11,
        json!({"start_time": minutes_ago(5),
        "end_time": (jiff::Timestamp::now() + jiff::SignedDuration::from_millis(900)).to_string(),
        "entity_ids": ["light.a"]}),
    ))
    .await;
    assert_eq!(c.recv().await["result"], Value::Null);
    assert_eq!(c.recv().await["event"]["partial"], true);
    assert_eq!(c.recv().await["event"]["events"], json!([]));
    let id = |s: &str| EntityId::parse(s).unwrap();
    s.hass
        .set_state(id("light.a"), "on", attrs(json!({"icon": "mdi:lamp"})));
    assert_eq!(c.recv().await["event"]["events"][0]["state"], "on");
    // (Read while waiting, so the heartbeat is answered.)
    assert!(
        tokio::time::timeout(Duration::from_millis(900), c.recv())
            .await
            .is_err()
    );
    s.hass
        .set_state(id("light.a"), "off", attrs(json!({"icon": "mdi:lamp"})));
    assert!(
        tokio::time::timeout(Duration::from_millis(800), c.recv())
            .await
            .is_err()
    );

    // The checks.
    let r = c
        .call(stream(12, json!({"start_time": minutes_ago(-5)})))
        .await;
    assert_eq!(
        r["error"],
        json!({"code": "invalid_start_time", "message": "Invalid start_time"})
    );
    let r = c.call(stream(13, json!({"start_time": "soon"}))).await;
    assert_eq!(r["error"]["code"], "invalid_start_time");
    let r = c
        .call(stream(
            14,
            json!({"start_time": minutes_ago(5), "end_time": minutes_ago(10)}),
        ))
        .await;
    assert_eq!(
        r["error"],
        json!({"code": "invalid_end_time", "message": "Invalid end_time"})
    );
    let r = c
        .call(stream(
            15,
            json!({"start_time": minutes_ago(5), "end_time": "later"}),
        ))
        .await;
    assert_eq!(r["error"]["code"], "invalid_end_time");
    // `context_id` is `get_events`'s alone.
    let r = c
        .call(stream(
            16,
            json!({"start_time": minutes_ago(5), "context_id": "x"}),
        ))
        .await;
    assert_eq!(r["error"]["code"], "invalid_format");
    // Live with nothing recorded: the first message is sent anyway (so the
    // client isn't left waiting on it), partial, then the closing one.
    c.send(stream(
        17,
        json!({"start_time": minutes_ago(5), "entity_ids": ["light.nobody"]}),
    ))
    .await;
    assert_eq!(c.recv().await["result"], Value::Null);
    let r = c.recv().await;
    assert_eq!(r["event"]["events"], json!([]));
    assert_eq!(r["event"]["partial"], true);
    let r = c.recv().await;
    assert_eq!(r["event"]["events"], json!([]));
    assert!(r["event"].get("partial").is_none());
    assert_eq!(
        c.call(json!({"id": 18, "type": "unsubscribe_events", "subscription": 17}))
            .await["success"],
        true
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// The home `scripts/vectors/template_functions.py` renders against
/// (`ha-core`'s `template/vectors/home.json`): its states written at their
/// times, its time zone, and its clock stopped at its `now` (tokio's time
/// must be paused).
fn vector_hass() -> Hass {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../ha-core/src/template/vectors/home.json"
    );
    let fixture: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let at = |v: &Value| -> jiff::Timestamp { v.as_str().unwrap().parse().unwrap() };
    let sim = ha_core::SimulatedClock::new(at(&fixture["now"]));
    let hass = Hass::with_clock(sim.clock());
    hass.set_config(ha_core::config::CoreConfig {
        time_zone: fixture["time_zone"].as_str().unwrap().into(),
        ..Default::default()
    });
    for s in fixture["states"].as_array().unwrap() {
        let id = EntityId::parse(s["entity_id"].as_str().unwrap()).unwrap();
        let state = s["state"].as_str().unwrap();
        let attributes = s["attributes"].as_object().unwrap().clone();
        sim.jump_to(at(&s["last_changed"]));
        hass.set_state(id.clone(), state, attributes.clone());
        if let Some(updated) = s.get("last_updated") {
            sim.jump_to(at(updated));
            let mut changed = attributes.clone();
            changed.insert("_".into(), json!(1));
            hass.set_state(id.clone(), state, changed);
            hass.set_state(id, state, attributes);
        }
    }
    sim.jump_to(at(&fixture["now"]));
    hass
}

/// `render_template` against a home (`render_template_states.json`, from
/// `scripts/vectors/template_functions.py`): the live states, clock and
/// time zone, and `listeners` naming what the template read.
#[tokio::test(start_paused = true)]
async fn render_template_reads_the_home_as_ha() {
    let _alone = RENDER_PERMITS.lock().await;
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/ws/vectors/render_template_states.json"
    );
    let conversations: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert!(conversations.len() >= 6);
    let hass = vector_hass();
    for conversation in conversations {
        let name = conversation["name"].as_str().unwrap();
        let (mut conn, mut rx) = bare_conn(&hass);
        for step in conversation["steps"].as_array().unwrap() {
            handle(&mut conn, step["send"].clone());
            settle(&mut conn).await;
            assert_eq!(&json!(sent(&mut rx)), &step["replies"], "{name}");
        }
    }
}

/// `render_template` conversations from `scripts/vectors/render_template.py`:
/// HA 2026.6.1's `handle_render_template` on a fresh `HomeAssistant` each,
/// with every message it sent back, in order.
#[tokio::test]
async fn render_template_as_ha() {
    let _alone = RENDER_PERMITS.lock().await;
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/ws/vectors/render_template.json"
    );
    let conversations: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert!(conversations.len() > 40);
    for conversation in conversations {
        let name = conversation["name"].as_str().unwrap();
        let s = server().await;
        let mut c = Client::authed(&s, &token(&s.auth).0).await;
        for step in conversation["steps"].as_array().unwrap() {
            c.send(step["send"].clone()).await;
            for want in step["replies"].as_array().unwrap() {
                assert_eq!(&c.recv().await, want, "{name}: after {}", step["send"]);
            }
        }
        // Nothing else follows.
        let pong = c.call(json!({"id": 1000, "type": "ping"})).await;
        assert_eq!(pong, json!({"id": 1000, "type": "pong"}), "{name}");
    }
}

/// A template that reads nothing holds nothing: no task, no listener, no
/// timer once rendered; only the id, so `unsubscribe_events` finds it.
#[tokio::test]
async fn a_rendered_template_holds_nothing() {
    let _alone = RENDER_PERMITS.lock().await;
    let hass = Hass::new();
    let (mut conn, mut rx) = bare_conn(&hass);
    handle(
        &mut conn,
        json!({"id": 1, "type": "render_template", "template": "## Welcome {{ user }}",
               "variables": {"user": "Ann"}, "strict": true}),
    );
    settle(&mut conn).await;
    assert!(matches!(conn.subs.get(&1), Some(super::Subscription::Idle)));
    assert!(conn.event_listener.is_none() && conn.busy.is_none());
    let sent = sent(&mut rx);
    assert_eq!(sent.len(), 2, "the result and one event");
    assert_eq!(sent[1]["event"]["result"], "## Welcome Ann");
}

/// A connection's messages are handled in order: what follows a
/// `render_template` (even in the same frame, and with a `timeout`) waits
/// until its replies are out, as in HA when it renders straight away.
#[tokio::test]
async fn messages_after_a_render_wait_for_it() {
    let _alone = RENDER_PERMITS.lock().await;
    let hass = Hass::new();
    let (mut conn, mut rx) = bare_conn(&hass);
    let frame = json!([
        {"id": 1, "type": "render_template", "template": "{{ 1 }}", "timeout": 5},
        {"id": 2, "type": "unsubscribe_events", "subscription": 1},
        {"id": 3, "type": "ping"},
    ]);
    assert!(conn.handle_frame(&frame.to_string()));
    assert!(conn.busy.is_some());
    assert!(
        sent(&mut rx).is_empty(),
        "nothing before the render is done"
    );
    settle(&mut conn).await;
    let sent = sent(&mut rx);
    let ids: Vec<_> = sent
        .iter()
        .map(|m| (m["id"].clone(), m["type"].clone()))
        .collect();
    assert_eq!(
        ids,
        [
            (json!(1), json!("result")),
            (json!(1), json!("event")),
            (json!(2), json!("result")),
            (json!(3), json!("pong")),
        ]
    );
    assert_eq!(sent[2]["success"], true);
    assert!(conn.subs.is_empty());
}

/// Renders hold one of a fixed number of permits, from before they start
/// until their thread is done, even after a `timeout` gave up waiting: a
/// client sending expensive templates with tiny timeouts can't pile up
/// threads. The permit is back once the render reaches its fuel budget.
#[tokio::test(flavor = "multi_thread")]
async fn timed_out_renders_keep_their_permit_until_they_stop() {
    let _alone = RENDER_PERMITS.lock().await;
    let hass = Hass::new();
    let (mut conn, mut rx) = bare_conn(&hass);
    let heavy = "{% for i in range(100000) %}{% for j in range(100000) %}{% endfor %}{% endfor %}";
    for id in 1..=3 {
        handle(
            &mut conn,
            json!({"id": id, "type": "render_template", "template": heavy, "timeout": -1}),
        );
        settle(&mut conn).await;
        let reply = sent(&mut rx);
        assert_eq!(reply[0]["error"]["code"], "template_error", "{reply:?}");
        assert!(
            super::template::renders_free() < super::template::RENDERS_AT_ONCE,
            "the render still runs and holds its permit"
        );
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while super::template::renders_free() < super::template::RENDERS_AT_ONCE {
        assert!(
            tokio::time::Instant::now() < deadline,
            "renders never stopped"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// When a connection closes, its pending render is dropped: it never
/// starts, sends nothing, and lets go of its template.
#[tokio::test]
async fn closing_drops_a_waiting_render() {
    let _alone = RENDER_PERMITS.lock().await;
    let hass = Hass::new();
    let (mut conn, mut rx) = bare_conn(&hass);
    // Every permit taken: the render waits for one.
    let held = super::template::take_all_renders().await;
    handle(
        &mut conn,
        json!({"id": 1, "type": "render_template", "template": "{{ 1 }}"}),
    );
    let template = conn.shared.templates.get("{{ 1 }}");
    tokio::task::yield_now().await;
    assert!(
        std::sync::Arc::strong_count(&template) > 2,
        "the render holds it"
    );
    conn.close();
    drop(held);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        sent(&mut rx).is_empty(),
        "nothing sent for a closed connection"
    );
    assert_eq!(
        std::sync::Arc::strong_count(&template),
        2,
        "the cache and us"
    );
    assert!(conn.busy.is_none());
}

/// Rendering happens on a blocking thread: an expensive template (here
/// one that runs to its fuel budget) doesn't hold up the async runtime,
/// even a single-threaded one.
#[tokio::test(flavor = "current_thread")]
async fn renders_leave_the_runtime_free() {
    let _alone = RENDER_PERMITS.lock().await;
    let hass = Hass::new();
    let (mut conn, mut rx) = bare_conn(&hass);
    let ticks = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counter = ticks.clone();
    let ticker = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(1)).await;
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    });
    let heavy = "{% for i in range(100000) %}{% for j in range(100000) %}{% endfor %}{% endfor %}";
    handle(
        &mut conn,
        json!({"id": 1, "type": "render_template", "template": heavy, "report_errors": true}),
    );
    settle(&mut conn).await;
    ticker.abort();
    let reply = sent(&mut rx);
    assert!(
        reply[0]["event"]["error"]
            .as_str()
            .unwrap()
            .contains("budget"),
        "{reply:?}"
    );
    assert!(
        ticks.load(std::sync::atomic::Ordering::Relaxed) > 2,
        "the runtime kept running"
    );
}

/// Only short templates are kept: 128 long ones would hold megabytes.
#[tokio::test]
async fn long_templates_are_not_kept() {
    let _alone = RENDER_PERMITS.lock().await;
    let hass = Hass::new();
    let (mut conn, mut rx) = bare_conn(&hass);
    let long = format!("{{{{ 1 }}}}{}", " ".repeat(9 * 1024));
    for (id, template) in [(1, "{{ 1 }}".to_owned()), (2, long)] {
        handle(
            &mut conn,
            json!({"id": id, "type": "render_template", "template": template}),
        );
        settle(&mut conn).await;
    }
    assert_eq!(sent(&mut rx).len(), 4);
    assert_eq!(conn.shared.templates.len(), 1);
}

/// A client that sends a render and goes away while every render permit
/// is taken: the connection still reads the close (it reads while busy),
/// so the waiting render is dropped and never runs once permits come back.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_closes_while_its_render_waits_drops_it() {
    let _alone = RENDER_PERMITS.lock().await;
    let s = server().await;
    let held = super::template::take_all_renders().await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    // Runs to its fuel budget, holding a permit, if it ever starts.
    let heavy = "{% for i in range(100000) %}{% for j in range(100000) %}{% endfor %}{% endfor %}";
    c.send(json!({"id": 2, "type": "render_template", "template": heavy}))
        .await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    c.ws.close(None).await.unwrap();
    drop(c);
    // Well before a missed heartbeat (400 ms in tests) would end it anyway.
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(held);
    let until = tokio::time::Instant::now() + Duration::from_millis(500);
    // The heavy render, had it started, would hold its permit for its whole
    // fuel budget (about 175 ms in a debug build here). A render of another
    // test running alongside holds one for a moment (a millisecond or two),
    // which isn't this render.
    let mut held_since = None;
    while tokio::time::Instant::now() < until {
        let now = tokio::time::Instant::now();
        held_since = match super::template::renders_free() == super::template::RENDERS_AT_ONCE {
            true => None,
            false => held_since.or(Some(now)),
        };
        assert!(
            held_since.is_none_or(|since| now - since < Duration::from_millis(20)),
            "the render of a closed connection started"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// A connection waiting for a render still answers heartbeats, so it isn't
/// dropped as dead while the render waits for a permit.
#[tokio::test(flavor = "multi_thread")]
async fn a_busy_connection_keeps_its_heartbeat() {
    let _alone = RENDER_PERMITS.lock().await;
    let s = server().await;
    let held = super::template::take_all_renders().await;
    let mut c = Client::authed(&s, &token(&s.auth).0).await;
    c.send(json!({"id": 2, "type": "render_template", "template": "{{ 6 * 7 }}"}))
        .await;
    // Several heartbeats (200 ms in tests) pass before a permit is free;
    // the client answers each ping as it waits.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        drop(held);
    });
    assert_eq!(c.recv().await["success"], true);
    assert_eq!(c.recv().await["event"]["result"], 42);
}

/// What waits behind a busy command is bounded: past it the client is
/// cut off, as one that falls behind is.
#[tokio::test]
async fn the_queue_behind_a_busy_command_is_bounded() {
    let _alone = RENDER_PERMITS.lock().await;
    let hass = Hass::new();
    let (mut conn, _rx) = bare_conn(&hass);
    let held = super::template::take_all_renders().await;
    assert!(conn.handle_frame(
        &json!({"id": 1, "type": "render_template", "template": "{{ 1 }}"}).to_string()
    ));
    assert!(conn.busy.is_some());
    let pings: Vec<Value> = (2..=super::MAX_QUEUED as u64 + 1)
        .map(|id| json!({"id": id, "type": "ping"}))
        .collect();
    assert!(
        conn.handle_frame(&Value::Array(pings).to_string()),
        "up to the limit"
    );
    assert!(
        !conn.handle_frame(&json!({"id": 5000, "type": "ping"}).to_string()),
        "one more is too many"
    );
    drop(held);
}

/// The queue's byte limit counts what is waiting, not all that was ever
/// sent: a client that keeps a small backlog behind busy commands can send
/// any amount over time.
#[tokio::test]
async fn the_queue_counts_only_what_waits() {
    let _alone = RENDER_PERMITS.lock().await;
    let hass = Hass::new();
    let (mut conn, _rx) = bare_conn(&hass);
    let padding = "x".repeat(3 * 1024 * 1024);
    let frame = |id: u64| {
        json!([
            {"id": id, "type": "render_template", "template": "{{ 1 }}"},
            {"id": id + 1, "type": "ping", "pad": padding},
        ])
        .to_string()
    };
    // A render, then 3 MiB waiting behind it; the next frame comes in while
    // the queue still holds the previous one's tail, again and again: 18
    // MiB in all, never more than 6 MiB waiting.
    assert!(conn.handle_frame(&frame(1)));
    for id in (3..12).step_by(2) {
        assert!(conn.handle_frame(&frame(id)), "frame {id}");
        settle(&mut conn).await;
    }
    assert!(conn.queued_bytes < 7 * 1024 * 1024);
}

/// The tests that take or count render permits run one at a time.
static RENDER_PERMITS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// HA has no admin or permission check on `render_template`.
#[tokio::test]
async fn read_only_users_render_templates() {
    let _alone = RENDER_PERMITS.lock().await;
    let s = server().await;
    s.auth.add_user("reader", "pw").unwrap();
    let client = "http://127.0.0.1:8123/";
    let flow = s.auth.start_login_flow(client, None);
    let code = s
        .auth
        .finish_login_flow(
            &flow,
            client,
            &s.auth.check_password("reader", "pw").unwrap(),
            None,
        )
        .unwrap();
    let t = s
        .auth
        .exchange_code(Some(client), Some(&code), None)
        .unwrap();
    let id = s
        .auth
        .validate_access_token(&t.access_token, None)
        .unwrap()
        .user_id;
    s.auth.set_user_groups(&id, &[ha_auth::GROUP_ID_READ_ONLY]);
    let mut c = Client::authed(&s, &t.access_token).await;
    let r = c
        .call(json!({"id": 2, "type": "render_template", "template": "{{ 6 * 7 }}"}))
        .await;
    assert_eq!(r["success"], true);
    assert_eq!(c.recv().await["event"]["result"], 42);
}

/// A vector's value as compared: times, a category's id and the order of
/// labels (a set in HA) taken out, as `scripts/vectors/entity_registry_ws.py`
/// writes them.
fn registry_scrub(v: &Value, key: Option<&str>) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), registry_scrub(v, Some(k))))
                .collect(),
        ),
        Value::Array(items) if key == Some("labels") => {
            let mut items = items.clone();
            items.sort_by_key(|i| i.to_string());
            Value::Array(items)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|i| registry_scrub(i, None)).collect())
        }
        Value::Number(_) if matches!(key, Some("created_at" | "modified_at")) => json!("<ts>"),
        Value::String(s) if s.parse::<jiff::Timestamp>().is_ok() && s.contains('T') => {
            json!("<time>")
        }
        Value::String(s)
            if s.len() == 26
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase()) =>
        {
            json!("<ulid>")
        }
        other => other.clone(),
    }
}

/// Against `vectors/entity_registry.json`, made by
/// `scripts/vectors/entity_registry_ws.py` sending the messages to HA
/// 2026.6.1 over a WebSocket: `config/entity_registry/list`, `get`,
/// `get_entries` and `update` (the user's area, labels and categories) and
/// `config/category_registry/list`, from an admin and from a user. Each
/// reply, the `entity_registry_updated` events an update fires, and the
/// row as stored afterwards.
#[tokio::test]
async fn entity_registry_commands_match_ha() {
    let all: Value = serde_json::from_str(include_str!("vectors/entity_registry.json")).unwrap();
    let dir = std::env::temp_dir().join(format!("hars-ws-registry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".storage")).unwrap();
    for (key, minor) in [
        ("core.entity_registry", 22),
        ("core.device_registry", 12),
        ("core.category_registry", 2),
    ] {
        let store = json!({"version": 1, "minor_version": minor, "key": key,
                           "data": all["stores"][key]});
        std::fs::write(dir.join(".storage").join(key), store.to_string()).unwrap();
    }
    let registries = ha_registry::Registries::load(&dir).unwrap();
    let hass = Hass::new();
    let registry = ha_registry::Registry::in_memory(&hass, registries);
    let s = server_on(
        hass.clone(),
        crate::ApiConfig {
            registry: Some(registry.clone()),
            ..Default::default()
        },
    )
    .await;
    // A second user, in HA's `system-users` group.
    s.auth.add_user("user", "pw").unwrap();
    const CLIENT: &str = "http://127.0.0.1:8123/";
    let flow = s.auth.start_login_flow(CLIENT, None);
    let login = s.auth.check_password("user", "pw").unwrap();
    let code = s
        .auth
        .finish_login_flow(&flow, CLIENT, &login, None)
        .unwrap();
    let user = s
        .auth
        .exchange_code(Some(CLIENT), Some(&code), None)
        .unwrap()
        .access_token;
    let user_id = s.auth.validate_access_token(&user, None).unwrap().user_id;
    s.auth.set_user_groups(&user_id, &[ha_auth::GROUP_ID_USER]);
    let mut admin = Client::authed(&s, &token(&s.auth).0).await;
    let mut user = Client::authed(&s, &user).await;
    let mut bus = hass.bus().subscribe();
    let rows = |registry: &ha_registry::Registry| -> std::collections::BTreeMap<String, Value> {
        registry
            .snapshot()
            .entities
            .iter()
            .map(|e| (e.entity_id.clone(), serde_json::to_value(&**e).unwrap()))
            .collect()
    };
    let mut failures = Vec::new();
    for step in all["steps"].as_array().unwrap() {
        let before = rows(&registry);
        let client = match step["user"].as_str().unwrap() {
            "admin" => &mut admin,
            _ => &mut user,
        };
        let reply = client.call(step["ws"].clone()).await;
        let mut events = Vec::new();
        while let Ok(e) = bus.try_recv() {
            if let ha_core::EventKind::Custom { name, data } = &e.kind
                && name == "entity_registry_updated"
            {
                events.push(registry_scrub(data, None));
            }
        }
        let after = rows(&registry);
        let changed: serde_json::Map<String, Value> = after
            .iter()
            .filter(|(k, v)| before.get(*k) != Some(v))
            .map(|(k, v)| (k.clone(), registry_scrub(v, None)))
            .collect();
        let moved: serde_json::Map<String, Value> = changed
            .keys()
            .map(|k| {
                let stamps: Vec<&str> = ["created_at", "modified_at"]
                    .into_iter()
                    .filter(|s| after[k][s] != before[k][s])
                    .collect();
                (k.clone(), json!(stamps))
            })
            .collect();
        let mut got = json!({"reply": registry_scrub(&reply, None), "events": events});
        if !changed.is_empty() {
            got["rows"] = Value::Object(changed);
            got["moved"] = Value::Object(moved);
        }
        let mut want = registry_scrub(step, None);
        let want = want.as_object_mut().unwrap();
        want.remove("user");
        want.remove("ws");
        if got != Value::Object(want.clone()) {
            failures.push(format!(
                "{} {}\n  HA:   {}\n  hars: {got}",
                step["user"],
                step["ws"],
                Value::Object(want.clone())
            ));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        failures.is_empty(),
        "{} of {} steps differ from HA:\n{}",
        failures.len(),
        all["steps"].as_array().unwrap().len(),
        failures.join("\n")
    );
    // hars's own answers: no blueprints (for admins, as HA's command
    // is), and the fields it can't change.
    assert_eq!(
        admin
            .call(json!({"id": 900, "type": "blueprint/list", "domain": "automation"}))
            .await["result"],
        json!({})
    );
    assert_eq!(
        user.call(json!({"id": 900, "type": "blueprint/list", "domain": "automation"}))
            .await["error"]["code"],
        "unauthorized"
    );
    assert_eq!(
        admin
            .call(json!({"id": 902, "type": "blueprint/list"}))
            .await["error"]["message"],
        "required key not provided @ data['domain']. Got None"
    );
    assert_eq!(
        admin
            .call(json!({"id": 903, "type": "config/entity_registry/update",
                         "entity_id": "light.hall", "name": "New name"}))
            .await["error"],
        json!({"code": "not_supported", "message": "hars can't change an entity's name yet"})
    );
}

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

struct Server {
    url: String,
    hass: Hass,
    auth: Auth,
}

async fn server() -> Server {
    server_with(crate::ApiConfig {
        config_dir: "/config".into(),
        ..Default::default()
    })
    .await
}

async fn server_with(config: crate::ApiConfig) -> Server {
    let hass = Hass::new();
    hass.start();
    let auth = crate::test_util::auth(true);
    let app = crate::router(hass.clone(), auth.clone(), &config)
        .into_make_service_with_connect_info::<std::net::SocketAddr>();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/api/websocket", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { url, hass, auth }
}

struct Client {
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

    async fn send(&mut self, msg: Value) {
        self.ws
            .send(Message::Text(msg.to_string().into()))
            .await
            .unwrap();
    }

    /// The next message, unpacking coalesced arrays. Records whether each
    /// frame was an array.
    async fn recv(&mut self) -> Value {
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

    async fn authed(server: &Server, token: &str) -> Self {
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
fn token(auth: &Auth) -> (String, String) {
    const CLIENT: &str = "http://127.0.0.1:8123/";
    let flow = auth.start_login_flow(CLIENT, None);
    let user = auth.check_password("adam", "secret").unwrap();
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

    let me = c.call(json!({"id": 4, "type": "auth/current_user"})).await["result"].clone();
    assert_eq!(me["name"], "adam");
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
}

#[tokio::test]
async fn read_only_users_are_limited() {
    let s = server().await;
    s.auth.add_user("reader", "pw").unwrap();
    let flow = s.auth.start_login_flow("http://127.0.0.1:8123/", None);
    let code = s
        .auth
        .finish_login_flow(&flow, "http://127.0.0.1:8123/", "reader", None)
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
    };
    (conn, rx)
}

fn handle(conn: &mut super::Conn, msg: Value) {
    conn.handle(msg);
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
        registries: std::sync::Arc::new(registries),
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
        .finish_login_flow(&flow, "http://127.0.0.1:8123/", "reader", None)
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
    let autos = ha_automation::Automations::setup_yaml(&hass, &Default::default(), &items);
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
    let autos = ha_automation::Automations::setup_yaml(&hass, &Default::default(), &items);
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
        "person.adam",
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
    assert_eq!(keys(&c.recv().await), ["person.adam", "zone.home"]);

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
            "person.adam",
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

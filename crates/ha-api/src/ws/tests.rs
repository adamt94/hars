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
    let hass = Hass::new();
    hass.start();
    let auth = crate::test_util::auth(true);
    let config = crate::ApiConfig {
        config_dir: "/config".into(),
        ..Default::default()
    };
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
        let frame = tokio::time::timeout(Duration::from_secs(5), self.ws.next())
            .await
            .expect("no message within 5 s")
            .expect("stream ended")
            .unwrap();
        let Message::Text(text) = frame else {
            panic!("unexpected frame {frame:?}")
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
        err(json!(6), "invalid_format", "extra keys not allowed")
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
    assert!(event["time_fired"].is_string() && event["context"]["id"].is_string());
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

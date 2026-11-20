//! Against the conversations Home Assistant 2026.6.1 had with the same
//! requests (`vectors/flow.json`, made by `scripts/vectors/config_flow.py`):
//! each scenario's HTTP requests and WebSocket messages are sent to `hars`,
//! with a SwitchBot API on 127.0.0.1 answering as the generator's did, and
//! the replies, the subscriptions' events and the entries left are compared.
//!
//! What is left out of the comparison, where `hars` differs on purpose:
//! - the `updated` HA sends when its setup adds a webhook id to the entry
//!   (and that id in the stored entry): `hars` registers no webhook;
//! - which integrations `flow_handlers` lists: `hars` lists its own, which
//!   must be among HA's.

use crate::ws::tests::{Client, Kept, Server, server_on, token};
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::Request;
use ha_core::Hass;
use ha_integrations::Endpoints;
use ha_integrations::components::Components;
use ha_integrations::config_entries::ConfigEntries;
use ha_integrations::flow::Manager;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

const BOT: &str = "AABBCC000001";
const HUB: &str = "AABBCC000002";

/// The SwitchBot API of `scripts/vectors/config_flow.py` (`FakeApi`): the
/// recorded responses, or what `mode` says.
struct FakeApi {
    mode: parking_lot::Mutex<String>,
}

impl FakeApi {
    /// (status, content type, body); `None` closes the connection unanswered.
    fn reply(&self, path: &str) -> Option<(u16, &'static str, String)> {
        let json = |status, body: &str| Some((status, "application/json", body.to_owned()));
        let error = r#"{"statusCode":190,"body":{},"message":"error"}"#;
        match self.mode.lock().as_str() {
            "refused" => return None,
            "forbidden" => return json(403, r#"{"message":"Unauthorized"}"#),
            "http500" => return json(500, r#"{"message":"Internal server error"}"#),
            "status190" => return json(200, error),
            "offline" => return json(200, r#"{"statusCode":161,"body":{},"message":"offline"}"#),
            "not_json" => return Some((200, "text/html", "<html>down</html>".into())),
            "no_lists" => return json(200, r#"{"statusCode":100,"body":{},"message":"success"}"#),
            "status_down" if path.ends_with("/status") => return json(200, error),
            _ => {}
        }
        if path == "/v1.1/devices" {
            return json(
                200,
                include_str!("../../../ha-integrations/src/switchbot_cloud/vectors/devices.json"),
            );
        }
        if path == format!("/v1.1/devices/{BOT}/status") {
            return json(
                200,
                include_str!(
                    "../../../ha-integrations/src/switchbot_cloud/vectors/status_Bot.json"
                ),
            );
        }
        if path == format!("/v1.1/devices/{HUB}/status") {
            return json(
                200,
                include_str!(
                    "../../../ha-integrations/src/switchbot_cloud/vectors/status_Hub_2.json"
                ),
            );
        }
        json(404, error)
    }

    /// Listen on a loopback port; the base URL.
    async fn start() -> (String, Arc<Self>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let api = Arc::new(Self {
            mode: parking_lot::Mutex::new("ok".into()),
        });
        let served = api.clone();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let api = served.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        let mut chunk = [0u8; 4096];
                        match s.read(&mut chunk).await {
                            Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                            _ => return,
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).into_owned();
                    let path = head.split(' ').nth(1).unwrap_or_default();
                    let Some((status, content_type, body)) = api.reply(path) else {
                        return;
                    };
                    let res = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(res.as_bytes()).await;
                });
            }
        });
        (base, api)
    }
}

/// `hars` as a fresh install with config flows, its SwitchBot API faked.
struct Home {
    server: Server,
    config: crate::ApiConfig,
    api: Arc<FakeApi>,
    entries: Arc<ConfigEntries>,
    registry: Arc<ha_registry::Registry>,
    tokens: HashMap<&'static str, String>,
}

async fn home() -> Home {
    let hass = Hass::new();
    let registry = ha_registry::Registry::in_memory(&hass, Default::default());
    home_on(hass, registry).await
}

/// [`home`], on `registry`.
async fn home_on(hass: Hass, registry: Arc<ha_registry::Registry>) -> Home {
    let (base, api) = FakeApi::start().await;
    let entries = ConfigEntries::in_memory(&hass, Vec::new());
    let components = Components::new(&hass, registry.clone(), Default::default());
    let flows = Manager::new(
        &hass,
        entries.clone(),
        components.clone(),
        Endpoints { switchbot: base },
        Vec::new(),
        None,
    );
    let config = crate::ApiConfig {
        registry: Some(registry.clone()),
        entities: Some(components),
        config_entries: Some(entries.clone()),
        // None when a store can't be written.
        flows: flows.ok(),
        ..Default::default()
    };
    let server = server_on(hass, config.clone()).await;
    // A second user, in HA's `system-users` group.
    server.auth.add_user("user", "pw").unwrap();
    const CLIENT: &str = "http://127.0.0.1:8123/";
    let flow = server.auth.start_login_flow(CLIENT, None);
    let login = server.auth.check_password("user", "pw").unwrap();
    let code = server
        .auth
        .finish_login_flow(&flow, CLIENT, &login, None)
        .unwrap();
    let user = server
        .auth
        .exchange_code(Some(CLIENT), Some(&code), None)
        .unwrap()
        .access_token;
    let user_id = server
        .auth
        .validate_access_token(&user, None)
        .unwrap()
        .user_id;
    server
        .auth
        .set_user_groups(&user_id, &[ha_auth::GROUP_ID_USER]);
    let tokens = HashMap::from([("admin", token(&server.auth).0), ("user", user)]);
    Home {
        server,
        config,
        api,
        entries,
        registry,
        tokens,
    }
}

/// The generator's placeholders for the ids `hars` gave out, in the order
/// they first appear.
#[derive(Default)]
struct Ids {
    flows: Vec<String>,
    entries: Vec<String>,
    /// A SwitchBot device's registry id, and `<device:IDENTIFIER>`.
    devices: Vec<(String, String)>,
}

impl Ids {
    fn learn(&mut self, v: &Value) {
        match v {
            Value::Object(m) => {
                for (key, table) in [
                    ("flow_id", &mut self.flows),
                    ("entry_id", &mut self.entries),
                ] {
                    if let Some(Value::String(id)) = m.get(key)
                        && !table.contains(id)
                    {
                        table.push(id.clone());
                    }
                }
                m.values().for_each(|v| self.learn(v));
            }
            Value::Array(a) => a.iter().for_each(|v| self.learn(v)),
            _ => {}
        }
    }

    /// The devices there are now, for the steps that name one.
    fn learn_devices(&mut self, registries: &ha_registry::Registries) {
        for d in &registries.devices {
            for i in &d.identifiers {
                let name = format!("<device:{}>", i[1].as_str().unwrap());
                if i[0] == "switchbot_cloud" && !self.devices.iter().any(|(id, _)| *id == d.id) {
                    self.devices.push((d.id.clone(), name));
                }
            }
        }
    }

    /// A step's path or message with the real ids.
    fn real(&self, path: &str) -> String {
        let mut path = path.to_owned();
        for (kind, table) in [("flow", &self.flows), ("entry", &self.entries)] {
            for (i, id) in table.iter().enumerate() {
                path = path.replace(&format!("<{kind}:{}>", i + 1), id);
            }
        }
        for (id, name) in &self.devices {
            path = path.replace(name, id);
        }
        path
    }

    /// As the generator writes a value: ids as placeholders, times as
    /// `<ts>` and `<time>`.
    fn plain(&self, v: &Value, key: Option<&str>) -> Value {
        match v {
            Value::Object(m) => Value::Object(
                m.iter()
                    .map(|(k, v)| {
                        // An id may be a key (a device's entries).
                        let key = match self.plain(&json!(k), None) {
                            Value::String(key) => key,
                            _ => k.clone(),
                        };
                        (key, self.plain(v, Some(k)))
                    })
                    .collect(),
            ),
            Value::Array(a) => Value::Array(a.iter().map(|v| self.plain(v, None)).collect()),
            Value::Number(n) if n.is_f64() && matches!(key, Some("created_at" | "modified_at")) => {
                json!("<ts>")
            }
            Value::String(_) if matches!(key, Some("created_at" | "modified_at")) => {
                json!("<time>")
            }
            Value::String(s) => {
                let mut s = s.clone();
                for (kind, table) in [("flow", &self.flows), ("entry", &self.entries)] {
                    for (i, id) in table.iter().enumerate() {
                        s = s.replace(id, &format!("<{kind}:{}>", i + 1));
                    }
                }
                for (id, name) in &self.devices {
                    s = s.replace(id, name);
                }
                // Any other registry id.
                if s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
                    s = "<id>".into();
                }
                Value::String(s)
            }
            v => v.clone(),
        }
    }
}

/// A WebSocket with the generator's id counter, kept read between its
/// uses (a replay opens several and leaves each alone for many steps, far
/// longer than the server's heartbeat on a slow machine).
struct Socket {
    client: Kept,
    next: u64,
    /// Where the test is (a scenario and step), for a failure's message.
    at: String,
}

impl Socket {
    async fn open(home: &Home, who: &str) -> Self {
        Self {
            client: Client::authed(&home.server, &home.tokens[who]).await.kept(),
            next: 1,
            at: format!("{who}'s socket"),
        }
    }

    /// Send, and read up to the command's result (or pong).
    async fn call(&mut self, msg: &Value) -> Vec<Value> {
        let mut sent = msg.as_object().unwrap().clone();
        sent.insert("id".into(), json!(self.next));
        self.next += 1;
        self.client.send(Value::Object(sent), &self.at);
        let mut replies = Vec::new();
        loop {
            let reply = self.client.recv(&self.at).await;
            let done = matches!(reply["type"].as_str(), Some("result" | "pong"));
            replies.push(reply);
            if done {
                return replies;
            }
        }
    }
}

/// HA's `updated` for the webhook id its setup adds to the entry's data:
/// the only one sent mid-setup once the integration is loaded.
fn webhook_update(event: &Value) -> bool {
    event["event"].as_array().is_some_and(|changes| {
        changes.iter().all(|c| {
            c["type"] == "updated"
                && c["entry"]["state"] == "setup_in_progress"
                && c["entry"]["supports_unload"] == true
        })
    })
}

/// A subscription's event as the generator writes it (`event_of`): a
/// `subscribe_events` one cut down to its type and data, a state to its
/// text and attributes.
fn event_of(event: &Value) -> Value {
    let Some(kind) = event.get("event_type") else {
        return event.clone();
    };
    let mut data = event["data"].clone();
    if kind == "state_changed" {
        let state = |s: &Value| match s {
            Value::Null => Value::Null,
            s => json!({"state": s["state"], "attributes": s["attributes"]}),
        };
        data = json!({"entity_id": data["entity_id"], "old_state": state(&data["old_state"]),
                      "new_state": state(&data["new_state"])});
    }
    json!({"event_type": kind, "data": data})
}

/// The registry row or entity a `subscribe_events` event is about
/// (`by_row` in `device_step.py`).
fn by_row(event: &Value) -> String {
    let data = &event["event"]["data"];
    ["entity_id", "device_id", "area_id"]
        .iter()
        .find_map(|k| data.get(k).and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned()
}

async fn replay(scenario: &Value) {
    replay_slowly(scenario, std::time::Duration::ZERO).await;
}

/// [`replay`], on a machine that takes `pause` to get from opening the
/// sockets to the first step.
async fn replay_slowly(scenario: &Value, pause: std::time::Duration) {
    let name = scenario["name"].as_str().unwrap();
    // `device_step.py`'s scenarios: events sorted by row within a step,
    // and the registries and states compared at the end.
    let rows = scenario.get("devices").is_some();
    let home = home().await;
    let router = crate::router(
        home.server.hass.clone(),
        home.server.auth.clone(),
        &home.config,
    );
    let mut ids = Ids::default();
    let mut sockets = HashMap::new();
    for who in ["admin", "user"] {
        sockets.insert(who, Socket::open(&home, who).await);
    }
    // Each subscription on a socket of its own.
    let mut subs = Vec::new();
    for sub in scenario["subscriptions"].as_array().unwrap() {
        let mut socket = Socket::open(&home, "admin").await;
        let mut msg = sub["msg"].as_object().unwrap().clone();
        msg.insert("id".into(), json!(1));
        socket.at = format!("{name}: subscribing to {}", sub["sub"]);
        socket.client.send(Value::Object(msg), &socket.at);
        let mut first = Vec::new();
        for _ in sub["first"].as_array().unwrap() {
            first.push(socket.client.recv(&socket.at).await);
        }
        assert_eq!(json!(first), sub["first"], "{name}: {}", sub["sub"]);
        socket.next = 2;
        subs.push((sub["sub"].as_str().unwrap(), socket));
    }
    tokio::time::sleep(pause).await;
    for (n, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
        let at = format!("{name}, step {n}: {step:.300}");
        // A socket that fails says where the replay was.
        for socket in sockets
            .values_mut()
            .chain(subs.iter_mut().map(|s| &mut s.1))
        {
            socket.at.clone_from(&at);
        }
        if let Some(mode) = step["api"].as_str() {
            *home.api.mode.lock() = mode.to_owned();
            continue;
        }
        let request = |method: &str, path: &str, user: Option<&str>, body: Option<&str>| {
            let mut req = Request::builder().method(method).uri(path);
            if let Some(user) = user {
                req = req.header("authorization", format!("Bearer {}", home.tokens[user]));
            }
            let body = body.map_or_else(Body::empty, |b| Body::from(b.to_owned()));
            let mut req = req.body(body).unwrap();
            req.extensions_mut()
                .insert(ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 1))));
            let router = router.clone();
            async move {
                let res = router.oneshot(req).await.unwrap();
                let header = |name: &str| {
                    res.headers()
                        .get(name)
                        .map(|v| json!(v.to_str().unwrap()))
                        .unwrap_or(Value::Null)
                };
                let (status, content_type, allow) = (
                    res.status().as_u16(),
                    header("content-type"),
                    header("allow"),
                );
                let body = axum::body::to_bytes(res.into_body(), 1 << 20)
                    .await
                    .unwrap();
                (
                    status,
                    content_type,
                    allow,
                    String::from_utf8(body.to_vec()).unwrap(),
                )
            }
        };
        if let Some(query) = step["handlers"].as_str() {
            let path = format!("{}_handlers{query}", super::FLOW);
            let (status, _, _, body) = request("GET", &path, Some("admin"), None).await;
            assert_eq!(status, step["status"], "{at}");
            if status == 200 {
                // `hars` lists its own flows, which HA has too.
                let listed: Vec<String> = serde_json::from_str(&body).unwrap();
                let want: &[&str] = match query {
                    "?type=helper" => &[],
                    _ => &["switchbot_cloud"],
                };
                assert_eq!(listed, want, "{at}");
                let known = step["known"].as_array().unwrap();
                assert!(listed.iter().all(|d| known.contains(&json!(d))), "{at}");
            }
            continue;
        }
        if let Some(sent) = step.get("ws") {
            let who = sent["user"].as_str().unwrap();
            let socket = sockets.get_mut(who).unwrap();
            let msg: Value = serde_json::from_str(&ids.real(&sent["msg"].to_string())).unwrap();
            let mut replies = socket.call(&msg).await;
            ids.learn(&json!(replies));
            ids.learn_devices(&home.registry.snapshot());
            for reply in &mut replies {
                reply.as_object_mut().unwrap().remove("id");
            }
            // The device and entity lists, sorted as the generator's.
            let sorted: Option<fn(&Value) -> String> = match msg["type"].as_str().unwrap() {
                "config/device_registry/list" => Some(|d| d["identifiers"].to_string()),
                "config/entity_registry/list" => Some(|e| e["entity_id"].to_string()),
                _ => None,
            };
            let list = replies[0].get_mut("result").and_then(Value::as_array_mut);
            if let (Some(key), Some(list)) = (sorted, list) {
                list.sort_by_key(key);
            }
            let got = ids.plain(&json!(replies), None);
            assert_eq!(got, step["replies"], "{at}");
            // The generator closes a subscription a step opened.
            let subscribed = sent["msg"]["type"].as_str().unwrap().ends_with("subscribe")
                && replies.last().unwrap()["success"] == true;
            if subscribed {
                let id = socket.next - 1;
                socket
                    .call(&json!({"type": "unsubscribe_events", "subscription": id}))
                    .await;
            }
        } else {
            let http = &step["http"];
            let path = ids.real(http["path"].as_str().unwrap());
            let (status, content_type, allow, body) = request(
                http["method"].as_str().unwrap(),
                &path,
                http["user"].as_str(),
                http["sent"].as_str(),
            )
            .await;
            assert_eq!(status, step["status"], "{at}\n{body}");
            assert_eq!(content_type, step["content_type"], "{at}");
            assert_eq!(allow, step["allow"], "{at}");
            if let Some(known) = step.get("known") {
                let listed: Vec<String> = serde_json::from_str(&body).unwrap();
                assert_eq!(listed, ["switchbot_cloud"], "{at}");
                assert!(
                    known
                        .as_array()
                        .unwrap()
                        .contains(&json!("switchbot_cloud"))
                );
            } else if let Some(want) = step.get("json") {
                let got: Value = serde_json::from_str(&body).unwrap();
                ids.learn(&got);
                assert_eq!(ids.plain(&got, None), *want, "{at}");
            } else {
                assert_eq!(json!(body), step["text"], "{at}");
            }
        }
        ids.learn_devices(&home.registry.snapshot());
        let want: Vec<&Value> = step["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| !webhook_update(e))
            .collect();
        // What the subscriptions got: as many as HA sent (an event of the
        // bus may still be on its way), then everything before a pong.
        let mut events = Vec::new();
        for (sub, socket) in &mut subs {
            let expected = want.iter().filter(|e| e["sub"] == *sub).count();
            let mut got = Vec::new();
            for _ in 0..expected {
                let wait = std::time::Duration::from_secs(5);
                match socket.client.recv_within(wait, &at).await {
                    Some(event) => got.push(event),
                    None => panic!("{at}\n{sub}: {expected} events expected, got {got:#?}"),
                }
            }
            let mut rest = socket.call(&json!({"type": "ping"})).await;
            rest.pop();
            got.extend(rest);
            for event in got {
                ids.learn(&event);
                let event = ids.plain(&event_of(&event["event"]), None);
                events.push(json!({"sub": sub, "event": event}));
            }
        }
        if rows {
            // Each subscription's events by row, a row's own in order.
            let order = |e: &Value| subs.iter().position(|(sub, _)| e["sub"] == *sub);
            events.sort_by_key(|e| (order(e), by_row(e)));
        }
        assert_eq!(json!(events), json!(want), "{at}");
    }
    // The entries as stored, but for the webhook id `hars` never adds.
    let mut want = scenario["entries"].clone();
    for entry in want.as_array_mut().unwrap() {
        entry["data"].as_object_mut().unwrap().remove("webhook_id");
    }
    let stored: Vec<Map<String, Value>> = home.entries.list().unwrap();
    assert_eq!(ids.plain(&json!(stored), None), want, "{name}: entries");
    let registries = home.registry.snapshot();
    assert_eq!(
        json!(registries.device_file.deleted.len()),
        scenario["deleted_devices"],
        "{name}"
    );
    assert_eq!(
        json!(registries.entity_file.deleted.len()),
        scenario["deleted_entities"],
        "{name}"
    );
    if !rows {
        return;
    }
    // The registries as stored, and the registered entities' states.
    ids.learn_devices(&registries);
    let mut devices: Vec<Value> = registries.devices.iter().map(|d| json!(d)).collect();
    devices.sort_by_key(|d| d["identifiers"].to_string());
    assert_eq!(
        ids.plain(&json!(devices), None),
        scenario["devices"],
        "{name}: devices"
    );
    let mut entities: Vec<Value> = registries.entities.iter().map(|e| json!(e)).collect();
    entities.sort_by_key(|e| e["entity_id"].to_string());
    assert_eq!(
        ids.plain(&json!(entities), None),
        scenario["entities"],
        "{name}: entities"
    );
    assert_eq!(
        ids.plain(&json!(registries.areas), None),
        scenario["areas"],
        "{name}: areas"
    );
    let states: Map<String, Value> = entities
        .iter()
        .filter_map(|e| {
            let id = ha_core::EntityId::parse(e["entity_id"].as_str()?).ok()?;
            let state = home.server.hass.states().get(&id)?;
            let state = json!({"state": state.state, "attributes": state.attributes});
            Some((id.to_string(), state))
        })
        .collect();
    assert_eq!(json!(states), scenario["states"], "{name}: states");
    // And no state is left under an id that has no row (an old id).
    let all = home.server.hass.states().all();
    let orphans: Vec<&str> = all
        .iter()
        .map(|s| s.entity_id.as_str())
        .filter(|id| !states.contains_key(*id))
        .collect();
    assert!(
        orphans.is_empty(),
        "{name}: states with no row: {orphans:?}"
    );
}

fn scenarios() -> Vec<Value> {
    let all: Value = serde_json::from_str(include_str!("vectors/flow.json")).unwrap();
    all["scenarios"].as_array().unwrap().clone()
}

/// Every scenario HA ran: the dialog start to finish, bad credentials, an
/// API that is down, duplicates, bad input, flows that don't exist, a
/// closed dialog, who may do what, a setup that must retry, removing an
/// entry (one that runs, and one waiting for its retry).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flows_answer_as_home_assistant() {
    let scenarios = scenarios();
    assert_eq!(scenarios.len(), 13);
    for scenario in &scenarios {
        replay(scenario).await;
    }
}

/// A replay's sockets wait their turn for longer than the server's
/// heartbeat allows a silent client (200 ms in test builds, dropped after
/// two) whenever the machine is slow: CI lost one that way, with a broken
/// pipe at its next use. They are read all the while, so the replay gets
/// through however slow.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replay_outlasts_the_heartbeat() {
    let pause = std::time::Duration::from_millis(900);
    replay_slowly(&scenarios()[0], pause).await;
    let all: Value = serde_json::from_str(include_str!("vectors/device_step.json")).unwrap();
    replay_slowly(&all["scenarios"][0], pause).await;
}

/// The dialog's last step and the device page's dialog, as HA 2026.6.1
/// answered them (`vectors/device_step.json`): a new area, a device's
/// name, area and labels, the entity ids that follow from the names and
/// an entity given one, with every registry and state event on the way
/// and the rows and states left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn devices_and_areas_are_edited_as_home_assistant() {
    let all: Value = serde_json::from_str(include_str!("vectors/device_step.json")).unwrap();
    let scenarios = all["scenarios"].as_array().unwrap();
    assert_eq!(scenarios.len(), 4);
    for scenario in scenarios {
        replay(scenario).await;
    }
}

/// Without a flow manager (`ApiConfig::flows`), the views aren't there.
#[tokio::test]
async fn no_views_without_a_manager() {
    let auth = crate::test_util::auth(true);
    let token = token(&auth).0;
    let router = crate::router(Hass::new(), auth, &crate::ApiConfig::default());
    let mut req = Request::builder()
        .method("POST")
        .uri(super::FLOW)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(r#"{"handler":"switchbot_cloud"}"#))
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 1))));
    assert_eq!(router.oneshot(req).await.unwrap().status(), 404);
}

/// `integration/descriptions`: what HA says of the integrations `hars` has
/// a flow for (`ws/vectors/integration_descriptions.json`), each on its
/// own; and the dialog's names for them (`get_translations` with
/// `config_flow`), which are those not loaded yet.
#[tokio::test]
async fn the_dialog_lists_what_can_be_added() {
    let ha: Value =
        serde_json::from_str(include_str!("../ws/vectors/integration_descriptions.json")).unwrap();
    let home = home().await;
    let mut admin = Socket::open(&home, "admin").await;
    let reply = admin
        .call(&json!({"type": "integration/descriptions"}))
        .await;
    let result = &reply[0]["result"];
    for (part, keys) in ha["keys"].as_object().unwrap() {
        let got: Vec<&String> = result[part].as_object().unwrap().keys().collect();
        assert_eq!(json!(got), *keys);
    }
    assert_eq!(result["custom"], ha["custom"]);
    assert_eq!(
        result["core"]["integration"],
        json!({"switchbot_cloud": ha["known"]["switchbot_cloud"]["description"]})
    );
    assert_eq!(result["core"]["helper"], json!({}));
    assert_eq!(result["core"]["translated_name"], json!([]));
    // The generated table is HA's, for every integration it describes.
    for (domain, description, translated) in crate::manifests_data::DESCRIPTIONS {
        let got: Value = serde_json::from_str(description).unwrap();
        assert_eq!(got, ha["known"][domain]["description"], "{domain}");
        let listed = ha["translated_name_known"]
            .as_array()
            .unwrap()
            .contains(&json!(domain));
        assert_eq!(*translated, listed, "{domain}");
    }

    let titles = json!({"type": "frontend/get_translations", "language": "en",
                        "category": "title", "config_flow": true});
    let reply = admin.call(&titles).await;
    assert_eq!(
        reply[0]["result"]["resources"],
        json!({"component.switchbot_cloud.title": "SwitchBot Cloud"})
    );
    // The form's labels and errors, as the dialog asks for them.
    let reply = admin
        .call(
            &json!({"type": "frontend/get_translations", "language": "en",
                      "category": "config", "integration": ["switchbot_cloud"]}),
        )
        .await;
    let config = &reply[0]["result"]["resources"];
    assert_eq!(
        config["component.switchbot_cloud.config.step.user.data.api_token"],
        "API token"
    );
    assert_eq!(
        config["component.switchbot_cloud.config.error.invalid_auth"],
        "Invalid authentication"
    );
    assert_eq!(
        config["component.switchbot_cloud.config.abort.already_configured"],
        "Device is already configured"
    );
    // Once it runs, it is loaded: no longer among the flows to name.
    let flows = home.config.flows.clone().unwrap();
    let ha_integrations::flow::FlowResult::Form { flow_id, .. } = flows
        .init("switchbot_cloud", ha_integrations::flow::SOURCE_USER)
        .await
        .unwrap()
    else {
        panic!("no form");
    };
    let input = vec![
        ("api_token".to_owned(), json!("token-one")),
        ("api_key".to_owned(), json!("secret-one")),
    ];
    flows.configure(&flow_id, Some(input)).await.unwrap();
    let reply = admin.call(&titles).await;
    assert_eq!(reply[0]["result"]["resources"], json!({}));
    let reply = admin.call(&json!({"type": "get_config"})).await;
    let components = reply[0]["result"]["components"].as_array().unwrap();
    for domain in ["switchbot_cloud", "button", "climate", "sensor", "switch"] {
        assert!(components.contains(&json!(domain)), "{domain}");
    }
}

/// A `HEAD` to a flow or entry URL is a 405 for everyone, as in HA (the
/// `head` scenario of the vectors has each reply): nothing about a flow
/// can be told from it, and nothing runs. Elsewhere a `HEAD` needs what
/// the `GET` that would answer it needs.
#[tokio::test]
async fn a_head_request_learns_nothing() {
    let home = home().await;
    let router = crate::router(
        home.server.hass.clone(),
        home.server.auth.clone(),
        &home.config,
    );
    let send = |method: &str, path: String, user: Option<&str>| {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(user) = user {
            req = req.header("authorization", format!("Bearer {}", home.tokens[user]));
        }
        let mut req = req.body(Body::empty()).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 1))));
        let router = router.clone();
        async move { router.oneshot(req).await.unwrap().status().as_u16() }
    };
    let flows = home.config.flows.clone().unwrap();
    let ha_integrations::flow::FlowResult::Form { flow_id, .. } = flows
        .init("switchbot_cloud", ha_integrations::flow::SOURCE_USER)
        .await
        .unwrap()
    else {
        panic!("no form");
    };
    let known = format!("{}/{flow_id}", super::FLOW);
    let unknown = format!("{}/nope", super::FLOW);
    // What a `GET` answers, for comparison.
    assert_eq!(send("GET", known.clone(), Some("admin")).await, 200);
    assert_eq!(send("GET", unknown.clone(), Some("admin")).await, 404);
    assert_eq!(send("GET", known.clone(), Some("user")).await, 401);
    for path in [&known, &unknown] {
        for user in [Some("admin"), Some("user"), None] {
            assert_eq!(send("HEAD", path.clone(), user).await, 405, "{path}");
        }
    }
    assert_eq!(flows.in_progress(), 1, "a HEAD ran nothing");
    // A view from before this change: its `GET` needs an admin, so its
    // `HEAD` does.
    let automation = "/api/config/automation/config/nope".to_owned();
    assert_eq!(send("HEAD", automation, Some("user")).await, 401);
}

/// The one refusal HA doesn't have (every flow is at work, so none can
/// make room): "not now", in words the frontend's dialog shows as they
/// are.
#[tokio::test]
async fn a_full_house_is_refused_in_words() {
    let res = super::refusal(ha_integrations::flow::Error::Busy);
    assert_eq!(res.status(), 503);
    assert_eq!(res.headers()["content-type"], "text/plain; charset=utf-8");
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(
        body,
        "Too many integrations are being set up at once. Try again in a moment."
    );
}

/// When a store can't be written, adding and removing are off, and the
/// user is told why where they tried: the integration is still in the
/// dialog's list, starting its flow answers with the reason in words (the
/// frontend's dialog shows a text body as it is), and so does a delete.
#[tokio::test]
async fn an_unwritable_store_is_said_where_the_user_tries() {
    let why = ha_integrations::flow::Unwritable {
        key: "core.entity_registry",
    }
    .to_string();
    let hass = Hass::new();
    let entry = json!({"entry_id": "one", "domain": "switchbot_cloud", "title": "SwitchBot Cloud",
                       "disabled_by": null, "data": {}})
    .as_object()
    .unwrap()
    .clone();
    let entries = ConfigEntries::in_memory(&hass, vec![entry]);
    let config = crate::ApiConfig {
        config_entries: Some(entries.clone()),
        flows_unavailable: Some(why.clone().into()),
        ..Default::default()
    };
    let server = server_on(hass, config.clone()).await;
    let admin = token(&server.auth).0;
    let router = crate::router(server.hass.clone(), server.auth.clone(), &config);
    let send = |method: &str, path: &str, body: &str| {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {admin}"))
            .body(Body::from(body.to_owned()))
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 1))));
        let router = router.clone();
        async move {
            let res = router.oneshot(req).await.unwrap();
            let (status, content_type) = (
                res.status().as_u16(),
                res.headers()
                    .get("content-type")
                    .map(|v| v.to_str().unwrap().to_owned()),
            );
            let body = axum::body::to_bytes(res.into_body(), 1 << 20)
                .await
                .unwrap();
            (
                status,
                content_type,
                String::from_utf8(body.to_vec()).unwrap(),
            )
        }
    };
    let text = Some("text/plain; charset=utf-8".to_owned());
    assert_eq!(
        send("POST", super::FLOW, r#"{"handler":"switchbot_cloud"}"#).await,
        (400, text.clone(), why.clone())
    );
    assert_eq!(
        send("DELETE", "/api/config/config_entries/entry/one", "").await,
        (400, text, why.clone())
    );
    assert_eq!(entries.list().unwrap().len(), 1, "nothing was removed");
    // No flow exists, and the integration is still offered.
    assert_eq!(send("GET", &format!("{}/x", super::FLOW), "").await.0, 404);
    assert_eq!(
        send("GET", &format!("{}_handlers", super::FLOW), "")
            .await
            .2,
        r#"["switchbot_cloud"]"#
    );
    let mut ws = Client::authed(&server, &admin).await;
    ws.send(json!({"id": 1, "type": "integration/descriptions"}))
        .await;
    let reply = ws.recv().await;
    assert!(reply["result"]["core"]["integration"]["switchbot_cloud"].is_object());
}

/// SwitchBot Cloud added through its flow: the admin's socket.
async fn with_switchbot(home: &Home) -> Socket {
    use ha_integrations::flow::{FlowResult, SOURCE_USER};
    let flows = home.config.flows.clone().unwrap();
    let FlowResult::Form { flow_id, .. } =
        flows.init("switchbot_cloud", SOURCE_USER).await.unwrap()
    else {
        panic!("no form");
    };
    let input = vec![
        ("api_token".to_owned(), json!("token-one")),
        ("api_key".to_owned(), json!("secret-one")),
    ];
    let added = flows.configure(&flow_id, Some(input)).await.unwrap();
    assert!(matches!(added, FlowResult::CreateEntry { .. }), "{added:?}");
    Socket::open(home, "admin").await
}

/// The registries' rows, for "nothing changed".
fn rows_of(home: &Home) -> Value {
    let r = home.registry.snapshot();
    json!([r.devices, r.entities, r.areas])
}

/// What `hars`'s running entities can't follow is refused in words, and
/// changes nothing: disabling a device (HA stops its entities), a name for
/// a device with an entity of an integration that keeps the name it
/// started with, or with one whose own name carries the device's (HA
/// rewrites that name), and another id for such an entity. The same
/// device can still be moved and labelled. None of this is in HA.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_running_entities_cant_follow_is_refused() {
    let home = home().await;
    let mut admin = with_switchbot(&home).await;
    let device = |identifier: &str| {
        let r = home.registry.snapshot();
        let found = r.devices.iter().find(|d| d.identifiers[0][1] == identifier);
        found.unwrap().id.clone()
    };
    let refused = |replies: Vec<Value>| {
        let reply = replies.last().unwrap();
        assert_eq!(reply["error"]["code"], "not_supported", "{reply}");
        reply["error"]["message"].as_str().unwrap().to_owned()
    };
    // Another integration's device, with an entity named after it, and a
    // companion app's, whose entity carries the device's name in its own.
    for (domain, platform, has_entity_name, name) in [
        ("light", "hue", true, None),
        ("sensor", "mobile_app", false, Some("Phone Battery")),
    ] {
        let made = home.registry.get_or_create_device(ha_registry::DeviceInfo {
            config_entry_id: "other".into(),
            identifiers: vec![json!([platform, "1"])],
            name: Some("Phone".into()),
            ..Default::default()
        });
        let patch = ha_registry::EntityPatch {
            device_id: Some(Some(made.id.clone())),
            has_entity_name: Some(has_entity_name),
            original_name: Some(name.map(str::to_owned)),
            ..Default::default()
        };
        home.registry
            .get_or_create_entity(domain, platform, "1", None, patch);
    }
    let before = rows_of(&home);
    let update = |id: String, fields: Value| {
        let mut msg = json!({"type": "config/device_registry/update", "device_id": id});
        msg.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        msg
    };

    let bot = device(BOT);
    let said = refused(
        admin
            .call(&update(bot, json!({"disabled_by": "user"})))
            .await,
    );
    assert_eq!(said, "hars can't disable or enable a device yet");
    let hue = device("1");
    let said = refused(
        admin
            .call(&update(hue.clone(), json!({"name_by_user": "Lamp"})))
            .await,
    );
    assert_eq!(
        said,
        "hars can't rename a device of the hue integration yet"
    );
    let phone = home.registry.snapshot().devices[4].id.clone();
    let said = refused(
        admin
            .call(&update(
                phone,
                json!({"name_by_user": "Mine", "area_id": "hall"}),
            ))
            .await,
    );
    assert!(said.contains("carry its name"), "{said}");
    let msg = json!({"type": "config/entity_registry/update", "entity_id": "light.phone",
                     "new_entity_id": "light.lamp", "area_id": "hall"});
    let said = refused(admin.call(&msg).await);
    assert_eq!(
        said,
        "hars can't change the id of an entity of the hue integration yet"
    );
    assert_eq!(rows_of(&home), before, "a refused change changed something");

    // What needs no entity to follow is taken, for any device.
    let fields = json!({"name_by_user": null, "disabled_by": null, "area_id": "hall",
                        "labels": ["lamps"]});
    let replies = admin.call(&update(hue, fields)).await;
    assert_eq!(replies[0]["result"]["area_id"], "hall", "{replies:?}");
    assert_eq!(replies[0]["result"]["labels"], json!(["lamps"]));
}

/// A registry file `hars` won't write (another minor version's) refuses
/// what would change its rows, naming the file and the way out, and the
/// rows in memory stay as they are too: a change that would be gone after
/// a restart is not shown as made.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_that_cant_be_written_is_not_changed_in_memory() {
    let older = |key: &str| {
        let current = ha_registry::STORES.iter().find(|s| s.0 == key).unwrap().2;
        current - 1
    };
    let device = ha_registry::Device {
        id: "dev".into(),
        identifiers: vec![json!(["switchbot_cloud", BOT])],
        name: Some("Towel switch".into()),
        ..Default::default()
    };
    let entity = ha_registry::Entity {
        entity_id: "button.towel_switch".into(),
        platform: "switchbot_cloud".into(),
        has_entity_name: true,
        device_id: Some("dev".into()),
        ..Default::default()
    };
    let rest = |key: &str| ha_registry::FileRest {
        minor: older(key),
        ..Default::default()
    };
    let registries = ha_registry::Registries {
        devices: vec![Arc::new(device)],
        entities: vec![Arc::new(entity)],
        device_file: rest(ha_registry::DEVICE_KEY),
        entity_file: rest(ha_registry::ENTITY_KEY),
        area_minor: older(ha_registry::AREA_KEY),
        ..Default::default()
    };
    let dir = std::env::temp_dir().join(format!("hars-read-only-{}", std::process::id()));
    let hass = Hass::new();
    let registry = ha_registry::Registry::new(&hass, &dir, registries);
    let home = home_on(hass, registry).await;
    let mut admin = Socket::open(&home, "admin").await;
    let before = rows_of(&home);
    for (msg, key) in [
        (
            json!({"type": "config/device_registry/update", "device_id": "dev",
                   "name_by_user": "Rail", "area_id": "bath"}),
            ha_registry::DEVICE_KEY,
        ),
        (
            json!({"type": "config/area_registry/create", "name": "Bath"}),
            ha_registry::AREA_KEY,
        ),
        (
            json!({"type": "config/entity_registry/update", "entity_id": "button.towel_switch",
                   "area_id": "bath"}),
            ha_registry::ENTITY_KEY,
        ),
        (
            json!({"type": "config/entity_registry/update", "entity_id": "button.towel_switch",
                   "new_entity_id": "button.rail"}),
            ha_registry::ENTITY_KEY,
        ),
    ] {
        let replies = admin.call(&msg).await;
        let error = &replies[0]["error"];
        assert_eq!(error["code"], "home_assistant_error", "{msg}: {replies:?}");
        let said = error["message"].as_str().unwrap();
        assert!(said.contains(&format!(".storage/{key} ")), "{said}");
        assert!(said.contains("import the folder into hars again"), "{said}");
        assert_eq!(rows_of(&home), before, "{msg}");
    }
    // Reading still works.
    let ids = json!({"type": "config/entity_registry/get_automatic_entity_ids",
                     "entity_ids": ["button.towel_switch"]});
    let replies = admin.call(&ids).await;
    assert_eq!(
        replies[0]["result"],
        json!({"button.towel_switch": "button.towel_switch"})
    );
    assert!(!dir.exists(), "something was written");
}

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

use crate::ws::tests::{Client, Server, server_on, token};
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
    let (base, api) = FakeApi::start().await;
    let registry = ha_registry::Registry::in_memory(&hass, Default::default());
    let entries = ConfigEntries::in_memory(&hass, Vec::new());
    let components = Components::new(&hass, registry.clone(), Default::default());
    let flows = Manager::new(
        &hass,
        entries.clone(),
        components,
        Endpoints { switchbot: base },
        Vec::new(),
        None,
    )
    .unwrap();
    let config = crate::ApiConfig {
        registry: Some(registry.clone()),
        config_entries: Some(entries.clone()),
        flows: Some(flows),
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

    /// A step's path with the real ids.
    fn real(&self, path: &str) -> String {
        let mut path = path.to_owned();
        for (kind, table) in [("flow", &self.flows), ("entry", &self.entries)] {
            for (i, id) in table.iter().enumerate() {
                path = path.replace(&format!("<{kind}:{}>", i + 1), id);
            }
        }
        path
    }

    /// As the generator writes a value: ids as placeholders, times as
    /// `<ts>` and `<time>`.
    fn plain(&self, v: &Value, key: Option<&str>) -> Value {
        match v {
            Value::Object(m) => Value::Object(
                m.iter()
                    .map(|(k, v)| (k.clone(), self.plain(v, Some(k))))
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
                Value::String(s)
            }
            v => v.clone(),
        }
    }
}

/// A WebSocket with the generator's id counter.
struct Socket {
    client: Client,
    next: u64,
}

impl Socket {
    async fn open(home: &Home, who: &str) -> Self {
        Self {
            client: Client::authed(&home.server, &home.tokens[who]).await,
            next: 1,
        }
    }

    /// Send, and read up to the command's result (or pong).
    async fn call(&mut self, msg: &Value) -> Vec<Value> {
        let mut sent = msg.as_object().unwrap().clone();
        sent.insert("id".into(), json!(self.next));
        self.next += 1;
        self.client.send(Value::Object(sent)).await;
        let mut replies = Vec::new();
        loop {
            let reply = self.client.recv().await;
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

async fn replay(scenario: &Value) {
    let name = scenario["name"].as_str().unwrap();
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
        socket.client.send(Value::Object(msg)).await;
        let first = vec![socket.client.recv().await, socket.client.recv().await];
        assert_eq!(json!(first), sub["first"], "{name}: {}", sub["sub"]);
        socket.next = 2;
        subs.push((sub["sub"].as_str().unwrap(), socket));
    }
    for (n, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
        let at = format!("{name}, step {n}: {step:.300}");
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
            let mut replies = socket.call(&sent["msg"]).await;
            ids.learn(&json!(replies));
            for reply in &mut replies {
                reply.as_object_mut().unwrap().remove("id");
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
        // What the subscriptions got: everything before a pong.
        let mut events = Vec::new();
        for (sub, socket) in &mut subs {
            let mut got = socket.call(&json!({"type": "ping"})).await;
            got.pop();
            for event in got {
                ids.learn(&event);
                events.push(json!({"sub": sub, "event": ids.plain(&event["event"], None)}));
            }
        }
        let want: Vec<&Value> = step["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| !webhook_update(e))
            .collect();
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
    assert_eq!(scenarios.len(), 12);
    for scenario in &scenarios {
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

/// A `HEAD` is checked as the `GET` it would be answered by: a signed-in
/// user who isn't an admin can't tell a flow in progress from an unknown
/// id by the status, and nobody gets the form's handler run by a `HEAD`
/// (HA's view has no `head`: 405).
#[tokio::test]
async fn a_head_request_needs_what_a_get_needs() {
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
        assert_eq!(
            send("HEAD", path.clone(), Some("user")).await,
            401,
            "{path}"
        );
        assert_eq!(send("HEAD", path.clone(), None).await, 401, "{path}");
        assert_eq!(
            send("HEAD", path.clone(), Some("admin")).await,
            405,
            "{path}"
        );
    }
    // The other new views: nothing a `HEAD` gets that a `GET` wouldn't.
    assert_eq!(send("HEAD", super::FLOW.into(), Some("user")).await, 405);
    assert_eq!(
        send("HEAD", format!("{}_handlers", super::FLOW), Some("user")).await,
        405
    );
    assert_eq!(
        send("HEAD", format!("{}_handlers", super::FLOW), None).await,
        401
    );
    let entry = "/api/config/config_entries/entry/nope".to_owned();
    assert_eq!(send("HEAD", entry.clone(), Some("user")).await, 405);
    assert_eq!(send("HEAD", entry, None).await, 401);
    // An admin-only view from before this change is covered too.
    let automation = "/api/config/automation/config/nope".to_owned();
    assert_eq!(send("HEAD", automation, Some("user")).await, 401);
    assert_eq!(flows.in_progress(), 1, "a HEAD ran nothing");
}

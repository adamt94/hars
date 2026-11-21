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
/// recorded responses, or what `mode` says. And the Hue bridge and its
/// discovery service of `scripts/vectors/hue_flow.py` (`Hue`), which get
/// their requests as `/<scheme>/<host>/<path>` and `/discovery`, and keep
/// them as that script records what aiohue asked.
struct FakeApi {
    mode: parking_lot::Mutex<String>,
    hue: parking_lot::Mutex<HueMode>,
    requests: parking_lot::Mutex<Vec<Value>>,
    /// What the bridge says on its event stream, to whoever listens.
    events: tokio::sync::broadcast::Sender<String>,
}

/// `Hue.set` in `hue_flow.py`.
struct HueMode {
    pressed: bool,
    bridge: String,
    discovery: String,
}

const HUE_HOST: &str = "192.0.2.10";
/// Another address of the same bridge.
const HUE_ALIAS: &str = "192.0.2.12";
const HUE_KEY: &str = "fake-hue-application-key-0000000000000001";

/// What the fake answers: a status, content type and body; the event
/// stream, which is opened and left silent; or nothing (the connection is
/// closed, as where nothing listens).
enum Answer {
    Reply(u16, &'static str, String),
    Stream,
    Hang,
    Refuse,
}

impl FakeApi {
    /// An `api` step: how SwitchBot's API, the bridge or the discovery
    /// answers from here on.
    fn set(&self, mode: &str) {
        let mut hue = self.hue.lock();
        match mode {
            "pressed" | "not_pressed" => hue.pressed = mode == "pressed",
            m if m.starts_with("discovery_") => hue.discovery = m["discovery_".len()..].into(),
            m if m.starts_with("bridge_") => hue.bridge = m["bridge_".len()..].into(),
            _ => *self.mode.lock() = mode.to_owned(),
        }
    }

    /// The requests the bridge and discovery got since the last call.
    fn take(&self) -> Vec<Value> {
        std::mem::take(&mut *self.requests.lock())
    }

    fn hue(&self, method: &str, path: &str, key: Option<&str>, body: &[u8]) -> Answer {
        let json =
            |status, body: Value| Answer::Reply(status, "application/json", body.to_string());
        let hue = self.hue.lock();
        let note = |url: String| {
            let mut request = json!({"method": method.to_uppercase(), "url": url});
            if let Ok(sent) = serde_json::from_slice::<Value>(body) {
                request["json"] = sent;
            }
            if key.is_some() {
                request["key"] = json!(true);
            }
            self.requests.lock().push(request);
        };
        if path == "/discovery" {
            note("https://discovery.meethue.com/".into());
            let bridge =
                |id: &str, host: &str| json!({"id": id, "internalipaddress": host, "port": 443});
            return match hue.discovery.as_str() {
                "down" => Answer::Refuse,
                "slow" => Answer::Hang,
                "none" => json(200, json!([])),
                "two" => json(
                    200,
                    json!([
                        bridge("c42996fffe000001", HUE_HOST),
                        bridge("c42996fffe000002", "192.0.2.11")
                    ]),
                ),
                _ => json(200, json!([bridge("c42996fffe000001", HUE_HOST)])),
            };
        }
        // `/<scheme>/<host>/<path>`.
        let mut parts = path.splitn(4, '/').skip(1);
        let (scheme, host, rest) = (
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        );
        if rest.starts_with("eventstream") {
            return Answer::Stream;
        }
        note(format!("{scheme}://{host}/{rest}"));
        if ![HUE_HOST, HUE_ALIAS].contains(&host) || hue.bridge == "down" {
            return Answer::Refuse;
        }
        match (method, rest) {
            ("GET", "api/config") => {
                let mut config = json!({"name": "Hue Bridge", "datastoreversion": "172",
                    "swversion": "1971060010", "apiversion": "1.71.0",
                    "mac": "c4:29:96:00:00:01", "bridgeid": "C42996FFFE000001",
                    "factorynew": false, "replacesbridgeid": null, "modelid": "BSB003",
                    "starterkitid": ""});
                if hue.bridge == "not_hue" {
                    config.as_object_mut().unwrap().remove("bridgeid");
                }
                json(200, config)
            }
            ("POST", "api") if hue.bridge == "broken" => {
                Answer::Reply(500, "text/plain", "broken".into())
            }
            ("POST", "api") if !hue.pressed => json(
                200,
                json!([{"error": {"type": 101, "address": "",
                                  "description": "link button not pressed"}}]),
            ),
            ("POST", "api") => json(
                200,
                json!([{"success": {"username": HUE_KEY, "clientkey": "FAKECLIENTKEY"}}]),
            ),
            ("GET", "clip/v2/resource") => {
                if hue.bridge == "v1" {
                    return Answer::Reply(404, "text/plain", "Not found".into());
                }
                if key != Some(HUE_KEY) || hue.bridge == "key_refused" {
                    return json(
                        403,
                        json!({"errors": [{"description": "unauthorized user"}], "data": []}),
                    );
                }
                if hue.bridge == "resources_down" {
                    return Answer::Refuse;
                }
                if hue.bridge == "empty" {
                    return json(200, json!({"errors": [], "data": []}));
                }
                let resources =
                    include_str!("../../../ha-integrations/src/hue/vectors/resource.json");
                match hue.bridge.as_str() {
                    // Not said to be JSON; said to be and isn't.
                    "html" => Answer::Reply(200, "text/html", resources.to_owned()),
                    "garbled" => Answer::Reply(200, "application/json", "<html>".into()),
                    // A device has a `bridge` service, but of some other
                    // bridge resource than the one there is.
                    "other_bridge" => {
                        let mut all: Value = serde_json::from_str(resources).unwrap();
                        for r in all["data"].as_array_mut().unwrap() {
                            for service in r["services"].as_array_mut().into_iter().flatten() {
                                if service["rtype"] == "bridge" {
                                    service["rid"] = json!("00000000-0000-4000-8000-00000000bbbb");
                                }
                            }
                        }
                        Answer::Reply(200, "application/json", all.to_string())
                    }
                    _ => Answer::Reply(200, "application/json", resources.to_owned()),
                }
            }
            ("PUT", _) => json(200, json!({"errors": [], "data": []})),
            _ => json(
                404,
                json!({"errors": [{"description": "not found"}], "data": []}),
            ),
        }
    }

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
            hue: parking_lot::Mutex::new(HueMode {
                pressed: false,
                bridge: "ok".into(),
                discovery: "one".into(),
            }),
            requests: parking_lot::Mutex::default(),
            events: tokio::sync::broadcast::channel(16).0,
        });
        let served = api.clone();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let api = served.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let end = loop {
                        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break at + 4;
                        }
                        let mut chunk = [0u8; 4096];
                        match s.read(&mut chunk).await {
                            Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                            _ => return,
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                    let mut first = head.split(' ');
                    let method = first.next().unwrap_or_default();
                    let path = first.next().unwrap_or_default();
                    let header = |name: &str| {
                        head.lines().find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
                        })
                    };
                    let length: usize = header("content-length")
                        .and_then(|l| l.parse().ok())
                        .unwrap_or(0);
                    while buf.len() < end + length {
                        let mut chunk = [0u8; 4096];
                        match s.read(&mut chunk).await {
                            Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                            _ => return,
                        }
                    }
                    let answer = if path.starts_with("/v1.1/") {
                        match api.reply(path) {
                            Some((status, kind, body)) => Answer::Reply(status, kind, body),
                            None => Answer::Refuse,
                        }
                    } else {
                        let key = header("hue-application-key");
                        api.hue(method, path, key.as_deref(), &buf[end..])
                    };
                    let (status, content_type, body) = match answer {
                        Answer::Reply(status, kind, body) => (status, kind, body),
                        Answer::Refuse => return,
                        Answer::Hang => {
                            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                            return;
                        }
                        Answer::Stream => {
                            let mut events = api.events.subscribe();
                            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n: hi\n\n";
                            let _ = s.write_all(head.as_bytes()).await;
                            // Until the other side goes.
                            let mut rest = [0u8; 64];
                            loop {
                                tokio::select! {
                                    event = events.recv() => {
                                        let Ok(event) = event else { return };
                                        let line = format!("id: 1:0\ndata: {event}\n\n");
                                        if s.write_all(line.as_bytes()).await.is_err() {
                                            return;
                                        }
                                    }
                                    read = s.read(&mut rest) => {
                                        if !matches!(read, Ok(n) if n > 0) {
                                            return;
                                        }
                                    }
                                }
                            }
                        }
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
        Endpoints {
            switchbot: base.clone(),
            hue_discovery: format!("{base}/discovery"),
            // HA gives the discovery 5 s; a replay doesn't wait that long.
            hue_discovery_timeout: std::time::Duration::from_millis(400),
            hue_bridge: Some(base),
        },
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
            // The first of its identifiers, sorted (a set in HA).
            let mut idents: Vec<&str> = d
                .identifiers
                .iter()
                .filter(|i| i[0] == "switchbot_cloud" || i[0] == "hue")
                .filter_map(|i| i[1].as_str())
                .collect();
            idents.sort_unstable();
            if let Some(first) = idents.first()
                && !self.devices.iter().any(|(id, _)| *id == d.id)
            {
                self.devices
                    .push((d.id.clone(), format!("<device:{first}>")));
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
            home.api.set(mode);
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
                    _ => &["hue", "switchbot_cloud"],
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
            assert!(
                !got.to_string().contains(HUE_KEY),
                "{at}: the key is in a reply"
            );
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
            assert!(!body.contains(HUE_KEY), "{at}: the key is in a reply");
            assert_eq!(status, step["status"], "{at}\n{body}");
            assert_eq!(content_type, step["content_type"], "{at}");
            assert_eq!(allow, step["allow"], "{at}");
            if let Some(known) = step.get("known") {
                let listed: Vec<String> = serde_json::from_str(&body).unwrap();
                assert_eq!(listed, ["hue", "switchbot_cloud"], "{at}");
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
        // What the bridge and the discovery service were asked, as aiohue
        // asked them. After a set-up `hars` reads the resources once more
        // (its event stream's reconnect rule), at a moment of its own:
        // those requests are only counted on.
        if let Some(want) = step.get("requests") {
            let shape = |all: &[Value]| -> Vec<Value> {
                all.iter()
                    .map(|r| {
                        json!({"method": r["method"].as_str().unwrap().to_uppercase(),
                               "url": r["url"], "json": r.get("json"),
                               "key": r["key"] == true})
                    })
                    .collect()
            };
            let want = shape(want.as_array().unwrap());
            // `hars` reads the resources again whenever its event stream
            // connects, at a moment of its own (often in a later step):
            // only as many reads with the key as HA made count, where HA
            // made them. Any other request with the key (a command) is
            // compared like the rest.
            let read = |r: &Value| {
                r["key"] == true
                    && r["method"] == "GET"
                    && r["url"].as_str().unwrap().ends_with("/clip/v2/resource")
            };
            let mut reads = want.iter().filter(|r| read(r)).count();
            let mut got = shape(&home.api.take());
            got.retain(|r| {
                let read = read(r);
                let kept = !read || reads > 0;
                reads -= usize::from(read && reads > 0);
                kept
            });
            assert_eq!(got, want, "{at}: requests");
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
        assert!(
            !json!(events).to_string().contains(HUE_KEY),
            "{at}: the key is in a push"
        );
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
    for device in &mut devices {
        // Sets in HA, written sorted by the generator.
        for key in ["identifiers", "connections"] {
            let set = device[key].as_array_mut().unwrap();
            set.sort_by_key(Value::to_string);
        }
    }
    devices.sort_by_key(|d| d["identifiers"].to_string());
    same_rows(
        &ids.plain(&json!(devices), None),
        &scenario["devices"],
        &format!("{name}: devices"),
    );
    let mut entities: Vec<Value> = registries.entities.iter().map(|e| json!(e)).collect();
    entities.sort_by_key(|e| e["entity_id"].to_string());
    same_rows(
        &ids.plain(&json!(entities), None),
        &scenario["entities"],
        &format!("{name}: entities"),
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
            let mut attributes = json!(state.attributes);
            // Sets in HA, written sorted by `hue_flow.py`.
            for key in ["hue_scenes", "lights", "entity_id"] {
                if let Some(set) = attributes.get_mut(key).and_then(Value::as_array_mut) {
                    set.sort_by_key(Value::to_string);
                }
            }
            let state = json!({"state": state.state, "attributes": attributes});
            Some((id.to_string(), state))
        })
        .collect();
    for (id, want) in scenario["states"].as_object().unwrap() {
        assert_eq!(states.get(id), Some(want), "{name}: the state of {id}");
    }
    assert_eq!(
        states.len(),
        scenario["states"].as_object().unwrap().len(),
        "{name}: states"
    );
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

/// Two lists of registry rows are the same; if not, the first row that
/// differs is shown by the fields that do.
fn same_rows(got: &Value, want: &Value, what: &str) {
    let (got, want) = (got.as_array().unwrap(), want.as_array().unwrap());
    for (n, (g, w)) in got.iter().zip(want).enumerate() {
        if g == w {
            continue;
        }
        let keys: Vec<&String> = g
            .as_object()
            .into_iter()
            .flat_map(|o| o.keys())
            .chain(w.as_object().into_iter().flat_map(|o| o.keys()))
            .filter(|k| g.get(k.as_str()) != w.get(k.as_str()))
            .collect();
        let field = |row: &Value| -> Vec<String> {
            let mut seen = Vec::new();
            for k in &keys {
                let line = format!("{k}: {}", row.get(k.as_str()).unwrap_or(&Value::Null));
                if !seen.contains(&line) {
                    seen.push(line);
                }
            }
            seen
        };
        panic!(
            "{what}: row {n} ({} / {}) differs\n  hars: {:?}\n  HA:   {:?}",
            g.get("entity_id").or(g.get("name")).unwrap_or(&Value::Null),
            w.get("entity_id").or(w.get("name")).unwrap_or(&Value::Null),
            field(g),
            field(w)
        );
    }
    assert_eq!(got.len(), want.len(), "{what}: how many");
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

/// Adding a Hue bridge, as HA 2026.6.1 with aiohue 4.8.1 answered
/// (`vectors/hue_flow.json`): discovery, a host typed in, the link button,
/// each failure, a bridge that is set up already, two flows at once, a
/// bridge that can't be reached once it has given its key, and removal.
/// Every request the flow makes of the bridge and of the discovery service
/// is HA's, in HA's order; the first scenario's registry rows and states
/// are HA's too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hue_bridge_is_added_as_in_home_assistant() {
    let all: Value = serde_json::from_str(include_str!("vectors/hue_flow.json")).unwrap();
    let scenarios = all["scenarios"].as_array().unwrap();
    assert_eq!(scenarios.len(), 9);
    for scenario in scenarios {
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
        json!({"hue": ha["known"]["hue"]["description"],
               "switchbot_cloud": ha["known"]["switchbot_cloud"]["description"]})
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
        json!({"component.hue.title": "Philips Hue",
               "component.switchbot_cloud.title": "SwitchBot Cloud"})
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
    assert_eq!(
        reply[0]["result"]["resources"],
        json!({"component.hue.title": "Philips Hue"})
    );
    // Hue's dialog: HA's words, and `hars`'s own for a bridge it can't run.
    let hue = json!({"type": "frontend/get_translations", "language": "en",
                     "category": "config", "integration": ["hue"]});
    let reply = admin.call(&hue).await;
    let config = &reply[0]["result"]["resources"];
    assert_eq!(
        config["component.hue.config.error.register_failed"],
        "Failed to register, please try again"
    );
    assert_eq!(config["component.hue.config.step.link.title"], "Link Hub");
    let own = config["component.hue.config.abort.not_v2_bridge"].as_str();
    assert!(own.is_some_and(|text| text.contains("first version of the Hue API")));
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
        r#"["hue","switchbot_cloud"]"#
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

/// Hue's flow, by the manager's own calls: the result of each.
async fn hue_flow(
    flows: &Arc<Manager>,
    answers: &[Option<Value>],
) -> ha_integrations::flow::FlowResult {
    use ha_integrations::flow::{FlowResult, SOURCE_USER};
    let mut result = flows.init("hue", SOURCE_USER).await.unwrap();
    for answer in answers {
        let FlowResult::Form { flow_id, .. } = &result else {
            panic!("no form to answer: {result:?}");
        };
        let input = answer.as_ref().map(|a| {
            let fields = a.as_object().unwrap();
            fields.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        });
        result = flows.configure(flow_id, input).await.unwrap();
    }
    result
}

/// A bridge that only has the first version of the Hue API ends the flow,
/// found by discovery or typed in, where HA goes on to make a v1 entry:
/// `hars` has no v1 client, so that entry could never be set up. No entry
/// is made and the bridge is asked for no key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bridge_without_the_v2_api_makes_no_entry() {
    use ha_integrations::flow::FlowResult;
    let home = home().await;
    let flows = home.config.flows.clone().unwrap();
    home.api.set("bridge_v1");
    for answers in [
        vec![Some(json!({"id": "c42996000001"}))],
        vec![
            Some(json!({"id": "manual"})),
            Some(json!({"host": HUE_HOST})),
        ],
    ] {
        let result = hue_flow(&flows, &answers).await;
        let FlowResult::Abort { reason, .. } = result else {
            panic!("{result:?}");
        };
        assert_eq!(reason, "not_v2_bridge");
    }
    assert_eq!(home.entries.list(), Some(Vec::new()));
    let asked = home.api.take();
    assert!(
        asked.iter().all(|r| r["method"] == "GET"),
        "the bridge was asked for a key: {asked:?}"
    );
    assert_eq!(flows.in_progress(), 0);
}

/// An install that has the same bridge twice (an import can bring that),
/// the first entry disabled or ignored: the first is not set up, so it is
/// not in the second's way, and the second runs the bridge. HA's Hue
/// set-up looks for another entry only when its own id is stale.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_usable_entry_of_a_bridge_listed_twice_is_set_up() {
    use ha_integrations::config_entries::EntryState;
    let (base, _api) = FakeApi::start().await;
    let endpoints = Endpoints {
        hue_bridge: Some(base),
        ..Default::default()
    };
    for (why, source, disabled_by) in [
        ("disabled", "user", json!("user")),
        ("ignored", "ignore", json!(null)),
    ] {
        let hass = Hass::new();
        let entries = ConfigEntries::in_memory(&hass, Vec::new());
        let mut ids = Vec::new();
        for (source, disabled_by) in [(source, disabled_by), ("user", json!(null))] {
            let data = json!({"host": HUE_HOST, "api_key": HUE_KEY, "api_version": 2});
            let data = data.as_object().unwrap().clone();
            let mut entry =
                entries.new_entry("hue", source, "Hue Bridge", Some("c42996000001"), data);
            entry.insert("disabled_by".into(), disabled_by);
            ids.push(entry["entry_id"].as_str().unwrap().to_owned());
            entries.add(entry);
        }
        let registry = ha_registry::Registry::in_memory(&hass, Default::default());
        let lookup = entries.clone();
        registry.set_entry_lookup(Arc::new(move |id: &str| lookup.info(id)));
        let stored = entries.list();
        let _started = ha_integrations::start(
            &hass,
            registry.clone().into(),
            stored.as_deref(),
            Default::default(),
            &endpoints,
            Some(&entries),
        );
        hass.setups_done().await;
        assert_eq!(
            entries.runtime(&ids[0]),
            None,
            "{why}: the first was set up"
        );
        assert_eq!(
            entries.runtime(&ids[1]).map(|r| r.state),
            Some(EntryState::Loaded),
            "{why}: the second, usable entry was not set up"
        );
        let lamp = ha_core::EntityId::parse("light.living_room_lamp").unwrap();
        assert!(hass.states().get(&lamp).is_some(), "{why}");
        let rows = registry.snapshot();
        assert!(!rows.entities.is_empty());
        assert!(
            rows.entities
                .iter()
                .all(|e| e.config_entry_id.as_deref() == Some(ids[1].as_str())),
            "{why}"
        );
    }
}

/// An ignored entry is `not_loaded`, as in HA, though another entry of its
/// integration is set up and its integration so is loaded; and removing
/// it needs no restart, since nothing of it runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ignored_entry_is_not_loaded_and_goes_without_a_restart() {
    use ha_integrations::flow::FlowResult;
    let home = home().await;
    let flows = home.config.flows.clone().unwrap();
    home.api.set("pressed");
    let answers = [Some(json!({"id": "c42996000001"})), Some(json!({}))];
    let added = hue_flow(&flows, &answers).await;
    assert!(matches!(added, FlowResult::CreateEntry { .. }), "{added:?}");
    let ignored = home.entries.new_entry(
        "hue",
        "ignore",
        "Another bridge",
        Some("c42996000002"),
        Default::default(),
    );
    let id = ignored["entry_id"].as_str().unwrap().to_owned();
    home.entries.add(ignored);
    let mut socket = Socket::open(&home, "admin").await;
    let replies = socket.call(&json!({"type": "config_entries/get"})).await;
    let listed = replies.last().unwrap()["result"]
        .as_array()
        .unwrap()
        .clone();
    let state = |source: &str| {
        let entry = listed.iter().find(|e| e["source"] == source).unwrap();
        entry["state"].clone()
    };
    assert_eq!(state("user"), "loaded");
    assert_eq!(state("ignore"), "not_loaded");
    assert_eq!(flows.remove_entry(&id).await, Some(false));
    assert_eq!(home.entries.list().unwrap().len(), 1);
}

/// A bridge's whole life on one config directory, each start of `hars` in
/// a runtime of its own (so the one before has really stopped), against
/// a bridge that outlives them.
///
/// Added from the UI, it is there after a restart: the entry starts from
/// the files, its devices and entities have the rows and ids the flow's
/// set-up gave them (none is made or changed), its lights have their
/// states. A scene the bridge reports later has no registry row, and
/// still goes when the entry is removed: with no restart needed, no row,
/// no state and nothing that reaches the bridge is left.
///
/// A start-up says how each entry's set-up went: a bridge that can't be
/// reached is retried, one that refuses its key or whose resources name
/// no bridge has failed. An entry that never had a unique id takes its
/// bridge's; a second entry for the same bridge (another address of it)
/// is not started, and the first keeps every row. An ignored entry is
/// never set up.
#[test]
fn a_hue_bridge_from_adding_to_removing_across_restarts() {
    use ha_integrations::config_entries::EntryState;
    use ha_integrations::flow::FlowResult;
    let dir = std::env::temp_dir().join(format!("hars-hue-restart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".storage")).unwrap();
    let outer = tokio::runtime::Runtime::new().unwrap();
    let (base, api) = outer.block_on(FakeApi::start());
    let endpoints = Endpoints {
        hue_discovery: format!("{base}/discovery"),
        hue_bridge: Some(base.clone()),
        ..Default::default()
    };
    // `hars` started on the directory.
    let start = |hass: &Hass| {
        let entries = ConfigEntries::load(hass, &dir);
        let loaded = ha_registry::Registries::load(&dir).unwrap();
        let registry = ha_registry::Registry::new(hass, &dir, loaded);
        let lookup = entries.clone();
        registry.set_entry_lookup(Arc::new(move |id: &str| lookup.info(id)));
        let stored = entries.list();
        let started = ha_integrations::start(
            hass,
            registry.clone().into(),
            stored.as_deref(),
            Default::default(),
            &endpoints,
            Some(&entries),
        );
        let flows = Manager::new(
            hass,
            entries.clone(),
            started.components,
            endpoints.clone(),
            started.tasks,
            None,
        )
        .unwrap();
        (entries, registry, flows)
    };
    // One run of `hars`: everything it started ends with it.
    fn run<T>(life: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(life)
    }
    let rows = |registry: &ha_registry::Registry| {
        let r = registry.snapshot();
        json!([r.devices, r.entities, r.areas])
    };
    let state = |hass: &Hass, id: &str| {
        let id = ha_core::EntityId::parse(id).unwrap();
        hass.states()
            .get(&id)
            .map(|s| json!({"state": s.state, "name": s.attributes["friendly_name"]}))
    };
    let lamp = Some(json!({"state": "off", "name": "Lamp"}));
    let entry_of = |entries: &ConfigEntries, source: &str, host: &str, unique: Option<&str>| {
        let data = json!({"host": host, "api_key": HUE_KEY, "api_version": 2});
        let data = data.as_object().unwrap().clone();
        let entry = entries.new_entry("hue", source, "Hue Bridge", unique, data);
        let id = entry["entry_id"].as_str().unwrap().to_owned();
        entries.add(entry);
        id
    };

    // Added from the UI.
    api.set("pressed");
    let (entry_id, before) = run(async {
        let hass = Hass::new();
        let (entries, registry, flows) = start(&hass);
        let answers = [Some(json!({"id": "c42996000001"})), Some(json!({}))];
        let added = hue_flow(&flows, &answers).await;
        let FlowResult::CreateEntry { entry, .. } = added else {
            panic!("{added:?}");
        };
        assert_eq!(state(&hass, "light.living_room_lamp"), lamp);
        entries.flush().await;
        registry.flush().await;
        let before = rows(&registry);
        assert_eq!(before[0].as_array().unwrap().len(), 10);
        assert_eq!(before[1].as_array().unwrap().len(), 28);
        (entry["entry_id"].as_str().unwrap().to_owned(), before)
    });

    // A restart: the same rows and states. Then a scene the bridge only
    // now reports, and the entry's removal.
    run(async {
        let hass = Hass::new();
        let (entries, registry, flows) = start(&hass);
        hass.setups_done().await;
        assert_eq!(entries.list().unwrap().len(), 1);
        let loaded = entries.runtime(&entry_id).map(|r| r.state);
        assert_eq!(loaded, Some(EntryState::Loaded));
        assert_eq!(rows(&registry), before, "a restart changed a row");
        assert_eq!(state(&hass, "light.living_room_lamp"), lamp);
        assert!(state(&hass, "sensor.bedroom_bedroom_temperature").is_some());

        let resources: Value = serde_json::from_str(include_str!(
            "../../../ha-integrations/src/hue/vectors/resource.json"
        ))
        .unwrap();
        // A scene made in the Hue app after `hars` set the bridge up.
        let mut late = resources["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["type"] == "scene")
            .unwrap()
            .clone();
        late["id"] = json!("00000000-0000-4000-8000-00000000aaaa");
        late["metadata"]["name"] = json!("Late");
        let event = json!([{"creationtime": "2026-01-01T00:00:00Z", "id": "late",
                            "type": "add", "data": [late]}]);
        let before_late = hass.states().len();
        let mut waited = 0;
        while hass.states().len() == before_late {
            // Until the stream is up and the event has been read.
            let _ = api.events.send(event.to_string());
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            waited += 1;
            assert!(waited < 250, "the late scene never got a state");
        }
        assert_eq!(rows(&registry), before, "a late resource changed a row");
        // `hue.hue_activate_scene` reaches the bridge while it is set up.
        let scene = json!({"group_name": "bedroom", "scene_name": "dimmed"});
        let scene = scene.as_object().unwrap().clone();
        api.take();
        let activated = hass
            .call_service(
                "hue",
                "hue_activate_scene",
                scene.clone(),
                None,
                false,
                None,
            )
            .await;
        assert!(activated.is_ok(), "{activated:?}");
        assert!(api.take().iter().any(|r| r["method"] == "PUT"));

        assert_eq!(flows.remove_entry(&entry_id).await, Some(false));
        let left = registry.snapshot();
        assert!(left.devices.is_empty() && left.entities.is_empty());
        let states: Vec<String> = hass
            .states()
            .all()
            .iter()
            .map(|s| s.entity_id.to_string())
            .collect();
        assert!(states.is_empty(), "states were left: {states:?}");
        api.take();
        let on = json!({"entity_id": "all"});
        let on = on.as_object().unwrap().clone();
        let _ = hass
            .call_service("light", "turn_on", on, None, false, None)
            .await;
        let _ = hass
            .call_service("hue", "hue_activate_scene", scene, None, false, None)
            .await;
        assert!(
            api.take().is_empty(),
            "a removed light or scene still reached the bridge"
        );
        // An entry as an old import left it, with no unique id.
        entry_of(&entries, "user", HUE_HOST, None);
        entries.flush().await;
        registry.flush().await;
    });

    // How a start-up's set-up went is known, and the entry takes its
    // bridge's id.
    let first = run(async {
        let hass = Hass::new();
        let (entries, registry, _flows) = start(&hass);
        hass.setups_done().await;
        let first = entries.list().unwrap()[0].clone();
        let id = first["entry_id"].as_str().unwrap().to_owned();
        assert_eq!(first["unique_id"], "c42996000001");
        assert_eq!(
            entries.runtime(&id).map(|r| r.state),
            Some(EntryState::Loaded)
        );
        // And a second entry for the same bridge, at its other address,
        // that came with the bridge's id too. And one the user ignored,
        // as an import can bring it: with what it takes to be set up.
        entry_of(&entries, "user", HUE_ALIAS, Some("c42996000001"));
        let ignored = entry_of(&entries, "ignore", HUE_HOST, None);
        entries.flush().await;
        registry.flush().await;
        (id, ignored)
    });
    let (first, ignored) = first;
    let unreachable = "Error connecting to the Hue bridge at 192.0.2.10";
    for (mode, expected, reason) in [
        ("bridge_down", EntryState::SetupRetry, Some(unreachable)),
        // An answer that doesn't say it is JSON is tried again, as in HA.
        ("bridge_html", EntryState::SetupRetry, Some(unreachable)),
        ("bridge_key_refused", EntryState::SetupError, None),
        ("bridge_garbled", EntryState::SetupError, None),
        ("bridge_empty", EntryState::SetupError, None),
        ("bridge_other_bridge", EntryState::SetupError, None),
        ("bridge_ok", EntryState::Loaded, None),
    ] {
        api.set(mode);
        run(async {
            let hass = Hass::new();
            let (entries, registry, _flows) = start(&hass);
            hass.setups_done().await;
            let runtime = entries.runtime(&first).unwrap();
            assert_eq!(runtime.state, expected, "{mode}");
            assert_eq!(runtime.reason.as_deref(), reason, "{mode}");
            if mode != "bridge_ok" {
                return;
            }
            // An ignored entry is never set up.
            assert_eq!(entries.runtime(&ignored), None);
            // The other entry for this bridge was not started, and every
            // row is the first one's.
            let listed = entries.list().unwrap();
            let other = listed[1]["entry_id"].as_str().unwrap();
            assert_eq!(
                entries.runtime(other).map(|r| r.state),
                Some(EntryState::SetupError)
            );
            let r = registry.snapshot();
            assert_eq!(r.entities.len(), 28);
            assert!(
                r.entities
                    .iter()
                    .all(|e| e.config_entry_id.as_deref() == Some(first.as_str())),
                "the second entry took rows"
            );
            assert!(
                r.devices
                    .iter()
                    .all(|d| d.config_entries == [first.clone()])
            );
        });
    }
    let _ = std::fs::remove_dir_all(&dir);
}

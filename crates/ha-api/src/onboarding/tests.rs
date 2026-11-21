//! Onboarding against HA's own answers. `vectors/` holds what Home
//! Assistant 2026.6.1 answered to the onboarding pages' calls, made by
//! `scripts/vectors/onboarding.py`; these tests make the same calls to a
//! real `hars` server (and its stores on disk) and compare, with random and
//! time-dependent values replaced by `<kind>` placeholders on both sides.

use crate::ApiConfig;
use ha_auth::Auth;
use ha_core::Hass;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ----- placeholders, as the generator makes them -----

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_ulid(s: &str) -> bool {
    s.len() == 26
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b.is_ascii_uppercase() && !b"ILOU".contains(&b)))
}

fn is_jwt(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|p| {
            p.len() >= 20
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
}

/// `\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?\+00:00`.
fn is_time(s: &str) -> bool {
    let Some(t) = s.strip_suffix("+00:00") else {
        return false;
    };
    let shape = "dddd-dd-ddTdd:dd:dd";
    if t.len() < shape.len() {
        return false;
    }
    let (head, frac) = t.split_at(shape.len());
    let head_ok = head.bytes().zip(shape.bytes()).all(|(c, p)| match p {
        b'd' => c.is_ascii_digit(),
        _ => c == p,
    });
    let frac_ok = frac.is_empty()
        || (frac.len() > 1
            && frac.starts_with('.')
            && frac[1..].bytes().all(|b| b.is_ascii_digit()));
    head_ok && frac_ok
}

const TIME_KEYS: [&str; 5] = [
    "created_at",
    "modified_at",
    "last_used_at",
    "expire_at",
    "time_fired",
];

/// The generator's `scrub`.
fn scrub(v: &Value, key: Option<&str>) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), scrub(v, Some(k))))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(|v| scrub(v, None)).collect()),
        _ if key.is_some_and(|k| TIME_KEYS.contains(&k)) && !v.is_null() => json!("<time>"),
        _ if key == Some("password") => json!("<hash>"),
        Value::String(s) => {
            let kind = if is_jwt(s) {
                "jwt"
            } else if is_time(s) {
                "time"
            } else if is_ulid(s) {
                "ulid"
            } else if is_hex(s, 32) {
                "hex32"
            } else if is_hex(s, 128) {
                "hex128"
            } else {
                return v.clone();
            };
            json!(format!("<{kind}>"))
        }
        _ => v.clone(),
    }
}

// ----- local stand-ins for the internet -----

/// Answers each connection with `reply(request_head)`'s (status line,
/// content type, body), and records the request heads.
async fn fake_http(
    reply: impl Fn(&str) -> (&'static str, &'static str, Vec<u8>) + Send + Sync + 'static,
) -> (String, Arc<parking_lot::Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0; 8192];
            let n = s.read(&mut buf).await.unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).into_owned();
            let (status, content_type, body) = reply(&head);
            log.lock().push(head);
            let out = format!(
                "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(out.as_bytes()).await;
            let _ = s.write_all(&body).await;
        }
    });
    (format!("http://{addr}"), seen)
}

// ----- a hars on a config directory -----

struct Install {
    dir: PathBuf,
    addr: SocketAddr,
    hass: Hass,
    auth: Auth,
    registry: Arc<ha_registry::Registry>,
    components: Arc<ha_components::Components>,
    entries: Arc<ha_integrations::config_entries::ConfigEntries>,
    met_requests: Arc<parking_lot::Mutex<Vec<String>>>,
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hars-onboarding-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// What `main` starts, on `dir`: the stores loaded, `zone` and `person`
/// set up, met's onboarding flow pointed at a local met.no (a fixture
/// forecast) and `config/core/detect` at a local whoami.
async fn start(dir: &Path, whoami_up: bool) -> Install {
    let hass = Hass::new();
    let core = ha_core::config::CoreConfig::load(dir).unwrap();
    hass.set_config(core);
    let registries = ha_registry::Registries::load(dir).unwrap();
    let registry = ha_registry::Registry::new(&hass, dir, registries);
    let components = ha_components::Components::load(dir);
    components.setup(&hass, &registry.snapshot());
    let components = Arc::new(components);
    components.track(&hass);
    components.save_to(dir);
    let entries = ha_integrations::config_entries::ConfigEntries::load(&hass, dir);
    ha_integrations::import_default_entries(&entries);
    let forecast = include_bytes!("../../../ha-integrations/src/met/vectors/fixture_a.json");
    let (met_url, met_requests) =
        fake_http(move |_| ("200 OK", "application/json", forecast.to_vec())).await;
    let (whoami, _) = fake_http(move |_| {
        if whoami_up {
            let body = json!({
                "ip": "192.0.2.1", "country": "GB", "currency": "GBP", "region_code": "ENG",
                "region": "England", "city": "London", "postal_code": "SW1A",
                "timezone": "Europe/London", "latitude": "51.5", "longitude": "-0.12",
            });
            ("200 OK", "application/json", body.to_string().into_bytes())
        } else {
            ("503 Service Unavailable", "text/plain", b"down".to_vec())
        }
    })
    .await;
    // As `main`: sun runs (its entry was imported), met from onboarding.
    let started = ha_integrations::start(
        &hass,
        registry.snapshot().into(),
        entries.list().as_deref(),
        ha_integrations::light::Profiles::load(dir),
        &Default::default(),
        None,
    );
    let weather = started.weather.clone();
    let onboarding = ha_integrations::onboarding::Onboarding::new(
        &hass,
        registry.clone(),
        weather.clone(),
        entries.clone(),
        format!("{met_url}/complete"),
        false,
    );
    let auth = Auth::load_with_cost(dir, hass.clock().clone(), 4).unwrap();
    let config = ApiConfig {
        config_dir: dir.display().to_string(),
        storage: Some(dir.to_owned()),
        installation_type: "Unknown",
        whoami_url: Some(format!("{whoami}/whoami/v1")),
        analytics: Arc::new(crate::Analytics::load(dir)),
        config_entries: Some(entries.clone()),
        onboarding: Some(onboarding),
        registry: Some(registry.clone()),
        components: components.clone(),
        weather,
        integrations: started.running,
        ..Default::default()
    };
    hass.start();
    let app = crate::router(hass.clone(), auth.clone(), &config)
        .into_make_service_with_connect_info::<SocketAddr>();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Install {
        dir: dir.to_owned(),
        addr,
        hass,
        auth,
        registry,
        components,
        entries,
        met_requests,
    }
}

impl Install {
    /// Write what's waiting on a delay, as at stop.
    async fn flush(&self) {
        self.registry.flush().await;
        self.components.flush().await;
        self.entries.flush().await;
        if self.auth.needs_save() {
            self.auth.save().unwrap();
        }
    }

    fn store(&self, key: &str) -> Option<Value> {
        let text = std::fs::read_to_string(self.dir.join(".storage").join(key)).ok()?;
        Some(serde_json::from_str(&text).unwrap())
    }
}

/// One HTTP exchange: (status, content type, body as JSON or text).
async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&Value>,
    token: Option<&str>,
) -> (u16, String, Value) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let payload = match body {
        None => String::new(),
        Some(Value::String(raw)) => raw.clone(),
        Some(v) => v.to_string(),
    };
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nhost: {addr}\r\nconnection: close\r\ncontent-length: {}\r\n",
        payload.len()
    );
    if let Some(t) = token {
        head.push_str(&format!("authorization: Bearer {t}\r\n"));
    }
    head.push_str("\r\n");
    head.push_str(&payload);
    // A 404 may answer and close before the body is read: what arrived
    // before the reset is the answer.
    let _ = s.write_all(head.as_bytes()).await;
    let mut raw = Vec::new();
    let mut buf = [0; 4096];
    loop {
        match s.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(_) if !raw.is_empty() => break,
            Err(e) => panic!("{method} {path}: {e}"),
        }
    }
    let text = String::from_utf8(raw).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status: u16 = head.split(' ').nth(1).unwrap().parse().unwrap();
    let content_type = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-type")
                .then(|| v.trim().split(';').next().unwrap().to_owned())
        })
        .unwrap_or_default();
    // The tests' responses are small and never chunked.
    let body = serde_json::from_str(body).unwrap_or_else(|_| json!(body));
    (status, content_type, body)
}

async fn exchange_code(addr: SocketAddr, code: &str, client_id: &str) -> (u16, Value) {
    let form = format!(
        "grant_type=authorization_code&code={code}&client_id={}",
        serde_urlencoded::to_string([("x", client_id)]).unwrap()[2..].to_owned()
    );
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "POST /auth/token HTTP/1.1\r\nhost: {addr}\r\nconnection: close\r\ncontent-type: application/x-www-form-urlencoded\r\ncontent-length: {}\r\n\r\n",
        form.len()
    );
    s.write_all(head.as_bytes()).await.unwrap();
    s.write_all(form.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8(raw).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status: u16 = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap())
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ws_connect(addr: SocketAddr, token: &str) -> Ws {
    use futures_util::{SinkExt, StreamExt};
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/websocket"))
        .await
        .unwrap();
    ws.next().await.unwrap().unwrap();
    let auth = json!({"type": "auth", "access_token": token}).to_string();
    ws.send(tokio_tungstenite::tungstenite::Message::Text(auth.into()))
        .await
        .unwrap();
    let ok = ws.next().await.unwrap().unwrap();
    assert!(ok.to_text().unwrap().contains("auth_ok"), "{ok:?}");
    ws
}

async fn ws_call(ws: &mut Ws, msg: &Value, replies: usize) -> Vec<Value> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    ws.send(Message::Text(msg.to_string().into()))
        .await
        .unwrap();
    let mut got = Vec::new();
    while got.len() < replies {
        let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .unwrap_or_else(|_| panic!("no reply to {msg}"))
            .unwrap()
            .unwrap();
        if let Message::Text(t) = frame {
            got.push(serde_json::from_str(&t).unwrap());
        }
    }
    got
}

/// Replay a scenario's steps and compare each answer with HA's.
async fn replay(i: &Install, steps: &[Value]) {
    let mut token: Option<String> = None;
    let mut code: Option<String> = None;
    let mut ws: Option<Ws> = None;
    for step in steps {
        let note = step.to_string();
        if let Some(req) = step.get("http") {
            let auth = req["auth"].as_bool().unwrap();
            let (status, content_type, body) = http(
                i.addr,
                req["method"].as_str().unwrap(),
                req["path"].as_str().unwrap(),
                req.get("body"),
                if auth { token.as_deref() } else { None },
            )
            .await;
            assert_eq!(status, step["status"], "{note}");
            if status == 404 {
                // HA's aiohttp says "404: Not Found"; hars's router sends
                // no body (for every path, not only these).
                continue;
            }
            assert_eq!(content_type, step["content_type"], "{note}");
            assert_eq!(scrub(&body, None), step["body"], "{note}");
            if let Some(c) = body.get("auth_code").and_then(Value::as_str) {
                code = Some(c.to_owned());
            }
        } else if let Some(t) = step.get("token") {
            let client_id = t["client_id"].as_str().unwrap();
            let (status, body) = exchange_code(i.addr, code.as_deref().unwrap(), client_id).await;
            assert_eq!(status, step["status"], "{note}");
            assert_eq!(scrub(&body, None), step["body"], "{note}");
            token = body["access_token"].as_str().map(str::to_owned);
            ws = None;
        } else if let Some(msg) = step.get("ws") {
            if ws.is_none() {
                ws = Some(ws_connect(i.addr, token.as_deref().unwrap()).await);
            }
            let mut want = step["replies"].as_array().unwrap().clone();
            let mut got = ws_call(ws.as_mut().unwrap(), msg, want.len()).await;
            if msg["type"] == "config/core/update" {
                // HA's event reaches the subscriber before the result (one
                // event loop); in `hars` the bus listener is its own task,
                // so either may come first.
                let by_type = |v: &Value| v["type"].as_str().unwrap_or("").to_owned();
                got.sort_by_key(by_type);
                want.sort_by_key(by_type);
            }
            for (got, want) in got.iter().zip(&want) {
                let got = scrub(got, None);
                match msg["type"].as_str().unwrap() {
                    "get_config" => compare_config(&got, want, &note),
                    "config_entries/subscribe" if got["type"] == "event" => {
                        compare_entries(&got, want, &note)
                    }
                    _ => assert_eq!(&got, want, "{note}"),
                }
            }
        }
    }
}

/// `get_config`, but `components` (HA's harness loads other components)
/// and the directory.
fn compare_config(got: &Value, want: &Value, note: &str) {
    let strip = |v: &Value| {
        let mut v = v.clone();
        let r = v["result"].as_object_mut().unwrap();
        r.remove("components");
        r.insert("config_dir".into(), json!("<config_dir>"));
        v
    };
    assert_eq!(strip(got), strip(want), "{note}");
}

/// `config_entries/subscribe`'s list: HA's harness has shopping_list too,
/// and says met is `setup_retry` (it couldn't fetch); `hars` doesn't track
/// setup, so met's `state` and `reason` are left out.
fn compare_entries(got: &Value, want: &Value, note: &str) {
    let ours = |v: &Value| {
        let list = v["event"].as_array().unwrap();
        let mut ours: Vec<Value> = list
            .iter()
            .filter(|e| matches!(e["entry"]["domain"].as_str(), Some("sun" | "met")))
            .cloned()
            .collect();
        ours.sort_by_key(|e| e["entry"]["domain"].as_str().unwrap().to_owned());
        for e in &mut ours {
            if e["entry"]["domain"] == "met" {
                let entry = e["entry"].as_object_mut().unwrap();
                entry.remove("state");
                entry.remove("reason");
            }
        }
        ours
    };
    assert_eq!(ours(got), ours(want), "{note}");
    assert_eq!(ours(got).len(), 2, "{note}");
    assert_eq!(
        got["event"].as_array().unwrap().len(),
        2,
        "only sun and met in hars: {note}"
    );
}

/// The parts of a store `hars` writes for onboarding.
fn rows<'a>(store: &'a Value, list: &str) -> &'a Vec<Value> {
    store["data"][list].as_array().unwrap()
}

fn vector(name: &str) -> Value {
    let text = match name {
        "frontend" => include_str!("vectors/frontend.json"),
        "bad_redirect" => include_str!("vectors/bad_redirect.json"),
        _ => include_str!("vectors/restarted.json"),
    };
    serde_json::from_str(text).unwrap()
}

/// The onboarding pages' calls, in their order, with HA's answers; then
/// the states and the files HA leaves, and a restart.
#[tokio::test(flavor = "multi_thread")]
async fn the_frontends_onboarding_matches_ha() {
    let dir = temp_dir("frontend");
    let v = vector("frontend");
    let i = start(&dir, true).await;
    replay(&i, v["steps"].as_array().unwrap()).await;

    // States HA has at the end.
    for (entity_id, want) in v["states"].as_object().unwrap() {
        let state = i
            .hass
            .states()
            .get(&ha_core::EntityId::parse(entity_id).unwrap())
            .unwrap_or_else(|| panic!("{entity_id}"));
        let got = json!({"state": state.state, "attributes": state.attributes});
        assert_eq!(scrub(&got, None), *want, "{entity_id}");
    }
    // met's onboarding entry runs: a forecast for the home.
    let forecast = ha_core::EntityId::parse("weather.forecast_home").unwrap();
    for _ in 0..100 {
        if i.hass.states().get(&forecast).is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(i.hass.states().get(&forecast).is_some());
    let requests = i.met_requests.lock().clone();
    assert!(
        requests[0].starts_with("GET /complete?lat=51.5&lon=-0.12&altitude=0 "),
        "{requests:?}"
    );

    // The files, as HA writes them.
    i.flush().await;
    let stores = &v["stores"];
    for key in [
        "onboarding",
        "core.config",
        "core.analytics",
        "core.area_registry",
    ] {
        assert_eq!(scrub(&i.store(key).unwrap(), None), stores[key], "{key}");
    }
    for (key, text) in v["raw"].as_object().unwrap() {
        let got = std::fs::read_to_string(dir.join(".storage").join(key)).unwrap();
        // HA's `preferences` keep the client's key order; `hars` sorts
        // them (the same here).
        assert_eq!(&got, text.as_str().unwrap(), "{key} byte for byte");
    }
    let person = scrub(&i.store("person").unwrap(), None);
    assert_eq!(rows(&person, "items"), rows(&stores["person"], "items"));
    let owner = |auth: &Value| {
        let users = rows(auth, "users");
        let user = users
            .iter()
            .find(|u| u["is_owner"] == true)
            .unwrap()
            .clone();
        let creds = rows(auth, "credentials").clone();
        (user, creds)
    };
    assert_eq!(
        owner(&scrub(&i.store("auth").unwrap(), None)),
        owner(&stores["auth"])
    );
    let person_row = |r: &Value| {
        rows(r, "entities")
            .iter()
            .find(|e| e["platform"] == "person")
            .cloned()
    };
    assert_eq!(
        person_row(&scrub(&i.store("core.entity_registry").unwrap(), None)),
        person_row(&stores["core.entity_registry"])
    );
    // The entries `hars` has (HA's harness has analytics' and
    // shopping_list's too): sun's, imported at the first start, and met's.
    for domain in ["sun", "met"] {
        let entry = |r: &Value| {
            rows(r, "entries")
                .iter()
                .find(|e| e["domain"] == domain)
                .cloned()
        };
        let got = entry(&scrub(&i.store("core.config_entries").unwrap(), None));
        assert!(got.is_some(), "{domain}");
        assert_eq!(got, entry(&stores["core.config_entries"]), "{domain}");
    }

    // A restart on the same directory: the location stays, and the views
    // are gone.
    let again = start(&dir, true).await;
    let config = again.hass.config();
    assert_eq!(
        (config.latitude, config.longitude, config.time_zone.as_str()),
        (51.5, -0.12, "Europe/London")
    );
    assert_eq!(config.source, ha_core::config::ConfigSource::Storage);
    assert!(again.auth.is_onboarded());
    replay(&again, vector("restarted")["steps"].as_array().unwrap()).await;
    let _ = std::fs::remove_dir_all(dir);
}

/// The integration step is done before its redirect URI is checked; and
/// location detection that can't reach HA's service answers `{}`.
#[tokio::test(flavor = "multi_thread")]
async fn a_bad_redirect_still_finishes_the_step() {
    let dir = temp_dir("bad-redirect");
    let v = vector("bad_redirect");
    let i = start(&dir, false).await;
    replay(&i, v["steps"].as_array().unwrap()).await;
    i.flush().await;
    let stores = &v["stores"];
    for key in ["onboarding", "core.area_registry"] {
        assert_eq!(scrub(&i.store(key).unwrap(), None), stores[key], "{key}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// What the generator can't ask HA: the checks `hars` makes first, and
/// who may call what.
#[tokio::test(flavor = "multi_thread")]
async fn refusals_create_nothing() {
    let dir = temp_dir("refusals");
    let i = start(&dir, true).await;
    let user = |name: &str, username: &str| {
        json!({"client_id": "http://127.0.0.1:8123/", "name": name, "username": username,
               "password": "pw", "language": "en"})
    };
    // HA's person schema: an empty name is a 400.
    let r = http(
        i.addr,
        "POST",
        "/api/onboarding/users",
        Some(&user("", "a")),
        None,
    )
    .await;
    assert_eq!((r.0, r.2), (400, json!("400: Bad Request")));
    // HA's `InvalidUsername`, unhandled: a 500.
    let r = http(
        i.addr,
        "POST",
        "/api/onboarding/users",
        Some(&user("A", "Bob")),
        None,
    )
    .await;
    assert_eq!(
        (r.0, r.2),
        (
            500,
            json!("500 Internal Server Error\n\nServer got itself in trouble")
        )
    );
    assert!(!i.auth.is_step_done("user"));
    assert!(i.components.persons().is_empty());
    assert!(i.registry.snapshot().areas.is_empty());
    // A token is needed for the other steps.
    for step in ["core_config", "analytics", "integration"] {
        let path = format!("/api/onboarding/{step}");
        let r = http(i.addr, "POST", &path, Some(&json!({})), None).await;
        assert_eq!(r.0, 401, "{step}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// `hars auth add` (no browser) still finishes onboarding: the views are
/// gone and `/` serves the dashboard.
#[tokio::test(flavor = "multi_thread")]
async fn the_command_line_owner_skips_onboarding() {
    let dir = temp_dir("cli");
    {
        let auth = Auth::load_with_cost(&dir, ha_core::Clock::system(), 4).unwrap();
        assert_eq!(auth.add_user("alex", "pw").unwrap(), ha_auth::Added::Owner);
        auth.save().unwrap();
    }
    let i = start(&dir, true).await;
    for (method, path) in [
        ("GET", "/api/onboarding"),
        ("GET", "/api/onboarding/installation_type"),
        ("POST", "/api/onboarding/users"),
    ] {
        let r = http(i.addr, method, path, None, None).await;
        assert_eq!(r.0, 404, "{path}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn installation_type_follows_has_rules() {
    use super::installation_type_of as of;
    let none = |_: &str| false;
    assert_eq!(of(false, &none, false, true), "Unknown");
    // Not Linux: HA doesn't look for a container.
    assert_eq!(of(false, &|p| p == "/.dockerenv", false, true), "Unknown");
    for marker in ["/.dockerenv", "/run/.containerenv"] {
        let f = |p: &str| p == marker;
        assert_eq!(
            of(true, &f, false, true),
            "Unsupported Third Party Container"
        );
    }
    assert_eq!(
        of(true, &none, true, true),
        "Unsupported Third Party Container"
    );
    let official = |p: &str| p == "/OFFICIAL_IMAGE";
    assert_eq!(of(true, &official, false, true), "Home Assistant Container");
    assert_eq!(
        of(true, &official, false, false),
        "Unsupported Third Party Container"
    );
    assert_eq!(of(true, &none, false, true), "Unknown");
}

#[test]
fn default_area_names_fall_back_to_english() {
    assert_eq!(
        super::area_names("de"),
        ["Wohnzimmer", "Küche", "Schlafzimmer"]
    );
    assert_eq!(
        super::area_names("en-GB"),
        ["Living Room", "Kitchen", "Bedroom"]
    );
    assert_eq!(
        super::area_names("xx"),
        ["Living Room", "Kitchen", "Bedroom"]
    );
}

/// A login for `username`, through the auth code path the views use.
fn login(auth: &Auth, username: &str) -> (String, String) {
    const CLIENT: &str = "http://127.0.0.1:8123/";
    let flow = auth.start_login_flow(CLIENT, None);
    let user = auth.check_password(username, "pw").unwrap();
    let code = auth.finish_login_flow(&flow, CLIENT, &user, None).unwrap();
    let t = auth.exchange_code(Some(CLIENT), Some(&code), None).unwrap();
    let user_id = auth
        .validate_access_token(&t.access_token, None)
        .unwrap()
        .user_id;
    (t.access_token, user_id)
}

/// The settings commands are the admin's (`require_admin`), after the
/// schema; `config_entries/subscribe` is anyone's.
#[tokio::test(flavor = "multi_thread")]
async fn settings_commands_are_for_admins() {
    let dir = temp_dir("admins");
    let i = start(&dir, true).await;
    i.auth.create_owner("Owner", "owner", "pw").unwrap();
    i.auth.add_user("reader", "pw").unwrap();
    let (token, user_id) = login(&i.auth, "reader");
    assert!(i.auth.set_user_groups(&user_id, &[ha_auth::GROUP_ID_USER]));
    let mut ws = ws_connect(i.addr, &token).await;
    let mut id = 0;
    for msg in [
        json!({"type": "config/core/update", "latitude": 1}),
        json!({"type": "config/core/detect"}),
        json!({"type": "analytics"}),
        json!({"type": "analytics/preferences", "preferences": {"base": true}}),
        json!({"type": "config_entries/flow/subscribe"}),
    ] {
        id += 1;
        let mut msg = msg;
        msg["id"] = json!(id);
        let r = ws_call(&mut ws, &msg, 1).await;
        assert_eq!(
            r[0]["error"],
            json!({"code": "unauthorized", "message": "Unauthorized"}),
            "{msg}"
        );
    }
    // The schema first: a bad value is `invalid_format` even for them.
    let r = ws_call(
        &mut ws,
        &json!({"id": 10, "type": "config/core/update", "latitude": 100}),
        1,
    )
    .await;
    assert_eq!(r[0]["error"]["code"], "invalid_format");
    assert_eq!(i.hass.config().latitude, 0.0);
    let r = ws_call(
        &mut ws,
        &json!({"id": 11, "type": "config_entries/subscribe", "type_filter": "service"}),
        2,
    )
    .await;
    assert_eq!(r[0]["result"], Value::Null);
    // sun's entry (a `service`), imported at the first start.
    assert_eq!(r[1]["event"][0]["entry"]["domain"], "sun");
    assert_eq!(r[1]["event"].as_array().unwrap().len(), 1);
    let r = ws_call(
        &mut ws,
        &json!({"id": 12, "type": "config_entries/subscribe", "type_filter": ["hub"]}),
        2,
    )
    .await;
    assert_eq!(r[1]["event"], json!([]));
    let r = ws_call(
        &mut ws,
        &json!({"id": 13, "type": "config_entries/subscribe", "type_filter": ["hub", 2]}),
        1,
    )
    .await;
    assert_eq!(
        r[0]["error"]["message"],
        "expected str @ data['type_filter'][1]. Got 2"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// `_get_whoami`'s failures: what can't be read as JSON is `{}`; a reply
/// HA can't use raises (`unknown_error`).
#[tokio::test(flavor = "multi_thread")]
async fn location_detection_failures() {
    let cases: [(&'static str, &'static [u8], Value); 4] = [
        (
            "text/plain",
            b"{\"latitude\": 1, \"longitude\": 2}",
            json!({"result": {}}),
        ),
        ("application/json", b"not json", json!({"result": {}})),
        (
            "application/json",
            b"{\"country\": \"US\"}",
            json!({"error": {"code": "unknown_error", "message": "Unknown error"}}),
        ),
        (
            "application/problem+json",
            b"{\"latitude\": 0, \"longitude\": \"2.5\", \"country\": \"US\", \"timezone\": \"\"}",
            json!({"result": {"unit_system": "us_customary", "longitude": 2.5, "country": "US"}}),
        ),
    ];
    for (n, (content_type, body, want)) in cases.into_iter().enumerate() {
        let dir = temp_dir(&format!("detect-{n}"));
        let mut i = start(&dir, true).await;
        let (url, _) = fake_http(move |_| ("200 OK", content_type, body.to_vec())).await;
        // Point detection at this case's server.
        let api = ApiConfig {
            whoami_url: Some(url),
            components: i.components.clone(),
            ..Default::default()
        };
        let app = crate::router(i.hass.clone(), i.auth.clone(), &api)
            .into_make_service_with_connect_info::<SocketAddr>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        i.addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        i.auth.create_owner("Owner", "owner", "pw").unwrap();
        let (token, _) = login(&i.auth, "owner");
        let mut ws = ws_connect(i.addr, &token).await;
        let r = ws_call(&mut ws, &json!({"id": 1, "type": "config/core/detect"}), 1).await;
        let got = r[0]
            .get("result")
            .map_or_else(|| json!({"error": r[0]["error"]}), |v| json!({"result": v}));
        assert_eq!(
            got,
            want,
            "{content_type} {}",
            String::from_utf8_lossy(body)
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// The analytics choice is HA's `.storage/core.analytics`, read back at
/// start.
#[tokio::test(flavor = "multi_thread")]
async fn analytics_preferences_survive_a_restart() {
    let dir = temp_dir("analytics");
    let i = start(&dir, true).await;
    i.auth.create_owner("Owner", "owner", "pw").unwrap();
    let (token, _) = login(&i.auth, "owner");
    let mut ws = ws_connect(i.addr, &token).await;
    let msg = json!({"id": 1, "type": "analytics/preferences", "preferences": {"usage": true}});
    ws_call(&mut ws, &msg, 1).await;
    let again = crate::Analytics::load(&dir);
    assert_eq!(
        again.json(),
        json!({"preferences": {"base": false, "diagnostics": false, "usage": true,
                               "statistics": false, "snapshots": false}, "onboarded": true})
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// A store that can't be written (a directory in the file's place).
fn block(dir: &Path, key: &str) -> PathBuf {
    let path = dir.join(".storage").join(key);
    std::fs::create_dir_all(path.join("blocked")).unwrap();
    path
}

fn owner_body() -> Value {
    json!({"client_id": "http://127.0.0.1:8123/", "name": "Owner", "username": "owner",
           "password": "pw", "language": "en"})
}

const ERROR_500: &str = "500 Internal Server Error\n\nServer got itself in trouble";

/// The logins can't be saved: HA's unhandled store error (a 500), no auth
/// code, nothing left behind, and a retry makes the one owner.
#[tokio::test(flavor = "multi_thread")]
async fn an_unsaved_owner_is_a_500_and_can_be_retried() {
    let dir = temp_dir("unsaved-owner");
    let i = start(&dir, true).await;
    let blocked = block(&dir, "auth_provider.homeassistant");
    let r = http(
        i.addr,
        "POST",
        "/api/onboarding/users",
        Some(&owner_body()),
        None,
    )
    .await;
    assert_eq!((r.0, r.2), (500, json!(ERROR_500)));
    assert!(!i.auth.is_step_done("user"));
    assert!(
        !dir.join(".storage/auth").exists(),
        "no owner without a password on disk"
    );
    assert!(i.components.persons().is_empty());
    assert!(i.registry.snapshot().areas.is_empty());

    std::fs::remove_dir_all(&blocked).unwrap();
    let r = http(
        i.addr,
        "POST",
        "/api/onboarding/users",
        Some(&owner_body()),
        None,
    )
    .await;
    assert_eq!(r.0, 200, "{:?}", r.2);
    assert!(r.2["auth_code"].is_string());
    let users = i.store("auth").unwrap()["data"]["users"].clone();
    assert_eq!(users.as_array().unwrap().len(), 1, "{users}");
    assert_eq!(users[0]["is_owner"], true);
    let logins = i.store("auth_provider.homeassistant").unwrap()["data"]["users"].clone();
    assert_eq!(logins.as_array().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

/// The login was saved but its user wasn't: after a restart the login is
/// a leftover, and onboarding makes the owner with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_half_saved_owner_is_retried_after_a_restart() {
    let dir = temp_dir("half-saved-owner");
    let i = start(&dir, true).await;
    let blocked = block(&dir, "auth");
    let r = http(
        i.addr,
        "POST",
        "/api/onboarding/users",
        Some(&owner_body()),
        None,
    )
    .await;
    assert_eq!((r.0, r.2), (500, json!(ERROR_500)));
    std::fs::remove_dir_all(&blocked).unwrap();

    let again = start(&dir, true).await;
    assert!(!again.auth.is_step_done("user"));
    let r = http(
        again.addr,
        "POST",
        "/api/onboarding/users",
        Some(&owner_body()),
        None,
    )
    .await;
    assert_eq!(r.0, 200, "{:?}", r.2);
    let logins = again.store("auth_provider.homeassistant").unwrap()["data"]["users"].clone();
    assert_eq!(logins.as_array().unwrap().len(), 1, "{logins}");
    let users = again.store("auth").unwrap()["data"]["users"].clone();
    assert_eq!(users.as_array().unwrap().len(), 1, "{users}");
    let _ = std::fs::remove_dir_all(dir);
}

/// The owner was saved, the onboarding store wasn't: a 500 and no code, as
/// HA; the step reads as done (in memory, and after a restart because an
/// owner exists), so the page logs in with the password instead.
#[tokio::test(flavor = "multi_thread")]
async fn an_unsaved_step_is_a_500_without_a_code() {
    let dir = temp_dir("unsaved-step");
    let i = start(&dir, true).await;
    let blocked = block(&dir, "onboarding");
    let r = http(
        i.addr,
        "POST",
        "/api/onboarding/users",
        Some(&owner_body()),
        None,
    )
    .await;
    assert_eq!((r.0, r.2), (500, json!(ERROR_500)));
    let r = http(
        i.addr,
        "POST",
        "/api/onboarding/users",
        Some(&owner_body()),
        None,
    )
    .await;
    assert_eq!(r.2, json!({"message": "User step already done"}));
    std::fs::remove_dir_all(&blocked).unwrap();
    let again = Auth::load_with_cost(&dir, ha_core::Clock::system(), 4).unwrap();
    assert!(again.is_step_done("user"));
    assert_eq!(
        again.check_password("owner", "pw").as_deref(),
        Some("owner")
    );

    // The other steps: the same 500.
    let (token, _) = login(&i.auth, "owner");
    let blocked = block(&dir, "onboarding");
    let r = http(
        i.addr,
        "POST",
        "/api/onboarding/analytics",
        None,
        Some(&token),
    )
    .await;
    assert_eq!((r.0, r.2), (500, json!(ERROR_500)));
    std::fs::remove_dir_all(&blocked).unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

/// An owner on disk with an onboarding file that doesn't list the user
/// step (its save failed, or an older file): no anonymous account can be
/// made, after a restart or in memory.
#[tokio::test(flavor = "multi_thread")]
async fn an_owner_closes_the_user_step() {
    let dir = temp_dir("owner-closes");
    {
        let auth = Auth::load_with_cost(&dir, ha_core::Clock::system(), 4).unwrap();
        auth.create_owner("Owner", "owner", "pw").unwrap();
        auth.save().unwrap();
    }
    std::fs::write(
        dir.join(".storage/onboarding"),
        json!({"version": 4, "minor_version": 1, "key": "onboarding", "data": {"done": []}})
            .to_string(),
    )
    .unwrap();
    let i = start(&dir, true).await;
    let other = json!({"client_id": "http://127.0.0.1:8123/", "name": "Mallory",
                       "username": "mallory", "password": "pw", "language": "en"});
    let r = http(i.addr, "POST", "/api/onboarding/users", Some(&other), None).await;
    assert_eq!(
        (r.0, r.2),
        (403, json!({"message": "User step already done"}))
    );
    let r = http(i.addr, "GET", "/api/onboarding", None, None).await;
    assert_eq!(r.2[0], json!({"step": "user", "done": true}));
    assert!(i.auth.check_password("mallory", "pw").is_none());

    // In memory: an owner the store doesn't list yet still closes it.
    let dir2 = temp_dir("owner-closes-memory");
    let j = start(&dir2, true).await;
    j.auth.create_owner("Owner", "owner", "pw").unwrap();
    assert!(!j.auth.is_step_done("user"));
    let r = http(j.addr, "POST", "/api/onboarding/users", Some(&other), None).await;
    assert_eq!(r.0, 403);
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(dir2);
}

/// A session opened on an owner onboarding then took back (its logins
/// couldn't be saved) ends with it: the WebSocket is closed and the token
/// no longer works, so the connection keeps no admin rights.
#[tokio::test(flavor = "multi_thread")]
async fn a_taken_back_owners_websocket_is_closed() {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    let dir = temp_dir("taken-back-ws");
    let i = start(&dir, true).await;
    // The provisional owner, logged into before its save fails.
    let owner = i.auth.create_owner("Owner", "owner", "pw").unwrap();
    let (token, _) = login(&i.auth, "owner");
    let mut ws = ws_connect(i.addr, &token).await;
    let r = ws_call(&mut ws, &json!({"id": 1, "type": "analytics"}), 1).await;
    assert_eq!(r[0]["success"], true, "an admin until then");

    i.auth.remove_owner(&owner);
    let closed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match ws.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the WebSocket is closed");
    assert!(i.auth.validate_access_token(&token, None).is_none());
    let _ = std::fs::remove_dir_all(dir);
}

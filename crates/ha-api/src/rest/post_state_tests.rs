//! `POST /api/states/<entity_id>` against HA's own answers:
//! `vectors/post_state.json`, made by `scripts/vectors/http.py`, holds what
//! Home Assistant 2026.6.1 answered to each body and `Content-Type`, and its
//! state machine's `light.a` after each step. These send the same requests
//! to hars and compare, with times, context ids and user ids replaced by
//! `<time>`, `<ulid>` and `<hex32>` on both sides.

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::Request;
use ha_auth::Auth;
use ha_core::{EntityId, Hass, SimulatedClock};
use serde_json::{Value, json};
use tower::ServiceExt;

const VECTORS: &str = include_str!("vectors/post_state.json");
const CLIENT: &str = "http://127.0.0.1:8123/";

fn login(auth: &Auth, user: &str, password: &str) -> String {
    let flow = auth.start_login_flow(CLIENT, None);
    let user = auth.check_password(user, password).unwrap();
    let code = auth.finish_login_flow(&flow, CLIENT, &user, None).unwrap();
    auth.exchange_code(Some(CLIENT), Some(&code), None)
        .unwrap()
        .access_token
}

fn is_time(s: &str) -> bool {
    s.len() >= 25 && s.ends_with("+00:00") && s.as_bytes()[10] == b'T'
}

fn is_ulid(s: &str) -> bool {
    s.len() == 26
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b.is_ascii_uppercase() && !b"ILOU".contains(&b)))
}

fn is_hex32(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The generator's `scrub`.
fn scrub(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), scrub(v))).collect()),
        Value::Array(a) => Value::Array(a.iter().map(scrub).collect()),
        Value::String(s) if is_time(s) => json!("<time>"),
        Value::String(s) if is_ulid(s) => json!("<ulid>"),
        Value::String(s) if is_hex32(s) => json!("<hex32>"),
        v => v.clone(),
    }
}

#[tokio::test]
async fn matches_home_assistant() {
    let sim = SimulatedClock::new("2026-06-01T12:00:00Z".parse().unwrap());
    let hass = Hass::with_clock(sim.clock());
    hass.start();
    let auth = crate::test_util::auth(true);
    auth.add_user("user", "pass").unwrap();
    let admin = login(&auth, "adam", "secret");
    let user = login(&auth, "user", "pass");
    let uid = auth.validate_access_token(&user, None).unwrap().user_id;
    auth.set_user_groups(&uid, &[ha_auth::GROUP_ID_USER]);
    let router = crate::router(hass.clone(), auth, &crate::ApiConfig::default());

    let light = EntityId::parse("light.a").unwrap();
    let mut before: Option<Value> = None;
    let steps: Vec<Value> = serde_json::from_str(VECTORS).unwrap();
    assert_eq!(steps.len(), 31);
    for (n, step) in steps.iter().enumerate() {
        // A tick between steps, so a moved time shows.
        sim.jump_to(
            format!("2026-06-01T12:00:{n:02}Z")
                .parse::<jiff::Timestamp>()
                .unwrap(),
        );
        let note = step["note"].as_str().unwrap();
        let r = &step["request"];
        let token = if r["user"] == "admin" { &admin } else { &user };
        let mut req = Request::post(r["path"].as_str().unwrap())
            .header("authorization", format!("Bearer {token}"));
        for (k, v) in r["headers"].as_object().into_iter().flatten() {
            req = req.header(k, v.as_str().unwrap());
        }
        let body = r["body"].as_str().unwrap().to_owned();
        let mut req = req.body(Body::from(body)).unwrap();
        let ip: std::net::SocketAddr = "127.0.0.1:50000".parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(ip));
        let res = router.clone().oneshot(req).await.unwrap();

        assert_eq!(res.status().as_u16(), step["status"], "{note}");
        let header = |name: &str| {
            res.headers()
                .get(name)
                .map(|v| Value::from(v.to_str().unwrap()))
                .unwrap_or(Value::Null)
        };
        assert_eq!(header("content-type"), step["content_type"], "{note}");
        assert_eq!(header("location"), step["location"], "{note}");
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        match &step["body"] {
            Value::String(s) => assert_eq!(&text, s, "{note}"),
            json => assert_eq!(
                &scrub(&serde_json::from_str::<Value>(&text).unwrap()),
                json,
                "{note}"
            ),
        }

        let now = hass.states().get(&light).map(|s| json!(s));
        assert_eq!(
            now.as_ref().map(scrub).unwrap_or(Value::Null),
            step["light.a"],
            "{note}"
        );
        let moved: Vec<&str> = match (&before, &now) {
            (Some(b), Some(n)) => ["last_changed", "last_updated", "last_reported", "context"]
                .into_iter()
                .filter(|k| b[k] != n[k])
                .collect(),
            _ => Vec::new(),
        };
        assert_eq!(json!(moved), step["moved"], "{note}");
        before = now;
    }
}

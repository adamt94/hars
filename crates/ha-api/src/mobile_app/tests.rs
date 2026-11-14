//! Against HA 2026.6.1 run over real HTTP (`vectors/`, sanitised fakes:
//! `spec-mobile-app.md` §0). HA's nonces were fixed; ours are random, so
//! sealed bodies are compared after opening them with the fake key.

use super::*;
use axum::http::Request;
use ha_core::Hass;
use ha_integrations::mobile_app::MobileApp;
use ha_integrations::mobile_app::crypto::Key;
use tower::ServiceExt;

const SECRET: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const WEBHOOK: &str = "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f";
const DELETED: &str = "2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d";
const USER: &str = "00112233445566778899aabbccddeeff";

fn vector(name: &str) -> Value {
    let text = match name {
        "transport" => include_str!("vectors/transport.json"),
        "commands" => include_str!("vectors/commands.json"),
        "entities" => include_str!("vectors/entity_registry_mobile_app.json"),
        "device" => include_str!("vectors/device_registry_mobile_app.json"),
        _ => unreachable!(),
    };
    serde_json::from_str(text).unwrap()
}

fn entry() -> Map<String, Value> {
    json!({
        "domain": "mobile_app",
        "entry_id": "01JFAKEMOBILEAPPENTRY00000",
        "disabled_by": null,
        "data": {
            "app_data": {"push_token": "fake-push-token",
                         "push_url": "https://mobile-apps.home-assistant.io/api/sendPushNotification",
                         "start_live_activity_token": "abababababababab"},
            "app_id": "io.robbie.HomeAssistant",
            "app_name": "Home Assistant",
            "app_version": "2026.9.0 (2026.2874)",
            "device_id": "11111111-2222-4333-8444-555555555555",
            "device_name": " Phone",
            "manufacturer": "Apple",
            "model": "iPhone17,1",
            "no_legacy_encryption": true,
            "os_name": "iOS",
            "os_version": "27.0",
            "secret": SECRET,
            "supports_encryption": true,
            "user_id": USER,
            "webhook_id": WEBHOOK,
        },
    })
    .as_object()
    .unwrap()
    .clone()
}

struct App {
    hass: Hass,
    router: Router,
}

fn app_with(entries: &[Map<String, Value>]) -> App {
    let hass = Hass::new();
    hass.start();
    hass.set_config(ha_core::config::CoreConfig {
        latitude: 0.5,
        longitude: 0.5,
        elevation: 10,
        location_name: "Home".into(),
        time_zone: "Europe/London".into(),
        ..Default::default()
    });
    // The fake home zone, as HA's harness had it.
    hass.set_state(
        EntityId::parse("zone.home").unwrap(),
        "0",
        json!({"latitude": 0.5, "longitude": 0.5, "radius": 100, "passive": false,
               "persons": [], "editable": true, "icon": "mdi:home", "friendly_name": "Home"})
        .as_object()
        .unwrap()
        .clone(),
    );
    let registries = ha_registry::Registries {
        entities: serde_json::from_value(vector("entities")).unwrap(),
        devices: ha_registry::rows![serde_json::from_value(vector("device")).unwrap()],
        ..Default::default()
    };
    let config = crate::ApiConfig {
        registry: Some(ha_registry::Registry::in_memory(&hass, registries)),
        mobile_app: Some(Arc::new(MobileApp::new(entries, vec![DELETED.into()]))),
        ..Default::default()
    };
    let shared = Shared {
        hass: hass.clone(),
        auth: crate::test_util::auth(true),
        config: Arc::new(config),
    };
    App {
        hass,
        router: routes(shared),
    }
}

fn app() -> App {
    app_with(&[entry()])
}

struct Got {
    status: u16,
    content_type: Option<String>,
    allow: Option<String>,
    body: String,
}

async fn send(app: &App, id: &str, method: &str, body: Option<&str>, ct: Option<&str>) -> Got {
    let mut req = Request::builder()
        .method(method)
        .uri(format!("/api/webhook/{id}"));
    req = req.header("content-type", ct.unwrap_or("application/json"));
    let req = req
        .body(Body::from(body.unwrap_or_default().to_owned()))
        .unwrap();
    let res = app.router.clone().oneshot(req).await.unwrap();
    let (content_type, allow) = {
        let h = res.headers();
        let get = |name| {
            h.get(name)
                .map(|v: &HeaderValue| v.to_str().unwrap().to_owned())
        };
        (get(header::CONTENT_TYPE), get(header::ALLOW))
    };
    let status = res.status().as_u16();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    Got {
        status,
        content_type,
        allow,
        body: String::from_utf8(body.to_vec()).unwrap(),
    }
}

/// A body, opened when it is a sealed envelope; state timestamps and
/// context ids (which differ per run) removed.
fn plain(body: &str) -> Value {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return Value::String(body.to_owned());
    };
    let v = match v.get("encrypted_data").and_then(Value::as_str) {
        Some(sealed) => {
            assert_eq!(
                v.as_object().unwrap().keys().collect::<Vec<_>>(),
                ["encrypted", "encrypted_data"]
            );
            let opened = Key::hex(SECRET).unwrap().open(sealed).expect("opens");
            serde_json::from_slice(&opened).unwrap()
        }
        None => v,
    };
    normalise(v)
}

fn normalise(mut v: Value) -> Value {
    if let Value::Array(items) = &mut v {
        for item in items {
            if let Value::Object(m) = item {
                for k in ["last_changed", "last_reported", "last_updated", "context"] {
                    m.remove(k);
                }
            }
        }
    }
    v
}

#[tokio::test]
async fn transport_as_ha() {
    let app = app();
    let v = vector("transport");
    let mut n = 0;
    for case in v["cases"].as_array().unwrap() {
        let Some(req) = case.get("request") else {
            continue;
        };
        let name = case["case"].as_str().unwrap();
        let id = if name.starts_with("unregistered") {
            "ab".repeat(32)
        } else if name.starts_with("webhook id of a removed entry") {
            // Removed during the run: unregistered.
            "cd".repeat(32)
        } else if name.starts_with("after restart") {
            DELETED.to_owned()
        } else {
            WEBHOOK.to_owned()
        };
        let method = req["method"].as_str().unwrap_or("POST");
        let ct = req["headers"]["Content-Type"].as_str();
        let got = send(&app, &id, method, req["body"].as_str(), ct).await;
        let want = &case["response"];
        assert_eq!(got.status, want["status"], "{name}");
        assert_eq!(
            got.content_type.as_deref(),
            want["headers"]["Content-Type"].as_str(),
            "{name}"
        );
        assert_eq!(
            got.allow.as_deref(),
            want["headers"]["Allow"].as_str(),
            "{name}"
        );
        assert_eq!(
            plain(&got.body),
            plain(want["body"].as_str().unwrap()),
            "{name}"
        );
        n += 1;
    }
    assert_eq!(n, 29);
}

/// Every command but those that need entities (sensors, location), which
/// come with them.
#[tokio::test]
async fn commands_as_ha() {
    let app = app();
    let mut bus = app.hass.bus().subscribe();
    let v = vector("commands");
    let called = Arc::new(parking_lot::Mutex::new(Vec::new()));
    for service in ["echo", "strict", "boom"] {
        let called = called.clone();
        let schema = (service == "strict").then(|| {
            ha_core::Schema::new().required("x", ha_core::Kind::Int(ha_core::Range::default()))
        });
        app.hass.services().register_with_schema(
            "test",
            service,
            schema,
            ha_core::SupportsResponse::None,
            move |call: ha_core::ServiceCall| {
                let called = called.clone();
                async move {
                    if call.service == "boom" {
                        return Err(ha_core::ServiceError::Unknown("boom".into()));
                    }
                    called.lock().push((
                        call.service.to_string(),
                        Value::Object((*call.data).clone()),
                        call.context.user_id.as_deref().map(str::to_owned),
                    ));
                    Ok(None)
                }
            },
        );
    }
    for (i, x) in v["exchanges"].as_array().unwrap().iter().enumerate() {
        let ty = x["type"].as_str().unwrap();
        let got = send(&app, WEBHOOK, "POST", x["request"]["body"].as_str(), None).await;
        let want = &x["response"];
        assert_eq!(got.status, want["status"], "{i} {ty}");
        assert_eq!(
            got.content_type.as_deref(),
            want["headers"]["Content-Type"].as_str(),
            "{i} {ty}"
        );
        let mut ours = plain(&got.body);
        let mut theirs = plain(want["body"].as_str().unwrap());
        match ty {
            // hars's own running integrations, not HA's.
            "get_config" => {
                ours.as_object_mut().unwrap().remove("components");
                theirs.as_object_mut().unwrap().remove("components");
            }
            // No Jinja yet: every template answers an error.
            "render_template" => {
                for (_, item) in theirs.as_object_mut().unwrap() {
                    *item =
                        json!({"error": "TemplateError: templates are not supported by hars yet"});
                }
            }
            _ => {}
        }
        assert_eq!(ours, theirs, "{i} {ty}");

        // Events and service calls, but HA's registry and `tag` entity ones.
        let want_events: Vec<Value> = x["effects"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| {
                !matches!(
                    e["event_type"].as_str(),
                    Some("device_registry_updated" | "entity_registry_updated")
                )
            })
            .cloned()
            .collect();
        let mut got_events = Vec::new();
        while let Ok(e) = bus.try_recv() {
            if let EventKind::Custom { name, data } = &e.kind {
                got_events.push(json!({
                    "event_type": name.as_str(),
                    "data": data,
                    "origin": e.origin.as_str(),
                    "context_user_id": e.context.user_id.as_deref(),
                }));
            }
        }
        assert_eq!(got_events, want_events, "{i} {ty}");
        let want_calls: Vec<Value> = x["effects"]["service_calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| json!([c["service"], c["data"], c["context_user_id"]]))
            .collect();
        let got_calls: Vec<Value> = called
            .lock()
            .drain(..)
            .map(|(s, d, u)| json!([s, d, u]))
            .collect();
        assert_eq!(got_calls, want_calls, "{i} {ty}");
    }
}

#[tokio::test]
async fn update_registration_merges_shallowly() {
    let app = app();
    let key = Key::hex(SECRET).unwrap();
    let body = |data: Value| {
        json!({"type": "update_registration", "encrypted": true,
               "encrypted_data": key.seal(data.to_string().as_bytes())})
        .to_string()
    };
    let got = send(
        &app,
        WEBHOOK,
        "POST",
        Some(&body(
            json!({"app_version": "2", "device_name": " Phone", "manufacturer": "Apple",
                          "model": "iPhone17,1", "app_data": {"other": 1}}),
        )),
        None,
    )
    .await;
    let reply = plain(&got.body);
    // `app_data` is replaced whole; secrets never come back.
    assert_eq!(reply["app_data"], json!({"other": 1}));
    assert_eq!(reply["app_version"], "2");
    assert_eq!(reply["os_version"], "27.0");
    for secret in ["secret", "webhook_id", "user_id"] {
        assert!(reply.get(secret).is_none());
    }
    // `push_token` without `push_url`, a bad URL, a bad boolean: `{}`.
    for app_data in [
        json!({"push_token": "t"}),
        json!({"push_token": "t", "push_url": "ftp://x"}),
        json!({"push_websocket_channel": "maybe"}),
    ] {
        let got = send(
            &app,
            WEBHOOK,
            "POST",
            Some(&body(
                json!({"app_version": "3", "device_name": "x", "manufacturer": "x",
                              "model": "x", "app_data": app_data}),
            )),
            None,
        )
        .await;
        assert_eq!(got.body, "{}");
    }
}

#[tokio::test]
async fn plaintext_registrations_and_enable_encryption() {
    let mut e = entry();
    let data = e["data"].as_object_mut().unwrap();
    data.insert("supports_encryption".into(), json!(false));
    data.remove("no_legacy_encryption");
    let app = app_with(&[e]);
    // Plain envelopes are accepted and answered in clear.
    let got = send(&app, WEBHOOK, "POST", Some(r#"{"type":"get_zones"}"#), None).await;
    assert_eq!(got.content_type.as_deref(), Some("application/json"));
    assert_eq!(plain(&got.body)[0]["entity_id"], "zone.home");
    // A number body raises (`in`): an empty 200.
    let got = send(&app, WEBHOOK, "POST", Some("1"), None).await;
    assert_eq!((got.status, got.body.as_str()), (200, ""));
    let got = send(
        &app,
        WEBHOOK,
        "POST",
        Some(r#"{"type":"enable_encryption"}"#),
        None,
    )
    .await;
    assert_eq!(
        got.content_type.as_deref(),
        Some("application/json; charset=utf-8")
    );
    let secret = serde_json::from_str::<Value>(&got.body).unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(got.body, format!(r#"{{"secret": "{secret}"}}"#));
    assert_eq!(secret.len(), 64);
    // Now encryption is required; the legacy key works until a hex success.
    let got = send(&app, WEBHOOK, "POST", Some(r#"{"type":"get_zones"}"#), None).await;
    assert_eq!(got.status, 400);
    let sealed = Key::legacy(&secret).seal(b"{}");
    let got = send(
        &app,
        WEBHOOK,
        "POST",
        Some(&json!({"type": "get_zones", "encrypted": 1, "encrypted_data": sealed}).to_string()),
        None,
    )
    .await;
    let env: Value = serde_json::from_str(&got.body).unwrap();
    assert!(
        Key::legacy(&secret)
            .open(env["encrypted_data"].as_str().unwrap())
            .is_some()
    );
}

#[test]
fn python_json_dumps() {
    assert_eq!(
        py_dumps(&json!({"a": [1, "\u{b0}"], "b": {"c": null}})),
        "{\"a\": [1, \"\\u00b0\"], \"b\": {\"c\": null}}"
    );
}

#[test]
fn urls_as_cv_url() {
    for good in ["https://x", "HTTP://x:80/p", "https://[::1]:8123/a?b"] {
        assert!(is_url(good), "{good}");
    }
    for bad in ["ftp://x", "https://", "x", "https://x:99999", "https://x:p"] {
        assert!(!is_url(bad), "{bad}");
    }
}

/// HA: a sealed `null` is `webhook_payload is None`, so plain `{}`.
#[tokio::test]
async fn a_sealed_null_is_an_empty_reply() {
    let app = app();
    let sealed = Key::hex(SECRET).unwrap().seal(b"null");
    let body = json!({"type": "get_config", "encrypted": true, "encrypted_data": sealed});
    let got = send(&app, WEBHOOK, "POST", Some(&body.to_string()), None).await;
    assert_eq!(
        (got.status, got.content_type.as_deref(), got.body.as_str()),
        (200, Some("application/json; charset=utf-8"), "{}")
    );
}

/// The check and the new secret are one step: only one request wins.
#[tokio::test]
async fn concurrent_enable_encryption_gives_one_secret() {
    let mut e = entry();
    let data = e["data"].as_object_mut().unwrap();
    data.insert("supports_encryption".into(), json!(false));
    data.remove("no_legacy_encryption");
    let app = Arc::new(app_with(&[e]));
    let tasks: Vec<_> = (0..16)
        .map(|_| {
            let app = app.clone();
            tokio::spawn(async move {
                send(
                    &app,
                    WEBHOOK,
                    "POST",
                    Some(r#"{"type":"enable_encryption"}"#),
                    None,
                )
                .await
            })
        })
        .collect();
    let mut won = 0;
    for t in tasks {
        let got = t.await.unwrap();
        match got.status {
            200 => won += 1,
            // Later plaintext requests are refused, before or after the
            // check: `encryption_required` or `encryption_already_enabled`.
            400 => assert!(got.body.contains("encryption_"), "{}", got.body),
            s => panic!("{s}"),
        }
    }
    assert_eq!(won, 1);
}

/// Over the cap, reading the body raises: HA's catch-all empty `200`.
#[tokio::test]
async fn oversized_bodies_are_an_empty_200() {
    let app = app();
    let big = format!(
        r#"{{"type":"get_zones","data":{{"x":"{}"}}}}"#,
        "a".repeat(BODY_LIMIT)
    );
    let got = send(&app, WEBHOOK, "POST", Some(&big), None).await;
    assert_eq!(
        (got.status, got.content_type, got.body.as_str()),
        (200, None, "")
    );
}

/// The command runs to completion when the client hangs up (`shield`).
#[tokio::test]
async fn a_hung_up_request_still_runs() {
    let app = app();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let rx = Arc::new(parking_lot::Mutex::new(Some(rx)));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = done.clone();
    app.hass
        .services()
        .register("test", "slow", ha_core::SupportsResponse::None, move |_| {
            let rx = rx.lock().take();
            let flag = flag.clone();
            async move {
                if let Some(rx) = rx {
                    let _ = rx.await;
                }
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(None)
            }
        });
    let sealed = Key::hex(SECRET)
        .unwrap()
        .seal(br#"{"domain":"test","service":"slow"}"#);
    let body =
        json!({"type": "call_service", "encrypted": true, "encrypted_data": sealed}).to_string();
    let req = Request::post(format!("/api/webhook/{WEBHOOK}"))
        .body(Body::from(body))
        .unwrap();
    let pending = tokio::spawn(app.router.clone().oneshot(req));
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    // The client goes away, then the service is allowed to finish.
    pending.abort();
    tx.send(()).unwrap();
    for _ in 0..100 {
        if done.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the service was cancelled with the request");
}

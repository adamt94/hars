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
        "sensors" => include_str!("vectors/sensors.json"),
        "startup" => include_str!("vectors/startup.json"),
        "restore" => include_str!("vectors/restore_state_phone.json"),
        "location" => include_str!("vectors/location.json"),
        "registration" => include_str!("vectors/registration.json"),
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
    registry: Arc<ha_registry::Registry>,
    entries: Arc<ha_integrations::config_entries::ConfigEntries>,
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
    // HA's last states for the phone, as the restore store has them.
    let dir = std::env::temp_dir().join(format!(
        "hars-mobile-{}-{}",
        std::process::id(),
        rand::random::<u32>()
    ));
    std::fs::create_dir_all(dir.join(".storage")).unwrap();
    std::fs::write(
        dir.join(".storage").join(ha_core::restore::KEY),
        json!({"version": 1, "minor_version": 1, "key": ha_core::restore::KEY,
               "data": vector("restore")})
        .to_string(),
    )
    .unwrap();
    hass.set_restore(ha_core::restore::RestoreState::load(&dir));
    let _ = std::fs::remove_dir_all(&dir);
    let registry = ha_registry::Registry::in_memory(&hass, registries);
    let mobile_app = Arc::new(MobileApp::new(entries, vec![DELETED.into()]));
    let config_entries =
        ha_integrations::config_entries::ConfigEntries::in_memory(&hass, entries.to_vec());
    mobile_app.setup(&hass, registry.clone(), config_entries.clone(), None);
    let config = crate::ApiConfig {
        registry: Some(registry.clone()),
        mobile_app: Some(mobile_app),
        ..Default::default()
    };
    let shared = Shared {
        hass: hass.clone(),
        auth: crate::test_util::auth(true),
        config: Arc::new(config),
        templates: Default::default(),
        core_config: Default::default(),
    };
    App {
        hass,
        router: routes(shared),
        registry,
        entries: config_entries,
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

        // Events and service calls, but the `tag` entity's registry ones
        // (`hars` keeps no tag entities).
        let want_events: Vec<Value> = x["effects"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["event_type"] != "entity_registry_updated")
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
        // The entry and the device, saved as HA left them.
        if let Some(data) = x.get("entry_data_after") {
            let entry = app.entries.list().unwrap()[0].clone();
            let mut ours = entry["data"].as_object().unwrap().clone();
            // The vector leaves the secret out.
            ours.remove("secret");
            assert_eq!(&Value::Object(ours), data, "{i} {ty} entry");
        }
        if let Some(devices) = x.get("devices_after") {
            let want = devices
                .as_array()
                .unwrap()
                .iter()
                .find(|d| d["id"] == "d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0")
                .unwrap();
            let snap = app.registry.snapshot();
            let got = serde_json::to_value(&*snap.devices[0]).unwrap();
            for (k, v) in want.as_object().unwrap() {
                assert_eq!(&got[k], v, "{i} {ty} device {k}");
            }
        }
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
    // The new secret is saved to the entry; the flag isn't, yet.
    let saved = || app.entries.list().unwrap()[0]["data"].clone();
    assert_eq!(saved()["secret"], secret.as_str());
    assert_eq!(saved()["supports_encryption"], true);
    assert!(saved().get("no_legacy_encryption").is_none());
    // The first hex-key request records `no_legacy_encryption`.
    let sealed = Key::hex(&secret).unwrap().seal(b"{}");
    send(
        &app,
        WEBHOOK,
        "POST",
        Some(
            &json!({"type": "get_zones", "encrypted": true, "encrypted_data": sealed}).to_string(),
        ),
        None,
    )
    .await;
    assert_eq!(saved()["no_legacy_encryption"], true);
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

fn state_json(hass: &Hass, id: &str) -> Value {
    match hass.states().get(&EntityId::parse(id).unwrap()) {
        Some(s) => json!({"state": s.state.as_str(), "attributes": s.attributes}),
        None => Value::Null,
    }
}

/// HA's `startup.json`: the phone's restored sensors before any webhook.
#[tokio::test]
async fn sensors_restore_as_ha() {
    let app = app();
    let v = vector("startup");
    let mut n = 0;
    for (id, want) in v["states"].as_object().unwrap() {
        if !(id.starts_with("sensor.") || id.starts_with("binary_sensor.")) {
            continue;
        }
        assert_eq!(state_json(&app.hass, id), *want, "{id}");
        n += 1;
    }
    assert_eq!(n, 17);
    // Disabled rows get no entity.
    assert_eq!(
        state_json(&app.hass, "sensor.phone_camera_stream"),
        Value::Null
    );
}

/// HA's `sensors.json`: updates and (re-)registrations in order: replies,
/// state writes, registry events and rows.
#[tokio::test]
async fn sensors_update_and_register_as_ha() {
    let app = app();
    let registry = app.registry.clone();
    let v = vector("sensors");
    let mut bus = app.hass.bus().subscribe();
    let mut feed = app.hass.subscribe_state_changes();
    for (i, x) in v["exchanges"].as_array().unwrap().iter().enumerate() {
        if let Some(rows) = x
            .get("registry_after_register")
            .or(x.get("registry_after_reregister"))
        {
            check_rows(&registry, rows, i);
            continue;
        }
        let ty = x["type"].as_str().unwrap();
        let got = send(&app, WEBHOOK, "POST", x["request"]["body"].as_str(), None).await;
        let want = &x["response"];
        assert_eq!(got.status, want["status"], "{i} {ty}");
        let theirs = match want.get("decrypted") {
            Some(Value::String(d)) => serde_json::from_str(d).unwrap(),
            _ => plain(want["body"].as_str().unwrap()),
        };
        let mut ours = plain(&got.body);
        let mut theirs = theirs;
        if ty == "get_config" {
            ours.as_object_mut().unwrap().remove("components");
            theirs.as_object_mut().unwrap().remove("components");
        }
        assert_eq!(ours, theirs, "{i} {ty}");

        // State writes: what HA wrote is what's there now.
        let mut written: Vec<String> = std::iter::from_fn(|| feed.try_recv())
            .filter_map(|e| match &e.kind {
                EventKind::StateChanged { entity_id, .. } => Some(entity_id.to_string()),
                _ => None,
            })
            .collect();
        written.sort();
        written.dedup();
        let writes = x["effects"]["state_writes"].as_array().unwrap();
        let mut want_ids: Vec<String> = writes
            .iter()
            .map(|w| w["entity_id"].as_str().unwrap().to_owned())
            .collect();
        want_ids.sort();
        want_ids.dedup();
        // Re-enabling adds the entity now, not 30 s later (spec D2).
        if i == 17 {
            want_ids.push("binary_sensor.phone_camera_motion".into());
        }
        assert_eq!(written, want_ids, "{i} {ty} written");
        for w in writes {
            let id = w["entity_id"].as_str().unwrap();
            let want = if w["state"].is_null() {
                Value::Null
            } else {
                json!({"state": w["state"], "attributes": w["attributes"]})
            };
            if writes.iter().rfind(|x| x["entity_id"] == id) == Some(w) {
                assert_eq!(state_json(&app.hass, id), want, "{i} {ty} {id}");
            }
        }

        // Registry events (as a set: HA delivers nested ones first), but the
        // display-precision option (spec R6).
        let mut got_events: Vec<String> = std::iter::from_fn(|| bus.try_recv().ok())
            .filter_map(|e| match &e.kind {
                EventKind::Custom { name, data } if name == "entity_registry_updated" => {
                    Some(data.to_string())
                }
                _ => None,
            })
            .collect();
        let mut want_events: Vec<String> = x["effects"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["event_type"] == "entity_registry_updated")
            .filter(|e| e["data"]["changes"] != json!({"options": {}}))
            .map(|e| e["data"].to_string())
            .collect();
        got_events.sort();
        want_events.sort();
        assert_eq!(got_events, want_events, "{i} {ty} events");
    }
}

/// The registry rows HA had after the registrations, field by field.
fn check_rows(registry: &ha_registry::Registry, rows: &Value, i: usize) {
    let snap = registry.snapshot();
    for (id, want) in rows.as_object().unwrap() {
        // HA's extra test registrations aren't in this fixture.
        if want["config_entry_id"] != "01JFAKEMOBILEAPPENTRY00000" {
            continue;
        }
        let row = snap
            .entities
            .iter()
            .find(|e| &e.entity_id == id)
            .unwrap_or_else(|| panic!("{i}: no row {id}"));
        let got = serde_json::to_value(&**row).unwrap();
        for (k, v) in want.as_object().unwrap() {
            assert_eq!(&got[k], v, "{i} {id} {k}");
        }
    }
}

/// HA's `location.json`: the tracker after each `update_location`, and
/// what it dumps for restore.
#[tokio::test]
async fn location_updates_as_ha() {
    let app = app();
    let v = vector("location");
    let steps = v["exchanges"].as_array().unwrap();
    assert_eq!(
        state_json(&app.hass, "device_tracker.phone"),
        steps[0]["initial"]["device_tracker.phone"]
    );
    let mut feed = app.hass.subscribe_state_changes();
    let mut n = 0;
    for step in &steps[1..] {
        if let Some(rows) = step.get("restore_rows") {
            let want = rows
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["state"]["entity_id"] == "device_tracker.phone")
                .unwrap();
            let got = app
                .hass
                .restore()
                .extra_data_now("device_tracker.phone")
                .unwrap();
            assert_eq!(got, want["extra_data"]);
            continue;
        }
        let case = step["case"].as_str().unwrap_or_default();
        let got = send(
            &app,
            WEBHOOK,
            "POST",
            step["request"]["body"].as_str(),
            None,
        )
        .await;
        assert_eq!((got.status, got.body.as_str()), (200, "{}"), "{case}");
        let want = &step["after"]["device_tracker.phone"];
        assert_eq!(
            state_json(&app.hass, "device_tracker.phone"),
            *want,
            "{case}"
        );
        // Valid updates always write (`force_update`); invalid ones don't.
        let wrote = std::iter::from_fn(|| feed.try_recv()).any(|e| {
            matches!(&e.kind, EventKind::StateChanged { entity_id, .. }
                if entity_id.as_str() == "device_tracker.phone")
        });
        assert_eq!(wrote, !case.contains("invalid"), "{case}");
        n += 1;
    }
    assert_eq!(n, 20);
}

/// The tracker and notify entity restore as HA's `startup.json`; the
/// notify services fail cleanly.
#[tokio::test]
async fn tracker_and_notify_restore_as_ha() {
    let app = app();
    let v = vector("startup");
    for id in ["device_tracker.phone", "notify.phone"] {
        assert_eq!(state_json(&app.hass, id), v["states"][id], "{id}");
    }
    for service in ["send_message", "mobile_app_phone"] {
        let err = app
            .hass
            .call_service("notify", service, Map::new(), None, false, None)
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Push notifications are not supported by hars yet"
        );
    }
}

/// HA's `supports_push`: cloud push or the local push channel makes the
/// notify entity; neither, none.
#[tokio::test]
async fn notify_needs_some_push() {
    for (app_data, exists) in [
        (json!({"push_websocket_channel": true}), true),
        (json!({}), false),
    ] {
        let mut e = entry();
        e["data"]["app_data"] = app_data.clone();
        let app = app_with(&[e]);
        assert_eq!(
            state_json(&app.hass, "notify.phone") != Value::Null,
            exists,
            "{app_data}"
        );
    }
}

/// Review regression: a first hex-key request whose payload is `null`
/// still saves `no_legacy_encryption` (HA sets it before the `None` check).
#[tokio::test]
async fn a_sealed_null_still_saves_the_flag() {
    let mut e = entry();
    e["data"]
        .as_object_mut()
        .unwrap()
        .remove("no_legacy_encryption");
    let app = app_with(&[e]);
    let sealed = Key::hex(SECRET).unwrap().seal(b"null");
    let body = json!({"type": "get_config", "encrypted": true, "encrypted_data": sealed});
    let got = send(&app, WEBHOOK, "POST", Some(&body.to_string()), None).await;
    assert_eq!(got.body, "{}");
    assert_eq!(
        app.entries.list().unwrap()[0]["data"]["no_legacy_encryption"],
        true
    );
}

/// An access token for the test owner, and their user id.
fn owner_token(auth: &ha_auth::Auth) -> (String, String) {
    const CLIENT: &str = "http://127.0.0.1:8123/";
    let flow = auth.start_login_flow(CLIENT, None);
    let user = auth.check_password("adam", "secret").unwrap();
    let code = auth.finish_login_flow(&flow, CLIENT, &user, None).unwrap();
    let t = auth.exchange_code(Some(CLIENT), Some(&code), None).unwrap();
    let user_id = auth
        .validate_access_token(&t.access_token, None)
        .unwrap()
        .user_id;
    (t.access_token, user_id)
}

/// `POST /api/mobile_app/registrations` through the whole router: HA's
/// `registration.json` and `repair.json` outcomes.
#[tokio::test]
async fn registrations_as_ha() {
    let hass = Hass::new();
    hass.start();
    let auth = crate::test_util::auth(true);
    let (token, user_id) = owner_token(&auth);
    // The user's person, who gets the new tracker.
    let dir = std::env::temp_dir().join(format!("hars-reg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".storage")).unwrap();
    std::fs::write(
        dir.join(".storage/person"),
        json!({"version": 2, "minor_version": 1, "key": "person", "data": {"items": [
            {"id": "adam", "name": "adam", "user_id": user_id, "device_trackers": []}]}})
        .to_string(),
    )
    .unwrap();
    let components = Arc::new(ha_components::Components::load_strict(&dir).unwrap());
    components.setup(&hass, &Default::default());
    let mut owned = entry();
    owned["data"]["user_id"] = json!(user_id);
    owned.insert(
        "unique_id".into(),
        json!("io.robbie.HomeAssistant-11111111-2222-4333-8444-555555555555"),
    );
    let registries = ha_registry::Registries {
        entities: serde_json::from_value(vector("entities")).unwrap(),
        devices: ha_registry::rows![serde_json::from_value(vector("device")).unwrap()],
        ..Default::default()
    };
    let registry = ha_registry::Registry::in_memory(&hass, registries);
    let entries =
        ha_integrations::config_entries::ConfigEntries::in_memory(&hass, vec![owned.clone()]);
    let looked = entries.clone();
    registry.set_entry_lookup(Arc::new(move |id: &str| looked.info(id)));
    let app = Arc::new(MobileApp::load(&dir, &[owned]));
    app.setup(
        &hass,
        registry.clone(),
        entries.clone(),
        Some(components.clone()),
    );
    let config = crate::ApiConfig {
        config_dir: "/config".into(),
        registry: Some(registry.clone()),
        mobile_app: Some(app.clone()),
        components: components.clone(),
        ..Default::default()
    };
    let router = crate::router(hass.clone(), auth, &config);
    let post = |body: Value, auth: bool| {
        let router = router.clone();
        let token = token.clone();
        async move {
            let mut req = Request::post("/api/mobile_app/registrations")
                .header("content-type", "application/json");
            if auth {
                req = req.header("authorization", format!("Bearer {token}"));
            }
            let req = req
                .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    1,
                ))))
                .body(Body::from(body.to_string()))
                .unwrap();
            let res = router.oneshot(req).await.unwrap();
            let status = res.status().as_u16();
            let body = axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, String::from_utf8(body.to_vec()).unwrap())
        }
    };
    let v = vector("registration");
    let iphone = v["cases"][2]["request"].clone();

    let (status, body) = post(iphone.clone(), false).await;
    assert_eq!((status, body.as_str()), (401, "401: Unauthorized"));
    let mut missing = iphone.clone();
    missing.as_object_mut().unwrap().remove("model");
    let (status, body) = post(missing, true).await;
    assert_eq!(status, 400);
    assert_eq!(body, v["cases"][1]["body"].as_str().unwrap());

    // A new phone: a new entry, its tracker on the user's person, and a
    // webhook that answers with the new secret.
    let (status, body) = post(iphone, true).await;
    assert_eq!(status, 201);
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        reply.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["cloudhook_url", "remote_ui_url", "secret", "webhook_id"]
    );
    let (webhook, secret) = (
        reply["webhook_id"].as_str().unwrap().to_owned(),
        reply["secret"].as_str().unwrap().to_owned(),
    );
    let all = entries.list().unwrap();
    assert_eq!(all.len(), 2);
    let new = all
        .iter()
        .find(|e| e["data"]["webhook_id"] == webhook.as_str())
        .unwrap();
    let want = &v["cases"][3]["new_entry"];
    assert_eq!(new["title"], want["title"]);
    assert_eq!(new["unique_id"], want["unique_id"]);
    assert_eq!(new["source"], "registration");
    let mut keys: Vec<&str> = new["data"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    let mut want_keys: Vec<&str> = want["data_keys_in_order"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap())
        .collect();
    want_keys.sort();
    assert_eq!(keys, want_keys);
    assert_eq!(
        components.persons()[0]["device_trackers"],
        json!(["device_tracker.phone_2"])
    );
    let sealed = Key::hex(&secret).unwrap().seal(b"{}");
    let shared_router = crate::router(hass.clone(), crate::test_util::auth(true), &config);
    let res = shared_router
        .oneshot(
            Request::post(format!("/api/webhook/{webhook}"))
                .body(Body::from(
                    json!({"type": "get_zones", "encrypted": true, "encrypted_data": sealed})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let env: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        Key::hex(&secret)
            .unwrap()
            .open(env["encrypted_data"].as_str().unwrap())
            .is_some()
    );

    // A request to the old webhook that is already past routing.
    let Webhook::Registered(stale) = app.webhook(WEBHOOK) else {
        panic!("registered");
    };
    // Re-pair the imported phone (same app + device id): the old entry and
    // webhook go, the tracker moves to the new entry, the old sensors'
    // rows go.
    let mut same = v["cases"][2]["request"].clone();
    same["device_id"] = json!("11111111-2222-4333-8444-555555555555");
    same["app_data"] = json!({});
    let (status, body) = post(same, true).await;
    assert_eq!(status, 201);
    let new_webhook = serde_json::from_str::<Value>(&body).unwrap()["webhook_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let all = entries.list().unwrap();
    assert_eq!(all.len(), 2);
    assert!(
        !all.iter()
            .any(|e| e["entry_id"] == "01JFAKEMOBILEAPPENTRY00000")
    );
    assert!(matches!(app.webhook(WEBHOOK), Webhook::Unknown));
    let new_entry = all
        .iter()
        .find(|e| e["data"]["webhook_id"] == new_webhook.as_str())
        .unwrap();
    let snap = registry.snapshot();
    let tracker = snap
        .entities
        .iter()
        .find(|e| e.entity_id == "device_tracker.phone")
        .unwrap();
    assert_eq!(
        tracker.config_entry_id.as_ref(),
        new_entry["entry_id"].as_str().map(str::to_owned).as_ref()
    );
    assert!(
        !snap
            .entities
            .iter()
            .any(|e| e.entity_id == "sensor.phone_battery_level")
    );
    assert!(
        hass.states()
            .get(&EntityId::parse("sensor.phone_battery_level").unwrap())
            .is_none()
    );
    let device = snap
        .devices
        .iter()
        .find(|d| d.id == "d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0")
        .unwrap();
    assert_eq!(
        device.config_entries,
        [new_entry["entry_id"].as_str().unwrap()]
    );
    // The old webhook id is saved as deleted at once (410 after a restart).
    let store: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join(".storage/mobile_app")).unwrap())
            .unwrap();
    assert_eq!(store["data"]["deleted_ids"], json!([WEBHOOK]));
    let restarted = MobileApp::load(&dir, &[]);
    assert!(matches!(restarted.webhook(WEBHOOK), Webhook::Deleted));
    // Review regression: a request already past routing to the old webhook
    // can't recreate anything for the removed entry.
    let shared = Shared {
        hass: hass.clone(),
        auth: crate::test_util::auth(true),
        config: Arc::new(config.clone()),
        templates: Default::default(),
        core_config: Default::default(),
    };
    let sealed = Key::hex(SECRET).unwrap().seal(
        json!({"name": "Ghost", "type": "sensor", "unique_id": "ghost", "state": 1})
            .to_string()
            .as_bytes(),
    );
    let body = json!({"type": "register_sensor", "encrypted": true, "encrypted_data": sealed});
    let reply = handle(&shared, &stale, body.to_string().as_bytes()).await;
    assert!(matches!(reply, Reply::Bare(StatusCode::OK)));
    assert!(
        hass.states()
            .get(&EntityId::parse("sensor.phone_ghost").unwrap())
            .is_none()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review regression: an unload waits for a command already running.
#[tokio::test]
async fn retiring_waits_for_running_commands() {
    let reg = Arc::new(Registration::from_entry(&entry()).unwrap());
    let guard = reg.enter().await.unwrap();
    let r = reg.clone();
    let retiring = tokio::spawn(async move { r.retire().await });
    tokio::task::yield_now().await;
    assert!(!retiring.is_finished());
    drop(guard);
    retiring.await.unwrap();
    assert!(reg.enter().await.is_none());
}

/// HA's `RequestDataValidator` messages (from HA's own schema).
#[test]
fn registration_errors_as_ha() {
    let base = json!({"app_id": "a", "app_name": "a", "app_version": "1", "device_name": "d",
                      "manufacturer": "m", "model": "x", "os_name": "iOS"});
    let err = |extra: Value| {
        let mut v = base.clone();
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        registration_schema(&v).unwrap_err()
    };
    for (extra, want) in [
        (
            json!({"supports_encryption": "bogus"}),
            "invalid boolean value bogus for dictionary value @ data['supports_encryption']",
        ),
        (
            json!({"app_data": {"push_token": "x"}}),
            "some but not all values in the same group of inclusion 'push_cloud' @ data['app_data'][<push_cloud>]",
        ),
        (
            json!({"app_data": {"push_url": "ftp://x", "push_token": "t"}}),
            "invalid url for dictionary value @ data['app_data']['push_url']",
        ),
        (
            json!({"app_data": 5}),
            "expected a dictionary for dictionary value @ data['app_data']",
        ),
        (
            json!({"app_data": {"push_websocket_channel": "maybe"}}),
            "invalid boolean value maybe for dictionary value @ data['app_data']['push_websocket_channel']",
        ),
        (
            json!({"app_id": null}),
            "string value is None for dictionary value @ data['app_id']",
        ),
        (
            json!({"app_id": [1]}),
            "value should be a string for dictionary value @ data['app_id']",
        ),
    ] {
        assert_eq!(err(extra.clone()), want, "{extra}");
    }
    // Extra keys are dropped; defaults filled.
    let mut v = base.clone();
    v["extra"] = json!("dropped");
    let ok = registration_schema(&v).unwrap();
    assert!(ok.get("extra").is_none());
    assert_eq!(
        (ok["app_data"].clone(), ok["supports_encryption"].clone()),
        (json!({}), json!(false))
    );
}

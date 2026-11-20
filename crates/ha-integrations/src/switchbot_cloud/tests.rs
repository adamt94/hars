//! Against the real GET responses (ids replaced) and vectors made by
//! running HA 2026.6.1's own code on them (see `vectors/`).

use super::*;
use crate::light::Profiles;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn fixture(name: &str) -> Value {
    let text = match name {
        "devices" => include_str!("vectors/devices.json"),
        "bot" => include_str!("vectors/status_Bot.json"),
        "hub" => include_str!("vectors/status_Hub_2.json"),
        "entities" => include_str!("vectors/entity_registry_switchbot.json"),
        "registry_devices" => include_str!("vectors/device_registry_switchbot.json"),
        "areas" => include_str!("vectors/area_registry.json"),
        "states" => include_str!("vectors/startup_states.json"),
        "signing" => include_str!("vectors/signing.json"),
        _ => include_str!("vectors/services.json"),
    };
    serde_json::from_str(text).unwrap()
}

fn registries() -> Registries {
    let areas = fixture("areas")["areas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| ha_registry::Area {
            id: a["id"].as_str().unwrap().into(),
            name: a["name"].as_str().unwrap().into(),
            ..Default::default()
        })
        .collect();
    Registries {
        entities: serde_json::from_value(fixture("entities")).unwrap(),
        devices: serde_json::from_value(fixture("registry_devices")).unwrap(),
        areas,
        ..Default::default()
    }
}

const BOT: &str = "AABBCC000001";
const HUB: &str = "AABBCC000002";
const AC: &str = "01-202301010000-00000001";

/// A request as the fake server saw it.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// A reply: status, content type (none if empty), body.
type Reply = (u16, &'static str, String);

#[derive(Default)]
struct Server {
    seen: parking_lot::Mutex<Vec<Seen>>,
    /// Replies by `"METHOD path"`, instead of the real ones.
    replies: parking_lot::Mutex<HashMap<String, Reply>>,
}

impl Server {
    fn reply(&self, method: &str, path: &str) -> Reply {
        if let Some(r) = self.replies.lock().get(&format!("{method} {path}")) {
            return r.clone();
        }
        let ok = |v: Value| (200, "application/json; charset=utf-8", v.to_string());
        match (method, path) {
            ("GET", "/v1.1/devices") => ok(fixture("devices")),
            ("GET", p) if p == format!("/v1.1/devices/{BOT}/status") => ok(fixture("bot")),
            ("GET", p) if p == format!("/v1.1/devices/{HUB}/status") => ok(fixture("hub")),
            ("POST", p) if p.ends_with("/commands") => {
                ok(json!({"statusCode": 100, "body": {}, "message": "success"}))
            }
            _ => (404, "", String::new()),
        }
    }

    fn set(&self, method: &str, path: &str, reply: Reply) {
        self.replies
            .lock()
            .insert(format!("{method} {path}"), reply);
    }

    fn take(&self) -> Vec<Seen> {
        std::mem::take(&mut *self.seen.lock())
    }
}

/// A SwitchBot API on 127.0.0.1, closing each connection as the real one.
async fn fake() -> (String, Arc<Server>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = Arc::new(Server::default());
    let srv = server.clone();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let srv = srv.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let (head, end, len) = loop {
                    let mut chunk = [0u8; 4096];
                    let n = s.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                        let len: usize = head
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break (head, end, len);
                        }
                    }
                };
                let mut lines = head.lines();
                let mut first = lines.next().unwrap().split(' ');
                let (method, path) = (first.next().unwrap(), first.next().unwrap());
                let headers = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.to_lowercase(), v.trim().to_owned()))
                    .collect();
                let body = String::from_utf8_lossy(&buf[end + 4..end + 4 + len]).into_owned();
                let (status, ct, reply) = srv.reply(method, path);
                srv.seen.lock().push(Seen {
                    method: method.into(),
                    path: path.into(),
                    headers,
                    body,
                });
                let ct = match ct {
                    "" => String::new(),
                    ct => format!("Content-Type: {ct}\r\n"),
                };
                let res = format!(
                    "HTTP/1.1 {status} X\r\n{ct}Content-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = s.write_all(res.as_bytes()).await;
            });
        }
    });
    (base, server)
}

const TOKEN: &str = "faketoken0123456789";
const SECRET: &str = "fakesecret";

fn api(base: &str) -> Api {
    let client = ha_http::Client::builder(crate::USER_AGENT).build().unwrap();
    Api::new(client, base.to_owned(), TOKEN.into(), SECRET.into())
}

/// The entry set up against the fake server, its entities in the
/// components and written.
async fn started(hass: &Hass) -> (Arc<Entry>, Arc<Server>, Arc<Components>) {
    let (base, server) = fake().await;
    let registries = Arc::new(registries());
    let components = Components::new(hass, registries.clone(), Profiles::default());
    let entry = setup(hass, &registries, api(&base)).await.unwrap();
    for c in &entry.climates {
        components.climates().add(c.clone());
    }
    for s in &entry.switches {
        components.switches().add(s.clone());
    }
    for b in &entry.buttons {
        components.buttons().add(b.clone());
    }
    entry.write_all();
    (entry, server, components)
}

fn state(hass: &Hass, id: &str) -> Value {
    match hass.states().get(&EntityId::parse(id).unwrap()) {
        Some(s) => json!({"state": s.state.as_str(), "attributes": s.attributes}),
        None => Value::Null,
    }
}

#[test]
fn signs_as_the_library() {
    for case in fixture("signing").as_array().unwrap() {
        let h = &case["headers"];
        let t: i64 = h["t"].as_str().unwrap().parse().unwrap();
        let got = api::sign(
            case["token"].as_str().unwrap(),
            case["secret"].as_str().unwrap(),
            t,
            h["nonce"].as_str().unwrap(),
        );
        assert_eq!(got, h["sign"].as_str().unwrap(), "{case}");
    }
    let n = api::uuid4();
    assert_eq!(n.len(), 36);
    assert_eq!(&n[14..15], "4");
    assert!(matches!(&n[19..20], "8" | "9" | "a" | "b"), "{n}");
}

#[tokio::test]
async fn starts_with_has_entities() {
    let hass = Hass::new();
    let (_, server, _) = started(&hass).await;
    let want = fixture("states");
    for (id, s) in want.as_object().unwrap() {
        assert_eq!(state(&hass, id), *s, "{id}");
    }
    let seen = server.take();
    let paths: Vec<(&str, &str)> = seen
        .iter()
        .map(|s| (s.method.as_str(), s.path.as_str()))
        .collect();
    assert_eq!(
        paths,
        [
            ("GET", "/v1.1/devices"),
            ("GET", "/v1.1/devices/AABBCC000001/status"),
            ("GET", "/v1.1/devices/AABBCC000002/status"),
        ]
    );
    // Signed as the library signs, with its headers.
    for s in &seen {
        let h = |k: &str| {
            s.headers
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
                .unwrap_or_else(|| panic!("no {k}"))
        };
        assert_eq!(h("authorization"), TOKEN);
        assert_eq!(h("content-type"), "application/json");
        assert_eq!(h("charset"), "utf8");
        assert_eq!(h("t").len(), 13);
        let t = h("t").parse().unwrap();
        assert_eq!(h("sign"), api::sign(TOKEN, SECRET, t, h("nonce")));
        assert!(h("user-agent").starts_with("hars/"));
    }
}

/// HA's service sequence for the AC, switch and Bot, in order: the exact
/// request bodies, the states after, the errors.
#[tokio::test]
async fn services_send_what_ha_sends() {
    let hass = Hass::new();
    let (_, server, _) = started(&hass).await;
    server.take();
    for (i, case) in fixture("services").as_array().unwrap().iter().enumerate() {
        if let Some(full) = case.get("climate_final_full_state") {
            assert_eq!(state(&hass, "climate.air_conditioner"), *full);
            continue;
        }
        if case.get("call").is_none() {
            continue;
        }
        let (domain, service) = case["call"].as_str().unwrap().split_once('.').unwrap();
        let data = case["data"].as_object().unwrap().clone();
        let r = hass
            .call_service(domain, service, data, None, false, None)
            .await;
        match (&case["error"], r) {
            (Value::Null, Ok(_)) => {}
            (want, Err(e)) => {
                let kind = want["kind"].as_str().unwrap();
                let ok = match &e {
                    ServiceError::Invalid(_) => kind.starts_with("vol.Invalid"),
                    ServiceError::Validation(_) => kind.starts_with("ServiceValidationError"),
                    _ => false,
                };
                assert!(ok, "#{i} {case}: {e:?}");
                let msg = match &e {
                    ServiceError::Invalid(m) => m.clone(),
                    e => e.to_string(),
                };
                assert_eq!(msg, want["str"].as_str().unwrap(), "#{i}");
                if let ServiceError::Validation(r) = &e {
                    assert_eq!(r.key, want["translation_key"].as_str().unwrap(), "#{i}");
                }
            }
            (want, got) => panic!("#{i} {case}: want {want}, got {got:?}"),
        }
        let bodies: Vec<(String, String, String)> = server
            .take()
            .into_iter()
            .map(|s| (s.method, s.path, s.body))
            .collect();
        let want: Vec<(String, String, String)> = case["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["method"].as_str().unwrap().to_owned(),
                    r["path"].as_str().unwrap().to_owned(),
                    r["body"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(bodies, want, "#{i} {case}");
        let after = &case["after"];
        let ac = state(&hass, "climate.air_conditioner");
        let want_ac = &after["climate.air_conditioner"];
        assert_eq!(ac["state"], want_ac["state"], "#{i}");
        assert_eq!(
            ac["attributes"]["temperature"], want_ac["temperature"],
            "#{i}"
        );
        assert_eq!(ac["attributes"]["fan_mode"], want_ac["fan_mode"], "#{i}");
        assert_eq!(
            state(&hass, "switch.air_conditioner")["state"],
            after["switch.air_conditioner"],
            "#{i}"
        );
        let button = state(&hass, "button.towel_switch")["state"].clone();
        match after["button.towel_switch"].as_str().unwrap() {
            "unknown" => assert_eq!(button, "unknown", "#{i}"),
            _ => assert!(button.as_str().unwrap().ends_with("+00:00"), "#{i}"),
        }
    }
    // Never the webhook endpoints.
    assert!(server.take().iter().all(|s| !s.path.contains("webhook")));
}

#[tokio::test]
async fn command_errors_as_ha() {
    let hass = Hass::new();
    let (_, server, _) = started(&hass).await;
    let call = |domain: &'static str, service: &'static str, id: &'static str| {
        let hass = hass.clone();
        async move {
            let data = json!({ "entity_id": id }).as_object().cloned().unwrap();
            hass.call_service(domain, service, data, None, false, None)
                .await
                .map(|_| ())
        }
    };
    let offline = (
        200,
        "application/json",
        json!({"statusCode": 161, "body": {}, "message": "device offline"}).to_string(),
    );
    server.set(
        "POST",
        &format!("/v1.1/devices/{AC}/commands"),
        offline.clone(),
    );
    server.set("POST", &format!("/v1.1/devices/{BOT}/commands"), offline);
    match call("climate", "turn_off", "climate.air_conditioner").await {
        Err(ServiceError::Translated(r)) => {
            assert_eq!(r.message, "SwitchBot Cloud device is offline");
            assert_eq!(
                (r.domain.as_str(), r.key.as_str()),
                (DOMAIN, "device_offline")
            );
            assert_eq!(r.placeholders, None);
        }
        other => panic!("{other:?}"),
    }
    // Unchanged on failure.
    assert_eq!(state(&hass, "climate.air_conditioner")["state"], "fan_only");
    // The Bot's errors escape as they are, after the press was recorded.
    assert_eq!(
        call("button", "press", "button.towel_switch").await,
        Err(ServiceError::Unknown(String::new()))
    );
    assert_ne!(state(&hass, "button.towel_switch")["state"], "unknown");

    let error_190 = (
        200,
        "application/json",
        json!({"statusCode": 190, "body": {}, "message": "x"}).to_string(),
    );
    server.set("POST", &format!("/v1.1/devices/{AC}/commands"), error_190);
    match call("switch", "turn_on", "switch.air_conditioner").await {
        Err(ServiceError::Translated(r)) => {
            assert_eq!(r.message, "Error communicating with SwitchBot Cloud: ");
            assert_eq!(r.key, "connection_error");
            assert_eq!(
                r.placeholders,
                Some(json!({"error": ""}).as_object().cloned().unwrap())
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(state(&hass, "switch.air_conditioner")["state"], "unknown");
    server.set(
        "POST",
        &format!("/v1.1/devices/{AC}/commands"),
        (403, "application/json", "{}".into()),
    );
    match call("switch", "turn_on", "switch.air_conditioner").await {
        Err(ServiceError::Translated(r)) => {
            assert_eq!(r.message, "SwitchBot Cloud command failed: ");
            assert_eq!(r.key, "command_failed");
        }
        other => panic!("{other:?}"),
    }
    // A success with no `body`: `body["body"]` fails, nothing changes.
    server.set(
        "POST",
        &format!("/v1.1/devices/{AC}/commands"),
        (200, "application/json", r#"{"statusCode":100}"#.into()),
    );
    assert_eq!(
        call("switch", "turn_on", "switch.air_conditioner").await,
        Err(ServiceError::Unknown("KeyError: 'body'".into()))
    );
    assert_eq!(state(&hass, "switch.air_conditioner")["state"], "unknown");
    // The real bad-token reply: a 401 with no content type.
    server.set(
        "POST",
        &format!("/v1.1/devices/{AC}/commands"),
        (401, "", r#"{"message":"Unauthorized"}"#.into()),
    );
    match call("switch", "turn_on", "switch.air_conditioner").await {
        Err(ServiceError::Unknown(m)) => assert!(
            m.starts_with(
                "401, message='Attempt to decode JSON with unexpected mimetype: none', url='"
            ),
            "{m}"
        ),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn polls_and_recovers() {
    let hass = Hass::new();
    let (entry, server, _) = started(&hass).await;
    let temp = || state(&hass, "sensor.hub_2_9d_temperature")["state"].clone();
    let hum = || state(&hass, "sensor.hub_2_9d_humidity")["state"].clone();
    let hub = format!("/v1.1/devices/{HUB}/status");
    let reply = |body: Value| {
        (
            200,
            "application/json",
            json!({"statusCode": 100, "body": body, "message": "success"}).to_string(),
        )
    };
    server.set(
        "GET",
        &hub,
        (
            200,
            "application/json",
            json!({"statusCode": 161, "body": {}}).to_string(),
        ),
    );
    entry.poll().await;
    assert_eq!(
        (temp(), hum()),
        (json!("unavailable"), json!("unavailable"))
    );
    // The Bot's entities are unaffected.
    assert_eq!(state(&hass, "sensor.towel_switch_battery")["state"], "100");
    // An empty body: the library's `response["deviceType"]` fails.
    server.set("GET", &hub, reply(json!({})));
    entry.poll().await;
    assert_eq!(
        (temp(), hum()),
        (json!("unavailable"), json!("unavailable"))
    );
    // A missing key is unknown; ints and floats keep their form.
    server.set(
        "GET",
        &hub,
        reply(json!({"humidity": 55, "deviceType": "Hub 2"})),
    );
    entry.poll().await;
    assert_eq!((temp(), hum()), (json!("unknown"), json!("55")));
    server.set(
        "GET",
        &hub,
        reply(json!({"temperature": 21, "humidity": 49.0, "deviceType": "Hub 2"})),
    );
    entry.poll().await;
    assert_eq!((temp(), hum()), (json!("21"), json!("49.0")));
    server.set(
        "GET",
        &hub,
        reply(json!({"temperature": -3.25, "humidity": 100, "deviceType": "Hub 2"})),
    );
    entry.poll().await;
    assert_eq!((temp(), hum()), (json!("-3.25"), json!("100")));
    // A Bot that stops answering: its button and battery go unavailable,
    // and a press then reaches nothing.
    server.set(
        "GET",
        &format!("/v1.1/devices/{BOT}/status"),
        (500, "text/html", "<html>".into()),
    );
    entry.poll().await;
    assert_eq!(state(&hass, "button.towel_switch")["state"], "unavailable");
    assert_eq!(
        state(&hass, "sensor.towel_switch_battery")["state"],
        "unavailable"
    );
    server.take();
    let data = json!({"entity_id": "button.towel_switch"})
        .as_object()
        .cloned()
        .unwrap();
    hass.call_service("button", "press", data, None, false, None)
        .await
        .unwrap();
    assert!(server.take().iter().all(|s| s.method == "GET"));
}

#[tokio::test]
async fn setup_failures_as_ha() {
    let hass = Hass::new();
    let registries = registries();
    let (base, server) = fake().await;
    let json_ct = "application/json";
    for (reply, want) in [
        (
            (403, json_ct, "{}".to_owned()),
            SetupError::Fatal("Invalid authentication while connecting to SwitchBot API: ".into()),
        ),
        (
            (500, json_ct, r#"{"message":"x"}"#.to_owned()),
            SetupError::Retry(String::new()),
        ),
        (
            (200, json_ct, r#"{"statusCode":190,"body":{}}"#.to_owned()),
            SetupError::Retry(String::new()),
        ),
    ] {
        server.set("GET", "/v1.1/devices", reply);
        assert_eq!(
            setup(&hass, &registries, api(&base)).await.err(),
            Some(want)
        );
    }
    for reply in [
        (401, "", r#"{"message":"Unauthorized"}"#.to_owned()),
        (500, "text/html", "<html>".to_owned()),
        (200, json_ct, r#"{"statusCode":161,"body":{}}"#.to_owned()),
        // No device lists: HA can't iterate them.
        (200, json_ct, r#"{"statusCode":100,"body":{}}"#.to_owned()),
    ] {
        server.set("GET", "/v1.1/devices", reply);
        assert!(matches!(
            setup(&hass, &registries, api(&base)).await,
            Err(SetupError::Fatal(_))
        ));
    }
    // A device's first status failing retries the whole entry.
    server.replies.lock().clear();
    server.set(
        "GET",
        &format!("/v1.1/devices/{HUB}/status"),
        (200, json_ct, r#"{"statusCode":190,"body":{}}"#.to_owned()),
    );
    assert_eq!(
        setup(&hass, &registries, api(&base)).await.err(),
        Some(SetupError::Retry("Error communicating with API: ".into()))
    );
    // Nothing listening.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    assert_eq!(
        setup(&hass, &registries, api(&closed)).await.err(),
        Some(SetupError::Retry(
            "Error occurred while communicating with the SwitchBot API".into()
        ))
    );
    assert!(server.take().iter().all(|s| !s.path.contains("webhook")));
}

/// `core.restore_state` rows for the climate, then what the next command
/// sends.
#[tokio::test]
async fn climate_restores_as_ha() {
    let cases = [
        (
            json!({"state": "off", "attributes": {"temperature": 19, "fan_mode": "low"}}),
            Some(("off", json!(19), "19,4,3,on")),
        ),
        (
            json!({"state": "heat", "attributes": {"temperature": 22.5, "fan_mode": "auto"}}),
            Some(("heat", json!(22), "22,5,3,on")),
        ),
        (
            json!({"state": "dry", "attributes": {}}),
            Some(("dry", json!(21), "21,3,3,on")),
        ),
        (
            json!({"state": "auto", "attributes": {}}),
            Some(("auto", json!(21), "21,4,3,on")),
        ),
        (
            json!({"state": "unavailable", "attributes": {}}),
            Some(("fan_only", json!(21), "21,4,3,on")),
        ),
        (json!({"state": "bogus", "attributes": {}}), None),
    ];
    for (n, (row, want)) in cases.into_iter().enumerate() {
        let dir = std::env::temp_dir().join(format!("hars-switchbot-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut st = row.clone();
        st["entity_id"] = json!("climate.air_conditioner");
        ha_core::storage::Store::new(&dir, "core.restore_state", 1, 1, false)
            .save(&json!([{"state": st, "extra_data": null, "last_seen": "2099-01-01T00:00:00+00:00"}]))
            .unwrap();
        let hass = Hass::new();
        hass.set_restore(ha_core::restore::RestoreState::load(&dir));
        let (_, server, _) = started(&hass).await;
        let ac = state(&hass, "climate.air_conditioner");
        let Some((mode, shown, sent)) = want else {
            assert_eq!(ac, Value::Null, "#{n}");
            let _ = std::fs::remove_dir_all(&dir);
            continue;
        };
        assert_eq!(ac["state"], mode, "#{n}");
        assert_eq!(ac["attributes"]["temperature"], shown, "#{n}");
        server.take();
        let data = json!({"entity_id": "climate.air_conditioner", "fan_mode": "medium"});
        hass.call_service(
            "climate",
            "set_fan_mode",
            data.as_object().cloned().unwrap(),
            None,
            false,
            None,
        )
        .await
        .unwrap();
        let seen = server.take();
        assert_eq!(
            seen[0].body,
            format!(r#"{{"commandType":"command","command":"setAll","parameter":"{sent}"}}"#),
            "#{n}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Every message logged at `info` or above (what `hars` prints unless
/// asked for more) while this is alive, on this thread.
#[derive(Clone, Default)]
struct Logged(Arc<parking_lot::Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Logged {
    fn on_event(&self, e: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                if f.name() == "message" {
                    self.0 = format!("{v:?}");
                }
            }
        }
        if *e.metadata().level() > tracing::Level::INFO {
            return;
        }
        let mut m = Message(String::new());
        e.record(&mut m);
        self.0.lock().push(m.0);
    }
}

/// An API error is logged with its HTTP status and the API's own
/// `statusCode`, never with the reply's body: a body can echo the request
/// back, the account's token and signature with it.
#[tokio::test]
async fn an_api_error_is_logged_without_its_body() {
    use tracing_subscriber::layer::SubscriberExt;
    let logged = Logged::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(logged.clone()));
    let (base, server) = fake().await;
    let echo = |code: &str| {
        format!(
            r#"{{{code}"message":"bad request","request":{{"Authorization":"{TOKEN}","sign":"{SECRET}"}}}}"#
        )
    };
    let json_ct = "application/json";
    for (status, body, want) in [
        (500, echo(""), "Error 500"),
        (
            400,
            echo(r#""statusCode":190,"#),
            "Error 400: statusCode 190",
        ),
        (
            200,
            echo(r#""statusCode":190,"#),
            "Error 200: statusCode 190",
        ),
        (200, echo(r#""statusCode":"oops","#), "Error 200"),
    ] {
        server.set("GET", "/v1.1/devices", (status, json_ct, body));
        assert_eq!(
            api(&base).list_devices().await,
            Err(ApiError::Connection(String::new()))
        );
        let lines = std::mem::take(&mut *logged.0.lock());
        assert_eq!(lines, [want]);
    }
    // Whatever else fails, nothing logged or returned names the token.
    for reply in [
        (403, json_ct, echo("")),
        (200, "text/html", format!("<html>{TOKEN}</html>")),
        (200, json_ct, format!(r#"["{TOKEN}"]"#)),
        (
            200,
            json_ct,
            format!(r#"{{"statusCode":100,"body":{{"x":"{TOKEN}"}}}}"#),
        ),
    ] {
        server.set("GET", "/v1.1/devices", reply);
        let error = api(&base).list_devices().await.unwrap_err();
        let said = format!("{error:?} {error} {:?}", logged.0.lock());
        assert!(!said.contains(TOKEN) && !said.contains(SECRET), "{said}");
    }
}

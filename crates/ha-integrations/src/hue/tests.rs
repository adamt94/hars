//! Against the real bridge snapshot and vectors made by running HA
//! 2026.6.1's own code on it (see `vectors/`).

use super::*;
use crate::light::Profiles;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

thread_local! {
    /// What this thread does between an entity's state being made and
    /// being written ([`Bridge::write_entity`]), once.
    static AFTER_RENDER: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

pub(super) fn after_render() {
    let hook = AFTER_RENDER.with(|hook| hook.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

fn fixture(name: &str) -> Value {
    let text = match name {
        "resource" => include_str!("vectors/resource.json"),
        "entities" => include_str!("vectors/entity_registry_hue.json"),
        "devices" => include_str!("vectors/device_registry_hue.json"),
        "areas" => include_str!("vectors/areas.json"),
        "snapshot" => include_str!("vectors/entities_snapshot.json"),
        "sse" => include_str!("vectors/sse_scenarios.json"),
        _ => include_str!("vectors/service_calls.json"),
    };
    serde_json::from_str(text).unwrap()
}

fn registries() -> Registries {
    let areas = fixture("areas")
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
        devices: serde_json::from_value(fixture("devices")).unwrap(),
        areas,
        ..Default::default()
    }
}

/// A bridge on the snapshot, with its entities made and written.
fn bridge(api: Api) -> Arc<Bridge> {
    let hass = Hass::new();
    let registries = Arc::new(registries());
    let platforms = Platforms::new(&crate::components::Components::new(
        &hass,
        registries.clone(),
        Profiles::default(),
    ));
    let resources = fixture("resource")["data"].as_array().unwrap().clone();
    let bridge = Arc::new(Bridge {
        hass,
        registries: RwLock::new(registries),
        names: Default::default(),
        api,
        store: RwLock::new(Store::new(resources)),
        ignore_availability: Vec::new(),
        lights: RwLock::default(),
        groups: RwLock::default(),
        others: RwLock::default(),
        platforms: platforms.clone(),
        alive: parking_lot::Mutex::new(true),
    });
    platforms.bridges.write().push(Arc::downgrade(&bridge));
    bridge.add_entities();
    bridge
}

fn offline() -> Api {
    Api::at("http://127.0.0.1:9", "key")
}

/// HA's set-valued group attributes, sorted (their order is Python's set
/// order, which varies between runs).
fn normalize(mut attrs: Value) -> Value {
    for key in ["hue_scenes", "lights", "entity_id"] {
        if let Some(Value::Array(a)) = attrs.get_mut(key) {
            a.sort_by_key(|x| x.to_string());
        }
    }
    attrs
}

fn state_json(hass: &Hass, id: &str) -> Option<(String, Value)> {
    let s = hass.states().get(&EntityId::parse(id).ok()?)?;
    Some((
        s.state.to_string(),
        normalize(Value::Object(s.attributes.clone())),
    ))
}

#[tokio::test]
async fn every_entity_matches_ha_on_the_real_bridge() {
    let b = bridge(offline());
    let mut checked = 0;
    for e in fixture("snapshot").as_array().unwrap() {
        let id = e["entity_id"].as_str().unwrap();
        let got = state_json(&b.hass, id);
        if !e["registry_disabled_by"].is_null() {
            assert!(got.is_none(), "{id} is disabled");
            continue;
        }
        let (state, attrs) = got.unwrap_or_else(|| panic!("{id} has no state"));
        assert_eq!(state, e["state"], "{id}");
        assert_eq!(attrs, normalize(e["attributes"].clone()), "{id}");
        checked += 1;
    }
    assert_eq!(checked, 19);
}

#[tokio::test]
async fn event_stream_items_write_what_ha_writes() {
    for (name, scenario) in fixture("sse").as_object().unwrap() {
        let b = bridge(offline());
        // HA writes every time; a write that changes nothing fires no
        // `state_changed`, so the expected events skip those.
        let mut current: std::collections::HashMap<String, (String, Value)> = b
            .hass
            .states()
            .all()
            .iter()
            .map(|s| {
                (
                    s.entity_id.to_string(),
                    (
                        s.state.to_string(),
                        normalize(Value::Object(s.attributes.clone())),
                    ),
                )
            })
            .collect();
        let mut want = Vec::new();
        for w in scenario["writes"].as_array().unwrap() {
            let id = w["entity_id"].as_str().unwrap();
            let new = (
                w["state"].as_str().unwrap().to_owned(),
                normalize(w["attributes"].clone()),
            );
            if current.get(id) != Some(&new) {
                want.push((id.to_owned(), new.0.clone(), new.1.clone()));
                current.insert(id.to_owned(), new);
            }
        }
        let mut rx = b.hass.bus().subscribe();
        for line in scenario["sse_lines"].as_array().unwrap() {
            if let Some(data) = line.as_str().unwrap().strip_prefix("data:") {
                b.handle_data(data.trim());
            }
        }
        let mut got = Vec::new();
        while let Ok(e) = rx.try_recv() {
            if let ha_core::EventKind::StateChanged {
                entity_id,
                new_state: Some(s),
                ..
            } = &e.kind
            {
                got.push((
                    entity_id.to_string(),
                    s.state.to_string(),
                    normalize(Value::Object(s.attributes.clone())),
                ));
            }
        }
        assert_eq!(got, want, "{name}");
    }
}

/// A bridge that answers every request `{"data":[],"errors":[]}` and keeps
/// (method, path, body) of each.
async fn fake_bridge() -> (
    String,
    Arc<parking_lot::Mutex<Vec<(String, String, Value)>>>,
) {
    fake_bridge_with(|_| 200).await
}

/// [`fake_bridge`], answering a request with the status `status` gives
/// for its body.
async fn fake_bridge_with(
    status: fn(&Value) -> u16,
) -> (
    String,
    Arc<parking_lot::Mutex<Vec<(String, String, Value)>>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let log = log.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                loop {
                    let mut chunk = [0u8; 4096];
                    let Ok(n) = s.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    // Every complete request in the buffer.
                    while let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                        let len: usize = head
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        if buf.len() < end + 4 + len {
                            break;
                        }
                        let body: Value = serde_json::from_slice(&buf[end + 4..end + 4 + len])
                            .unwrap_or(Value::Null);
                        let mut first = head.lines().next().unwrap().split(' ');
                        let (method, path) = (
                            first.next().unwrap().to_owned(),
                            first.next().unwrap().to_owned(),
                        );
                        let status = status(&body);
                        log.lock()
                            .push((method, path.trim_start_matches('/').to_owned(), body));
                        buf.drain(..end + 4 + len);
                        let reply = r#"{"data":[],"errors":[]}"#;
                        let res = format!(
                            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{reply}",
                            reply.len()
                        );
                        if s.write_all(res.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    (base, seen)
}

#[tokio::test]
async fn services_send_what_aiohue_sends() {
    let (base, seen) = fake_bridge().await;
    let b = bridge(Api::at(&base, "key"));
    let mut checked = 0;
    for (i, case) in fixture("calls").as_array().unwrap().iter().enumerate() {
        let service = case["service"].as_str().unwrap();
        let id = case["entity_id"].as_str().unwrap_or_default();
        seen.lock().clear();
        if service == "sse" {
            let mut item = case["data"].clone();
            item["id"] = json!(id);
            item["type"] = json!("light");
            b.handle("update", &item);
            continue;
        }
        let (domain, name) = service.split_once('.').unwrap();
        let mut data = case["data"].as_object().unwrap().clone();
        if !id.is_empty() {
            data.insert("entity_id".into(), json!(id));
        }
        b.hass
            .call_service(domain, name, data, None, false, None)
            .await
            .unwrap_or_else(|e| panic!("#{}: {e}", i + 1));
        let mut got: Vec<Value> = seen
            .lock()
            .iter()
            .map(|(m, p, body)| json!({"method": m, "path": p, "json": body}))
            .collect();
        let mut want: Vec<Value> = case["requests"].as_array().unwrap().clone();
        // Concurrent identifies (#16) come in any order.
        got.sort_by_key(|v| v.to_string());
        want.sort_by_key(|v| v.to_string());
        assert_eq!(got, want, "#{} {service} {}", i + 1, case["data"]);
        checked += 1;
        // The bridge's echo of on/dimming, as the vectors emulate it.
        let puts: Vec<(String, Value)> = seen
            .lock()
            .iter()
            .map(|(_, p, body)| (p.clone(), body.clone()))
            .collect();
        for (path, body) in puts {
            if body.get("on").is_none() {
                continue;
            }
            let rid = path.rsplit('/').next().unwrap();
            let ty = if path.contains("/grouped_light/") {
                "grouped_light"
            } else {
                "light"
            };
            let mut item = json!({"id": rid, "type": ty, "on": body["on"]});
            if let Some(d) = body.get("dimming") {
                item["dimming"] = d.clone();
            }
            b.handle("update", &item);
        }
    }
    assert_eq!(checked, 59);
}

/// aiohue sends a transition only with no effect or `no_effect`: a named
/// effect drops it, a timed one uses it as its duration.
#[tokio::test]
async fn transitions_go_with_effects_as_aiohue_sends_them() {
    let (base, seen) = fake_bridge().await;
    let b = bridge(Api::at(&base, "key"));
    for (data, want) in [
        (
            json!({"effect": "candle", "transition": 2}),
            json!({"on": {"on": true}, "effects": {"effect": "candle"}}),
        ),
        (
            json!({"effect": "sunset", "transition": 2}),
            json!({"on": {"on": true}, "timed_effects": {"effect": "sunset", "duration": 2000}}),
        ),
    ] {
        seen.lock().clear();
        let mut data = data.as_object().unwrap().clone();
        data.insert("entity_id".into(), json!("light.bedroom_lamp"));
        b.hass
            .call_service("light", "turn_on", data, None, false, None)
            .await
            .unwrap();
        assert_eq!(seen.lock()[0].2, want);
    }
}

/// The stream: connect, reconcile with a fresh fetch (the lamp was turned
/// on while we weren't listening), apply events, then on reconnect resume
/// from the last event id.
#[tokio::test]
async fn follows_the_stream_reconciles_and_resumes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mut fresh = fixture("resource");
    for r in fresh["data"].as_array_mut().unwrap() {
        if r["id"] == "81c174c4-7664-4c0e-9604-d453834a8688" {
            r["on"] = json!({"on": true});
        }
    }
    let resources = serde_json::to_vec(&fresh).unwrap();
    let streams = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
    let log = streams.clone();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = s.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_lowercase();
            if req.starts_with("get /clip/v2/resource") {
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    resources.len()
                );
                s.write_all(head.as_bytes()).await.unwrap();
                s.write_all(&resources).await.unwrap();
            } else {
                let first = log.lock().is_empty();
                log.lock().push(req);
                s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n: hi\n\n")
                    .await
                    .unwrap();
                if first {
                    // The filament bulb 1 goes off, then the stream drops.
                    let ev = json!([{"id": "e1", "type": "update", "creationtime": "2026-11-14T14:13:08Z",
                        "data": [{"id": "8d3138ee-1c09-4003-800d-33d7789e7d1e", "type": "light", "on": {"on": false}}]}]);
                    s.write_all(format!("id: 1794665588:0\ndata: {ev}\n\n").as_bytes())
                        .await
                        .unwrap();
                } else {
                    // Stay connected.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
        }
    });
    let b = bridge(Api::at(&base, "key"));
    let task = tokio::spawn(b.clone().listen());
    let state = |id: &str| state_json(&b.hass, id).map(|s| s.0);
    // Reconciled: the lamp is on; the event: the bulb is off.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while streams.lock().len() < 2 {
        assert!(tokio::time::Instant::now() < deadline, "no reconnect");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(state("light.bedroom_lamp").as_deref(), Some("on"));
    assert_eq!(
        state("light.hallway_hue_filament_bulb_1").as_deref(),
        Some("off")
    );
    let s = streams.lock();
    assert!(s[0].starts_with("get /eventstream/clip/v2"), "{}", s[0]);
    assert!(
        s[0].contains("accept: text/event-stream") && s[0].contains("hue-application-key: key")
    );
    assert!(!s[0].contains("last-event-id"));
    assert!(s[1].contains("last-event-id: 1794665588:0"), "{}", s[1]);
    task.abort();
}

const LAMP: &str = "81c174c4-7664-4c0e-9604-d453834a8688";
const LAMP_DEVICE: &str = "13592c33-2ede-42f1-8e4a-c0e241bf29c4";

/// A device gone while disconnected takes its lights with it on the
/// reconcile: their states, and their place as service targets.
#[tokio::test]
async fn a_reconcile_removes_what_the_bridge_no_longer_has() {
    let (base, seen) = fake_bridge().await;
    let b = bridge(Api::at(&base, "key"));
    assert!(state_json(&b.hass, "light.bedroom_lamp").is_some());
    let fresh: Vec<Value> = fixture("resource")["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["id"] != LAMP && r["id"] != LAMP_DEVICE && r["owner"]["rid"] != LAMP_DEVICE)
        .cloned()
        .collect();
    b.apply_snapshot(fresh);
    assert!(state_json(&b.hass, "light.bedroom_lamp").is_none());
    let mut data = Map::new();
    data.insert("entity_id".into(), json!("all"));
    b.hass
        .call_service("light", "turn_on", data, None, false, None)
        .await
        .unwrap();
    assert!(
        seen.lock().iter().all(|(_, p, _)| !p.ends_with(LAMP)),
        "{:?}",
        seen.lock()
    );
    assert_eq!(seen.lock().len(), 7);
}

/// Two new lights on same-named devices get different entity ids, and a
/// group whose room arrives after it is still made.
#[tokio::test]
async fn new_resources_get_their_own_ids_and_late_rooms_their_groups() {
    let b = bridge(offline());
    // Both arrive in one reconcile, so both are made before either is
    // written.
    let mut fresh = fixture("resource")["data"].as_array().unwrap().clone();
    for n in 1..=2 {
        let (device, light) = (format!("dev-{n}"), format!("light-{n}"));
        fresh.push(
            json!({"id": device, "type": "device", "metadata": {"name": "Desk"},
            "product_data": {"certified": true}, "services": [{"rid": light, "rtype": "light"}]}),
        );
        fresh.push(
            json!({"id": light, "type": "light", "owner": {"rid": device, "rtype": "device"},
            "on": {"on": true}, "dimming": {"brightness": 50.0}, "mode": "normal",
            "dynamics": {"status": "none"}}),
        );
    }
    b.apply_snapshot(fresh);
    assert_eq!(state_json(&b.hass, "light.desk").unwrap().0, "on");
    assert_eq!(state_json(&b.hass, "light.desk_2").unwrap().0, "on");
    // The group before its room.
    b.handle(
        "add",
        &json!({"id": "gl-new", "type": "grouped_light", "owner": {"rid": "room-new", "rtype": "room"}, "on": {"on": true}}),
    );
    b.handle(
        "add",
        &json!({"id": "room-new", "type": "room", "metadata": {"name": "Study"},
            "children": [{"rid": "dev-1", "rtype": "device"}],
            "services": [{"rid": "gl-new", "rtype": "grouped_light"}]}),
    );
    let (state, attrs) = state_json(&b.hass, "light.study").unwrap();
    assert_eq!(
        (state.as_str(), attrs["entity_id"].clone()),
        ("on", json!([]))
    );
    assert_eq!(attrs["lights"], json!(["Desk"]));
}

/// A server that answers every request with `reply`.
async fn answering(reply: String) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = s.read(&mut buf).await;
            let _ = s.write_all(reply.as_bytes()).await;
        }
    });
    base
}

/// Valid JSON of the wrong shape is no data, not a panic. An answer that
/// doesn't say it is JSON is aiohttp's `ContentTypeError`, which HA's
/// set-up tries again; one that says so and isn't is a failure it
/// doesn't.
#[tokio::test]
async fn odd_bridge_answers_are_not_a_crash() {
    let answer = |kind: &str, body: &str| {
        let len = body.len();
        format!("HTTP/1.1 200 OK\r\n{kind}Content-Length: {len}\r\nConnection: close\r\n\r\n{body}")
    };
    let json = "Content-Type: application/json; charset=utf-8\r\n";
    let api = Api::at(&answering(answer(json, "[]")).await, "key");
    assert!(api.fetch_all().await.unwrap().is_empty());
    for kind in ["", "Content-Type: text/html\r\n"] {
        let api = Api::at(&answering(answer(kind, r#"{"data":[]}"#)).await, "key");
        let err = api.fetch_all().await.unwrap_err();
        assert!(matches!(err, ApiError::Connection(_)), "{kind}: {err:?}");
    }
    let api = Api::at(&answering(answer(json, "<html>")).await, "key");
    let err = api.fetch_all().await.unwrap_err();
    assert!(matches!(err, ApiError::Decode(_)), "{err:?}");
}

/// A redirect the client refuses names where it led. If that names the
/// key, the error of a request, and of the event stream (which is
/// logged), has it replaced.
#[tokio::test]
async fn an_error_that_names_the_key_has_it_replaced() {
    const KEY: &str = "fake-application-key-0001";
    let reply = format!(
        "HTTP/1.1 302 Found\r\nLocation: http://[bad/{KEY}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let api = Api::at(&answering(reply).await, KEY);
    let request = api.fetch_all().await.unwrap_err().to_string();
    let stream = api.events(None).await.err().unwrap().to_string();
    for text in [request, stream] {
        assert!(text.contains("http://[bad/***"), "{text}");
        assert!(!text.contains(KEY), "{text}");
    }
    // A status error names the request's URL, and with it a resource's
    // id, which the bridge gave: a service's error would show it.
    let reply = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let api = Api::at(&answering(reply.to_owned()).await, KEY);
    let err = api.put("scene", KEY, &json!({})).await.unwrap_err();
    assert!(matches!(err, ApiError::Status { .. }), "{err:?}");
    let text = err.service_message().unwrap();
    assert!(text.contains("/clip/v2/resource/scene/***"), "{text}");
    assert!(!text.contains(KEY), "{text}");
}

/// The bridge's device is the one with the `bridge` resource itself among
/// its services (aiohue's `bridge_device`), not any device with a service
/// of that type.
#[test]
fn the_bridges_device_has_the_bridge_resource() {
    let device = |rid: &str| {
        json!({"id": "d1", "type": "device",
               "services": [{"rid": rid, "rtype": "bridge"}]})
    };
    let bridge = json!({"id": "b1", "type": "bridge", "bridge_id": "001788fffe000001"});
    let store = Store::new(vec![bridge.clone(), device("b1")]);
    assert_eq!(store.bridge_device().unwrap()["id"], "d1");
    let store = Store::new(vec![bridge, device("another")]);
    assert!(store.bridge_device().is_none());
    assert!(Store::new(vec![device("b1")]).bridge_device().is_none());
}

/// A stopped bridge writes nothing, whoever still holds one of its
/// entities: a scene that is activated while its entry is removed (the
/// service call got the scene before) can't put back the state the
/// removal took, and `hue.hue_activate_scene` no longer finds the bridge.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_bridge_writes_nothing() {
    let (base, seen) = fake_bridge().await;
    let b = bridge(Api::at(&base, "key"));
    let scenes: Vec<Arc<HueEntity>> = b
        .others
        .read()
        .iter()
        .filter(|e| e.entity_id.domain() == "scene")
        .cloned()
        .collect();
    assert!(!scenes.is_empty() && !b.hass.states().all().is_empty());
    // Writes at every moment of the stop.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (b, scenes, stop) = (b.clone(), scenes.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                for scene in &scenes {
                    b.write_entity(scene);
                }
            }
        })
    };
    b.stop();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
    let left = |b: &Bridge| -> Vec<String> {
        let all = b.hass.states().all();
        all.iter().map(|s| s.entity_id.to_string()).collect()
    };
    assert_eq!(left(&b), Vec::<String>::new());
    // After: an activation writes no time and sends nothing, and the
    // service finds no bridge.
    let late = scenes[0].activate(None, false, None, None).await;
    assert!(late.is_err(), "{late:?}");
    assert_eq!(left(&b), Vec::<String>::new());
    assert!(seen.lock().is_empty(), "{:?}", seen.lock());
    let data = json!({"group_name": "bedroom", "scene_name": "dimmed"});
    let Value::Object(data) = data else { panic!() };
    let _ = b
        .hass
        .call_service("hue", "hue_activate_scene", data, None, false, None)
        .await;
    assert!(seen.lock().is_empty(), "{:?}", seen.lock());
}

/// A command that is waiting to be sent again when its entry is removed
/// ends there: the service call holds the bridge and its HTTP client
/// itself, and would go on trying (aiohue's 25 attempts) and then send
/// what follows. Here a scene is activated with a speed, the bridge is
/// busy (503), and the entry is stopped during the wait.
#[tokio::test]
async fn a_command_ends_when_its_entry_is_removed() {
    let busy = |body: &Value| match body.get("speed") {
        Some(_) => 503,
        None => 200,
    };
    let (base, seen) = fake_bridge_with(busy).await;
    let b = bridge(Api::at(&base, "key"));
    let scene = b
        .others
        .read()
        .iter()
        .find(|e| e.entity_id.as_str() == "scene.bedroom_bedroom_dimmed")
        .cloned()
        .unwrap();
    let call = tokio::spawn(async move { scene.activate(None, true, Some(50), None).await });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while seen.lock().is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "nothing was sent");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    b.stop();
    // The call ends at once, not after its next attempt (0.5 s later).
    let ended = tokio::time::timeout(Duration::from_millis(250), call).await;
    assert!(matches!(ended, Ok(Ok(Err(_)))), "{ended:?}");
    // And neither the speed again nor the recall is ever sent.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let seen = seen.lock();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].2, json!({"speed": 0.5}));
}

/// A scene's state is made on the service call's task and written after.
/// If the event stream takes the scene away in between, the state is not
/// written: a scene that came after set-up has no registry row, so the
/// state would outlive the entry.
#[tokio::test]
async fn a_scene_deleted_while_it_is_activated_leaves_no_state() {
    let b = bridge(offline());
    let id = "scene.bedroom_bedroom_dimmed";
    let scene = b
        .others
        .read()
        .iter()
        .find(|e| e.entity_id.as_str() == id)
        .cloned()
        .unwrap();
    assert!(state_json(&b.hass, id).is_some());
    let deleted = json!([{"creationtime": "2026-01-01T00:00:00Z", "id": "gone", "type": "delete",
                          "data": [{"id": scene.rid, "type": "scene"}]}]);
    let stream = b.clone();
    AFTER_RENDER.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || stream.handle_data(&deleted.to_string())));
    });
    let _ = scene.activate(None, false, None, None).await;
    assert!(
        AFTER_RENDER.with(|hook| hook.borrow().is_none()),
        "not reached"
    );
    assert_eq!(state_json(&b.hass, id), None);
    assert!(b.others.read().iter().all(|e| e.entity_id.as_str() != id));
}

/// The registries `b` has, with `change` made to a copy of each row.
fn changed(
    b: &Bridge,
    device: impl Fn(&mut ha_registry::Device),
    entity: impl Fn(&mut ha_registry::Entity),
) -> Arc<Registries> {
    let mut r = (*b.rows()).clone();
    for d in &mut r.devices {
        let mut row = (**d).clone();
        device(&mut row);
        *d = Arc::new(row);
    }
    for e in &mut r.entities {
        let mut row = (**e).clone();
        entity(&mut row);
        *e = Arc::new(row);
    }
    Arc::new(r)
}

/// The scene `id` and the Hue id of its room.
fn scene_and_room(b: &Bridge, id: &str) -> (Arc<HueEntity>, String) {
    let scene = b
        .others
        .read()
        .iter()
        .find(|e| e.entity_id.as_str() == id)
        .cloned();
    let scene = scene.unwrap();
    let store = b.store.read();
    let r = store.get(&scene.rid).unwrap();
    let room = HueEntity::device(scene.kind, &scene.rid, r, &store).unwrap();
    let room = room["id"].as_str().unwrap().to_owned();
    (scene.clone(), room)
}

/// A state made before the user names the device is made again with the
/// new name before it's written: a scene activated (its state made on the
/// service call's task) while its room is named shows the room's new name,
/// not the old one written over the follow's.
#[tokio::test]
async fn a_state_made_before_a_device_is_named_takes_the_new_name() {
    let b = bridge(offline());
    let id = "scene.bedroom_bedroom_dimmed";
    let (scene, room) = scene_and_room(&b, id);
    let named = changed(
        &b,
        |d| {
            if d.identifiers.iter().any(|i| i[1] == room.as_str()) {
                d.name_by_user = Some("Main bedroom".into());
            }
        },
        |_| {},
    );
    let follower = b.clone();
    AFTER_RENDER.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || follower.follow(named)));
    });
    let _ = scene.activate(None, false, None, None).await;
    assert!(
        AFTER_RENDER.with(|hook| hook.borrow().is_none()),
        "not reached"
    );
    let (_, attributes) = state_json(&b.hass, id).unwrap();
    assert_eq!(attributes["friendly_name"], "Main bedroom Dimmed");
}

/// An entity given another id while a state of it is being made writes
/// that state nowhere: its old id has none, and the entity runs under the
/// new one, where it writes.
#[tokio::test]
async fn an_entity_moved_while_its_state_is_made_writes_only_under_its_new_id() {
    let b = bridge(offline());
    let (old, new) = ("scene.bedroom_bedroom_dimmed", "scene.bedroom_dimmed");
    let (scene, _) = scene_and_room(&b, old);
    let moved = changed(
        &b,
        |_| {},
        |e| {
            if e.entity_id == old {
                e.entity_id = new.into();
            }
        },
    );
    let follower = b.clone();
    AFTER_RENDER.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || follower.follow(moved)));
    });
    let _ = scene.activate(None, false, None, None).await;
    assert!(
        AFTER_RENDER.with(|hook| hook.borrow().is_none()),
        "not reached"
    );
    assert_eq!(state_json(&b.hass, old), None);
    let (_, attributes) = state_json(&b.hass, new).unwrap();
    assert_eq!(attributes["friendly_name"], "Bedroom Dimmed");
    let running = b.others.read().iter().any(|e| e.entity_id.as_str() == new);
    assert!(running);
    // What still holds the entity under its old id writes nothing more.
    assert!(scene.gone.load(std::sync::atomic::Ordering::Relaxed));
    b.write_entity(&scene);
    assert_eq!(state_json(&b.hass, old), None);
}

/// A device deleted on the bridge while one of its lights is given another
/// id (the delete found the light under its old id): the light goes from
/// where it is now, its new id, with nothing left under either.
#[tokio::test]
async fn a_device_deleted_while_its_light_moves_leaves_nothing() {
    let b = bridge(offline());
    let light = b.lights.read()[0].clone();
    let (old, new) = (light.entity_id.to_string(), "light.moved_away");
    let device = {
        let store = b.store.read();
        let device = store.device_of(&light.rid).unwrap();
        device["id"].as_str().unwrap().to_owned()
    };
    let gone = b.on_device(&device);
    assert!(gone.lights.iter().any(|l| Arc::ptr_eq(l, &light)));
    let moved = changed(
        &b,
        |_| {},
        |e| {
            if e.entity_id == old {
                e.entity_id = new.into();
            }
        },
    );
    b.follow(moved);
    assert!(state_json(&b.hass, new).is_some());
    b.remove(&gone);
    assert_eq!(state_json(&b.hass, &old), None);
    assert_eq!(state_json(&b.hass, new), None);
    assert!(b.lights.read().iter().all(|l| l.rid != light.rid));
}

/// A scene a service call picked under its old id, activated once it has
/// another: the activation is the entity's, written under its new id.
#[tokio::test]
async fn a_scene_activated_under_its_old_id_shows_it_under_the_new() {
    let b = bridge(offline());
    let (old, new) = ("scene.bedroom_bedroom_dimmed", "scene.bedroom_dimmed");
    let (scene, _) = scene_and_room(&b, old);
    assert_eq!(state_json(&b.hass, old).unwrap().0, "unknown");
    let moved = changed(
        &b,
        |_| {},
        |e| {
            if e.entity_id == old {
                e.entity_id = new.into();
            }
        },
    );
    b.follow(moved);
    let _ = scene.activate(None, false, None, None).await;
    assert_eq!(state_json(&b.hass, old), None);
    let (state, _) = state_json(&b.hass, new).unwrap();
    assert_ne!(state, "unknown", "the activation was lost");
}

/// What a light command keeps (the brightness a fading turn-off restores)
/// is the entity's: one that began under the old id is there under the
/// new one.
#[tokio::test]
async fn what_a_command_keeps_follows_a_light_to_its_new_id() {
    let b = bridge(offline());
    let light = b.lights.read()[0].clone();
    let old = light.entity_id.to_string();
    let moved = changed(
        &b,
        |_| {},
        |e| {
            if e.entity_id == old {
                e.entity_id = "light.moved_away".into();
            }
        },
    );
    b.follow(moved);
    let now = b
        .lights
        .read()
        .iter()
        .find(|l| l.rid == light.rid)
        .cloned()
        .unwrap();
    assert_eq!(now.entity_id.as_str(), "light.moved_away");
    *light.last_brightness.lock() = Some(42.0);
    assert_eq!(*now.last_brightness.lock(), Some(42.0));
}

/// A scene activated while a rename moves it to an id under which
/// `unknown` was kept: the move's restore comes before the activation,
/// not over it.
#[tokio::test]
async fn an_activation_is_not_restored_over_by_a_move() {
    let b = bridge(offline());
    let (old, new) = ("scene.bedroom_bedroom_dimmed", "scene.bedroom_dimmed");
    let kept = EntityId::parse(new).unwrap();
    b.hass
        .set_state(kept.clone(), "unknown", Default::default());
    b.hass.restore().register(new, None);
    b.hass.restore().unregister(&b.hass, new);
    b.hass.remove_state(&kept);
    let (scene, _) = scene_and_room(&b, old);
    let moved = changed(
        &b,
        |_| {},
        |e| {
            if e.entity_id == old {
                e.entity_id = new.into();
            }
        },
    );
    let follower = b.clone();
    AFTER_RENDER.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || follower.follow(moved)));
    });
    let _ = scene.activate(None, false, None, None).await;
    assert!(
        AFTER_RENDER.with(|hook| hook.borrow().is_none()),
        "not reached"
    );
    let (state, _) = state_json(&b.hass, new).unwrap();
    assert_ne!(state, "unknown", "the move restored over the activation");
}

/// How many lights the light component serves under `id`.
fn routed(b: &Bridge, id: &str) -> usize {
    b.platforms
        .lights
        .ids()
        .iter()
        .filter(|i| i.as_str() == id)
        .count()
}

/// `change` for the row now under `old`: the registries with it at `new`.
fn moved_to(b: &Bridge, old: &str, new: &'static str) -> Arc<Registries> {
    let old = old.to_owned();
    changed(
        b,
        |_| {},
        move |e| {
            if e.entity_id == old {
                e.entity_id = new.into();
            }
        },
    )
}

/// Run `rename` at `point` if the write guard is free there (the code
/// doesn't hold it), else once the step that reached it is done
/// (`finish`).
fn rename_at(b: &Arc<Bridge>, point: &'static str, moved: Arc<Registries>) -> impl FnOnce() {
    let later = Arc::new(parking_lot::Mutex::new(None));
    let (bridge, deferred, fresh) = (b.clone(), later.clone(), moved);
    crate::components::hooks::set(point, move || {
        let free = bridge.alive.try_lock().is_some();
        match free {
            true => bridge.follow(fresh),
            false => *deferred.lock() = Some(fresh),
        }
    });
    let bridge = b.clone();
    move || {
        assert!(
            !crate::components::hooks::pending(point),
            "{point} not reached"
        );
        if let Some(fresh) = later.lock().take() {
            bridge.follow(fresh);
        }
    }
}

/// A light group deleted on the bridge while a rename is moving it (the
/// rename has put the group in the bridge's list under its new id, and
/// not yet handed it to the light component): the deletion waits for the
/// rename, then drops the group where it is, so neither id serves it or
/// has a state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_group_deleted_while_it_moves_leaves_nothing() {
    let b = bridge(offline());
    let g = b.groups.read()[0].clone();
    let (old, new) = (g.entity_id.to_string(), "light.moved_group");
    let moved = moved_to(&b, &old, new);
    let deleted = json!([{"creationtime": "2026-01-01T00:00:00Z", "id": "gone", "type": "delete",
                          "data": [{"id": g.rid, "type": "grouped_light"}]}])
    .to_string();
    // The rename, paused between the list and the component.
    let (paused_tx, paused) = std::sync::mpsc::channel();
    let (resume, resume_rx) = std::sync::mpsc::channel::<()>();
    let renamer = b.clone();
    let rename = std::thread::spawn(move || {
        crate::components::hooks::set("group_swapped", move || {
            let _ = paused_tx.send(());
            let _ = resume_rx.recv();
        });
        renamer.follow(moved);
    });
    paused.recv().unwrap();
    // The deletion, on its own thread: it goes on once it waits for the
    // write guard, or is done.
    let (said_tx, said) = std::sync::mpsc::channel();
    let deleter = b.clone();
    let waits = said_tx.clone();
    let delete = std::thread::spawn(move || {
        crate::components::hooks::set("alive_waits", move || {
            let _ = waits.send("waits");
        });
        deleter.handle_data(&deleted);
        let _ = said_tx.send("done");
    });
    said.recv().unwrap();
    resume.send(()).unwrap();
    rename.join().unwrap();
    delete.join().unwrap();
    assert_eq!((routed(&b, &old), routed(&b, new)), (0, 0));
    assert_eq!(state_json(&b.hass, &old), None);
    assert_eq!(state_json(&b.hass, new), None);
}

/// A scene reported again as a smart scene (the same resource id) while
/// the scene runs: as HA's platform does with an entity whose unique id is
/// taken, the new one is not made, and one entity serves the id.
#[tokio::test]
async fn a_resource_back_as_another_type_makes_no_second_entity() {
    let b = bridge(offline());
    let id = "scene.bedroom_bedroom_dimmed";
    let (scene, _) = scene_and_room(&b, id);
    let mut smart = b.store.read().get(&scene.rid).unwrap().clone();
    smart["type"] = json!("smart_scene");
    let added = json!([{"creationtime": "2026-01-01T00:00:00Z", "id": "again", "type": "add",
                        "data": [smart]}]);
    b.handle_data(&added.to_string());
    let running: Vec<String> = b
        .others
        .read()
        .iter()
        .filter(|e| e.rid == scene.rid)
        .map(|e| e.entity_id.to_string())
        .collect();
    assert_eq!(running, [id]);
    let served = b
        .platforms
        .scenes
        .ids()
        .iter()
        .filter(|s| s.as_str() == id)
        .count();
    assert_eq!(served, 1);
}

/// A scene renamed while its room is away (it can't be written then, so
/// it has no state under its new id) has its state again when the room
/// comes back.
#[tokio::test]
async fn a_scene_renamed_while_its_room_is_away_is_written_when_it_returns() {
    let b = bridge(offline());
    let (old, new) = ("scene.bedroom_bedroom_dimmed", "scene.bedroom_dimmed");
    let (_, room) = scene_and_room(&b, old);
    let resource = b.store.read().get(&room).unwrap().clone();
    let ty = resource["type"].as_str().unwrap().to_owned();
    let deleted = json!([{"creationtime": "2026-01-01T00:00:00Z", "id": "gone", "type": "delete",
                          "data": [{"id": room, "type": ty}]}]);
    b.handle_data(&deleted.to_string());
    b.follow(moved_to(&b, old, new));
    assert_eq!(state_json(&b.hass, new), None);
    let added = json!([{"creationtime": "2026-01-01T00:00:00Z", "id": "back", "type": "add",
                        "data": [resource]}]);
    b.handle_data(&added.to_string());
    assert!(
        state_json(&b.hass, new).is_some(),
        "no state when its room came back"
    );
}

/// A light group that comes back while a rename gives its row another id
/// is served once, under the new id.
#[tokio::test]
async fn a_group_added_while_it_moves_is_served_once_under_its_new_id() {
    let b = bridge(offline());
    let g = b.groups.read()[0].clone();
    let (old, new) = (g.entity_id.to_string(), "light.moved_group");
    let resource = b.store.read().get(&g.rid).unwrap().clone();
    let moved = moved_to(&b, &old, new);
    let deleted = json!([{"creationtime": "2026-01-01T00:00:00Z", "id": "gone", "type": "delete",
                          "data": [{"id": g.rid, "type": "grouped_light"}]}]);
    b.handle_data(&deleted.to_string());
    assert_eq!(routed(&b, &old), 0);
    let finish = rename_at(&b, "hue_added", moved);
    let added = json!([{"creationtime": "2026-01-01T00:00:00Z", "id": "back", "type": "add",
                        "data": [resource]}]);
    b.handle_data(&added.to_string());
    finish();
    assert_eq!((routed(&b, &old), routed(&b, new)), (0, 1));
    assert_eq!(state_json(&b.hass, &old), None);
    assert!(state_json(&b.hass, new).is_some());
}

/// A scene picked for activation, then deleted and added again on the
/// bridge (the same resource id) before the activation is taken: the new
/// scene is another entity and keeps its own state.
#[tokio::test]
async fn an_activation_does_not_reach_a_scene_made_since() {
    let b = bridge(offline());
    let id = "scene.bedroom_bedroom_dimmed";
    let (scene, _) = scene_and_room(&b, id);
    let resource = b.store.read().get(&scene.rid).unwrap().clone();
    let stream = b.clone();
    let rid = scene.rid.clone();
    AFTER_RENDER.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            let deleted = json!([{"creationtime": "2026-01-01T00:00:00Z", "id": "gone",
                                  "type": "delete", "data": [{"id": rid, "type": "scene"}]}]);
            stream.handle_data(&deleted.to_string());
            let added = json!([{"creationtime": "2026-01-01T00:00:00Z", "id": "back",
                                "type": "add", "data": [resource]}]);
            stream.handle_data(&added.to_string());
        }));
    });
    let _ = scene.activate(None, false, None, None).await;
    assert!(
        AFTER_RENDER.with(|hook| hook.borrow().is_none()),
        "not reached"
    );
    let replacement = b
        .others
        .read()
        .iter()
        .any(|e| e.rid == scene.rid && !Arc::ptr_eq(e, &scene));
    assert!(replacement);
    assert_eq!(state_json(&b.hass, id).unwrap().0, "unknown");
}

/// A bridge on a live registry (in memory), following it as a set-up
/// one does, with its entities made and written. Run inside a runtime.
fn live_bridge() -> (
    Arc<Bridge>,
    Arc<ha_registry::Registry>,
    Arc<crate::components::Components>,
) {
    let hass = Hass::new();
    let registry = ha_registry::Registry::in_memory(&hass, registries());
    let components =
        crate::components::Components::new(&hass, registry.clone(), Profiles::default());
    let resources = fixture("resource")["data"].as_array().unwrap().clone();
    let b = Arc::new(Bridge {
        hass,
        registries: RwLock::new(components.registries()),
        names: Default::default(),
        api: offline(),
        store: RwLock::new(Store::new(resources)),
        ignore_availability: Vec::new(),
        lights: RwLock::default(),
        groups: RwLock::default(),
        others: RwLock::default(),
        platforms: components.hue(),
        alive: parking_lot::Mutex::new(true),
    });
    b.add_entities();
    components.follow("entry", Arc::new(follow::Follow(Arc::downgrade(&b))));
    (b, registry, components)
}

/// Rename the entity `old` to `new` as the WebSocket command does.
fn rename(
    registry: &ha_registry::Registry,
    components: &crate::components::Components,
    old: &str,
    new: &str,
) -> Result<Arc<ha_registry::Entity>, ha_registry::RenameRefusal> {
    let follows = &crate::components::follows;
    components.registry_edit(|| registry.rename_entity(old, new, &Default::default(), follows))
}

/// A new resource like `like`, with id `id` and `fields` replaced.
fn like(b: &Bridge, like: &str, id: &str, fields: Value) -> Value {
    let mut r = b.store.read().get(like).unwrap().clone();
    r["id"] = json!(id);
    for (k, v) in fields.as_object().unwrap() {
        r[k] = v.clone();
    }
    r
}

/// A Hue light the bridge reports while a rename gives another light the
/// id it would take (the registry has the new row, the running entities
/// have not followed yet), by the event stream or in a reconnect's
/// snapshot: it waits for them, and takes another id. One light is served
/// under the rename's id.
#[test]
fn a_new_light_does_not_take_an_id_a_rename_is_giving() {
    for snapshot in [false, true] {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let _rt = rt.enter();
        new_light_during_a_rename(snapshot);
    }
}

fn new_light_during_a_rename(snapshot: bool) {
    let (b, registry, components) = live_bridge();
    let light = b.lights.read()[0].clone();
    let old = light.entity_id.to_string();
    let wanted = "light.review_desk";
    // A new device with one light, named so its light takes `wanted`.
    let (device_id, light_id) = (
        "00000000-0000-4000-8000-0000000000d1",
        "00000000-0000-4000-8000-0000000000d2",
    );
    let device = b.store.read().device_of(&light.rid).unwrap()["id"].clone();
    let device = like(
        &b,
        device.as_str().unwrap(),
        device_id,
        json!({
            "metadata": {"name": "Review Desk", "archetype": "classic_bulb"},
            "services": [{"rid": light_id, "rtype": "light"}],
        }),
    );
    let resource = like(
        &b,
        &light.rid,
        light_id,
        json!({
            "owner": {"rid": device_id, "rtype": "device"},
        }),
    );
    let mut fresh = fixture("resource")["data"].as_array().unwrap().clone();
    fresh.extend([device.clone(), resource.clone()]);
    let added = json!([{"creationtime": "2026-01-01T00:00:00Z", "id": "new", "type": "add",
                        "data": [device, resource]}])
    .to_string();
    // Between the registry's change and the entities following it, the
    // bridge reports the new light, on a thread of its own as the stream
    // does. The rename goes on once that thread either waits for the
    // set-up lock or is done.
    let stream = b.clone();
    let reported: Arc<parking_lot::Mutex<Option<std::thread::JoinHandle<()>>>> = Arc::default();
    let handle = reported.clone();
    crate::components::hooks::set("edited", move || {
        let (tx, rx) = std::sync::mpsc::channel();
        let waits = tx.clone();
        let t = std::thread::spawn(move || {
            crate::components::hooks::set("setup_waits", move || {
                let _ = waits.send("waits");
            });
            match snapshot {
                true => stream.apply_snapshot(fresh),
                false => stream.handle_data(&added),
            }
            let _ = tx.send("done");
        });
        let _ = rx.recv();
        *handle.lock() = Some(t);
    });
    let renamed = rename(&registry, &components, &old, wanted);
    assert!(renamed.is_ok(), "{renamed:?}");
    reported.lock().take().expect("not reached").join().unwrap();
    let lights = b.lights.read().clone();
    let under: Vec<&str> = lights
        .iter()
        .filter(|l| l.entity_id.as_str() == wanted)
        .map(|l| l.rid.as_str())
        .collect();
    assert_eq!(under, [light.rid.as_str()], "snapshot: {snapshot}");
    assert!(
        lights
            .iter()
            .any(|l| l.rid == light_id && l.entity_id.as_str() != wanted)
    );
    assert_eq!(routed(&b, wanted), 1, "snapshot: {snapshot}");
}

/// A scene the bridge reports before its room is made when the room comes,
/// with its state written then: a rename to its id after that is refused
/// (the id is taken), and one scene is served under it.
#[tokio::test]
async fn a_scene_made_when_its_room_comes_has_its_state_at_once() {
    let (b, registry, components) = live_bridge();
    let (scene, room) = scene_and_room(&b, "scene.bedroom_bedroom_dimmed");
    let (room_id, scene_id) = (
        "00000000-0000-4000-8000-0000000000e1",
        "00000000-0000-4000-8000-0000000000e2",
    );
    let new_room = like(
        &b,
        &room,
        room_id,
        json!({
            "metadata": {"name": "Review Desk", "archetype": "office"},
            "children": [], "services": [],
        }),
    );
    let new_scene = like(
        &b,
        &scene.rid,
        scene_id,
        json!({
            "group": {"rid": room_id, "rtype": "room"},
        }),
    );
    let add = |r: &Value| {
        json!([{"creationtime": "2026-01-01T00:00:00Z", "id": "new", "type": "add",
                "data": [r]}])
        .to_string()
    };
    b.handle_data(&add(&new_scene));
    assert!(
        b.others.read().iter().all(|e| e.rid != scene_id),
        "made before its room"
    );
    b.handle_data(&add(&new_room));
    let made = b.others.read().iter().find(|e| e.rid == scene_id).cloned();
    let made = made.expect("not made when its room came");
    let id = made.entity_id.to_string();
    assert!(state_json(&b.hass, &id).is_some(), "{id} has no state");
    let renamed = rename(&registry, &components, scene.entity_id.as_str(), &id);
    assert_eq!(renamed.err(), Some(ha_registry::RenameRefusal::Registered));
    let served = b
        .platforms
        .scenes
        .ids()
        .iter()
        .filter(|s| s.as_str() == id)
        .count();
    assert_eq!(served, 1);
}

/// A reconnect's snapshot with an update before an addition: the update
/// is written without the set-up lock held (only the addition takes it),
/// so a registry edit or a set-up elsewhere doesn't wait for the whole
/// batch.
#[tokio::test]
async fn a_reconnects_updates_wait_for_no_lock() {
    let b = bridge(offline());
    let mut fresh = fixture("resource")["data"].as_array().unwrap().clone();
    let first_light = fresh.iter().position(|r| r["type"] == "light").unwrap();
    fresh[first_light]["on"]["on"] = json!(!fresh[first_light]["on"]["on"].as_bool().unwrap());
    let (device_id, light_id) = (
        "00000000-0000-4000-8000-0000000000f0",
        "00000000-0000-4000-8000-0000000000f1",
    );
    let device = like(
        &b,
        LAMP_DEVICE,
        device_id,
        json!({
            "metadata": {"name": "Late lamp", "archetype": "classic_bulb"},
            "services": [{"rid": light_id, "rtype": "light"}],
        }),
    );
    let light = like(
        &b,
        LAMP,
        light_id,
        json!({"owner": {"rid": device_id, "rtype": "device"}}),
    );
    fresh.extend([device, light]);
    let free = Arc::new(parking_lot::Mutex::new(None));
    let seen = free.clone();
    AFTER_RENDER.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            *seen.lock() = Some(!crate::switchbot_cloud::SETUP.held_here());
        }));
    });
    b.apply_snapshot(fresh);
    assert_eq!(
        *free.lock(),
        Some(true),
        "the update waited for the set-up lock"
    );
    assert!(
        b.lights
            .read()
            .iter()
            .any(|l| l.rid == "00000000-0000-4000-8000-0000000000f1")
    );
}

/// The ids `state_changed` fired for, in order.
fn changed_ids(rx: &mut tokio::sync::broadcast::Receiver<Arc<ha_core::Event>>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if let ha_core::EventKind::StateChanged { entity_id, .. } = &e.kind {
            out.push(entity_id.to_string());
        }
    }
    out
}

/// A reconnect's snapshot is written in its own order: a device the
/// bridge adds, then a change to the lamp, then the device's light. The
/// lamp's change is written before the new light, as the events would be.
#[tokio::test]
async fn a_reconnect_writes_in_the_snapshots_order() {
    let b = bridge(offline());
    let mut fresh = fixture("resource")["data"].as_array().unwrap().clone();
    let lamp = fresh.iter().position(|r| r["id"] == LAMP).unwrap();
    fresh[lamp]["on"]["on"] = json!(!fresh[lamp]["on"]["on"].as_bool().unwrap());
    let (device_id, light_id) = (
        "00000000-0000-4000-8000-0000000000c0",
        "00000000-0000-4000-8000-0000000000c1",
    );
    let device = like(
        &b,
        LAMP_DEVICE,
        device_id,
        json!({
            "metadata": {"name": "Late lamp", "archetype": "classic_bulb"},
            "services": [{"rid": light_id, "rtype": "light"}],
        }),
    );
    let light = like(
        &b,
        LAMP,
        light_id,
        json!({"owner": {"rid": device_id, "rtype": "device"}}),
    );
    fresh.insert(lamp + 1, light);
    fresh.insert(lamp, device);
    let lamp_id = b
        .lights
        .read()
        .iter()
        .find(|l| l.rid == LAMP)
        .unwrap()
        .entity_id
        .to_string();
    let mut rx = b.hass.bus().subscribe();
    b.apply_snapshot(fresh);
    let new_id = b
        .lights
        .read()
        .iter()
        .find(|l| l.rid == light_id)
        .unwrap()
        .entity_id
        .to_string();
    let order = changed_ids(&mut rx);
    let at = |id: &str| order.iter().position(|o| o == id);
    assert!(
        at(&lamp_id) < at(&new_id) && at(&lamp_id).is_some(),
        "{order:?}"
    );
}

/// A light the bridge reports after another entry made a row (disabled,
/// so it has no state) under the id the light would take: the light takes
/// another id, and calls to that one reach no Hue light.
#[tokio::test]
async fn a_new_light_does_not_take_an_id_another_entry_has() {
    let (b, registry, _components) = live_bridge();
    let patch = ha_registry::EntityPatch {
        has_entity_name: Some(false),
        original_name: Some(Some("Reserved Name".into())),
        ..Default::default()
    };
    let made =
        registry.get_or_create_entity("light", "lifx", "reserved", Some("user".into()), patch);
    // As that entry's user named it; not an edit this bridge follows.
    let reserved = registry.rename_entity(
        &made.entity_id,
        "light.reserved_name",
        &Default::default(),
        &|_| true,
    );
    assert!(reserved.is_ok(), "{reserved:?}");
    let lamp = b
        .lights
        .read()
        .iter()
        .find(|l| l.rid == LAMP)
        .cloned()
        .unwrap();
    let (device_id, light_id) = (
        "00000000-0000-4000-8000-0000000000b0",
        "00000000-0000-4000-8000-0000000000b1",
    );
    let device = like(
        &b,
        LAMP_DEVICE,
        device_id,
        json!({
            "metadata": {"name": "Reserved Name", "archetype": "classic_bulb"},
            "services": [{"rid": light_id, "rtype": "light"}],
        }),
    );
    let light = like(
        &b,
        &lamp.rid,
        light_id,
        json!({"owner": {"rid": device_id, "rtype": "device"}}),
    );
    let added = json!([{"creationtime": "2026-01-01T00:00:00Z", "id": "new", "type": "add",
                        "data": [device, light]}]);
    b.handle_data(&added.to_string());
    let new = b
        .lights
        .read()
        .iter()
        .find(|l| l.rid == light_id)
        .cloned()
        .unwrap();
    assert_ne!(new.entity_id.as_str(), "light.reserved_name");
    assert_eq!(routed(&b, "light.reserved_name"), 0);
}

/// A second bridge, sharing the first's hass and services.
fn second_bridge(first: &Arc<Bridge>, api: Api) -> Arc<Bridge> {
    let resources = fixture("resource")["data"].as_array().unwrap().clone();
    let b = Arc::new(Bridge {
        hass: first.hass.clone(),
        registries: RwLock::new(first.rows()),
        names: Default::default(),
        api,
        store: RwLock::new(Store::new(resources)),
        ignore_availability: Vec::new(),
        lights: RwLock::default(),
        groups: RwLock::default(),
        others: RwLock::default(),
        platforms: first.platforms.clone(),
        alive: parking_lot::Mutex::new(true),
    });
    first.platforms.bridges.write().push(Arc::downgrade(&b));
    b
}

const MOTION_DEVICE_ZIGBEE: &str = "0a8892fe-e99f-4e3c-aad8-1db9a952ff97";

/// A device's connectivity change writes its entities in HA's creation
/// order: binary sensors, then sensors in controller order (temperature,
/// illuminance, battery), then switches.
#[tokio::test]
async fn a_connectivity_change_writes_in_has_order() {
    let b = bridge(offline());
    let mut rx = b.hass.bus().subscribe();
    b.handle(
        "update",
        &json!({"id": MOTION_DEVICE_ZIGBEE, "type": "zigbee_connectivity", "status": "connectivity_issue"}),
    );
    let mut order = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if let ha_core::EventKind::StateChanged {
            entity_id,
            new_state: Some(s),
            ..
        } = &e.kind
        {
            assert_eq!(s.state, "unavailable", "{entity_id}");
            order.push(entity_id.to_string());
        }
    }
    assert_eq!(
        order,
        [
            "binary_sensor.hallway_motion",
            "sensor.hallway_temperature",
            "sensor.hallway_illuminance",
            "sensor.hallway_motion_battery",
            "switch.hallway_motion_sensor_enabled",
            "switch.hallway_light_sensor_enabled",
        ]
    );
}

/// New sensors on same-named devices get their own ids; a new zigbee
/// connectivity sensor is disabled by default (no state); numbers in
/// strings parse as aiohue parses them, and an unknown battery state is
/// none.
#[tokio::test]
async fn new_sensors_like_ha() {
    let b = bridge(offline());
    let mut fresh = fixture("resource")["data"].as_array().unwrap().clone();
    for n in 1..=2 {
        let dev = format!("sdev-{n}");
        fresh.push(
            json!({"id": dev, "type": "device", "metadata": {"name": "Desk"},
            "services": [{"rid": format!("t-{n}"), "rtype": "temperature"},
                         {"rid": format!("z-{n}"), "rtype": "zigbee_connectivity"},
                         {"rid": format!("p-{n}"), "rtype": "device_power"},
                         {"rid": format!("l-{n}"), "rtype": "light_level"}]}),
        );
        fresh.push(json!({"id": format!("t-{n}"), "type": "temperature", "owner": {"rid": dev, "rtype": "device"},
            "enabled": true, "temperature": {"temperature_report": {"temperature": "21.25"}}}));
        fresh.push(json!({"id": format!("z-{n}"), "type": "zigbee_connectivity", "owner": {"rid": dev, "rtype": "device"},
            "status": "connected", "mac_address": "00:00"}));
        fresh.push(json!({"id": format!("p-{n}"), "type": "device_power", "owner": {"rid": dev, "rtype": "device"},
            "power_state": {"battery_level": "85", "battery_state": "weird"}}));
        fresh.push(json!({"id": format!("l-{n}"), "type": "light_level", "owner": {"rid": dev, "rtype": "device"},
            "enabled": true, "light": {"light_level": "10001"}}));
    }
    b.apply_snapshot(fresh);
    let (t1, t2) = (
        state_json(&b.hass, "sensor.desk_temperature"),
        state_json(&b.hass, "sensor.desk_temperature_2"),
    );
    assert_eq!(t1.unwrap().0, "21.2");
    assert_eq!(t2.unwrap().0, "21.2");
    assert!(state_json(&b.hass, "sensor.desk_zigbee_connectivity").is_none());
    let (battery, attrs) = state_json(&b.hass, "sensor.desk_battery").unwrap();
    assert_eq!(battery, "85");
    assert!(attrs.get("battery_state").is_none(), "{attrs}");
    assert_eq!(
        state_json(&b.hass, "sensor.desk_illuminance").unwrap().0,
        "10"
    );
}

/// A legacy-named entity whose name starts with its device's name isn't
/// prefixed twice.
#[tokio::test]
async fn legacy_names_strip_the_device_prefix() {
    let b = bridge(offline());
    b.handle(
        "add",
        &json!({"id": "ent-1", "type": "entertainment_configuration", "metadata": {"name": "Hue Bridge Cinema"},
            "status": "inactive"}),
    );
    let names: Vec<String> = b
        .hass
        .states()
        .all()
        .iter()
        .filter(|s| s.entity_id.as_str().starts_with("binary_sensor."))
        .filter_map(|s| {
            s.attributes
                .get("friendly_name")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    assert!(names.iter().any(|n| n == "Hue Bridge Cinema"), "{names:?}");
}

/// `hue.hue_activate_scene` recalls the scene on every bridge that has it,
/// and a huge transition saturates rather than wrapping.
#[tokio::test]
async fn legacy_activation_reaches_every_bridge() {
    let (base1, seen1) = fake_bridge().await;
    let (base2, seen2) = fake_bridge().await;
    let b1 = bridge(Api::at(&base1, "key"));
    let _b2 = second_bridge(&b1, Api::at(&base2, "key"));
    let data = json!({"group_name": "bedroom", "scene_name": "dimmed", "transition": 9_223_372_036_854_776_i64});
    let Value::Object(data) = data else { panic!() };
    b1.hass
        .call_service("hue", "hue_activate_scene", data, None, false, None)
        .await
        .unwrap();
    for seen in [seen1, seen2] {
        let seen = seen.lock();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].2,
            json!({"recall": {"action": "active", "duration": i64::MAX}})
        );
    }
}

/// A scene comes back with its last activation, and is dumped from then on.
#[tokio::test]
async fn scenes_restore_their_last_activation() {
    let dir = std::env::temp_dir().join(format!("hars-hue-restore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    ha_core::storage::Store::new(&dir, "core.restore_state", 1, 1, false)
        .save(&json!([
            {"state": {"entity_id": "scene.bedroom_bedroom_dimmed", "state": "2026-11-13T20:00:00.123456+00:00", "attributes": {}},
             "extra_data": null, "last_seen": "2099-01-01T00:00:00+00:00"},
            {"state": {"entity_id": "scene.bedroom_bedroom_bright", "state": "unavailable", "attributes": {}},
             "extra_data": null, "last_seen": "2099-01-01T00:00:00+00:00"}
        ]))
        .unwrap();
    let hass = Hass::new();
    hass.set_restore(ha_core::restore::RestoreState::load(&dir));
    let registries = Arc::new(registries());
    let platforms = Platforms::new(&crate::components::Components::new(
        &hass,
        registries.clone(),
        Profiles::default(),
    ));
    let b = Arc::new(Bridge {
        hass: hass.clone(),
        registries: RwLock::new(registries),
        names: Default::default(),
        api: offline(),
        store: RwLock::new(Store::new(
            fixture("resource")["data"].as_array().unwrap().clone(),
        )),
        ignore_availability: Vec::new(),
        lights: RwLock::default(),
        groups: RwLock::default(),
        others: RwLock::default(),
        platforms,
        alive: parking_lot::Mutex::new(true),
    });
    b.add_entities();
    assert_eq!(
        state_json(&hass, "scene.bedroom_bedroom_dimmed").unwrap().0,
        "2026-11-13T20:00:00.123456+00:00"
    );
    assert_eq!(
        state_json(&hass, "scene.bedroom_bedroom_bright").unwrap().0,
        "unknown"
    );
    let stored = hass.restore().stored(&hass, hass.clock().now());
    assert!(
        stored
            .iter()
            .any(|i| i["state"]["entity_id"] == "scene.hallway_hallway_nightlight")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A set-up over the rows HA itself made for this bridge (an imported
/// install: the user's areas, names and disabled entities, HA's own ids
/// and times) changes none of them, makes none, and fires nothing: what
/// `hars` registers is what HA registered.
#[test]
fn registering_leaves_has_rows_alone() {
    let hass = Hass::new();
    let imported = registries();
    let entry_id = imported.devices[0].config_entries[0].clone();
    let registry = ha_registry::Registry::in_memory(&hass, imported);
    let before = registry.snapshot();
    let mut events = hass.bus().subscribe();
    let resources = fixture("resource")["data"].as_array().unwrap().clone();
    register::register(&hass, &registry, &entry_id, &Store::new(resources), false);
    let after = registry.snapshot();
    let rows = |r: &Registries| json!([r.devices, r.entities, r.areas]);
    for (n, (was, is)) in before.devices.iter().zip(&after.devices).enumerate() {
        assert_eq!(json!(is), json!(was), "device {n}");
    }
    for (was, is) in before.entities.iter().zip(&after.entities) {
        assert_eq!(json!(is), json!(was), "{}", was.entity_id);
    }
    assert_eq!(rows(&after), rows(&before));
    assert!(events.try_recv().is_err(), "a registry event was fired");
    // And no copy of the registries was made for it.
    assert!(Arc::ptr_eq(&before, &after));
}

/// On an install with no rows, the bridge's set-up makes the devices,
/// areas and entities HA 2026.6.1 made (`vectors/fresh_setup.json`), with
/// HA's entity ids.
#[test]
fn registering_makes_has_rows() {
    let ha: Value = serde_json::from_str(include_str!("vectors/fresh_setup.json")).unwrap();
    let hass = Hass::new();
    let registry = ha_registry::Registry::in_memory(&hass, Registries::default());
    let resources = fixture("resource")["data"].as_array().unwrap().clone();
    register::register(&hass, &registry, "entry", &Store::new(resources), false);
    let made = registry.snapshot();
    let mut ids: Vec<&str> = made.entities.iter().map(|e| e.entity_id.as_str()).collect();
    ids.sort_unstable();
    let want: Vec<&str> = ha["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["entity_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, want);
    let areas: Vec<&str> = made.areas.iter().map(|a| a.id.as_str()).collect();
    let want: Vec<&str> = ha["areas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(areas, want);
    assert_eq!(made.devices.len(), ha["devices"].as_array().unwrap().len());
    // A second set-up is the first again.
    let mut events = hass.bus().subscribe();
    let resources = fixture("resource")["data"].as_array().unwrap().clone();
    register::register(&hass, &registry, "entry", &Store::new(resources), false);
    assert!(events.try_recv().is_err(), "a second set-up changed a row");
}

/// What HA derived from the install it ran on stays in a row it made: a
/// name in another language, and a temperature's unit of another unit
/// system, which the row's options then say is kept (HA's `sensor`
/// does that when the unit system changes under a row). A name that
/// is the resource's own (a scene's) follows the resource. A row made
/// now starts disabled if the entry says so; an enabled row, made now or
/// not, on a disabled device is disabled with it (HA's order: the
/// entry's preference in the registry, then the device).
#[test]
fn registering_keeps_what_ha_derived_and_obeys_the_disabling() {
    const TEMPERATURE: &str = "0cfa6b7f-fdc8-490a-9261-6e721a4afc58";
    const SCENE: &str = "f2d28599-c0cb-4335-9522-4f278b438ac9";
    const BULB: &str = "c60ee3d8-e55f-4fc9-8d31-9e3dc6aa077d";
    const ROOM_LIGHT: &str = "df1e030d-f506-4bfb-bf5e-548706002545";
    let unique = |e: &ha_registry::Entity, id: &str| e.unique_id == Some(json!(id));
    let hass = Hass::new();
    let entry_id = registries().devices[0].config_entries[0].clone();
    let install = || {
        let mut imported = registries();
        for row in &mut imported.entities {
            let mut changed = (**row).clone();
            if unique(row, TEMPERATURE) {
                changed.unit_of_measurement = Some("°F".into());
                changed.original_name = Some("Temperatur".into());
            } else if unique(row, SCENE) {
                changed.original_name = Some("Old name".into());
            }
            *row = Arc::new(changed);
        }
        // Two lights the import never saw, one on a device the user disabled,
        // and a room's light that is enabled though its device is disabled.
        let room = imported
            .entities
            .iter()
            .find(|e| unique(e, ROOM_LIGHT))
            .and_then(|e| e.device_id.clone())
            .unwrap();
        for device in &mut imported.devices {
            if device.identifiers.iter().any(|i| i[1] == LAMP_DEVICE) || device.id == room {
                let mut disabled = (**device).clone();
                disabled.disabled_by = Some("user".into());
                *device = Arc::new(disabled);
            }
        }
        imported
            .entities
            .retain(|e| !unique(e, LAMP) && !unique(e, BULB));
        imported
    };
    let registry = ha_registry::Registry::in_memory(&hass, install());
    let resources = fixture("resource")["data"].as_array().unwrap().clone();
    let store = Store::new(resources);
    register::register(&hass, &registry, &entry_id, &store, true);
    let after = registry.snapshot();
    let row = |id: &str| after.entities.iter().find(|e| unique(e, id)).unwrap();
    assert_eq!(row(TEMPERATURE).unit_of_measurement.as_deref(), Some("°F"));
    assert_eq!(
        row(TEMPERATURE).options.object()["sensor.private"],
        json!({"suggested_unit_of_measurement": "°F"})
    );
    assert_eq!(
        row(TEMPERATURE).original_name.as_deref(),
        Some("Temperatur")
    );
    assert_eq!(row(SCENE).original_name.as_deref(), Some("Bright"));
    // New: by the entry's preference, on a disabled device too.
    assert_eq!(row(LAMP).disabled_by.as_deref(), Some("integration"));
    assert_eq!(row(BULB).disabled_by.as_deref(), Some("integration"));
    // Enabled on a disabled device: disabled with it.
    assert_eq!(row(ROOM_LIGHT).disabled_by.as_deref(), Some("device"));
    // And without the preference a new row on a disabled device is too.
    let registry = ha_registry::Registry::in_memory(&hass, install());
    register::register(&hass, &registry, &entry_id, &store, false);
    let after = registry.snapshot();
    let row = |id: &str| after.entities.iter().find(|e| unique(e, id)).unwrap();
    assert_eq!(row(LAMP).disabled_by.as_deref(), Some("device"));
    assert_eq!(row(BULB).disabled_by, None);
}

/// With a device or entity store that can't be written (an install from
/// an older Home Assistant), no row is made or changed: rows made now
/// would be gone after a restart while rows that point at them were
/// saved.
#[tokio::test]
async fn nothing_is_registered_in_a_store_that_cant_be_written() {
    let hass = Hass::new();
    let older = ha_registry::STORES
        .iter()
        .find(|s| s.0 == ha_registry::DEVICE_KEY)
        .unwrap()
        .2
        - 1;
    let registries = Registries {
        device_file: ha_registry::FileRest {
            minor: older,
            ..Default::default()
        },
        ..Default::default()
    };
    let dir = std::env::temp_dir().join(format!("hars-hue-read-only-{}", std::process::id()));
    let registry = ha_registry::Registry::new(&hass, &dir, registries);
    let resources = fixture("resource")["data"].as_array().unwrap().clone();
    register::register(&hass, &registry, "entry", &Store::new(resources), false);
    let after = registry.snapshot();
    assert!(after.devices.is_empty() && after.entities.is_empty() && after.areas.is_empty());
}

//! Against the real bridge snapshot and vectors made by running HA
//! 2026.6.1's own code on it (see `vectors/`).

use super::*;
use crate::light::Profiles;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
        registries,
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
    bridge.write_all();
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
                        log.lock()
                            .push((method, path.trim_start_matches('/').to_owned(), body));
                        buf.drain(..end + 4 + len);
                        let reply = r#"{"data":[],"errors":[]}"#;
                        let res = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{reply}",
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
    // After: an activation writes no time, and the service finds nothing.
    scenes[0].activate(None, false, None, None).await.unwrap();
    assert_eq!(left(&b), Vec::<String>::new());
    seen.lock().clear();
    let data = json!({"group_name": "bedroom", "scene_name": "dimmed"});
    let Value::Object(data) = data else { panic!() };
    let _ = b
        .hass
        .call_service("hue", "hue_activate_scene", data, None, false, None)
        .await;
    assert!(seen.lock().is_empty(), "{:?}", seen.lock());
}

/// A second bridge, sharing the first's hass and services.
fn second_bridge(first: &Arc<Bridge>, api: Api) -> Arc<Bridge> {
    let resources = fixture("resource")["data"].as_array().unwrap().clone();
    let b = Arc::new(Bridge {
        hass: first.hass.clone(),
        registries: first.registries.clone(),
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
        registries,
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
    b.write_all();
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
    register::register(&registry, &entry_id, &Store::new(resources), false);
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
    register::register(&registry, "entry", &Store::new(resources), false);
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
    register::register(&registry, "entry", &Store::new(resources), false);
    assert!(events.try_recv().is_err(), "a second set-up changed a row");
}

/// What HA derived from the install it ran on stays in a row it made: a
/// unit of another unit system, a name in another language. A name that
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
    register::register(&registry, &entry_id, &store, true);
    let after = registry.snapshot();
    let row = |id: &str| after.entities.iter().find(|e| unique(e, id)).unwrap();
    assert_eq!(row(TEMPERATURE).unit_of_measurement.as_deref(), Some("°F"));
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
    register::register(&registry, &entry_id, &store, false);
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
    register::register(&registry, "entry", &Store::new(resources), false);
    let after = registry.snapshot();
    assert!(after.devices.is_empty() && after.entities.is_empty() && after.areas.is_empty());
}

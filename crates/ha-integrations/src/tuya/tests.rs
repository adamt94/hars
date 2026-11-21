//! The integration against a fake Tuya cloud on 127.0.0.1, checked against
//! what Home Assistant 2026.6.1 did with the same cloud: the vectors in
//! `vectors/` come from `scripts/vectors/tuya.py`, which runs HA's `tuya`
//! integration on tuya-device-sharing-sdk 0.2.8 and tuya-device-handlers
//! 0.0.22 against the devices in `vectors/cloud.json`.
//!
//! The fake signs and encrypts as the cloud does: it derives each request's
//! key from its request id and the refresh token it last gave out. Its MQTT
//! broker is a plain socket.

use super::*;
use crate::config_entries::ConfigEntries;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use md5::Digest;
use std::collections::VecDeque;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn vector(name: &str) -> Value {
    let text = match name {
        "cloud" => include_str!("vectors/cloud.json"),
        "setup" => include_str!("vectors/setup.json"),
        "pushes" => include_str!("vectors/pushes.json"),
        "services" => include_str!("vectors/services.json"),
        "crypto" => include_str!("vectors/crypto.json"),
        _ => unreachable!(),
    };
    serde_json::from_str(text).unwrap()
}

/// A request as the fake saw it, its body decrypted.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    body: Option<Value>,
}

#[derive(Default)]
struct Cloud {
    /// The refresh token the fake last handed out: requests are keyed on it.
    refresh: Mutex<String>,
    seen: Mutex<Vec<Seen>>,
    /// Replies to use instead, by `"METHOD path"`, each used once.
    once: Mutex<HashMap<String, VecDeque<Value>>>,
    /// HTTP 500 for the next n requests.
    fail: Mutex<usize>,
    /// Where `access/config` sends MQTT.
    broker: Mutex<String>,
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() + 1 {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The SDK's encryption read back: `b64(nonce) + b64(ciphertext)`.
fn open(enc: &str, secret: &str) -> Value {
    let (n, c) = enc.split_at(16);
    let mut raw = B64.decode(n).unwrap();
    raw.extend(B64.decode(c).unwrap());
    serde_json::from_str(&api::decrypt(&B64.encode(raw), secret).unwrap()).unwrap()
}

/// A `result` as Tuya encrypts it: `b64(nonce + ciphertext)`.
fn seal(v: &Value, secret: &str) -> String {
    let enc = api::encrypt(&v.to_string(), secret);
    let (n, c) = enc.split_at(16);
    let mut raw = B64.decode(n).unwrap();
    raw.extend(B64.decode(c).unwrap());
    B64.encode(raw)
}

impl Cloud {
    /// `scripts/vectors/tuya.py`'s `Cloud.result`, on the same input.
    fn result(&self, method: &str, path: &str) -> Value {
        if let Some(v) = self
            .once
            .lock()
            .get_mut(&format!("{method} {path}"))
            .and_then(VecDeque::pop_front)
        {
            return v;
        }
        let cloud = vector("cloud");
        let devices: Vec<String> = cloud["devices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["id"].as_str().unwrap().to_owned())
            .collect();
        if path == "/v1.0/m/life/users/homes" {
            return cloud["homes"].clone();
        }
        if path == "/v1.0/m/life/ha/home/devices" {
            return cloud["devices"].clone();
        }
        for d in &devices {
            if path == format!("/v1.1/m/life/{d}/specifications") {
                return cloud["specifications"][d].clone();
            }
            if path == format!("/v1.0/m/life/devices/{d}/status") {
                return cloud["status"][d].clone();
            }
            if path == format!("/v1.0/m/life/ha/{d}/dp-report-types") {
                return cloud["report_types"].get(d).cloned().unwrap_or(json!([]));
            }
            if path == format!("/v1.1/m/thing/{d}/commands") {
                return json!(true);
            }
        }
        if path == "/v1.0/m/life/ha/access/config" {
            let mut mqtt = cloud["mqtt"].clone();
            mqtt["url"] = json!(*self.broker.lock());
            return mqtt;
        }
        if path.starts_with("/v1.0/m/token/") {
            *self.refresh.lock() = "refresh2".into();
            return json!({"expireTime": 7200, "uid": "uid",
                "accessToken": "access2", "refreshToken": "refresh2"});
        }
        Value::Null
    }

    fn take(&self) -> Vec<Seen> {
        std::mem::take(&mut *self.seen.lock())
    }
}

/// The fake cloud's base URL.
async fn cloud() -> (String, Arc<Cloud>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let c = Arc::new(Cloud::default());
    *c.refresh.lock() = "refresh1".into();
    *c.broker.lock() = "tcp://127.0.0.1:1".into();
    let srv = c.clone();
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
                let method = first.next().unwrap().to_owned();
                let target = first.next().unwrap().to_owned();
                let headers: HashMap<String, String> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.to_lowercase(), v.trim().to_owned()))
                    .collect();
                let path = target.split('?').next().unwrap().to_owned();
                let rid = headers.get("x-requestid").cloned().unwrap_or_default();
                let refresh = srv.refresh.lock().clone();
                let hash_key = hex(&md5::Md5::digest(format!("{rid}{refresh}").as_bytes()));
                let secret = api::secret(&rid, &hash_key);
                let raw = String::from_utf8_lossy(&buf[end + 4..end + 4 + len]).into_owned();
                // The signature, checked as Tuya checks it.
                let query = target
                    .split_once("?encdata=")
                    .map(|(_, q)| percent_decode(q))
                    .unwrap_or_default();
                let body_enc = serde_json::from_str::<Value>(&raw)
                    .ok()
                    .and_then(|b| b.get("encdata").and_then(Value::as_str).map(str::to_owned))
                    .unwrap_or_default();
                let h = |k: &str| headers.get(k).map(String::as_str).unwrap_or("");
                let signed = [
                    ("X-appKey", h("x-appkey")),
                    ("X-requestId", h("x-requestid")),
                    ("X-sid", h("x-sid")),
                    ("X-time", h("x-time")),
                    ("X-token", h("x-token")),
                ];
                assert_eq!(
                    api::sign(&hash_key, &signed, &query, &body_enc),
                    h("x-sign"),
                    "bad signature on {method} {path}"
                );
                let body = serde_json::from_str::<Value>(&raw).ok().and_then(|b| {
                    b.get("encdata")
                        .and_then(Value::as_str)
                        .map(|e| open(e, &secret))
                });
                srv.seen.lock().push(Seen {
                    method: method.clone(),
                    path: path.clone(),
                    body,
                });
                let failing = {
                    let mut f = srv.fail.lock();
                    let failing = *f > 0;
                    *f = f.saturating_sub(1);
                    failing
                };
                let (status, reply) = if failing {
                    (500, String::new())
                } else {
                    let result = srv.result(&method, &path);
                    let reply = match result.get("error") {
                        Some(e) => json!({"success": false, "code": e["code"], "msg": e["msg"]}),
                        None => json!({"success": true, "t": 1, "result": seal(&result, &secret)}),
                    };
                    (200, reply.to_string())
                };
                let res = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = s.write_all(res.as_bytes()).await;
            });
        }
    });
    (base, c)
}

/// The entry, with a token good for an hour, or one that ran out.
fn entry(hass: &Hass, expired: bool) -> Map<String, Value> {
    let now = hass.clock().now().as_millisecond();
    let t = if expired { now - 3 * 3_600_000 } else { now };
    json!({
        "entry_id": "tuya1", "domain": "tuya", "title": "Tuya",
        "data": {"endpoint": "unused", "terminal_id": "term", "user_code": "code",
            "token_info": {"t": t, "expire_time": 3600, "uid": "uid",
                "access_token": "access1", "refresh_token": "refresh1"}},
        "options": {}, "pref_disable_new_entities": false,
    })
    .as_object()
    .cloned()
    .unwrap()
}

struct Running {
    hass: Hass,
    registry: Arc<Registry>,
    store: Arc<ConfigEntries>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn start(base: &str, expired: bool) -> Running {
    let hass = Hass::new();
    let entry = entry(&hass, expired);
    let store = ConfigEntries::in_memory(&hass, vec![entry.clone()]);
    let registry = Registry::in_memory(&hass, Default::default());
    let components = Components::new(&hass, registry.clone(), crate::light::Profiles::default());
    let endpoints = crate::Endpoints {
        tuya: Some(base.to_owned()),
        ..Default::default()
    };
    let task = crate::spawn_tuya(
        &components,
        entry,
        &endpoints,
        Some(store.clone()),
        Status::new(store.clone(), "tuya1"),
    );
    Running {
        hass,
        registry,
        store,
        task,
    }
}

async fn until(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..200 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Every state, as the vectors hold them.
fn states(hass: &Hass) -> Value {
    let mut out = Map::new();
    for s in hass.states().all() {
        out.insert(
            s.entity_id.to_string(),
            json!({"state": s.state.as_str(), "attributes": s.attributes}),
        );
    }
    Value::Object(out)
}

fn entry_state(r: &Running) -> Option<EntryState> {
    r.store.runtime("tuya1").map(|rt| rt.state)
}

async fn loaded(base: &str) -> Running {
    let r = start(base, false);
    until("loaded", || entry_state(&r) == Some(EntryState::Loaded)).await;
    r
}

#[test]
fn crypto_matches_the_sdk() {
    let v = vector("crypto");
    let s = |k: &str| v[k].as_str().unwrap().to_owned();
    let hk = hex(&md5::Md5::digest(
        format!("{}{}", s("rid"), s("refresh_token")).as_bytes(),
    ));
    assert_eq!(hk, s("hash_key"));
    assert_eq!(api::secret(&s("rid"), &hk), s("secret"));
    let h = &v["headers"];
    let headers: Vec<(&str, &str)> = ["X-appKey", "X-requestId", "X-sid", "X-time", "X-token"]
        .iter()
        .map(|k| (*k, h[*k].as_str().unwrap()))
        .collect();
    assert_eq!(api::sign(&hk, &headers, &s("query"), &s("body")), s("sign"));
    // The SDK's ciphertext, opened here, is its plain text.
    let enc = v["encrypt"]["out"].as_str().unwrap();
    let plain: Value = serde_json::from_str(v["encrypt"]["plain"].as_str().unwrap()).unwrap();
    assert_eq!(open(enc, &s("secret")), plain);
}

#[tokio::test]
async fn setup_matches_ha() {
    let (base, cloud) = cloud().await;
    let r = loaded(&base).await;
    let want = vector("setup");
    assert_eq!(states(&r.hass), want["states"]);
    // The registry rows HA made, field by field.
    let rows = serde_json::to_value(&r.registry.snapshot().entities).unwrap();
    let keys: Vec<String> = want["entities"][0]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    let mut got: Vec<Value> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let row: Map<String, Value> = keys
                .iter()
                .map(|k| (k.clone(), e.get(k).cloned().unwrap_or(Value::Null)))
                .collect();
            Value::Object(row)
        })
        .collect();
    got.sort_by_key(|e| e["unique_id"].as_str().unwrap().to_owned());
    assert_eq!(Value::Array(got), want["entities"]);
    let devices = serde_json::to_value(&r.registry.snapshot().devices).unwrap();
    let mut got: Vec<Value> = devices
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            json!({"identifiers": d["identifiers"], "manufacturer": d["manufacturer"],
                "model": d["model"], "model_id": d["model_id"], "name": d["name"]})
        })
        .collect();
    got.sort_by_key(|d| d["identifiers"].to_string());
    assert_eq!(Value::Array(got), want["devices"]);
    // The same requests, in the same order. hars has no Tuya scenes, so it
    // doesn't ask for them.
    let paths: Vec<String> = cloud
        .take()
        .into_iter()
        .map(|s| s.path)
        .filter(|p| p != "/v1.0/m/life/ha/access/config")
        .collect();
    let want_paths: Vec<String> = want["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| q["path"].as_str().unwrap().to_owned())
        .filter(|p| p != "/v1.0/m/scene/ha/home/scenes")
        .collect();
    assert_eq!(paths, want_paths);
}

#[tokio::test]
async fn services_match_ha() {
    let (base, cloud) = cloud().await;
    let r = loaded(&base).await;
    for step in vector("services").as_array().unwrap() {
        let call = &step["call"];
        let (domain, service) = (call[0].as_str().unwrap(), call[1].as_str().unwrap());
        cloud.take();
        let data = call[2].as_object().cloned().unwrap();
        let result = r
            .hass
            .call_service(domain, service, data, None, false, None)
            .await;
        let sent: Vec<Value> = cloud
            .take()
            .into_iter()
            .filter(|s| s.method == "POST")
            .map(|s| s.body.unwrap()["commands"].clone())
            .collect();
        assert_eq!(Value::Array(sent), step["sent"], "{call}");
        match (&result, step["error"].get("message")) {
            (Ok(_), None) => {}
            (Err(e), Some(m)) => assert_eq!(e.to_string(), m.as_str().unwrap(), "{call}"),
            _ => panic!("{call}: {result:?} against {}", step["error"]),
        }
    }
}

async fn read_packet(s: &mut tokio::net::TcpStream) -> (u8, Vec<u8>) {
    let first = s.read_u8().await.unwrap();
    let mut len = 0usize;
    let mut shift = 0;
    loop {
        let b = s.read_u8().await.unwrap();
        len |= ((b & 0x7f) as usize) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    let mut body = vec![0; len];
    s.read_exact(&mut body).await.unwrap();
    (first, body)
}

fn publish(payload: &Value) -> Vec<u8> {
    let mut body = vec![0, 1, b't'];
    body.extend_from_slice(payload.to_string().as_bytes());
    let mut p = vec![0x30];
    let mut len = body.len();
    loop {
        let mut b = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            b |= 0x80;
        }
        p.push(b);
        if len == 0 {
            break;
        }
    }
    p.extend_from_slice(&body);
    p
}

/// A broker: each connection's subscribed topics are reported, and each
/// message from `rx` is published on the connection there is. `None`
/// drops it.
async fn broker(
    mut rx: tokio::sync::mpsc::Receiver<Option<Value>>,
) -> (String, tokio::sync::mpsc::Receiver<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("tcp://{}", listener.local_addr().unwrap());
    let (tx, topics) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let _connect = read_packet(&mut s).await;
            s.write_all(&[0x20, 2, 0, 0]).await.unwrap();
            let (_, sub) = read_packet(&mut s).await;
            let (mut at, mut these) = (2, Vec::new());
            while at + 2 <= sub.len() {
                let n = u16::from_be_bytes([sub[at], sub[at + 1]]) as usize;
                these.push(String::from_utf8_lossy(&sub[at + 2..at + 2 + n]).into_owned());
                at += 3 + n;
            }
            tx.send(these).await.unwrap();
            while let Some(Some(msg)) = rx.recv().await {
                s.write_all(&publish(&msg)).await.unwrap();
            }
        }
    });
    (url, topics)
}

#[tokio::test]
async fn pushes_match_ha_and_a_dropped_link_reconnects() {
    let (base, cloud) = cloud().await;
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let (url, mut topics) = broker(rx).await;
    *cloud.broker.lock() = url;
    let r = loaded(&base).await;
    let want = vector("setup")["topics"].clone();
    assert_eq!(json!(topics.recv().await.unwrap()), want);
    for (i, step) in vector("pushes").as_array().unwrap().iter().enumerate() {
        if i == 2 {
            // The broker drops the link; the next message comes on a new
            // one, made with fresh credentials.
            tx.send(None).await.unwrap();
            assert_eq!(json!(topics.recv().await.unwrap()), want);
        }
        tx.send(Some(step["message"].clone())).await.unwrap();
        let expected = step["states"].clone();
        until(&format!("push {i}"), || states(&r.hass) == expected).await;
    }
    let configs = cloud
        .take()
        .iter()
        .filter(|s| s.path == "/v1.0/m/life/ha/access/config")
        .count();
    assert_eq!(configs, 2);
}

#[tokio::test]
async fn retries_until_the_cloud_answers() {
    let (base, cloud) = cloud().await;
    *cloud.fail.lock() = 1;
    let r = start(&base, false);
    until("retry", || entry_state(&r) == Some(EntryState::SetupRetry)).await;
    // The services exist while the cloud is down.
    for (domain, service) in [
        ("humidifier", "set_humidity"),
        ("fan", "set_direction"),
        ("select", "select_option"),
        ("number", "set_value"),
    ] {
        assert!(
            r.hass.services().has_service(domain, service),
            "{domain}.{service}"
        );
    }
    assert_eq!(states(&r.hass), json!({}));
    // Before start, HA retries once `homeassistant_started` fires.
    r.hass.start();
    until("loaded", || entry_state(&r) == Some(EntryState::Loaded)).await;
    assert_eq!(states(&r.hass), vector("setup")["states"]);
}

#[tokio::test]
async fn an_invalid_signature_is_not_retried() {
    let (base, cloud) = cloud().await;
    cloud.once.lock().insert(
        "GET /v1.0/m/life/users/homes".into(),
        VecDeque::from([json!({"error": {"code": -9999999, "msg": "sign invalid"}})]),
    );
    let r = start(&base, false);
    until("setup error", || {
        entry_state(&r) == Some(EntryState::SetupError)
    })
    .await;
}

#[tokio::test]
async fn an_expired_token_is_refreshed_and_kept() {
    let (base, cloud) = cloud().await;
    let r = start(&base, true);
    until("loaded", || entry_state(&r) == Some(EntryState::Loaded)).await;
    assert_eq!(cloud.take()[0].path, "/v1.0/m/token/refresh1");
    let kept = r.store.get("tuya1").unwrap()["data"]["token_info"].clone();
    assert_eq!(kept["access_token"], "access2");
    assert_eq!(kept["refresh_token"], "refresh2");
    assert_eq!(kept["uid"], "uid");
    // Later requests were keyed on the new refresh token, or they would
    // not have decrypted.
    assert_eq!(states(&r.hass), vector("setup")["states"]);
}

#[tokio::test]
async fn the_same_command_is_sent_once_unless_it_failed() {
    let (base, cloud) = cloud().await;
    let r = loaded(&base).await;
    let data = || {
        json!({"entity_id": "switch.dry_child_lock"})
            .as_object()
            .cloned()
            .unwrap()
    };
    cloud.take();
    *cloud.fail.lock() = 1;
    let call = |d| {
        r.hass
            .call_service("switch", "turn_on", d, None, false, None)
    };
    assert!(call(data()).await.is_err());
    call(data()).await.unwrap();
    // The SDK's filter: the same command within 10 s, once it was sent.
    call(data()).await.unwrap();
    let posts = cloud
        .take()
        .into_iter()
        .filter(|s| s.method == "POST")
        .count();
    assert_eq!(posts, 2);
}

#[test]
fn temperature_units_are_normalised() {
    let d = Dev::default();
    assert_eq!(temperature_unit(&d, Some("℃")), Some("°C"));
    assert_eq!(temperature_unit(&d, Some("F")), Some("°F"));
    assert_eq!(temperature_unit(&d, Some("")), None);
    let mut d = Dev::default();
    d.status.insert("temp_unit_convert".into(), json!("f"));
    assert_eq!(temperature_unit(&d, None), Some("°F"));
    assert!((convert_temp(68.0, "°F", "°C") - 20.0).abs() < 1e-9);
}

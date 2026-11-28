//! Against vectors made by running HA 2026.6.1's `mqtt` on paho-mqtt
//! 2.1.0 against a fake broker (`scripts/vectors/mqtt.py`): the same
//! steps are replayed against the same fake broker here, written in Rust,
//! on a simulated clock, and what `hars` sends, the states it writes and
//! the registry rows it makes are compared with what HA did.

use super::*;
use crate::config_entries::ConfigEntries;
use crate::light::Profiles;
use ha_core::Hass;
use ha_registry::{Registries, Registry};
use parking_lot::Mutex;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

// ---------------------------------------------------------------- the fake broker

fn read16(b: &[u8], i: &mut usize) -> Vec<u8> {
    let n = usize::from(u16::from_be_bytes([b[*i], b[*i + 1]]));
    let out = b[*i + 2..*i + 2 + n].to_vec();
    *i += 2 + n;
    out
}

fn varint(b: &[u8], i: &mut usize) -> u32 {
    let mut v = 0u32;
    let mut shift = 0;
    loop {
        let byte = b[*i];
        *i += 1;
        v |= u32::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return v;
        }
        shift += 7;
    }
}

fn put_varint(out: &mut Vec<u8>, mut n: usize) {
    loop {
        let b = (n % 128) as u8;
        n /= 128;
        out.push(if n > 0 { b | 0x80 } else { b });
        if n == 0 {
            return;
        }
    }
}

/// MQTT 5 properties, by paho's names (the ones `hars` sends).
fn props(b: &[u8], i: &mut usize) -> Value {
    let len = varint(b, i) as usize;
    let end = *i + len;
    let mut m = Map::new();
    while *i < end {
        let id = b[*i];
        *i += 1;
        match id {
            0x02 => {
                let v = u32::from_be_bytes([b[*i], b[*i + 1], b[*i + 2], b[*i + 3]]);
                *i += 4;
                m.insert("MessageExpiryInterval".into(), json!(v));
            }
            0x0B => {
                let v = varint(b, i);
                m.entry("SubscriptionIdentifier")
                    .or_insert_with(|| json!([]))
                    .as_array_mut()
                    .unwrap()
                    .push(json!(v));
            }
            other => panic!("property {other:#x}"),
        }
    }
    Value::Object(m)
}

/// A packet from `hars`, as `mqtt.py`'s `decode` writes HA's.
fn decode(first: u8, b: &[u8], v5: bool) -> Value {
    let mut i = 0;
    let text = |b: Vec<u8>| match String::from_utf8(b) {
        Ok(s) => json!(s),
        Err(e) => {
            json!({"hex": e.into_bytes().iter().map(|x| format!("{x:02x}")).collect::<String>()})
        }
    };
    match first >> 4 {
        1 => {
            let name = read16(b, &mut i);
            let (level, flags) = (b[i], b[i + 1]);
            let keepalive = u16::from_be_bytes([b[i + 2], b[i + 3]]);
            i += 4;
            let mut out = json!({"type": "CONNECT", "protocol": String::from_utf8(name).unwrap(),
                                 "level": level, "clean": flags & 2 != 0, "keepalive": keepalive});
            if level == 5 {
                out["properties"] = props(b, &mut i);
            }
            out["client_id"] = json!(String::from_utf8(read16(b, &mut i)).unwrap());
            if flags & 4 != 0 {
                let mut will = json!({"qos": (flags >> 3) & 3, "retain": flags & 0x20 != 0});
                if level == 5 {
                    will["properties"] = props(b, &mut i);
                }
                will["topic"] = text(read16(b, &mut i));
                will["payload"] = text(read16(b, &mut i));
                out["will"] = will;
            }
            if flags & 0x80 != 0 {
                out["username"] = text(read16(b, &mut i));
            }
            if flags & 0x40 != 0 {
                out["password"] = text(read16(b, &mut i));
            }
            out
        }
        3 => {
            let qos = (first >> 1) & 3;
            let topic = text(read16(b, &mut i));
            let mut out = json!({"type": "PUBLISH", "topic": topic, "qos": qos,
                                 "retain": first & 1 == 1, "dup": first & 8 != 0});
            if qos > 0 {
                out["packet_id"] = json!(u16::from_be_bytes([b[i], b[i + 1]]));
                i += 2;
            }
            if v5 {
                out["properties"] = props(b, &mut i);
            }
            out["payload"] = text(b[i..].to_vec());
            out
        }
        4..=7 => {
            let name = ["PUBACK", "PUBREC", "PUBREL", "PUBCOMP"][usize::from((first >> 4) - 4)];
            json!({"type": name, "packet_id": u16::from_be_bytes([b[0], b[1]])})
        }
        8 => {
            let id = u16::from_be_bytes([b[0], b[1]]);
            i = 2;
            let mut out = json!({"type": "SUBSCRIBE", "packet_id": id});
            if v5 {
                out["properties"] = props(b, &mut i);
            }
            let mut topics = Vec::new();
            while i < b.len() {
                let t = String::from_utf8(read16(b, &mut i)).unwrap();
                topics.push(json!([t, b[i]]));
                i += 1;
            }
            out["topics"] = json!(topics);
            out
        }
        10 => {
            let id = u16::from_be_bytes([b[0], b[1]]);
            i = 2;
            let mut out = json!({"type": "UNSUBSCRIBE", "packet_id": id});
            if v5 {
                out["properties"] = props(b, &mut i);
            }
            let mut topics = Vec::new();
            while i < b.len() {
                topics.push(json!(String::from_utf8(read16(b, &mut i)).unwrap()));
            }
            out["topics"] = json!(topics);
            out
        }
        12 => json!({"type": "PINGREQ"}),
        14 => json!({"type": "DISCONNECT"}),
        other => json!({"type": format!("unknown {other}")}),
    }
}

struct Conn {
    write: mpsc::UnboundedSender<Option<Vec<u8>>>,
    /// Closed by the broker: what comes after isn't read.
    dropped: bool,
    v5: bool,
    connected: bool,
    subs: Vec<(String, u8, Option<u32>)>,
    next_id: u16,
}

#[derive(Default)]
struct BrokerState {
    v5_ok: bool,
    conns: Vec<Conn>,
    retained: Vec<(String, Vec<u8>, u8)>,
    /// (connection, packet) `hars` sent.
    sent: Vec<(usize, Value)>,
    /// What `hars` was sent.
    delivered: Vec<Value>,
    /// Refuse every `CONNECT` with this code.
    refuse: Option<u8>,
    /// Refuse connections.
    down: bool,
}

#[derive(Clone)]
struct Broker {
    port: u16,
    state: Arc<Mutex<BrokerState>>,
}

fn str16(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s);
}

impl BrokerState {
    fn deliver(
        &mut self,
        c: usize,
        topic: &str,
        payload: &[u8],
        qos: u8,
        retain: bool,
        ids: &[u32],
    ) {
        let conn = &mut self.conns[c];
        let mut body = Vec::new();
        str16(&mut body, topic.as_bytes());
        if qos > 0 {
            conn.next_id = conn.next_id % 65535 + 1;
            body.extend_from_slice(&conn.next_id.to_be_bytes());
        }
        if conn.v5 {
            let mut p = Vec::new();
            for id in ids {
                p.push(0x0B);
                put_varint(&mut p, *id as usize);
            }
            put_varint(&mut body, p.len());
            body.extend(p);
        }
        body.extend_from_slice(payload);
        let mut out = vec![0x30 | (qos << 1) | u8::from(retain)];
        put_varint(&mut out, body.len());
        out.extend(body);
        let _ = conn.write.send(Some(out));
        self.delivered.push(json!({"conn": c, "topic": topic,
                                   "payload": String::from_utf8_lossy(payload), "qos": qos,
                                   "retain": retain}));
    }

    fn route(&mut self, topic: &str, payload: &[u8], qos: u8, retain: bool) {
        if retain {
            let at = self.retained.iter().position(|r| r.0 == topic);
            match (payload.is_empty(), at) {
                (true, Some(i)) => {
                    self.retained.remove(i);
                }
                (true, None) => {}
                (false, Some(i)) => self.retained[i] = (topic.into(), payload.to_vec(), qos),
                (false, None) => self.retained.push((topic.into(), payload.to_vec(), qos)),
            }
        }
        for c in 0..self.conns.len() {
            if !self.conns[c].connected {
                continue;
            }
            let got: Vec<(u8, Option<u32>)> = self.conns[c]
                .subs
                .iter()
                .filter(|(f, _, _)| client::matches(f, topic))
                .map(|(_, q, i)| (*q, *i))
                .collect();
            if got.is_empty() {
                continue;
            }
            let q = qos.min(got.iter().map(|g| g.0).max().unwrap());
            let mut ids: Vec<u32> = got.iter().filter_map(|g| g.1).collect();
            ids.sort_unstable();
            self.deliver(c, topic, payload, q, false, &ids);
        }
    }

    fn answer(&mut self, c: usize, p: &Value) -> bool {
        let send = |s: &BrokerState, bytes: Vec<u8>| {
            let _ = s.conns[c].write.send(Some(bytes));
        };
        match p["type"].as_str().unwrap() {
            "CONNECT" => {
                if self.conns[c].v5 && !self.v5_ok {
                    send(self, vec![0x20, 2, 0, 1]);
                    return false;
                }
                if let Some(code) = self.refuse {
                    send(self, vec![0x20, 2, 0, code]);
                    return false;
                }
                let body = match self.conns[c].v5 {
                    // Properties as `mqtt.py`'s broker sends them.
                    true => vec![
                        0, 0, 16, 0x22, 0, 10, 0x21, 0, 20, 0x25, 1, 0x24, 2, 0x28, 1, 0x29, 1,
                        0x2A, 1,
                    ],
                    false => vec![0, 0],
                };
                let mut out = vec![0x20];
                put_varint(&mut out, body.len());
                out.extend(body);
                send(self, out);
                self.conns[c].connected = true;
            }
            "SUBSCRIBE" => {
                let id = p["properties"]["SubscriptionIdentifier"][0]
                    .as_u64()
                    .map(|i| i as u32);
                let topics: Vec<(String, u8)> = p["topics"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| {
                        (
                            t[0].as_str().unwrap().to_owned(),
                            t[1].as_u64().unwrap() as u8,
                        )
                    })
                    .collect();
                let mut body = (p["packet_id"].as_u64().unwrap() as u16)
                    .to_be_bytes()
                    .to_vec();
                if self.conns[c].v5 {
                    body.push(0);
                }
                body.extend(topics.iter().map(|t| t.1 & 3));
                let mut out = vec![0x90];
                put_varint(&mut out, body.len());
                out.extend(body);
                send(self, out);
                for (t, q) in &topics {
                    self.conns[c].subs.retain(|s| s.0 != *t);
                    self.conns[c].subs.push((t.clone(), *q, id));
                }
                for (t, q) in &topics {
                    let matching: Vec<(String, Vec<u8>, u8)> = self
                        .retained
                        .iter()
                        .filter(|r| client::matches(t, &r.0))
                        .cloned()
                        .collect();
                    for (rt, payload, rq) in matching {
                        let ids: Vec<u32> = id.into_iter().collect();
                        self.deliver(c, &rt, &payload, rq.min(*q), true, &ids);
                    }
                }
            }
            "UNSUBSCRIBE" => {
                let n = p["topics"].as_array().unwrap().len();
                let mut body = (p["packet_id"].as_u64().unwrap() as u16)
                    .to_be_bytes()
                    .to_vec();
                if self.conns[c].v5 {
                    body.push(0);
                    body.extend(std::iter::repeat_n(0, n));
                }
                let mut out = vec![0xB0];
                put_varint(&mut out, body.len());
                out.extend(body);
                send(self, out);
                for t in p["topics"].as_array().unwrap() {
                    self.conns[c].subs.retain(|s| s.0 != t.as_str().unwrap());
                }
            }
            "PUBLISH" => {
                let id = p["packet_id"].as_u64().unwrap_or(0) as u16;
                match p["qos"].as_u64().unwrap() {
                    1 => send(self, vec![0x40, 2, (id >> 8) as u8, id as u8]),
                    2 => send(self, vec![0x50, 2, (id >> 8) as u8, id as u8]),
                    _ => {}
                }
                let payload = p["payload"]
                    .as_str()
                    .unwrap_or_default()
                    .as_bytes()
                    .to_vec();
                let topic = p["topic"].as_str().unwrap().to_owned();
                self.route(
                    &topic,
                    &payload,
                    p["qos"].as_u64().unwrap() as u8,
                    p["retain"].as_bool().unwrap(),
                );
            }
            "PUBREL" => {
                let id = p["packet_id"].as_u64().unwrap() as u16;
                send(self, vec![0x70, 2, (id >> 8) as u8, id as u8]);
            }
            "PINGREQ" => send(self, vec![0xD0, 0]),
            "DISCONNECT" => return false,
            _ => {}
        }
        true
    }
}

/// One connection's broker side.
fn serve(st: Arc<Mutex<BrokerState>>, stream: tokio::io::DuplexStream) {
    let (mut read, mut write) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::unbounded_channel::<Option<Vec<u8>>>();
    let c = {
        let mut s = st.lock();
        s.conns.push(Conn {
            write: tx,
            dropped: false,
            v5: false,
            connected: false,
            subs: Vec::new(),
            next_id: 0,
        });
        s.conns.len() - 1
    };
    tokio::spawn(async move {
        while let Some(Some(bytes)) = rx.recv().await {
            if write.write_all(&bytes).await.is_err() {
                return;
            }
        }
        // Closed: `hars` reads its end's EOF.
        let _ = write.shutdown().await;
    });
    tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            while let Ok(Some((first, body))) = packet::frame(&buf) {
                let end = body.end;
                let mut s = st.lock();
                if first >> 4 == 1 {
                    s.conns[c].v5 = buf[body.start + 6] == 5;
                }
                let v5 = s.conns[c].v5;
                let p = decode(first, &buf[body], v5);
                s.sent.push((c, p.clone()));
                let keep = s.answer(c, &p);
                drop(s);
                buf.drain(..end);
                if !keep {
                    let mut s = st.lock();
                    s.conns[c].connected = false;
                    let _ = s.conns[c].write.send(None);
                    return;
                }
            }
            match read.read(&mut chunk).await {
                Ok(0) | Err(_) => {
                    st.lock().conns[c].connected = false;
                    return;
                }
                Ok(n) => {
                    if st.lock().conns[c].dropped {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
        }
    });
}

impl Broker {
    async fn start(v5_ok: bool) -> Broker {
        let state = Arc::new(Mutex::new(BrokerState {
            v5_ok,
            ..Default::default()
        }));
        Broker { port: 1883, state }
    }

    /// Connections to this broker: in-memory pipes (a real socket's
    /// readiness races the paused clock).
    fn dial(&self) -> client::Dial {
        let st = self.state.clone();
        Arc::new(move |host, port| {
            let st = st.clone();
            Box::pin(async move {
                assert_eq!((host.as_str(), port), ("127.0.0.1", 1883));
                if st.lock().down {
                    return Err(std::io::ErrorKind::ConnectionRefused.into());
                }
                let (ours, theirs) = tokio::io::duplex(1 << 16);
                serve(st, theirs);
                Ok(Box::new(ours) as Box<dyn client::Stream>)
            })
        })
    }

    fn publish(&self, topic: &str, payload: &str, retain: bool, qos: u8) {
        self.state
            .lock()
            .route(topic, payload.as_bytes(), qos, retain);
    }

    /// Close every connection, as a broker restart does.
    fn drop_all(&self) {
        let mut s = self.state.lock();
        for c in &mut s.conns {
            c.connected = false;
            c.dropped = true;
            let _ = c.write.send(None);
        }
    }

    fn take(&self) -> Vec<(usize, Value)> {
        std::mem::take(&mut self.state.lock().sent)
    }

    fn received(&self) -> Vec<Value> {
        std::mem::take(&mut self.state.lock().delivered)
    }
}

// ---------------------------------------------------------------- replaying

/// Topics HA subscribes to for other integrations' config flows, which
/// `hars` doesn't (`generated/mqtt.py`).
const INTEGRATION_TOPICS: &[&str] = &[
    "drop_connect/discovery/#",
    "dsmr/#",
    "esphome/discover/#",
    "fully/deviceInfo/+",
    "inels/status/#",
    "pglab/discovery/#",
    "cloudapp/QBUSMQTTGW/state",
    "cloudapp/QBUSMQTTGW/config",
    "cloudapp/QBUSMQTTGW/+/state",
    "tasmota/discovery/#",
];

/// What is compared of a step's packets: the wildcard subscriptions in
/// order, the plain topics subscribed (a set: HA's order within a batch
/// follows its tasks), and every other packet but pings in order, with
/// `hars`'s own packet ids numbered by appearance.
#[derive(Debug, PartialEq)]
struct Sent {
    wildcards: Vec<Value>,
    plain: BTreeSet<(String, u64)>,
    others: Vec<Value>,
}

fn sent(packets: &[Value], ids: &mut BTreeMap<u64, usize>) -> Sent {
    let mut out = Sent {
        wildcards: Vec::new(),
        plain: BTreeSet::new(),
        others: Vec::new(),
    };
    let mut number = |id: &Value| {
        let id = id.as_u64().unwrap();
        let n = ids.len();
        json!(*ids.entry(id).or_insert(n))
    };
    for p in packets {
        let mut p = p.clone();
        match p["type"].as_str().unwrap() {
            "PINGREQ" => {}
            "SUBSCRIBE" => {
                let topics: Vec<Value> = p["topics"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|t| !INTEGRATION_TOPICS.contains(&t[0].as_str().unwrap()))
                    .cloned()
                    .collect();
                for t in &topics {
                    let topic = t[0].as_str().unwrap();
                    if topic.contains(['+', '#']) {
                        out.wildcards.push(json!([topic, t[1], p["properties"]]));
                    } else {
                        out.plain.insert((topic.to_owned(), t[1].as_u64().unwrap()));
                    }
                }
            }
            "PUBLISH" | "PUBREL" => {
                if let Some(id) = p.get("packet_id").cloned() {
                    p["packet_id"] = number(&id);
                }
                out.others.push(p);
            }
            "UNSUBSCRIBE" => {
                p.as_object_mut().unwrap().remove("packet_id");
                let mut t: Vec<String> = p["topics"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t.as_str().unwrap().to_owned())
                    .collect();
                t.sort();
                p["topics"] = json!(t);
                out.others.push(p);
            }
            _ => out.others.push(p),
        }
    }
    out
}

/// The `mqtt` entities' states: what changed since `before`.
fn changed(
    hass: &Hass,
    before: &mut BTreeMap<String, Arc<ha_core::State>>,
) -> BTreeMap<String, Value> {
    let mut now = BTreeMap::new();
    for s in hass.states().all() {
        if matches!(s.entity_id.domain(), "sensor" | "binary_sensor") {
            now.insert(s.entity_id.as_str().to_owned(), s);
        }
    }
    let mut out = BTreeMap::new();
    let keys: BTreeSet<String> = now.keys().chain(before.keys()).cloned().collect();
    for k in keys {
        match (before.get(&k), now.get(&k)) {
            (Some(_), None) => {
                out.insert(k, Value::Null);
            }
            (old, Some(new)) => {
                let same = old.is_some_and(|o| {
                    o.state == new.state
                        && o.attributes == new.attributes
                        && o.last_changed == new.last_changed
                        && o.last_updated == new.last_updated
                });
                if same {
                    continue;
                }
                let mut v = json!({"state": new.state.as_str(), "attributes": new.attributes});
                if let Some(o) = old {
                    v["changed"] = json!(o.last_changed != new.last_changed);
                    v["updated"] = json!(o.last_updated != new.last_updated);
                }
                out.insert(k, v);
            }
            (None, None) => {}
        }
    }
    *before = now;
    out
}

fn vector(name: &str) -> Value {
    let text = match name {
        "v5" => include_str!("vectors/v5.json"),
        "v311" => include_str!("vectors/v311.json"),
        _ => unreachable!(),
    };
    serde_json::from_str(text).unwrap()
}

/// What a replay left.
struct Replayed {
    hass: Hass,
    registry: Arc<Registry>,
    entries: Arc<ConfigEntries>,
    broker: Broker,
}

/// Replay a vector's scenario against `hars` and check each step.
async fn replay(name: &str) -> Replayed {
    let v = vector(name);
    let hass = Hass::with_clock(
        ha_core::SimulatedClock::new("2026-06-01T12:00:00Z".parse().unwrap()).clock(),
    );
    let registry = Registry::in_memory(&hass, Registries::default());
    let components =
        crate::components::Components::new(&hass, registry.clone(), Profiles::default());
    let broker = Broker::start(name == "v5").await;
    for step in v["retained_before"].as_array().unwrap() {
        broker.publish(
            step["topic"].as_str().unwrap(),
            step["payload"].as_str().unwrap(),
            step["retain"].as_bool().unwrap(),
            step["qos"].as_u64().unwrap() as u8,
        );
    }
    let mut entry = v["entry"].as_object().unwrap().clone();
    entry.insert("entry_id".into(), json!("mqtt-entry"));
    entry.insert("domain".into(), json!("mqtt"));
    entry.get_mut("data").unwrap()["port"] = json!(broker.port);
    let entries = ConfigEntries::in_memory(&hass, vec![entry.clone()]);
    let status = crate::config_entries::Status::new(entries.clone(), "mqtt-entry");
    tokio::spawn(run(
        components.clone(),
        entry,
        Some(entries.clone()),
        broker.dial(),
        hass.setup_guard(),
        status,
    ));
    let mut ids = BTreeMap::new();
    let mut hars_ids = BTreeMap::new();
    let mut states = BTreeMap::new();
    for (k, step) in v["steps"].as_array().unwrap().iter().enumerate() {
        let what = format!("{name} step {k} ({})", step["do"]);
        match step["do"].as_str().unwrap() {
            "setup" => {
                hass.setups_done().await;
                assert_eq!(
                    entries.runtime("mqtt-entry").map(|r| r.state),
                    Some(crate::config_entries::EntryState::Loaded),
                    "{what}"
                );
            }
            "start" => {
                hass.start();
                tokio::time::sleep(Duration::from_secs(step["waited"].as_u64().unwrap())).await;
            }
            "publish" => {
                broker.publish(
                    step["topic"].as_str().unwrap(),
                    step["payload"].as_str().unwrap(),
                    step["retain"].as_bool().unwrap(),
                    step["qos"].as_u64().unwrap() as u8,
                );
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            "wait" => {
                tokio::time::sleep(Duration::from_secs(step["seconds"].as_u64().unwrap())).await;
            }
            "drop" => {
                broker.drop_all();
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            "service" => {
                let data = step["data"].as_object().unwrap().clone();
                let got = hass
                    .call_service(
                        "mqtt",
                        step["service"].as_str().unwrap(),
                        data,
                        None,
                        false,
                        None,
                    )
                    .await;
                let want = &step["result"];
                match (&got, want.as_str()) {
                    (Ok(_), Some("ok")) => {}
                    (Err(e), None) => assert_eq!(
                        e.to_string()
                            .trim_start_matches("invalid service data: ")
                            .to_owned(),
                        want["message"].as_str().unwrap(),
                        "{what}"
                    ),
                    _ => panic!("{what}: {got:?}, want {want}"),
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            other => panic!("{other}"),
        }
        // `settle`: three rounds of 0.05 s.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let want_sent: Vec<Value> = step["sent"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                let mut p = p["packet"].clone();
                if p["client_id"] == "<random>" {
                    p["client_id"] = json!("");
                }
                p
            })
            .collect();
        let got_sent: Vec<Value> = broker
            .take()
            .into_iter()
            .map(|(_, mut p)| {
                if p["type"] == "CONNECT"
                    && v["entry"]["data"].get("client_id").is_none()
                    && let Some(id) = p["client_id"].as_str()
                {
                    assert_eq!(id.len(), 22, "{id}");
                    assert!(id.bytes().all(|b| b.is_ascii_alphanumeric()), "{id}");
                    p["client_id"] = json!("");
                }
                p
            })
            .collect();
        assert_eq!(
            sent(&got_sent, &mut hars_ids),
            sent(&want_sent, &mut ids),
            "{what}: packets"
        );
        // The broker sent `hars` what it sent HA.
        let strip = |r: &Value| {
            json!({"topic": r["topic"], "payload": r["payload"], "qos": r["qos"],
                   "retain": r["retain"]})
        };
        // As a multiset: within one `SUBSCRIBE` of plain topics HA's order
        // follows its tasks, and the broker's retained messages that.
        let mut got_received: Vec<Value> = broker.received().iter().map(strip).collect();
        let mut want_received: Vec<Value> = step["received"]
            .as_array()
            .map(|r| r.iter().map(strip).collect())
            .unwrap_or_default();
        got_received.sort_by_key(Value::to_string);
        want_received.sort_by_key(Value::to_string);
        for (i, (g, w)) in got_received.iter().zip(&want_received).enumerate() {
            assert_eq!(g, w, "{what}: received #{i}");
        }
        assert_eq!(got_received.len(), want_received.len(), "{what}: received");
        let got_states = changed(&hass, &mut states);
        let want_states: BTreeMap<String, Value> = step["states"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        assert_eq!(got_states, want_states, "{what}: states");
    }
    Replayed {
        hass,
        registry,
        entries,
        broker,
    }
}

/// The registry rows and the stored entry, as HA left them.
fn check_rows(name: &str, r: &Replayed) {
    let v = vector(name);
    let snap = r.registry.snapshot();
    let mut devices: Vec<Value> = Vec::new();
    for d in &snap.devices {
        let ids: Vec<Value> = d.identifiers.clone();
        devices.push(json!({"identifiers": ids, "connections": d.connections,
            "manufacturer": d.manufacturer, "model": d.model, "model_id": d.model_id,
            "name": d.name, "sw_version": d.sw_version, "hw_version": d.hw_version,
            "serial_number": d.serial_number, "configuration_url": d.configuration_url,
            "via": d.via_device_id.as_ref().and_then(|via| snap.devices.iter().find(|x| &x.id == via))
                .map(|x| json!(x.identifiers)),
            "primary_config_entry": d.primary_config_entry.as_ref().map(|_| "<entry>")}));
    }
    let want_devices: Vec<Value> = v["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            let via = d["via_device_id"].as_str().map(|via| {
                v["devices"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|x| x["id"] == via)
                    .unwrap()["identifiers"]
                    .clone()
            });
            json!({"identifiers": d["identifiers"], "connections": d["connections"],
                "manufacturer": d["manufacturer"], "model": d["model"], "model_id": d["model_id"],
                "name": d["name"], "sw_version": d["sw_version"], "hw_version": d["hw_version"],
                "serial_number": d["serial_number"], "configuration_url": d["configuration_url"],
                "via": via, "primary_config_entry": d["primary_config_entry"]})
        })
        .collect();
    let key = |d: &Value| d["identifiers"].to_string();
    let mut a = devices.clone();
    let mut b = want_devices.clone();
    a.sort_by_key(key);
    b.sort_by_key(key);
    assert_eq!(a, b, "{name}: devices");
    let device_ids = |id: &Option<String>| {
        id.as_ref()
            .and_then(|id| snap.devices.iter().find(|d| &d.id == id))
            .map(|d| json!(d.identifiers))
    };
    let mut entities: Vec<Value> = snap
        .entities
        .iter()
        .filter(|e| e.platform == "mqtt")
        .map(|e| {
            json!({"entity_id": e.entity_id, "unique_id": e.unique_id, "platform": e.platform,
                "config_entry_id": e.config_entry_id.as_ref().map(|_| "<entry>"),
                "device": device_ids(&e.device_id), "disabled_by": e.disabled_by,
                "hidden_by": e.hidden_by, "entity_category": e.entity_category,
                "has_entity_name": e.has_entity_name, "original_name": e.original_name,
                "original_device_class": e.original_device_class,
                "original_icon": e.original_icon, "object_id_base": e.object_id_base,
                "suggested_object_id": e.suggested_object_id,
                "capabilities": e.capabilities.as_ref().map(|c| c.value()),
                "supported_features": e.supported_features,
                "unit_of_measurement": e.unit_of_measurement, "options": e.options.value(),
                "translation_key": e.translation_key})
        })
        .collect();
    entities.sort_by_key(|e| e["entity_id"].as_str().unwrap().to_owned());
    let want_entities: Vec<Value> = v["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let mut e = e.clone();
            let device = e["device_id"].as_str().map(|id| {
                v["devices"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|x| x["id"] == id)
                    .unwrap()["identifiers"]
                    .clone()
            });
            let m = e.as_object_mut().unwrap();
            m.remove("device_id");
            m.insert("device".into(), json!(device));
            e
        })
        .collect();
    assert_eq!(entities.len(), want_entities.len(), "{name}: entity rows");
    for (got, want) in entities.iter().zip(&want_entities) {
        assert_eq!(got, want, "{name}: entity row");
    }
    let stored = r.entries.get("mqtt-entry").unwrap();
    let mut data = stored["data"].clone();
    data["port"] = json!("<port>");
    assert_eq!(data, v["stored_entry"]["data"], "{name}: stored data");
    assert_eq!(
        stored["options"], v["stored_entry"]["options"],
        "{name}: stored options"
    );
    assert_eq!(stored["version"], v["stored_entry"]["version"]);
    assert_eq!(stored["minor_version"], v["stored_entry"]["minor_version"]);
}

/// A 2026.6.1 entry on an MQTT 5 broker: Zigbee2MQTT's, Tasmota's and a
/// device's discovery, their states and availability, a changed, an
/// unchanged and an empty config, `mqtt.publish`, and the broker going
/// away and coming back.
#[tokio::test(start_paused = true)]
async fn mqtt_5_matches_ha() {
    let r = replay("v5").await;
    check_rows("v5", &r);
    let _ = (&r.hass, &r.broker);
}

/// An older entry on a 3.1.1 broker: migrated to 1.2, MQTT 5 tried and
/// refused, then 3.1.1 with its own client id, keepalive, prefix, birth
/// and will.
#[tokio::test(start_paused = true)]
async fn mqtt_3_1_1_matches_ha() {
    let r = replay("v311").await;
    check_rows("v311", &r);
}

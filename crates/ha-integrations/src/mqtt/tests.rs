//! Against vectors made by running HA 2026.6.1's `mqtt` on paho-mqtt
//! 2.1.0 against a fake broker (`scripts/vectors/mqtt.py`): the same
//! steps are replayed against the same fake broker here, written in Rust,
//! on a simulated clock, and what `hars` sends, the states it writes and
//! the registry rows it makes are compared with what HA did.

use super::*;
use crate::config_entries::ConfigEntries;
use crate::light::Profiles;
use ha_core::{EntityId, Hass};
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
    /// False: the broker stops reading (its buffer fills).
    reading: bool,
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
    /// Take `PUBLISH`es without acknowledging them.
    no_ack: bool,
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
                    _ if self.no_ack => {}
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
            "PUBREL" if self.no_ack => {}
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
            reading: true,
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
            while !st.lock().conns[c].reading {
                tokio::time::sleep(Duration::from_millis(100)).await;
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

// ---------------------------------------------------------------- timing and failures

/// An entry with `data`, started against `broker`; the times it dialled
/// are recorded (seconds since start).
struct Running {
    hass: Hass,
    entries: Arc<ConfigEntries>,
    dials: Arc<Mutex<Vec<f64>>>,
    task: tokio::task::JoinHandle<()>,
}

async fn start(broker: &Broker, data: Value) -> Running {
    let hass = Hass::with_clock(
        ha_core::SimulatedClock::new("2026-06-01T12:00:00Z".parse().unwrap()).clock(),
    );
    hass.start();
    let registry = Registry::in_memory(&hass, Registries::default());
    let components = crate::components::Components::new(&hass, registry, Profiles::default());
    let entry = json!({"entry_id": "e", "domain": "mqtt", "title": "127.0.0.1",
                       "version": 2, "minor_version": 1, "data": data, "options": {}});
    let entry = entry.as_object().unwrap().clone();
    let entries = ConfigEntries::in_memory(&hass, vec![entry.clone()]);
    let status = crate::config_entries::Status::new(entries.clone(), "e");
    let dials = Arc::new(Mutex::new(Vec::new()));
    let (inner, log, t0) = (broker.dial(), dials.clone(), tokio::time::Instant::now());
    let dial: client::Dial = Arc::new(move |host, port| {
        log.lock().push(t0.elapsed().as_secs_f64());
        inner(host, port)
    });
    let task = tokio::spawn(run(
        components,
        entry,
        Some(entries.clone()),
        dial,
        hass.setup_guard(),
        status,
    ));
    hass.setups_done().await;
    Running {
        hass,
        entries,
        dials,
        task,
    }
}

fn types(packets: &[(usize, Value)]) -> Vec<String> {
    packets
        .iter()
        .map(|(c, p)| format!("{c}:{}", p["type"].as_str().unwrap()))
        .collect()
}

/// A `PINGREQ` once `keepalive` seconds have passed with nothing sent or
/// nothing received (paho's test, on `hars`'s own send times); a broker
/// that doesn't answer one is left after another `keepalive`, and tried
/// again 10 s later.
#[tokio::test(start_paused = true)]
async fn pings_when_idle_and_leaves_a_broker_that_stops_answering() {
    let broker = Broker::start(true).await;
    let r = start(
        &broker,
        json!({"broker": "127.0.0.1", "port": 1883, "protocol": "5", "keepalive": 15,
               "discovery": false}),
    )
    .await;
    let t0 = tokio::time::Instant::now();
    let at = |t: u64| t0 + Duration::from_secs(t);
    // Connected at 0, the birth message at 5: nothing received since 0.
    tokio::time::sleep_until(at(14)).await;
    assert_eq!(types(&broker.take()), ["0:CONNECT", "0:PUBLISH"]);
    tokio::time::sleep_until(at(16)).await;
    assert_eq!(types(&broker.take()), ["0:PINGREQ"]);
    tokio::time::sleep_until(at(31)).await;
    assert_eq!(types(&broker.take()), ["0:PINGREQ"]);
    // A message each way at 33: the next ping at 48.
    tokio::time::sleep_until(at(33)).await;
    broker
        .state
        .lock()
        .deliver(0, "anything", b"x", 0, false, &[]);
    r.hass
        .call_service(
            "mqtt",
            "publish",
            json!({"topic": "a", "payload": "b"})
                .as_object()
                .unwrap()
                .clone(),
            None,
            false,
            None,
        )
        .await
        .unwrap();
    tokio::time::sleep_until(at(47)).await;
    assert_eq!(types(&broker.take()), ["0:PUBLISH"]);
    // The broker stops answering: the ping at 48 isn't, and at 63 the
    // connection is given up; 10 s later, tried again.
    broker.state.lock().conns[0].dropped = true;
    tokio::time::sleep_until(at(72)).await;
    assert_eq!(*r.dials.lock(), [0.0]);
    tokio::time::sleep_until(at(74)).await;
    assert_eq!(*r.dials.lock(), [0.0, 73.0]);
    assert_eq!(types(&broker.take())[0], "1:CONNECT");
}

/// A connection lost is tried again 10 s later, then every 10 s; a first
/// connection that fails is tried again at once. Entities are unavailable
/// meanwhile, and `mqtt.publish` fails as HA's does.
#[tokio::test(start_paused = true)]
async fn reconnects_on_has_schedule() {
    let broker = Broker::start(true).await;
    broker.state.lock().down = true;
    let r = start(
        &broker,
        json!({"broker": "127.0.0.1", "port": 1883, "protocol": "5"}),
    )
    .await;
    assert_eq!(*r.dials.lock(), [0.0, 0.0]);
    assert_eq!(
        r.entries.runtime("e").map(|x| x.state),
        Some(crate::config_entries::EntryState::Loaded)
    );
    let err = r
        .hass
        .call_service(
            "mqtt",
            "publish",
            json!({"topic": "a", "payload": "b"})
                .as_object()
                .unwrap()
                .clone(),
            None,
            false,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Error talking to MQTT: The client is not currently connected"
    );
    tokio::time::sleep(Duration::from_secs(25)).await;
    assert_eq!(*r.dials.lock(), [0.0, 0.0, 10.0, 20.0]);
    broker.state.lock().down = false;
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(types(&broker.take())[0], "0:CONNECT");
    let lost = r.dials.lock().len();
    broker.drop_all();
    tokio::time::sleep(Duration::from_millis(9900)).await;
    assert_eq!(r.dials.lock().len(), lost);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(r.dials.lock().len(), lost + 1);
}

/// Bad credentials end the tries (HA starts a reauthentication there).
#[tokio::test(start_paused = true)]
async fn refused_credentials_are_not_tried_again() {
    let broker = Broker::start(true).await;
    broker.state.lock().refuse = Some(0x86);
    let r = start(
        &broker,
        json!({"broker": "127.0.0.1", "port": 1883, "protocol": "5", "username": "u",
               "password": "wrong"}),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(*r.dials.lock(), [0.0]);
}

/// What `hars` doesn't do yet is said, and the entry isn't set up.
#[tokio::test(start_paused = true)]
async fn tls_and_websockets_entries_are_not_set_up() {
    for data in [
        json!({"broker": "b", "port": 8883, "protocol": "5", "certificate": "auto"}),
        json!({"broker": "b", "port": 80, "protocol": "5", "transport": "websockets"}),
    ] {
        let broker = Broker::start(true).await;
        let r = start(&broker, data).await;
        assert!(r.dials.lock().is_empty());
        assert_eq!(
            r.entries.runtime("e").map(|x| x.state),
            Some(crate::config_entries::EntryState::SetupError)
        );
    }
}

/// `async_migrate_entry`: 1.x and 2.1 are set up; a later 2.x or 3 isn't.
#[test]
fn entry_versions() {
    let entry = |v: u64, m: u64| {
        json!({"domain": "mqtt", "version": v, "minor_version": m})
            .as_object()
            .unwrap()
            .clone()
    };
    assert!(!crate::entry_too_new(&entry(1, 1)));
    assert!(!crate::entry_too_new(&entry(1, 2)));
    assert!(!crate::entry_too_new(&entry(2, 1)));
    assert!(crate::entry_too_new(&entry(2, 2)));
    assert!(crate::entry_too_new(&entry(3, 1)));
}

/// A QoS 2 message reaches its entity at its `PUBREL`, as paho delivers
/// it; an `expire_after` sensor is unavailable until a value comes, and
/// again once it is that old.
#[tokio::test(start_paused = true)]
async fn qos_2_and_expire_after() {
    let broker = Broker::start(true).await;
    broker.publish(
        "homeassistant/sensor/s/config",
        r#"{"stat_t": "s/state", "uniq_id": "s1", "exp_aft": 60, "qos": 2, "name": "S"}"#,
        true,
        0,
    );
    let r = start(
        &broker,
        json!({"broker": "127.0.0.1", "port": 1883, "protocol": "5"}),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let id = EntityId::parse("sensor.s").unwrap();
    let state = |r: &Running| r.hass.states().get(&id).unwrap().state.to_string();
    assert_eq!(state(&r), "unavailable");
    // Delivered at QoS 2: held until the broker's `PUBREL`.
    {
        let mut s = broker.state.lock();
        let conn = &mut s.conns[0];
        let mut body = vec![0, 7];
        body.extend_from_slice(b"s/state");
        body.extend_from_slice(&[0, 9, 0]);
        body.extend_from_slice(b"21");
        let mut out = vec![0x34, body.len() as u8];
        out.extend(body);
        let _ = conn.write.send(Some(out));
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(state(&r), "unavailable");
    assert!(broker.take().iter().any(|(_, p)| p["type"] == "PUBREC"));
    {
        let s = broker.state.lock();
        let _ = s.conns[0].write.send(Some(vec![0x62, 2, 0, 9]));
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(state(&r), "21");
    assert!(broker.take().iter().any(|(_, p)| p["type"] == "PUBCOMP"));
    tokio::time::sleep(Duration::from_secs(61)).await;
    assert_eq!(state(&r), "unavailable");
}

/// `cv.positive_int` has no upper bound: an `expire_after` or `off_delay`
/// past what a clock can hold is a timer that never fires (HA's
/// `async_call_later` with such a delay never comes due), not a panic.
#[tokio::test(start_paused = true)]
async fn timers_too_far_off_never_fire() {
    let broker = Broker::start(true).await;
    let huge = u64::MAX;
    broker.publish(
        "homeassistant/sensor/s/config",
        &json!({"stat_t": "s/state", "uniq_id": "s1", "exp_aft": huge, "name": "S"}).to_string(),
        true,
        0,
    );
    broker.publish(
        "homeassistant/binary_sensor/b/config",
        &json!({"stat_t": "b/state", "uniq_id": "b1", "off_dly": huge, "name": "B"}).to_string(),
        true,
        0,
    );
    // No keepalive: nothing else wakes the client over the year below.
    let r = start(
        &broker,
        json!({"broker": "127.0.0.1", "port": 1883, "protocol": "5", "keepalive": 0}),
    )
    .await;
    // Subscribed 0.5 s after the discovery subscriptions' retained configs.
    tokio::time::sleep(Duration::from_secs(2)).await;
    broker.publish("s/state", "21", false, 0);
    broker.publish("b/state", "ON", false, 0);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let state = |id: &str| {
        r.hass
            .states()
            .get(&EntityId::parse(id).unwrap())
            .unwrap()
            .state
            .to_string()
    };
    assert_eq!(state("sensor.s"), "21");
    assert_eq!(state("binary_sensor.b"), "on");
    tokio::time::sleep(Duration::from_secs(365 * 86400)).await;
    assert_eq!(state("sensor.s"), "21");
    assert_eq!(state("binary_sensor.b"), "on");
}

/// A broker that stops reading: once the connection's buffer is full a
/// write waits at most `keepalive` (paho's queue is cut off by its
/// keepalive check then), the connection is dropped and tried again 10 s
/// later; the action that waited on it fails as a disconnected publish
/// does, and the client goes on.
#[tokio::test(start_paused = true)]
async fn a_broker_that_stops_reading_is_left() {
    let broker = Broker::start(true).await;
    let r = start(
        &broker,
        json!({"broker": "127.0.0.1", "port": 1883, "protocol": "5", "keepalive": 15,
               "discovery": false}),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    broker.state.lock().conns[0].reading = false;
    let t0 = tokio::time::Instant::now();
    let big = "x".repeat(200 * 1024);
    let hass = r.hass.clone();
    let call = tokio::spawn(async move {
        hass.call_service(
            "mqtt",
            "publish",
            json!({"topic": "big", "payload": big})
                .as_object()
                .unwrap()
                .clone(),
            None,
            false,
            None,
        )
        .await
    });
    tokio::time::sleep(Duration::from_secs(16)).await;
    assert!(
        call.is_finished(),
        "the call still waits after {:?}",
        t0.elapsed()
    );
    tokio::time::sleep(Duration::from_secs(10)).await;
    let dials = r.dials.lock().clone();
    assert_eq!(dials.len(), 2, "{dials:?}");
}

/// QoS 2 messages a broker never releases are held up to a budget; past
/// it the connection is a protocol error, dropped and tried again.
#[tokio::test(start_paused = true)]
async fn unreleased_qos_2_messages_are_bounded() {
    let broker = Broker::start(true).await;
    let r = start(
        &broker,
        json!({"broker": "127.0.0.1", "port": 1883, "protocol": "5", "discovery": false}),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let payload = vec![b'x'; 60 * 1024];
    for id in 1..=40u16 {
        let mut body = vec![0, 1, b't'];
        body.extend_from_slice(&id.to_be_bytes());
        body.push(0);
        body.extend_from_slice(&payload);
        let mut out = vec![0x34];
        put_varint(&mut out, body.len());
        out.extend(body);
        let _ = broker.state.lock().conns[0].write.send(Some(out));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_secs(11)).await;
    let dials = r.dials.lock().clone();
    assert_eq!(dials.len(), 2, "{dials:?}");
}

fn publish_call(r: &Running, data: Value) -> tokio::task::JoinHandle<Result<(), String>> {
    let hass = r.hass.clone();
    tokio::spawn(async move {
        hass.call_service(
            "mqtt",
            "publish",
            data.as_object().unwrap().clone(),
            None,
            false,
            None,
        )
        .await
        .map(drop)
        .map_err(|e| e.to_string())
    })
}

/// paho keeps a QoS 1 or 2 message until the broker has it: one in flight
/// when the connection is lost, and one published while disconnected,
/// are published again after the reconnect (on HA's clean session, a
/// QoS 2 one as a `PUBLISH` even after its `PUBREC`), the first marked
/// `dup`, under its packet id (`_messages_reconnect_reset_out`,
/// `_handle_connack`).
#[tokio::test(start_paused = true)]
async fn messages_in_flight_are_sent_again_after_a_reconnect() {
    let broker = Broker::start(true).await;
    let r = start(
        &broker,
        json!({"broker": "127.0.0.1", "port": 1883, "protocol": "5", "discovery": false}),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    broker.take();
    broker.state.lock().no_ack = true;
    let one = publish_call(&r, json!({"topic": "a", "payload": "1", "qos": 1}));
    let two = publish_call(&r, json!({"topic": "b", "payload": "2", "qos": 2}));
    tokio::time::sleep(Duration::from_secs(1)).await;
    let sent = broker.take();
    let ids: Vec<u64> = sent
        .iter()
        .map(|(_, p)| p["packet_id"].as_u64().unwrap())
        .collect();
    broker.drop_all();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let queued = publish_call(&r, json!({"topic": "c", "payload": "3", "qos": 1}));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        queued.await.unwrap(),
        Err("Error talking to MQTT: The client is not currently connected".into())
    );
    broker.state.lock().no_ack = false;
    tokio::time::sleep(Duration::from_secs(10)).await;
    let again: Vec<Value> = broker
        .take()
        .into_iter()
        .filter(|(c, p)| *c == 1 && p["type"] == "PUBLISH")
        .map(|(_, p)| json!([p["topic"], p["qos"], p["dup"], p["packet_id"]]))
        .collect();
    assert_eq!(
        again,
        [
            json!(["a", 1, true, ids[0]]),
            json!(["b", 2, true, ids[1]]),
            json!(["c", 1, false, ids[1] + 1]),
        ]
    );
    // HA's actions had stopped waiting after 10 s, with its warning, and
    // succeeded (`_async_wait_for_mid_or_raise`).
    assert_eq!(one.await.unwrap(), Ok(()));
    assert_eq!(two.await.unwrap(), Ok(()));
}

/// An acknowledgement of the wrong kind doesn't end a publish: a
/// `PUBACK` or `PUBCOMP` for a QoS 2 message before its `PUBREC`.
#[tokio::test(start_paused = true)]
async fn only_the_expected_acknowledgement_ends_a_publish() {
    let broker = Broker::start(true).await;
    let r = start(
        &broker,
        json!({"broker": "127.0.0.1", "port": 1883, "protocol": "5", "discovery": false}),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    broker.take();
    broker.state.lock().no_ack = true;
    let call = publish_call(&r, json!({"topic": "a", "payload": "1", "qos": 2}));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let id = broker.take()[0].1["packet_id"].as_u64().unwrap() as u16;
    let ack = |first: u8| {
        let s = broker.state.lock();
        let _ = s.conns[0]
            .write
            .send(Some(vec![first, 2, (id >> 8) as u8, id as u8]));
    };
    ack(0x40);
    ack(0x70);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!call.is_finished());
    ack(0x50);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!call.is_finished());
    assert!(broker.take().iter().any(|(_, p)| p["type"] == "PUBREL"));
    ack(0x70);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(call.await.unwrap(), Ok(()));
}

/// An entry stopped (its task aborted, as removing it does): every entity
/// it ran goes with its state, one without a unique id (no registry row)
/// too, and the client says goodbye with a `DISCONNECT`, so the broker
/// doesn't publish the will (HA's `async_unload_entry`).
#[tokio::test(start_paused = true)]
async fn a_stopped_entry_takes_its_entities_and_disconnects() {
    let broker = Broker::start(true).await;
    broker.publish(
        "homeassistant/sensor/n/config",
        r#"{"stat_t": "n/state", "name": "N"}"#,
        true,
        0,
    );
    let r = start(
        &broker,
        json!({"broker": "127.0.0.1", "port": 1883, "protocol": "5"}),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    let id = EntityId::parse("sensor.n").unwrap();
    assert!(r.hass.states().get(&id).is_some());
    broker.take();
    let Running { hass, task, .. } = r;
    task.abort();
    let _ = task.await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(hass.states().get(&id).is_none());
    assert_eq!(types(&broker.take()), ["0:DISCONNECT"]);
}

/// `date` and `timestamp` sensors (`sensor.py`'s `_update_state`): a date
/// is the payload's own, in its own offset; a timestamp is shown in UTC.
/// Expected values from HA 2026.6.1's `dt_util.parse_datetime`, run on
/// these payloads: `2026-11-28T00:30:00+02:00` is the date `2026-11-28`
/// and the time `2026-11-27T22:30:00+00:00`; `2026-11-28` is the date
/// `2026-11-28`.
#[tokio::test(start_paused = true)]
async fn dates_keep_their_offset_and_timestamps_are_utc() {
    let broker = Broker::start(true).await;
    broker.publish(
        "homeassistant/sensor/d/config",
        r#"{"stat_t": "d/state", "uniq_id": "d1", "name": "D", "dev_cla": "date"}"#,
        true,
        0,
    );
    broker.publish(
        "homeassistant/sensor/t/config",
        r#"{"stat_t": "t/state", "uniq_id": "t1", "name": "T", "dev_cla": "timestamp"}"#,
        true,
        0,
    );
    let r = start(
        &broker,
        json!({"broker": "127.0.0.1", "port": 1883, "protocol": "5"}),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let state = |id: &str| {
        r.hass
            .states()
            .get(&EntityId::parse(id).unwrap())
            .unwrap()
            .state
            .to_string()
    };
    broker.publish("d/state", "2026-11-28T00:30:00+02:00", false, 0);
    broker.publish("t/state", "2026-11-28T00:30:00+02:00", false, 0);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(state("sensor.d"), "2026-11-28");
    assert_eq!(state("sensor.t"), "2026-11-27T22:30:00+00:00");
    broker.publish("d/state", "2026-11-29", false, 0);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(state("sensor.d"), "2026-11-29");
}

/// The discovery subscriptions take the entry's `discovery_qos` option
/// (`discovery.py`: `config_entry.options.get(CONF_DISCOVERY_QOS, 0)`).
#[tokio::test(start_paused = true)]
async fn discovery_subscribes_at_the_entrys_qos() {
    let broker = Broker::start(true).await;
    let hass = Hass::new();
    hass.start();
    let registry = Registry::in_memory(&hass, Registries::default());
    let components = crate::components::Components::new(&hass, registry, Profiles::default());
    let entry = json!({"entry_id": "e", "domain": "mqtt", "title": "t", "version": 2,
                       "minor_version": 1, "options": {"discovery_qos": 1},
                       "data": {"broker": "127.0.0.1", "port": 1883, "protocol": "5"}});
    tokio::spawn(run(
        components,
        entry.as_object().unwrap().clone(),
        None,
        broker.dial(),
        hass.setup_guard(),
        crate::config_entries::Status::none(),
    ));
    tokio::time::sleep(Duration::from_secs(1)).await;
    let qos: Vec<u64> = broker
        .take()
        .iter()
        .filter(|(_, p)| p["type"] == "SUBSCRIBE")
        .map(|(_, p)| p["topics"][0][1].as_u64().unwrap())
        .collect();
    assert_eq!(qos.len(), 64);
    assert!(qos.iter().all(|q| *q == 1), "{qos:?}");
}

/// A subscription takes a retained message once per topic, a wildcard one
/// too (`_retained_topics`): a second sensor on the same wildcard topic
/// brings the retained reading again, which the first doesn't take (its
/// `force_update` would write it again).
#[tokio::test(start_paused = true)]
async fn a_wildcard_subscription_takes_a_retained_message_once() {
    let broker = Broker::start(true).await;
    broker.publish(
        "homeassistant/sensor/a/config",
        r#"{"stat_t": "w/+", "uniq_id": "a1", "name": "A", "frc_upd": true}"#,
        true,
        0,
    );
    broker.publish("w/x", "5", true, 0);
    let r = start(
        &broker,
        json!({"broker": "127.0.0.1", "port": 1883, "protocol": "5"}),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let id = EntityId::parse("sensor.a").unwrap();
    let first = r.hass.states().get(&id).unwrap();
    assert_eq!(first.state, "5");
    broker.publish(
        "homeassistant/sensor/b/config",
        r#"{"stat_t": "w/+", "uniq_id": "b1", "name": "B"}"#,
        true,
        0,
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    let id_b = EntityId::parse("sensor.b").unwrap();
    assert_eq!(r.hass.states().get(&id_b).unwrap().state, "5");
    let after = r.hass.states().get(&id).unwrap();
    assert_eq!(after.last_updated, first.last_updated);
}

/// `mqtt.publish` publishes to the topic as `cv.string` made it: a number
/// is its text.
#[tokio::test(start_paused = true)]
async fn a_numeric_topic_is_published_as_its_text() {
    let broker = Broker::start(true).await;
    let r = start(
        &broker,
        json!({"broker": "127.0.0.1", "port": 1883, "protocol": "5", "discovery": false}),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    broker.take();
    publish_call(&r, json!({"topic": 123, "payload": "x"}))
        .await
        .unwrap()
        .unwrap();
    let sent = broker.take();
    assert_eq!(sent[0].1["topic"], "123");
}

/// A message reaches a wildcard subscription only if one of its
/// subscription identifiers is that subscription's, however many it has
/// (`_matching_subscriptions` checks every one): nine identifiers, none of
/// them `homeassistant/sensor/+/config`'s, don't make a discovery.
#[tokio::test(start_paused = true)]
async fn every_subscription_identifier_is_checked() {
    let broker = Broker::start(true).await;
    let r = start(
        &broker,
        json!({"broker": "127.0.0.1", "port": 1883, "protocol": "5"}),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let ids: Vec<u32> = (100..109).collect();
    broker.state.lock().deliver(
        0,
        "homeassistant/sensor/x/config",
        br#"{"stat_t": "x/state", "uniq_id": "x1", "name": "X"}"#,
        0,
        false,
        &ids,
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        r.hass
            .states()
            .get(&EntityId::parse("sensor.x").unwrap())
            .is_none()
    );
    // With its own among them, it does.
    let mut ids = ids;
    ids.push(24);
    broker.state.lock().deliver(
        0,
        "homeassistant/sensor/x/config",
        br#"{"stat_t": "x/state", "uniq_id": "x1", "name": "X"}"#,
        0,
        false,
        &ids,
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        r.hass
            .states()
            .get(&EntityId::parse("sensor.x").unwrap())
            .is_some()
    );
}

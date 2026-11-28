//! The MQTT client (`client.py` on paho-mqtt 2.1.0) and what it feeds:
//! discovery (`discovery.py`) and the discovered entities (`entity.py`).
//!
//! One task per entry owns everything: the connection, HA's
//! subscriptions, the discovered entities and their timers. It sleeps on
//! the socket, the `mqtt.publish` channel and its next deadline; nothing
//! wakes it otherwise.
//!
//! What matches HA:
//! - **Connecting.** A clean session (clean start on MQTT 5) with the
//!   entry's client id (else a random 22-character one, kept for the
//!   client's life), keepalive, username and password, and the will. A
//!   connection lost is tried again 10 s later, then every 10 s
//!   (`RECONNECT_INTERVAL_SECONDS`); a first connection that fails is
//!   tried again at once, then every 10 s. A broker that refuses the
//!   credentials (or authorisation) is not tried again. While
//!   disconnected every entity is unavailable.
//! - **Subscriptions.** HA's, with its debounce (0.5 s until the birth
//!   message, 0.1 s after; 0.1 s for unsubscribes): a wildcard filter one
//!   `SUBSCRIBE` each, with its MQTT 5 subscription identifier (from 2,
//!   in the order subscribed, reused when released), the others together
//!   (500 at most) with identifier 1; on reconnect every topic again (the
//!   plain ones sorted), at its highest QoS. A subscription takes a
//!   retained message once per topic; a message reaches a wildcard
//!   subscription only if it carries that subscription's identifier.
//!   Packet ids count as paho's (every publish, subscribe and unsubscribe
//!   takes one, QoS 0 too).
//! - **The birth message**, once HA has started and discovery has been
//!   quiet for 5 s (`DISCOVERY_COOLDOWN`), after the debounced
//!   subscriptions; again after each reconnect.
//! - **Discovery**: the 64 subscriptions of `<prefix>/<component>/+/config`,
//!   `…/+/+/config` and `<prefix>/device/…`, in HA's order; new, changed,
//!   unchanged and empty payloads as `async_process_discovery_payload`.
//! - **`mqtt.publish`** ([`register_publish`]): HA's schema and errors;
//!   QoS 1 and 2 wait for the broker's acknowledgement (10 s at most,
//!   then a warning, as HA).
//!
//! Deliberate differences: HA's integration-discovery subscriptions
//! (`tasmota/discovery/#`, `dsmr/#`, … from other integrations'
//! manifests) aren't made, so packet ids and subscription identifiers
//! after the discovery subscriptions differ from HA's by those; a
//! `PINGREQ` goes out after `keepalive` seconds in which nothing was sent
//! or nothing received (paho sends one every `keepalive` seconds whatever
//! else was sent: its send time moves only on a ping), so `hars` pings
//! less, never later than the broker needs; the subscriptions of one
//! debounce go out without waiting for each `SUBACK` (HA waits for each
//! wildcard one before the next); only UTF-8 payloads are decoded (an entity's other
//! `encoding` is read as UTF-8).

use super::discovery::{self, Dropped, Found};
use super::entity::{self, Domain, Mode, Platform};
use super::packet::{self, Incoming, Version};
use super::{Config, Message};
use crate::components::Components;
use crate::config_entries::{EntryState, Status};
use ha_core::template::{Mode as TplMode, Render, Var, Vars};
use ha_core::{CoreState, EntityId, Hass, ServiceError, SetupGuard};
use serde_json::{Map, Value, json};
use smol_str::SmolStr;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep_until, timeout};

/// `MQTT_TIMEOUT`: a connection attempt's time.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const INITIAL_SUBSCRIBE_COOLDOWN: Duration = Duration::from_millis(500);
pub const SUBSCRIBE_COOLDOWN: Duration = Duration::from_millis(100);
pub const UNSUBSCRIBE_COOLDOWN: Duration = Duration::from_millis(100);
pub const TIMEOUT_ACK: Duration = Duration::from_secs(10);
pub const RECONNECT_INTERVAL: Duration = Duration::from_secs(10);
pub const DISCOVERY_COOLDOWN: Duration = Duration::from_secs(5);
const MAX_SUBSCRIBES_PER_CALL: usize = 500;
/// `MAX_LENGTH_STATE_STATE`.
const MAX_STATE: usize = 255;
/// QoS 2 messages a broker hasn't released yet, held at most this many
/// bytes (paho holds any number); past it the broker is in breach of the
/// protocol and the connection is dropped.
pub const INCOMING_BYTES: usize = 1 << 20;
/// `mqtt.publish` calls waiting for the client: a caller past these waits
/// its turn.
const COMMANDS: usize = 64;

/// A connection to a broker: TCP in `hars`, an in-memory pipe in tests.
pub trait Stream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Stream for T {}

/// How a connection is opened.
pub type Dial = Arc<
    dyn Fn(String, u16) -> Pin<Box<dyn Future<Output = std::io::Result<Box<dyn Stream>>> + Send>>
        + Send
        + Sync,
>;

/// Plain TCP, as paho opens it (no Nagle: paho sets `TCP_NODELAY`).
pub fn tcp() -> Dial {
    Arc::new(|host, port| {
        Box::pin(async move {
            let stream = TcpStream::connect((host.as_str(), port)).await?;
            let _ = stream.set_nodelay(true);
            Ok(Box::new(stream) as Box<dyn Stream>)
        })
    })
}

/// A request to the client.
#[derive(Debug)]
pub enum Command {
    Publish {
        topic: String,
        payload: Vec<u8>,
        qos: u8,
        retain: bool,
        expiry: Option<u32>,
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
}

/// `try_connection` with MQTT 5: the broker takes a v5 `CONNECT` (no will,
/// keepalive 60) within 5 s. The connection is closed with a
/// `DISCONNECT`.
pub async fn try_v5(config: &Config, dial: &Dial) -> bool {
    let attempt = async {
        let mut stream = dial(config.broker.clone(), config.port).await?;
        let client_id = config
            .client_id
            .clone()
            .unwrap_or_else(super::random_client_id);
        let connect = packet::connect(&packet::Connect {
            version: Version::V5,
            client_id: &client_id,
            keepalive: 60,
            clean: true,
            will: None,
            username: config.username.as_deref(),
            password: config.password.as_deref(),
        });
        stream.write_all(&connect).await?;
        let mut buf = Vec::with_capacity(64);
        loop {
            if let Ok(Some((first, body))) = packet::frame(&buf) {
                let ok = matches!(
                    packet::parse(first, &buf[body], Version::V5),
                    Ok(Incoming::ConnAck { code: 0, .. })
                );
                if ok {
                    let _ = stream.write_all(&packet::DISCONNECT).await;
                }
                return Ok::<bool, std::io::Error>(ok);
            }
            let mut chunk = [0u8; 256];
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Ok(false);
            }
            buf.extend_from_slice(&chunk[..n]);
        }
    };
    matches!(timeout(CONNECT_TIMEOUT, attempt).await, Ok(Ok(true)))
}

/// Who a subscription is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    Discovery,
    State(usize),
    Availability(usize, usize),
    Attributes(usize),
}

/// One of HA's subscriptions (`Subscription`).
#[derive(Debug)]
struct Sub {
    topic: SmolStr,
    qos: u8,
    wildcard: bool,
    /// MQTT 5's subscription identifier: 1 for a plain topic.
    id: u32,
    owner: Owner,
    /// `_retained_topics`: a plain subscription takes a retained message
    /// once (until the next connection).
    got_retained: bool,
    /// And a wildcard one once per topic.
    retained_topics: HashSet<SmolStr>,
}

/// `SubscriptionID`: identifiers for wildcard filters, from 2.
#[derive(Default)]
struct SubscriptionIds {
    next: u32,
    free: Vec<u32>,
    by_topic: HashMap<SmolStr, u32>,
}

impl SubscriptionIds {
    fn get_or_generate(&mut self, topic: &SmolStr) -> u32 {
        if let Some(id) = self.by_topic.get(topic) {
            return *id;
        }
        let id = match self.free.pop() {
            Some(id) => id,
            None => {
                if self.next < 2 {
                    self.next = 2;
                }
                self.next += 1;
                self.next - 1
            }
        };
        self.by_topic.insert(topic.clone(), id);
        id
    }

    fn release(&mut self, topic: &str) {
        if let Some(id) = self.by_topic.remove(topic) {
            self.free.push(id);
        }
    }
}

/// A sensor's or binary sensor's value.
#[derive(Debug, Clone, PartialEq)]
enum Native {
    Sensor(Option<String>),
    Binary(Option<bool>),
}

/// A discovered entity that runs.
struct Ent {
    cfg: entity::Config,
    hash: (String, String),
    /// The discovery topic it came on, and its payload then.
    topic: String,
    payload: Map<String, Value>,
    entity_id: EntityId,
    registered: bool,
    /// `Entity.name`, and the friendly name it gives.
    name: Option<String>,
    friendly: Option<String>,
    native: Native,
    available: Vec<bool>,
    latest: bool,
    attrs: Option<Map<String, Value>>,
    /// `None`: no `expire_after`.
    expired: Option<bool>,
    expire_timer: Option<TimerKey>,
    off_timer: Option<TimerKey>,
    subs: Vec<usize>,
    /// The unit shown, and the one to convert a temperature to.
    unit: Option<String>,
    convert_to: Option<&'static str>,
    /// The icon shown: the user's (the row's), else the config's.
    icon: Option<String>,
}

type TimerKey = (Instant, u64);

/// What an entity is shown as: its id, whether it has a registry row, its
/// friendly name, unit (and a temperature's conversion) and icon.
struct Shown {
    entity_id: EntityId,
    registered: bool,
    friendly: Option<String>,
    unit: Option<String>,
    convert_to: Option<&'static str>,
    icon: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum Timer {
    Expire(usize),
    OffDelay(usize),
}

/// Where an outgoing QoS 1 or 2 message is (paho's `mqtt_ms_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Published while disconnected: sent at the next connection.
    Unsent,
    WaitPubAck,
    WaitPubRec,
    WaitPubComp,
}

/// A QoS 1 or 2 message until the broker has it (paho's
/// `_out_messages`): kept across a reconnect and sent again then.
struct Out {
    mid: u16,
    message: Message,
    expiry: Option<u32>,
    stage: Stage,
    /// Sent before: a resend is marked `dup`.
    dup: bool,
    /// The action waiting for it, and when HA stops waiting
    /// (`TIMEOUT_ACK`: a warning, and the action succeeds).
    reply: Option<oneshot::Sender<Result<(), ServiceError>>>,
    deadline: Option<Instant>,
}

/// The connection.
enum Link {
    Down,
    Up {
        read: ReadHalf<Box<dyn Stream>>,
        write: WriteHalf<Box<dyn Stream>>,
        buf: Vec<u8>,
    },
}

/// The birth message's progress after a connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Birth {
    None,
    /// Waiting for HA to start.
    WaitStart,
    /// Waiting for discovery to be quiet.
    Cooldown,
}

pub(crate) struct Client {
    dial: Dial,
    hass: Hass,
    components: Arc<Components>,
    entry_id: String,
    pref_disable_new: bool,
    config: Arc<Config>,
    client_id: String,
    link: Link,
    /// `CONNACK` taken: `MQTT.connected`.
    connected: bool,
    identifiers: bool,
    /// Don't try again (the broker refused the credentials).
    give_up: bool,
    reconnect_at: Option<Instant>,
    last_out: Instant,
    last_in: Instant,
    ping_out: bool,
    mid: u16,
    outbound: Vec<Out>,
    /// QoS 2 messages waiting for their `PUBREL`.
    incoming: HashMap<u16, (String, Vec<u8>, bool, packet::Ids)>,
    /// Their topics' and payloads' bytes.
    incoming_bytes: usize,
    subs: Vec<Option<Sub>>,
    free_subs: Vec<usize>,
    plain: HashMap<SmolStr, Vec<usize>>,
    wildcards: Vec<usize>,
    ids: SubscriptionIds,
    max_qos: HashMap<SmolStr, u8>,
    pending: Vec<(SmolStr, u8)>,
    unsubscribing: Vec<SmolStr>,
    subscribe_at: Option<Instant>,
    unsubscribe_at: Option<Instant>,
    cooldown: Duration,
    last_subscribe: Instant,
    last_discovery: Instant,
    birth: Birth,
    birth_at: Option<Instant>,
    /// The discovered entities, and those discovered disabled.
    ents: Vec<Option<Ent>>,
    free_ents: Vec<usize>,
    discovered: HashMap<(String, String), usize>,
    /// `discovery_discovered_and_disabled`: hash to (domain, unique id).
    disabled: HashMap<(String, String), (Domain, String)>,
    /// Components with a discovery `hars` doesn't run, said once each.
    unsupported: HashSet<String>,
    timers: BTreeMap<TimerKey, Timer>,
    timer_seq: u64,
    writes: Vec<usize>,
    /// A message's subscriptions' owners: kept between messages.
    scratch: Vec<Owner>,
}

/// Run the client for an entry, forever.
pub(crate) async fn run(
    hass: Hass,
    components: Arc<Components>,
    config: Arc<Config>,
    dial: Dial,
    ready: SetupGuard,
    status: Status,
) {
    let (tx, rx) = mpsc::channel(COMMANDS);
    register_publish(&hass, tx);
    let mut client = Client::new(hass, components, config, dial);
    client.start_discovery();
    // `async_setup_entry` waits for the first try only: the reconnect loop
    // runs on its own.
    client.connect().await;
    status.set(EntryState::Loaded, None);
    ready.done();
    if client.reconnect_at.is_some() && !client.give_up {
        // `_reconnect_loop`'s first pass: at once.
        client.connect().await;
    }
    client.serve(rx).await;
}

impl Client {
    pub(crate) fn new(
        hass: Hass,
        components: Arc<Components>,
        config: Arc<Config>,
        dial: Dial,
    ) -> Client {
        let now = Instant::now();
        Client {
            dial,
            client_id: config
                .client_id
                .clone()
                .unwrap_or_else(super::random_client_id),
            hass,
            components,
            entry_id: config.entry_id.clone(),
            pref_disable_new: config.pref_disable_new,
            config,
            link: Link::Down,
            connected: false,
            identifiers: false,
            give_up: false,
            reconnect_at: None,
            last_out: now,
            last_in: now,
            ping_out: false,
            mid: 0,
            outbound: Vec::new(),
            incoming: HashMap::new(),
            incoming_bytes: 0,
            subs: Vec::new(),
            free_subs: Vec::new(),
            plain: HashMap::new(),
            wildcards: Vec::new(),
            ids: SubscriptionIds::default(),
            max_qos: HashMap::new(),
            pending: Vec::new(),
            unsubscribing: Vec::new(),
            subscribe_at: None,
            unsubscribe_at: None,
            cooldown: INITIAL_SUBSCRIBE_COOLDOWN,
            last_subscribe: now,
            last_discovery: now,
            birth: Birth::None,
            birth_at: None,
            ents: Vec::new(),
            free_ents: Vec::new(),
            discovered: HashMap::new(),
            disabled: HashMap::new(),
            unsupported: HashSet::new(),
            timers: BTreeMap::new(),
            timer_seq: 0,
            writes: Vec::new(),
            scratch: Vec::new(),
        }
    }

    fn version(&self) -> Version {
        self.config.version
    }

    /// paho's `_mid_generate`, past the ids of messages still in flight
    /// (after a wrap, an id could otherwise be taken twice).
    fn next_mid(&mut self) -> u16 {
        loop {
            self.mid = if self.mid == 65535 { 1 } else { self.mid + 1 };
            if !self.outbound.iter().any(|o| o.mid == self.mid) {
                return self.mid;
            }
        }
    }

    // ------------------------------------------------------------ the wire

    /// Write `bytes`, waiting at most the keepalive (60 s without one) for
    /// a broker that doesn't read: then the connection is given up, as
    /// paho's keepalive check gives it up.
    async fn send(&mut self, bytes: &[u8]) -> bool {
        let limit = Duration::from_secs(match self.config.keepalive {
            0 => 60,
            k => u64::from(k),
        });
        let Link::Up { write, .. } = &mut self.link else {
            return false;
        };
        match timeout(limit, write.write_all(bytes)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                self.lost().await;
                return false;
            }
            Err(_) => {
                tracing::warn!("Error returned from MQTT server: The connection was lost.");
                self.lost().await;
                return false;
            }
        }
        self.last_out = Instant::now();
        true
    }

    /// `connect`/`reconnect`: open the socket and send `CONNECT`. The
    /// `CONNACK` comes through [`Client::serve`].
    async fn connect(&mut self) {
        self.reconnect_at = None;
        let config = self.config.clone();
        let attempt = timeout(
            CONNECT_TIMEOUT,
            (self.dial)(config.broker.clone(), config.port),
        )
        .await;
        let stream = match attempt {
            Ok(Ok(stream)) => stream,
            Ok(Err(e)) => {
                tracing::error!("Failed to connect to MQTT server due to exception: {e}");
                self.reconnect_at = Some(Instant::now() + RECONNECT_INTERVAL);
                return;
            }
            Err(_) => {
                tracing::error!("Failed to connect to MQTT server due to exception: timed out");
                self.reconnect_at = Some(Instant::now() + RECONNECT_INTERVAL);
                return;
            }
        };
        let (read, write) = tokio::io::split(stream);
        self.link = Link::Up {
            read,
            write,
            buf: Vec::new(),
        };
        let will = config.will.as_ref().map(|w| packet::Will {
            topic: &w.topic,
            payload: &w.payload,
            qos: w.qos,
            retain: w.retain,
        });
        let connect = packet::connect(&packet::Connect {
            version: config.version,
            client_id: &self.client_id,
            keepalive: config.keepalive,
            clean: true,
            will,
            username: config.username.as_deref(),
            password: config.password.as_deref(),
        });
        let now = Instant::now();
        self.last_in = now;
        self.ping_out = false;
        if self.send(&connect).await {
            self.last_out = now;
        }
    }

    /// The connection is gone (`_async_on_socket_close`,
    /// `_async_mqtt_on_disconnect`).
    async fn lost(&mut self) {
        self.link = Link::Down;
        self.birth = Birth::None;
        self.birth_at = None;
        self.subscribe_at = None;
        self.ping_out = false;
        // A clean session: QoS 2 messages waiting for their release go.
        self.incoming.clear();
        self.incoming_bytes = 0;
        if !self.give_up && self.reconnect_at.is_none() {
            self.reconnect_at = Some(Instant::now() + RECONNECT_INTERVAL);
        }
        // Waiting publishes keep their timeout, as HA's futures do.
        if self.connected {
            self.connected = false;
            tracing::debug!(
                "Disconnected from MQTT server {}:{} (Unspecified error)",
                self.config.broker,
                self.config.port
            );
            if !matches!(
                self.hass.core_state(),
                CoreState::Stopping | CoreState::FinalWrite | CoreState::Stopped
            ) {
                self.write_all_entities();
            }
        }
    }

    /// The next thing to do.
    fn deadline(&self) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        let mut at = |t: Option<Instant>| {
            if let Some(t) = t {
                next = Some(next.map_or(t, |n| n.min(t)));
            }
        };
        at(self.reconnect_at.filter(|_| !self.give_up));
        if matches!(self.link, Link::Up { .. }) && self.config.keepalive > 0 {
            let k = Duration::from_secs(u64::from(self.config.keepalive));
            at(Some(self.last_out.min(self.last_in) + k));
        }
        at(self.subscribe_at);
        at(self.unsubscribe_at);
        at(self.birth_at);
        at(self.timers.keys().next().map(|k| k.0));
        at(self.outbound.iter().filter_map(|o| o.deadline).min());
        next
    }

    /// Sleep on the socket, the commands and the next deadline.
    async fn serve(&mut self, mut rx: mpsc::Receiver<Command>) {
        let hass = self.hass.clone();
        loop {
            let deadline = self.deadline();
            let wait_start = self.birth == Birth::WaitStart;
            let mut chunk = [0u8; 4096];
            let read = async {
                match &mut self.link {
                    Link::Up { read, .. } => read.read(&mut chunk).await,
                    Link::Down => std::future::pending().await,
                }
            };
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(cmd) => self.command(cmd).await,
                    None => return,
                },
                n = read => {
                    match n {
                        Ok(0) | Err(_) => {
                            tracing::warn!(
                                "Error returned from MQTT server: The connection was lost."
                            );
                            self.lost().await;
                        }
                        Ok(n) => {
                            if let Link::Up { buf, .. } = &mut self.link {
                                buf.extend_from_slice(&chunk[..n]);
                            }
                            self.read_packets().await;
                        }
                    }
                }
                () = hass.wait_started(), if wait_start => {
                    self.birth = Birth::Cooldown;
                    self.reset_cooldown();
                }
                () = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                    self.due().await;
                }
            }
        }
    }

    async fn read_packets(&mut self) {
        loop {
            let Link::Up { buf, .. } = &mut self.link else {
                return;
            };
            let mut taken = std::mem::take(buf);
            let framed = packet::frame(&taken);
            let (first, body) = match framed {
                Ok(Some(f)) => f,
                Ok(None) => {
                    if let Link::Up { buf, .. } = &mut self.link {
                        *buf = taken;
                    }
                    return;
                }
                Err(e) => {
                    tracing::warn!("Error returned from MQTT server: {e}");
                    self.lost().await;
                    return;
                }
            };
            let end = body.end;
            self.last_in = Instant::now();
            let parsed = packet::parse(first, &taken[body.clone()], self.version());
            match parsed {
                Ok(p) => self.packet(p).await,
                Err(e) => {
                    tracing::warn!("Error returned from MQTT server: {e}");
                    self.lost().await;
                    return;
                }
            }
            taken.drain(..end);
            if let Link::Up { buf, .. } = &mut self.link {
                // Anything read meanwhile came after.
                taken.extend_from_slice(buf);
                // Don't hold a large message's buffer while idle.
                if taken.is_empty() && taken.capacity() > 64 * 1024 {
                    taken = Vec::new();
                }
                *buf = taken;
            } else {
                return;
            }
        }
    }

    async fn packet(&mut self, p: Incoming<'_>) {
        match p {
            Incoming::ConnAck {
                code,
                no_identifiers,
            } => self.connack(code, no_identifiers).await,
            Incoming::Publish(p) => {
                let topic = p.topic;
                match p.qos {
                    0 => {
                        self.message(topic, p.payload, p.retain, &p.identifiers)
                            .await
                    }
                    1 => {
                        self.message(topic, p.payload, p.retain, &p.identifiers)
                            .await;
                        if let Some(id) = p.packet_id {
                            self.send(&packet::ack(packet::PUBACK, id)).await;
                        }
                    }
                    _ => {
                        // Delivered at its `PUBREL`, as paho does.
                        if let Some(id) = p.packet_id {
                            if let Ok(t) = std::str::from_utf8(topic) {
                                let size = t.len() + p.payload.len();
                                if let Some((t, b, ..)) = self.incoming.insert(
                                    id,
                                    (t.to_owned(), p.payload.to_vec(), p.retain, p.identifiers),
                                ) {
                                    self.incoming_bytes -= t.len() + b.len();
                                }
                                self.incoming_bytes += size;
                                if self.incoming_bytes > INCOMING_BYTES {
                                    tracing::warn!(
                                        "Error returned from MQTT server: A network protocol \
                                         error occurred when communicating with the broker."
                                    );
                                    self.lost().await;
                                    return;
                                }
                            }
                            self.send(&packet::ack(packet::PUBREC, id)).await;
                        }
                    }
                }
            }
            Incoming::PubRel(id) => {
                if let Some((topic, payload, retain, ids)) = self.incoming.remove(&id) {
                    self.incoming_bytes -= topic.len() + payload.len();
                    self.message(topic.as_bytes(), &payload, retain, &ids).await;
                }
                self.send(&packet::ack(packet::PUBCOMP, id)).await;
            }
            Incoming::PubAck(id) => self.acked(id, Stage::WaitPubAck),
            Incoming::PubComp(id) => self.acked(id, Stage::WaitPubComp),
            Incoming::PubRec(id) => {
                if let Some(o) = self
                    .outbound
                    .iter_mut()
                    .find(|o| o.mid == id && o.stage == Stage::WaitPubRec)
                {
                    o.stage = Stage::WaitPubComp;
                }
                self.send(&packet::ack(packet::PUBREL, id)).await;
            }
            Incoming::SubAck(_) | Incoming::UnsubAck(_) => {}
            Incoming::PingResp => self.ping_out = false,
            Incoming::Disconnect => self.lost().await,
        }
    }

    /// The broker has message `id`, if it is at `stage` (a `PUBACK` for a
    /// QoS 2 message, or a `PUBCOMP` before its `PUBREC`, isn't one).
    fn acked(&mut self, id: u16, stage: Stage) {
        let Some(i) = self
            .outbound
            .iter()
            .position(|o| o.mid == id && o.stage == stage)
        else {
            return;
        };
        if let Some(reply) = self.outbound.remove(i).reply {
            let _ = reply.send(Ok(()));
        }
    }

    /// `_messages_reconnect_reset_out` and `_handle_connack`: on a clean
    /// session every QoS 1 and 2 message the broker hasn't got is
    /// published again, marked `dup` if it was sent before.
    async fn resend(&mut self) {
        let v = self.version();
        for i in 0..self.outbound.len() {
            let o = &mut self.outbound[i];
            o.dup |= o.stage != Stage::Unsent;
            o.stage = match o.message.qos {
                1 => Stage::WaitPubAck,
                _ => Stage::WaitPubRec,
            };
            let bytes = packet::publish(
                v,
                &packet::Out {
                    topic: &o.message.topic,
                    payload: &o.message.payload,
                    qos: o.message.qos,
                    retain: o.message.retain,
                    dup: o.dup,
                    packet_id: o.mid,
                    expiry: o.expiry,
                },
            );
            if !self.send(&bytes).await {
                return;
            }
        }
    }

    /// `_async_mqtt_on_connect`.
    async fn connack(&mut self, code: u8, no_identifiers: bool) {
        let v5 = self.version().is_v5();
        if v5 && no_identifiers {
            tracing::warn!(
                "Your MQTT broker reports it does not support Subscription Identifiers, see \
                 https://docs.oasis-open.org/mqtt/mqtt/v5.0/os/mqtt-v5.0-os.html#_Toc3901092. \
                 Please use a supported MQTT broker"
            );
        }
        self.identifiers = v5 && !no_identifiers;
        if code != 0 {
            let (name, auth) = refusal(code, v5);
            if auth {
                self.give_up = true;
            }
            tracing::error!("Unable to connect to the MQTT broker: {name}");
            self.link = Link::Down;
            if !self.give_up {
                self.reconnect_at = Some(Instant::now() + RECONNECT_INTERVAL);
            }
            return;
        }
        self.connected = true;
        tracing::debug!(
            "Connected to MQTT server {}:{} (Success)",
            self.config.broker,
            self.config.port
        );
        self.resend().await;
        self.write_all_entities();
        self.queue_resubscribe();
        self.schedule_subscribe();
        if self.config.birth.is_some() {
            self.birth = match self.hass.core_state() {
                CoreState::Running => {
                    self.reset_cooldown();
                    Birth::Cooldown
                }
                _ => Birth::WaitStart,
            };
        }
    }

    /// `_discovery_cooldown`'s start: 5 s from now, moved by discovery
    /// and subscriptions.
    fn reset_cooldown(&mut self) {
        let now = Instant::now();
        self.last_discovery = now;
        self.last_subscribe = now;
        self.birth_at = Some(now + DISCOVERY_COOLDOWN);
    }

    async fn due(&mut self) {
        let now = Instant::now();
        if self.reconnect_at.is_some_and(|t| t <= now) && !self.give_up {
            self.reconnect_at = None;
            if !self.connected {
                self.connect().await;
            }
            if self.reconnect_at.is_none() && !matches!(self.link, Link::Up { .. }) {
                self.reconnect_at = Some(now + RECONNECT_INTERVAL);
            }
        }
        if matches!(self.link, Link::Up { .. }) && self.config.keepalive > 0 {
            let k = Duration::from_secs(u64::from(self.config.keepalive));
            if self.last_out.min(self.last_in) + k <= now {
                if self.connected && !self.ping_out {
                    if self.send(&packet::PINGREQ).await {
                        self.ping_out = true;
                        self.last_out = now;
                        self.last_in = now;
                    }
                } else {
                    tracing::warn!("Error returned from MQTT server: The connection was lost.");
                    self.lost().await;
                }
            }
        }
        if self.unsubscribe_at.is_some_and(|t| t <= now) {
            self.unsubscribe_at = None;
            self.perform_unsubscribes().await;
        }
        if self.subscribe_at.is_some_and(|t| t <= now) {
            self.subscribe_at = None;
            self.perform_subscriptions().await;
        }
        if self.birth == Birth::Cooldown && self.birth_at.is_some_and(|t| t <= now) {
            // Moved on by discovery or subscriptions since?
            let pending = !self.pending.is_empty();
            let last = self
                .last_discovery
                .max(if pending { now } else { self.last_subscribe });
            if last + DISCOVERY_COOLDOWN > now {
                self.birth_at = Some(last + DISCOVERY_COOLDOWN);
            } else {
                self.birth_at = None;
                self.birth = Birth::None;
                self.subscribe_at = None;
                self.perform_subscriptions().await;
                self.cooldown = SUBSCRIBE_COOLDOWN;
                if let Some(birth) = self.config.birth.clone() {
                    let _ = self.publish(&birth, None, None).await;
                    tracing::info!("MQTT client initialized, birth message sent");
                }
            }
        }
        while let Some((&key, &timer)) = self.timers.iter().next() {
            if key.0 > now {
                break;
            }
            self.timers.remove(&key);
            self.timer(timer, key);
        }
        self.flush_writes();
        // `_async_wait_for_mid_or_raise`: HA stops waiting (the message
        // stays with paho until the broker has it).
        for o in &mut self.outbound {
            if o.deadline.is_some_and(|d| d <= now) {
                o.deadline = None;
                tracing::warn!(
                    "No ACK from MQTT server in {} seconds (mid: {})",
                    TIMEOUT_ACK.as_secs(),
                    o.mid
                );
                if let Some(reply) = o.reply.take() {
                    let _ = reply.send(Ok(()));
                }
            }
        }
    }

    // ------------------------------------------------------------ publishing

    async fn command(&mut self, cmd: Command) {
        match cmd {
            Command::Publish {
                topic,
                payload,
                qos,
                retain,
                expiry,
                reply,
            } => {
                if expiry.is_some() && !self.version().is_v5() {
                    let mut p = Map::new();
                    p.insert("topic".into(), json!(topic));
                    p.insert("protocol".into(), json!(self.version().as_str()));
                    let message = format!(
                        "Publishing to topic {topic} with a Message Expiry Interval is not \
                         supported for protocol version {}",
                        self.version().as_str()
                    );
                    let _ = reply.send(Err(ServiceError::validation(
                        super::DOMAIN,
                        "mqtt_message_expiry_interval_not_supported",
                        Some(p),
                        message,
                    )));
                    return;
                }
                let m = Message {
                    topic,
                    payload,
                    qos,
                    retain,
                };
                if let Err(e) = self.publish(&m, expiry, Some(reply)).await {
                    tracing::debug!("{e}");
                }
            }
        }
    }

    /// `MQTT.async_publish`. With `reply`, the answer goes there once the
    /// broker has it (QoS 0: once sent).
    async fn publish(
        &mut self,
        m: &Message,
        expiry: Option<u32>,
        reply: Option<oneshot::Sender<Result<(), ServiceError>>>,
    ) -> Result<(), String> {
        let mid = self.next_mid();
        tracing::debug!(
            "Transmitting{} message on {}: '{}', mid: {mid}, qos: {}",
            if m.retain { " retained" } else { "" },
            m.topic,
            String::from_utf8_lossy(&m.payload),
            m.qos
        );
        if !self.connected {
            // paho keeps a QoS 1 or 2 message to send at the next
            // connection, and answers that it isn't connected.
            if m.qos > 0 {
                self.outbound.push(Out {
                    mid,
                    message: m.clone(),
                    expiry,
                    stage: Stage::Unsent,
                    dup: false,
                    reply: None,
                    deadline: None,
                });
            }
            let message = "Error talking to MQTT: The client is not currently connected".to_owned();
            if let Some(reply) = reply {
                let mut p = Map::new();
                p.insert(
                    "error_message".into(),
                    json!("The client is not currently connected."),
                );
                let _ = reply.send(Err(ServiceError::translated(
                    super::DOMAIN,
                    "mqtt_broker_error",
                    Some(p),
                    message.clone(),
                )));
            }
            return Err(message);
        }
        let bytes = packet::publish(
            self.version(),
            &packet::Out {
                topic: &m.topic,
                payload: &m.payload,
                qos: m.qos,
                retain: m.retain,
                dup: false,
                packet_id: mid,
                expiry,
            },
        );
        if m.qos == 0 {
            self.send(&bytes).await;
            if let Some(reply) = reply {
                let _ = reply.send(Ok(()));
            }
            return Ok(());
        }
        // Kept before it is sent: a connection lost on the way sends it
        // again.
        self.outbound.push(Out {
            mid,
            message: m.clone(),
            expiry,
            stage: match m.qos {
                1 => Stage::WaitPubAck,
                _ => Stage::WaitPubRec,
            },
            dup: false,
            deadline: reply.as_ref().map(|_| Instant::now() + TIMEOUT_ACK),
            reply,
        });
        self.send(&bytes).await;
        Ok(())
    }

    // ------------------------------------------------------------ subscriptions

    /// `async_subscribe`: track it, and ask the broker if connected.
    fn subscribe(&mut self, topic: &str, qos: u8, owner: Owner) -> usize {
        let topic = SmolStr::new(topic);
        let wildcard = topic.contains(['+', '#']);
        let id = if wildcard {
            self.ids.get_or_generate(&topic)
        } else {
            1
        };
        let sub = Sub {
            topic: topic.clone(),
            qos,
            wildcard,
            id,
            owner,
            got_retained: false,
            retained_topics: HashSet::new(),
        };
        let i = match self.free_subs.pop() {
            Some(i) => {
                self.subs[i] = Some(sub);
                i
            }
            None => {
                self.subs.push(Some(sub));
                self.subs.len() - 1
            }
        };
        if wildcard {
            self.wildcards.push(i);
        } else {
            self.plain.entry(topic.clone()).or_default().push(i);
        }
        if self.connected {
            self.queue(&[(topic, qos)]);
            self.schedule_subscribe();
        }
        i
    }

    /// `_async_queue_subscriptions`.
    fn queue(&mut self, topics: &[(SmolStr, u8)]) {
        for (topic, qos) in topics {
            let max = self.max_qos.entry(topic.clone()).or_insert(0);
            if *max < *qos {
                *max = *qos;
            }
            let max = *max;
            match self.pending.iter_mut().find(|(t, _)| t == topic) {
                Some(p) => p.1 = max,
                None => self.pending.push((topic.clone(), max)),
            }
            self.unsubscribing.retain(|t| t != topic);
        }
    }

    /// `EnsureJobAfterCooldown.async_schedule`.
    fn schedule_subscribe(&mut self) {
        let at = Instant::now() + self.cooldown;
        self.subscribe_at = Some(self.subscribe_at.map_or(at, |t| t.max(at)));
    }

    /// `_async_queue_resubscribe`: every topic again, at its highest QoS,
    /// sorted.
    fn queue_resubscribe(&mut self) {
        self.max_qos.clear();
        for sub in self.subs.iter_mut().flatten() {
            sub.got_retained = false;
            sub.retained_topics = HashSet::new();
        }
        let mut topics: Vec<(SmolStr, u8)> = Vec::new();
        for sub in self.subs.iter().flatten() {
            match topics.iter_mut().find(|(t, _)| *t == sub.topic) {
                Some(t) => t.1 = t.1.max(sub.qos),
                None => topics.push((sub.topic.clone(), sub.qos)),
            }
        }
        topics.sort_by(|a, b| a.0.cmp(&b.0));
        self.queue(&topics);
    }

    /// `_async_perform_subscriptions`.
    async fn perform_subscriptions(&mut self) {
        if self.pending.is_empty() || !self.connected {
            return;
        }
        let mut pending = std::mem::take(&mut self.pending);
        // The wildcard ones first, one by one, in the order subscribed.
        let mut wild: Vec<(SmolStr, u8, u32)> = Vec::new();
        for &i in &self.wildcards {
            let Some(sub) = &self.subs[i] else { continue };
            if let Some(at) = pending.iter().position(|(t, _)| *t == sub.topic) {
                let (t, q) = pending.remove(at);
                wild.push((t, q, sub.id));
            }
        }
        let v = self.version();
        for (topic, qos, id) in wild {
            let mid = self.next_mid();
            let bytes = packet::subscribe(
                v,
                mid,
                &[(topic.as_str(), qos)],
                self.identifiers.then_some(id),
            );
            tracing::debug!("Subscribing with mid: {mid} to topic {topic} with qos: {qos}");
            self.last_subscribe = Instant::now();
            if !self.send(&bytes).await {
                return;
            }
        }
        for chunk in pending.chunks(MAX_SUBSCRIBES_PER_CALL) {
            let mid = self.next_mid();
            let topics: Vec<(&str, u8)> = chunk.iter().map(|(t, q)| (t.as_str(), *q)).collect();
            let bytes = packet::subscribe(v, mid, &topics, self.identifiers.then_some(1));
            tracing::debug!("Subscribing with mid: {mid} to topics with qos: {topics:?}");
            self.last_subscribe = Instant::now();
            if !self.send(&bytes).await {
                return;
            }
        }
    }

    /// `_async_remove` and `_async_unsubscribe`.
    fn unsubscribe(&mut self, i: usize) {
        let Some(sub) = self.subs[i].take() else {
            return;
        };
        self.free_subs.push(i);
        if sub.wildcard {
            self.wildcards.retain(|&w| w != i);
        } else if let Some(list) = self.plain.get_mut(&sub.topic) {
            list.retain(|&p| p != i);
            if list.is_empty() {
                self.plain.remove(&sub.topic);
            }
        }
        if !self.connected {
            return;
        }
        let still: Vec<u8> = self
            .subs
            .iter()
            .flatten()
            .filter(|s| s.topic == sub.topic)
            .map(|s| s.qos)
            .collect();
        if !still.is_empty() {
            if self.max_qos.get(&sub.topic).copied().unwrap_or(0) == 0 {
                return;
            }
            self.max_qos
                .insert(sub.topic.clone(), still.into_iter().max().unwrap_or(0));
            return;
        }
        self.max_qos.remove(&sub.topic);
        self.pending.retain(|(t, _)| *t != sub.topic);
        if !self.unsubscribing.contains(&sub.topic) {
            self.unsubscribing.push(sub.topic.clone());
        }
        let at = Instant::now() + UNSUBSCRIBE_COOLDOWN;
        self.unsubscribe_at = Some(self.unsubscribe_at.map_or(at, |t| t.max(at)));
    }

    /// `_async_perform_unsubscribes`.
    async fn perform_unsubscribes(&mut self) {
        if self.unsubscribing.is_empty() {
            return;
        }
        let topics = std::mem::take(&mut self.unsubscribing);
        let v = self.version();
        for chunk in topics.chunks(MAX_SUBSCRIBES_PER_CALL) {
            let mid = self.next_mid();
            let list: Vec<&str> = chunk.iter().map(SmolStr::as_str).collect();
            let bytes = packet::unsubscribe(v, mid, &list);
            if !self.send(&bytes).await {
                break;
            }
        }
        for t in &topics {
            self.ids.release(t);
        }
    }

    // ------------------------------------------------------------ messages

    /// `_async_mqtt_on_message`: every matching subscription's callback,
    /// then the entities' state writes.
    async fn message(&mut self, topic: &[u8], payload: &[u8], retain: bool, ids: &packet::Ids) {
        let Ok(topic) = std::str::from_utf8(topic) else {
            tracing::warn!(
                "Skipping received{} message on invalid topic {:?}",
                if retain { " retained" } else { "" },
                topic
            );
            return;
        };
        let filter_ids = self.identifiers && !ids.overflow && !ids.as_slice().is_empty();
        let ids = ids.as_slice();
        // The owners of the subscriptions it reaches, in a buffer kept
        // between messages (`_matching_subscriptions`). A retained message
        // reaches a subscription once per topic (`_retained_topics`).
        let mut owners = std::mem::take(&mut self.scratch);
        owners.clear();
        let take = |sub: &mut Sub| -> bool {
            if !retain {
                return true;
            }
            if !sub.wildcard {
                return !std::mem::replace(&mut sub.got_retained, true);
            }
            sub.retained_topics.insert(SmolStr::new(topic))
        };
        if let Some(list) = self.plain.get(topic)
            && !(filter_ids && !ids.contains(&1))
        {
            for &i in list {
                if let Some(sub) = &mut self.subs[i]
                    && take(sub)
                {
                    owners.push(sub.owner);
                }
            }
        }
        for &i in &self.wildcards {
            if let Some(sub) = &mut self.subs[i]
                && matches(&sub.topic, topic)
                && !(filter_ids && !ids.contains(&sub.id))
                && take(sub)
            {
                owners.push(sub.owner);
            }
        }
        if owners.is_empty() {
            self.scratch = owners;
            return;
        }
        let Ok(text) = std::str::from_utf8(payload) else {
            tracing::warn!(
                "Can't decode payload {:?} on {topic} with encoding utf-8",
                String::from_utf8_lossy(&payload[..payload.len().min(8192)])
            );
            self.scratch = owners;
            return;
        };
        // Discovery first: it doesn't share topics with entities.
        let mut entities = false;
        for k in 0..owners.len() {
            match owners[k] {
                Owner::Discovery => self.discovery_message(topic, text),
                _ => entities = true,
            }
        }
        if entities {
            owners.retain(|o| *o != Owner::Discovery);
            self.entity_messages(topic, payload, text, &owners).await;
        }
        self.scratch = owners;
        self.flush_writes();
    }

    /// Every entity callback for one message: templates rendered together
    /// on a render thread, then applied in order.
    async fn entity_messages(&mut self, topic: &str, raw: &[u8], text: &str, owners: &[Owner]) {
        // No template: nothing to render, nothing to set up for it.
        let templated = |o: &Owner| match *o {
            Owner::State(e) => self.ent(e).is_some_and(|e| e.cfg.value_template.is_some()),
            Owner::Availability(e, a) => self
                .ent(e)
                .and_then(|e| e.cfg.availability.get(a))
                .is_some_and(|a| a.template.is_some()),
            Owner::Attributes(e) => self
                .ent(e)
                .is_some_and(|e| e.cfg.attributes_template.is_some()),
            Owner::Discovery => false,
        };
        if !owners.iter().any(templated) {
            for owner in owners {
                self.callback(*owner, topic, raw, text, None);
            }
            return;
        }
        let mut jobs: Vec<(Arc<ha_core::template::Template>, Vars)> = Vec::new();
        let mut slot: Vec<Option<usize>> = Vec::with_capacity(owners.len());
        let mut value_json: Option<Option<Var>> = None;
        for owner in owners {
            let template = match *owner {
                Owner::State(e) => self.ent(e).and_then(|e| e.cfg.value_template.clone()),
                Owner::Availability(e, a) => self
                    .ent(e)
                    .and_then(|e| e.cfg.availability.get(a))
                    .and_then(|a| a.template.clone()),
                Owner::Attributes(e) => self.ent(e).and_then(|e| e.cfg.attributes_template.clone()),
                Owner::Discovery => None,
            };
            let Some(template) = template else {
                slot.push(None);
                continue;
            };
            let e = match *owner {
                Owner::State(e) | Owner::Availability(e, _) | Owner::Attributes(e) => e,
                Owner::Discovery => unreachable!("handled above"),
            };
            let json = value_json
                .get_or_insert_with(|| {
                    serde_json::from_str::<Value>(text)
                        .ok()
                        .map(|v| Var::json(&v))
                })
                .clone();
            let Some(ent) = self.ent(e) else {
                slot.push(None);
                continue;
            };
            let mut vars = Vars::new();
            vars.insert("entity_id".into(), Var::str(ent.entity_id.as_str()));
            vars.insert(
                "name".into(),
                ent.name.as_deref().map_or_else(Var::none, Var::str),
            );
            vars.insert(
                "this".into(),
                Var::opt_state(self.hass.states().get(&ent.entity_id).as_ref()),
            );
            vars.insert("value".into(), Var::str(text));
            if let Some(json) = json {
                vars.insert("value_json".into(), json);
            }
            slot.push(Some(jobs.len()));
            jobs.push((template, vars));
        }
        let mut rendered: Vec<Option<Render>> = Vec::new();
        if !jobs.is_empty() {
            let home = ha_core::template::Home::new(self.hass.clone());
            let out = ha_core::template::run::run(move || {
                jobs.into_iter()
                    .map(|(t, vars)| t.render_vars(Some(&home), &vars, TplMode::Lenient, false))
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            rendered = out.into_iter().map(Some).collect();
        }
        for (k, owner) in owners.iter().enumerate() {
            let render = match slot[k] {
                None => None,
                Some(j) => match rendered.get_mut(j).and_then(Option::take) {
                    Some(r) => Some(r),
                    // No render thread could be had: the message isn't
                    // taken as it is in the template's place.
                    None => {
                        tracing::error!("Could not render the template for a message on {topic}");
                        continue;
                    }
                },
            };
            self.callback(*owner, topic, raw, text, render);
        }
    }

    fn ent(&self, i: usize) -> Option<&Ent> {
        self.ents.get(i).and_then(Option::as_ref)
    }

    /// `available`.
    fn available(&self, e: &Ent) -> bool {
        if !self.connected
            && !matches!(
                self.hass.core_state(),
                CoreState::Stopping | CoreState::FinalWrite | CoreState::Stopped
            )
        {
            return false;
        }
        let topics = e.available.is_empty()
            || match e.cfg.mode {
                Mode::All => e.available.iter().all(|a| *a),
                Mode::Any => e.available.iter().any(|a| *a),
                Mode::Latest => e.latest,
            };
        topics && e.expired != Some(true)
    }

    /// One callback (`_message_callback`): the message, then a state
    /// write if what the callback tracks changed.
    fn callback(
        &mut self,
        owner: Owner,
        topic: &str,
        raw: &[u8],
        text: &str,
        render: Option<Render>,
    ) {
        let e = match owner {
            Owner::State(e) | Owner::Availability(e, _) | Owner::Attributes(e) => e,
            Owner::Discovery => return,
        };
        let Some(ent) = self.ents.get(e).and_then(Option::as_ref) else {
            return;
        };
        let force = ent.cfg.force_update;
        let default = ent.cfg.domain == Domain::Sensor && matches!(owner, Owner::State(_));
        // A state message makes an `expire_after` value fresh before its
        // template is rendered.
        let mut changed = matches!(owner, Owner::State(_)) && self.fresh(e);
        let Some(ent) = self.ents.get(e).and_then(Option::as_ref) else {
            return;
        };
        // `Some(None)`: the message is ignored (what the callback tracks
        // is still compared); `None`: an error, logged, and no write.
        let value: Option<Option<Cow<'_, str>>> = match render {
            None => Some(Some(Cow::Borrowed(text))),
            Some(r) => {
                for (_, line) in &r.log {
                    tracing::warn!("{line}");
                }
                let source = match owner {
                    Owner::State(_) => ent.cfg.value_template.as_ref(),
                    Owner::Availability(_, a) => ent
                        .cfg
                        .availability
                        .get(a)
                        .and_then(|a| a.template.as_ref()),
                    Owner::Attributes(_) => ent.cfg.attributes_template.as_ref(),
                    Owner::Discovery => None,
                }
                .map_or("", |t| t.source());
                match r.result {
                    Ok(out) => Some(Some(Cow::Owned(
                        out.as_text().unwrap_or_default().to_owned(),
                    ))),
                    Err(err) if jinja_error(&err.0) => match default {
                        // `PayloadSentinel.DEFAULT`: the message is
                        // ignored.
                        true => Some(None),
                        false => {
                            tracing::error!(
                                "Error parsing value: {} (value: {text}, template: {source})",
                                err.0.split_once(": ").map_or(err.0.as_str(), |(_, m)| m)
                            );
                            Some(Some(Cow::Borrowed(text)))
                        }
                    },
                    Err(err) => {
                        tracing::warn!(
                            "{} rendering template for entity '{}', template: \
                             '{source}'{} and payload: {text}",
                            err.0,
                            ent.entity_id,
                            if default {
                                ", default value: default"
                            } else {
                                ""
                            }
                        );
                        None
                    }
                }
            }
        };
        let Some(value) = value else {
            return;
        };
        changed |= match (owner, value) {
            (Owner::State(_), Some(value)) => self.on_state(e, topic, raw, &value),
            (Owner::Availability(_, a), Some(value)) => self.on_availability(e, a, &value),
            (Owner::Attributes(_), Some(value)) => self.on_attributes(e, &value),
            _ => false,
        };
        if changed || force {
            self.writes.retain(|&w| w != e);
            self.writes.push(e);
        }
    }

    /// A timer `after` from now; `None` for one past what the clock can
    /// hold, which never comes due (`cv.positive_int` has no bound).
    fn set_timer(&mut self, after: Duration, timer: Timer) -> Option<TimerKey> {
        let at = Instant::now().checked_add(after)?;
        self.timer_seq += 1;
        let key = (at, self.timer_seq);
        self.timers.insert(key, timer);
        Some(key)
    }

    fn timer(&mut self, timer: Timer, key: TimerKey) {
        match timer {
            Timer::Expire(e) => {
                if let Some(Some(ent)) = self.ents.get_mut(e)
                    && ent.expire_timer == Some(key)
                {
                    ent.expire_timer = None;
                    ent.expired = Some(true);
                    self.writes.push(e);
                }
            }
            Timer::OffDelay(e) => {
                if let Some(Some(ent)) = self.ents.get_mut(e)
                    && ent.off_timer == Some(key)
                {
                    ent.off_timer = None;
                    ent.native = Native::Binary(Some(false));
                    self.writes.push(e);
                }
            }
        }
    }

    /// `expire_after`: a state message makes the value fresh, and its
    /// timer starts again. True if it had expired.
    fn fresh(&mut self, e: usize) -> bool {
        let Some(ent) = self.ents.get(e).and_then(Option::as_ref) else {
            return false;
        };
        let Some(after) = ent.cfg.expire_after.filter(|a| *a > 0) else {
            return false;
        };
        if let Some(k) = ent.expire_timer {
            self.timers.remove(&k);
        }
        let key = self.set_timer(Duration::from_secs(after), Timer::Expire(e));
        let Some(Some(ent)) = self.ents.get_mut(e) else {
            return false;
        };
        ent.expire_timer = key;
        ent.expired.replace(false) != Some(false)
    }

    /// A state message's value (`_update_state`, `_state_message_received`).
    /// True if the value changed.
    fn on_state(&mut self, e: usize, topic: &str, raw: &[u8], value: &str) -> bool {
        let Some(Some(ent)) = self.ents.get_mut(e) else {
            return false;
        };
        // The sensor's value, set without a copy when it is the same.
        fn put(native: &mut Native, value: Option<&str>) -> bool {
            if matches!(native, Native::Sensor(old) if old.as_deref() == value) {
                return false;
            }
            *native = Native::Sensor(value.map(str::to_owned));
            true
        }
        match &ent.cfg.platform {
            Platform::Sensor {
                device_class,
                state_class,
                unit,
                precision,
                options,
            } => {
                if value == "None" {
                    put(&mut ent.native, None)
                } else if entity::numeric_expected(
                    device_class.as_deref(),
                    state_class.as_deref(),
                    unit.as_deref(),
                    *precision,
                ) {
                    if value.is_empty() {
                        tracing::debug!("Ignore empty state from '{topic}'");
                        false
                    } else {
                        put(&mut ent.native, Some(value))
                    }
                } else if let Some(o) = options
                    && !o.iter().any(|x| x == value)
                {
                    tracing::warn!(
                        "Ignoring invalid option received on topic '{topic}', got '{value}', \
                         allowed: {}",
                        o.join(", ")
                    );
                    false
                } else if matches!(device_class.as_deref(), None | Some("enum")) {
                    if value.chars().count() > MAX_STATE {
                        tracing::warn!(
                            "Cannot update state for entity {} after processing payload on \
                             topic {topic}. The requested state ({}) exceeds the maximum \
                             allowed length ({MAX_STATE}). Fall back to unknown, failed \
                             state: {}",
                            ent.entity_id,
                            value.chars().count(),
                            value.chars().take(8192).collect::<String>()
                        );
                        tracing::warn!("Invalid state message '{value}' from '{topic}'");
                        put(&mut ent.native, None)
                    } else {
                        put(&mut ent.native, Some(value))
                    }
                } else {
                    // `timestamp` and `date`: `dt_util.parse_datetime`,
                    // then a date in the payload's own offset, or the time
                    // in UTC.
                    let date = device_class.as_deref() == Some("date");
                    match parse_datetime(value) {
                        Some((local, _)) if date => {
                            put(&mut ent.native, Some(&local.date().to_string()))
                        }
                        Some((local, Some(offset))) => {
                            let utc = offset
                                .to_timestamp(local)
                                .map(|t| t.strftime("%Y-%m-%dT%H:%M:%S+00:00").to_string());
                            match utc {
                                Ok(utc) => put(&mut ent.native, Some(&utc)),
                                Err(_) => put(&mut ent.native, None),
                            }
                        }
                        Some((_, None)) => {
                            tracing::error!(
                                "Invalid datetime: {} provides state '{value}', which is \
                                 missing timezone information",
                                ent.entity_id
                            );
                            false
                        }
                        None => {
                            tracing::warn!("Invalid state message '{value}' from '{topic}'");
                            put(&mut ent.native, None)
                        }
                    }
                }
            }
            Platform::BinarySensor {
                payload_on,
                payload_off,
                off_delay,
                ..
            } => {
                let payload = ha_core::py::strip(value);
                if payload.is_empty() {
                    tracing::debug!(
                        "Empty template output for entity: {} with state topic: {topic}. \
                         Payload: '{}'",
                        ent.entity_id,
                        String::from_utf8_lossy(raw)
                    );
                    return false;
                }
                let on = if value == payload_on {
                    Some(true)
                } else if value == payload_off {
                    Some(false)
                } else if value == "None" {
                    None
                } else {
                    tracing::info!(
                        "No matching payload found for entity: {} with state topic: {topic}. \
                         Payload: '{}'{}",
                        ent.entity_id,
                        String::from_utf8_lossy(raw),
                        match &ent.cfg.value_template {
                            Some(t) => format!(
                                ", template output: '{value}', with value template '{}'",
                                t.source()
                            ),
                            None => String::new(),
                        }
                    );
                    return false;
                };
                let changed = ent.native != Native::Binary(on);
                ent.native = Native::Binary(on);
                let off_delay = *off_delay;
                let old = ent.off_timer.take();
                if let Some(k) = old {
                    self.timers.remove(&k);
                }
                if on == Some(true)
                    && let Some(delay) = off_delay
                {
                    let key = self.set_timer(Duration::from_secs(delay), Timer::OffDelay(e));
                    if let Some(Some(ent)) = self.ents.get_mut(e) {
                        ent.off_timer = key;
                    }
                }
                changed
            }
        }
    }

    fn on_availability(&mut self, e: usize, a: usize, value: &str) -> bool {
        let Some(ent) = self.ent(e) else {
            return false;
        };
        let before = self.available(ent);
        let Some(Some(ent)) = self.ents.get_mut(e) else {
            return false;
        };
        let Some(conf) = ent.cfg.availability.get(a) else {
            return false;
        };
        if value == conf.available {
            ent.available[a] = true;
            ent.latest = true;
        } else if value == conf.not_available {
            ent.available[a] = false;
            ent.latest = false;
        }
        let Some(ent) = self.ent(e) else {
            return false;
        };
        before != self.available(ent)
    }

    fn on_attributes(&mut self, e: usize, value: &str) -> bool {
        let Some(Some(ent)) = self.ents.get_mut(e) else {
            return false;
        };
        match serde_json::from_str::<Value>(value) {
            Err(_) => {
                tracing::warn!("Erroneous JSON: {value}");
                false
            }
            Ok(Value::Object(m)) => {
                let domain = ent.cfg.domain;
                let filtered: Map<String, Value> = m
                    .into_iter()
                    .filter(|(k, _)| !entity::attribute_blocked(domain, k))
                    .collect();
                let changed = ent.attrs.as_ref() != Some(&filtered);
                ent.attrs = Some(filtered);
                changed
            }
            Ok(_) => {
                tracing::warn!("JSON result was not a dictionary");
                false
            }
        }
    }

    /// `process_write_state_requests`: each entity once, the last asked
    /// first (`popitem`).
    fn flush_writes(&mut self) {
        while let Some(e) = self.writes.pop() {
            self.write(e);
        }
    }

    fn write_all_entities(&mut self) {
        for e in 0..self.ents.len() {
            if self.ents[e].is_some() {
                self.write(e);
            }
        }
    }

    /// `async_write_ha_state`.
    fn write(&self, e: usize) {
        let Some(ent) = self.ent(e) else {
            return;
        };
        let available = self.available(ent);
        let state = match (available, &ent.native) {
            (false, _) => Ok("unavailable".to_owned()),
            (true, Native::Binary(None)) | (true, Native::Sensor(None)) => Ok("unknown".to_owned()),
            (true, Native::Binary(Some(true))) => Ok("on".to_owned()),
            (true, Native::Binary(Some(false))) => Ok("off".to_owned()),
            (true, Native::Sensor(Some(v))) => sensor_state(ent, v),
        };
        let state = match state {
            Ok(s) => s,
            Err(err) => {
                tracing::error!(
                    "Value error while updating state of {}, topic: '{}': {err}",
                    ent.entity_id,
                    ent.cfg.state_topic
                );
                return;
            }
        };
        let mut a = ent.cfg.capabilities().unwrap_or_default();
        if available && let Some(attrs) = &ent.attrs {
            for (k, v) in attrs {
                a.insert(k.clone(), v.clone());
            }
        }
        if let Some(unit) = &ent.unit {
            a.insert("unit_of_measurement".into(), json!(unit));
        }
        if let Some(dc) = ent.cfg.device_class() {
            a.insert("device_class".into(), json!(dc));
        }
        if let Some(p) = &ent.cfg.entity_picture {
            a.insert("entity_picture".into(), json!(p));
        }
        if let Some(icon) = &ent.icon {
            a.insert("icon".into(), json!(icon));
        }
        if let Some(f) = ent.friendly.as_ref().filter(|f| !f.is_empty()) {
            a.insert("friendly_name".into(), json!(f));
        }
        match ent.cfg.force_update {
            true => {
                self.hass
                    .set_state_forced(ent.entity_id.clone(), state, a, None);
            }
            false => {
                self.hass
                    .set_state_with_context(ent.entity_id.clone(), state, a, None);
            }
        }
    }

    // ------------------------------------------------------------ discovery

    /// `discovery.async_start`'s subscriptions.
    fn start_discovery(&mut self) {
        if !self.config.discovery {
            return;
        }
        let prefix = self.config.prefix.clone();
        let qos = self.config.discovery_qos;
        for c in super::tables::SUPPORTED_COMPONENTS {
            self.subscribe(&format!("{prefix}/{c}/+/config"), qos, Owner::Discovery);
        }
        for c in super::tables::SUPPORTED_COMPONENTS {
            self.subscribe(&format!("{prefix}/{c}/+/+/config"), qos, Owner::Discovery);
        }
        self.subscribe(&format!("{prefix}/device/+/config"), qos, Owner::Discovery);
        self.subscribe(
            &format!("{prefix}/device/+/+/config"),
            qos,
            Owner::Discovery,
        );
        self.last_discovery = Instant::now();
    }

    /// `async_discovery_message_received`.
    fn discovery_message(&mut self, topic: &str, payload: &str) {
        self.last_discovery = Instant::now();
        let parsed = match discovery::parse_topic(&self.config.prefix, topic) {
            Ok(Some(t)) => t,
            Ok(None) => return,
            Err(discovery::IllegalTopic) => {
                tracing::warn!(
                    "Received message on illegal discovery topic '{topic}'. The topic contains \
                     non allowed characters. For more information see \
                     https://www.home-assistant.io/integrations/mqtt/#discovery-topic"
                );
                return;
            }
        };
        let found = if parsed.component == "device" {
            let already: Vec<(String, String)> = self.discovered.keys().cloned().collect();
            discovery::device(
                parsed.node_id.as_deref(),
                &parsed.object_id,
                payload,
                already.iter().map(|(c, i)| (c.as_str(), i.as_str())),
            )
        } else {
            discovery::component(
                &parsed.component,
                parsed.node_id.as_deref(),
                &parsed.object_id,
                payload,
            )
            .map(|f| vec![f])
        };
        match found {
            Ok(found) => {
                for f in found {
                    self.process(f, topic);
                }
            }
            Err(Dropped::Warn(w)) => tracing::warn!("{w}"),
            Err(Dropped::Quiet) => {}
        }
    }

    /// `async_process_discovery_payload`.
    fn process(&mut self, f: Found, topic: &str) {
        let hash = (f.component.clone(), f.discovery_id.clone());
        let Some(domain) = Domain::of(&f.component) else {
            if !f.payload.is_empty() && self.unsupported.insert(f.component.clone()) {
                tracing::info!(
                    "hars does not run MQTT discovered {} entities yet: {} {}",
                    f.component,
                    f.component,
                    f.discovery_id
                );
            }
            return;
        };
        if let Some(&e) = self.discovered.get(&hash) {
            self.update(e, f, topic);
            return;
        }
        if f.payload.is_empty() {
            // A disabled entity's row and device go.
            if let Some((domain, unique_id)) = self.disabled.remove(&hash)
                && let Some(registry) = self.components.registry()
                && let Some(row) =
                    registry.entity_by_unique_id(domain.as_str(), super::DOMAIN, &unique_id)
            {
                registry.remove_entity(&row.entity_id);
                self.cleanup_device(row.device_id.as_deref());
            }
            return;
        }
        tracing::debug!("Found new component: {} {}", f.component, f.discovery_id);
        let cfg = match entity::Config::parse(domain, &f.payload) {
            Ok(cfg) => cfg,
            Err(err) => {
                let message = Value::Object(f.payload.clone());
                tracing::error!(
                    "Error '{err}' when processing MQTT discovery message topic: '{topic}', \
                     message: '{message}'"
                );
                return;
            }
        };
        self.add(cfg, hash, topic, f.payload);
    }

    /// The entity is new: its registry rows, its subscriptions, its first
    /// state (`_async_add_entity`, `async_added_to_hass`).
    fn add(
        &mut self,
        cfg: entity::Config,
        hash: (String, String),
        topic: &str,
        payload: Map<String, Value>,
    ) {
        if cfg.device.as_ref().is_some_and(|d| d.name.is_none()) {
            let got = Value::Object(payload.clone());
            tracing::info!(
                "MQTT device information always needs to include a name, got {got}, if device \
                 information is shared between multiple entities, the device name must be \
                 included in each entity's device configuration"
            );
        }
        // A unique id another running entity has: HA's platform refuses it.
        if let (Some(unique_id), Some(registry)) = (&cfg.unique_id, self.components.registry())
            && let Some(row) =
                registry.entity_by_unique_id(cfg.domain.as_str(), super::DOMAIN, unique_id)
            && self
                .ents
                .iter()
                .flatten()
                .any(|x| x.entity_id.as_str() == row.entity_id)
        {
            tracing::error!(
                "Platform mqtt does not generate unique IDs. ID {unique_id} already exists - \
                 ignoring {}",
                row.entity_id
            );
            return;
        }
        let name = cfg.entity_name();
        let Some(Shown {
            entity_id,
            registered,
            friendly,
            unit,
            convert_to,
            icon,
        }) = self.register(&cfg, &name, &hash)
        else {
            return;
        };
        if cfg.domain == Domain::Sensor && cfg.entity_category.as_deref() == Some("config") {
            tracing::error!(
                "Entity {entity_id} cannot be added as the entity category is set to config"
            );
            return;
        }
        let native = match cfg.domain {
            Domain::Sensor => Native::Sensor(None),
            Domain::BinarySensor => Native::Binary(None),
        };
        let expired = cfg.expire_after.filter(|a| *a > 0).map(|_| true);
        let ent = Ent {
            available: vec![false; cfg.availability.len()],
            latest: false,
            hash: hash.clone(),
            topic: topic.to_owned(),
            payload,
            entity_id,
            registered,
            name,
            friendly,
            native,
            attrs: None,
            expired,
            expire_timer: None,
            off_timer: None,
            subs: Vec::new(),
            unit,
            convert_to,
            icon,
            cfg,
        };
        let e = match self.free_ents.pop() {
            Some(e) => {
                self.ents[e] = Some(ent);
                e
            }
            None => {
                self.ents.push(Some(ent));
                self.ents.len() - 1
            }
        };
        self.discovered.insert(hash, e);
        self.subscribe_entity(e);
        self.write(e);
    }

    /// An entity's subscriptions, in HA's order: availability, JSON
    /// attributes, state.
    fn subscribe_entity(&mut self, e: usize) {
        let Some(ent) = self.ent(e) else { return };
        let qos = ent.cfg.qos;
        let mut wanted: Vec<(String, Owner)> = Vec::new();
        for (a, av) in ent.cfg.availability.iter().enumerate() {
            wanted.push((av.topic.clone(), Owner::Availability(e, a)));
        }
        if let Some(t) = &ent.cfg.attributes_topic {
            wanted.push((t.clone(), Owner::Attributes(e)));
        }
        wanted.push((ent.cfg.state_topic.clone(), Owner::State(e)));
        let mut subs = Vec::with_capacity(wanted.len());
        for (topic, owner) in wanted {
            subs.push(self.subscribe(&topic, qos, owner));
        }
        if let Some(Some(ent)) = self.ents.get_mut(e) {
            ent.subs = subs;
        }
    }

    /// A message for a discovered entity: removed, changed or the same.
    fn update(&mut self, e: usize, f: Found, topic: &str) {
        let Some(ent) = self.ent(e) else { return };
        if ent.topic != topic {
            tracing::warn!(
                "Received a conflicting MQTT discovery message for entity {}; the entity was \
                 previously discovered on topic {}; the conflicting discovery message was \
                 received on topic {topic}; for documentation on migration to device schema or \
                 rollback to discovery schema, visit https://www.home-assistant.io/integrations/\
                 mqtt/#migration-from-single-component-to-device-based-discovery",
                ent.entity_id,
                ent.topic
            );
            return;
        }
        if f.payload.is_empty() {
            tracing::info!("Removing component: {}", ent.entity_id);
            self.remove(e);
            return;
        }
        if ent.payload == f.payload {
            tracing::debug!("Ignoring unchanged update for: {}", ent.entity_id);
            return;
        }
        tracing::info!("Updating component: {}", ent.entity_id);
        let cfg = match entity::Config::parse(ent.cfg.domain, &f.payload) {
            Ok(cfg) => cfg,
            Err(err) => {
                let message = Value::Object(f.payload.clone());
                tracing::error!(
                    "Error '{err}' when processing MQTT discovery message topic: '{topic}', \
                     message: '{message}'"
                );
                return;
            }
        };
        let name = cfg.entity_name();
        let hash = ent.hash.clone();
        let entity_id = ent.entity_id.clone();
        let registered = ent.registered;
        let old_subs = ent.subs.clone();
        // The device (`device_info_discovery_update`) and the row's names
        // and capabilities (what a state write syncs).
        let shown = self.refresh_rows(&cfg, &name, &entity_id, registered);
        let Some(Some(ent)) = self.ents.get_mut(e) else {
            return;
        };
        // `_availability_prepare_subscribe_topics`: a topic kept keeps
        // what it said.
        let mut available = vec![false; cfg.availability.len()];
        for (i, a) in cfg.availability.iter().enumerate() {
            if let Some(j) = ent.cfg.availability.iter().position(|o| o.topic == a.topic) {
                available[i] = ent.available[j];
            }
        }
        ent.available = available;
        ent.expired = cfg.expire_after.filter(|a| *a > 0).map(|_| true);
        ent.payload = f.payload;
        ent.name = name;
        ent.friendly = shown.friendly;
        ent.unit = shown.unit;
        ent.convert_to = shown.convert_to;
        ent.icon = shown.icon;
        ent.cfg = cfg;
        let _ = hash;
        // Subscriptions: those whose topic and QoS stay are kept.
        let kept: Vec<usize> = old_subs;
        let mut wanted: Vec<(String, Owner)> = Vec::new();
        for (a, av) in ent.cfg.availability.iter().enumerate() {
            wanted.push((av.topic.clone(), Owner::Availability(e, a)));
        }
        if let Some(t) = &ent.cfg.attributes_topic {
            wanted.push((t.clone(), Owner::Attributes(e)));
        }
        wanted.push((ent.cfg.state_topic.clone(), Owner::State(e)));
        let qos = ent.cfg.qos;
        let mut subs = Vec::new();
        let mut unused: Vec<usize> = kept;
        for (topic, owner) in wanted {
            let same = unused.iter().position(|&s| {
                self.subs[s].as_ref().is_some_and(|x| {
                    x.topic == topic.as_str() && x.qos == qos && same_role(x.owner, owner)
                })
            });
            match same {
                Some(at) => {
                    let s = unused.remove(at);
                    if let Some(sub) = &mut self.subs[s] {
                        sub.owner = owner;
                    }
                    subs.push(s);
                }
                None => subs.push(self.subscribe(&topic, qos, owner)),
            }
        }
        for s in unused {
            self.unsubscribe(s);
        }
        if let Some(Some(ent)) = self.ents.get_mut(e) {
            ent.subs = subs;
        }
        self.write(e);
    }

    /// The entity goes, with its state, its row and (if nothing else of it
    /// is left) its device's link to the entry.
    fn remove(&mut self, e: usize) {
        let Some(ent) = self.ents.get_mut(e).and_then(Option::take) else {
            return;
        };
        self.free_ents.push(e);
        self.discovered.remove(&ent.hash);
        for k in [ent.expire_timer, ent.off_timer].into_iter().flatten() {
            self.timers.remove(&k);
        }
        for s in ent.subs.iter().copied() {
            self.unsubscribe(s);
        }
        self.writes.retain(|&w| w != e);
        if ent.registered
            && let Some(registry) = self.components.registry()
        {
            let device = registry
                .snapshot()
                .entities
                .iter()
                .find(|r| r.entity_id == ent.entity_id.as_str())
                .and_then(|r| r.device_id.clone());
            registry.remove_entity(ent.entity_id.as_str());
            self.cleanup_device(device.as_deref());
        }
        self.hass.remove_state(&ent.entity_id);
    }

    /// `async_cleanup_device_registry`: the entry leaves a device with no
    /// enabled entities.
    fn cleanup_device(&self, device_id: Option<&str>) {
        let (Some(device_id), Some(registry)) = (device_id, self.components.registry()) else {
            return;
        };
        let r = registry.snapshot();
        let in_use = r
            .entities
            .iter()
            .any(|row| row.device_id.as_deref() == Some(device_id) && row.disabled_by.is_none());
        if !in_use {
            registry.remove_device_entry(device_id, &self.entry_id);
        }
    }

    /// The registry rows of a new entity, and its id, whether it has a
    /// row, its friendly name and unit. `None`: it doesn't run (its row is
    /// disabled).
    fn register(
        &mut self,
        cfg: &entity::Config,
        name: &Option<String>,
        hash: &(String, String),
    ) -> Option<Shown> {
        let domain = cfg.domain.as_str();
        let registry = self.components.registry().cloned();
        let entry_id = self.entry_id.clone();
        if let (Some(registry), Some(d)) = (&registry, &cfg.device)
            && let Some(via) = &d.via_device
        {
            // `ensure_via_device_exists`.
            let via_id = json!([super::DOMAIN, via]);
            if !registry
                .snapshot()
                .devices
                .iter()
                .any(|x| x.identifiers.contains(&via_id))
            {
                registry.get_or_create_device(ha_registry::DeviceInfo {
                    config_entry_id: entry_id.clone(),
                    identifiers: vec![via_id],
                    ..Default::default()
                });
            }
        }
        // `default_entity_id`: the id it asks for, made free in the state
        // machine (`async_generate_entity_id`).
        let asked = cfg.default_entity_id.as_ref().map(|d| {
            let object_id = d.split_once('.').map_or(d.as_str(), |(_, o)| o);
            let base = format!("{domain}.{}", ha_components::slugify(object_id));
            free_in_states(&self.hass, &base)
        });
        let (unit, convert_to) = display_unit(&self.hass, cfg, None);
        let (Some(unique_id), Some(registry)) = (&cfg.unique_id, registry) else {
            // No unique id: no row. The id from the name.
            let id = match asked {
                Some(id) => id,
                None => {
                    let object_id = name.clone().unwrap_or_else(|| "Unnamed Device".into());
                    let r = self.components.registries();
                    ha_registry::available_entity_id(
                        &r,
                        &self.hass,
                        domain,
                        &ha_components::slugify(&object_id),
                    )
                }
            };
            return Some(Shown {
                entity_id: EntityId::parse(&id).ok()?,
                registered: false,
                friendly: name.clone(),
                unit,
                convert_to,
                icon: cfg.icon.clone(),
            });
        };
        let device = cfg
            .device
            .as_ref()
            .map(|d| registry.get_or_create_device(device_info(&entry_id, &self.config.title, d)));
        let existing = registry.entity_by_unique_id(domain, super::DOMAIN, unique_id);
        if !cfg.enabled_by_default && existing.as_ref().is_none_or(|r| r.disabled_by.is_some()) {
            self.disabled
                .insert(hash.clone(), (cfg.domain, unique_id.clone()));
        }
        let (unit, _) = display_unit(&self.hass, cfg, existing.as_deref());
        let patch = ha_registry::EntityPatch {
            capabilities: Some(cfg.capabilities().map(Value::Object)),
            config_entry_id: Some(Some(entry_id.clone())),
            device_id: Some(device.as_ref().map(|d| d.id.clone())),
            entity_category: Some(cfg.entity_category.clone()),
            has_entity_name: Some(true),
            object_id_base: Some(if asked.is_some() { None } else { name.clone() }),
            suggested_object_id: asked.as_ref().map(|id| {
                Some(
                    id.split_once('.')
                        .map_or(id.as_str(), |(_, o)| o)
                        .to_owned(),
                )
            }),
            original_device_class: Some(cfg.device_class().map(str::to_owned)),
            original_icon: Some(cfg.icon.clone()),
            original_name: Some(name.clone()),
            supported_features: Some(0),
            translation_key: Some(None),
            unit_of_measurement: Some(unit.clone()),
            pref_disable_new: self.pref_disable_new,
            ..Default::default()
        };
        let disabled_by = (!cfg.enabled_by_default).then(|| "integration".to_owned());
        let mut row =
            registry.get_or_create_entity(domain, super::DOMAIN, unique_id, disabled_by, patch);
        if device.as_ref().is_some_and(|d| d.disabled_by.is_some()) && row.disabled_by.is_none() {
            registry.update_entity(
                &row.entity_id,
                ha_registry::EntityPatch {
                    disabled_by: Some(Some("device".into())),
                    ..Default::default()
                },
            );
            row = registry.entity_by_unique_id(domain, super::DOMAIN, unique_id)?;
        }
        if row.disabled_by.is_some() {
            tracing::debug!("Not adding entity {} because it's disabled", row.entity_id);
            return None;
        }
        self.sync_precision(cfg, &row.entity_id);
        let (unit, convert_to) = display_unit(&self.hass, cfg, Some(&row));
        let friendly = friendly_name(&registry.snapshot(), &row, name.as_deref());
        Some(Shown {
            entity_id: EntityId::parse(&row.entity_id).ok()?,
            registered: true,
            friendly,
            unit,
            convert_to,
            icon: row.icon.clone().or_else(|| cfg.icon.clone()),
        })
    }

    /// `_update_suggested_precision`.
    fn sync_precision(&self, cfg: &entity::Config, entity_id: &str) {
        let Platform::Sensor {
            device_class,
            unit,
            precision,
            ..
        } = &cfg.platform
        else {
            return;
        };
        let Some(registry) = self.components.registry() else {
            return;
        };
        let r = registry.snapshot();
        let Some(row) = r.entities.iter().find(|x| x.entity_id == entity_id) else {
            return;
        };
        let p = entity::display_precision(device_class.as_deref(), unit.as_deref(), *precision);
        let options = match p {
            Some(p) => crate::sensor::with_precision(row, p as i64),
            None => {
                let mut options = row.options.object();
                let removed = match options.get_mut("sensor").and_then(Value::as_object_mut) {
                    Some(s) => s.remove("suggested_display_precision").is_some(),
                    None => false,
                };
                removed.then_some(Value::Object(options))
            }
        };
        if let Some(options) = options {
            registry.update_entity(
                entity_id,
                ha_registry::EntityPatch {
                    options: Some(options),
                    ..Default::default()
                },
            );
        }
    }

    /// A changed config's registry work: the device, the row's names and
    /// capabilities. The friendly name and unit after.
    fn refresh_rows(
        &self,
        cfg: &entity::Config,
        name: &Option<String>,
        entity_id: &EntityId,
        registered: bool,
    ) -> Shown {
        let registry = match (registered, self.components.registry()) {
            (true, Some(r)) => r.clone(),
            _ => {
                let (unit, convert_to) = display_unit(&self.hass, cfg, None);
                return Shown {
                    entity_id: entity_id.clone(),
                    registered,
                    friendly: name.clone(),
                    unit,
                    convert_to,
                    icon: cfg.icon.clone(),
                };
            }
        };
        if let Some(d) = &cfg.device {
            if let Some(via) = &d.via_device {
                let via_id = json!([super::DOMAIN, via]);
                if !registry
                    .snapshot()
                    .devices
                    .iter()
                    .any(|x| x.identifiers.contains(&via_id))
                {
                    registry.get_or_create_device(ha_registry::DeviceInfo {
                        config_entry_id: self.entry_id.clone(),
                        identifiers: vec![via_id],
                        ..Default::default()
                    });
                }
            }
            registry.get_or_create_device(device_info(&self.entry_id, &self.config.title, d));
        }
        registry.update_entity(
            entity_id.as_str(),
            ha_registry::EntityPatch {
                capabilities: Some(cfg.capabilities().map(Value::Object)),
                original_device_class: Some(cfg.device_class().map(str::to_owned)),
                original_name: Some(name.clone()),
                supported_features: Some(0),
                ..Default::default()
            },
        );
        let r = registry.snapshot();
        let row = r
            .entities
            .iter()
            .find(|x| x.entity_id == entity_id.as_str());
        let (unit, convert_to) = display_unit(&self.hass, cfg, row.map(|x| &**x));
        let friendly = row.and_then(|row| friendly_name(&r, row, name.as_deref()));
        let icon = row
            .and_then(|x| x.icon.clone())
            .or_else(|| cfg.icon.clone());
        Shown {
            entity_id: entity_id.clone(),
            registered,
            friendly,
            unit,
            convert_to,
            icon,
        }
    }
}

/// The entry's task ends: aborted when the entry is removed, or dropped
/// with the runtime at stop. A removal is HA's `async_unload_entry`: the
/// entities go with their states, and the client disconnects cleanly (a
/// `DISCONNECT`, written on a task of its own, as `Drop` can't wait), so
/// the broker doesn't publish the will. At stop HA leaves both: the
/// states are the last ones, and the will tells the broker it went.
impl Drop for Client {
    fn drop(&mut self) {
        if self.hass.core_state() != CoreState::Running {
            return;
        }
        for ent in self.ents.iter().flatten() {
            self.hass.remove_state(&ent.entity_id);
        }
        if let (Link::Up { mut write, .. }, Ok(rt)) = (
            std::mem::replace(&mut self.link, Link::Down),
            tokio::runtime::Handle::try_current(),
        ) {
            rt.spawn(async move {
                let _ = write.write_all(&packet::DISCONNECT).await;
                let _ = write.shutdown().await;
            });
        }
    }
}

/// Whether two owners are the same kind of subscription for an entity.
fn same_role(a: Owner, b: Owner) -> bool {
    matches!(
        (a, b),
        (Owner::State(_), Owner::State(_))
            | (Owner::Attributes(_), Owner::Attributes(_))
            | (Owner::Availability(..), Owner::Availability(..))
    )
}

/// `async_generate_entity_id(…, current_ids=None, hass)`: `base`, or with
/// `_2`, `_3`, … while the state machine has it.
fn free_in_states(hass: &Hass, base: &str) -> String {
    let taken = |id: &str| EntityId::parse(id).is_ok_and(|id| hass.states().get(&id).is_some());
    if !taken(base) {
        return base.to_owned();
    }
    (2..)
        .map(|n| format!("{base}_{n}"))
        .find(|id| !taken(id))
        .unwrap_or_else(|| base.to_owned())
}

/// `device_info_from_specifications`, as the registry takes it.
fn device_info(entry_id: &str, title: &str, d: &entity::Device) -> ha_registry::DeviceInfo {
    ha_registry::DeviceInfo {
        config_entry_id: entry_id.to_owned(),
        identifiers: d
            .identifiers
            .iter()
            .map(|i| json!([super::DOMAIN, i]))
            .collect(),
        connections: d.connections.iter().map(|(t, v)| json!([t, v])).collect(),
        manufacturer: d.manufacturer.clone(),
        model: d.model.clone(),
        model_id: d.model_id.clone(),
        name: d.name.clone(),
        sw_version: d.sw_version.clone(),
        hw_version: d.hw_version.clone(),
        serial_number: d.serial_number.clone(),
        configuration_url: d.configuration_url.clone(),
        via_device: d.via_device.as_ref().map(|v| json!([super::DOMAIN, v])),
        suggested_area: d.suggested_area.clone(),
        entry_title: Some(title.to_owned()),
        ..Default::default()
    }
}

/// `async_get_full_entity_name`: the user's name, else the device's name
/// and the entity's.
fn friendly_name(
    r: &ha_registry::Registries,
    row: &ha_registry::Entity,
    original: Option<&str>,
) -> Option<String> {
    if let Some(user) = &row.name {
        return Some(user.clone());
    }
    let device = row
        .device_id
        .as_deref()
        .and_then(|id| r.devices.iter().find(|d| d.id == id))
        .and_then(|d| d.display_name());
    let full: Vec<&str> = [device, original]
        .into_iter()
        .flatten()
        .filter(|p| !p.is_empty())
        .collect();
    (!full.is_empty()).then(|| full.join(" "))
}

/// The unit a sensor shows, and what a temperature is converted to: a
/// temperature in °C, °F or K follows the row's options, else the unit
/// system (HA's legacy temperature conversion).
fn display_unit(
    hass: &Hass,
    cfg: &entity::Config,
    row: Option<&ha_registry::Entity>,
) -> (Option<String>, Option<&'static str>) {
    let Platform::Sensor {
        device_class, unit, ..
    } = &cfg.platform
    else {
        return (None, None);
    };
    if device_class.as_deref() == Some("temperature")
        && let Some(native) = unit.as_deref().filter(|u| matches!(*u, "°C" | "°F" | "K"))
    {
        let shown = crate::sensor::state_unit(hass, row);
        let to = (shown != native).then_some(shown);
        return (Some(shown.to_owned()), to);
    }
    (unit.clone(), None)
}

/// The sensor component's `state` for a value: a number checked (and a
/// temperature converted) where one is expected.
fn sensor_state(ent: &Ent, value: &str) -> Result<String, String> {
    let Platform::Sensor {
        device_class,
        state_class,
        unit,
        precision,
        options,
    } = &ent.cfg.platform
    else {
        return Ok(value.to_owned());
    };
    let dc = device_class.as_deref();
    if options.is_some() || dc == Some("enum") {
        if dc != Some("enum") {
            return Err(format!(
                "Sensor {} is providing enum options, but is missing the enum device class",
                ent.entity_id
            ));
        }
        return Ok(value.to_owned());
    }
    if !entity::numeric_expected(dc, state_class.as_deref(), unit.as_deref(), *precision) {
        return Ok(value.to_owned());
    }
    let number = if !value.contains('.') && !value.contains('e') {
        crate::num::py_int_str(value)
            .map(|i| i as f64)
            .or_else(|| crate::num::py_float_str(value))
    } else {
        crate::num::py_float_str(value)
    };
    let py_none = |s: &Option<String>| s.as_deref().map_or("None".to_owned(), |s| s.to_owned());
    let Some(number) = number else {
        return Err(format!(
            "Sensor {} has device class '{}', state class '{}' unit '{}' and suggested \
             precision '{}' thus indicating it has a numeric value; however, it has the \
             non-numeric value: '{value}' (<class 'str'>)",
            ent.entity_id,
            py_none(device_class),
            py_none(state_class),
            py_none(&ent.unit),
            precision.map_or("None".to_owned(), |p| p.to_string()),
        ));
    };
    if !number.is_finite() {
        return Err(format!(
            "Sensor {} has device class '{}', state class '{}' unit '{}' and suggested \
             precision '{}' thus indicating it has a numeric value; however, it has the \
             non-finite value: '{}'",
            ent.entity_id,
            py_none(device_class),
            py_none(state_class),
            py_none(&ent.unit),
            precision.map_or("None".to_owned(), |p| p.to_string()),
            ha_core::py::repr_float(number)
        ));
    }
    if let (Some(to), Some(from)) = (ent.convert_to, unit.as_deref()) {
        let celsius = match from {
            "°F" => (number - 32.0) / 1.8,
            "K" => number - 273.15,
            _ => number,
        };
        return Ok(ha_core::py::repr_float(crate::sensor::from_celsius(
            celsius, to,
        )));
    }
    Ok(value.to_owned())
}

/// `dt_util.parse_datetime`: the time as written, and its offset if it
/// has one.
fn parse_datetime(s: &str) -> Option<(jiff::civil::DateTime, Option<jiff::tz::Offset>)> {
    let pieces = jiff::fmt::temporal::Pieces::parse(s.trim()).ok()?;
    let time = pieces.time().unwrap_or(jiff::civil::Time::midnight());
    Some((
        pieces.date().to_datetime(time),
        pieces.offset().map(|o| o.to_numeric_offset()),
    ))
}

/// Whether a template error is one of Jinja's (`jinja2.TemplateError`),
/// which a value template's caller handles, rather than another
/// exception (`ValueError`, …).
fn jinja_error(message: &str) -> bool {
    let class = message.split_once(':').map_or(message, |(c, _)| c);
    matches!(
        class,
        "UndefinedError"
            | "TemplateSyntaxError"
            | "TemplateAssertionError"
            | "TemplateRuntimeError"
            | "TemplateError"
            | "SecurityError"
    )
}

/// The reason paho names for a refused `CONNACK`, and whether it is one
/// that stops HA trying (bad credentials or not authorised).
fn refusal(code: u8, v5: bool) -> (&'static str, bool) {
    if v5 {
        return match code {
            0x80 => ("Unspecified error", false),
            0x81 => ("Malformed packet", false),
            0x82 => ("Protocol error", false),
            0x83 => ("Implementation specific error", false),
            0x84 => ("Unsupported protocol version", false),
            0x85 => ("Client identifier not valid", false),
            0x86 => ("Bad user name or password", true),
            0x87 => ("Not authorized", true),
            0x88 => ("Server unavailable", false),
            0x89 => ("Server busy", false),
            0x8A => ("Banned", false),
            0x8C => ("Bad authentication method", true),
            0x18 => ("Continue authentication", true),
            0x19 => ("Re-authenticate", true),
            1 => ("Unsupported protocol version", false),
            _ => ("Unspecified error", false),
        };
    }
    match code {
        1 => ("Unsupported protocol version", false),
        2 => ("Client identifier not valid", false),
        3 => ("Server unavailable", false),
        4 => ("Bad user name or password", true),
        5 => ("Not authorized", true),
        _ => ("Unspecified error", false),
    }
}

/// `MQTTMatcher`: whether `topic` matches the filter.
pub fn matches(filter: &str, topic: &str) -> bool {
    let mut f = filter.split('/');
    let mut t = topic.split('/');
    let mut first = true;
    loop {
        match (f.next(), t.next()) {
            (Some("#"), _) => return !(first && topic.starts_with('$')),
            (Some("+"), Some(_)) => {
                if first && topic.starts_with('$') {
                    return false;
                }
            }
            (Some(a), Some(b)) if a == b => {}
            (None, None) => return true,
            _ => return false,
        }
        first = false;
    }
}

/// `mqtt.publish`, registered once: HA's schema
/// (`MQTT_PUBLISH_SCHEMA`), handed to the client.
pub fn register_publish(hass: &Hass, tx: mpsc::Sender<Command>) {
    use ha_core::{Kind, Schema, SupportsResponse};
    let schema = Schema::new()
        .pre_validate(publish_pre_validate)
        .required("topic", Kind::Any)
        .optional("payload", Kind::Any)
        .optional("evaluate_payload", Kind::Boolean)
        .optional("qos", Kind::Any)
        .optional("retain", Kind::Boolean)
        .optional("message_expiry_interval", Kind::Any);
    hass.services().register_with_schema(
        super::DOMAIN,
        "publish",
        Some(schema),
        SupportsResponse::None,
        move |call| {
            let tx = tx.clone();
            async move {
                let data = &call.data;
                let topic = data
                    .get("topic")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let mut payload = match data.get("payload") {
                    None | Some(Value::Null) => Vec::new(),
                    Some(v) => entity::cv_string(v).unwrap_or_default().into_bytes(),
                };
                if data.get("evaluate_payload").and_then(Value::as_bool) == Some(true)
                    && let Ok(text) = std::str::from_utf8(&payload)
                    && let Some(bytes) = bytes_literal(text)
                {
                    payload = bytes;
                }
                let qos = data.get("qos").and_then(qos_of).unwrap_or(0);
                let retain = data.get("retain").and_then(Value::as_bool).unwrap_or(false);
                let expiry = match data.get("message_expiry_interval") {
                    None => None,
                    Some(v) => Some(period(v).ok_or_else(|| {
                        ServiceError::Invalid(
                            "expected a dictionary for dictionary value @ \
                             data['message_expiry_interval']"
                                .into(),
                        )
                    })?),
                };
                let (reply, rx) = oneshot::channel();
                let cmd = Command::Publish {
                    topic,
                    payload,
                    qos,
                    retain,
                    expiry,
                    reply,
                };
                if tx.send(cmd).await.is_err() {
                    return Err(ServiceError::Failed("MQTT is not running".into()));
                }
                rx.await
                    .unwrap_or_else(|_| Err(ServiceError::Failed("MQTT is not running".into())))?;
                Ok(None)
            }
        },
    );
}

/// `valid_qos_schema`: `vol.All(vol.Coerce(int), vol.In([0, 1, 2]))`.
fn qos_of(v: &Value) -> Option<u8> {
    let n = match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => crate::num::py_int_str(s).and_then(|i| i64::try_from(i).ok()),
        Value::Bool(b) => Some(i64::from(*b)),
        _ => None,
    }?;
    (0..=2).contains(&n).then_some(n as u8)
}

/// The checks voluptuous makes before the fields: the topic and the QoS.
fn publish_pre_validate(
    data: &ha_core::ServiceData,
) -> Result<Option<ha_core::ServiceData>, ServiceError> {
    let invalid = |key: &str, why: &str| {
        Err(ServiceError::Invalid(format!(
            "{why} for dictionary value @ data['{key}']"
        )))
    };
    for (key, v) in data {
        match key.as_str() {
            "topic" => {
                let Ok(topic) = entity::cv_string(v) else {
                    return invalid("topic", "expected str");
                };
                if let Err(e) = entity::valid_publish_topic(&topic) {
                    return invalid("topic", &e);
                }
            }
            "qos" => {
                let coerced = match v {
                    Value::String(s) => crate::num::py_int_str(s).is_some(),
                    Value::Number(_) | Value::Bool(_) => true,
                    _ => false,
                };
                if !coerced {
                    return invalid("qos", "expected int");
                }
                if qos_of(v).is_none() {
                    return invalid("qos", "value must be one of [0, 1, 2]");
                }
            }
            "payload" if !v.is_null() && entity::cv_string(v).is_err() => {
                return invalid("payload", "expected str");
            }
            _ => {}
        }
    }
    Ok(None)
}

/// `cv.positive_time_period_dict`, in whole seconds.
fn period(v: &Value) -> Option<u32> {
    let Value::Object(m) = v else {
        return None;
    };
    let mut seconds = 0.0;
    for (k, v) in m {
        let n = v.as_f64()?;
        seconds += n * match k.as_str() {
            "days" => 86400.0,
            "hours" => 3600.0,
            "minutes" => 60.0,
            "seconds" => 1.0,
            "milliseconds" => 0.001,
            _ => return None,
        };
    }
    (seconds >= 0.0).then_some(seconds as u32)
}

/// `convert_outgoing_mqtt_payload`: a Python bytes literal (`b'\x01'`)
/// as its bytes.
fn bytes_literal(s: &str) -> Option<Vec<u8>> {
    let body = s
        .strip_prefix("b'")
        .and_then(|r| r.strip_suffix('\''))
        .or_else(|| s.strip_prefix("b\"").and_then(|r| r.strip_suffix('"')))?;
    let mut out = Vec::with_capacity(body.len());
    let b = body.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            if !b[i].is_ascii() {
                return None;
            }
            out.push(b[i]);
            i += 1;
            continue;
        }
        let c = *b.get(i + 1)?;
        i += 2;
        match c {
            b'\\' => out.push(b'\\'),
            b'\'' => out.push(b'\''),
            b'"' => out.push(b'"'),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'0'..=b'7' => {
                let mut v = u32::from(c - b'0');
                let mut n = 1;
                while n < 3 && i < b.len() && (b'0'..=b'7').contains(&b[i]) {
                    v = v * 8 + u32::from(b[i] - b'0');
                    i += 1;
                    n += 1;
                }
                out.push((v & 0xFF) as u8);
            }
            b'x' => {
                let hex = std::str::from_utf8(b.get(i..i + 2)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 2;
            }
            other => {
                out.push(b'\\');
                out.push(other);
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn filters_match_as_paho() {
        assert!(matches(
            "homeassistant/sensor/+/config",
            "homeassistant/sensor/a/config"
        ));
        assert!(!matches(
            "homeassistant/sensor/+/config",
            "homeassistant/sensor/a/b/config"
        ));
        assert!(matches("a/#", "a"));
        assert!(matches("a/#", "a/b/c"));
        assert!(!matches("#", "$SYS/x"));
        assert!(!matches("+/x", "$SYS/x"));
        assert!(matches("a/+", "a/"));
    }

    #[test]
    fn bytes_literals_are_pythons() {
        assert_eq!(bytes_literal(r"b'\x01\x02'"), Some(vec![1, 2]));
        assert_eq!(bytes_literal(r#"b"a\n""#), Some(b"a\n".to_vec()));
        assert_eq!(bytes_literal("hello"), None);
    }

    /// After a wrap, the ids of messages still in flight are passed over.
    #[tokio::test]
    async fn packet_ids_in_flight_are_not_taken_again() {
        let hass = Hass::new();
        let components = crate::components::Components::new(
            &hass,
            Arc::new(ha_registry::Registries::default()),
            crate::light::Profiles::default(),
        );
        let entry = json!({"entry_id": "e", "data": {"broker": "b"}});
        let config = Config::of(entry.as_object().unwrap()).unwrap();
        let mut client = Client::new(hass, components, Arc::new(config), tcp());
        client.mid = 65534;
        for mid in [65535, 1] {
            client.outbound.push(Out {
                mid,
                message: Message {
                    topic: "t".into(),
                    payload: Vec::new(),
                    qos: 1,
                    retain: false,
                },
                expiry: None,
                stage: Stage::WaitPubAck,
                dup: false,
                reply: None,
                deadline: None,
            });
        }
        assert_eq!(client.next_mid(), 2);
    }

    #[test]
    fn periods_are_seconds() {
        assert_eq!(period(&json!({"minutes": 1, "seconds": 5})), Some(65));
        assert_eq!(period(&json!(5)), None);
    }
}

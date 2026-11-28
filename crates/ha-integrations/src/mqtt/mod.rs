//! The `mqtt` integration (`components/mqtt`), as HA 2026.6.1 runs it on
//! paho-mqtt 2.1.0: one client per entry over plain TCP (MQTT 3.1, 3.1.1
//! and 5), HA's birth and will messages, discovery of `sensor` and
//! `binary_sensor` entities (per component and per device), and the
//! `mqtt.publish` action.
//!
//! - **The entry** ([`Config`]): `data` and `options` merged, as HA's
//!   `entry.data | entry.options`. A version 1.1 entry has its options moved
//!   out of `data` (HA's `async_migrate_entry`, to 1.2), and an entry not on
//!   MQTT 5 is tried on it first and moved to it if the broker takes it
//!   (`async_setup_entry`'s protocol migration).
//! - **The client** ([`client`]): `client.py`'s connection, subscriptions,
//!   message dispatch and birth message.
//! - **Discovery** ([`discovery`]) and the entities ([`entity`]):
//!   `discovery.py`, `entity.py`, `sensor.py` and `binary_sensor.py`.
//!
//! Not yet: TLS, WebSockets, client certificates (an entry that asks for
//! one is not set up, and says so); the other discovered platforms (their
//! discovery messages are received and logged); YAML `mqtt:` entities;
//! MQTT device triggers and tags; the `mqtt.dump` and `mqtt.reload`
//! actions; the WebSocket commands (`mqtt/subscribe`, debug info); the
//! config flow; the `subscribe` side of other integrations (HA subscribes
//! to the topics other integrations' manifests list, to start their config
//! flows: `hars` has none of those flows and doesn't); restoring an
//! `expire_after` sensor's last value across a restart; `last_reset`.
//! Deliberate differences are listed on [`client`].

pub mod client;
pub mod discovery;
pub mod entity;
pub mod packet;
mod tables;
#[cfg(test)]
mod tests;

use crate::components::Components;
use crate::config_entries::{ConfigEntries, EntryState, Status};
use ha_core::SetupGuard;
use packet::Version;
use serde_json::{Map, Value};
use std::sync::Arc;

pub const DOMAIN: &str = "mqtt";

/// What a running entry adds to `get_config`'s `components`.
pub const COMPONENTS: &[&str] = &["mqtt", "sensor", "binary_sensor"];

/// The entry versions `async_migrate_entry` takes: 1.x, and 2.1 (what the
/// config flow writes).
pub fn entry_too_new(entry: &Map<String, Value>) -> bool {
    let version = entry.get("version").and_then(Value::as_u64).unwrap_or(1);
    let minor = entry
        .get("minor_version")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    version > 2 || (version == 2 && minor > 1)
}

/// `ENTRY_OPTION_FIELDS`: what version 1.2 keeps in `options`.
const OPTION_FIELDS: [&str; 4] = [
    "discovery",
    "discovery_prefix",
    "birth_message",
    "will_message",
];

/// A birth or will message (`PublishMessage`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub topic: String,
    pub payload: Vec<u8>,
    pub qos: u8,
    pub retain: bool,
}

/// `DEFAULT_BIRTH` and `DEFAULT_WILL`, on `homeassistant/status`.
fn default_message(payload: &str) -> Message {
    Message {
        topic: "homeassistant/status".into(),
        payload: payload.as_bytes().to_vec(),
        qos: 0,
        retain: false,
    }
}

/// An entry's settings: `entry.data | entry.options`.
#[derive(Debug, Clone)]
pub struct Config {
    pub entry_id: String,
    /// The entry's title, for logs (and an unnamed device's name).
    pub title: String,
    /// `pref_disable_new_entities`.
    pub pref_disable_new: bool,
    pub broker: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    /// `None`: a random one, made once per client (`_base62(uuid4)`).
    pub client_id: Option<String>,
    pub keepalive: u16,
    pub version: Version,
    pub discovery: bool,
    pub prefix: String,
    pub birth: Option<Message>,
    pub will: Option<Message>,
}

/// Why an entry isn't set up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// What `hars` doesn't do yet (TLS, WebSockets).
    Unsupported(&'static str),
    /// A setting that isn't valid.
    Invalid(String),
}

fn message(v: Option<&Value>, default: &str) -> Result<Option<Message>, Refused> {
    let Some(v) = v else {
        return Ok(Some(default_message(default)));
    };
    // `if birth := conf.get(...)`: an empty dict is no message.
    let Some(m) = v.as_object().filter(|m| !m.is_empty()) else {
        return match v {
            Value::Object(_) | Value::Null => Ok(None),
            _ => Err(Refused::Invalid(format!("invalid message {v}"))),
        };
    };
    let topic = m.get("topic").and_then(Value::as_str);
    let payload = m.get("payload").and_then(Value::as_str);
    let (Some(topic), Some(payload)) = (topic, payload) else {
        return Err(Refused::Invalid(format!("invalid message {v}")));
    };
    Ok(Some(Message {
        topic: topic.to_owned(),
        payload: payload.as_bytes().to_vec(),
        qos: m.get("qos").and_then(Value::as_u64).unwrap_or(0).min(2) as u8,
        retain: m.get("retain").and_then(Value::as_bool).unwrap_or(false),
    }))
}

impl Config {
    /// The settings of `entry` (a `core.config_entries` item).
    pub fn of(entry: &Map<String, Value>) -> Result<Config, Refused> {
        let empty = Map::new();
        let data = entry
            .get("data")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let options = entry
            .get("options")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let get = |k: &str| options.get(k).or_else(|| data.get(k));
        let str_of = |k: &str| get(k).and_then(Value::as_str).map(str::to_owned);
        if get("transport").and_then(Value::as_str) == Some("websockets") {
            return Err(Refused::Unsupported("MQTT over WebSockets"));
        }
        for key in ["certificate", "client_cert", "client_key"] {
            if get(key).is_some_and(|v| !v.is_null() && v != &Value::Bool(false)) {
                return Err(Refused::Unsupported("MQTT over TLS"));
            }
        }
        let broker = str_of("broker").ok_or(Refused::Invalid("no broker".into()))?;
        let port = match get("port") {
            None | Some(Value::Null) => 1883,
            Some(v) => v
                .as_u64()
                .and_then(|p| u16::try_from(p).ok())
                .ok_or(Refused::Invalid(format!("invalid port {v}")))?,
        };
        let keepalive = match get("keepalive") {
            None | Some(Value::Null) => 60,
            Some(v) => v
                .as_u64()
                .and_then(|p| u16::try_from(p).ok())
                .ok_or(Refused::Invalid(format!("invalid keepalive {v}")))?,
        };
        // No protocol: an entry from YAML, on 3.1.1.
        let version = match get("protocol").and_then(Value::as_str) {
            None => Version::V311,
            Some("3.1") => Version::V31,
            Some("3.1.1") => Version::V311,
            Some("5") => Version::V5,
            Some(other) => return Err(Refused::Invalid(format!("invalid protocol {other}"))),
        };
        Ok(Config {
            entry_id: entry
                .get("entry_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            pref_disable_new: entry
                .get("pref_disable_new_entities")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            title: entry
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            broker,
            port,
            username: str_of("username"),
            password: str_of("password"),
            client_id: str_of("client_id"),
            keepalive,
            version,
            discovery: get("discovery").and_then(Value::as_bool).unwrap_or(true),
            prefix: str_of("discovery_prefix").unwrap_or_else(|| "homeassistant".into()),
            birth: message(get("birth_message"), "online")?,
            will: message(get("will_message"), "offline")?,
        })
    }
}

/// `async_migrate_entry` for a version 1.1 entry: the options move out of
/// `data`, and it becomes 1.2.
pub fn migrate(entries: &ConfigEntries, entry_id: &str) -> Option<Map<String, Value>> {
    entries.update(entry_id, |entry| {
        let version = entry.get("version").and_then(Value::as_u64).unwrap_or(1);
        let minor = entry
            .get("minor_version")
            .and_then(Value::as_u64)
            .unwrap_or(1);
        if version != 1 || minor >= 2 {
            return false;
        }
        let mut data = entry
            .get("data")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut options = entry
            .get("options")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for key in OPTION_FIELDS {
            if let Some(v) = data.remove(key) {
                options.insert(key.to_owned(), v);
            }
        }
        entry.insert("data".into(), Value::Object(data));
        entry.insert("options".into(), Value::Object(options));
        entry.insert("minor_version".into(), Value::from(2));
        true
    });
    entries.get(entry_id)
}

/// Run an `mqtt` entry until the runtime stops. `entries` is where it is
/// stored (for its migrations), if anywhere.
pub async fn run(
    components: Arc<Components>,
    mut entry: Map<String, Value>,
    entries: Option<Arc<ConfigEntries>>,
    dial: client::Dial,
    ready: SetupGuard,
    status: Status,
) {
    let hass = components.hass().clone();
    let entry_id = entry
        .get("entry_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    status.set(EntryState::SetupInProgress, None);
    if let Some(entries) = &entries
        && let Some(migrated) = migrate(entries, &entry_id)
    {
        entry = migrated;
    }
    let mut config = match Config::of(&entry) {
        Ok(config) => config,
        Err(Refused::Unsupported(what)) => {
            let title = entry
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default();
            tracing::error!(
                "The MQTT entry {title} is not set up: hars does not support {what} yet"
            );
            status.set(EntryState::SetupError, None);
            ready.done();
            return;
        }
        Err(Refused::Invalid(why)) => {
            tracing::error!("Invalid MQTT entry: {why}");
            status.set(EntryState::SetupError, None);
            ready.done();
            return;
        }
    };
    // "Automatically migrate the broker protocol to v5 if possible."
    if config.version != Version::V5 && client::try_v5(&config, &dial).await {
        let mut data = entry
            .get("data")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        data.insert("protocol".into(), Value::from("5"));
        if let Some(entries) = &entries {
            entries.update_data(&entry_id, data);
        }
        tracing::info!("The MQTT protocol version was successfully updated to version 5");
        config.version = Version::V5;
    }
    client::run(hass, components, Arc::new(config), dial, ready, status).await;
}

/// The random client id paho would make: 128 random bits in base 62,
/// 22 characters (`_base62(uuid4().int, padding=22)`).
pub fn random_client_id() -> String {
    const DIGITS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    // A version 4 UUID: 122 random bits.
    let mut n = rand::random::<u128>();
    n = (n & !(0xF << 76)) | (0x4 << 76);
    n = (n & !(0x3 << 62)) | (0x2 << 62);
    let mut out = Vec::with_capacity(22);
    while n > 0 {
        out.push(DIGITS[(n % 62) as usize]);
        n /= 62;
    }
    while out.len() < 22 {
        out.push(b'0');
    }
    out.reverse();
    String::from_utf8(out).expect("ASCII")
}

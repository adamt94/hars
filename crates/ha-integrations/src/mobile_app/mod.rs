//! HA's `mobile_app`: the companion apps' registrations, from
//! `core.config_entries`. The webhook itself is served by `ha-api`.

pub mod crypto;
pub mod sensor;

use crypto::Key;
use parking_lot::RwLock;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

pub const DOMAIN: &str = "mobile_app";

/// One app registration: a `mobile_app` config entry. Its `data` changes
/// at runtime (`update_registration`, the `no_legacy_encryption` flag).
#[derive(Debug)]
pub struct Registration {
    pub entry_id: String,
    pub webhook_id: Arc<str>,
    data: RwLock<Map<String, Value>>,
}

/// What opening a request's `encrypted_data` gave.
#[derive(Debug, PartialEq)]
pub enum Opened {
    /// The plaintext, parsed.
    Json(Value),
    /// Bad base64, a failed box, or a plaintext that isn't JSON:
    /// `webhook_payload = None`.
    Failed,
}

impl Registration {
    /// From a `core.config_entries` row; `None` without a `webhook_id`.
    pub fn from_entry(entry: &Map<String, Value>) -> Option<Self> {
        let data = entry.get("data")?.as_object()?.clone();
        let webhook_id = data.get("webhook_id")?.as_str()?.into();
        Some(Self {
            entry_id: entry.get("entry_id")?.as_str()?.to_owned(),
            webhook_id,
            data: RwLock::new(data),
        })
    }

    /// A copy of the entry's `data`.
    pub fn data(&self) -> Map<String, Value> {
        self.data.read().clone()
    }

    fn str_field(&self, key: &str) -> Option<String> {
        self.data.read().get(key)?.as_str().map(str::to_owned)
    }

    /// `config_entry.data[ATTR_SUPPORTS_ENCRYPTION]`, as Python truthiness.
    pub fn supports_encryption(&self) -> bool {
        self.data
            .read()
            .get("supports_encryption")
            .is_some_and(truthy)
    }

    /// The owner: the context `user_id` of everything the app does.
    pub fn user_id(&self) -> Option<String> {
        self.str_field("user_id")
    }

    /// The app's device id (the device registry identifier).
    pub fn device_id(&self) -> Option<String> {
        self.str_field("device_id")
    }

    pub fn device_name(&self) -> Option<String> {
        self.str_field("device_name")
    }

    fn has_flag(&self) -> bool {
        self.data.read().contains_key("no_legacy_encryption")
    }

    /// `handle_webhook`'s decryption: the hex key, then the legacy key
    /// unless `no_legacy_encryption` is set. A hex-key success sets the
    /// flag (`true` is returned when it was newly set, so it can be saved).
    pub fn open(&self, encrypted_data: &str) -> (Opened, bool) {
        let Some(secret) = self.str_field("secret") else {
            return (Opened::Failed, false);
        };
        let parse = |plain: Vec<u8>| match serde_json::from_slice(&plain) {
            Ok(v) => Opened::Json(v),
            Err(_) => Opened::Failed,
        };
        // A base64 error is a `ValueError`: no legacy attempt.
        let Some(raw) = crypto::b64decode(encrypted_data) else {
            return (Opened::Failed, false);
        };
        if let Some(plain) = Key::hex(&secret).and_then(|k| k.open_raw(&raw)) {
            let opened = parse(plain);
            // A plaintext that isn't JSON raises before the flag is set.
            if matches!(opened, Opened::Json(_)) && !self.has_flag() {
                self.data
                    .write()
                    .insert("no_legacy_encryption".into(), Value::Bool(true));
                return (opened, true);
            }
            return (opened, false);
        }
        if self.has_flag() {
            return (Opened::Failed, false);
        }
        match Key::legacy(&secret).open_raw(&raw) {
            Some(plain) => (parse(plain), false),
            None => (Opened::Failed, false),
        }
    }

    /// `webhook_response`'s body for `json`: sealed under the hex key once
    /// `no_legacy_encryption` is set (else the legacy key) when the
    /// registration encrypts; `json` itself otherwise.
    pub fn response_body(&self, json: String) -> String {
        if !self.supports_encryption() {
            return json;
        }
        let Some(secret) = self.str_field("secret") else {
            return json;
        };
        let key = if self.has_flag() {
            Key::hex(&secret)
        } else {
            Some(Key::legacy(&secret))
        };
        match key {
            Some(key) => {
                let sealed = key.seal(json.as_bytes());
                format!(r#"{{"encrypted":true,"encrypted_data":"{sealed}"}}"#)
            }
            None => json,
        }
    }

    /// `update_registration`: a shallow merge into `data`. Returns the new
    /// data.
    pub fn merge(&self, update: Map<String, Value>) -> Map<String, Value> {
        let mut data = self.data.write();
        data.extend(update);
        data.clone()
    }

    /// `enable_encryption`: on a plaintext registration, set `secret` and
    /// turn encryption on, atomically. `false` if it already encrypts.
    pub fn enable_encryption(&self, secret: String) -> bool {
        let mut data = self.data.write();
        if data.get("supports_encryption").is_some_and(truthy) {
            return false;
        }
        data.insert("secret".into(), Value::String(secret));
        data.insert("supports_encryption".into(), Value::Bool(true));
        true
    }
}

/// Python truthiness of a JSON value.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

/// Every registration, by webhook id, and the ids of entries removed
/// before this run (`.storage/mobile_app` `deleted_ids`), which answer 410.
#[derive(Debug, Default)]
pub struct MobileApp {
    registrations: RwLock<HashMap<Arc<str>, Arc<Registration>>>,
    deleted_ids: HashSet<String>,
    sensors: std::sync::OnceLock<sensor::Sensors>,
}

/// The webhook's view of an id.
#[derive(Debug)]
pub enum Webhook {
    Registered(Arc<Registration>),
    /// Removed before this run: `410 Gone`.
    Deleted,
    Unknown,
}

impl MobileApp {
    /// The enabled `mobile_app` entries and `.storage/mobile_app`.
    pub fn load(config_dir: &Path, entries: &[Map<String, Value>]) -> Self {
        #[derive(serde::Deserialize, Default)]
        struct Stored {
            #[serde(default)]
            deleted_ids: Vec<String>,
        }
        let store = ha_core::storage::Store::new(config_dir, DOMAIN, 1, 1, true);
        let deleted_ids = match store.load::<Stored>() {
            Ok(s) => s.unwrap_or_default().deleted_ids,
            Err(e) => {
                tracing::error!("{e}");
                Vec::new()
            }
        };
        Self::new(entries, deleted_ids)
    }

    pub fn new(entries: &[Map<String, Value>], deleted_ids: Vec<String>) -> Self {
        let registrations = entries
            .iter()
            .filter(|e| e.get("domain").and_then(Value::as_str) == Some(DOMAIN))
            .filter(|e| e.get("disabled_by").is_none_or(Value::is_null))
            .filter_map(Registration::from_entry)
            .map(|r| (r.webhook_id.clone(), Arc::new(r)))
            .collect();
        Self {
            registrations: RwLock::new(registrations),
            deleted_ids: deleted_ids.into_iter().collect(),
            sensors: std::sync::OnceLock::new(),
        }
    }

    /// The registrations' entities, restored and written (before start, as
    /// HA's platforms set up).
    pub fn setup(&self, hass: &ha_core::Hass, registry: Arc<ha_registry::Registry>) {
        let regs: Vec<Arc<Registration>> = self.registrations.read().values().cloned().collect();
        let _ = self
            .sensors
            .set(sensor::Sensors::setup(hass, registry, &regs));
    }

    /// The sensors, once [`MobileApp::setup`] ran.
    pub fn sensors(&self) -> Option<&sensor::Sensors> {
        self.sensors.get()
    }

    /// Whether any app is registered (for `components`).
    pub fn is_empty(&self) -> bool {
        self.registrations.read().is_empty()
    }

    pub fn webhook(&self, webhook_id: &str) -> Webhook {
        if self.deleted_ids.contains(webhook_id) {
            return Webhook::Deleted;
        }
        match self.registrations.read().get(webhook_id) {
            Some(r) => Webhook::Registered(r.clone()),
            None => Webhook::Unknown,
        }
    }
}

#[cfg(test)]
mod tests;

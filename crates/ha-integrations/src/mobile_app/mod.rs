//! HA's `mobile_app`: the companion apps' registrations, from
//! `core.config_entries`. The webhook itself is served by `ha-api`.

pub mod crypto;
pub mod notify;
pub mod sensor;
pub mod tracker;

use crypto::Key;
use parking_lot::RwLock;
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

pub const DOMAIN: &str = "mobile_app";

/// One app registration: a `mobile_app` config entry. Its `data` changes
/// at runtime (`update_registration`, the `no_legacy_encryption` flag).
#[derive(Debug)]
pub struct Registration {
    pub entry_id: String,
    pub webhook_id: Arc<str>,
    /// The config entry's title (HA's `entry.title`, which names the
    /// device in notification errors).
    pub title: String,
    /// Load order (HA's `DATA_CONFIG_ENTRIES` order).
    seq: u64,
    data: RwLock<Map<String, Value>>,
    /// Multi-step changes (`update_registration`) run one at a time.
    ops: parking_lot::Mutex<()>,
    /// `false` once the entry is unloaded. Webhook commands hold a read
    /// guard while they run, so an unload waits for them and none starts
    /// after it.
    live: tokio::sync::RwLock<bool>,
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
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        Some(Self {
            entry_id: entry.get("entry_id")?.as_str()?.to_owned(),
            webhook_id,
            title: entry
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            seq: SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            data: RwLock::new(data),
            ops: parking_lot::Mutex::new(()),
            live: tokio::sync::RwLock::new(true),
        })
    }

    /// Hold while a webhook command runs: `None` once the registration
    /// is unloaded (its webhook then answers as an unknown one).
    pub async fn enter(&self) -> Option<tokio::sync::RwLockReadGuard<'_, bool>> {
        let guard = self.live.read().await;
        (*guard).then_some(guard)
    }

    /// Unload: wait for running commands, and let no other start.
    pub async fn retire(&self) {
        *self.live.write().await = false;
    }

    /// Hold while a change spans several steps (the data, the device,
    /// the saved entry), so concurrent requests apply in one order.
    pub fn lock_ops(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.ops.lock()
    }

    /// Run `f` on the data with it locked against changes.
    pub fn with_data<T>(&self, f: impl FnOnce(&Map<String, Value>) -> T) -> T {
        f(&self.data.read())
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
struct Ctx {
    hass: ha_core::Hass,
    registry: Arc<ha_registry::Registry>,
    components: Option<Arc<ha_components::Components>>,
}

impl std::fmt::Debug for Ctx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx").finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
pub struct MobileApp {
    registrations: notify::Registrations,
    /// Removed before this run: they answer 410.
    deleted_ids: HashSet<String>,
    /// Every removed id, as `.storage/mobile_app` keeps them.
    stored_deleted: parking_lot::Mutex<Vec<String>>,
    dir: Option<std::path::PathBuf>,
    /// What registering at runtime needs, from [`MobileApp::setup`].
    ctx: std::sync::OnceLock<Ctx>,
    /// One registration at a time.
    registering: tokio::sync::Mutex<()>,
    sensors: std::sync::OnceLock<sensor::Sensors>,
    trackers: std::sync::OnceLock<tracker::Trackers>,
    notifier: std::sync::OnceLock<Arc<notify::Notifier>>,
    /// Where cloud pushes go, if not [`notify::HttpRelay`].
    relay: std::sync::OnceLock<Arc<dyn notify::Relay>>,
    entries: std::sync::OnceLock<Arc<crate::config_entries::ConfigEntries>>,
}

/// The webhook's view of an id.
#[derive(Debug)]
pub enum Webhook {
    Registered(Arc<Registration>),
    /// Removed before this run: `410 Gone`.
    Deleted,
    Unknown,
}

/// `.storage/mobile_app`'s data.
#[derive(serde::Deserialize, Default)]
struct Stored {
    #[serde(default)]
    deleted_ids: Vec<String>,
}

impl MobileApp {
    /// Whether `.storage/mobile_app` decodes as [`MobileApp::load`]
    /// decodes it (a missing file does): for checking a copy before
    /// importing it.
    pub fn check_store(config_dir: &Path) -> Result<(), ha_core::storage::StoreError> {
        ha_core::storage::Store::new(config_dir, DOMAIN, 1, 1, true)
            .load::<Stored>()
            .map(drop)
    }

    /// The enabled `mobile_app` entries and `.storage/mobile_app`.
    pub fn load(config_dir: &Path, entries: &[Map<String, Value>]) -> Self {
        let store = ha_core::storage::Store::new(config_dir, DOMAIN, 1, 1, true);
        let deleted_ids = match store.load::<Stored>() {
            Ok(s) => s.unwrap_or_default().deleted_ids,
            Err(e) => {
                tracing::error!("{e}");
                Vec::new()
            }
        };
        let mut app = Self::new(entries, deleted_ids);
        app.dir = Some(config_dir.to_owned());
        app
    }

    pub fn new(entries: &[Map<String, Value>], deleted_ids: Vec<String>) -> Self {
        let registrations = entries
            .iter()
            .filter(|e| e.get("domain").and_then(Value::as_str) == Some(DOMAIN))
            .filter(|e| e.get("disabled_by").is_none_or(Value::is_null))
            .filter(|e| !crate::entry_too_new(e))
            .filter_map(Registration::from_entry)
            .map(|r| (r.webhook_id.clone(), Arc::new(r)))
            .collect();
        Self {
            registrations: Arc::new(RwLock::new(registrations)),
            stored_deleted: parking_lot::Mutex::new(deleted_ids.clone()),
            dir: None,
            ctx: std::sync::OnceLock::new(),
            registering: tokio::sync::Mutex::new(()),
            deleted_ids: deleted_ids.into_iter().collect(),
            sensors: std::sync::OnceLock::new(),
            trackers: std::sync::OnceLock::new(),
            notifier: std::sync::OnceLock::new(),
            relay: std::sync::OnceLock::new(),
            entries: std::sync::OnceLock::new(),
        }
    }

    /// Send cloud pushes to `relay` instead of over HTTPS (tests answer
    /// them from memory). Call before [`MobileApp::setup`].
    pub fn set_relay(&self, relay: Arc<dyn notify::Relay>) {
        let _ = self.relay.set(relay);
    }

    /// The registrations' entities, restored and written (before start, as
    /// HA's platforms set up).
    pub fn setup(
        &self,
        hass: &ha_core::Hass,
        registry: Arc<ha_registry::Registry>,
        entries: Arc<crate::config_entries::ConfigEntries>,
        components: Option<Arc<ha_components::Components>>,
    ) {
        let _ = self.entries.set(entries);
        let _ = self.ctx.set(Ctx {
            hass: hass.clone(),
            registry: registry.clone(),
            components,
        });
        let regs: Vec<Arc<Registration>> = self.registrations.read().values().cloned().collect();
        let _ = self
            .sensors
            .set(sensor::Sensors::setup(hass, registry.clone(), &regs));
        let _ = self
            .trackers
            .set(tracker::Trackers::setup(hass, registry.clone(), &regs));
        notify::setup_entities(hass, &registry, &regs);
        let notifier = self.notifier.get_or_init(|| {
            let relay = self
                .relay
                .get()
                .cloned()
                .unwrap_or_else(|| Arc::new(notify::HttpRelay));
            notify::Notifier::new(hass, registry.clone(), self.registrations.clone(), relay)
        });
        // HA registers the notify services whenever `mobile_app` is set up;
        // hars sets it up only with a registration.
        if !regs.is_empty() {
            notifier.register_services();
        }
    }

    /// `POST /api/mobile_app/registrations` after its schema: HA's
    /// registration flow. The device tracker row is made (and given to
    /// the user's person); an entry for the same app and device is
    /// replaced (re-pair: its id is saved as deleted, its entities go,
    /// the tracker moves over); the new entry is set up. Returns the new
    /// webhook id and secret.
    pub async fn register(
        &self,
        user_id: &str,
        mut data: Map<String, Value>,
    ) -> Option<(String, Option<String>)> {
        let _turn = self.registering.lock().await;
        let ctx = self.ctx.get()?;
        let entries = self.entries.get()?;
        let hex = || -> String {
            rand::random::<[u8; 32]>()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        };
        let webhook_id = hex();
        data.insert("webhook_id".into(), Value::String(webhook_id.clone()));
        let secret = data
            .get("supports_encryption")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then(hex);
        if let Some(secret) = &secret {
            data.insert("secret".into(), Value::String(secret.clone()));
        }
        data.insert("user_id".into(), Value::String(user_id.to_owned()));
        let str_of =
            |d: &Map<String, Value>, k: &str| d.get(k).and_then(Value::as_str).map(str::to_owned);
        // An empty slug falls back to the device id.
        if ha_core::slugify(&str_of(&data, "device_name").unwrap_or_default()).is_empty()
            && let Some(id) = data.get("device_id").cloned()
        {
            data.insert("device_name".into(), id);
        }
        // `async_step_registration`.
        let unique_id = match str_of(&data, "device_id") {
            Some(device) => Some(format!(
                "{}-{device}",
                str_of(&data, "app_id").unwrap_or_default()
            )),
            None => {
                let id = format!("{:032x}", rand::random::<u128>());
                data.insert("device_id".into(), Value::String(id));
                None
            }
        };
        let device_id = str_of(&data, "device_id")?;
        let device_name = str_of(&data, "device_name").unwrap_or_default();
        let tracker = ctx.registry.get_or_create_entity(
            "device_tracker",
            DOMAIN,
            &device_id,
            None,
            ha_registry::EntityPatch {
                object_id_base: Some(Some(device_name.clone())),
                ..Default::default()
            },
        );
        if let Some(components) = &ctx.components {
            components.add_user_device_tracker(&ctx.hass, user_id, &tracker.entity_id);
        }
        let entry = entries.new_entry(
            DOMAIN,
            "registration",
            &device_name,
            unique_id.as_deref(),
            data,
        );
        // Re-pair: the old entry unloads and is removed first.
        let old = unique_id
            .as_deref()
            .and_then(|u| entries.find_unique(DOMAIN, u))
            .and_then(|e| Registration::from_entry(&e))
            .map(|r| {
                // The live registration, if loaded: its requests finish first.
                self.registrations
                    .read()
                    .get(&*r.webhook_id)
                    .cloned()
                    .unwrap_or_else(|| Arc::new(r))
            });
        if let Some(old) = &old {
            old.retire().await;
            self.unload(ctx, old);
            self.save_deleted(&old.webhook_id).await;
        }
        entries.add(entry.clone());
        // `async_finish_flow`: the old entry's `removed` follows the new
        // one's `added`.
        if let Some(old) = &old {
            entries.remove(&old.entry_id);
        }
        let reg = Arc::new(Registration::from_entry(&entry)?);
        self.registrations
            .write()
            .insert(reg.webhook_id.clone(), reg.clone());
        self.set_up(ctx, &reg, &device_id, &device_name);
        if let Some(old) = &old {
            ctx.registry.clear_config_entry(&old.entry_id);
        }
        Some((webhook_id, secret))
    }

    /// `async_setup_entry` for a registration added at runtime.
    fn set_up(&self, ctx: &Ctx, reg: &Arc<Registration>, device_id: &str, device_name: &str) {
        let field =
            |k: &str| reg.with_data(|d| d.get(k).and_then(Value::as_str).map(str::to_owned));
        let device = ctx.registry.get_or_create_device(ha_registry::DeviceInfo {
            config_entry_id: reg.entry_id.clone(),
            identifiers: vec![serde_json::json!([DOMAIN, device_id])],
            manufacturer: field("manufacturer"),
            model: field("model"),
            name: Some(device_name.to_owned()),
            sw_version: field("os_version"),
        });
        // The tracker platform attaches the row.
        ctx.registry.get_or_create_entity(
            "device_tracker",
            DOMAIN,
            device_id,
            None,
            ha_registry::EntityPatch {
                config_entry_id: Some(Some(reg.entry_id.clone())),
                device_id: Some(Some(device.id.clone())),
                entity_category: Some(Some("diagnostic".into())),
                has_entity_name: Some(false),
                original_name: Some(Some(device_name.to_owned())),
                ..Default::default()
            },
        );
        if let Some(t) = self.trackers.get() {
            t.add(reg);
        }
        if notify::supports_push(reg) {
            ctx.registry.get_or_create_entity(
                "notify",
                DOMAIN,
                device_id,
                None,
                ha_registry::EntityPatch {
                    config_entry_id: Some(Some(reg.entry_id.clone())),
                    device_id: Some(Some(device.id.clone())),
                    has_entity_name: Some(true),
                    original_name: Some(None),
                    supported_features: Some(1),
                    translation_key: Some(Some("notify".into())),
                    ..Default::default()
                },
            );
            notify::setup_entities(&ctx.hass, &ctx.registry, std::slice::from_ref(reg));
        }
        // `notify.async_reload`.
        if let Some(n) = self.notifier.get() {
            n.register_services();
        }
    }

    /// A registration's entry is being removed from the UI
    /// (`async_unload_entry`, then `async_remove_entry`): its requests
    /// finish, its entities go, its webhook stops answering and its id is
    /// saved as deleted. An entry with no registration here (a disabled
    /// one) has nothing to unload, and its id is saved all the same.
    pub async fn stop_entry(&self, entry: &Map<String, Value>) {
        let _turn = self.registering.lock().await;
        let Some(stored) = Registration::from_entry(entry) else {
            return;
        };
        let live = self.registrations.read().get(&*stored.webhook_id).cloned();
        if let (Some(reg), Some(ctx)) = (live, self.ctx.get()) {
            reg.retire().await;
            self.unload(ctx, &reg);
        }
        self.save_deleted(&stored.webhook_id).await;
    }

    /// `async_unload_entry`: the entities go and the webhook stops
    /// answering (an unknown id until a restart, then 410).
    fn unload(&self, ctx: &Ctx, reg: &Registration) {
        if let Some(s) = self.sensors.get() {
            s.unload(&reg.webhook_id);
        }
        if let Some(t) = self.trackers.get() {
            t.unload(&reg.entry_id);
        }
        notify::unload_entity(&ctx.hass, &ctx.registry, reg);
        self.registrations.write().remove(&*reg.webhook_id);
        // `notify.async_reload`.
        if let Some(n) = self.notifier.get() {
            n.register_services();
        }
    }

    /// `async_remove_entry`: the webhook id joins `deleted_ids`, saved now.
    async fn save_deleted(&self, webhook_id: &str) {
        let ids = {
            let mut ids = self.stored_deleted.lock();
            ids.push(webhook_id.to_owned());
            ids.clone()
        };
        let Some(dir) = self.dir.clone() else {
            return;
        };
        let res = tokio::task::spawn_blocking(move || {
            ha_core::storage::Store::new(&dir, DOMAIN, 1, 1, false)
                .save(&serde_json::json!({ "deleted_ids": ids }))
        })
        .await;
        if let Ok(Err(e)) | Err(e) = res.map_err(std::io::Error::other) {
            tracing::error!("saving {DOMAIN}: {e}");
        }
    }

    /// Save a registration's data to its config entry (HA's
    /// `async_update_entry`), if it changed.
    pub fn persist(&self, reg: &Registration) {
        if let Some(entries) = self.entries.get() {
            // Read and saved under the data's lock: a newer change can't be
            // overwritten by an older snapshot.
            reg.with_data(|data| entries.update_data(&reg.entry_id, data.clone()));
        }
    }

    /// The trackers, once [`MobileApp::setup`] ran.
    pub fn trackers(&self) -> Option<&tracker::Trackers> {
        self.trackers.get()
    }

    /// The push notification services and channels, once
    /// [`MobileApp::setup`] ran.
    pub fn notifier(&self) -> Option<&Arc<notify::Notifier>> {
        self.notifier.get()
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
pub(crate) mod tests;

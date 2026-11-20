//! The `switchbot_cloud` integration (`components/switchbot_cloud`), for
//! the devices in the inventory: a Bot in press mode (a button and its
//! battery), a Hub 2 (temperature and humidity) and a DIY IR air
//! conditioner (a climate entity and a switch, both remembered rather
//! than read back).
//!
//! Where `hars` differs from HA 2026.6.1, on purpose:
//! - The webhook flow is never run: HA's deletes any URL that isn't its
//!   own, so two instances would keep deleting each other's, and these are
//!   writes to the user's account. So the Bot is polled every 600 s like
//!   the Hub (HA stops polling it once its webhook is registered, and its
//!   battery then only moves at restarts).
//! - It speaks to the API with its own User-Agent.
//! - Other device types, and a Bot in switch mode, are logged and skipped.
//!
//! Each setup makes the devices' and entities' registry rows as HA's
//! platforms do (`async_get_or_create` in both registries), so devices
//! added from the UI show on the integration's page and keep their entity
//! ids. Rows an import brought are left as they are when HA wrote them.

pub mod api;
pub mod config_flow;

use crate::button::Button;
use crate::climate::{self, Climate, Precision, SetTemperature, Temp};
use crate::components::Components;
use crate::config_entries::{EntryState, Status};
use crate::light::BoxFuture;
use crate::switch::Switch;
use api::{Api, ApiError, Device};
use ha_core::{Attributes, EntityId, Hass, ServiceError};
use ha_registry::{DeviceInfo, EntityPatch, Registries, Registry};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub const DOMAIN: &str = "switchbot_cloud";

/// What running an entry loads, for `get_config`'s `components`.
pub const COMPONENTS: &[&str] = &["switchbot_cloud", "button", "climate", "sensor", "switch"];

/// Held while an entry's rows, entities and first states are made, and
/// while a removed entry's are taken away: one entry at a time, so two
/// accounts that list the same devices can't both take them (HA reserves
/// an entity id before its setup yields). Never held across an `await`.
pub(crate) static SETUP: Mutex<()> = parking_lot::const_mutex(());

/// Every coordinator's `update_interval`.
const INTERVAL: Duration = Duration::from_secs(600);

const HVAC_MODES: &[&str] = &["heat_cool", "cool", "dry", "fan_only", "heat", "off"];
const FAN_MODES: &[&str] = &["auto", "low", "medium", "high"];

/// A polled device's coordinator: its last good status, and whether the
/// last refresh worked (its entities' availability).
#[derive(Debug, Default)]
struct Polled {
    data: Map<String, Value>,
    ok: bool,
}

/// What an entry's entities share: the API and the polled devices.
pub struct Shared {
    hass: Hass,
    api: Api,
    polled: Mutex<HashMap<String, Polled>>,
    /// False once the entry is stopped ([`run`] ended).
    alive: Mutex<bool>,
}

impl Shared {
    /// Write an entity's state, unless the entry was stopped: a command
    /// still in flight when its entry is removed must not bring a state
    /// back (HA's entity, once removed, writes nothing).
    fn write(
        &self,
        entity_id: EntityId,
        state: &str,
        attributes: Attributes,
        context: Option<ha_core::Context>,
    ) {
        let alive = self.alive.lock();
        if *alive {
            self.hass
                .set_state_with_context(entity_id, state, attributes, context);
        }
    }

    fn ok(&self, device: &str) -> bool {
        self.polled.lock().get(device).is_some_and(|p| p.ok)
    }
}

/// One config entry: its shared state and entities.
pub struct Entry {
    shared: Arc<Shared>,
    sensors: Vec<Arc<Sensor>>,
    buttons: Vec<Arc<BotButton>>,
    climates: Vec<Arc<AcClimate>>,
    switches: Vec<Arc<AcSwitch>>,
}

impl Entry {
    fn ok(&self, device: &str) -> bool {
        self.shared.ok(device)
    }

    /// No entity: every device belongs to another entry.
    fn is_empty(&self) -> bool {
        self.sensors.is_empty()
            && self.buttons.is_empty()
            && self.climates.is_empty()
            && self.switches.is_empty()
    }

    /// Write every entity, in HA's (unstable) startup order.
    fn write_all(&self) {
        self.climates.iter().for_each(|c| c.write());
        self.buttons.iter().for_each(|b| b.write());
        self.switches.iter().for_each(|s| s.write());
        self.sensors.iter().for_each(|s| s.write());
    }

    /// Write a polled device's entities.
    fn write_device(&self, device: &str) {
        self.buttons
            .iter()
            .filter(|b| b.device == device)
            .for_each(|b| b.write());
        self.sensors
            .iter()
            .filter(|s| s.device == device)
            .for_each(|s| s.write());
    }

    /// One poll of every polled device (sequentially), then the remotes'
    /// refresh, which asks nothing and re-reports their states.
    pub async fn poll(&self) {
        let ids: Vec<String> = self.shared.polled.lock().keys().cloned().collect();
        for id in ids {
            let result = self.shared.api.status(&id).await;
            let was_ok = self.ok(&id);
            let changed = {
                let mut polled = self.shared.polled.lock();
                let p = polled.entry(id.clone()).or_default();
                match result {
                    Ok(data) => {
                        if !p.ok {
                            tracing::info!("Fetching switchbot_cloud data recovered");
                        }
                        p.data = data;
                        p.ok = true;
                        true
                    }
                    Err(e) => {
                        log_failure(&e, was_ok);
                        p.ok = false;
                        was_ok
                    }
                }
            };
            if changed {
                self.write_device(&id);
            }
        }
        self.climates.iter().for_each(|c| c.write());
        self.switches.iter().for_each(|s| s.write());
    }
}

/// The coordinator's log for a failed refresh: some classes only on the
/// first failure in a row, the unexpected ones on every poll.
fn log_failure(e: &ApiError, first: bool) {
    match e {
        ApiError::Timeout if first => tracing::error!("Timeout fetching switchbot_cloud data"),
        ApiError::Other(m) if first => {
            tracing::error!("Error requesting switchbot_cloud data: {m}")
        }
        ApiError::Connection(m) if first => tracing::error!(
            "Error fetching switchbot_cloud data: Error communicating with API: {m}"
        ),
        ApiError::Auth | ApiError::Offline => {
            tracing::error!("Unexpected error fetching switchbot_cloud data: {e:?}")
        }
        _ => {}
    }
}

/// Why setup stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum SetupError {
    /// `ConfigEntryNotReady`: retried on HA's ladder.
    Retry(String),
    /// `setup_error`: given up until the entry changes.
    Fatal(String),
}

/// HA's setup: the device list, then each polled device's first status
/// (any failure retries the whole entry), then the entities, named from
/// `registries`.
pub async fn setup(
    hass: &Hass,
    registries: &Registries,
    api: Api,
) -> Result<Arc<Entry>, SetupError> {
    let (devices, polled) = fetch(&api).await?;
    Ok(build(hass, registries, api, &devices, polled))
}

/// The device list and each polled device's first status.
async fn fetch(api: &Api) -> Result<(Vec<Device>, HashMap<String, Polled>), SetupError> {
    let devices = match api.list_devices().await {
        Ok(d) => d,
        Err(ApiError::Connection(m)) => return Err(SetupError::Retry(m)),
        Err(ApiError::Auth) => {
            return Err(SetupError::Fatal(
                "Invalid authentication while connecting to SwitchBot API: ".into(),
            ));
        }
        Err(e) => return Err(SetupError::Fatal(format!("{e:?}"))),
    };
    let mut polled = HashMap::new();
    for d in devices.iter().filter(|d| polls(d)) {
        match api.status(&d.id).await {
            Ok(data) => {
                polled.insert(d.id.clone(), Polled { data, ok: true });
            }
            Err(ApiError::Connection(m)) => {
                return Err(SetupError::Retry(format!(
                    "Error communicating with API: {m}"
                )));
            }
            Err(e) => return Err(SetupError::Retry(format!("{e:?}"))),
        }
    }
    Ok((devices, polled))
}

/// A registry row a platform makes for a device's entity.
struct Row {
    domain: &'static str,
    /// `_attr_unique_id`.
    unique_id: String,
    /// The entity description's name (`None`: the device's main entity).
    name: Option<&'static str>,
    device_class: Option<&'static str>,
    unit: Option<&'static str>,
    capabilities: Option<Value>,
    supported_features: i64,
    /// The sensor component's `suggested_display_precision` option.
    precision: Option<i64>,
}

/// The entities HA's platforms make for a device `hars` runs: what
/// [`build`] makes of it, as registry rows.
fn rows(d: &Device, status: Option<&Polled>) -> Vec<Row> {
    let row = |domain, unique_id: String| Row {
        domain,
        unique_id,
        name: None,
        device_class: None,
        unit: None,
        capabilities: None,
        supported_features: 0,
        precision: None,
    };
    let sensor = |key: &str, name, class, unit, precision| Row {
        name: Some(name),
        device_class: Some(class),
        unit: Some(unit),
        capabilities: Some(json!({"state_class": "measurement"})),
        precision,
        ..row("sensor", format!("{}_{key}", d.id))
    };
    match (d.remote, d.kind.as_str()) {
        (true, kind) if kind.ends_with("Air Conditioner") => vec![
            Row {
                // `ClimateEntity.capability_attributes`.
                capabilities: Some(json!({
                    "hvac_modes": HVAC_MODES,
                    "min_temp": 7,
                    "max_temp": 35,
                    "target_temp_step": 1,
                    "fan_modes": FAN_MODES,
                })),
                supported_features: {
                    use climate::feature::*;
                    i64::from(TARGET_TEMPERATURE | FAN_MODE | TURN_OFF | TURN_ON)
                },
                ..row("climate", d.id.clone())
            },
            Row {
                device_class: Some("switch"),
                ..row("switch", d.id.clone())
            },
        ],
        (false, "Bot") => {
            let press = status
                .and_then(|p| p.data.get("deviceMode"))
                .and_then(Value::as_str)
                == Some("pressMode");
            let mut rows = Vec::new();
            if press {
                rows.push(row("button", d.id.clone()));
            }
            rows.push(sensor("battery", "Battery", "battery", "%", None));
            rows
        }
        (false, "Hub 2") => vec![
            // `UNITS_PRECISION`: a temperature in its default unit shows
            // one decimal.
            sensor("temperature", "Temperature", "temperature", "°C", Some(1)),
            sensor("humidity", "Humidity", "humidity", "%", None),
        ],
        _ => Vec::new(),
    }
}

/// `_async_add_entity`'s check of a unique id: the entity registered for
/// it, if that entity is running (it has a state that isn't a restored
/// placeholder). A second account that lists the same devices meets this:
/// HA's platform logs "does not generate unique IDs" and leaves the entity
/// out, with no registry row touched.
fn taken(hass: &Hass, registries: &Registries, domain: &str, unique_id: &str) -> Option<String> {
    let row = registries.entities.iter().find(|e| {
        e.platform == DOMAIN
            && e.entity_id.split('.').next() == Some(domain)
            && e.unique_id.as_ref().and_then(Value::as_str) == Some(unique_id)
    })?;
    let state = hass.states().get(&EntityId::parse(&row.entity_id).ok()?)?;
    (!state.attributes.contains_key("restored")).then(|| row.entity_id.clone())
}

/// The devices' and entities' registry rows, as `SwitchBotCloudEntity`'s
/// `DeviceInfo` and the entity platform register them on every setup: made
/// if missing, changed where HA's values differ (a device renamed in the
/// app), left alone otherwise. An entity another entry runs is skipped,
/// and its device too if nothing else is made for it. `disable_new` is the
/// entry's `pref_disable_new_entities`: a row made now starts disabled by
/// the integration, as the entity registry makes it.
fn register(
    hass: &Hass,
    registry: &Registry,
    entry_id: &str,
    disable_new: bool,
    devices: &[Device],
    polled: &HashMap<String, Polled>,
) {
    let before = registry.snapshot();
    for d in devices {
        let status = polled.get(&d.id);
        let rows: Vec<Row> = rows(d, status)
            .into_iter()
            .filter(|r| taken(hass, &before, r.domain, &r.unique_id).is_none())
            .collect();
        if rows.is_empty() {
            continue;
        }
        let device = registry.get_or_create_device(DeviceInfo {
            config_entry_id: entry_id.to_owned(),
            identifiers: vec![json!([DOMAIN, d.id])],
            manufacturer: Some("SwitchBot".into()),
            model: Some(d.kind.clone()),
            name: Some(d.name.clone()),
            // `coordinator.data.get("version")`; a remote has no data.
            sw_version: status
                .and_then(|p| p.data.get("version"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
        for row in rows {
            let text = |s: Option<&str>| Some(s.map(str::to_owned));
            // The sensor's option goes beside the options the row has
            // (`async_update_entity_options` for the `sensor` domain).
            let options = row.precision.map(|precision| {
                let mut options = registry
                    .entity_by_unique_id(row.domain, DOMAIN, &row.unique_id)
                    .map(|e| e.options.object())
                    .unwrap_or_default();
                let mut sensor = match options.remove("sensor") {
                    Some(Value::Object(o)) => o,
                    _ => Map::new(),
                };
                sensor.insert("suggested_display_precision".into(), json!(precision));
                options.insert("sensor".into(), Value::Object(sensor));
                Value::Object(options)
            });
            let patch = EntityPatch {
                capabilities: Some(row.capabilities),
                config_entry_id: Some(Some(entry_id.to_owned())),
                device_id: Some(Some(device.id.clone())),
                entity_category: Some(None),
                has_entity_name: Some(true),
                object_id_base: text(row.name),
                options,
                original_device_class: text(row.device_class),
                original_icon: Some(None),
                original_name: text(row.name),
                supported_features: Some(row.supported_features),
                translation_key: Some(None),
                unit_of_measurement: text(row.unit),
                ..Default::default()
            };
            let disabled_by = disable_new.then(|| "integration".to_owned());
            registry.get_or_create_entity(row.domain, DOMAIN, &row.unique_id, disabled_by, patch);
        }
    }
}

/// The entry's entities, with ids and names from `registries`.
fn build(
    hass: &Hass,
    registries: &Registries,
    api: Api,
    devices: &[Device],
    polled: HashMap<String, Polled>,
) -> Arc<Entry> {
    let shared = Arc::new(Shared {
        hass: hass.clone(),
        api,
        polled: Mutex::new(polled),
        alive: Mutex::new(true),
    });
    let names = crate::naming::Names::new(hass, registries, DOMAIN);
    // An entity another entry already runs is left out, as HA's platform
    // does (see `taken`).
    let free = |domain: &str, unique_id: &str| match taken(hass, registries, domain, unique_id) {
        Some(entity_id) => {
            tracing::error!(
                "Platform switchbot_cloud does not generate unique IDs. ID {unique_id} already exists - ignoring {entity_id}"
            );
            false
        }
        None => true,
    };
    let (mut sensors, mut buttons, mut climates, mut switches) = (vec![], vec![], vec![], vec![]);
    for d in devices {
        let sensor = |key: &'static str, name: &str, class: &str, unit: &str| {
            let unique_id = format!("{}_{key}", d.id);
            if !free("sensor", &unique_id) {
                return None;
            }
            let (entity_id, friendly) =
                names.entity("sensor", &unique_id, &d.id, &d.name, Some(name))?;
            let attrs = json!({"state_class": "measurement", "unit_of_measurement": unit,
                "device_class": class, "friendly_name": friendly});
            Some(Sensor {
                shared: shared.clone(),
                device: d.id.clone(),
                key,
                entity_id,
                attrs: attrs.as_object().cloned().unwrap_or_default(),
            })
        };
        match (d.remote, d.kind.as_str()) {
            (true, kind) if kind.ends_with("Air Conditioner") => {
                if free("climate", &d.id)
                    && let Some((entity_id, friendly)) =
                        names.entity("climate", &d.id, &d.id, &d.name, None)
                    && let Some(c) = AcClimate::restored(&shared, &d.id, entity_id, friendly)
                {
                    climates.push(c);
                }
                if free("switch", &d.id)
                    && let Some((entity_id, friendly)) =
                        names.entity("switch", &d.id, &d.id, &d.name, None)
                {
                    switches.push(AcSwitch {
                        shared: shared.clone(),
                        device: d.id.clone(),
                        entity_id,
                        friendly,
                        on: Mutex::new(None),
                    });
                }
            }
            (false, "Bot") => {
                sensors.extend(sensor("battery", "Battery", "battery", "%"));
                let mode = shared
                    .polled
                    .lock()
                    .get(&d.id)
                    .and_then(|p| p.data.get("deviceMode").cloned());
                if mode.as_ref().and_then(|m| m.as_str()) != Some("pressMode") {
                    tracing::warn!(
                        "SwitchBot Bot {} is in {mode:?}; hars only supports press mode",
                        d.name
                    );
                } else if free("button", &d.id)
                    && let Some((entity_id, friendly)) =
                        names.entity("button", &d.id, &d.id, &d.name, None)
                {
                    let last = crate::button::restore(hass, &entity_id);
                    buttons.push(BotButton {
                        shared: shared.clone(),
                        device: d.id.clone(),
                        entity_id,
                        friendly,
                        last: Mutex::new(last),
                    });
                }
            }
            (false, "Hub 2") => {
                sensors.extend(sensor("temperature", "Temperature", "temperature", "°C"));
                sensors.extend(sensor("humidity", "Humidity", "humidity", "%"));
            }
            _ => tracing::info!(
                "SwitchBot {} ({}) isn't supported by hars yet",
                d.name,
                d.kind
            ),
        }
    }
    // A device none of whose entities are this entry's isn't polled: HA's
    // coordinator refreshes only while an entity listens to it.
    shared.polled.lock().retain(|id, _| {
        sensors.iter().any(|s: &Sensor| s.device == *id)
            || buttons.iter().any(|b: &BotButton| b.device == *id)
    });
    Arc::new(Entry {
        shared,
        sensors: sensors.into_iter().map(Arc::new).collect(),
        buttons: buttons.into_iter().map(Arc::new).collect(),
        climates: climates.into_iter().map(Arc::new).collect(),
        switches: switches.into_iter().map(Arc::new).collect(),
    })
}

/// Devices whose status is fetched.
fn polls(d: &Device) -> bool {
    !d.remote && matches!(d.kind.as_str(), "Bot" | "Hub 2")
}

/// Set up an entry (on HA's retry ladder), feed the components and poll.
/// `ready` is done after the first setup attempt; `status` hears where the
/// setup is (`ConfigEntryState`).
pub async fn run(
    components: Arc<Components>,
    entry: Map<String, Value>,
    host: String,
    ready: ha_core::SetupGuard,
    status: Status,
) {
    let mut ready = Some(ready);
    let data = entry.get("data").and_then(Value::as_object);
    let field = |k: &str| {
        data.and_then(|d| d.get(k))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let (Some(token), Some(secret)) = (field("api_token"), field("api_key")) else {
        tracing::error!("switchbot_cloud: the entry has no API token or secret");
        status.set(EntryState::SetupError, None);
        return;
    };
    let entry_id = entry.get("entry_id").and_then(Value::as_str);
    let polling = entry.get("pref_disable_polling").and_then(Value::as_bool) != Some(true);
    let client = match ha_http::Client::builder(crate::USER_AGENT).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("switchbot_cloud: {e}");
            status.set(EntryState::SetupError, None);
            return;
        }
    };
    let hass = components.hass().clone();
    let mut tries = 0u32;
    let (api, devices, polled) = loop {
        if tries > 0 {
            // `async_setup` again: each attempt starts in progress.
            status.set(EntryState::SetupInProgress, None);
        }
        let api = Api::new(client.clone(), host.clone(), token.clone(), secret.clone());
        match fetch(&api).await {
            Ok((devices, polled)) => break (api, devices, polled),
            // `async_setup_entry` returned `False`.
            Err(SetupError::Fatal(why)) => {
                tracing::error!(
                    "Error setting up entry SwitchBot Cloud for switchbot_cloud: {why}"
                );
                status.set(EntryState::SetupError, None);
                return;
            }
            // `ConfigEntryNotReady`: its message is the entry's `reason`.
            Err(SetupError::Retry(why)) => {
                status.set(EntryState::SetupRetry, Some(why.clone()));
                if let Some(r) = ready.take() {
                    r.done();
                }
                let wait = (5u64 << tries.min(7)).min(600);
                tracing::info!(
                    "Config entry 'SwitchBot Cloud' for switchbot_cloud integration not ready yet: {why}; Retrying in {wait} seconds"
                );
                tries += 1;
                hass.setup_retry_wait(Duration::from_secs(wait) + jitter())
                    .await;
            }
        }
    };
    let entry = {
        let _one = SETUP.lock();
        if let (Some(registry), Some(entry_id)) = (components.registry(), entry_id) {
            let disable_new = entry
                .get("pref_disable_new_entities")
                .and_then(Value::as_bool)
                == Some(true);
            register(&hass, registry, entry_id, disable_new, &devices, &polled);
        }
        let entry = build(&hass, &components.registries(), api, &devices, polled);
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
        entry
    };
    // When this task ends (its entry was removed), the entities stop
    // writing: whoever stops it waits for that before removing the states.
    struct Stopped(Arc<Shared>);
    impl Drop for Stopped {
        fn drop(&mut self) {
            *self.0.alive.lock() = false;
        }
    }
    let _stopped = Stopped(entry.shared.clone());
    status.set(EntryState::Loaded, None);
    drop(ready);
    if entry.is_empty() {
        // Every device is another entry's, or disabled: loaded, with
        // nothing to run and nothing kept.
        return;
    }
    if !polling {
        // Nothing to do, but its entities run until the entry is stopped.
        return std::future::pending().await;
    }
    let every = INTERVAL + jitter();
    loop {
        tokio::time::sleep(every).await;
        entry.poll().await;
    }
}

/// HA's per-coordinator offset, 0.05–0.5 s.
fn jitter() -> Duration {
    Duration::from_micros(rand::random_range(50_000..500_001))
}

/// A climate or switch command's library error as HA's entity raises it.
fn ha_error(e: ApiError) -> ServiceError {
    let placeholders = |m: &str| {
        let mut p = Map::new();
        p.insert("error".into(), json!(m));
        Some(p)
    };
    match e {
        ApiError::Offline => ServiceError::translated(
            DOMAIN,
            "device_offline",
            None,
            "SwitchBot Cloud device is offline".into(),
        ),
        ApiError::Connection(m) => ServiceError::translated(
            DOMAIN,
            "connection_error",
            placeholders(&m),
            format!("Error communicating with SwitchBot Cloud: {m}"),
        ),
        ApiError::Auth => ServiceError::translated(
            DOMAIN,
            "command_failed",
            placeholders(""),
            "SwitchBot Cloud command failed: ".into(),
        ),
        ApiError::Other(m) => ServiceError::Unknown(m),
        ApiError::Timeout => ServiceError::Unknown(String::new()),
    }
}

/// A float state as HA writes it (`f"{x:.15}"`), an int as it is.
fn sensor_state(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) if n.is_f64() => Some(crate::num::py_format(n.as_f64()?, 15)),
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(if *b { "True" } else { "False" }.into()),
        _ => None,
    }
}

/// A Bot's battery, or a Hub's temperature or humidity.
struct Sensor {
    shared: Arc<Shared>,
    device: String,
    key: &'static str,
    entity_id: EntityId,
    attrs: Attributes,
}

impl Sensor {
    fn write(&self) {
        let entry = &self.shared;
        let state = match entry.polled.lock().get(&self.device) {
            Some(p) if p.ok => p
                .data
                .get(self.key)
                .and_then(sensor_state)
                .unwrap_or_else(|| "unknown".into()),
            _ => "unavailable".into(),
        };
        entry.write(self.entity_id.clone(), &state, self.attrs.clone(), None);
    }
}

/// A Bot in press mode: its state is HA's time of the last press.
struct BotButton {
    shared: Arc<Shared>,
    device: String,
    entity_id: EntityId,
    friendly: String,
    last: Mutex<Option<String>>,
}

impl BotButton {
    fn write(&self) {
        self.write_with(None);
    }

    fn write_with(&self, context: Option<ha_core::Context>) {
        let entry = &self.shared;
        let state = match entry.ok(&self.device) {
            true => self.last.lock().clone().unwrap_or_else(|| "unknown".into()),
            false => "unavailable".into(),
        };
        let mut a = Map::new();
        a.insert("friendly_name".into(), json!(self.friendly));
        entry.write(self.entity_id.clone(), &state, a, context);
    }
}

impl Button for BotButton {
    fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }

    fn available(&self) -> bool {
        self.shared.ok(&self.device)
    }

    fn pressed(&self, at: String, context: &ha_core::Context) {
        *self.last.lock() = Some(at);
        self.write_with(Some(context.clone()));
    }

    /// The library's errors escape unmapped (`unknown_error`).
    fn press(&self) -> BoxFuture<'_> {
        Box::pin(async move {
            let entry = &self.shared;
            entry
                .api
                .command(&self.device, "press", "default")
                .await
                .map_err(|e| ServiceError::Unknown(e.to_string()))
        })
    }
}

/// The DIY IR air conditioner's climate entity: its state is what was
/// last sent, or restored.
pub struct AcClimate {
    shared: Arc<Shared>,
    device: String,
    entity_id: EntityId,
    friendly: String,
    st: Mutex<AcState>,
}

#[derive(Debug, Clone, PartialEq)]
struct AcState {
    mode: String,
    fan: Option<String>,
    target: Option<Temp>,
}

impl AcClimate {
    /// `async_added_to_hass`: the last state if it was a mode, its
    /// `fan_mode` and `temperature` if present. A state that isn't an
    /// `HVACMode` fails the entity (HA doesn't add it).
    fn restored(
        shared: &Arc<Shared>,
        device: &str,
        entity_id: EntityId,
        friendly: String,
    ) -> Option<Self> {
        let mut st = AcState {
            mode: "fan_only".into(),
            fan: Some("auto".into()),
            target: Some(Temp::Int(21)),
        };
        let restore = shared.hass.restore();
        if let Some(last) = restore.last_state(entity_id.as_str()) {
            let state = last["state"].as_str().unwrap_or_default();
            if !matches!(state, "unavailable" | "unknown") {
                if !climate::HVAC_MODES.contains(&state) {
                    tracing::error!(
                        "Error adding entity {entity_id} for domain climate with platform switchbot_cloud: '{state}' is not a valid HVACMode"
                    );
                    return None;
                }
                st.mode = state.to_owned();
                let attrs = &last["attributes"];
                if let Some(f) = attrs.get("fan_mode") {
                    st.fan = f.as_str().map(str::to_owned);
                }
                if let Some(t) = attrs.get("temperature") {
                    st.target = match t {
                        Value::Number(n) if n.is_f64() => n.as_f64().map(Temp::Float),
                        Value::Number(n) => n.as_i64().map(Temp::Int),
                        _ => None,
                    };
                }
            }
        }
        restore.register(entity_id.as_str(), None);
        Some(Self {
            shared: shared.clone(),
            device: device.to_owned(),
            entity_id,
            friendly,
            st: Mutex::new(st),
        })
    }

    fn write(&self) {
        let entry = &self.shared;
        let mut a = climate::attributes(self);
        a.insert("assumed_state".into(), json!(true));
        a.insert("friendly_name".into(), json!(self.friendly));
        a.insert(
            "supported_features".into(),
            json!(self.supported_features()),
        );
        let mode = self.st.lock().mode.clone();
        entry.write(self.entity_id.clone(), &mode, a, None);
    }

    /// `_do_send_command`: always `setAll`, from the call's values or the
    /// remembered ones.
    async fn send(
        &self,
        hvac: Option<&str>,
        fan: Option<&str>,
        temperature: Option<f64>,
    ) -> Result<(), ServiceError> {
        let entry = &self.shared;
        let st = self.st.lock().clone();
        let t = match temperature.filter(|t| *t != 0.0) {
            Some(t) => t,
            None => st.target.map(Temp::get).ok_or_else(|| {
                ServiceError::Unknown(
                    "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"
                        .into(),
                )
            })?,
        };
        let code = |m: &str| match m {
            "heat_cool" => 1,
            "cool" => 2,
            "dry" => 3,
            "heat" => 5,
            _ => 4,
        };
        let mode = match hvac.unwrap_or(&st.mode) {
            "off" => code(&st.mode),
            m => code(m),
        };
        let fan = match fan.or(st.fan.as_deref()) {
            Some("low") => 2,
            Some("medium") => 3,
            Some("high") => 4,
            _ => 1,
        };
        let power = if hvac == Some("off") { "off" } else { "on" };
        let parameter = format!("{},{mode},{fan},{power}", t.trunc() as i64);
        entry
            .api
            .command(&self.device, "setAll", &parameter)
            .await
            .map_err(ha_error)
    }

    async fn set_mode(&self, mode: &str) -> Result<(), ServiceError> {
        self.send(Some(mode), None, None).await?;
        self.st.lock().mode = mode.to_owned();
        self.write();
        Ok(())
    }
}

impl Climate for AcClimate {
    fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }
    fn available(&self) -> bool {
        true
    }
    fn supported_features(&self) -> u32 {
        use climate::feature::*;
        TARGET_TEMPERATURE | FAN_MODE | TURN_OFF | TURN_ON
    }
    fn hvac_mode(&self) -> Option<String> {
        Some(self.st.lock().mode.clone())
    }
    fn hvac_modes(&self) -> &[&'static str] {
        HVAC_MODES
    }
    fn fan_mode(&self) -> Option<String> {
        self.st.lock().fan.clone()
    }
    fn fan_modes(&self) -> &[&'static str] {
        FAN_MODES
    }
    fn precision(&self) -> Precision {
        Precision::Whole
    }
    fn target_temperature_step(&self) -> Option<Temp> {
        Some(Temp::Int(1))
    }
    fn target_temperature(&self) -> Option<Temp> {
        self.st.lock().target
    }

    fn set_hvac_mode(&self, mode: &str) -> BoxFuture<'_> {
        let mode = mode.to_owned();
        Box::pin(async move { self.set_mode(&mode).await })
    }

    fn set_fan_mode(&self, mode: &str) -> BoxFuture<'_> {
        let mode = mode.to_owned();
        Box::pin(async move {
            self.send(None, Some(&mode), None).await?;
            self.st.lock().fan = Some(mode);
            self.write();
            Ok(())
        })
    }

    fn set_temperature(&self, args: SetTemperature) -> BoxFuture<'_> {
        Box::pin(async move {
            let Some(t) = args.temperature else {
                return Ok(());
            };
            self.send(None, None, Some(t)).await?;
            self.st.lock().target = Some(Temp::Float(t));
            self.write();
            Ok(())
        })
    }

    fn turn_on(&self) -> BoxFuture<'_> {
        Box::pin(async move {
            let mode = self.st.lock().mode.clone();
            let mode = if mode == "off" {
                "fan_only".into()
            } else {
                mode
            };
            self.set_mode(&mode).await
        })
    }

    fn turn_off(&self) -> BoxFuture<'_> {
        Box::pin(self.set_mode("off"))
    }
}

/// The IR remote's power switch: `unknown` until commanded.
struct AcSwitch {
    shared: Arc<Shared>,
    device: String,
    entity_id: EntityId,
    friendly: String,
    on: Mutex<Option<bool>>,
}

impl AcSwitch {
    fn write(&self) {
        let entry = &self.shared;
        let state = match *self.on.lock() {
            Some(true) => "on",
            Some(false) => "off",
            None => "unknown",
        };
        let mut a = Map::new();
        a.insert("device_class".into(), json!("switch"));
        a.insert("friendly_name".into(), json!(self.friendly));
        entry.write(self.entity_id.clone(), state, a, None);
    }
}

impl Switch for AcSwitch {
    fn entity_id(&self) -> &EntityId {
        &self.entity_id
    }

    fn available(&self) -> bool {
        true
    }

    fn is_on(&self) -> bool {
        *self.on.lock() == Some(true)
    }

    fn turn(&self, on: bool) -> BoxFuture<'_> {
        Box::pin(async move {
            let entry = &self.shared;
            let command = if on { "turnOn" } else { "turnOff" };
            entry
                .api
                .command(&self.device, command, "default")
                .await
                .map_err(ha_error)?;
            *self.on.lock() = Some(on);
            self.write();
            Ok(())
        })
    }
}

#[cfg(test)]
pub(crate) mod tests;

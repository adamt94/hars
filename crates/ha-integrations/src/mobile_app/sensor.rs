//! `mobile_app`'s sensors and binary sensors (`sensor.py`,
//! `binary_sensor.py`, `entity.py`, and the sensor component's `state`),
//! and the webhook commands that feed them: `update_sensor_states` and
//! `register_sensor`.

use super::{DOMAIN, Registration, truthy};
use crate::num::py_format;
use ha_core::{EntityId, Hass};
use ha_registry::{EntityPatch, Registry};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;

/// `SensorDeviceClass`.
const SENSOR_CLASSES: &[&str] = &[
    "date",
    "enum",
    "timestamp",
    "uptime",
    "absolute_humidity",
    "apparent_power",
    "aqi",
    "area",
    "atmospheric_pressure",
    "battery",
    "blood_glucose_concentration",
    "carbon_monoxide",
    "carbon_dioxide",
    "conductivity",
    "current",
    "data_rate",
    "data_size",
    "distance",
    "duration",
    "energy",
    "energy_distance",
    "energy_storage",
    "frequency",
    "gas",
    "humidity",
    "illuminance",
    "irradiance",
    "moisture",
    "monetary",
    "nitrogen_dioxide",
    "nitrogen_monoxide",
    "nitrous_oxide",
    "ozone",
    "ph",
    "pm1",
    "pm10",
    "pm25",
    "pm4",
    "power_factor",
    "power",
    "precipitation",
    "precipitation_intensity",
    "pressure",
    "reactive_energy",
    "reactive_power",
    "signal_strength",
    "sound_pressure",
    "speed",
    "sulphur_dioxide",
    "temperature",
    "temperature_delta",
    "volatile_organic_compounds",
    "volatile_organic_compounds_parts",
    "voltage",
    "volume",
    "volume_storage",
    "volume_flow_rate",
    "water",
    "weight",
    "wind_direction",
    "wind_speed",
];
/// `BinarySensorDeviceClass`.
const BINARY_CLASSES: &[&str] = &[
    "battery",
    "battery_charging",
    "carbon_monoxide",
    "cold",
    "connectivity",
    "door",
    "garage_door",
    "gas",
    "heat",
    "light",
    "lock",
    "moisture",
    "motion",
    "moving",
    "occupancy",
    "opening",
    "plug",
    "power",
    "presence",
    "problem",
    "running",
    "safety",
    "smoke",
    "sound",
    "tamper",
    "update",
    "vibration",
    "window",
];
/// `NON_NUMERIC_DEVICE_CLASSES`.
const NON_NUMERIC: &[&str] = &["date", "enum", "timestamp", "uptime"];
const STATE_CLASSES: &[&str] = &[
    "measurement",
    "measurement_angle",
    "total",
    "total_increasing",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Sensor,
    BinarySensor,
}

impl Kind {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "sensor" => Some(Self::Sensor),
            "binary_sensor" => Some(Self::BinarySensor),
            _ => None,
        }
    }

    pub fn domain(self) -> &'static str {
        match self {
            Self::Sensor => "sensor",
            Self::BinarySensor => "binary_sensor",
        }
    }
}

/// A loaded (enabled) sensor entity.
#[derive(Debug, Clone)]
struct Sensor {
    entity_id: EntityId,
    kind: Kind,
    /// The name it was created with (`_attr_name`): never updated.
    name: Option<String>,
    /// HA's `_config`: `state`, `attributes`, `icon`, `device_class`,
    /// `unit_of_measurement`, `state_class`, …
    config: Map<String, Value>,
}

type Key = (Kind, String);

#[derive(Default)]
struct Inner {
    sensors: HashMap<Key, Sensor>,
    /// `DATA_PENDING_UPDATES`: the latest update per sensor, kept until an
    /// entity takes it.
    pending: HashMap<Key, Map<String, Value>>,
}

/// The sensors of every registration.
pub struct Sensors {
    hass: Hass,
    registry: Arc<Registry>,
    inner: Arc<Mutex<Inner>>,
    /// One command at a time (HA's event loop): a change and its write
    /// can't interleave with another request's.
    ops: Mutex<()>,
}

fn str_of<'a>(m: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    m.get(k).and_then(Value::as_str)
}

impl std::fmt::Debug for Sensors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sensors").finish_non_exhaustive()
    }
}

impl Sensors {
    /// The entities for each registration's enabled sensor rows, restored
    /// from their last state as HA's `RestoreSensor` does, and written.
    pub fn setup(
        hass: &Hass,
        registry: Arc<Registry>,
        registrations: &[Arc<Registration>],
    ) -> Self {
        let this = Self {
            hass: hass.clone(),
            registry,
            inner: Arc::default(),
            ops: Mutex::new(()),
        };
        let snap = this.registry.snapshot();
        for reg in registrations {
            for row in snap.entities.iter().filter(|e| {
                e.platform == DOMAIN
                    && e.config_entry_id.as_deref() == Some(reg.entry_id.as_str())
                    && e.disabled_by.is_none()
            }) {
                this.add_entity(row);
            }
        }
        this
    }

    /// An entity for an enabled row (`async_added_to_hass`): the config
    /// from the row, any pending update applied, then the last state
    /// restored if it has none; then written.
    fn add_entity(&self, row: &ha_registry::Entity) {
        let Some(kind) = row.entity_id.split('.').next().and_then(Kind::parse) else {
            return;
        };
        let Some(unique) = row.unique_id.as_ref().and_then(Value::as_str) else {
            return;
        };
        let Ok(entity_id) = EntityId::parse(&row.entity_id) else {
            return;
        };
        let mut config = Map::new();
        config.insert("state".into(), Value::Null);
        config.insert("attributes".into(), json!({}));
        let dc = row
            .device_class
            .clone()
            .or(row.original_device_class.clone());
        config.insert("device_class".into(), json!(dc));
        config.insert("icon".into(), json!(row.original_icon));
        config.insert("unit_of_measurement".into(), json!(row.unit_of_measurement));
        if kind == Kind::Sensor
            && let Some(sc) = row
                .capabilities
                .as_ref()
                .and_then(|c| c.object().get("state_class").cloned())
        {
            config.insert("state_class".into(), sc);
        }
        let mut sensor = Sensor {
            entity_id,
            kind,
            name: row.original_name.clone(),
            config,
        };
        let key = (kind, unique.to_owned());
        if let Some(update) = self.inner.lock().pending.remove(&key) {
            sensor.config.extend(update);
        }
        self.restore(&mut sensor);
        self.register_restore(&sensor, key.clone());
        self.write(&sensor);
        self.inner.lock().sensors.insert(key, sensor);
    }

    /// `async_restore_last_state` (+ the sensor's extra data).
    fn restore(&self, s: &mut Sensor) {
        let restore = self.hass.restore();
        let Some(last) = restore.last_state(s.entity_id.as_str()) else {
            return;
        };
        if !matches!(s.config.get("state"), None | Some(Value::Null))
            && s.config.get("state") != Some(&json!("unknown"))
        {
            return;
        }
        let last_state = last.get("state").cloned().unwrap_or(Value::Null);
        let last_attrs = last
            .get("attributes")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut attrs = last_attrs.clone();
        if let Some(Value::Object(own)) = s.config.get("attributes") {
            attrs.extend(own.clone());
        }
        s.config.insert("attributes".into(), Value::Object(attrs));
        if let Some(icon) = last_attrs.get("icon") {
            s.config.insert("icon".into(), icon.clone());
        }
        match s.kind {
            Kind::BinarySensor => {
                s.config
                    .insert("state".into(), json!(last_state.as_str() == Some("on")));
            }
            Kind::Sensor => {
                let extra = restore.last_extra_data(s.entity_id.as_str());
                match extra.as_ref().and_then(Value::as_object) {
                    Some(data) => {
                        // A date or datetime stays typed ({"__type", "isoformat"}).
                        let native = data.get("native_value").cloned().unwrap_or(Value::Null);
                        s.config.insert("state".into(), native);
                        s.config.insert(
                            "unit_of_measurement".into(),
                            data.get("native_unit_of_measurement")
                                .cloned()
                                .unwrap_or(Value::Null),
                        );
                    }
                    None => {
                        s.config.insert("state".into(), Value::Null);
                    }
                }
            }
        }
    }

    /// Sensors dump `{"native_value", "native_unit_of_measurement"}`.
    fn register_restore(&self, s: &Sensor, key: Key) {
        let extra: Option<ha_core::restore::ExtraData> = match s.kind {
            Kind::BinarySensor => None,
            Kind::Sensor => {
                let inner = Arc::downgrade(&self.inner);
                Some(Arc::new(move || {
                    let inner = inner.upgrade()?;
                    let inner = inner.lock();
                    let s = inner.sensors.get(&key)?;
                    let native = native_value(&s.config).map(|n| n.dump());
                    Some(json!({
                        "native_value": native.unwrap_or(Value::Null),
                        "native_unit_of_measurement": s.config.get("unit_of_measurement").cloned().unwrap_or(Value::Null),
                    }))
                }))
            }
        };
        self.hass.restore().register(s.entity_id.as_str(), extra);
    }

    /// `async_write_ha_state`. A sensor whose value HA would reject (a
    /// non-numeric value for a numeric sensor) isn't written.
    fn write(&self, s: &Sensor) {
        let snap = self.registry.snapshot();
        let row = snap
            .entities
            .iter()
            .find(|e| e.entity_id == s.entity_id.as_str());
        let available = s.config.get("state") != Some(&json!("unavailable"));
        let state = if !available {
            "unavailable".to_owned()
        } else {
            match s.kind {
                Kind::BinarySensor => match s.config.get("state") {
                    None | Some(Value::Null) => "unknown".into(),
                    Some(v) => if truthy(v) { "on" } else { "off" }.into(),
                },
                Kind::Sensor => match sensor_state(&s.config, &self.system_temperature()) {
                    Ok(state) => state,
                    Err(why) => {
                        tracing::warn!("not writing {}: {why}", s.entity_id);
                        return;
                    }
                },
            }
        };
        let mut a = Map::new();
        if s.kind == Kind::Sensor
            && let Some(sc) = s.config.get("state_class").filter(|v| !v.is_null())
        {
            a.insert("state_class".into(), sc.clone());
        }
        if available && let Some(Value::Object(extra)) = s.config.get("attributes") {
            a.extend(extra.clone());
        }
        if s.kind == Kind::Sensor
            && let Some(u) = display_unit(&s.config, &self.system_temperature())
        {
            a.insert("unit_of_measurement".into(), u);
        }
        let or = |reg: Option<&String>, own: Option<&Value>| -> Option<Value> {
            reg.filter(|v| !v.is_empty())
                .map(|v| json!(v))
                .or_else(|| own.filter(|v| !v.is_null()).cloned())
        };
        if let Some(dc) = or(
            row.and_then(|r| r.device_class.as_ref()),
            s.config.get("device_class"),
        ) {
            a.insert("device_class".into(), dc);
        }
        if let Some(icon) = or(row.and_then(|r| r.icon.as_ref()), s.config.get("icon")) {
            a.insert("icon".into(), icon);
        }
        if let Some(name) = or(
            row.and_then(|r| r.name.as_ref()),
            s.name.as_ref().map(|n| json!(n)).as_ref(),
        ) {
            a.insert("friendly_name".into(), name);
        }
        self.hass.set_state(s.entity_id.clone(), state, a);
    }

    /// The unit system's temperature unit.
    fn system_temperature(&self) -> String {
        if self.hass.config().unit_system_v2 == "us_customary" {
            "°F".into()
        } else {
            "°C".into()
        }
    }

    /// `_async_update_sensor_entity`: kept as the pending update, then
    /// applied if the entity is loaded.
    fn update(&self, key: Key, data: Map<String, Value>) {
        let sensor = {
            let mut inner = self.inner.lock();
            inner.pending.insert(key.clone(), data);
            let Some(s) = inner.sensors.get(&key).cloned() else {
                return;
            };
            let mut s = s;
            if let Some(update) = inner.pending.remove(&key) {
                s.config.extend(update);
            }
            inner.sensors.insert(key, s.clone());
            s
        };
        self.write(&sensor);
    }

    /// `update_sensor_states`: the per-sensor replies, in request order.
    /// `None` when the payload fails the outer schema (HA's `200 {}`).
    pub fn update_states(&self, reg: &Registration, payload: &Value) -> Option<Map<String, Value>> {
        let _turn = self.ops.lock();
        let items: Vec<&Value> = match payload {
            Value::Array(a) => a.iter().collect(),
            v => vec![v],
        };
        // The outer schema: every item has a known `type` and a `unique_id`.
        let mut parsed = Vec::with_capacity(items.len());
        for item in &items {
            let m = item.as_object()?;
            let kind = Kind::parse(str_of(m, "type")?)?;
            let unique_id = crate_cv_string(m.get("unique_id")?)?;
            parsed.push((kind, unique_id, m));
        }
        let mut resp = Map::new();
        let webhook_id = &*reg.webhook_id;
        for (kind, unique_id, m) in parsed {
            let store_key = format!("{webhook_id}_{unique_id}");
            let Some(row) = self
                .registry
                .entity_by_unique_id(kind.domain(), DOMAIN, &store_key)
            else {
                resp.insert(
                    unique_id.clone(),
                    json!({"success": false, "error": {"code": "not_registered",
                        "message": format!("{} {unique_id} is not registered", kind.domain())}}),
                );
                continue;
            };
            let data = match sensor_schema_full(m) {
                Ok(d) => d,
                Err(message) => {
                    resp.insert(
                        unique_id,
                        json!({"success": false, "error": {"code": "invalid_format", "message": message}}),
                    );
                    continue;
                }
            };
            self.update((kind, store_key), data);
            let mut ok = json!({"success": true});
            if row.disabled_by.is_some() {
                ok["is_disabled"] = json!(true);
            }
            resp.insert(unique_id, ok);
        }
        Some(resp)
    }

    /// `register_sensor`: a new registry row and entity, or the existing
    /// row updated (`disabled` included) and the update applied. `false`
    /// when the payload fails the schema (`200 {}`).
    pub fn register(&self, reg: &Registration, device_id: Option<&str>, payload: &Value) -> bool {
        let _turn = self.ops.lock();
        let Some(data) = register_schema(payload) else {
            return false;
        };
        let kind = Kind::parse(str_of(&data, "type").unwrap_or_default()).expect("validated");
        let unique_id = str_of(&data, "unique_id").unwrap_or_default().to_owned();
        let device_name = reg.device_name().unwrap_or_default();
        let name = format!(
            "{device_name} {}",
            str_of(&data, "name").unwrap_or_default()
        );
        let store_key = format!("{}_{unique_id}", reg.webhook_id);
        let key = (kind, store_key.clone());
        let opt = |k: &str| data.get(k).and_then(Value::as_str).map(str::to_owned);

        let Some(row) = self
            .registry
            .entity_by_unique_id(kind.domain(), DOMAIN, &store_key)
        else {
            let disabled = data.get("disabled") == Some(&json!(true));
            let state_class = data.get("state_class").filter(|v| !v.is_null());
            let row = self.registry.get_or_create_entity(
                kind.domain(),
                DOMAIN,
                &store_key,
                disabled.then(|| "integration".to_owned()),
                EntityPatch {
                    capabilities: Some(state_class.map(|sc| json!({"state_class": sc}))),
                    config_entry_id: Some(Some(reg.entry_id.clone())),
                    device_id: Some(device_id.map(str::to_owned)),
                    entity_category: Some(opt("entity_category")),
                    has_entity_name: Some(false),
                    object_id_base: Some(Some(name.clone())),
                    original_device_class: Some(opt("device_class")),
                    original_icon: Some(opt("icon")),
                    original_name: Some(Some(name.clone())),
                    supported_features: Some(0),
                    translation_key: Some(None),
                    unit_of_measurement: Some(opt("unit_of_measurement")),
                    ..Default::default()
                },
            );
            if !disabled && let Ok(entity_id) = EntityId::parse(&row.entity_id) {
                let sensor = Sensor {
                    entity_id,
                    kind,
                    name: Some(name),
                    config: data,
                };
                self.register_restore(&sensor, key.clone());
                self.write(&sensor);
                self.inner.lock().sensors.insert(key, sensor);
            }
            return true;
        };

        // Re-registration: mobile_app's change set against the row.
        let mut patch = EntityPatch::default();
        if row.original_name.as_deref() != Some(name.as_str()) {
            patch.original_name = Some(Some(name));
        }
        let disabled_now = row.disabled_by.is_some();
        match data.get("disabled").and_then(Value::as_bool) {
            Some(true) if !disabled_now => patch.disabled_by = Some(Some("integration".into())),
            Some(false) if disabled_now => patch.disabled_by = Some(None),
            _ => {}
        }
        let differs = |key: &str, current: &Option<String>| {
            data.get(key)
                .map(|v| v.as_str().map(str::to_owned))
                .filter(|v| v != current)
        };
        patch.device_class = differs("device_class", &row.device_class);
        patch.unit_of_measurement = differs("unit_of_measurement", &row.unit_of_measurement);
        patch.entity_category = differs("entity_category", &row.entity_category);
        patch.original_icon = differs("icon", &row.original_icon);
        let disabling = patch.disabled_by == Some(Some("integration".into()));
        let enabling = patch.disabled_by == Some(None);
        self.registry.update_entity(&row.entity_id, patch);

        if disabling {
            // The entity is removed. (Unregistering reads its restore data,
            // which takes the lock: not held here.)
            let removed = self.inner.lock().sensors.get(&key).cloned();
            if let Some(s) = removed {
                self.hass
                    .restore()
                    .unregister(&self.hass, s.entity_id.as_str());
                self.inner.lock().sensors.remove(&key);
                self.hass.remove_state(&s.entity_id);
            }
        }
        // HA reloads the whole entry 30 s after an enable; `hars` adds just
        // this entity now (spec decision D2): the update becomes its pending
        // one, then it restores as any added entity does.
        let enabled_row = enabling
            .then(|| {
                self.registry
                    .entity_by_unique_id(kind.domain(), DOMAIN, &store_key)
            })
            .flatten();
        if let Some(row) = enabled_row {
            self.inner.lock().pending.insert(key.clone(), data.clone());
            self.add_entity(&row);
        } else {
            self.update(key.clone(), data.clone());
        }
        let loaded = self.inner.lock().sensors.contains_key(&key);
        // The loaded entity's write reconciles the row with its own values
        // (`entity.py`): capabilities, original_device_class, and the name
        // it was created with.
        let loaded_sensor = loaded
            .then(|| self.inner.lock().sensors.get(&key).cloned())
            .flatten();
        if let Some(s) = loaded_sensor {
            let state_class = s.config.get("state_class").filter(|v| !v.is_null());
            self.registry.update_entity(
                s.entity_id.as_str(),
                EntityPatch {
                    capabilities: Some(match kind {
                        Kind::Sensor => state_class.map(|sc| json!({"state_class": sc})),
                        Kind::BinarySensor => None,
                    }),
                    original_device_class: Some(
                        s.config
                            .get("device_class")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    ),
                    original_name: Some(s.name.clone()),
                    supported_features: Some(0),
                    ..Default::default()
                },
            );
        }
        true
    }
}

/// `cv.string` for a JSON value.
fn crate_cv_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(if *b { "True" } else { "False" }.into()),
        Value::Number(n) => Some(match n.as_f64().filter(|_| n.is_f64()) {
            Some(f) => crate::sun::py_float(f),
            None => n.to_string(),
        }),
        _ => None,
    }
}

/// `cv.icon` inside `vol.Any(None, cv.icon)`.
fn icon_ok(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.contains(':'),
        Value::Number(_) | Value::Bool(_) => false,
        _ => false,
    }
}

/// `vol.Any(None, bool, int, float, str)`.
fn state_ok(v: &Value) -> bool {
    !matches!(v, Value::Array(_) | Value::Object(_))
}

/// `SENSOR_SCHEMA_FULL`, with HA's humanised error for a bad item.
fn sensor_schema_full(m: &Map<String, Value>) -> Result<Map<String, Value>, String> {
    let got = |k: &str| {
        let mut s = String::new();
        py_repr(&mut s, m.get(k).unwrap_or(&Value::Null));
        s
    };
    let mut out = Map::new();
    let mut errors = Vec::new();
    for k in m.keys() {
        if !matches!(
            k.as_str(),
            "attributes" | "icon" | "state" | "type" | "unique_id"
        ) {
            errors.push(format!(
                "extra keys not allowed @ data['{k}']. Got {}",
                got(k)
            ));
        }
    }
    match m.get("attributes") {
        None => {
            out.insert("attributes".into(), json!({}));
        }
        Some(Value::Object(a)) => {
            out.insert("attributes".into(), Value::Object(a.clone()));
        }
        Some(_) => errors.push(format!(
            "expected dict for dictionary value @ data['attributes']. Got {}",
            got("attributes")
        )),
    }
    match m.get("icon") {
        None => {
            out.insert("icon".into(), json!("mdi:cellphone"));
        }
        Some(v) if icon_ok(v) => {
            out.insert("icon".into(), v.clone());
        }
        Some(_) => errors.push(format!(
            "not a valid value for dictionary value @ data['icon']. Got {}",
            got("icon")
        )),
    }
    match m.get("state") {
        None => errors.push("required key not provided @ data['state']. Got None".into()),
        Some(v) if state_ok(v) => {
            out.insert("state".into(), v.clone());
        }
        Some(_) => errors.push(format!(
            "not a valid value for dictionary value @ data['state']. Got {}",
            got("state")
        )),
    }
    out.insert("type".into(), m["type"].clone());
    out.insert(
        "unique_id".into(),
        json!(crate_cv_string(&m["unique_id"]).unwrap_or_default()),
    );
    if errors.is_empty() {
        Ok(out)
    } else {
        errors.sort();
        Err(errors.join("\n"))
    }
}

/// `register_sensor`'s schema (+ `_validate_state_class_sensor`), with
/// HA's coercions. `None` on any failure.
fn register_schema(payload: &Value) -> Option<Map<String, Value>> {
    let m = payload.as_object()?;
    const KEYS: &[&str] = &[
        "attributes",
        "device_class",
        "name",
        "type",
        "unique_id",
        "unit_of_measurement",
        "state",
        "entity_category",
        "icon",
        "state_class",
        "disabled",
    ];
    if !m.keys().all(|k| KEYS.contains(&k.as_str())) {
        return None;
    }
    let mut out = Map::new();
    out.insert(
        "attributes".into(),
        match m.get("attributes") {
            None => json!({}),
            Some(v @ Value::Object(_)) => v.clone(),
            Some(_) => return None,
        },
    );
    if let Some(dc) = m.get("device_class") {
        let v = match dc {
            Value::Null => Value::Null,
            Value::String(s) => {
                let lower = s.to_lowercase();
                if !BINARY_CLASSES.contains(&lower.as_str())
                    && !SENSOR_CLASSES.contains(&lower.as_str())
                {
                    return None;
                }
                Value::String(lower)
            }
            _ => return None,
        };
        out.insert("device_class".into(), v);
    }
    out.insert("name".into(), json!(crate_cv_string(m.get("name")?)?));
    let ty = str_of(m, "type")?;
    Kind::parse(ty)?;
    out.insert("type".into(), json!(ty));
    out.insert(
        "unique_id".into(),
        json!(crate_cv_string(m.get("unique_id")?)?),
    );
    if let Some(u) = m.get("unit_of_measurement") {
        let v = match u {
            Value::Null => Value::Null,
            v => json!(crate_cv_string(v)?),
        };
        out.insert("unit_of_measurement".into(), v);
    }
    let state = m.get("state").cloned().unwrap_or(Value::Null);
    if !state_ok(&state) {
        return None;
    }
    out.insert("state".into(), state);
    if let Some(c) = m.get("entity_category") {
        match c {
            Value::Null => {}
            Value::String(s) if s == "config" || s == "diagnostic" => {}
            _ => return None,
        }
        out.insert("entity_category".into(), c.clone());
    }
    let icon = m.get("icon").cloned().unwrap_or(json!("mdi:cellphone"));
    if !icon_ok(&icon) {
        return None;
    }
    out.insert("icon".into(), icon);
    if let Some(sc) = m.get("state_class") {
        match sc {
            Value::Null => {}
            Value::String(s) if STATE_CLASSES.contains(&s.as_str()) => {}
            _ => return None,
        }
        if ty != "sensor" {
            return None;
        }
        out.insert("state_class".into(), sc.clone());
    }
    if let Some(d) = m.get("disabled") {
        if !d.is_boolean() {
            return None;
        }
        out.insert("disabled".into(), d.clone());
    }
    Some(out)
}

/// A sensor's native value: as the app sent it, or a parsed date or
/// datetime (from a date/timestamp sensor's string, or restored typed).
#[derive(Debug, Clone, PartialEq)]
enum Native {
    Plain(Value),
    Date(jiff::civil::Date),
    /// With its offset; `None` is naive.
    DateTime(jiff::civil::DateTime, Option<jiff::tz::Offset>),
}

const DATE_TYPE: &str = "<class 'datetime.date'>";
const DATETIME_TYPE: &str = "<class 'datetime.datetime'>";

impl Native {
    /// HA's JSON encoder for the restore store: dates and datetimes as
    /// `{"__type": "<class …>", "isoformat": …}`.
    fn dump(self) -> Value {
        match self {
            Self::Plain(v) => v,
            Self::Date(d) => json!({"__type": DATE_TYPE, "isoformat": d.to_string()}),
            Self::DateTime(dt, off) => {
                json!({"__type": DATETIME_TYPE, "isoformat": py_isoformat(dt, off)})
            }
        }
    }

    /// Python's `str()` of the value.
    fn py_str(&self) -> String {
        match self {
            Self::Plain(v) => stringify(v),
            Self::Date(d) => d.to_string(),
            Self::DateTime(dt, off) => py_isoformat(*dt, *off).replacen('T', " ", 1),
        }
    }
}

/// `datetime.isoformat()`: microseconds only when non-zero; the offset as
/// `±HH:MM`.
fn py_isoformat(dt: jiff::civil::DateTime, off: Option<jiff::tz::Offset>) -> String {
    let mut out = dt.strftime("%Y-%m-%dT%H:%M:%S").to_string();
    let micros = dt.subsec_nanosecond() / 1000;
    if micros != 0 {
        out.push_str(&format!(".{micros:06}"));
    }
    if let Some(off) = off {
        let secs = off.seconds();
        let sign = if secs < 0 { '-' } else { '+' };
        let a = secs.unsigned_abs();
        out.push_str(&format!("{sign}{:02}:{:02}", a / 3600, a / 60 % 60));
        if a % 60 != 0 {
            out.push_str(&format!(":{:02}", a % 60));
        }
    }
    out
}

/// `dt_util.parse_datetime` (`datetime.fromisoformat`): a date, a time if
/// any, an offset if any.
fn parse_datetime(s: &str) -> Option<(jiff::civil::DateTime, Option<jiff::tz::Offset>)> {
    let pieces = jiff::fmt::temporal::Pieces::parse(s).ok()?;
    let time = pieces.time().unwrap_or(jiff::civil::Time::midnight());
    Some((
        pieces.date().to_datetime(time),
        pieces.offset().map(|o| o.to_numeric_offset()),
    ))
}

/// `_calculate_native_value`: `None` for no value or `unknown`; strings
/// parsed only for `date` and `timestamp` sensors (an unparsable one
/// stays a string); typed values restored as dates and datetimes.
fn native_value(config: &Map<String, Value>) -> Option<Native> {
    let state = config.get("state").cloned().unwrap_or(Value::Null);
    if state.is_null() || state == "unknown" {
        return None;
    }
    if let Value::Object(m) = &state
        && let (Some(ty), Some(iso)) = (str_of(m, "__type"), str_of(m, "isoformat"))
    {
        if ty == DATE_TYPE
            && let Ok(d) = iso.parse::<jiff::civil::Date>()
        {
            return Some(Native::Date(d));
        }
        if ty == DATETIME_TYPE
            && let Some((dt, off)) = parse_datetime(iso)
        {
            return Some(Native::DateTime(dt, off));
        }
    }
    let dc = config.get("device_class").and_then(Value::as_str);
    if let (Some(dc @ ("date" | "timestamp")), Value::String(s)) = (dc, &state)
        && let Some((dt, off)) = parse_datetime(s)
    {
        return Some(if dc == "date" {
            Native::Date(dt.date())
        } else {
            Native::DateTime(dt, off)
        });
    }
    Some(Native::Plain(state))
}

/// The unit shown: a temperature in °C or °F follows the unit system.
fn display_unit(config: &Map<String, Value>, system_temp: &str) -> Option<Value> {
    let unit = config.get("unit_of_measurement").filter(|v| !v.is_null())?;
    if converts_temperature(config, system_temp) {
        return Some(json!(system_temp));
    }
    Some(unit.clone())
}

fn converts_temperature(config: &Map<String, Value>, system_temp: &str) -> bool {
    let unit = config.get("unit_of_measurement").and_then(Value::as_str);
    config.get("device_class").and_then(Value::as_str) == Some("temperature")
        && matches!(unit, Some("°C" | "°F"))
        && unit != Some(system_temp)
}

/// The sensor component's `state`, then `_stringify_state`.
fn sensor_state(config: &Map<String, Value>, system_temp: &str) -> Result<String, String> {
    let Some(value) = native_value(config) else {
        return Ok("unknown".into());
    };
    let dc = config
        .get("device_class")
        .and_then(Value::as_str)
        .filter(|d| SENSOR_CLASSES.contains(d));
    let unit = config.get("unit_of_measurement").filter(|v| !v.is_null());
    let state_class = config.get("state_class").and_then(Value::as_str);
    if dc.is_some_and(|d| NON_NUMERIC.contains(&d)) && unit.is_some_and(|u| u != "") {
        return Err("a unit with a non-numeric device class".into());
    }
    match (dc, &value) {
        // Only an aware datetime: anything else raises.
        (Some("timestamp" | "uptime"), Native::DateTime(dt, Some(off))) => {
            let t = off.to_timestamp(*dt).map_err(|e| e.to_string())?;
            return Ok(t.strftime("%Y-%m-%dT%H:%M:%S+00:00").to_string());
        }
        (Some("timestamp" | "uptime"), _) => return Err("not an aware datetime".into()),
        (Some("date"), Native::Date(d)) => return Ok(d.to_string()),
        (Some("date"), Native::DateTime(dt, off)) => return Ok(py_isoformat(*dt, *off)),
        (Some("date"), Native::Plain(_)) => return Err("not a date".into()),
        (Some("enum"), v) => return Ok(v.py_str()),
        _ => {}
    }
    let numeric = match dc {
        Some(d) if NON_NUMERIC.contains(&d) => false,
        _ => state_class.is_some() || unit.is_some() || dc.is_some(),
    };
    let Native::Plain(value) = value else {
        return Ok(value.py_str());
    };
    if !numeric {
        return Ok(stringify(&value));
    }
    let number = match &value {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::String(s) => py_number(s),
        _ => None,
    };
    let number = match number {
        Some(n) if n.is_finite() => n,
        Some(_) => return Err(format!("non-finite value {value}")),
        None => return Err(format!("non-numeric value {value}")),
    };
    if converts_temperature(config, system_temp) {
        // HA's TemperatureConverter, then `_stringify_state` of a float.
        let c = if system_temp == "°C" {
            (number - 32.0) / 1.8
        } else {
            number * 1.8 + 32.0
        };
        return Ok(py_format(c, 15));
    }
    if state_class == Some("measurement_angle") && unit.and_then(Value::as_str) != Some("°") {
        return Err("measurement_angle needs °".into());
    }
    Ok(stringify(&value))
}

/// Python's `int(s)` when there's no `.`/`e`, else `float(s)`: Unicode
/// whitespace stripped, Unicode decimal digits, `_` only between digits.
fn py_number(s: &str) -> Option<f64> {
    let t = s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c));
    let chars: Vec<char> = t.chars().collect();
    let mut ascii = String::with_capacity(chars.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' {
            let digit = |c: Option<&char>| c.is_some_and(|c| ha_core::decimal_value(*c).is_some());
            if !(i > 0 && digit(chars.get(i - 1)) && digit(chars.get(i + 1))) {
                return None;
            }
            continue;
        }
        match ha_core::decimal_value(c) {
            Some(d) => ascii.push(char::from_digit(d, 10)?),
            None if c.is_ascii() => ascii.push(c),
            None => return None,
        }
    }
    if !s.contains('.') && !s.contains('e') {
        let digits = ascii.strip_prefix(['+', '-']).unwrap_or(&ascii);
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            return digits.parse::<f64>().ok();
        }
    }
    // Rust's float grammar is Python's here (`inf`, `nan`, `1.`, `.5`).
    ascii.parse::<f64>().ok()
}

/// `_stringify_state`: strings as they are, floats to 15 significant
/// digits, `True`/`False`.
fn stringify(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Number(n) => match n.as_f64().filter(|_| n.is_f64()) {
            Some(f) => py_format(f, 15),
            None => n.to_string(),
        },
        v => v.to_string(),
    }
}

/// Python's `repr` of a JSON value (for HA's error messages).
fn py_repr(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("None"),
        Value::Bool(b) => out.push_str(if *b { "True" } else { "False" }),
        Value::Number(n) => match n.as_f64().filter(|_| n.is_f64()) {
            Some(f) => out.push_str(&crate::sun::py_float(f)),
            None => out.push_str(&n.to_string()),
        },
        Value::String(s) => {
            let q = if s.contains('\'') && !s.contains('"') {
                '"'
            } else {
                '\''
            };
            out.push(q);
            for c in s.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    c if c == q => {
                        out.push('\\');
                        out.push(c);
                    }
                    c => out.push(c),
                }
            }
            out.push(q);
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                py_repr(out, x);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                py_repr(out, &Value::String(k.clone()));
                out.push_str(": ");
                py_repr(out, x);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    /// Python's `int()`/`float()` on a numeric sensor's string (HA keeps
    /// the string as written when it parses).
    #[test]
    fn numeric_strings_as_python() {
        let state = |s: &str| {
            sensor_state(
                &cfg(json!({"state": s, "state_class": "measurement"})),
                "°C",
            )
        };
        assert_eq!(state("1_2").unwrap(), "1_2");
        assert_eq!(state("\u{661}\u{662}").unwrap(), "\u{661}\u{662}");
        assert_eq!(state(" 3.5 ").unwrap(), " 3.5 ");
        assert_eq!(state("1e3").unwrap(), "1e3");
        for bad in ["1__2", "_12", "12_", "1_.5", "abc", "nan", "inf", ""] {
            assert!(state(bad).is_err(), "{bad:?}");
        }
    }

    /// A temperature follows the unit system (HA's TemperatureConverter).
    #[test]
    fn temperatures_convert_to_the_unit_system() {
        let c =
            cfg(json!({"state": 77, "device_class": "temperature", "unit_of_measurement": "°F"}));
        assert_eq!(sensor_state(&c, "°C").unwrap(), "25.0");
        assert_eq!(display_unit(&c, "°C"), Some(json!("°C")));
        assert_eq!(sensor_state(&c, "°F").unwrap(), "77");
        let c = cfg(
            json!({"state": "21.5", "device_class": "temperature", "unit_of_measurement": "°C"}),
        );
        assert_eq!(sensor_state(&c, "°F").unwrap(), "70.7");
    }

    #[test]
    fn dates_and_timestamps_as_ha() {
        let date = |s: &str| sensor_state(&cfg(json!({"state": s, "device_class": "date"})), "°C");
        assert_eq!(date("20260101").unwrap(), "2026-01-01");
        assert_eq!(date("2026-11-14").unwrap(), "2026-11-14");
        assert_eq!(date("2026-11-14T23:00:00-05:00").unwrap(), "2026-11-14");
        assert!(date("soon").is_err());
        let ts =
            |s: &str| sensor_state(&cfg(json!({"state": s, "device_class": "timestamp"})), "°C");
        assert_eq!(
            ts("2026-11-14T03:00:00-05:00").unwrap(),
            "2026-11-14T08:00:00+00:00"
        );
        assert_eq!(
            ts("2026-11-14T08:00:00.000+00:00").unwrap(),
            "2026-11-14T08:00:00+00:00"
        );
        assert_eq!(
            ts("2026-11-14T08:00:00Z").unwrap(),
            "2026-11-14T08:00:00+00:00"
        );
        // Naive: HA's sensor raises.
        assert!(ts("2026-11-14T08:00:00").is_err());
        // Only date and timestamp sensors parse strings.
        let up = sensor_state(
            &cfg(json!({"state": "2026-01-01T00:00:00Z", "device_class": "uptime"})),
            "°C",
        );
        assert!(up.is_err());
    }

    /// Dumped as HA's JSON encoder writes dates and datetimes, and
    /// restored from that.
    #[test]
    fn typed_native_values_round_trip() {
        let n = native_value(&cfg(
            json!({"state": "2026-11-14T03:00:00.5-05:00", "device_class": "timestamp"}),
        ))
        .unwrap()
        .dump();
        assert_eq!(
            n,
            json!({"__type": "<class 'datetime.datetime'>", "isoformat": "2026-11-14T03:00:00.500000-05:00"})
        );
        let restored = cfg(json!({"state": n, "device_class": "timestamp"}));
        assert_eq!(
            sensor_state(&restored, "°C").unwrap(),
            "2026-11-14T08:00:00+00:00"
        );
        let d = native_value(&cfg(json!({"state": "2026-11-14", "device_class": "date"})))
            .unwrap()
            .dump();
        assert_eq!(
            d,
            json!({"__type": "<class 'datetime.date'>", "isoformat": "2026-11-14"})
        );
        let restored = cfg(json!({"state": d, "device_class": "date"}));
        assert_eq!(sensor_state(&restored, "°C").unwrap(), "2026-11-14");
        // Plain values dump as they are.
        assert_eq!(
            native_value(&cfg(json!({"state": 61}))).unwrap().dump(),
            json!(61)
        );
    }

    /// Error messages use Python's `repr` of floats.
    #[test]
    fn errors_repr_floats_as_python() {
        let m = cfg(
            json!({"state": 1, "type": "sensor", "unique_id": "x", "extra": 1.2345678901234567}),
        );
        let err = sensor_schema_full(&m).unwrap_err();
        assert_eq!(
            err,
            "extra keys not allowed @ data['extra']. Got 1.2345678901234567"
        );
    }
}

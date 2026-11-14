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
        };
        let snap = this.registry.snapshot();
        for reg in registrations {
            for row in snap.entities.iter().filter(|e| {
                e.platform == DOMAIN
                    && e.config_entry_id.as_deref() == Some(reg.entry_id.as_str())
                    && e.disabled_by.is_none()
            }) {
                let Some(kind) = row.entity_id.split('.').next().and_then(Kind::parse) else {
                    continue;
                };
                let Some(unique) = row.unique_id.as_ref().and_then(Value::as_str) else {
                    continue;
                };
                let Ok(entity_id) = EntityId::parse(&row.entity_id) else {
                    continue;
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
                this.restore(&mut sensor);
                let key = (kind, unique.to_owned());
                this.register_restore(&sensor, key.clone());
                this.write(&sensor);
                this.inner.lock().sensors.insert(key, sensor);
            }
        }
        this
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
                        let native = data.get("native_value").cloned().unwrap_or(Value::Null);
                        // A datetime is stored as {"__type", "isoformat"}.
                        let native = match &native {
                            Value::Object(m) if m.contains_key("isoformat") => {
                                m["isoformat"].clone()
                            }
                            _ => native,
                        };
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
                    let native = native_value(&s.config).ok()?;
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
                Kind::Sensor => match sensor_state(&s.config) {
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
            && let Some(u) = s.config.get("unit_of_measurement").filter(|v| !v.is_null())
        {
            a.insert("unit_of_measurement".into(), u.clone());
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
        // this entity now (spec decision D2), with the pending update.
        if enabling
            && let Some(row) = self
                .registry
                .entity_by_unique_id(kind.domain(), DOMAIN, &store_key)
            && let Ok(entity_id) = EntityId::parse(&row.entity_id)
        {
            let mut config = Map::new();
            config.insert("state".into(), Value::Null);
            config.insert("attributes".into(), json!({}));
            config.insert(
                "device_class".into(),
                json!(
                    row.device_class
                        .clone()
                        .or(row.original_device_class.clone())
                ),
            );
            config.insert("icon".into(), json!(row.original_icon));
            config.insert("unit_of_measurement".into(), json!(row.unit_of_measurement));
            let sensor = Sensor {
                entity_id,
                kind,
                name: row.original_name.clone(),
                config,
            };
            self.register_restore(&sensor, key.clone());
            self.inner.lock().sensors.insert(key.clone(), sensor);
        }
        let loaded = self.inner.lock().sensors.contains_key(&key);
        self.update(key.clone(), data.clone());
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

/// `_calculate_native_value`: `None` for no value or `unknown`; a parsed
/// date or datetime for those classes; the value otherwise.
fn native_value(config: &Map<String, Value>) -> Result<Option<Value>, String> {
    let state = config.get("state").cloned().unwrap_or(Value::Null);
    if state.is_null() || state == "unknown" {
        return Ok(None);
    }
    Ok(Some(state))
}

/// The sensor component's `state`, then `_stringify_state`.
fn sensor_state(config: &Map<String, Value>) -> Result<String, String> {
    let Some(value) = native_value(config)? else {
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
        (Some("timestamp" | "uptime"), Value::String(s)) => {
            return timestamp(s).ok_or_else(|| format!("invalid timestamp {s:?}"));
        }
        (Some("date"), Value::String(s)) => {
            return date(s).ok_or_else(|| format!("invalid date {s:?}"));
        }
        (Some("timestamp" | "uptime" | "date"), _) => {
            return Err("not a date".into());
        }
        (Some("enum"), _) => return Ok(stringify(&value)),
        _ => {}
    }
    let numeric = match dc {
        Some(d) if NON_NUMERIC.contains(&d) => false,
        _ => state_class.is_some() || unit.is_some() || dc.is_some(),
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
    match number {
        Some(n) if n.is_finite() => {}
        Some(_) => return Err(format!("non-finite value {value}")),
        None => return Err(format!("non-numeric value {value}")),
    }
    if state_class == Some("measurement_angle") && unit.and_then(Value::as_str) != Some("°") {
        return Err("measurement_angle needs °".into());
    }
    Ok(stringify(&value))
}

/// Python's `int(s)` (no `.`/`e`), else `float(s)`.
fn py_number(s: &str) -> Option<f64> {
    let t = s.trim().replace('_', "");
    if !s.contains('.') && !s.contains('e') {
        if let Ok(i) = t.parse::<i64>() {
            return Some(i as f64);
        }
    }
    match t.to_ascii_lowercase().as_str() {
        "nan" | "+nan" | "-nan" => Some(f64::NAN),
        "inf" | "+inf" | "infinity" | "+infinity" => Some(f64::INFINITY),
        "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
        _ => t.parse::<f64>().ok(),
    }
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

/// `dt_util.parse_datetime`, then UTC, `isoformat(timespec="seconds")`. A
/// naive value is an error, as HA's sensor raises.
fn timestamp(s: &str) -> Option<String> {
    let t: jiff::Timestamp = s.parse().ok()?;
    Some(t.strftime("%Y-%m-%dT%H:%M:%S+00:00").to_string())
}

/// `parse_datetime(s).date().isoformat()`: the date as written (an
/// offset doesn't move it).
fn date(s: &str) -> Option<String> {
    let parses = s.parse::<jiff::Timestamp>().is_ok()
        || s.parse::<jiff::civil::DateTime>().is_ok()
        || s.parse::<jiff::civil::Date>().is_ok();
    if !parses {
        return None;
    }
    s.get(..10)?
        .parse::<jiff::civil::Date>()
        .ok()
        .map(|d| d.to_string())
}

/// Python's `repr` of a JSON value (for HA's error messages).
fn py_repr(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("None"),
        Value::Bool(b) => out.push_str(if *b { "True" } else { "False" }),
        Value::Number(n) => out.push_str(&stringify(&Value::Number(n.clone()))),
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

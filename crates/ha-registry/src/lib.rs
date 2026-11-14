//! Home Assistant's registries (`helpers/{entity,device,area,floor,label}_registry.py`),
//! read from HA's own `.storage` files: `core.entity_registry` (1.22),
//! `core.device_registry` (1.12), `core.area_registry` (1.9),
//! `core.floor_registry` (1.3) and `core.label_registry` (1.2).
//!
//! Each entry keeps the fields `hars` doesn't use in `extra`, so a file
//! written back is the file that was read. Missing fields (older minor
//! versions) take HA's defaults. The `*_json` methods give the WebSocket
//! shapes the frontend reads.

use ha_core::storage::{Store, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::path::Path;

pub const ENTITY_KEY: &str = "core.entity_registry";
pub const DEVICE_KEY: &str = "core.device_registry";
pub const AREA_KEY: &str = "core.area_registry";
pub const FLOOR_KEY: &str = "core.floor_registry";
pub const LABEL_KEY: &str = "core.label_registry";
/// The stores, with their versions (major, minor).
pub const STORES: [(&str, u32, u32); 5] = [
    (ENTITY_KEY, 1, 22),
    (DEVICE_KEY, 1, 12),
    (AREA_KEY, 1, 9),
    (FLOOR_KEY, 1, 3),
    (LABEL_KEY, 1, 2),
];

/// `EntityCategory`, by index (`ENTITY_CATEGORY_INDEX_TO_VALUE`).
const ENTITY_CATEGORIES: [&str; 2] = ["config", "diagnostic"];

/// `datetime.isoformat()` → `datetime.timestamp()`.
fn timestamp(iso: &Option<String>) -> Value {
    iso.as_deref()
        .and_then(|s| s.parse::<jiff::Timestamp>().ok())
        .map_or(json!(0.0), |t| json!(t.as_microsecond() as f64 / 1e6))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Area {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub floor_id: Option<String>,
    #[serde(default)]
    pub humidity_entity_id: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub picture: Option<String>,
    #[serde(default)]
    pub temperature_entity_id: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub modified_at: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Area {
    /// `AreaEntry.json_fragment`.
    pub fn json(&self) -> Value {
        json!({
            "aliases": self.aliases,
            "area_id": self.id,
            "floor_id": self.floor_id,
            "humidity_entity_id": self.humidity_entity_id,
            "icon": self.icon,
            "labels": self.labels,
            "name": self.name,
            "picture": self.picture,
            "temperature_entity_id": self.temperature_entity_id,
            "created_at": timestamp(&self.created_at),
            "modified_at": timestamp(&self.modified_at),
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Floor {
    pub floor_id: String,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub level: Option<i64>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub modified_at: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Floor {
    /// `config/floor_registry.py::_entry_dict`.
    pub fn json(&self) -> Value {
        json!({
            "aliases": self.aliases,
            "created_at": timestamp(&self.created_at),
            "floor_id": self.floor_id,
            "icon": self.icon,
            "level": self.level,
            "name": self.name,
            "modified_at": timestamp(&self.modified_at),
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Label {
    pub label_id: String,
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub modified_at: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Label {
    /// `config/label_registry.py::_entry_dict`.
    pub fn json(&self) -> Value {
        json!({
            "color": self.color,
            "created_at": timestamp(&self.created_at),
            "description": self.description,
            "icon": self.icon,
            "label_id": self.label_id,
            "name": self.name,
            "modified_at": timestamp(&self.modified_at),
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    #[serde(default)]
    pub area_id: Option<String>,
    #[serde(default)]
    pub configuration_url: Option<String>,
    #[serde(default)]
    pub config_entries: Vec<String>,
    #[serde(default)]
    pub config_entries_subentries: Map<String, Value>,
    #[serde(default)]
    pub connections: Vec<Value>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub disabled_by: Option<String>,
    #[serde(default)]
    pub entry_type: Option<String>,
    #[serde(default)]
    pub hw_version: Option<String>,
    #[serde(default)]
    pub identifiers: Vec<Value>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub manufacturer: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub modified_at: Option<String>,
    #[serde(default)]
    pub name_by_user: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub primary_config_entry: Option<String>,
    #[serde(default)]
    pub serial_number: Option<String>,
    #[serde(default)]
    pub sw_version: Option<String>,
    #[serde(default)]
    pub via_device_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Device {
    /// `DeviceEntry.dict_repr`.
    pub fn json(&self) -> Value {
        json!({
            "area_id": self.area_id,
            "configuration_url": self.configuration_url,
            "config_entries": self.config_entries,
            "config_entries_subentries": self.config_entries_subentries,
            "connections": self.connections,
            "created_at": timestamp(&self.created_at),
            "disabled_by": self.disabled_by,
            "entry_type": self.entry_type,
            "hw_version": self.hw_version,
            "id": self.id,
            "identifiers": self.identifiers,
            "labels": self.labels,
            "manufacturer": self.manufacturer,
            "model": self.model,
            "model_id": self.model_id,
            "modified_at": timestamp(&self.modified_at),
            "name_by_user": self.name_by_user,
            "name": self.name,
            "primary_config_entry": self.primary_config_entry,
            "serial_number": self.serial_number,
            "sw_version": self.sw_version,
            "via_device_id": self.via_device_id,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Entity {
    pub entity_id: String,
    pub platform: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub unique_id: Option<Value>,
    #[serde(default)]
    pub area_id: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub translation_key: Option<String>,
    #[serde(default)]
    pub entity_category: Option<String>,
    #[serde(default)]
    pub hidden_by: Option<String>,
    #[serde(default)]
    pub disabled_by: Option<String>,
    #[serde(default)]
    pub has_entity_name: bool,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub original_name: Option<String>,
    #[serde(default)]
    pub options: Map<String, Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Entity {
    fn domain(&self) -> &str {
        self.entity_id.split('.').next().unwrap_or_default()
    }

    /// `RegistryEntry._as_display_dict`: what `list_for_display` sends.
    /// `device_name` is the entity's device's `name_by_user or name`.
    pub fn display_json(&self, device_name: Option<&str>) -> Value {
        let mut m = Map::new();
        m.insert("ei".into(), json!(self.entity_id));
        m.insert("pl".into(), json!(self.platform));
        if let Some(a) = &self.area_id {
            m.insert("ai".into(), json!(a));
        }
        m.insert("lb".into(), json!(self.labels));
        if let Some(d) = &self.device_id {
            m.insert("di".into(), json!(d));
        }
        if let Some(i) = &self.icon {
            m.insert("ic".into(), json!(i));
        }
        if let Some(t) = &self.translation_key {
            m.insert("tk".into(), json!(t));
        }
        if let Some(i) = self
            .entity_category
            .as_deref()
            .and_then(|c| ENTITY_CATEGORIES.iter().position(|v| *v == c))
        {
            m.insert("ec".into(), json!(i));
        }
        if self.hidden_by.is_some() {
            m.insert("hb".into(), json!(true));
        }
        if self.has_entity_name {
            m.insert("hn".into(), json!(true));
        }
        // `_unprefix_original_name`: only for legacy-named device entities.
        let unprefixed = (!self.has_entity_name && self.device_id.is_some())
            .then(|| strip_prefix(self.original_name.as_deref()?, device_name?))
            .flatten();
        if let Some(name) = self
            .name
            .clone()
            .or(unprefixed)
            .or_else(|| self.original_name.clone())
        {
            m.insert("en".into(), json!(name));
        }
        let sensor = self.options.get("sensor").and_then(Value::as_object);
        if let Some(sensor) = sensor.filter(|_| self.domain() == "sensor") {
            let precision = [
                sensor.get("display_precision"),
                sensor.get("suggested_display_precision"),
            ]
            .into_iter()
            .flatten()
            .find(|v| !v.is_null());
            if let Some(p) = precision {
                m.insert("dp".into(), p.clone());
            }
        }
        Value::Object(m)
    }
}

/// `_async_strip_prefix_from_entity_name`: "Kitchen Light Power" with the
/// device "Kitchen Light" becomes "Power". `None` if the prefix doesn't
/// meaningfully match.
fn strip_prefix(entity_name: &str, prefix: &str) -> Option<String> {
    use ha_core::casefold::casefold;
    if entity_name.is_empty() || prefix.is_empty() {
        return None;
    }
    let prefix_lower = casefold(prefix);
    let prefix_len = prefix_lower.chars().count();
    let chars: Vec<char> = entity_name.chars().collect();
    let candidate: String = chars.iter().take(prefix_len).collect();
    let mut true_prefix_len = candidate.chars().count();
    let candidate = casefold(&candidate);
    if !candidate.starts_with(&prefix_lower) {
        return None;
    }
    // Case folding can change the length; walk back to a character boundary.
    let mut prefix_diff = candidate.chars().count() as isize - prefix_len as isize;
    while prefix_diff > 0 {
        true_prefix_len -= 1;
        prefix_diff -= casefold(&chars[true_prefix_len].to_string())
            .chars()
            .count() as isize;
    }
    if prefix_diff < 0 {
        return None;
    }
    let rest: String = chars[true_prefix_len..].iter().collect();
    let new_name = rest.trim_start_matches([' ', '-', ':']);
    if new_name.is_empty() {
        return Some(String::new());
    }
    // There must be a separator.
    if new_name.chars().count() == chars.len() - true_prefix_len {
        return None;
    }
    let first_word = new_name.split(' ').next().unwrap_or_default();
    if !is_lower(first_word) {
        return Some(new_name.to_owned());
    }
    let mut c = new_name.chars();
    let first = c.next().expect("not empty");
    Some(first.to_uppercase().chain(c).collect())
}

/// Python's `str.islower()`: some cased character, and none uppercase.
fn is_lower(s: &str) -> bool {
    let mut cased = false;
    for c in s.chars() {
        if c.is_uppercase()
            || (c.is_alphabetic() && !c.is_lowercase() && c.to_lowercase().ne(c.to_uppercase()))
        {
            return false;
        }
        cased |= c.is_lowercase();
    }
    cased
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct EntityData {
    #[serde(default)]
    entities: Vec<Entity>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DeviceData {
    #[serde(default)]
    devices: Vec<Device>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AreaData {
    #[serde(default)]
    areas: Vec<Area>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FloorData {
    #[serde(default)]
    floors: Vec<Floor>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LabelData {
    #[serde(default)]
    labels: Vec<Label>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// All five registries, as loaded.
#[derive(Debug, Clone, Default)]
pub struct Registries {
    pub entities: Vec<Entity>,
    pub devices: Vec<Device>,
    pub areas: Vec<Area>,
    pub floors: Vec<Floor>,
    pub labels: Vec<Label>,
}

fn store(dir: &Path, key: &'static str) -> Store {
    let &(_, major, minor) = STORES
        .iter()
        .find(|(k, _, _)| *k == key)
        .expect("known store");
    Store::new(dir, key, major, minor, false)
}

impl Registries {
    /// Read the registries for the server (blocking). A missing file is an
    /// empty registry; a corrupt one is moved to `<file>.corrupt.<time>` and
    /// starts empty, as HA's `Store` does. A version `hars` can't read is an
    /// error.
    pub fn load(config_dir: &Path) -> Result<Self, StoreError> {
        Self::load_with(config_dir, true)
    }

    /// Like [`Registries::load`], but any problem is an error and nothing is
    /// moved: for checking a copy before importing it.
    pub fn load_strict(config_dir: &Path) -> Result<Self, StoreError> {
        Self::load_with(config_dir, false)
    }

    fn load_with(config_dir: &Path, quarantine: bool) -> Result<Self, StoreError> {
        fn read<T: serde::de::DeserializeOwned + Default>(
            store: Store,
            quarantine: bool,
        ) -> Result<T, StoreError> {
            match store.load::<T>() {
                Ok(data) => Ok(data.unwrap_or_default()),
                Err(StoreError::Json { path, .. }) if quarantine => {
                    let aside = format!(
                        "{}.corrupt.{}",
                        path.display(),
                        jiff::Timestamp::now().as_second()
                    );
                    tracing::error!("{} is not valid JSON; moved to {aside}", path.display());
                    if let Err(e) = std::fs::rename(&path, &aside) {
                        tracing::error!("could not move {}: {e}", path.display());
                    }
                    Ok(T::default())
                }
                Err(e) => Err(e),
            }
        }
        let entities: EntityData = read(store(config_dir, ENTITY_KEY), quarantine)?;
        let devices: DeviceData = read(store(config_dir, DEVICE_KEY), quarantine)?;
        let areas: AreaData = read(store(config_dir, AREA_KEY), quarantine)?;
        let floors: FloorData = read(store(config_dir, FLOOR_KEY), quarantine)?;
        let labels: LabelData = read(store(config_dir, LABEL_KEY), quarantine)?;
        Ok(Self {
            entities: entities.entities,
            devices: devices.devices,
            areas: areas.areas,
            floors: floors.floors,
            labels: labels.labels,
        })
    }

    /// `config/entity_registry/list_for_display`: enabled entities only.
    pub fn entities_for_display(&self) -> Value {
        let device_name = |id: &str| {
            self.devices
                .iter()
                .find(|d| d.id == id)
                .and_then(|d| d.name_by_user.as_deref().or(d.name.as_deref()))
        };
        let entities: Vec<Value> = self
            .entities
            .iter()
            .filter(|e| e.disabled_by.is_none())
            .map(|e| e.display_json(e.device_id.as_deref().and_then(device_name)))
            .collect();
        json!({
            "entity_categories": {"0": ENTITY_CATEGORIES[0], "1": ENTITY_CATEGORIES[1]},
            "entities": entities,
        })
    }

    pub fn devices_json(&self) -> Value {
        Value::Array(self.devices.iter().map(Device::json).collect())
    }

    pub fn areas_json(&self) -> Value {
        Value::Array(self.areas.iter().map(Area::json).collect())
    }

    pub fn floors_json(&self) -> Value {
        Value::Array(self.floors.iter().map(Floor::json).collect())
    }

    pub fn labels_json(&self) -> Value {
        Value::Array(self.labels.iter().map(Label::json).collect())
    }
}

#[cfg(test)]
mod tests;

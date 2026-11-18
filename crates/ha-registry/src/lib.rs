//! Home Assistant's registries (`helpers/{entity,device,area,floor,label}_registry.py`),
//! read from HA's own `.storage` files: `core.entity_registry` (1.22),
//! `core.device_registry` (1.12), `core.area_registry` (1.9),
//! `core.floor_registry` (1.3) and `core.label_registry` (1.2).
//!
//! Each entry keeps the fields `hars` doesn't use in `extra`, so a file
//! written back is the file that was read. Missing fields (older minor
//! versions) take HA's defaults. The `*_json` methods give the WebSocket
//! shapes the frontend reads.

mod live;
pub use live::{DeviceInfo, EntityPatch, EntryInfo, EntryLookup, Registry, available_entity_id};

use ha_core::storage::{Store, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// `normalized_name_base_registry.normalize_name`.
pub fn normalize_name(name: &str) -> String {
    ha_core::casefold::casefold(name).replace(' ', "")
}

/// `vec![...]` of rows, each in an `Arc`.
#[macro_export]
macro_rules! rows {
    ($($row:expr),* $(,)?) => { vec![$(::std::sync::Arc::new($row)),*] };
}

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

/// `null` reads as the empty collection, as HA's loaders treat it (e.g.
/// `_protect_entity_options(None)`).
fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

fn zero() -> Value {
    json!(0)
}

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
    #[serde(default, deserialize_with = "null_as_default")]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub floor_id: Option<String>,
    #[serde(default)]
    pub humidity_entity_id: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
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
    #[serde(default, deserialize_with = "null_as_default")]
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

/// A device row, its fields in HA's storage order (`DeviceEntry.as_storage_fragment`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Device {
    #[serde(default)]
    pub area_id: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub config_entries: Vec<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub config_entries_subentries: Map<String, Value>,
    #[serde(default)]
    pub configuration_url: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub connections: Vec<Value>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub disabled_by: Option<String>,
    #[serde(default)]
    pub entry_type: Option<String>,
    #[serde(default)]
    pub hw_version: Option<String>,
    pub id: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub identifiers: Vec<Value>,
    #[serde(default, deserialize_with = "null_as_default")]
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
    /// `name_by_user or name`, with Python's `or`: an empty name doesn't count.
    pub fn display_name(&self) -> Option<&str> {
        self.name_by_user
            .as_deref()
            .filter(|n| !n.is_empty())
            .or(self.name.as_deref().filter(|n| !n.is_empty()))
    }

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

/// An entity row, its fields in HA's storage order
/// (`RegistryEntry.as_storage_fragment`); keys `hars` doesn't know are kept
/// in `extra`, written last.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Entity {
    #[serde(default, deserialize_with = "null_as_default")]
    pub aliases: Vec<Value>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub aliases_v2: Vec<Value>,
    #[serde(default)]
    pub area_id: Option<String>,
    /// Kept as written, `null` included (HA doesn't normalise it).
    #[serde(default)]
    pub categories: RawJson,
    /// `{"state_class": …}` and the like, or `null`.
    #[serde(default)]
    pub capabilities: Option<RawJson>,
    #[serde(default)]
    pub config_entry_id: Option<String>,
    #[serde(default)]
    pub config_subentry_id: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    /// The user's override of the integration's `original_device_class`.
    #[serde(default)]
    pub device_class: Option<String>,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub disabled_by: Option<String>,
    #[serde(default)]
    pub entity_category: Option<String>,
    pub entity_id: String,
    #[serde(default)]
    pub hidden_by: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub has_entity_name: bool,
    #[serde(default, deserialize_with = "null_as_default")]
    pub labels: Vec<String>,
    #[serde(default)]
    pub modified_at: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub object_id_base: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub options: RawJson,
    #[serde(default)]
    pub original_device_class: Option<String>,
    #[serde(default)]
    pub original_icon: Option<String>,
    #[serde(default)]
    pub original_name: Option<String>,
    pub platform: String,
    #[serde(default)]
    pub suggested_object_id: Option<String>,
    #[serde(default = "zero")]
    pub supported_features: Value,
    #[serde(default)]
    pub translation_key: Option<String>,
    #[serde(default)]
    pub unique_id: Option<Value>,
    #[serde(default)]
    pub previous_unique_id: Option<Value>,
    #[serde(default)]
    pub unit_of_measurement: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Entity {
    /// What a registered entity nobody provides shows
    /// (`RegistryEntry.write_unavailable_state`): `restored`, then what
    /// the registry knows of it. Its state is `unavailable`.
    pub fn unavailable_attributes(&self) -> Map<String, Value> {
        let mut a = Map::new();
        a.insert("restored".into(), Value::Bool(true));
        if let Some(caps) = &self.capabilities {
            a.extend(caps.object());
        }
        // Python's `or`: an empty override falls back too.
        let or = |a: &Option<String>, b: &Option<String>| {
            a.clone().filter(|s| !s.is_empty()).or_else(|| b.clone())
        };
        if let Some(dc) = or(&self.device_class, &self.original_device_class) {
            a.insert("device_class".into(), json!(dc));
        }
        if let Some(icon) = or(&self.icon, &self.original_icon) {
            a.insert("icon".into(), json!(icon));
        }
        if let Some(name) = or(&self.name, &self.original_name) {
            a.insert("friendly_name".into(), Value::String(name));
        }
        // `is not None`: 0 is written too.
        if !self.supported_features.is_null() {
            a.insert("supported_features".into(), self.supported_features.clone());
        }
        if let Some(u) = &self.unit_of_measurement {
            a.insert("unit_of_measurement".into(), json!(u));
        }
        a
    }

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
        let options = (self.domain() == "sensor").then(|| self.options.object());
        let sensor = options
            .as_ref()
            .and_then(|o| o.get("sensor"))
            .and_then(Value::as_object);
        if let Some(sensor) = sensor {
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
pub fn strip_prefix(entity_name: &str, prefix: &str) -> Option<String> {
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

/// A JSON value kept as the file wrote it, so key order inside survives a
/// write (HA's insertion order; `serde_json::Map` would sort it). Parsed
/// when read.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RawJson(Box<serde_json::value::RawValue>);

impl RawJson {
    pub fn new(v: &Value) -> Self {
        Self(serde_json::value::to_raw_value(v).expect("a Value serialises"))
    }

    pub fn value(&self) -> Value {
        serde_json::from_str(self.0.get()).unwrap_or(Value::Null)
    }

    /// The value as an object (`{}` if it isn't one).
    pub fn object(&self) -> Map<String, Value> {
        match self.value() {
            Value::Object(m) => m,
            _ => Map::new(),
        }
    }

    pub fn get(&self) -> &str {
        self.0.get()
    }
}

impl Default for RawJson {
    /// `{}`.
    fn default() -> Self {
        Self(serde_json::value::RawValue::from_string("{}".into()).expect("valid JSON"))
    }
}

impl PartialEq for RawJson {
    fn eq(&self, other: &Self) -> bool {
        self.value() == other.value()
    }
}

/// A row kept as the file had it (deleted entities and devices: `hars`
/// doesn't change them yet).
pub type RawRow = Box<serde_json::value::RawValue>;

#[derive(Debug, Default, Deserialize)]
struct EntityData {
    #[serde(default)]
    entities: Vec<Arc<Entity>>,
    #[serde(default)]
    deleted_entities: Vec<RawRow>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Debug, Default, Deserialize)]
struct DeviceData {
    #[serde(default)]
    devices: Vec<Arc<Device>>,
    #[serde(default)]
    deleted_devices: Vec<RawRow>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// What a registry file holds besides its rows, for writing it back.
#[derive(Debug, Clone, Default)]
pub struct FileRest {
    /// The file's `minor_version` (0 when there was no file).
    pub minor: u32,
    pub deleted: Arc<Vec<RawRow>>,
    pub extra: Map<String, Value>,
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

/// All five registries. Rows are shared, so a copy with one row changed
/// costs a pointer per row.
#[derive(Debug, Clone, Default)]
pub struct Registries {
    pub entities: Vec<Arc<Entity>>,
    pub devices: Vec<Arc<Device>>,
    /// The entity and device files' deleted rows and other keys.
    pub entity_file: FileRest,
    pub device_file: FileRest,
    pub areas: Vec<Area>,
    /// The area file's `minor_version` (the current one when there was no
    /// file).
    pub area_minor: u32,
    pub floors: Vec<Floor>,
    pub labels: Vec<Label>,
}

fn store_minor(store: &Store) -> u32 {
    let key = store
        .path()
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    STORES
        .iter()
        .find(|(k, _, _)| *k == key)
        .map_or(1, |&(_, _, minor)| minor)
}

/// Each row as compact JSON (HA's `json_fragment`s).
fn raw_rows<T: Serialize>(rows: &[Arc<T>]) -> std::io::Result<Vec<RawRow>> {
    rows.iter()
        .map(|r| serde_json::value::to_raw_value(&**r).map_err(std::io::Error::other))
        .collect()
}

/// `hars` writes only the current minor version (or a new file).
fn check_minor(store: &Store, loaded: u32) -> std::io::Result<()> {
    let current = store_minor(store);
    if store.path().exists() && loaded != current {
        return Err(std::io::Error::other(format!(
            "{} is at minor version {loaded}, not {current}; not overwriting it",
            store.path().display()
        )));
    }
    Ok(())
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
        ) -> Result<(T, u32), StoreError> {
            let current = store_minor(&store);
            match store.load_with_minor::<T>() {
                Ok(Some(loaded)) => Ok(loaded),
                Ok(None) => Ok((T::default(), current)),
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
                    Ok((T::default(), current))
                }
                Err(e) => Err(e),
            }
        }
        let (entities, entity_minor): (EntityData, _) =
            read(store(config_dir, ENTITY_KEY), quarantine)?;
        let (devices, device_minor): (DeviceData, _) =
            read(store(config_dir, DEVICE_KEY), quarantine)?;
        let (areas, area_minor): (AreaData, _) = read(store(config_dir, AREA_KEY), quarantine)?;
        let (floors, floor_minor): (FloorData, _) = read(store(config_dir, FLOOR_KEY), quarantine)?;
        let (labels, _): (LabelData, _) = read(store(config_dir, LABEL_KEY), quarantine)?;
        let mut r = Self {
            entities: entities.entities,
            devices: devices.devices,
            entity_file: FileRest {
                minor: entity_minor,
                deleted: Arc::new(entities.deleted_entities),
                extra: entities.extra,
            },
            device_file: FileRest {
                minor: device_minor,
                deleted: Arc::new(devices.deleted_devices),
                extra: devices.extra,
            },
            areas: areas.areas,
            area_minor,
            floors: floors.floors,
            labels: labels.labels,
        };
        r.migrate(entity_minor, device_minor, area_minor, floor_minor);
        Ok(r)
    }

    /// Write `core.entity_registry` as HA does: each row compact, on its
    /// own line, in HA's field order, inside a 2-space indented envelope.
    /// A file at an older minor version is never overwritten: HA would
    /// skip the migrations that were never run on it.
    pub fn save_entities(&self, config_dir: &Path) -> std::io::Result<()> {
        #[derive(Serialize)]
        struct Out<'a> {
            entities: Vec<RawRow>,
            deleted_entities: &'a [RawRow],
            #[serde(flatten)]
            extra: &'a Map<String, Value>,
        }
        let store = store(config_dir, ENTITY_KEY);
        check_minor(&store, self.entity_file.minor)?;
        store.save(&Out {
            entities: raw_rows(&self.entities)?,
            deleted_entities: &self.entity_file.deleted,
            extra: &self.entity_file.extra,
        })
    }

    /// Write `core.device_registry`, as [`Registries::save_entities`].
    pub fn save_devices(&self, config_dir: &Path) -> std::io::Result<()> {
        #[derive(Serialize)]
        struct Out<'a> {
            devices: Vec<RawRow>,
            deleted_devices: &'a [RawRow],
            #[serde(flatten)]
            extra: &'a Map<String, Value>,
        }
        let store = store(config_dir, DEVICE_KEY);
        check_minor(&store, self.device_file.minor)?;
        store.save(&Out {
            devices: raw_rows(&self.devices)?,
            deleted_devices: &self.device_file.deleted,
            extra: &self.device_file.extra,
        })
    }

    /// Write `core.area_registry` as HA's `_data_to_save` does: plain
    /// indented JSON, each area's keys in HA's order. A file at an older
    /// minor version is never overwritten, as for the entities.
    pub fn save_areas(&self, config_dir: &Path) -> std::io::Result<()> {
        #[derive(Serialize)]
        struct Row<'a> {
            aliases: &'a [String],
            floor_id: &'a Option<String>,
            humidity_entity_id: &'a Option<String>,
            icon: &'a Option<String>,
            id: &'a str,
            labels: &'a [String],
            name: &'a str,
            picture: &'a Option<String>,
            temperature_entity_id: &'a Option<String>,
            created_at: &'a Option<String>,
            modified_at: &'a Option<String>,
        }
        #[derive(Serialize)]
        struct Out<'a> {
            areas: Vec<Row<'a>>,
        }
        let store = store(config_dir, AREA_KEY);
        check_minor(&store, self.area_minor)?;
        store.save(&Out {
            areas: self
                .areas
                .iter()
                .map(|a| Row {
                    aliases: &a.aliases,
                    floor_id: &a.floor_id,
                    humidity_entity_id: &a.humidity_entity_id,
                    icon: &a.icon,
                    id: &a.id,
                    labels: &a.labels,
                    name: &a.name,
                    picture: &a.picture,
                    temperature_entity_id: &a.temperature_entity_id,
                    created_at: &a.created_at,
                    modified_at: &a.modified_at,
                })
                .collect(),
        })
    }

    /// `async_get_area_by_name`: names compare case-folded, without spaces
    /// (`normalize_name`).
    pub fn area_by_name(&self, name: &str) -> Option<&Area> {
        let wanted = normalize_name(name);
        self.areas
            .iter()
            .find(|a| normalize_name(&a.name) == wanted)
    }

    /// The parts of HA's minor-version migrations that change what `hars`
    /// serves. HA only rewrites a registry file when it changes, so files
    /// still at an older minor version are normal.
    fn migrate(&mut self, entity_minor: u32, device_minor: u32, area_minor: u32, floor_minor: u32) {
        use ha_core::casefold::casefold;
        // Device 1.9 (2025.2) adds subentries: each config entry maps to [null].
        if device_minor < 9 {
            for d in &mut self.devices {
                let d = Arc::make_mut(d);
                d.config_entries_subentries = d
                    .config_entries
                    .iter()
                    .map(|e| (e.clone(), json!([null])))
                    .collect();
            }
        }
        // Area 1.9 sorts areas by name.
        if area_minor < 9 {
            self.areas.sort_by_cached_key(|a| casefold(&a.name));
        }
        // Floor 1.3 sorts by level (highest first, none last), then name.
        if floor_minor < 3 {
            self.floors.sort_by_cached_key(|f| {
                (
                    f.level.map_or(i128::MAX, |l| -(l as i128)),
                    casefold(&f.name),
                )
            });
        }
        // Entity 1.21 was reverted: names come back from `name_v2`, with the
        // device name in front when the user renamed it since.
        if entity_minor == 21 {
            let device_names: HashMap<&str, String> = self
                .devices
                .iter()
                .filter_map(|d| Some((d.id.as_str(), d.display_name()?.to_owned())))
                .collect();
            for e in &mut self.entities {
                let e = Arc::make_mut(e);
                let old = e.name.take();
                let v2 = match e.extra.remove("name_v2") {
                    Some(Value::String(s)) => Some(s),
                    _ => None,
                };
                let device = e.device_id.as_deref().and_then(|id| device_names.get(id));
                e.name = match (&v2, device) {
                    (Some(n), Some(device)) if v2 != old => Some(format!("{device} {n}")),
                    _ => v2,
                };
            }
        }
    }

    /// The entities a service target's `device_id`, `area_id`, `floor_id`
    /// and `label_id` reach, as HA's `async_extract_referenced_entity_ids`
    /// (primary entities only): hidden entities and config/diagnostic ones
    /// are left out of device and area expansion, and an entity reached
    /// through its device's area only when it has no area of its own.
    /// `entity_id` itself is not resolved here.
    pub fn target_entities(&self, target: &Map<String, Value>) -> Vec<String> {
        use std::collections::BTreeSet;
        let ids = |key: &str| -> BTreeSet<&str> {
            match target.get(key) {
                Some(Value::String(s)) if s != "none" => BTreeSet::from([s.as_str()]),
                Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
                _ => BTreeSet::new(),
            }
        };
        let (floors, labels) = (ids("floor_id"), ids("label_id"));
        let mut areas = ids("area_id");
        let mut devices = ids("device_id");
        let mut out = BTreeSet::new();
        let enabled = |e: &&Arc<Entity>| e.disabled_by.is_none();
        let primary = |e: &Entity| e.hidden_by.is_none() && e.entity_category.is_none();
        for label in &labels {
            let has = |l: &Vec<String>| l.iter().any(|x| x == label);
            out.extend(
                self.entities
                    .iter()
                    .filter(|e| has(&e.labels) && e.hidden_by.is_none())
                    .map(|e| e.entity_id.as_str()),
            );
            devices.extend(
                self.devices
                    .iter()
                    .filter(|d| has(&d.labels))
                    .map(|d| d.id.as_str()),
            );
            areas.extend(
                self.areas
                    .iter()
                    .filter(|a| has(&a.labels))
                    .map(|a| a.id.as_str()),
            );
        }
        areas.extend(
            self.areas
                .iter()
                .filter(|a| a.floor_id.as_deref().is_some_and(|f| floors.contains(f)))
                .map(|a| a.id.as_str()),
        );
        out.extend(
            self.entities
                .iter()
                .filter(enabled)
                .filter(|e| e.device_id.as_deref().is_some_and(|d| devices.contains(d)))
                .filter(|e| primary(e))
                .map(|e| e.entity_id.as_str()),
        );
        let area_devices: BTreeSet<&str> = self
            .devices
            .iter()
            .filter(|d| d.area_id.as_deref().is_some_and(|a| areas.contains(a)))
            .map(|d| d.id.as_str())
            .collect();
        out.extend(
            self.entities
                .iter()
                .filter(|e| primary(e))
                .filter(|e| match e.area_id.as_deref() {
                    Some(a) => areas.contains(a),
                    None => {
                        enabled(e)
                            && e.device_id
                                .as_deref()
                                .is_some_and(|d| area_devices.contains(d))
                    }
                })
                .map(|e| e.entity_id.as_str()),
        );
        out.into_iter().map(str::to_owned).collect()
    }

    /// `display_precision`, else `suggested_display_precision`, of a
    /// sensor's entry (`sensor.async_rounded_state`'s `_display_precision`).
    pub fn sensor_display_precision(&self, entity_id: &str) -> Option<i64> {
        let entry = self.entities.iter().find(|e| e.entity_id == entity_id)?;
        let options = entry.options.object();
        let sensor = options.get("sensor")?.as_object()?;
        match sensor.get("display_precision") {
            Some(p) if !p.is_null() => p.as_i64(),
            _ => sensor.get("suggested_display_precision")?.as_i64(),
        }
    }

    /// HA's `_write_unavailable_states` at start: every enabled entity with
    /// no state yet gets `unavailable`, marked `restored`, with what the
    /// registry knows of it (`RegistryEntry.write_unavailable_state`).
    pub fn write_unavailable_states(&self, hass: &ha_core::Hass) {
        for e in self.entities.iter().filter(|e| e.disabled_by.is_none()) {
            let Ok(id) = ha_core::EntityId::parse(&e.entity_id) else {
                continue;
            };
            if hass.states().get(&id).is_some() {
                continue;
            }
            // Written only if still absent (an integration may write first).
            hass.set_state_if_absent(id, "unavailable", e.unavailable_attributes());
        }
    }

    /// `config/entity_registry/list_for_display`: enabled entities only.
    pub fn entities_for_display(&self) -> Value {
        // Indexed once per call: linear in entities + devices.
        let devices: HashMap<&str, &Device> =
            self.devices.iter().map(|d| (d.id.as_str(), &**d)).collect();
        let device_name = |id: &str| devices.get(id).and_then(|d| d.display_name());
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
        Value::Array(self.devices.iter().map(|d| d.json()).collect())
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

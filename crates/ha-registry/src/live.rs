//! The entity and device registries at runtime: what integrations add or
//! change while `hars` runs (the companion app's sensors and device), with
//! HA's events and delayed saves (`helpers/entity_registry.py`,
//! `helpers/device_registry.py`).
//!
//! Readers take a [`Registry::snapshot`] (an `Arc` clone) and work without
//! a lock. A writer copies the row list (a pointer per row), replaces the
//! rows it changes, and swaps the snapshot in; writers are serialised.

use crate::{Device, Entity, RawJson, RawRow, Registries, strip_prefix};
use ha_core::storage::DelayedSave;
use ha_core::{Context, CoreState, Event, EventKind, Hass};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// `helpers/registry.py`: a save 10 s after the last change while
/// running, 180 s before that.
const SAVE_DELAY: Duration = Duration::from_secs(10);
const SAVE_DELAY_STARTING: Duration = Duration::from_secs(180);
/// `MAX_LENGTH_STATE_ENTITY_ID`.
const MAX_ENTITY_ID: usize = 255;

pub struct Registry {
    hass: Hass,
    snap: RwLock<Arc<Registries>>,
    /// One writer at a time, so no change is lost between copy and swap.
    writer: Mutex<()>,
    saves: Option<[Arc<DelayedSave>; 2]>,
    entries: RwLock<Option<EntryLookup>>,
}

/// What an integration wants a row to have (HA's keyword arguments to
/// `async_get_or_create` / `async_update_entity`): `None` leaves a field
/// as it is, `Some(None)` clears it.
#[derive(Debug, Clone, Default)]
pub struct EntityPatch {
    pub capabilities: Option<Option<Value>>,
    pub config_entry_id: Option<Option<String>>,
    pub device_class: Option<Option<String>>,
    pub device_id: Option<Option<String>>,
    pub disabled_by: Option<Option<String>>,
    pub entity_category: Option<Option<String>>,
    pub has_entity_name: Option<bool>,
    pub object_id_base: Option<Option<String>>,
    pub options: Option<Value>,
    pub original_device_class: Option<Option<String>>,
    pub original_icon: Option<Option<String>>,
    pub original_name: Option<Option<String>>,
    pub supported_features: Option<i64>,
    pub translation_key: Option<Option<String>>,
    pub unit_of_measurement: Option<Option<String>>,
}

/// A device as an integration describes it (`async_get_or_create`'s
/// arguments, as `mobile_app` passes them).
#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    pub config_entry_id: String,
    pub identifiers: Vec<Value>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub name: Option<String>,
    pub sw_version: Option<String>,
}

fn now_iso(hass: &Hass) -> String {
    ha_core::isoformat(hass.clock().now())
}

/// `random_uuid_hex`: 128 random bits as hex (not a version-4 UUID).
fn random_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

/// One write: a copy of the registries, and what to publish after it is
/// swapped in.
struct Tx {
    r: Registries,
    events: Vec<(&'static str, Value)>,
    save_entities: bool,
    save_devices: bool,
}

impl Tx {
    fn entity_event(&mut self, data: Value) {
        self.events.push(("entity_registry_updated", data));
        self.save_entities = true;
    }

    fn device_event(&mut self, data: Value) {
        self.events.push(("device_registry_updated", data));
        self.save_devices = true;
    }
}

/// What the registries need to know of a config entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryInfo {
    pub domain: String,
    pub disabled: bool,
}

/// A config entry by id, if it exists.
pub type EntryLookup = Arc<dyn Fn(&str) -> Option<EntryInfo> + Send + Sync>;

/// `LOW_PRIO_CONFIG_ENTRY_DOMAINS`: their entries yield a device's primary
/// entry to another integration's.
const LOW_PRIO_DOMAINS: [&str; 4] = ["homekit_controller", "matter", "mqtt", "upnp"];

impl Registry {
    /// Kept in memory only (tests, and checking a copy).
    pub fn in_memory(hass: &Hass, registries: Registries) -> Arc<Self> {
        Arc::new(Self {
            hass: hass.clone(),
            snap: RwLock::new(Arc::new(registries)),
            writer: Mutex::new(()),
            saves: None,
            entries: RwLock::new(None),
        })
    }

    /// Saved to `config_dir/.storage` after changes, as HA does.
    pub fn new(hass: &Hass, config_dir: &Path, registries: Registries) -> Arc<Self> {
        Arc::new_cyclic(|weak: &std::sync::Weak<Self>| {
            let save = |entities: bool| {
                let (weak, dir) = (weak.clone(), config_dir.to_owned());
                let key = if entities {
                    crate::ENTITY_KEY
                } else {
                    crate::DEVICE_KEY
                };
                DelayedSave::spawn(key, move || {
                    let Some(this) = weak.upgrade() else {
                        return Ok(());
                    };
                    let snap = this.snapshot();
                    if entities {
                        snap.save_entities(&dir)
                    } else {
                        snap.save_devices(&dir)
                    }
                })
            };
            Self {
                hass: hass.clone(),
                snap: RwLock::new(Arc::new(registries)),
                writer: Mutex::new(()),
                saves: Some([save(true), save(false)]),
                entries: RwLock::new(None),
            }
        })
    }

    /// How to look up a config entry (a device's primary entry; disables
    /// that follow the entry). Without it, an existing primary entry is
    /// never replaced and entries count as enabled.
    pub fn set_entry_lookup(&self, f: EntryLookup) {
        *self.entries.write() = Some(f);
    }

    fn entry(&self, id: &str) -> Option<EntryInfo> {
        let lookup = self.entries.read().clone()?;
        lookup(id)
    }

    fn entry_disabled(&self, id: Option<&str>) -> bool {
        id.and_then(|id| self.entry(id)).is_some_and(|e| e.disabled)
    }

    /// The registries as they are now.
    pub fn snapshot(&self) -> Arc<Registries> {
        self.snap.read().clone()
    }

    /// Write whatever is pending, and wait for a write in progress (at stop).
    pub async fn flush(&self) {
        if let Some(saves) = &self.saves {
            for s in saves {
                s.flush().await;
            }
        }
    }

    fn schedule_save(&self, entities: bool) {
        if let Some(saves) = &self.saves {
            let delay = if self.hass.core_state() == CoreState::Running {
                SAVE_DELAY
            } else {
                SAVE_DELAY_STARTING
            };
            saves[usize::from(!entities)].schedule(delay);
        }
    }

    /// One write, start to finish under the writer lock: read and change a
    /// copy, swap it in, then publish its events and schedule the saves, so
    /// lookups can't race creation and events come in commit order.
    fn write<T>(&self, f: impl FnOnce(&mut Tx) -> T) -> T {
        let _turn = self.writer.lock();
        let mut tx = Tx {
            r: (*self.snapshot()).clone(),
            events: Vec::new(),
            save_entities: false,
            save_devices: false,
        };
        let out = f(&mut tx);
        *self.snap.write() = Arc::new(tx.r);
        if tx.save_entities {
            self.schedule_save(true);
        }
        if tx.save_devices {
            self.schedule_save(false);
        }
        let now = self.hass.clock().now();
        for (name, data) in tx.events {
            self.hass.bus().fire(Event::new(
                EventKind::Custom {
                    name: name.into(),
                    data,
                },
                now,
                Context::at(now),
            ));
        }
        out
    }

    /// The row for `(domain, platform, unique_id)`.
    pub fn entity_by_unique_id(
        &self,
        domain: &str,
        platform: &str,
        unique_id: &str,
    ) -> Option<Arc<Entity>> {
        find_unique(&self.snapshot(), domain, platform, unique_id).cloned()
    }

    /// `async_get_or_create`: the existing row, updated with `patch`, or a
    /// new one (`disabled_by` applies only to a new row). Fires
    /// `entity_registry_updated` and schedules the save.
    pub fn get_or_create_entity(
        &self,
        domain: &str,
        platform: &str,
        unique_id: &str,
        disabled_by: Option<String>,
        patch: EntityPatch,
    ) -> Arc<Entity> {
        let now = now_iso(&self.hass);
        self.write(|tx| {
            if let Some(i) =
                tx.r.entities
                    .iter()
                    .position(|e| is_unique(e, domain, platform, unique_id))
            {
                update_row(tx, i, &patch, &now);
                return tx.r.entities[i].clone();
            }
            let object_id_base = patch.object_id_base.clone().flatten();
            let device_id = patch.device_id.clone().flatten();
            // A deleted row for the same unique id comes back: its id,
            // creation time and the user's settings (HA's `deleted_entities`).
            let key = (domain.to_owned(), platform.to_owned(), json!(unique_id));
            let deleted =
                tx.r.entity_file
                    .deleted
                    .iter()
                    .position(|d| DeletedEntity::parse(d).map(|d| d.key()).as_ref() == Some(&key));
            let deleted = deleted.and_then(|i| {
                let mut rows = (*tx.r.entity_file.deleted).clone();
                let row = DeletedEntity::parse(&rows.remove(i));
                tx.r.entity_file.deleted = Arc::new(rows);
                row
            });
            let available = |id: &str| {
                !tx.r.entities.iter().any(|e| e.entity_id == id)
                    && ha_core::EntityId::parse(id)
                        .ok()
                        .is_none_or(|id| self.hass.states().get(&id).is_none())
            };
            let entity_id = match &deleted {
                Some(d) if available(&d.entity_id) => d.entity_id.clone(),
                _ => generate_entity_id(
                    &tx.r,
                    &self.hass,
                    (domain, platform, unique_id),
                    device_id.as_deref(),
                    deleted.as_ref().and_then(|d| d.area_id.as_deref()),
                    // The user's name has priority.
                    deleted
                        .as_ref()
                        .and_then(|d| d.name.as_deref())
                        .or(object_id_base.as_deref()),
                ),
            };
            let row = Arc::new(Entity {
                aliases: Vec::new(),
                aliases_v2: vec![Value::Null],
                capabilities: patch
                    .capabilities
                    .clone()
                    .flatten()
                    .map(|c| RawJson::new(&c)),
                config_entry_id: patch.config_entry_id.clone().flatten(),
                created_at: Some(now.clone()),
                device_id,
                disabled_by,
                entity_category: patch.entity_category.clone().flatten(),
                entity_id,
                id: Some(random_id()),
                has_entity_name: patch.has_entity_name.unwrap_or(false),
                modified_at: Some(now.clone()),
                object_id_base,
                options: patch.options.as_ref().map(RawJson::new).unwrap_or_default(),
                original_device_class: patch.original_device_class.clone().flatten(),
                original_icon: patch.original_icon.clone().flatten(),
                original_name: patch.original_name.clone().flatten(),
                platform: platform.to_owned(),
                supported_features: json!(patch.supported_features.unwrap_or(0)),
                translation_key: patch.translation_key.clone().flatten(),
                unique_id: Some(Value::String(unique_id.to_owned())),
                unit_of_measurement: patch.unit_of_measurement.clone().flatten(),
                ..Default::default()
            });
            let row = match deleted {
                None => row,
                Some(d) => {
                    let mut r = (*row).clone();
                    r.aliases = d.aliases;
                    r.aliases_v2 = d.aliases_v2;
                    r.area_id = d.area_id;
                    r.categories = d.categories;
                    r.created_at = d.created_at;
                    r.device_class = d.device_class;
                    r.icon = d.icon;
                    r.id = d.id;
                    r.labels = d.labels;
                    r.name = d.name;
                    if !d.options_undefined {
                        r.options = d.options;
                    }
                    if !d.hidden_by_undefined {
                        r.hidden_by = d.hidden_by;
                    }
                    // The deleted row's disable wins over the caller's.
                    if !d.disabled_by_undefined {
                        r.disabled_by = restored_disabled_by(
                            d.disabled_by,
                            self.entry_disabled(r.config_entry_id.as_deref()),
                        );
                    }
                    Arc::new(r)
                }
            };
            tracing::info!(
                "Registered new {domain}.{platform} entity: {}",
                row.entity_id
            );
            tx.r.entities.push(row.clone());
            tx.entity_event(json!({"action": "create", "entity_id": row.entity_id}));
            row
        })
    }

    /// `async_update_entity`: the fields of `patch` that differ are
    /// written, `modified_at` is bumped and one `update` event carries the
    /// old values. Nothing happens when nothing differs (`None`).
    pub fn update_entity(&self, entity_id: &str, patch: EntityPatch) -> Option<Map<String, Value>> {
        let now = now_iso(&self.hass);
        self.write(|tx| {
            let i =
                tx.r.entities
                    .iter()
                    .position(|e| e.entity_id == entity_id)?;
            update_row(tx, i, &patch, &now)
        })
    }

    /// `async_remove`: the row moves to `deleted_entities` (replacing an
    /// older one for the same unique id), orphaned now if it has no
    /// config entry. The caller removes the state.
    pub fn remove_entity(&self, entity_id: &str) {
        let now = self.hass.clock().now();
        self.write(|tx| {
            if let Some(i) = tx.r.entities.iter().position(|e| e.entity_id == entity_id) {
                remove_row(tx, i, now);
            }
        });
    }

    /// A config entry goes away (`async_clear_config_entry` in both
    /// registries): devices lose it (one left with no entry is removed,
    /// with its entities); its other entities are removed; its deleted
    /// rows are orphaned now (purged by HA after 30 days).
    pub fn clear_config_entry(&self, entry_id: &str) {
        let now = self.hass.clock().now();
        let now_iso = ha_core::isoformat(now);
        let stamp = json!(now.as_microsecond() as f64 / 1e6);
        self.write(|tx| {
            let devices: Vec<Arc<Device>> =
                tx.r.devices
                    .iter()
                    .filter(|d| d.config_entries.iter().any(|e| e == entry_id))
                    .cloned()
                    .collect();
            for device in devices {
                let Some(i) = tx.r.devices.iter().position(|d| d.id == device.id) else {
                    continue;
                };
                if device.config_entries.len() == 1 {
                    // `async_remove_device`.
                    tx.r.devices.remove(i);
                    let deleted = DeletedDevice {
                        area_id: device.area_id.clone(),
                        config_entries: device.config_entries.clone(),
                        config_entries_subentries: device.config_entries_subentries.clone(),
                        connections: device.connections.clone(),
                        created_at: device.created_at.clone(),
                        disabled_by: device.disabled_by.clone(),
                        disabled_by_undefined: false,
                        identifiers: device.identifiers.clone(),
                        id: device.id.clone(),
                        labels: device.labels.clone(),
                        modified_at: Some(now_iso.clone()),
                        name_by_user: device.name_by_user.clone(),
                        orphaned_timestamp: None,
                        extra: Map::new(),
                    };
                    let mut rows: Vec<RawRow> = tx
                        .r
                        .device_file
                        .deleted
                        .iter()
                        .filter(|raw| DeletedDevice::parse(raw).is_none_or(|d| d.id != device.id))
                        .cloned()
                        .collect();
                    rows.extend(deleted.raw());
                    tx.r.device_file.deleted = Arc::new(rows);
                    // Devices connected via it lose the link first.
                    for i in 0..tx.r.devices.len() {
                        if tx.r.devices[i].via_device_id.as_deref() == Some(device.id.as_str()) {
                            let mut child = (*tx.r.devices[i]).clone();
                            child.via_device_id = None;
                            child.modified_at = Some(now_iso.clone());
                            let id = child.id.clone();
                            tx.r.devices[i] = Arc::new(child);
                            tx.device_event(json!({"action": "update", "device_id": id,
                                                   "changes": {"via_device_id": device.id}}));
                        }
                    }
                    tx.device_event(json!({"action": "remove", "device_id": device.id,
                                           "device": device.json()}));
                    // The entity registry removes the device's entities of
                    // its entries, and unlinks any other.
                    while let Some(e) = tx.r.entities.iter().position(|e| {
                        e.device_id.as_deref() == Some(device.id.as_str())
                            && e.config_entry_id
                                .as_ref()
                                .is_some_and(|c| device.config_entries.contains(c))
                    }) {
                        remove_row(tx, e, now);
                    }
                    let unlink = EntityPatch {
                        device_id: Some(None),
                        ..Default::default()
                    };
                    while let Some(e) =
                        tx.r.entities
                            .iter()
                            .position(|e| e.device_id.as_deref() == Some(device.id.as_str()))
                    {
                        update_row(tx, e, &unlink, &now_iso);
                    }
                } else {
                    let mut d = (*device).clone();
                    let mut changes = Map::new();
                    if d.primary_config_entry.as_deref() == Some(entry_id) {
                        changes.insert("primary_config_entry".into(), json!(entry_id));
                        d.primary_config_entry = None;
                    }
                    changes.insert("config_entries".into(), json!(d.config_entries));
                    changes.insert(
                        "config_entries_subentries".into(),
                        Value::Object(d.config_entries_subentries.clone()),
                    );
                    d.config_entries.retain(|e| e != entry_id);
                    d.config_entries_subentries.remove(entry_id);
                    // Every remaining entry disabled: so is the device.
                    if d.disabled_by.is_none()
                        && d.config_entries
                            .iter()
                            .all(|e| self.entry_disabled(Some(e)))
                    {
                        changes.insert("disabled_by".into(), Value::Null);
                        d.disabled_by = Some("config_entry".into());
                    }
                    d.modified_at = Some(now_iso.clone());
                    tx.r.devices[i] = Arc::new(d);
                    tx.device_event(json!({"action": "update", "device_id": device.id,
                                           "changes": changes}));
                }
            }
            while let Some(e) =
                tx.r.entities
                    .iter()
                    .position(|e| e.config_entry_id.as_deref() == Some(entry_id))
            {
                remove_row(tx, e, now);
            }
            // Orphan what's left of the entry, with one timestamp.
            let rows = rewrite::<DeletedEntity>(&tx.r.entity_file.deleted, |d| {
                if d.config_entry_id.as_deref() != Some(entry_id) {
                    return false;
                }
                d.orphaned_timestamp = Some(stamp.clone());
                d.config_entry_id = None;
                true
            });
            tx.r.entity_file.deleted = Arc::new(rows);
            let rows = rewrite::<DeletedDevice>(&tx.r.device_file.deleted, |d| {
                if !d.config_entries.iter().any(|e| e == entry_id) {
                    return false;
                }
                d.config_entries.retain(|e| e != entry_id);
                d.config_entries_subentries.remove(entry_id);
                if d.config_entries.is_empty() {
                    d.orphaned_timestamp = Some(stamp.clone());
                }
                true
            });
            tx.r.device_file.deleted = Arc::new(rows);
            tx.save_entities = true;
            tx.save_devices = true;
        });
    }

    /// `async_get_or_create` for a device, as `mobile_app` calls it
    /// (`device_info_type` "primary"): found by identifier, else created;
    /// the config entry is added, and made primary when the device has
    /// none, or its primary entry is gone or a low-priority integration's;
    /// changed fields fire one `update` with the old values. A name change
    /// ripples into the device's legacy-named entities (`changes: {}`).
    pub fn get_or_create_device(&self, info: DeviceInfo) -> Arc<Device> {
        let now = now_iso(&self.hass);
        let domains = self.entries.read().clone();
        self.write(|tx| {
            let r = &mut tx.r;
            let found = r
                .devices
                .iter()
                .position(|d| d.identifiers.iter().any(|i| info.identifiers.contains(i)));
            // A deleted device with one of the identifiers comes back
            // (`to_device_entry`): its id, creation time and the user's
            // settings, with this entry.
            let restored = found.is_none().then(|| {
                let rows = &r.device_file.deleted;
                let i = rows.iter().position(|raw| {
                    DeletedDevice::parse(raw)
                        .is_some_and(|d| d.identifiers.iter().any(|x| info.identifiers.contains(x)))
                })?;
                let mut rows = (**rows).clone();
                let d = DeletedDevice::parse(&rows.remove(i))?;
                r.device_file.deleted = Arc::new(rows);
                Some(d)
            });
            let (i, mut d, is_new) = match (found, restored.flatten()) {
                (Some(i), _) => (Some(i), (*r.devices[i]).clone(), false),
                (None, Some(old)) => {
                    let mut subentries = Map::new();
                    subentries.insert(info.config_entry_id.clone(), json!([null]));
                    (
                        None,
                        Device {
                            id: old.id,
                            area_id: old.area_id,
                            config_entries: vec![info.config_entry_id.clone()],
                            config_entries_subentries: subentries,
                            created_at: old.created_at,
                            disabled_by: if old.disabled_by_undefined {
                                None
                            } else {
                                restored_disabled_by(
                                    old.disabled_by,
                                    self.entry_disabled(Some(&info.config_entry_id)),
                                )
                            },
                            identifiers: old
                                .identifiers
                                .into_iter()
                                .filter(|x| info.identifiers.contains(x))
                                .collect(),
                            labels: old.labels,
                            modified_at: Some(now.clone()),
                            name_by_user: old.name_by_user,
                            ..Default::default()
                        },
                        true,
                    )
                }
                (None, None) => (
                    None,
                    Device {
                        id: random_id(),
                        created_at: Some(now.clone()),
                        modified_at: Some(now.clone()),
                        ..Default::default()
                    },
                    true,
                ),
            };
            let old_name = d.display_name().map(str::to_owned);
            let mut changes = Map::new();
            let entry = &info.config_entry_id;
            if d.primary_config_entry.as_ref() != Some(entry) {
                let replace = match (&d.primary_config_entry, &domains) {
                    (None, _) => true,
                    (Some(primary), Some(domains)) => domains(primary)
                        .is_none_or(|e| LOW_PRIO_DOMAINS.contains(&e.domain.as_str())),
                    (Some(_), None) => false,
                };
                if replace {
                    changes.insert("primary_config_entry".into(), json!(d.primary_config_entry));
                    d.primary_config_entry = Some(entry.clone());
                }
            }
            if !d.config_entries.contains(entry) {
                changes.insert("config_entries".into(), json!(d.config_entries));
                changes.insert(
                    "config_entries_subentries".into(),
                    Value::Object(d.config_entries_subentries.clone()),
                );
                d.config_entries.push(entry.clone());
                d.config_entries_subentries
                    .insert(entry.clone(), json!([null]));
            }
            let new_ids: Vec<Value> = info
                .identifiers
                .iter()
                .filter(|id| !d.identifiers.contains(id))
                .cloned()
                .collect();
            if !new_ids.is_empty() {
                changes.insert("identifiers".into(), json!(d.identifiers));
                d.identifiers.extend(new_ids);
            }
            let mut set = |key: &str, field: &mut Option<String>, value: &Option<String>| {
                if value.is_some() && field != value {
                    changes.insert(key.into(), json!(field));
                    field.clone_from(value);
                }
            };
            set("manufacturer", &mut d.manufacturer, &info.manufacturer);
            set("model", &mut d.model, &info.model);
            set("name", &mut d.name, &info.name);
            set("sw_version", &mut d.sw_version, &info.sw_version);
            if !is_new && changes.is_empty() {
                return r.devices[i.expect("found")].clone();
            }
            d.modified_at = Some(now.clone());
            let renamed = !is_new && changes.contains_key("name");
            let device = Arc::new(d);
            match i {
                Some(i) => r.devices[i] = device.clone(),
                None => r.devices.push(device.clone()),
            }
            tx.device_event(if is_new {
                json!({"action": "create", "device_id": device.id})
            } else {
                json!({"action": "update", "device_id": device.id, "changes": changes})
            });
            if renamed {
                device_renamed(tx, &device, old_name.as_deref(), &now);
            }
            device
        })
    }
}

/// `async_remove` for row `i`: into `deleted_entities` (replacing an older
/// row for the same unique id), orphaned now if it has no config entry.
fn remove_row(tx: &mut Tx, i: usize, now: jiff::Timestamp) {
    let row = tx.r.entities.remove(i);
    let orphaned = row
        .config_entry_id
        .is_none()
        .then(|| now.as_microsecond() as f64 / 1e6);
    let deleted = DeletedEntity::of(&row, ha_core::isoformat(now), orphaned);
    let key = deleted.key();
    let mut kept: Vec<RawRow> =
        tx.r.entity_file
            .deleted
            .iter()
            .filter(|d| DeletedEntity::parse(d).map(|d| d.key()).as_ref() != Some(&key))
            .cloned()
            .collect();
    kept.extend(deleted.raw());
    tx.r.entity_file.deleted = Arc::new(kept);
    tx.entity_event(json!({"action": "remove", "entity_id": row.entity_id}));
}

fn is_unique(e: &Entity, domain: &str, platform: &str, unique_id: &str) -> bool {
    e.platform == platform
        && e.entity_id.split('.').next() == Some(domain)
        && e.unique_id.as_ref().and_then(Value::as_str) == Some(unique_id)
}

/// Apply `patch` to row `i`: on a change, bump `modified_at` and queue the
/// `update` event with the old values.
fn update_row(tx: &mut Tx, i: usize, patch: &EntityPatch, now: &str) -> Option<Map<String, Value>> {
    let mut row = (*tx.r.entities[i]).clone();
    let changes = apply(&mut row, patch);
    if changes.is_empty() {
        return None;
    }
    row.modified_at = Some(now.to_owned());
    let entity_id = row.entity_id.clone();
    tx.r.entities[i] = Arc::new(row);
    tx.entity_event(json!({"action": "update", "entity_id": entity_id, "changes": changes}));
    Some(changes)
}

/// `async_device_modified` for a `name` change: each legacy-named entity's
/// unprefixed name (not stored) is recomputed; where it differs,
/// `modified_at` is bumped and `update` fires with empty `changes`.
fn device_renamed(tx: &mut Tx, device: &Device, old_name: Option<&str>, now: &str) {
    let new_name = device.display_name();
    let unprefixed = |name: Option<&str>, device: Option<&str>| strip_prefix(name?, device?);
    let mut touched = Vec::new();
    for e in tx.r.entities.iter_mut() {
        if e.device_id.as_deref() != Some(device.id.as_str()) || e.has_entity_name {
            continue;
        }
        let name = e.original_name.as_deref();
        if unprefixed(name, old_name) == unprefixed(name, new_name) {
            continue;
        }
        let mut row = (**e).clone();
        row.modified_at = Some(now.to_owned());
        touched.push(row.entity_id.clone());
        *e = Arc::new(row);
    }
    for entity_id in touched {
        tx.entity_event(json!({"action": "update", "entity_id": entity_id, "changes": {}}));
    }
}

fn find_unique<'a>(
    r: &'a Registries,
    domain: &str,
    platform: &str,
    unique_id: &str,
) -> Option<&'a Arc<Entity>> {
    r.entities
        .iter()
        .find(|e| is_unique(e, domain, platform, unique_id))
}

/// Apply `patch`, returning the old values of what changed.
fn apply(row: &mut Entity, patch: &EntityPatch) -> Map<String, Value> {
    let mut changes = Map::new();
    fn set(
        changes: &mut Map<String, Value>,
        key: &str,
        field: &mut Option<String>,
        v: &Option<Option<String>>,
    ) {
        if let Some(v) = v
            && field != v
        {
            changes.insert(key.into(), json!(field));
            field.clone_from(v);
        }
    }
    if let Some(caps) = &patch.capabilities {
        let old = row.capabilities.as_ref().map(RawJson::value);
        if &old != caps {
            changes.insert("capabilities".into(), old.unwrap_or(Value::Null));
            row.capabilities = caps.as_ref().map(RawJson::new);
        }
    }
    set(
        &mut changes,
        "config_entry_id",
        &mut row.config_entry_id,
        &patch.config_entry_id,
    );
    set(
        &mut changes,
        "device_class",
        &mut row.device_class,
        &patch.device_class,
    );
    set(
        &mut changes,
        "device_id",
        &mut row.device_id,
        &patch.device_id,
    );
    set(
        &mut changes,
        "disabled_by",
        &mut row.disabled_by,
        &patch.disabled_by,
    );
    set(
        &mut changes,
        "entity_category",
        &mut row.entity_category,
        &patch.entity_category,
    );
    if let Some(h) = patch.has_entity_name
        && row.has_entity_name != h
    {
        changes.insert("has_entity_name".into(), json!(row.has_entity_name));
        row.has_entity_name = h;
    }
    set(
        &mut changes,
        "object_id_base",
        &mut row.object_id_base,
        &patch.object_id_base,
    );
    if let Some(options) = &patch.options {
        let old = row.options.value();
        if &old != options {
            changes.insert("options".into(), old);
            row.options = RawJson::new(options);
        }
    }
    set(
        &mut changes,
        "original_device_class",
        &mut row.original_device_class,
        &patch.original_device_class,
    );
    set(
        &mut changes,
        "original_icon",
        &mut row.original_icon,
        &patch.original_icon,
    );
    set(
        &mut changes,
        "original_name",
        &mut row.original_name,
        &patch.original_name,
    );
    if let Some(f) = patch.supported_features
        && row.supported_features != json!(f)
    {
        changes.insert("supported_features".into(), row.supported_features.clone());
        row.supported_features = json!(f);
    }
    set(
        &mut changes,
        "translation_key",
        &mut row.translation_key,
        &patch.translation_key,
    );
    set(
        &mut changes,
        "unit_of_measurement",
        &mut row.unit_of_measurement,
        &patch.unit_of_measurement,
    );
    changes
}

/// `_async_generate_entity_id` for a new row with no user name:
/// `<area> <device> <name>` (the name with the device's prefix stripped),
/// slugified, made unique against the registry and the state machine.
fn generate_entity_id(
    r: &Registries,
    hass: &Hass,
    (domain, platform, unique_id): (&str, &str, &str),
    device_id: Option<&str>,
    area_id: Option<&str>,
    object_id_base: Option<&str>,
) -> String {
    let device = device_id.and_then(|id| r.devices.iter().find(|d| d.id == id));
    let device_name = device.and_then(|d| d.display_name());
    // The entity's own area, else its device's.
    let area_name = area_id
        .or_else(|| device.and_then(|d| d.area_id.as_deref()))
        .and_then(|a| r.areas.iter().find(|x| x.id == a))
        .map(|a| a.name.as_str());
    let entity_name = match (object_id_base, device_name) {
        (Some(name), Some(device)) => strip_prefix(name, device).or_else(|| Some(name.to_owned())),
        (name, _) => name.map(str::to_owned),
    };
    let full: Vec<&str> = [area_name, device_name, entity_name.as_deref()]
        .into_iter()
        .flatten()
        .filter(|p| !p.is_empty())
        .collect();
    let object_id = if full.is_empty() {
        format!("{platform}_{unique_id}")
    } else {
        full.join(" ")
    };
    available_entity_id(r, hass, domain, &object_id)
}

/// `async_get_available_entity_id`: `<domain>.<slug>`, cut to 255, with
/// `_2`, `_3`, … while the registry or the state machine has it.
pub fn available_entity_id(r: &Registries, hass: &Hass, domain: &str, object_id: &str) -> String {
    let preferred = format!("{domain}.{}", ha_core::slugify(object_id));
    let taken = |id: &str| {
        r.entities.iter().any(|e| e.entity_id == id)
            || ha_core::EntityId::parse(id)
                .ok()
                .is_some_and(|id| hass.states().get(&id).is_some())
    };
    let cut = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    let mut test = cut(&preferred, MAX_ENTITY_ID);
    let mut tries = 1;
    while taken(&test) {
        tries += 1;
        let suffix = format!("_{tries}");
        test = format!("{}{suffix}", cut(&preferred, MAX_ENTITY_ID - suffix.len()));
    }
    test
}

/// A `deleted_entities` row (`DeletedRegistryEntry.as_storage_fragment`),
/// in HA's key order; nested blobs kept raw.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct DeletedEntity {
    #[serde(default)]
    aliases: Vec<Value>,
    #[serde(default)]
    aliases_v2: Vec<Value>,
    #[serde(default)]
    area_id: Option<String>,
    #[serde(default)]
    categories: RawJson,
    #[serde(default)]
    config_entry_id: Option<String>,
    #[serde(default)]
    config_subentry_id: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    device_class: Option<String>,
    #[serde(default)]
    disabled_by: Option<String>,
    #[serde(default)]
    disabled_by_undefined: bool,
    entity_id: String,
    #[serde(default)]
    hidden_by: Option<String>,
    #[serde(default)]
    hidden_by_undefined: bool,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    modified_at: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    options: RawJson,
    #[serde(default)]
    options_undefined: bool,
    #[serde(default)]
    orphaned_timestamp: Option<Value>,
    platform: String,
    #[serde(default)]
    unique_id: Option<Value>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl DeletedEntity {
    /// `async_remove`'s row for `e`.
    fn of(e: &Entity, modified_at: String, orphaned_timestamp: Option<f64>) -> Self {
        Self {
            aliases: e.aliases.clone(),
            aliases_v2: e.aliases_v2.clone(),
            area_id: e.area_id.clone(),
            categories: e.categories.clone(),
            config_entry_id: e.config_entry_id.clone(),
            config_subentry_id: e.config_subentry_id.clone(),
            created_at: e.created_at.clone(),
            device_class: e.device_class.clone(),
            disabled_by: e.disabled_by.clone(),
            disabled_by_undefined: false,
            entity_id: e.entity_id.clone(),
            hidden_by: e.hidden_by.clone(),
            hidden_by_undefined: false,
            icon: e.icon.clone(),
            id: e.id.clone(),
            labels: e.labels.clone(),
            modified_at: Some(modified_at),
            name: e.name.clone(),
            options: e.options.clone(),
            options_undefined: false,
            orphaned_timestamp: orphaned_timestamp.map(|t| json!(t)),
            platform: e.platform.clone(),
            unique_id: e.unique_id.clone(),
            extra: Map::new(),
        }
    }

    fn key(&self) -> (String, String, Value) {
        (
            self.entity_id
                .split('.')
                .next()
                .unwrap_or_default()
                .to_owned(),
            self.platform.clone(),
            self.unique_id.clone().unwrap_or(Value::Null),
        )
    }

    fn parse(raw: &RawRow) -> Option<Self> {
        serde_json::from_str(raw.get()).ok()
    }

    fn raw(&self) -> Option<RawRow> {
        serde_json::value::to_raw_value(self).ok()
    }
}

/// A `deleted_devices` row (`DeletedDeviceEntry.as_storage_fragment`), in
/// HA's key order (`identifiers` before `id`).
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct DeletedDevice {
    #[serde(default)]
    area_id: Option<String>,
    #[serde(default)]
    config_entries: Vec<String>,
    #[serde(default)]
    config_entries_subentries: Map<String, Value>,
    #[serde(default)]
    connections: Vec<Value>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    disabled_by: Option<String>,
    #[serde(default)]
    disabled_by_undefined: bool,
    #[serde(default)]
    identifiers: Vec<Value>,
    id: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    modified_at: Option<String>,
    #[serde(default)]
    name_by_user: Option<String>,
    #[serde(default)]
    orphaned_timestamp: Option<Value>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl DeletedDevice {
    fn parse(raw: &RawRow) -> Option<Self> {
        serde_json::from_str(raw.get()).ok()
    }

    fn raw(&self) -> Option<RawRow> {
        serde_json::value::to_raw_value(self).ok()
    }
}

/// `disabled_by` from a deleted row, adjusted for the entry it comes back
/// to: a disabled entry disables it (`config_entry`), an enabled one lifts
/// a `config_entry` disable.
fn restored_disabled_by(by: Option<String>, entry_disabled: bool) -> Option<String> {
    match (by, entry_disabled) {
        (None, true) => Some("config_entry".into()),
        (Some(b), false) if b == "config_entry" => None,
        (by, _) => by,
    }
}

/// Rewrite a deleted-row list, row by row (unparsable rows kept as they
/// are).
fn rewrite<T: serde::de::DeserializeOwned + Serialize>(
    rows: &[RawRow],
    mut f: impl FnMut(&mut T) -> bool,
) -> Vec<RawRow> {
    rows.iter()
        .map(|raw| {
            let Ok(mut row) = serde_json::from_str::<T>(raw.get()) else {
                return raw.clone();
            };
            if !f(&mut row) {
                return raw.clone();
            }
            serde_json::value::to_raw_value(&row).unwrap_or_else(|_| raw.clone())
        })
        .collect()
}

#[cfg(test)]
mod tests;

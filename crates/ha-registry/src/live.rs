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

impl Registry {
    /// Kept in memory only (tests, and checking a copy).
    pub fn in_memory(hass: &Hass, registries: Registries) -> Arc<Self> {
        Arc::new(Self {
            hass: hass.clone(),
            snap: RwLock::new(Arc::new(registries)),
            writer: Mutex::new(()),
            saves: None,
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
            }
        })
    }

    /// The registries as they are now.
    pub fn snapshot(&self) -> Arc<Registries> {
        self.snap.read().clone()
    }

    /// Write whatever is pending (at stop).
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

    fn fire(&self, event_type: &str, data: Value) {
        let now = self.hass.clock().now();
        self.hass.bus().fire(Event::new(
            EventKind::Custom {
                name: event_type.into(),
                data,
            },
            now,
            Context::at(now),
        ));
    }

    /// Run `f` on a copy of the registries and swap it in.
    fn change<T>(&self, f: impl FnOnce(&mut Registries) -> T) -> T {
        let _turn = self.writer.lock();
        let mut next = (*self.snapshot()).clone();
        let out = f(&mut next);
        *self.snap.write() = Arc::new(next);
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
        if let Some(existing) = self.entity_by_unique_id(domain, platform, unique_id) {
            self.update_entity(&existing.entity_id, patch);
            return self
                .entity_by_unique_id(domain, platform, unique_id)
                .unwrap_or(existing);
        }
        let now = now_iso(&self.hass);
        let row = self.change(|r| {
            let object_id_base = patch.object_id_base.clone().flatten();
            let device_id = patch.device_id.clone().flatten();
            let entity_id = generate_entity_id(
                r,
                &self.hass,
                domain,
                platform,
                unique_id,
                device_id.as_deref(),
                object_id_base.as_deref(),
            );
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
            r.entities.push(row.clone());
            row
        });
        tracing::info!(
            "Registered new {domain}.{platform} entity: {}",
            row.entity_id
        );
        self.schedule_save(true);
        self.fire(
            "entity_registry_updated",
            json!({"action": "create", "entity_id": row.entity_id}),
        );
        row
    }

    /// `async_update_entity`: the fields of `patch` that differ are
    /// written, `modified_at` is bumped and one `update` event carries the
    /// old values. Nothing happens when nothing differs (`None`).
    pub fn update_entity(&self, entity_id: &str, patch: EntityPatch) -> Option<Map<String, Value>> {
        let now = now_iso(&self.hass);
        let changes = self.change(|r| {
            let i = r.entities.iter().position(|e| e.entity_id == entity_id)?;
            let mut row = (*r.entities[i]).clone();
            let changes = apply(&mut row, &patch);
            if changes.is_empty() {
                return None;
            }
            row.modified_at = Some(now);
            r.entities[i] = Arc::new(row);
            Some(changes)
        })?;
        self.schedule_save(true);
        self.fire(
            "entity_registry_updated",
            json!({"action": "update", "entity_id": entity_id, "changes": changes}),
        );
        Some(changes)
    }

    /// `async_remove`: the row moves to `deleted_entities` (replacing an
    /// older one for the same unique id), orphaned now if it has no
    /// config entry. The caller removes the state.
    pub fn remove_entity(&self, entity_id: &str) {
        let now = self.hass.clock().now();
        let removed = self.change(|r| {
            let i = r.entities.iter().position(|e| e.entity_id == entity_id)?;
            let row = r.entities.remove(i);
            let orphaned = row
                .config_entry_id
                .is_none()
                .then(|| now.as_microsecond() as f64 / 1e6);
            let deleted = DeletedEntity::of(&row, ha_core::isoformat(now), orphaned);
            let raw = serde_json::value::to_raw_value(&deleted).ok()?;
            let key = deleted.key();
            let mut kept: Vec<RawRow> = r
                .entity_file
                .deleted
                .iter()
                .filter(|d| deleted_key(d).as_ref() != Some(&key))
                .cloned()
                .collect();
            kept.push(raw);
            r.entity_file.deleted = Arc::new(kept);
            Some(())
        });
        if removed.is_some() {
            self.schedule_save(true);
            self.fire(
                "entity_registry_updated",
                json!({"action": "remove", "entity_id": entity_id}),
            );
        }
    }

    /// `async_get_or_create` for a device, as `mobile_app` calls it: found
    /// by identifier, else created; the config entry is added; changed
    /// fields fire one `update` with the old values. A name change ripples
    /// into the device's legacy-named entities (`changes: {}` each).
    pub fn get_or_create_device(&self, info: DeviceInfo) -> Arc<Device> {
        let now = now_iso(&self.hass);
        let (device, event, renamed) = self.change(
            |r| -> (Arc<Device>, Option<Value>, Option<Option<String>>) {
                let found = r
                    .devices
                    .iter()
                    .position(|d| d.identifiers.iter().any(|i| info.identifiers.contains(i)));
                let (i, mut d, is_new) = match found {
                    Some(i) => (Some(i), (*r.devices[i]).clone(), false),
                    None => (
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
                let mut changes = Map::new();
                let entry = &info.config_entry_id;
                if d.primary_config_entry.is_none() {
                    changes.insert("primary_config_entry".into(), json!(d.primary_config_entry));
                    d.primary_config_entry = Some(entry.clone());
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
                    return (r.devices[i.expect("found")].clone(), None, None);
                }
                d.modified_at = Some(now.clone());
                // The display name before, if `name` changed.
                let renamed = (!is_new && changes.contains_key("name")).then(|| {
                    let old = i.map(|i| r.devices[i].clone());
                    old.and_then(|o| o.display_name().map(str::to_owned))
                });
                let device = Arc::new(d);
                match i {
                    Some(i) => r.devices[i] = device.clone(),
                    None => r.devices.push(device.clone()),
                }
                let event = if is_new {
                    json!({"action": "create", "device_id": device.id})
                } else {
                    json!({"action": "update", "device_id": device.id, "changes": changes})
                };
                (device, Some(event), renamed)
            },
        );
        let Some(event) = event else {
            return device;
        };
        self.schedule_save(false);
        self.fire("device_registry_updated", event);
        if let Some(old_name) = renamed {
            self.device_renamed(&device, old_name.as_deref());
        }
        device
    }

    /// `async_device_modified` for a `name` change: each legacy-named
    /// entity's unprefixed name (not stored) is recomputed; where it
    /// differs, `modified_at` is bumped and `update` fires with empty
    /// `changes`.
    fn device_renamed(&self, device: &Device, old_name: Option<&str>) {
        let new_name = device.display_name();
        let unprefixed = |name: Option<&str>, device: Option<&str>| strip_prefix(name?, device?);
        let now = now_iso(&self.hass);
        let touched = self.change(|r| {
            let mut touched = Vec::new();
            for e in r.entities.iter_mut() {
                if e.device_id.as_deref() != Some(device.id.as_str()) || e.has_entity_name {
                    continue;
                }
                let name = e.original_name.as_deref();
                if unprefixed(name, old_name) == unprefixed(name, new_name) {
                    continue;
                }
                let mut row = (**e).clone();
                row.modified_at = Some(now.clone());
                touched.push(row.entity_id.clone());
                *e = Arc::new(row);
            }
            touched
        });
        if touched.is_empty() {
            return;
        }
        self.schedule_save(true);
        for entity_id in touched {
            self.fire(
                "entity_registry_updated",
                json!({"action": "update", "entity_id": entity_id, "changes": {}}),
            );
        }
    }
}

fn find_unique<'a>(
    r: &'a Registries,
    domain: &str,
    platform: &str,
    unique_id: &str,
) -> Option<&'a Arc<Entity>> {
    r.entities.iter().find(|e| {
        e.platform == platform
            && e.entity_id.split('.').next() == Some(domain)
            && e.unique_id.as_ref().and_then(Value::as_str) == Some(unique_id)
    })
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
    domain: &str,
    platform: &str,
    unique_id: &str,
    device_id: Option<&str>,
    object_id_base: Option<&str>,
) -> String {
    let device = device_id.and_then(|id| r.devices.iter().find(|d| d.id == id));
    let device_name = device.and_then(|d| d.display_name());
    let area_name = device
        .and_then(|d| d.area_id.as_deref())
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

/// `DeletedRegistryEntry.as_storage_fragment`, in HA's key order.
#[derive(Serialize)]
struct DeletedEntity<'a> {
    aliases: &'a [Value],
    aliases_v2: &'a [Value],
    area_id: &'a Option<String>,
    categories: &'a RawJson,
    config_entry_id: &'a Option<String>,
    config_subentry_id: &'a Option<String>,
    created_at: &'a Option<String>,
    device_class: &'a Option<String>,
    disabled_by: &'a Option<String>,
    disabled_by_undefined: bool,
    entity_id: &'a str,
    hidden_by: &'a Option<String>,
    hidden_by_undefined: bool,
    icon: &'a Option<String>,
    id: &'a Option<String>,
    labels: &'a [String],
    modified_at: String,
    name: &'a Option<String>,
    options: &'a RawJson,
    options_undefined: bool,
    orphaned_timestamp: Option<f64>,
    platform: &'a str,
    unique_id: &'a Option<Value>,
}

impl<'a> DeletedEntity<'a> {
    fn of(e: &'a Entity, modified_at: String, orphaned_timestamp: Option<f64>) -> Self {
        Self {
            aliases: &e.aliases,
            aliases_v2: &e.aliases_v2,
            area_id: &e.area_id,
            categories: &e.categories,
            config_entry_id: &e.config_entry_id,
            config_subentry_id: &e.config_subentry_id,
            created_at: &e.created_at,
            device_class: &e.device_class,
            disabled_by: &e.disabled_by,
            disabled_by_undefined: false,
            entity_id: &e.entity_id,
            hidden_by: &e.hidden_by,
            hidden_by_undefined: false,
            icon: &e.icon,
            id: &e.id,
            labels: &e.labels,
            modified_at,
            name: &e.name,
            options: &e.options,
            options_undefined: false,
            orphaned_timestamp,
            platform: &e.platform,
            unique_id: &e.unique_id,
        }
    }

    fn key(&self) -> (String, String, Value) {
        (
            self.entity_id
                .split('.')
                .next()
                .unwrap_or_default()
                .to_owned(),
            self.platform.to_owned(),
            self.unique_id.clone().unwrap_or(Value::Null),
        )
    }
}

/// A deleted row's `(domain, platform, unique_id)`.
fn deleted_key(raw: &RawRow) -> Option<(String, String, Value)> {
    let v: Value = serde_json::from_str(raw.get()).ok()?;
    let domain = v.get("entity_id")?.as_str()?.split('.').next()?.to_owned();
    Some((
        domain,
        v.get("platform")?.as_str()?.to_owned(),
        v.get("unique_id").cloned().unwrap_or(Value::Null),
    ))
}

#[cfg(test)]
mod tests;

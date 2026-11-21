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
use std::collections::HashMap;
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
    /// Entities, devices, areas.
    saves: Option<[Arc<DelayedSave>; 3]>,
    entries: RwLock<Option<EntryLookup>>,
    /// The entity or device store that won't be written
    /// ([`Registries::unwritable`]), as found when the registry was made.
    read_only: Vec<&'static str>,
}

/// The registries as something long-lived reads them (a service resolving
/// its targets, an integration set up while `hars` runs): one fixed set
/// (tests), or whatever the live registry holds when asked, so rows made
/// after start-up are seen.
#[derive(Clone)]
pub enum Lookup {
    Fixed(Arc<Registries>),
    Live(Arc<Registry>),
}

impl Lookup {
    /// The registries as they are now.
    pub fn get(&self) -> Arc<Registries> {
        match self {
            Self::Fixed(r) => r.clone(),
            Self::Live(r) => r.snapshot(),
        }
    }

    /// The live registry, where rows can be made.
    pub fn live(&self) -> Option<&Arc<Registry>> {
        match self {
            Self::Fixed(_) => None,
            Self::Live(r) => Some(r),
        }
    }
}

impl From<Arc<Registries>> for Lookup {
    fn from(r: Arc<Registries>) -> Self {
        Self::Fixed(r)
    }
}

impl From<Arc<Registry>> for Lookup {
    fn from(r: Arc<Registry>) -> Self {
        Self::Live(r)
    }
}

/// What an integration wants a row to have (HA's keyword arguments to
/// `async_get_or_create` / `async_update_entity`): `None` leaves a field
/// as it is, `Some(None)` clears it.
#[derive(Debug, Clone, Default)]
pub struct EntityPatch {
    /// The user's area (`config/entity_registry/update`).
    pub area_id: Option<Option<String>>,
    /// The user's categories to change, by scope: a category sets its
    /// scope, `null` clears it, the other scopes stay.
    pub categories: Option<Map<String, Value>>,
    /// The user's labels: a set (the order given is kept, repeats
    /// dropped).
    pub labels: Option<Vec<String>>,
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
    /// The entry's `pref_disable_new_entities`, for
    /// [`Registry::get_or_create_entity`]: a row that is made, or comes
    /// back from the deleted ones, enabled is disabled by the
    /// integration. An existing row is not touched by it.
    pub pref_disable_new: bool,
}

/// What the user changes of a device (`config/device_registry/update`):
/// `None` leaves a field as it is.
#[derive(Debug, Clone, Default)]
pub struct DevicePatch {
    pub area_id: Option<Option<String>>,
    /// Only the value the device has is taken: see [`DeviceRefusal`].
    pub disabled_by: Option<Option<String>>,
    /// A set (the order given is kept, repeats dropped).
    pub labels: Option<Vec<String>>,
    pub name_by_user: Option<Option<String>>,
}

/// Why a device was not changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceRefusal {
    /// No such device (a `KeyError` in HA).
    Unknown,
    /// The store named can't be written ([`Registry::unwritable`]).
    Unwritable(&'static str),
    /// Disabling or enabling a device: its entities would have to stop
    /// or start, which `hars`'s don't do while they run.
    Disable,
    /// A new name, for a device with an entity that can't take it: one
    /// of the integration named, whose running entities keep the name
    /// they started with, or one named the old way (`None`), whose own
    /// name HA rewrites.
    Rename(Option<String>),
}

/// Why an entity did not get another id (`async_update_entity`'s
/// `ValueError`s, and what `hars` can't do).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameRefusal {
    Unknown,
    /// The entities' store can't be written ([`Registry::unwritable`]).
    Unwritable,
    Registered,
    Invalid,
    Domain,
    /// An entity of this integration, whose running entities keep the id
    /// they started with.
    Platform(String),
}

impl RenameRefusal {
    /// HA's message, where it has one.
    pub fn message(&self) -> Option<&'static str> {
        match self {
            Self::Registered => Some("Entity with this ID is already registered"),
            Self::Invalid => Some("Invalid entity ID"),
            Self::Domain => Some("New entity ID should be same domain"),
            Self::Unknown | Self::Unwritable | Self::Platform(_) => None,
        }
    }
}

/// Why an area was not made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AreaRefusal {
    /// HA's `ValueError`, in its words.
    Invalid(String),
    /// The areas' store can't be written ([`Registry::unwritable`]).
    Unwritable,
}

/// A new area (`async_create`'s arguments).
#[derive(Debug, Clone, Default)]
pub struct NewArea {
    pub name: String,
    /// A set (the order given is kept, repeats dropped).
    pub aliases: Vec<String>,
    pub floor_id: Option<String>,
    pub humidity_entity_id: Option<String>,
    pub icon: Option<String>,
    /// A set, as the aliases.
    pub labels: Vec<String>,
    pub picture: Option<String>,
    pub temperature_entity_id: Option<String>,
}

/// An area of this name and nothing else: its id is the name slugified,
/// with `_2`, `_3`, ... when taken (`generate_id_from_name`).
fn new_area(areas: &[crate::Area], name: &str, now: &str) -> crate::Area {
    let base = ha_core::slugify(name);
    let mut id = base.clone();
    let mut tries = 1;
    while areas.iter().any(|a| a.id == id) {
        tries += 1;
        id = format!("{base}_{tries}");
    }
    crate::Area {
        id,
        name: name.to_owned(),
        created_at: Some(now.to_owned()),
        modified_at: Some(now.to_owned()),
        ..Default::default()
    }
}

/// `format_mac`: a MAC address as HA keeps it (lower case, colons). One
/// in a spelling HA doesn't know is left as it is.
fn format_mac(mac: &str) -> String {
    let hex = |s: &str| s.len() == 12 && s.bytes().all(|b| b.is_ascii_hexdigit());
    let colons = |s: &str| {
        let pairs: Vec<&str> = (0..6).map(|i| &s[i * 2..i * 2 + 2]).collect();
        pairs.join(":").to_lowercase()
    };
    if mac.len() == 17 && mac.matches(':').count() == 5 {
        return mac.to_lowercase();
    }
    if mac.len() == 17 && mac.matches('-').count() == 5 {
        return mac.replace('-', ":").to_lowercase();
    }
    if mac.len() == 14 && mac.matches('.').count() == 2 {
        let bare = mac.replace('.', "");
        if hex(&bare) {
            return colons(&bare);
        }
    }
    if hex(mac) {
        return colons(mac);
    }
    mac.to_owned()
}

/// A set as HA is given one: each value once, in the order given.
fn as_set(values: &[String]) -> Vec<String> {
    let mut set: Vec<String> = Vec::with_capacity(values.len());
    for v in values {
        if !set.contains(v) {
            set.push(v.clone());
        }
    }
    set
}

/// A device as an integration describes it (`async_get_or_create`'s
/// arguments, as `mobile_app` passes them).
#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    pub config_entry_id: String,
    pub identifiers: Vec<Value>,
    /// `connections`: `[type, value]` pairs, added to those it has.
    pub connections: Vec<Value>,
    /// `entry_type` (`service`), when given.
    pub entry_type: Option<String>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub model_id: Option<String>,
    pub name: Option<String>,
    pub sw_version: Option<String>,
    /// `via_device`: an identifier of the device this one is reached
    /// through. `Some` sets the link, to nothing if no device has it.
    pub via_device: Option<Value>,
    /// `suggested_area`: a new device goes in the area of this name,
    /// which is made if there is none.
    pub suggested_area: Option<String>,
}

fn now_iso(hass: &Hass) -> String {
    ha_core::isoformat(hass.clock().now())
}

/// `random_uuid_hex`: 128 random bits as hex (not a version-4 UUID).
fn random_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

/// The registries during a write: the snapshot itself until the write
/// changes something, a copy of it from then on. Most of what an
/// integration asks at each start changes nothing (every row is there and
/// matches); such a write copies and publishes nothing.
///
/// One of these is on the stack for the length of a write: boxing the copy
/// to make the variants alike would add an allocation to every write that
/// changes something.
#[allow(clippy::large_enum_variant)]
enum Working {
    Same(Arc<Registries>),
    Changed(Registries),
}

impl std::ops::Deref for Working {
    type Target = Registries;

    fn deref(&self) -> &Registries {
        match self {
            Self::Same(r) => r,
            Self::Changed(r) => r,
        }
    }
}

impl std::ops::DerefMut for Working {
    fn deref_mut(&mut self) -> &mut Registries {
        if let Self::Same(r) = self {
            *self = Self::Changed((**r).clone());
        }
        match self {
            Self::Changed(r) => r,
            Self::Same(_) => unreachable!("copied above"),
        }
    }
}

/// One write: the registries (copied when first changed), and what to
/// publish after the copy is swapped in.
struct Tx {
    r: Working,
    events: Vec<(&'static str, Value)>,
    /// Entities given another id: the old id, and the row. A placeholder
    /// state under the old id moves once the events are out.
    renamed: Vec<(String, Arc<Entity>)>,
    save_entities: bool,
    save_devices: bool,
    save_areas: bool,
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
    /// What templates read through `states(…, rounded=True)`: a sensor's
    /// display precision, as the registry has it when the template runs.
    pub fn display_precision(self: &Arc<Self>) -> ha_core::template::DisplayPrecision {
        let me = self.clone();
        Arc::new(move |entity_id: &str| me.snapshot().sensor_display_precision(entity_id))
    }

    /// Kept in memory only (tests, and checking a copy).
    pub fn in_memory(hass: &Hass, registries: Registries) -> Arc<Self> {
        Arc::new(Self {
            hass: hass.clone(),
            snap: RwLock::new(Arc::new(registries)),
            writer: Mutex::new(()),
            saves: None,
            entries: RwLock::new(None),
            read_only: Vec::new(),
        })
    }

    /// Saved to `config_dir/.storage` after changes, as HA does.
    pub fn new(hass: &Hass, config_dir: &Path, registries: Registries) -> Arc<Self> {
        let read_only: Vec<&'static str> = registries.unwritable_stores().collect();
        Arc::new_cyclic(|weak: &std::sync::Weak<Self>| {
            let save = |key: &'static str| {
                let (weak, dir) = (weak.clone(), config_dir.to_owned());
                DelayedSave::spawn(key, move || {
                    let Some(this) = weak.upgrade() else {
                        return Ok(());
                    };
                    let snap = this.snapshot();
                    match key {
                        crate::ENTITY_KEY => snap.save_entities(&dir),
                        crate::DEVICE_KEY => snap.save_devices(&dir),
                        _ => snap.save_areas(&dir),
                    }
                })
            };
            Self {
                hass: hass.clone(),
                snap: RwLock::new(Arc::new(registries)),
                writer: Mutex::new(()),
                saves: Some([
                    save(crate::ENTITY_KEY),
                    save(crate::DEVICE_KEY),
                    save(crate::AREA_KEY),
                ]),
                entries: RwLock::new(None),
                read_only,
            }
        })
    }

    /// The entity or device store whose changes can't be saved, if any
    /// (see [`Registries::unwritable`]).
    pub fn read_only(&self) -> Option<&'static str> {
        self.read_only
            .iter()
            .copied()
            .find(|key| *key != crate::AREA_KEY)
    }

    /// Whether `key`'s store (the entities', the devices' or the areas')
    /// can't be written, so a change to its rows would not outlast a
    /// restart.
    pub fn unwritable(&self, key: &str) -> bool {
        self.read_only.contains(&key)
    }

    /// HA's `_write_unavailable_states` at start. No row can be removed
    /// between being found to exist and its placeholder being written, so
    /// whoever removes rows afterwards (a config entry's removal) finds
    /// every placeholder there is. The rows are done a batch at a time
    /// under the writer's lock, each checked against the registry as it is
    /// then, so a change to the registry waits for one batch, not for the
    /// whole pass.
    pub fn write_unavailable_states(&self, hass: &Hass) {
        /// Rows per turn of the lock.
        const BATCH: usize = 64;
        let rows = self.snapshot();
        // The registry as it was last found changed, and its rows by id.
        let mut seen = rows.clone();
        let mut by_id: HashMap<String, Arc<Entity>> = HashMap::new();
        for batch in rows.entities.chunks(BATCH) {
            let turn = self.writer.lock();
            let now = self.snapshot();
            if Arc::ptr_eq(&now, &rows) {
                // Nothing has changed: every row of the batch is there.
                batch.iter().for_each(|e| e.write_unavailable_state(hass));
            } else {
                if !Arc::ptr_eq(&now, &seen) {
                    by_id = now
                        .entities
                        .iter()
                        .map(|e| (e.entity_id.clone(), e.clone()))
                        .collect();
                    seen = now;
                }
                // Each row as it is now, if it is still there.
                batch
                    .iter()
                    .filter_map(|e| by_id.get(&e.entity_id))
                    .for_each(|e| e.write_unavailable_state(hass));
            }
            // Whoever waits to change the registry goes next, rather than
            // this pass taking the lock again for its next batch.
            parking_lot::MutexGuard::unlock_fair(turn);
        }
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

    /// `which`: 0 entities, 1 devices, 2 areas.
    fn schedule_save(&self, which: usize) {
        if let Some(saves) = &self.saves {
            let delay = if self.hass.core_state() == CoreState::Running {
                SAVE_DELAY
            } else {
                SAVE_DELAY_STARTING
            };
            saves[which].schedule(delay);
        }
    }

    /// One write, start to finish under the writer lock: read and change a
    /// copy, swap it in, then publish its events and schedule the saves, so
    /// lookups can't race creation and events come in commit order. A
    /// write that changes nothing makes no copy and swaps nothing in.
    fn write<T>(&self, f: impl FnOnce(&mut Tx) -> T) -> T {
        let _turn = self.writer.lock();
        let mut tx = Tx {
            r: Working::Same(self.snapshot()),
            events: Vec::new(),
            renamed: Vec::new(),
            save_entities: false,
            save_devices: false,
            save_areas: false,
        };
        let out = f(&mut tx);
        if let Working::Changed(r) = tx.r {
            *self.snap.write() = Arc::new(r);
        }
        for (which, save) in [tx.save_entities, tx.save_devices, tx.save_areas]
            .into_iter()
            .enumerate()
        {
            if save {
                self.schedule_save(which);
            }
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
        // HA's `cleanup_restored_states`, a listener of the event: still
        // in this write's turn, so the next change finds it done.
        for (old, row) in tx.renamed {
            if let Ok(old) = ha_core::EntityId::parse(&old)
                && let Some(state) = self.hass.states().get(&old)
                && state.attributes.get("restored") == Some(&Value::Bool(true))
            {
                self.hass.remove_state(&old);
                row.write_unavailable_state(&self.hass);
            }
        }
        out
    }

    /// `async_create` for an area: refused, in HA's words, when the name
    /// (compared as `normalize_name` does) is in use, or a sensor named
    /// for its humidity or temperature has no state or is another kind.
    /// Its id is the name slugified, with `_2`, `_3`, ... when taken
    /// (`generate_id_from_name`). Fires `area_registry_updated` and
    /// schedules the save.
    pub fn create_area(&self, new: NewArea) -> Result<crate::Area, AreaRefusal> {
        let now = now_iso(&self.hass);
        if self.unwritable(crate::AREA_KEY) {
            return Err(AreaRefusal::Unwritable);
        }
        self.write(|tx| {
            let name = &new.name;
            if let Some(area) = tx.r.area_by_name(name) {
                return Err(AreaRefusal::Invalid(format!(
                    "The name {name} ({}) is already in use",
                    crate::normalize_name(&area.name)
                )));
            }
            // `_validate_humidity_entity`, then `_validate_temperature_entity`.
            for (entity_id, class) in [
                (&new.humidity_entity_id, "humidity"),
                (&new.temperature_entity_id, "temperature"),
            ] {
                let Some(entity_id) = entity_id else { continue };
                // `hass.states.get` lower-cases what it is asked for.
                let state = ha_core::EntityId::parse(&entity_id.to_lowercase())
                    .ok()
                    .and_then(|id| self.hass.states().get(&id))
                    .ok_or_else(|| {
                        AreaRefusal::Invalid(format!("Entity {entity_id} does not exist"))
                    })?;
                if state.entity_id.domain() != "sensor"
                    || state.attributes.get("device_class").and_then(Value::as_str) != Some(class)
                {
                    return Err(AreaRefusal::Invalid(format!(
                        "Entity {entity_id} is not a {class} sensor"
                    )));
                }
            }
            let area = new_area(&tx.r.areas, name, &now);
            let id = area.id.clone();
            let area = crate::Area {
                aliases: as_set(&new.aliases),
                floor_id: new.floor_id.clone(),
                humidity_entity_id: new.humidity_entity_id.clone(),
                icon: new.icon.clone(),
                labels: as_set(&new.labels),
                picture: new.picture.clone(),
                temperature_entity_id: new.temperature_entity_id.clone(),
                ..area
            };
            tx.r.areas.push(area.clone());
            tx.events.push((
                "area_registry_updated",
                json!({"action": "create", "area_id": id}),
            ));
            tx.save_areas = true;
            Ok(area)
        })
    }

    /// `async_update_device` as `config/device_registry/update` calls it:
    /// the fields of `patch` that differ are written, `modified_at` is
    /// bumped and one `update` event carries the old values; nothing
    /// happens when nothing differs. The device as it is afterwards.
    ///
    /// `follows` says whether an integration's running entities take a
    /// new device name: a name is refused for a device with an entity
    /// that wouldn't (see [`DeviceRefusal`]). HA's entity registry changes
    /// nothing of an entity that has its own name beside the device's
    /// when the device is named or moved; an entity the device's disable
    /// had disabled is enabled again when a change finds the device
    /// enabled (`async_device_modified`).
    pub fn update_device(
        &self,
        device_id: &str,
        patch: &DevicePatch,
        follows: &dyn Fn(&str) -> bool,
    ) -> Result<Arc<Device>, DeviceRefusal> {
        let now = now_iso(&self.hass);
        self.write(|tx| {
            let i =
                tx.r.devices
                    .iter()
                    .position(|d| d.id == device_id)
                    .ok_or(DeviceRefusal::Unknown)?;
            if self.unwritable(crate::DEVICE_KEY) {
                return Err(DeviceRefusal::Unwritable(crate::DEVICE_KEY));
            }
            let mut d = (*tx.r.devices[i]).clone();
            // The old values, in the order HA goes through the fields.
            let mut changes = Map::new();
            if let Some(area_id) = &patch.area_id
                && d.area_id != *area_id
            {
                changes.insert("area_id".into(), json!(d.area_id));
                d.area_id.clone_from(area_id);
            }
            if patch
                .disabled_by
                .as_ref()
                .is_some_and(|by| *by != d.disabled_by)
            {
                return Err(DeviceRefusal::Disable);
            }
            if let Some(labels) = &patch.labels {
                let new = as_set(labels);
                if new.len() != d.labels.len() || new.iter().any(|l| !d.labels.contains(l)) {
                    let mut old = std::mem::replace(&mut d.labels, new);
                    old.sort_unstable();
                    changes.insert("labels".into(), json!(old));
                }
            }
            if let Some(name) = &patch.name_by_user
                && d.name_by_user != *name
            {
                let cant = tx.r.entities.iter().find_map(|e| {
                    (e.device_id.as_deref() == Some(device_id)).then_some(())?;
                    match e.has_entity_name {
                        false => Some(None),
                        true => (!follows(&e.platform)).then(|| Some(e.platform.clone())),
                    }
                });
                if let Some(platform) = cant {
                    return Err(DeviceRefusal::Rename(platform));
                }
                changes.insert("name_by_user".into(), json!(d.name_by_user));
                d.name_by_user.clone_from(name);
            }
            if changes.is_empty() {
                return Ok(tx.r.devices[i].clone());
            }
            // Entities the device's disable had disabled come back with
            // an enabled device, whatever the change was.
            let stale: Vec<usize> = match d.disabled_by {
                Some(_) => Vec::new(),
                None => (0..tx.r.entities.len())
                    .filter(|&e| {
                        let row = &tx.r.entities[e];
                        row.device_id.as_deref() == Some(device_id)
                            && row.disabled_by.as_deref() == Some("device")
                    })
                    .collect(),
            };
            if !stale.is_empty() && self.unwritable(crate::ENTITY_KEY) {
                return Err(DeviceRefusal::Unwritable(crate::ENTITY_KEY));
            }
            d.modified_at = Some(now.clone());
            let device = Arc::new(d);
            tx.r.devices[i] = device.clone();
            tx.device_event(
                json!({"action": "update", "device_id": device.id, "changes": changes}),
            );
            let enable = EntityPatch {
                disabled_by: Some(None),
                ..Default::default()
            };
            for e in stale {
                update_row(tx, e, &enable, &now, None);
            }
            Ok(device)
        })
    }

    /// `async_regenerate_entity_id` for each id, as
    /// `config/entity_registry/get_automatic_entity_ids` answers: the id
    /// that follows from the names as they are now (an entity keeps its
    /// own id if that is it), or `None` for an id nothing is registered
    /// under. An id given to one is not given to the next.
    pub fn automatic_entity_ids(&self, entity_ids: &[String]) -> Vec<(String, Option<String>)> {
        let r = self.snapshot();
        let mut reserved: Vec<String> = Vec::new();
        entity_ids
            .iter()
            .map(|entity_id| {
                let new = r
                    .entities
                    .iter()
                    .find(|e| e.entity_id == *entity_id)
                    .map(|e| regenerate_entity_id(&r, &self.hass, e, &reserved));
                reserved.extend(new.clone());
                (entity_id.clone(), new)
            })
            .collect()
    }

    /// `async_update_entity` with a `new_entity_id` (and whatever else
    /// `patch` changes, in the same event, which names the old id): the
    /// row as this call left it. `follows` says whether an integration's
    /// running entities take a new id.
    ///
    /// A placeholder state start-up wrote under the old id (an entity
    /// nothing provides) moves to the new one, as HA's
    /// `cleanup_restored_states` moves it. A running entity's state is its
    /// entity's to move: the caller tells it
    /// (`ha_integrations::components::Components::registry_edit`).
    pub fn rename_entity(
        &self,
        entity_id: &str,
        new_entity_id: &str,
        patch: &EntityPatch,
        follows: &dyn Fn(&str) -> bool,
    ) -> Result<Arc<Entity>, RenameRefusal> {
        let now = now_iso(&self.hass);
        self.write(|tx| {
            let i =
                tx.r.entities
                    .iter()
                    .position(|e| e.entity_id == entity_id)
                    .ok_or(RenameRefusal::Unknown)?;
            if self.unwritable(crate::ENTITY_KEY) {
                return Err(RenameRefusal::Unwritable);
            }
            let mut row = (*tx.r.entities[i]).clone();
            let mut changes = apply(&mut row, patch);
            if new_entity_id != entity_id {
                // `_entity_id_available`: not registered, and no state
                // (the state machine lower-cases what it is asked for).
                let state = ha_core::EntityId::parse(&new_entity_id.to_lowercase())
                    .is_ok_and(|id| self.hass.states().get(&id).is_some());
                if state || tx.r.entities.iter().any(|e| e.entity_id == new_entity_id) {
                    return Err(RenameRefusal::Registered);
                }
                let new =
                    ha_core::EntityId::parse(new_entity_id).map_err(|_| RenameRefusal::Invalid)?;
                if Some(new.domain()) != entity_id.split('.').next() {
                    return Err(RenameRefusal::Domain);
                }
                if !follows(&row.platform) {
                    return Err(RenameRefusal::Platform(row.platform.clone()));
                }
                changes.insert("entity_id".into(), json!(entity_id));
                row.entity_id = new_entity_id.to_owned();
            }
            if changes.is_empty() {
                return Ok(tx.r.entities[i].clone());
            }
            row.modified_at = Some(now);
            let row = Arc::new(row);
            tx.r.entities[i] = row.clone();
            let mut event = json!({"action": "update", "entity_id": new_entity_id,
                                   "changes": changes});
            if new_entity_id != entity_id {
                event["old_entity_id"] = json!(entity_id);
                tx.renamed.push((entity_id.to_owned(), row.clone()));
            }
            tx.entity_event(event);
            Ok(row)
        })
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

    /// LIFX's `async_migrate_entities_devices`: each device of config entry
    /// `from` that has `identifier` moves to entry `to`
    /// (`async_update_device(add_config_entry_id=to,
    /// remove_config_entry_id=from)`), and then each entity of `from` on such
    /// a device (`async_update_entity(config_entry_id=to)`). Each change
    /// fires its `update`, as HA's do.
    pub fn move_devices_to_entry(&self, identifier: &Value, from: &str, to: &str) {
        let now = now_iso(&self.hass);
        let to_disabled = self.entry_disabled(Some(to));
        let moved: Vec<String> = self.write(|tx| {
            let mut moved = Vec::new();
            for i in 0..tx.r.devices.len() {
                let old = &tx.r.devices[i];
                if !old.config_entries.iter().any(|e| e == from)
                    || !old.identifiers.contains(identifier)
                {
                    continue;
                }
                let mut d = (**old).clone();
                let mut changes = Map::new();
                if !d.config_entries.iter().any(|e| e == to) {
                    changes.insert("config_entries".into(), json!(old.config_entries));
                    changes.insert(
                        "config_entries_subentries".into(),
                        Value::Object(old.config_entries_subentries.clone()),
                    );
                    d.config_entries.push(to.to_owned());
                    d.config_entries_subentries
                        .insert(to.to_owned(), json!([null]));
                    if d.disabled_by.as_deref() == Some("config_entry") && !to_disabled {
                        changes.insert("disabled_by".into(), json!(d.disabled_by));
                        d.disabled_by = None;
                    }
                }
                changes
                    .entry("config_entries")
                    .or_insert_with(|| json!(old.config_entries));
                changes
                    .entry("config_entries_subentries")
                    .or_insert_with(|| Value::Object(old.config_entries_subentries.clone()));
                d.config_entries_subentries.remove(from);
                d.config_entries.retain(|e| e != from);
                if d.primary_config_entry.as_deref() == Some(from) {
                    changes.insert("primary_config_entry".into(), json!(d.primary_config_entry));
                    d.primary_config_entry = None;
                }
                d.modified_at = Some(now.clone());
                let id = d.id.clone();
                tx.r.devices[i] = Arc::new(d);
                tx.device_event(json!({"action": "update", "device_id": id, "changes": changes}));
                moved.push(id);
            }
            moved
        });
        let rows: Vec<String> = self
            .snapshot()
            .entities
            .iter()
            .filter(|e| e.config_entry_id.as_deref() == Some(from))
            .filter(|e| e.device_id.as_ref().is_some_and(|d| moved.contains(d)))
            .map(|e| e.entity_id.clone())
            .collect();
        for entity_id in rows {
            let patch = EntityPatch {
                config_entry_id: Some(Some(to.to_owned())),
                ..Default::default()
            };
            self.update_entity(&entity_id, patch);
        }
    }

    /// `async_get_or_create`: the existing row, updated with `patch`, or a
    /// new one. `disabled_by` applies only to a new row; a deleted row
    /// that comes back brings its own; then, in HA's order, the entry's
    /// preference ([`EntityPatch::pref_disable_new`]) disables a row that
    /// is still enabled. Fires `entity_registry_updated` and schedules
    /// the save.
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
                // `_async_update_entity`, given an entry and no
                // `disabled_by`: the row's disable follows the entry's. A
                // row a disabled entry disabled is enabled when an enabled
                // entry takes it; the user's own disable stays.
                let disable = match (&patch.disabled_by, &patch.config_entry_id) {
                    (None, Some(entry)) => {
                        match (
                            self.entry_disabled(entry.as_deref()),
                            tx.r.entities[i].disabled_by.as_deref(),
                        ) {
                            (true, None) => Some(Some("config_entry".to_owned())),
                            (false, Some("config_entry")) => Some(None),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                update_row(tx, i, &patch, &now, disable);
                return tx.r.entities[i].clone();
            }
            let object_id_base = patch.object_id_base.clone().flatten();
            let device_id = patch.device_id.clone().flatten();
            // A deleted row for the same unique id comes back: its id,
            // creation time and the user's settings (HA's `deleted_entities`).
            let key = DeletedKey::new((domain.to_owned(), platform.to_owned(), json!(unique_id)));
            let deleted = tx.r.entity_file.deleted.iter().position(|d| key.is(d));
            let deleted = deleted.and_then(|i| {
                let row = Arc::make_mut(&mut tx.r.entity_file.deleted).remove(i);
                DeletedEntity::parse(&row)
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
                    // The user's name has priority, and always loses the
                    // device's name from its front; the integration's
                    // only if it is named the old way.
                    match deleted.as_ref().and_then(|d| d.name.as_deref()) {
                        Some(name) => (Some(name), true),
                        None => (
                            object_id_base.as_deref(),
                            patch.has_entity_name != Some(true),
                        ),
                    },
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
            // After the deleted row's own setting, as in HA: a row the
            // user removed while it was enabled comes back disabled when
            // the entry says new entities start that way.
            let row = match row.disabled_by.is_none() && patch.pref_disable_new {
                true => {
                    let mut r = (*row).clone();
                    r.disabled_by = Some("integration".into());
                    Arc::new(r)
                }
                false => row,
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
            update_row(tx, i, &patch, &now, None)
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
                        update_row(tx, e, &unlink, &now_iso, None);
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
    pub fn get_or_create_device(&self, mut info: DeviceInfo) -> Arc<Device> {
        let now = now_iso(&self.hass);
        let domains = self.entries.read().clone();
        // `_normalize_connections`: a MAC address in HA's one spelling.
        for connection in &mut info.connections {
            if connection[0] == "mac"
                && let Some(mac) = connection[1].as_str()
            {
                *connection = json!(["mac", format_mac(mac)]);
            }
        }
        let areas_unwritable = self.unwritable(crate::AREA_KEY);
        let (identifiers, connections) = (info.identifiers.clone(), info.connections.clone());
        self.write(|tx| {
            let r = &mut tx.r;
            // `async_get_device`: by an identifier first; only if no device
            // has one of them, by a connection (so a device of another
            // integration that shares an address never wins over the row
            // that has this device's own identifier).
            let by_identifier = |ids: &[Value]| ids.iter().any(|i| identifiers.contains(i));
            let by_connection = |cs: &[Value]| cs.iter().any(|c| connections.contains(c));
            let found = r
                .devices
                .iter()
                .position(|d| by_identifier(&d.identifiers))
                .or_else(|| r.devices.iter().position(|d| by_connection(&d.connections)));
            // A deleted device with one of the identifiers, or else one of
            // the connections, comes back (`to_device_entry`): its id,
            // creation time and the user's settings, with this entry.
            let restored = found.is_none().then(|| {
                let rows = &r.device_file.deleted;
                // One pass: a row with an identifier ends it; else the
                // first row with a connection.
                let mut by_address = None;
                let mut by_id = None;
                for (i, row) in rows.iter().enumerate() {
                    let Some(d) = DeletedDevice::parse(row) else {
                        continue;
                    };
                    if by_identifier(&d.identifiers) {
                        by_id = Some((i, d));
                        break;
                    }
                    if by_address.is_none() && by_connection(&d.connections) {
                        by_address = Some((i, d));
                    }
                }
                let (i, d) = by_id.or(by_address)?;
                let mut rows = (**rows).clone();
                rows.remove(i);
                r.device_file.deleted = Arc::new(rows);
                Some(d)
            });
            let restored = restored.flatten();
            // Never seen before: neither registered nor deleted.
            let fresh = found.is_none() && restored.is_none();
            let (i, mut d, is_new) = match (found, restored) {
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
                // "Enable the device if it was disabled by config entry and
                // we're adding a non disabled config entry."
                if d.disabled_by.as_deref() == Some("config_entry")
                    && !self.entry_disabled(Some(entry))
                {
                    changes.insert("disabled_by".into(), json!(d.disabled_by));
                    d.disabled_by = None;
                }
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
            set("entry_type", &mut d.entry_type, &info.entry_type);
            set("manufacturer", &mut d.manufacturer, &info.manufacturer);
            set("model", &mut d.model, &info.model);
            set("model_id", &mut d.model_id, &info.model_id);
            set("name", &mut d.name, &info.name);
            set("sw_version", &mut d.sw_version, &info.sw_version);
            let new_connections: Vec<Value> = info
                .connections
                .iter()
                .filter(|c| !d.connections.contains(c))
                .cloned()
                .collect();
            if !new_connections.is_empty() {
                changes.insert("connections".into(), json!(d.connections));
                d.connections.extend(new_connections);
            }
            if let Some(via) = &info.via_device {
                let via = r
                    .devices
                    .iter()
                    .find(|x| x.identifiers.contains(via))
                    .map(|x| x.id.clone());
                if d.via_device_id != via {
                    changes.insert("via_device_id".into(), json!(d.via_device_id));
                    d.via_device_id = via;
                }
            }
            if !is_new && changes.is_empty() {
                return r.devices[i.expect("found")].clone();
            }
            // `suggested_area`, for a device that is new (one that came
            // back keeps the area it had, or none).
            if fresh && let Some(name) = info.suggested_area.as_deref().filter(|n| !n.is_empty()) {
                d.area_id = match r.area_by_name(name) {
                    Some(area) => Some(area.id.clone()),
                    // An area that could not be saved is not made: the
                    // device's row, which may be saved, would point at an
                    // area gone after a restart.
                    None if areas_unwritable => None,
                    None => {
                        let area = new_area(&r.areas, name, &now);
                        let id = area.id.clone();
                        r.areas.push(area);
                        tx.events.push((
                            "area_registry_updated",
                            json!({"action": "create", "area_id": id}),
                        ));
                        tx.save_areas = true;
                        Some(id)
                    }
                };
            }
            d.modified_at = Some(now.clone());
            let renamed = !is_new && changes.contains_key("name");
            let enabled = changes.contains_key("disabled_by");
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
            if !is_new && enabled {
                // `async_device_modified`: entities the device's disable
                // had disabled come back with it.
                let enable = EntityPatch {
                    disabled_by: Some(None),
                    ..Default::default()
                };
                for e in 0..tx.r.entities.len() {
                    let row = &tx.r.entities[e];
                    if row.device_id.as_deref() == Some(device.id.as_str())
                        && row.disabled_by.as_deref() == Some("device")
                    {
                        update_row(tx, e, &enable, &now, None);
                    }
                }
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
    let key = DeletedKey::new(deleted.key());
    // Changed in place: the rows are copied once for the write, however
    // many of them it removes.
    let rows = Arc::make_mut(&mut tx.r.entity_file.deleted);
    rows.retain(|d| !key.is(d));
    rows.extend(deleted.raw());
    tx.entity_event(json!({"action": "remove", "entity_id": row.entity_id}));
}

/// Which deleted row is the one for an entity (its domain, platform and
/// unique id). A unique id of plain characters is looked for in a row's
/// text first, so that only a row that has it is parsed: parsing every
/// deleted row for each entity removed made removing an entry's entities
/// quadratic.
struct DeletedKey {
    key: (String, String, Value),
    /// The unique id as it stands in a row's JSON when its writer spelled
    /// every character as itself, as HA and `hars` do for these.
    text: Option<String>,
}

impl DeletedKey {
    fn new(key: (String, String, Value)) -> Self {
        let plain = |c: char| c.is_ascii_alphanumeric() || " _-.:".contains(c);
        let text = match &key.2 {
            Value::String(id) if id.chars().all(plain) => Some(format!("\"{id}\"")),
            _ => None,
        };
        Self { key, text }
    }

    fn is(&self, row: &RawRow) -> bool {
        // A row that spells a character as `\u….` may spell the id so.
        if let Some(text) = &self.text
            && !row.get().contains(text.as_str())
            && !row.get().contains("\\u")
        {
            return false;
        }
        DeletedEntity::parse(row).map(|d| d.key()).as_ref() == Some(&self.key)
    }
}

fn is_unique(e: &Entity, domain: &str, platform: &str, unique_id: &str) -> bool {
    e.platform == platform
        && e.entity_id.split('.').next() == Some(domain)
        && e.unique_id.as_ref().and_then(Value::as_str) == Some(unique_id)
}

/// Apply `patch` to row `i`: on a change, bump `modified_at` and queue the
/// `update` event with the old values.
///
/// `disable` is a disable that follows the row's config entry: applied
/// like any change, but HA leaves it out of the event's `changes`.
fn update_row(
    tx: &mut Tx,
    i: usize,
    patch: &EntityPatch,
    now: &str,
    disable: Option<Option<String>>,
) -> Option<Map<String, Value>> {
    let mut row = (*tx.r.entities[i]).clone();
    let changes = apply(&mut row, patch);
    let followed = disable.is_some();
    if let Some(disabled_by) = disable {
        row.disabled_by = disabled_by;
    }
    if changes.is_empty() && !followed {
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
    set(&mut changes, "area_id", &mut row.area_id, &patch.area_id);
    if let Some(named) = &patch.categories {
        // Merged here, in the write, so two updates of different scopes
        // can't undo each other.
        let old = row.categories.object();
        let mut new = old.clone();
        for (scope, category) in named {
            if category.is_null() {
                new.remove(scope);
            } else {
                new.insert(scope.clone(), category.clone());
            }
        }
        if old != new {
            changes.insert("categories".into(), Value::Object(old));
            row.categories = RawJson::new(&Value::Object(new));
        }
    }
    if let Some(labels) = &patch.labels {
        // Sets in HA: the same labels in another order are no change.
        let mut new: Vec<String> = Vec::with_capacity(labels.len());
        for l in labels {
            if !new.contains(l) {
                new.push(l.clone());
            }
        }
        let same = new.len() == row.labels.len() && new.iter().all(|l| row.labels.contains(l));
        if !same {
            let mut old = row.labels.clone();
            old.sort_unstable();
            changes.insert("labels".into(), json!(old));
            row.labels = new;
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
    (object_id_base, unprefix): (Option<&str>, bool),
) -> String {
    let device = device_id.and_then(|id| r.devices.iter().find(|d| d.id == id));
    let device_name = device.and_then(|d| d.display_name());
    // The entity's own area, else its device's.
    let area_name = area_id
        .or_else(|| device.and_then(|d| d.area_id.as_deref()))
        .and_then(|a| r.areas.iter().find(|x| x.id == a))
        .map(|a| a.name.as_str());
    let entity_name = match (object_id_base, device_name) {
        (Some(name), Some(device)) if unprefix => {
            strip_prefix(name, device).or_else(|| Some(name.to_owned()))
        }
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

/// `async_regenerate_entity_id`: the id `row` would get from the names as
/// they are now. Its user's name comes first (after the device's, with
/// that prefix stripped), then `suggested_object_id` on its own, then
/// what the integration calls it; `reserved` ids are not given.
fn regenerate_entity_id(r: &Registries, hass: &Hass, row: &Entity, reserved: &[String]) -> String {
    let domain = row.entity_id.split('.').next().unwrap_or_default();
    let object_id = match (&row.name, &row.suggested_object_id) {
        // On its own, even when empty (then the fallback below).
        (None, Some(suggested)) => suggested.clone(),
        _ => {
            let device = row
                .device_id
                .as_deref()
                .and_then(|id| r.devices.iter().find(|d| d.id == id));
            let device_name = device.and_then(|d| d.display_name());
            // The entity's own area, else its device's.
            let area_name = row
                .area_id
                .as_deref()
                .or_else(|| device.and_then(|d| d.area_id.as_deref()))
                .and_then(|a| r.areas.iter().find(|x| x.id == a))
                .map(|a| a.name.as_str());
            let unprefixed = |name: &str| device_name.and_then(|device| strip_prefix(name, device));
            let entity_name = match (&row.name, &row.object_id_base) {
                (Some(name), _) => Some(unprefixed(name).unwrap_or_else(|| name.clone())),
                // Only a name of the old kind carries the device's.
                (None, Some(base)) if !row.has_entity_name => {
                    Some(unprefixed(base).unwrap_or_else(|| base.clone()))
                }
                (None, base) => base.clone(),
            };
            [area_name, device_name, entity_name.as_deref()]
                .into_iter()
                .flatten()
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        }
    };
    let unique_id = match &row.unique_id {
        Some(Value::String(id)) => id.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    let object_id = match object_id.is_empty() {
        true => format!("{}_{unique_id}", row.platform),
        false => object_id,
    };
    available_id(r, hass, domain, &object_id, Some(&row.entity_id), reserved)
}

/// `async_get_available_entity_id`: `<domain>.<slug>`, cut to 255, with
/// `_2`, `_3`, … while the registry or the state machine has it.
pub fn available_entity_id(r: &Registries, hass: &Hass, domain: &str, object_id: &str) -> String {
    available_id(r, hass, domain, object_id, None, &[])
}

/// [`available_entity_id`], where `current` (the entity's own id) counts
/// as free and the `reserved` ids as taken.
fn available_id(
    r: &Registries,
    hass: &Hass,
    domain: &str,
    object_id: &str,
    current: Option<&str>,
    reserved: &[String],
) -> String {
    let preferred = format!("{domain}.{}", ha_core::slugify(object_id));
    let taken = |id: &str| {
        let used = r.entities.iter().any(|e| e.entity_id == id)
            || ha_core::EntityId::parse(id)
                .ok()
                .is_some_and(|id| hass.states().get(&id).is_some());
        (used && Some(id) != current) || reserved.iter().any(|r| r == id)
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

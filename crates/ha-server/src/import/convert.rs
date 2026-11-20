//! What Home Assistant 2026.8 to 2026.11 store differently, turned back into
//! the 2026.6.1 form `hars` reads and writes. It runs on the staged copy,
//! before the checks every import makes, so the source is never written and
//! nothing reaches the destination unless every store converts and loads.
//!
//! Home Assistant has no code that goes back a version. Each rule here is
//! the reverse of the migration named beside it, or the view of the newer
//! data that Home Assistant itself gives code written for the old form
//! (its compatibility properties). Where neither exists the data is kept
//! as it is on the row (`hars` writes unknown fields back), or the import
//! stops. The rules go by storage version, not by release:
//!
//! - `core.device_registry` 3.2 and 3.4 (2026.8, 2026.9) to 1.12:
//!   [`device_from_v3`], [`deleted_device_from_v3`].
//! - `core.entity_registry` 1.23 and 1.24 (2026.8, 2026.11) to 1.22:
//!   [`convert_entities`].
//! - `core.restore_state` rows with `entity_registry_id` (2026.11, no
//!   version change): [`convert_restore_state`].
//! - `lifx` entries at version 2.1 (2026.10) and `met` entries at 2.1
//!   (2026.11) to 1.1: [`convert_entries`].
//!
//! The test of all of it is a round trip made with Home Assistant's own
//! code (`tests/import.rs`, `scripts/vectors/import_releases.py`): one
//! 2026.6.1 install, started on by each later release, must come back as
//! it was except for the differences that test lists.

use super::{runs, sanitized, too_new, too_old};
use anyhow::bail;
use ha_core::storage::Store;
use ha_registry::{DEVICE_KEY, Device, ENTITY_KEY, Entity, FileRest, RawRow, Registries};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

const ENTRIES_KEY: &str = "core.config_entries";
const RESTORE_KEY: &str = ha_core::restore::KEY;

/// The device registry versions converted: 3.2 is 2026.8's, 3.4 is what
/// 2026.9 to 2026.11 write. (3.3 was in no release.)
const DEVICE_FROM: &[Version] = &[(3, 2), (3, 4)];
/// The entity registry versions converted: 2026.8's and 2026.11's.
const ENTITY_FROM: &[Version] = &[(1, 23), (1, 24)];
/// `(domain, version, minor_version)` of the config entries converted.
const ENTRY_FROM: &[(&str, u64, u64)] = &[("lifx", 2, 1), ("met", 2, 1)];

/// Whether an entry is one [`convert_entries`] brings back to version 1.
pub(super) fn entry_converts(domain: &str, version: u64, minor: u64) -> bool {
    ENTRY_FROM.contains(&(domain, version, minor))
}

/// What the conversion did, for the report.
#[derive(Debug, Default)]
pub(super) struct Conversion {
    pub stores: Vec<ConvertedStore>,
    pub entries: Vec<ConvertedEntry>,
    pub dropped: Vec<Dropped>,
    pub split_devices: Vec<SplitDevice>,
    pub parent_devices: Vec<ParentDevice>,
}

/// A store rewritten in the form of an older storage version.
#[derive(Debug, Serialize)]
pub struct ConvertedStore {
    pub key: String,
    /// The storage version the file had, `major.minor`.
    pub from: String,
    /// The one it has now, which `hars` reads and writes.
    pub to: String,
    /// What changed, in words.
    pub changes: Vec<String>,
}

/// A config entry brought back to the version `hars` runs.
#[derive(Debug, Serialize)]
pub struct ConvertedEntry {
    pub entry_id: String,
    pub domain: String,
    pub title: String,
    pub from: String,
    pub to: String,
    pub changes: Vec<String>,
}

/// Something the 2026.6.1 form has no place for, left out.
#[derive(Debug, Serialize)]
pub struct Dropped {
    pub store: String,
    pub what: String,
    pub reason: String,
}

/// A device two integrations shared, which Home Assistant 2026.8 replaced
/// with one device for each. They stay apart, with the ids 2026.8 gave
/// them: entities already point at those.
#[derive(Debug, Serialize)]
pub struct SplitDevice {
    /// The id the shared device had. Nothing has it now.
    pub old_id: String,
    pub new_ids: Vec<String>,
    pub name: Option<String>,
    /// The YAML files and dashboards that still use `old_id`. Home
    /// Assistant 2026.8 and later take it to mean all of `new_ids`; `hars`
    /// (as 2026.6.1) finds no device by it.
    pub used_in: Vec<String>,
}

/// A storage version: major, minor.
type Version = (u32, u32);

/// A device two integrations shared, as 2026.8 left it: the id it had,
/// and the devices it became (id, config entry, subentry).
type Split = (String, Vec<(String, String, Option<String>)>);

/// A device with child devices (2026.9). Home Assistant 2026.9 and later
/// take an action on it to mean its children's entities too
/// (`helpers/target.py`); `hars`, as 2026.6.1, reaches only its own.
#[derive(Debug, Serialize)]
pub struct ParentDevice {
    pub id: String,
    pub name: Option<String>,
    /// The devices that were its children.
    pub children: Vec<String>,
    /// The YAML files and dashboards that use `id`.
    pub used_in: Vec<String>,
}

#[derive(Deserialize)]
struct Head {
    version: u32,
    minor_version: Option<u32>,
}

/// A staged store's storage version, if it is there and is a store.
fn head(root: &Path, key: &str) -> Option<(u32, u32)> {
    let bytes = std::fs::read(root.join(".storage").join(key)).ok()?;
    let head: Head = serde_json::from_slice(&bytes).ok()?;
    // `Store.async_load`: no minor version means 1.
    Some((head.version, head.minor_version.unwrap_or(1)))
}

fn dotted((major, minor): (u32, u32)) -> String {
    format!("{major}.{minor}")
}

/// `1 device`, `2 devices`.
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

#[derive(Deserialize, Default)]
struct DeviceRows {
    #[serde(default)]
    devices: Vec<Map<String, Value>>,
    /// 3.4 (2026.9).
    #[serde(default)]
    child_devices: Vec<Map<String, Value>>,
    #[serde(default)]
    deleted_devices: Vec<RawRow>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Deserialize, Default)]
struct EntityRows {
    #[serde(default)]
    entities: Vec<Entity>,
    #[serde(default)]
    deleted_entities: Vec<RawRow>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// Convert the staged stores in `root` that are in a newer form `hars` has
/// a rule for. A store of a version it has none for stops the import here,
/// named, as does anything in a convertible store that has no 2026.6.1
/// meaning.
pub(super) fn run(root: &Path, ha_version: &str) -> anyhow::Result<Conversion> {
    let mut done = Conversion::default();
    let native: [(&str, Version, &[Version]); 3] = [
        (ENTRIES_KEY, (1, 5), &[]),
        (DEVICE_KEY, (1, 12), DEVICE_FROM),
        (ENTITY_KEY, (1, 22), ENTITY_FROM),
    ];
    let mut heads = Vec::new();
    for (key, reads, converts) in native {
        let found = head(root, key);
        if let Some(found) = found.filter(|f| *f != reads && !converts.contains(f)) {
            // The three depend on each other (names, owners), so none is
            // converted beside one that can't be read.
            if found > reads {
                bail!("{}", too_new(key, found, reads, ha_version));
            }
            bail!("{}", too_old(key, found, reads));
        }
        heads.push(found);
    }
    let (device_head, entity_head) = (heads[1], heads[2]);

    // The config entries, as stored: their domains say whose a device is.
    let mut entries: Option<Map<String, Value>> = Store::new(root, ENTRIES_KEY, 1, 5, true)
        .load()
        .map_err(|e| anyhow::anyhow!("{ENTRIES_KEY} doesn't load ({})", sanitized(&e)))?;
    let entry_list = |entries: &Option<Map<String, Value>>| -> Vec<Map<String, Value>> {
        entries
            .as_ref()
            .and_then(|d| d.get("entries"))
            .and_then(Value::as_array)
            .map(|rows| rows.iter().filter_map(|e| e.as_object().cloned()).collect())
            .unwrap_or_default()
    };
    let before = entry_list(&entries);
    let text = |e: &Map<String, Value>, k: &str| {
        e.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let domain_of: HashMap<String, String> = before
        .iter()
        .map(|e| (text(e, "entry_id"), text(e, "domain")))
        .collect();
    // The entries about to go back to version 1, by id.
    let converting = |domain: &str| -> HashSet<String> {
        before
            .iter()
            .filter(|e| text(e, "domain") == domain)
            .filter(|e| {
                let n = |k: &str| e.get(k).and_then(Value::as_u64).unwrap_or(1);
                entry_converts(domain, n("version"), n("minor_version"))
            })
            .map(|e| text(e, "entry_id"))
            .collect()
    };
    let (lifx, met) = (converting("lifx"), converting("met"));

    // ----- devices -----
    let device_major = device_head.map_or(1, |h| h.0);
    let rows: DeviceRows = Store::new(root, DEVICE_KEY, device_major, 1, false)
        .load()
        .map_err(|e| anyhow::anyhow!("{DEVICE_KEY} doesn't load ({})", sanitized(&e)))?
        .unwrap_or_default();
    let from_v3 = device_major == 3;
    let mut device_changes = Vec::new();
    // The shared devices 2026.8 split: old id, then the devices it became
    // (id, config entry, subentry).
    let mut splits: Vec<Split> = Vec::new();
    let mut devices: Vec<Device> = Vec::new();
    let mut deleted = rows.deleted_devices;
    let mut inherited = 0;
    if from_v3 {
        let children = rows.child_devices.len();
        let parents: HashSet<&str> = rows
            .devices
            .iter()
            .filter_map(|d| d.get("id").and_then(Value::as_str))
            .collect();
        for child in &rows.child_devices {
            let parent = child.get("parent_device_id").and_then(Value::as_str);
            if !parent.is_some_and(|p| parents.contains(p)) {
                // Home Assistant drops such a row when it loads the file.
                bail!(
                    "{DEVICE_KEY}: the child device {} has no parent device in the file. \
                     Nothing was imported",
                    id_of(child)
                );
            }
        }
        for row in &rows.devices {
            if let Some(old) = row.get("composite_device_id").and_then(Value::as_str) {
                let part = (
                    id_of(row),
                    text(row, "config_entry_id"),
                    row.get("config_subentry_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                );
                match splits.iter_mut().find(|(id, _)| id == old) {
                    Some((_, parts)) => parts.push(part),
                    None => splits.push((old.to_owned(), vec![part])),
                }
            }
        }
        let active = rows.devices.len();
        for row in rows.devices {
            devices.push(device_from_v3(row, false)?);
        }
        for row in rows.child_devices {
            let parent = text(&row, "parent_device_id");
            let mut child = device_from_v3(row, true)?;
            // `async_get_effective_area_id`: a child device without an
            // area of its own is in its parent's. A device has only its
            // own, so that is where it is written.
            let area = devices.iter().find(|d| d.id == parent).map(|d| &d.area_id);
            if child.area_id.is_none() && area.is_some_and(Option::is_some) {
                child.area_id = area.cloned().flatten();
                inherited += 1;
            }
            match done.parent_devices.iter_mut().find(|p| p.id == parent) {
                Some(p) => p.children.push(child.id.clone()),
                None => done.parent_devices.push(ParentDevice {
                    name: devices
                        .iter()
                        .find(|d| d.id == parent)
                        .and_then(|d| d.display_name().map(str::to_owned)),
                    id: parent,
                    children: vec![child.id.clone()],
                    used_in: Vec::new(),
                }),
            }
            devices.push(child);
        }
        deleted = deleted
            .iter()
            .map(deleted_device_from_v3)
            .collect::<anyhow::Result<_>>()?;
        device_changes.push(format!(
            "{} and {} list their config entry the way 2026.6.1 does (`config_entries`, \
             `config_entries_subentries`, `primary_config_entry`)",
            count(active + children, "device", "devices"),
            count(deleted.len(), "removed device", "removed devices")
        ));
        if children > 0 {
            device_changes.push(format!(
                "{} listed with the devices again; the parent's id stays on the row as \
                 `parent_device_id`",
                count(children, "child device is", "child devices are")
            ));
        }
        if inherited > 0 {
            device_changes.push(format!(
                "{} the area of its parent, which Home Assistant 2026.9 takes it to be in",
                count(inherited, "of them has", "of them have")
            ));
        }
        if !splits.is_empty() {
            device_changes.push(format!(
                "the parts of {} that Home Assistant 2026.8 split by integration stay apart, \
                 with the ids it gave them",
                count(splits.len(), "device", "devices")
            ));
        }
    } else {
        for row in rows.devices {
            let id = id_of(&row);
            devices.push(serde_json::from_value(Value::Object(row)).map_err(|_| bad_row(&id))?);
        }
    }
    // What an entity's name is put after, as 2026.11 has it (before the
    // `met` devices get their 2026.6.1 name back, below).
    let device_names: HashMap<String, String> = devices
        .iter()
        .filter_map(|d| Some((d.id.clone(), d.display_name()?.to_owned())))
        .collect();
    let owned_by =
        |d: &Device, ids: &HashSet<String>| d.config_entries.iter().any(|e| ids.contains(e));

    // `lifx` 2 to 1, `components/lifx/migration.py::async_migrate_serials`
    // reversed: the serial in the identifiers of the entry's devices is
    // written as a MAC address again.
    let mut lifx_devices = 0;
    for d in devices.iter_mut().filter(|d| owned_by(d, &lifx)) {
        for identifier in &mut d.identifiers {
            let serial = (identifier[0] == "lifx")
                .then(|| identifier[1].as_str().and_then(colon_serial))
                .flatten();
            if let Some(serial) = serial {
                identifier[1] = Value::String(serial);
                lifx_devices += 1;
            }
        }
    }
    if lifx_devices > 0 {
        device_changes.push(format!(
            "{} the serial with colons again",
            count(lifx_devices, "lifx identifier has", "lifx identifiers have")
        ));
    }
    // `met` 2 to 1: 2026.11's `components/met/weather.py` names the device
    // after the place; 2026.6.1's names it "Forecast" and gives the place's
    // name to the entity. Both write the name at every start.
    let mut met_devices = 0;
    for d in devices.iter_mut().filter(|d| owned_by(d, &met)) {
        let is_met = |i: &Value| i[0] == "met";
        if d.identifiers.iter().any(is_met) && d.name.as_deref() != Some("Forecast") {
            d.name = Some("Forecast".into());
            met_devices += 1;
        }
    }
    if met_devices > 0 {
        device_changes.push(format!(
            "{} named \"Forecast\" again, as in 2026.6.1",
            count(met_devices, "met device is", "met devices are")
        ));
    }
    // Only where version 3 let two devices hold one identifier: a file of
    // 1.12 keeps the order it has.
    if from_v3 {
        let holders: Vec<Holder> = devices
            .iter()
            .map(|d| {
                (
                    d.id.clone(),
                    d.identifiers.clone(),
                    d.config_entries.clone(),
                )
            })
            .collect();
        let order = order_shared_identifiers(&holders, &domain_of, true)?;
        let removed: Vec<Holder> = deleted.iter().map(deleted_holder).collect();
        let removed_order = order_shared_identifiers(&removed, &domain_of, false)?;
        if order.is_some() || removed_order.is_some() {
            device_changes.push(
                "a device is listed before another that still holds a copy of its identifier"
                    .into(),
            );
        }
        if let Some(order) = order {
            devices = order.into_iter().map(|i| devices[i].clone()).collect();
        }
        if let Some(order) = removed_order {
            deleted = order.into_iter().map(|i| deleted[i].clone()).collect();
        }
    }
    if !device_changes.is_empty() {
        Registries {
            devices: devices.iter().cloned().map(Arc::new).collect(),
            device_file: FileRest {
                minor: 12,
                deleted: Arc::new(deleted),
                extra: rows.extra,
            },
            ..Default::default()
        }
        .save_devices(root)?;
        done.stores.push(ConvertedStore {
            key: DEVICE_KEY.into(),
            from: dotted(device_head.unwrap_or((1, 12))),
            to: "1.12".into(),
            changes: device_changes,
        });
    }

    // ----- entities -----
    let entity_rows: EntityRows = Store::new(root, ENTITY_KEY, 1, 22, false)
        .load()
        .map_err(|e| anyhow::anyhow!("{ENTITY_KEY} doesn't load ({})", sanitized(&e)))?
        .unwrap_or_default();
    let core_config = ha_core::config::CoreConfig::load(root).ok();
    let (entities, entity_changes) = convert_entities(
        entity_rows,
        entity_head.unwrap_or((1, 22)),
        &Names {
            devices: &devices,
            device_names: &device_names,
            splits: &splits,
            lifx: &lifx,
            met: &met,
            met_names: before
                .iter()
                .filter(|e| met.contains(&text(e, "entry_id")))
                .map(|e| {
                    let home = e
                        .get("data")
                        .and_then(|d| d.get("track_home"))
                        .and_then(Value::as_bool)
                        == Some(true);
                    let name = match (home, &core_config) {
                        (true, Some(config)) => Some(config.location_name.clone()),
                        (true, None) => None,
                        (false, _) => Some(text(e, "title")),
                    };
                    (text(e, "entry_id"), name)
                })
                .collect(),
        },
        &mut done.dropped,
    )?;
    let registry_ids: HashMap<String, String> = entities
        .entities
        .iter()
        .filter_map(|e| Some((e.id.clone()?, e.entity_id.clone())))
        .collect();
    if !entity_changes.is_empty() {
        Registries {
            entities: entities.entities.into_iter().map(Arc::new).collect(),
            entity_file: FileRest {
                minor: 22,
                deleted: Arc::new(entities.deleted_entities),
                extra: entities.extra,
            },
            ..Default::default()
        }
        .save_entities(root)?;
        done.stores.push(ConvertedStore {
            key: ENTITY_KEY.into(),
            from: dotted(entity_head.unwrap_or((1, 22))),
            to: "1.22".into(),
            changes: entity_changes,
        });
    }

    convert_restore_state(root, &registry_ids, ha_version, &mut done)?;

    if let Some(data) = &mut entries {
        done.entries = convert_entries(data)?;
        if !done.entries.is_empty() {
            Store::new(root, ENTRIES_KEY, 1, 5, true).save(data)?;
        }
    }

    done.split_devices = splits
        .into_iter()
        .map(|(old_id, parts)| SplitDevice {
            name: parts.first().and_then(|p| device_names.get(&p.0)).cloned(),
            new_ids: parts.into_iter().map(|p| p.0).collect(),
            old_id,
            used_in: Vec::new(),
        })
        .collect();
    Ok(done)
}

fn id_of(row: &Map<String, Value>) -> String {
    row.get("id")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_owned()
}

/// A row with a field of the wrong type. serde's own message can quote the
/// file, so it isn't passed on.
fn bad_row(id: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{DEVICE_KEY}: the device {id} has a field of a type Home Assistant doesn't write. \
         Nothing was imported"
    )
}

/// A device row of storage version 3 (`DeviceEntry.as_storage_fragment`,
/// or `ChildDeviceEntry`'s) as version 1.12 has it.
///
/// Version 3 (`DeviceRegistryStore._async_migrate_func`) gave every device
/// one config entry. The three fields 1.12 has instead are what Home
/// Assistant 2026.8 itself answers for them (`BaseDeviceEntry`'s deprecated
/// `config_entries`, `config_entries_subentries` and
/// `primary_config_entry`): the one entry, its subentry, and the entry
/// again.
///
/// A device split from a shared one keeps `composite_device_id`,
/// `composite_primary_config_entry`, `split_at` and, while its integration
/// hasn't registered it again, `has_composite_identifiers`: unknown fields
/// to `hars`, written back with the row. Putting the parts back together
/// would need the old id in every entity, automation and dashboard that
/// has the new ones, and the parts may have been renamed apart.
///
/// A child device (3.4) has no hardware fields; code that asks for them
/// gets nothing (`_CHILD_DEVICE_COMPAT_ATTRS`), and so does the row here.
/// Its parent's id stays on it as `parent_device_id`. One disabled through
/// its parent (`disabled_by: "device"`) has no 2026.6.1 equivalent and
/// stops the import.
fn device_from_v3(mut row: Map<String, Value>, child: bool) -> anyhow::Result<Device> {
    let id = id_of(&row);
    let Some(Value::String(entry)) = row.remove("config_entry_id") else {
        bail!(
            "{DEVICE_KEY}: the device {id} has no config entry, which storage version 3 \
             doesn't allow. Nothing was imported"
        );
    };
    let subentry = match row.remove("config_subentry_id") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(s)) => Value::String(s),
        Some(_) => return Err(bad_row(&id)),
    };
    row.insert("config_entries".into(), json!([entry]));
    row.insert(
        "config_entries_subentries".into(),
        json!({ entry.as_str(): [subentry] }),
    );
    row.insert("primary_config_entry".into(), json!(entry));
    for key in [
        "composite_device_id",
        "composite_primary_config_entry",
        "split_at",
    ] {
        if row.get(key).is_some_and(Value::is_null) {
            row.remove(key);
        }
    }
    if row.get("has_composite_identifiers") == Some(&Value::Bool(false)) {
        row.remove("has_composite_identifiers");
    }
    if child && row.get("disabled_by").and_then(Value::as_str) == Some("device") {
        bail!(
            "{DEVICE_KEY}: the child device {id} is disabled because its parent device is \
             (`disabled_by: device`), which Home Assistant 2026.6.1 has no way to say. Enable \
             the parent device in Home Assistant, then copy the config directory again. \
             Nothing was imported"
        );
    }
    serde_json::from_value(Value::Object(row)).map_err(|_| bad_row(&id))
}

/// A `deleted_devices` row of version 1.12, in Home Assistant's key order
/// (`DeletedDeviceEntry.as_storage_fragment`).
#[derive(Serialize)]
struct DeletedDevice {
    area_id: Value,
    config_entries: Vec<String>,
    config_entries_subentries: Map<String, Value>,
    connections: Value,
    created_at: Value,
    disabled_by: Value,
    disabled_by_undefined: Value,
    identifiers: Value,
    id: Value,
    labels: Value,
    modified_at: Value,
    name_by_user: Value,
    orphaned_timestamp: Value,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// A removed device's row of storage version 3 as 1.12 has it: the same
/// three fields as [`device_from_v3`], empty for one whose config entry is
/// gone (an orphan). `domain`, new in version 3, stays on the row when it
/// is set. (2026.8 brings a removed device back for the config entry it
/// belonged to, or an orphan for an integration of its `domain`; `hars`,
/// as 2026.6.1, for whoever registers one of its identifiers.)
fn deleted_device_from_v3(raw: &RawRow) -> anyhow::Result<RawRow> {
    let mut row: Map<String, Value> =
        serde_json::from_str(raw.get()).map_err(|_| bad_row("(removed)"))?;
    let id = id_of(&row);
    let entry = match row.remove("config_entry_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s),
        Some(_) => return Err(bad_row(&id)),
    };
    let subentry = match row.remove("config_subentry_id") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(s)) => Value::String(s),
        Some(_) => return Err(bad_row(&id)),
    };
    if row.get("domain").is_some_and(Value::is_null) {
        row.remove("domain");
    }
    // A removed child device that was disabled through its parent: Home
    // Assistant clears that when the device comes back
    // (`DeletedDeviceEntry._calculate_disable_by`), and 2026.6.1 has no
    // such state.
    if row.get("disabled_by").and_then(Value::as_str) == Some("device") {
        row.insert("disabled_by".into(), Value::Null);
    }
    let mut take = |key: &str, missing: Value| row.remove(key).unwrap_or(missing);
    let out = DeletedDevice {
        area_id: take("area_id", Value::Null),
        config_entries: entry.iter().cloned().collect(),
        config_entries_subentries: entry
            .iter()
            .map(|e| (e.clone(), json!([subentry])))
            .collect(),
        connections: take("connections", json!([])),
        created_at: take("created_at", Value::Null),
        disabled_by: take("disabled_by", Value::Null),
        disabled_by_undefined: take("disabled_by_undefined", json!(false)),
        identifiers: take("identifiers", json!([])),
        id: take("id", Value::Null),
        labels: take("labels", json!([])),
        modified_at: take("modified_at", Value::Null),
        name_by_user: take("name_by_user", Value::Null),
        orphaned_timestamp: take("orphaned_timestamp", Value::Null),
        extra: row,
    };
    serde_json::value::to_raw_value(&out).map_err(|_| bad_row(&id))
}

/// A raw LIFX serial (`d073d5000001`, what 2026.10's `normalize_serial`
/// leaves) written as 2026.6.1 writes it, like a MAC address
/// (`formatted_serial`: `d0:73:d5:00:00:01`). `None` for anything else.
fn colon_serial(raw: &str) -> Option<String> {
    let hex = raw.len() == 12
        && raw
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    hex.then(|| {
        raw.as_bytes()
            .chunks(2)
            .map(|pair| String::from_utf8_lossy(pair).into_owned())
            .collect::<Vec<_>>()
            .join(":")
    })
}

/// A device row for [`order_shared_identifiers`]: its id, identifiers and
/// config entries.
type Holder = (String, Vec<Value>, Vec<String>);

fn deleted_holder(raw: &RawRow) -> Holder {
    #[derive(Deserialize, Default)]
    struct Row {
        #[serde(default)]
        id: String,
        #[serde(default)]
        identifiers: Vec<Value>,
        #[serde(default)]
        config_entries: Vec<String>,
    }
    let row: Row = serde_json::from_str(raw.get()).unwrap_or_default();
    (row.id, row.identifiers, row.config_entries)
}

/// Rows that share an identifier of an integration `hars` runs: the one
/// that integration owns goes first. The new order, if it differs.
///
/// Until 2026.8 an identifier named one device. Since then two devices of
/// different config entries may hold the same one (Home Assistant finds a
/// device by identifier and config entry), and each part of a split device
/// starts with a copy of them all until its integration registers it
/// again. `hars`'s integrations find their device by identifier alone, the
/// first row that has it, and bring a removed device back the same way. So
/// the row of the integration's own config entry is put before the copies.
///
/// `strict` (the devices, not the removed ones): where that can't be done
/// the import stops, because `hars` would find the wrong device. That is
/// two devices of the same integration with one identifier (two phones'
/// apps that give one device id, say, which 2026.8 keeps apart by config
/// entry), and two devices that each hold a copy of the other's.
fn order_shared_identifiers(
    rows: &[Holder],
    domain_of: &HashMap<String, String>,
    strict: bool,
) -> anyhow::Result<Option<Vec<usize>>> {
    let own = |i: usize, domain: &str| {
        let owners = &rows[i].2;
        owners
            .iter()
            .any(|e| domain_of.get(e).map(String::as_str) == Some(domain))
    };
    // Each identifier `hars` looks up, with the rows that hold it.
    let mut shared: Vec<(Value, Vec<usize>)> = Vec::new();
    for (i, (_, identifiers, _)) in rows.iter().enumerate() {
        for identifier in identifiers {
            if identifier[0].as_str().is_some_and(runs) {
                match shared.iter_mut().find(|(id, _)| id == identifier) {
                    Some((_, at)) => at.push(i),
                    None => shared.push((identifier.clone(), vec![i])),
                }
            }
        }
    }
    shared.retain(|(_, at)| at.len() > 1);
    let refuse = |a: usize, b: usize, domain: &str| {
        anyhow::anyhow!(
            "{DEVICE_KEY}: the devices {} and {} both have an identifier of the {domain} \
             integration, and hars, which finds a device by its identifier alone as Home \
             Assistant 2026.6.1 does, can't tell which one the integration means. Home \
             Assistant 2026.8 and later tell them apart by config entry. Remove one of the \
             two devices in Home Assistant, or start Home Assistant once with both of their \
             integrations loaded, then copy the config directory again. Nothing was imported",
            rows[a].0,
            rows[b].0
        )
    };
    let mut order: Vec<usize> = (0..rows.len()).collect();
    // Each pass moves one row up, so the passes end unless two rows must
    // each come before the other.
    for _ in 0..=rows.len() * rows.len() {
        let place = |i: usize| order.iter().position(|&x| x == i).unwrap_or_default();
        let mut change: Option<(usize, usize)> = None;
        for (identifier, at) in &shared {
            let domain = identifier[0].as_str().unwrap_or_default();
            let owners: Vec<usize> = at.iter().copied().filter(|&i| own(i, domain)).collect();
            if strict && owners.len() > 1 {
                return Err(refuse(owners[0], owners[1], domain));
            }
            let first = at.iter().map(|&i| place(i)).min().unwrap_or_default();
            if let [one] = owners[..] {
                if place(one) != first && change.is_none_or(|(_, to)| first < to) {
                    change = Some((place(one), first));
                }
            }
        }
        let Some((from, to)) = change else {
            let moved = order.iter().enumerate().any(|(at, &i)| at != i);
            return Ok(moved.then_some(order));
        };
        let row = order.remove(from);
        order.insert(to, row);
    }
    if strict {
        let (identifier, at) = &shared[0];
        return Err(refuse(
            at[0],
            at[1],
            identifier[0].as_str().unwrap_or_default(),
        ));
    }
    Ok(None)
}

/// What [`convert_entities`] needs from the other stores.
struct Names<'a> {
    devices: &'a [Device],
    /// Each device's `name_by_user or name`, as the file had it.
    device_names: &'a HashMap<String, String>,
    splits: &'a [Split],
    /// The `lifx` and `met` entries going back to version 1.
    lifx: &'a HashSet<String>,
    met: &'a HashSet<String>,
    /// What 2026.6.1's `met` names each of those entries' entity: the
    /// home's name, or the entry's title (`None`: not known).
    met_names: HashMap<String, Option<String>>,
}

/// The entity registry as 1.22 has it, and what changed.
///
/// - 1.23 (`EntityRegistryStore._async_migrate_func`) added the top-level
///   `settings`. It is left out; a setting that isn't the default is
///   reported as dropped.
/// - 1.24 stores a name the user gave without the device's name, which
///   Home Assistant then puts in front, and keeps the old name in
///   `compat_name` where that changes what is shown. 2026.11 still gives
///   every entity the friendly name it had (`async_get_legacy_friendly_name`:
///   `compat_name`, else the device's name and the stored name joined), and
///   that is the name stored here: the entity's `friendly_name` is the
///   same before and after the import. Where the migration (for entities
///   exposed to Assist) or `async_preserve_compat_name_as_alias` also made
///   the old name the first alias, in place of "the entity's name", it
///   stays: the file doesn't say whether the user had written that alias
///   out already.
/// - An entity still on a shared device's old id goes to the part its
///   config entry owns, as 2026.8 does when it loads the file
///   (`EntityRegistry._async_load`'s `_split_device_id`); with no such part
///   it is detached, as there.
/// - `lifx` 2 to 1 (`migration.py::_raw_unique_id` reversed): the serial at
///   the front of a unique id is written as a MAC address again.
/// - `met` 2 to 1: the entity gets back the name 2026.6.1 gives it
///   (`components/met/weather.py`), which 2026.11 moved to the device.
fn convert_entities(
    mut rows: EntityRows,
    found: (u32, u32),
    names: &Names,
    dropped: &mut Vec<Dropped>,
) -> anyhow::Result<(EntityRows, Vec<String>)> {
    let mut changes = Vec::new();
    if found.1 >= 23 {
        let settings = rows.extra.remove("settings");
        let parts = settings.as_ref().and_then(|s| s.get("entity_id_parts"));
        if parts.is_some_and(|p| !p.is_null()) {
            dropped.push(Dropped {
                store: ENTITY_KEY.into(),
                what: "the setting for what a new entity's id is made of (`entity_id_parts`)"
                    .into(),
                reason: "hars makes entity ids as Home Assistant 2026.6.1 does".into(),
            });
        }
        changes.push("the registry's `settings` are left out".into());
    }
    let device_ids: HashSet<&str> = names.devices.iter().map(|d| d.id.as_str()).collect();
    let (mut moved, mut renamed, mut serials, mut met) = (0, 0, 0, 0);
    for e in &mut rows.entities {
        let split = e
            .device_id
            .clone()
            .filter(|d| !device_ids.contains(d.as_str()))
            .and_then(|old| {
                let (_, parts) = names.splits.iter().find(|(id, _)| *id == old)?;
                Some((old, parts))
            });
        if let Some((old, parts)) = split {
            let entry = e.config_entry_id.as_deref();
            let part = parts
                .iter()
                .find(|p| Some(p.1.as_str()) == entry && p.2 == e.config_subentry_id)
                .or_else(|| parts.iter().find(|p| Some(p.1.as_str()) == entry));
            e.device_id = part.map(|p| p.0.clone());
            match part {
                Some(_) => moved += 1,
                None => dropped.push(Dropped {
                    store: ENTITY_KEY.into(),
                    what: format!("{}'s link to the device {old}", e.entity_id),
                    reason: "Home Assistant 2026.8 split that device and no part belongs to \
                             the entity's integration; Home Assistant detaches it too"
                        .into(),
                }),
            }
        }
        if found.1 >= 24 {
            let compat = match e.extra.remove("compat_name") {
                Some(Value::String(name)) => Some(name),
                _ => None,
            };
            if let Some(name) = e.name.clone() {
                let device = e
                    .device_id
                    .as_deref()
                    .and_then(|id| names.device_names.get(id));
                // `_async_get_full_entity_name` with the device and entity
                // parts: the ones that aren't empty, joined by a space.
                let legacy = match (&compat, device) {
                    (Some(old), _) => old.clone(),
                    (None, Some(device)) if name.is_empty() => device.clone(),
                    (None, Some(device)) => format!("{device} {name}"),
                    (None, None) => name.clone(),
                };
                if legacy != name {
                    e.name = Some(legacy);
                    renamed += 1;
                }
            }
        }
        let entry = e.config_entry_id.clone().unwrap_or_default();
        let unique_id = e.unique_id.as_ref().and_then(Value::as_str);
        if let Some(unique_id) =
            unique_id.filter(|_| e.platform == "lifx" && names.lifx.contains(&entry))
        {
            let (serial, rest) = match unique_id.split_once('_') {
                Some((serial, key)) => (serial, format!("_{key}")),
                None => (unique_id, String::new()),
            };
            if let Some(serial) = colon_serial(serial) {
                e.unique_id = Some(Value::String(format!("{serial}{rest}")));
                serials += 1;
            }
        }
        let weather = e.platform == "met"
            && e.entity_id.starts_with("weather.")
            && e.original_name.is_none()
            && names.met.contains(&entry);
        if let Some(Some(name)) = names.met_names.get(&entry).filter(|_| weather) {
            e.original_name = Some(name.clone());
            met += 1;
        }
    }
    // A unique id written back must not be one another row already has:
    // the two rows would be one entity.
    let mut seen = HashSet::new();
    for e in rows.entities.iter().filter(|e| e.platform == "lifx") {
        let domain = e.entity_id.split('.').next().unwrap_or_default();
        let unique = e.unique_id.clone().unwrap_or_default().to_string();
        if serials > 0 && !seen.insert((domain.to_owned(), unique)) {
            bail!(
                "{ENTITY_KEY}: two `lifx` entities of the same kind have the unique id of \
                 {}, one with the serial written each way. Remove one of them in Home \
                 Assistant, then copy the config directory again. Nothing was imported",
                e.entity_id
            );
        }
    }
    if moved > 0 {
        changes.push(format!(
            "{} from a split device's old id to the part its integration owns",
            count(moved, "entity is moved", "entities are moved")
        ));
    }
    if found.1 >= 24 {
        changes.push(format!(
            "`compat_name` is left out; {} stored in full again (friendly names stay the \
             same)",
            count(renamed, "name the user gave is", "names the user gave are")
        ));
    }
    if serials > 0 {
        changes.push(format!(
            "{} the serial with colons again",
            count(serials, "lifx unique id has", "lifx unique ids have")
        ));
    }
    if met > 0 {
        changes.push(format!(
            "{} the place's name again, as in 2026.6.1",
            count(met, "met entity has", "met entities have")
        ));
    }
    Ok((rows, changes))
}

/// `core.restore_state` rows as 2026.6.1 has them: found by entity id.
///
/// 2026.11 (`helpers/restore_state.py`) adds `entity_registry_id` to each
/// row and keeps two tables: the rows of entities with a registry row, by
/// that row's id, and the rest by entity id (a later row of the same key
/// replaces an earlier one). An entity in the registry is given the row of
/// its registry id, whatever entity id is stored in it; any other entity
/// the row of its entity id from the second table
/// (`async_get_stored_state`). 2026.6.1 has one table, by the stored
/// entity id. So that each entity is given the state 2026.11 would give
/// it, and no other:
///
/// - the row of a registered entity takes that entity's current id, which
///   differs when the entity was renamed since;
/// - a row for a registry id that is gone is dropped: 2026.11 gives it to
///   nobody (unless that same registry row comes back), and under its
///   entity id 2026.6.1 would give it to whatever takes that id;
/// - a row without a registry id stays, unless a registered entity has its
///   entity id now: 2026.11 doesn't give it to that entity, and 2026.6.1
///   would.
///
/// A row with no `entity_registry_id` at all was stored before 2026.11,
/// which reads it as the registered entity's of that id, if there is one.
/// A file with no such field is left alone. One of a later minor version
/// with the field has no rule and stops the import.
fn convert_restore_state(
    root: &Path,
    registry_ids: &HashMap<String, String>,
    ha_version: &str,
    done: &mut Conversion,
) -> anyhow::Result<()> {
    let store = Store::new(root, RESTORE_KEY, 1, 1, false);
    // One that doesn't decode is the loader's to refuse, after this.
    let Ok(Some(rows)) = store.load::<Vec<Value>>() else {
        return Ok(());
    };
    if !rows.iter().any(|r| r.get("entity_registry_id").is_some()) {
        return Ok(());
    }
    if let Some(found) = head(root, RESTORE_KEY).filter(|found| *found != (1, 1)) {
        bail!("{}", too_new(RESTORE_KEY, found, (1, 1), ha_version));
    }
    let registered: HashMap<&str, &str> = registry_ids
        .iter()
        .map(|(id, entity_id)| (entity_id.as_str(), id.as_str()))
        .collect();
    // Which table each row is in, and its key there.
    #[derive(PartialEq, Eq, Hash, Clone)]
    enum Key {
        Registry(String),
        Entity(String),
        /// Not an entity's row (Home Assistant skips it); kept as it is.
        Other(usize),
    }
    let mut dropped = Vec::new();
    let mut renamed = 0;
    let mut kept: Vec<(Key, Value)> = Vec::new();
    for (n, mut row) in rows.into_iter().enumerate() {
        let stored = row["state"]["entity_id"].as_str().map(str::to_owned);
        let registry_id = row
            .as_object_mut()
            .and_then(|o| o.remove("entity_registry_id"));
        let Some(stored) = stored else {
            kept.push((Key::Other(n), row));
            continue;
        };
        let key = match registry_id {
            // "Stored before states were indexed by entity registry id".
            None => match registered.get(stored.as_str()) {
                Some(id) => Key::Registry((*id).to_owned()),
                None => Key::Entity(stored.clone()),
            },
            Some(Value::String(id)) => Key::Registry(id),
            Some(_) => Key::Entity(stored.clone()),
        };
        match &key {
            Key::Registry(id) => match registry_ids.get(id) {
                Some(current) if *current != stored => {
                    row["state"]["entity_id"] = Value::String(current.clone());
                    renamed += 1;
                }
                Some(_) => {}
                None => {
                    dropped.push((
                        stored,
                        "the entity it was stored for is not in the registry any more; Home \
                         Assistant 2026.11 gives it to no other",
                    ));
                    continue;
                }
            },
            Key::Entity(id) if registered.contains_key(id.as_str()) => {
                dropped.push((
                    stored,
                    "it was stored for an entity that was not in the registry; Home Assistant \
                     2026.11 doesn't give it to the one that has this id now",
                ));
                continue;
            }
            _ => {}
        }
        // A later row of a key replaces an earlier one, as in both tables.
        if let Some(i) = kept.iter().position(|(k, _)| *k == key) {
            let (_, old) = kept.remove(i);
            dropped.push((
                old["state"]["entity_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                "a later one was stored for the same entity, and Home Assistant keeps that",
            ));
        }
        kept.push((key, row));
    }
    for (entity_id, reason) in dropped {
        done.dropped.push(Dropped {
            store: RESTORE_KEY.into(),
            what: format!("a last state stored for {entity_id}"),
            reason: reason.into(),
        });
    }
    let kept: Vec<Value> = kept.into_iter().map(|(_, row)| row).collect();
    store.save(&kept)?;
    done.stores.push(ConvertedStore {
        key: RESTORE_KEY.into(),
        from: "1.1".into(),
        to: "1.1".into(),
        changes: vec![format!(
            "`entity_registry_id` is left out of {}; {} the entity id its entity has today",
            count(kept.len(), "row", "rows"),
            count(renamed, "row has", "rows have")
        )],
    });
    Ok(())
}

/// The config entries of version 2 that have a rule, as version 1.
///
/// `lifx` (`components/lifx/__init__.py::async_migrate_entry`, 2026.10):
/// the unique id is the bulb's serial, which version 1 writes as a MAC
/// address and version 2 without the colons; version 2 also keeps it in
/// `data` as `serial`. The colons go back and `serial` goes. (The shared
/// entry from before one entry per bulb, which that migration removes,
/// stays removed: it ran nothing.)
///
/// `met` (`components/met/__init__.py::async_migrate_entry`, 2026.11):
/// `latitude` and `longitude` come out of `location`. The migration also
/// dropped `name`, the forecast's name; 2026.11 shows the entry's title in
/// its place (`weather.py`: `device_name = config_entry.title`), so that
/// is the name it gets. An entry that follows the home has neither.
fn convert_entries(data: &mut Map<String, Value>) -> anyhow::Result<Vec<ConvertedEntry>> {
    let mut done = Vec::new();
    let Some(entries) = data.get_mut("entries").and_then(Value::as_array_mut) else {
        return Ok(done);
    };
    for entry in entries.iter_mut().filter_map(Value::as_object_mut) {
        let n = |k: &str| entry.get(k).and_then(Value::as_u64).unwrap_or(1);
        let (domain, title) = (text_of(entry, "domain"), text_of(entry, "title"));
        let from = (n("version"), n("minor_version"));
        if !entry_converts(&domain, from.0, from.1) {
            continue;
        }
        let refuse = |why: &str| {
            anyhow::anyhow!(
                "the {domain} integration's config entry {title:?} is version {}.{} and {why}, \
                 so hars can't bring it back to version 1. Nothing was imported",
                from.0,
                from.1
            )
        };
        let mut changes = Vec::new();
        let unique_id = entry
            .get("unique_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let Some(Value::Object(mut fields)) = entry.remove("data") else {
            return Err(refuse("has no data"));
        };
        if domain == "lifx" {
            let serial = unique_id
                .as_deref()
                .and_then(colon_serial)
                .ok_or_else(|| refuse("its unique id is not a LIFX serial"))?;
            match fields.remove("serial") {
                None => {}
                Some(Value::String(s)) if Some(&s) == unique_id.as_ref() => {}
                Some(_) => return Err(refuse("its serial is not its unique id")),
            }
            entry.insert("unique_id".into(), Value::String(serial));
            changes.push(
                "the unique id is the serial with colons again; `serial` left out of `data` \
                 (it is the unique id)"
                    .into(),
            );
        } else {
            match fields.remove("location") {
                None => {}
                Some(Value::Object(location))
                    if location.get("latitude").is_some_and(Value::is_number)
                        && location.get("longitude").is_some_and(Value::is_number)
                        && location.len() == 2
                        && !fields.contains_key("latitude")
                        && !fields.contains_key("longitude") =>
                {
                    fields.extend(location);
                    changes.push("`latitude` and `longitude` out of `location`".into());
                }
                Some(_) => return Err(refuse("its location is not a latitude and a longitude")),
            }
            let home = fields.get("track_home").and_then(Value::as_bool) == Some(true);
            if !home && !fields.contains_key("name") {
                fields.insert("name".into(), Value::String(title.clone()));
                changes.push("`name` is the entry's title, which 2026.11 shows instead".into());
            }
        }
        entry.insert("data".into(), Value::Object(fields));
        entry.insert("version".into(), json!(1));
        entry.insert("minor_version".into(), json!(1));
        done.push(ConvertedEntry {
            entry_id: text_of(entry, "entry_id"),
            domain,
            title,
            from: format!("{}.{}", from.0, from.1),
            to: "1.1".into(),
            changes,
        });
    }
    Ok(done)
}

fn text_of(entry: &Map<String, Value>, key: &str) -> String {
    entry
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `normalize_serial` (2026.10) leaves twelve lower-case hex digits;
    /// only that goes back to `formatted_serial`'s form.
    #[test]
    fn serials() {
        assert_eq!(
            colon_serial("d073d5000001").as_deref(),
            Some("d0:73:d5:00:00:01")
        );
        for other in [
            "d0:73:d5:00:00:01",
            "D073D5000001",
            "d073d500000",
            "d073d50000011",
            "lifx",
            "",
        ] {
            assert_eq!(colon_serial(other), None, "{other}");
        }
    }

    fn holder(id: &str, entry: &str, identifiers: &Value) -> Holder {
        (
            id.into(),
            identifiers.as_array().unwrap().clone(),
            vec![entry.into()],
        )
    }

    /// The integration's own row goes before the copies of its
    /// identifier; what can't be put right stops the import for devices,
    /// and is left alone for removed ones.
    #[test]
    fn shared_identifiers() {
        let domain_of: HashMap<String, String> = [
            ("h", "hue"),
            ("h2", "hue"),
            ("k", "homekit_controller"),
            ("l", "lifx"),
        ]
        .into_iter()
        .map(|(id, domain)| (id.to_owned(), domain.to_owned()))
        .collect();
        let order = |rows: &[Holder], strict: bool| {
            order_shared_identifiers(rows, &domain_of, strict).map(|order| {
                order.map(|o| {
                    o.iter()
                        .map(|&i| rows[i].0.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
            })
        };
        let bulb = json!([
            ["lifx", "d0:73:d5:00:00:01"],
            ["homekit_controller:accessory-id", "x"]
        ]);
        let lamp = json!([["hue", "a"]]);
        let rows = [
            holder(
                "other",
                "k",
                &json!([["homekit_controller:accessory-id", "y"]]),
            ),
            holder("copy", "k", &bulb),
            holder("lamp", "h", &lamp),
            holder("own", "l", &bulb),
        ];
        assert_eq!(
            order(&rows, true).unwrap().as_deref(),
            Some("other own copy lamp")
        );
        // Already in order, or nothing of the integration's own: as it is.
        assert_eq!(
            order(&[rows[3].clone(), rows[1].clone()], true).unwrap(),
            None
        );
        let unowned = [holder("a", "k", &bulb), holder("b", "gone", &bulb)];
        assert_eq!(order(&unowned, true).unwrap(), None);
        // An identifier of an integration hars doesn't run is not looked up.
        let strip = json!([["mqtt", "strip"]]);
        assert_eq!(
            order(&[holder("a", "k", &strip), holder("b", "m", &strip)], true).unwrap(),
            None
        );

        // Two of one integration's own: 2026.8 tells them apart by config
        // entry, hars can't.
        let twins = [holder("one", "h", &lamp), holder("two", "h2", &lamp)];
        let err = order(&twins, true).unwrap_err().to_string();
        assert!(
            err.contains("the devices one and two both have an identifier of the hue"),
            "{err}"
        );
        assert_eq!(order(&twins, false).unwrap(), None);
        // Each holds a copy of the other's: no order puts both first.
        let both = json!([["hue", "a"], ["lifx", "d0:73:d5:00:00:01"]]);
        let crossed = [holder("lamp", "h", &both), holder("bulb", "l", &both)];
        let err = order(&crossed, true).unwrap_err().to_string();
        assert!(err.contains("Nothing was imported"), "{err}");
        assert_eq!(order(&crossed, false).unwrap(), None);
    }
}

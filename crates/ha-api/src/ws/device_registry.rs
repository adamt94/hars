//! What the user changes of a device, and a new area
//! (`components/config/device_registry.py` and `area_registry.py`): the
//! last step of the add-integration dialog, the area picker's "add new
//! area", and the device page's dialog.
//!
//! - `config/device_registry/update` (admin): the user's name, area and
//!   labels. Running entities take a new name at once
//!   ([`ha_integrations::components::Follower`]); a name is refused for a
//!   device with an entity that wouldn't, and so is disabling or enabling
//!   a device, which HA's entities follow by stopping and starting.
//! - `config/area_registry/create` (admin).
//!
//! Nothing is changed when its store can't be written
//! ([`ha_registry::Registry::unwritable`]): the change would be gone at
//! the next start.
//!
//! Checked by `config_entries/vectors/device_step.json`, made by
//! `scripts/vectors/device_step.py`.

use super::{Conn, Invalid, only_keys};
use ha_auth::Permission;
use ha_registry::{AreaRefusal, DevicePatch, DeviceRefusal, NewArea};
use serde_json::{Map, Value, json};

/// `vol.Any(str, None)`.
pub(super) fn text_or_null(
    msg: &Map<String, Value>,
    key: &str,
) -> Result<Option<Option<String>>, Invalid> {
    match msg.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(s)) => Ok(Some(Some(s.clone()))),
        Some(_) => Err(format!("expected str for dictionary value @ data['{key}']")),
    }
}

/// `str`.
fn text(msg: &Map<String, Value>, key: &str) -> Result<Option<String>, Invalid> {
    match msg.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("expected str for dictionary value @ data['{key}']")),
    }
}

/// `[str]`.
pub(super) fn labels(msg: &Map<String, Value>) -> Result<Option<Vec<String>>, Invalid> {
    match msg.get("labels") {
        None => Ok(None),
        Some(Value::Array(labels)) => labels
            .iter()
            .enumerate()
            .map(|(i, l)| match l {
                Value::String(l) => Ok(l.clone()),
                _ => Err(format!("expected str @ data['labels'][{i}]")),
            })
            .collect::<Result<Vec<String>, Invalid>>()
            .map(Some),
        Some(_) => Err("expected a list for dictionary value @ data['labels']".into()),
    }
}

/// The answer for a change whose store `hars` won't write.
pub(super) fn unwritable(conn: &Conn, id: u64, key: &str) -> bool {
    conn.out.error(
        json!(id),
        "home_assistant_error",
        &format!(
            "This can't be changed here, because hars would not be able to save it: \
             .storage/{key} was written by a Home Assistant version whose format hars reads \
             but doesn't write. Start Home Assistant 2026.6 or 2026.7 once on this config \
             folder, so it updates its files, then import the folder into hars again."
        ),
    )
}

/// Make a change to a device's name or an entity's id, with the running
/// entities taking it before anything else sees it and before the command
/// is answered (HA's entities have written their states by then).
pub(super) fn edit<T>(conn: &Conn, change: impl FnOnce() -> T) -> T {
    match &conn.shared.config.entities {
        Some(entities) => entities.registry_edit(change),
        None => change(),
    }
}

fn update(conn: &Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(
        msg,
        &[
            "area_id",
            "device_id",
            "disabled_by",
            "labels",
            "name_by_user",
        ],
    )?;
    let area_id = text_or_null(msg, "area_id")?;
    let device_id = match text(msg, "device_id")? {
        Some(id) => id,
        None => return Err("required key not provided @ data['device_id']".into()),
    };
    // `vol.Any("user", None)`: all the API may set.
    let disabled_by = match msg.get("disabled_by") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::String(by)) if by == "user" => Some(Some(by.clone())),
        Some(_) => {
            return Err("not a valid value for dictionary value @ data['disabled_by']".into());
        }
    };
    let labels = labels(msg)?;
    let name_by_user = text_or_null(msg, "name_by_user")?;
    if conn.principal.permission < Permission::Admin {
        conn.out.error(json!(id), "unauthorized", "Unauthorized");
        return Ok(());
    }
    // `self.devices[device_id]`: a `KeyError`.
    let unknown = || conn.out.error(json!(id), "unknown_error", "Unknown error");
    let Some(registry) = &conn.shared.config.registry else {
        unknown();
        return Ok(());
    };
    let patch = DevicePatch {
        area_id,
        disabled_by,
        labels,
        name_by_user,
    };
    let follows = &ha_integrations::components::follows;
    let cant = |what: &str| conn.out.error(json!(id), "not_supported", what);
    match edit(conn, || registry.update_device(&device_id, &patch, follows)) {
        Ok(device) => {
            conn.out.result(id, device.json());
        }
        Err(DeviceRefusal::Unknown) => {
            unknown();
        }
        Err(DeviceRefusal::Unwritable(key)) => {
            unwritable(conn, id, key);
        }
        Err(DeviceRefusal::Disable) => {
            cant("hars can't disable or enable a device yet");
        }
        Err(DeviceRefusal::Rename(Some(platform))) => {
            cant(&format!(
                "hars can't rename a device of the {platform} integration yet"
            ));
        }
        Err(DeviceRefusal::Rename(None)) => {
            cant("hars can't rename a device whose entities carry its name in their own yet");
        }
    }
    Ok(())
}

fn create_area(conn: &Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(
        msg,
        &[
            "aliases",
            "floor_id",
            "humidity_entity_id",
            "icon",
            "labels",
            "name",
            "picture",
            "temperature_entity_id",
        ],
    )?;
    // `list`: of anything, as far as the schema goes.
    let aliases = match msg.get("aliases") {
        None => None,
        Some(Value::Array(aliases)) => Some(aliases),
        Some(_) => return Err("expected list for dictionary value @ data['aliases']".into()),
    };
    let floor_id = text(msg, "floor_id")?;
    let humidity_entity_id = text_or_null(msg, "humidity_entity_id")?.flatten();
    let icon = text(msg, "icon")?;
    let labels = labels(msg)?;
    let name = match msg.get("name") {
        None => return Err("required key not provided @ data['name']".into()),
        Some(Value::String(name)) => name.clone(),
        Some(_) => return Err("expected str for dictionary value @ data['name']".into()),
    };
    let picture = text_or_null(msg, "picture")?.flatten();
    let temperature_entity_id = text_or_null(msg, "temperature_entity_id")?.flatten();
    if conn.principal.permission < Permission::Admin {
        conn.out.error(json!(id), "unauthorized", "Unauthorized");
        return Ok(());
    }
    // Each alias stripped, the empty ones dropped; one that isn't text
    // has no `strip` (an `AttributeError`).
    let mut stripped = Vec::new();
    for alias in aliases.into_iter().flatten() {
        match alias.as_str().map(str::trim) {
            Some("") => {}
            Some(alias) => stripped.push(alias.to_owned()),
            None => {
                conn.out.error(json!(id), "unknown_error", "Unknown error");
                return Ok(());
            }
        }
    }
    let Some(registry) = &conn.shared.config.registry else {
        conn.out.error(json!(id), "unknown_error", "Unknown error");
        return Ok(());
    };
    let new = NewArea {
        name,
        aliases: stripped,
        floor_id,
        humidity_entity_id,
        icon,
        labels: labels.unwrap_or_default(),
        picture,
        temperature_entity_id,
    };
    match registry.create_area(new) {
        Ok(area) => conn.out.result(id, area.json()),
        // A `ValueError`.
        Err(AreaRefusal::Invalid(why)) => conn.out.error(json!(id), "invalid_info", &why),
        Err(AreaRefusal::Unwritable) => unwritable(conn, id, ha_registry::AREA_KEY),
    };
    Ok(())
}

pub(super) fn dispatch(
    conn: &mut Conn,
    id: u64,
    ty: &str,
    msg: &Map<String, Value>,
) -> Option<Result<(), Invalid>> {
    Some(match ty {
        "config/device_registry/update" => update(conn, id, msg),
        "config/area_registry/create" => create_area(conn, id, msg),
        _ => return None,
    })
}

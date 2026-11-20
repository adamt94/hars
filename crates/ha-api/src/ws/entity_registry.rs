//! The entity registry's commands the Settings pages use
//! (`components/config/entity_registry.py`), and the category list
//! (`category_registry.py`):
//!
//! - `config/entity_registry/list`, `get` and `get_entries`: rows as HA's
//!   `as_partial_dict` and `extended_dict`.
//! - `config/entity_registry/update` (admin): the user's area, labels and
//!   categories, and a new entity id for an entity of an integration whose
//!   running entities take it (`ha_integrations::components::follows`).
//!   HA's other fields (a name, an icon, disabling, hiding, aliases,
//!   options) need the entity to follow the change, which hars's entities
//!   don't do yet: those are refused. Nothing is changed when the store
//!   can't be written.
//! - `config/entity_registry/get_automatic_entity_ids`: the ids that
//!   follow from the names as they are now.
//! - `config/category_registry/list`: the stored categories of a scope.
//! - `blueprint/list`: hars has no blueprints, so none.
//!
//! Checked by `vectors/entity_registry.json`, made by
//! `scripts/vectors/entity_registry_ws.py`, and for entity ids by
//! `config_entries/vectors/device_step.json` (`device_step.py`).

use super::{Conn, Invalid, only_keys};
use ha_auth::Permission;
use serde_json::{Map, Value, json};

/// `cv.entity_id`: a valid id, lower-cased.
fn entity_id(msg: &Map<String, Value>) -> Result<String, Invalid> {
    let at = "for dictionary value @ data['entity_id']";
    match msg.get("entity_id") {
        None => Err("required key not provided @ data['entity_id']".into()),
        Some(Value::String(s)) => {
            let id = s.to_lowercase();
            match ha_core::EntityId::parse(&id) {
                Ok(_) => Ok(id),
                Err(_) => Err(format!("Entity ID {s} is an invalid entity ID {at}")),
            }
        }
        Some(other) => Err(format!("Entity ID {other} is an invalid entity ID {at}")),
    }
}

fn not_found(conn: &Conn, id: u64) -> bool {
    conn.out.error(json!(id), "not_found", "Entity not found")
}

fn get(conn: &Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(msg, &["entity_id"])?;
    let entity_id = entity_id(msg)?;
    let r = conn.shared.config.registries();
    match r.entities.iter().find(|e| e.entity_id == entity_id) {
        Some(e) => conn
            .out
            .result(id, Value::Object(e.extended_json(r.device_name_of(e)))),
        None => not_found(conn, id),
    };
    Ok(())
}

/// `cv.entity_ids`: a list, or one string with commas between ids; each
/// id trimmed and lower-cased. `None` once the command is answered.
fn entity_ids(
    conn: &Conn,
    id: u64,
    msg: &Map<String, Value>,
) -> Result<Option<Vec<String>>, Invalid> {
    only_keys(msg, &["entity_ids"])?;
    let ids: Vec<String> = match msg.get("entity_ids") {
        None => return Err("required key not provided @ data['entity_ids']".into()),
        Some(Value::Null) => {
            return Err(
                "Entity IDs cannot be None for dictionary value @ data['entity_ids']".into(),
            );
        }
        Some(Value::String(s)) => s.split(',').map(|i| i.trim().to_owned()).collect(),
        // Python goes through a mapping's keys, in the order sent. That
        // order is gone here (a message's maps are sorted), which shows
        // only when two of the keys would get the same automatic id.
        Some(Value::Object(ids)) => ids.keys().cloned().collect(),
        Some(Value::Array(ids)) => ids
            .iter()
            .map(|i| match i {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        // HA iterates whatever it is given: anything else raises.
        Some(_) => {
            conn.out.error(json!(id), "unknown_error", "Unknown error");
            return Ok(None);
        }
    };
    ids.into_iter()
        .map(|written| {
            let entity_id = written.to_lowercase();
            match ha_core::EntityId::parse(&entity_id) {
                Ok(_) => Ok(entity_id),
                Err(_) => Err(format!(
                    "Entity ID {written} is an invalid entity ID for dictionary value @ data['entity_ids']"
                )),
            }
        })
        .collect::<Result<Vec<String>, Invalid>>()
        .map(Some)
}

fn get_automatic_entity_ids(conn: &Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    let Some(ids) = entity_ids(conn, id, msg)? else {
        return Ok(());
    };
    let found: Map<String, Value> = match &conn.shared.config.registry {
        Some(registry) => registry
            .automatic_entity_ids(&ids)
            .into_iter()
            .map(|(old, new)| (old, json!(new)))
            .collect(),
        None => ids.into_iter().map(|id| (id, Value::Null)).collect(),
    };
    conn.out.result(id, Value::Object(found));
    Ok(())
}

fn get_entries(conn: &Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    let Some(ids) = entity_ids(conn, id, msg)? else {
        return Ok(());
    };
    let r = conn.shared.config.registries();
    let mut out = Map::new();
    for entity_id in ids {
        let row = r.entities.iter().find(|e| e.entity_id == entity_id);
        out.insert(
            entity_id,
            row.map_or(Value::Null, |e| {
                Value::Object(e.extended_json(r.device_name_of(e)))
            }),
        );
    }
    conn.out.result(id, Value::Object(out));
    Ok(())
}

/// What HA's schema takes that hars can't act on yet.
const UNSUPPORTED: [&str; 8] = [
    "aliases",
    "device_class",
    "icon",
    "name",
    "disabled_by",
    "hidden_by",
    "options_domain",
    "options",
];

fn update(conn: &Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    let mut allowed = vec![
        "entity_id",
        "area_id",
        "categories",
        "labels",
        "new_entity_id",
    ];
    allowed.extend(UNSUPPORTED);
    only_keys(msg, &allowed)?;
    let entity_id = entity_id(msg)?;
    let entity_id = entity_id.as_str();
    // `vol.Any(str, None)`.
    let area_id = match msg.get("area_id") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::String(a)) => Some(Some(a.clone())),
        Some(_) => return Err("expected str for dictionary value @ data['area_id']".into()),
    };
    // `cv.schema_with_slug_keys(vol.Any(str, None))`.
    let categories = match msg.get("categories") {
        None => None,
        Some(Value::Object(c)) => {
            for scope in c.keys() {
                let slug = ha_core::slugify(scope);
                if *scope != slug {
                    return Err(format!(
                        "invalid slug {scope} (try {slug}) for dictionary value @ data['categories']"
                    ));
                }
            }
            for (scope, category) in c {
                if !category.is_null() && !category.is_string() {
                    return Err(format!(
                        "expected str for dictionary value @ data['categories']['{scope}']"
                    ));
                }
            }
            Some(c)
        }
        Some(_) => {
            return Err("expected dictionary for dictionary value @ data['categories']".into());
        }
    };
    let labels = super::device_registry::labels(msg)?;
    // `str`: checked by the registry.
    let new_entity_id = match msg.get("new_entity_id") {
        None => None,
        Some(Value::String(new)) => Some(new.as_str()),
        Some(_) => {
            return Err("expected str for dictionary value @ data['new_entity_id']".into());
        }
    };
    if conn.principal.permission < Permission::Admin {
        conn.out.error(json!(id), "unauthorized", "Unauthorized");
        return Ok(());
    }
    let Some(registry) = conn.shared.config.registry.clone() else {
        not_found(conn, id);
        return Ok(());
    };
    let extended = |entity_id: &str| {
        let r = registry.snapshot();
        let row = r.entities.iter().find(|e| e.entity_id == entity_id)?;
        Some(row.extended_json(r.device_name_of(row)))
    };
    if extended(entity_id).is_none() {
        not_found(conn, id);
        return Ok(());
    }
    if let Some(key) = UNSUPPORTED.iter().find(|k| msg.contains_key(**k)) {
        conn.out.error(
            json!(id),
            "not_supported",
            &format!("hars can't change an entity's {key} yet"),
        );
        return Ok(());
    }
    if registry.unwritable(ha_registry::ENTITY_KEY) {
        super::device_registry::unwritable(conn, id, ha_registry::ENTITY_KEY);
        return Ok(());
    }
    let patch = ha_registry::EntityPatch {
        area_id,
        // Only the scopes named change.
        categories: categories.cloned(),
        labels,
        ..Default::default()
    };
    let Some(new) = new_entity_id else {
        registry.update_entity(entity_id, patch);
        match extended(entity_id) {
            Some(row) => conn.out.result(id, json!({"entity_entry": row})),
            None => not_found(conn, id),
        };
        return Ok(());
    };
    use ha_registry::RenameRefusal;
    let follows = &ha_integrations::components::follows;
    let renamed = super::device_registry::edit(conn, || {
        registry.rename_entity(entity_id, new, &patch, follows)
    });
    match renamed {
        // The row as this change left it, whatever came after.
        Ok(row) => {
            let r = registry.snapshot();
            let row = row.extended_json(r.device_name_of(&row));
            conn.out.result(id, json!({"entity_entry": row}))
        }
        Err(RenameRefusal::Unknown) => not_found(conn, id),
        Err(RenameRefusal::Unwritable) => {
            super::device_registry::unwritable(conn, id, ha_registry::ENTITY_KEY)
        }
        Err(RenameRefusal::Platform(platform)) => conn.out.error(
            json!(id),
            "not_supported",
            &format!("hars can't change the id of an entity of the {platform} integration yet"),
        ),
        // A `ValueError`.
        Err(refused) => {
            let why = refused.message().unwrap_or_default();
            conn.out.error(json!(id), "invalid_info", why)
        }
    };
    Ok(())
}

fn categories(conn: &Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(msg, &["scope"])?;
    let scope = match msg.get("scope") {
        None => return Err("required key not provided @ data['scope']".into()),
        Some(Value::String(s)) => s,
        Some(_) => return Err("expected str for dictionary value @ data['scope']".into()),
    };
    let r = conn.shared.config.registries();
    // `_entry_dict`: the stored row, its times as timestamps.
    let seconds = |iso: &Value| {
        iso.as_str()
            .and_then(|t| t.parse::<jiff::Timestamp>().ok())
            .map_or(0.0, |t| t.as_microsecond() as f64 / 1e6)
    };
    let list: Vec<Value> = r
        .categories
        .get(scope)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|c| {
            json!({
                "category_id": c["category_id"],
                "created_at": seconds(&c["created_at"]),
                "icon": c["icon"],
                "modified_at": seconds(&c["modified_at"]),
                "name": c["name"],
            })
        })
        .collect();
    conn.out.result(id, json!(list));
    Ok(())
}

pub(super) fn dispatch(
    conn: &mut Conn,
    id: u64,
    ty: &str,
    msg: &Map<String, Value>,
) -> Option<Result<(), Invalid>> {
    Some(match ty {
        "config/entity_registry/list" => only_keys(msg, &[]).map(|()| {
            let r = conn.shared.config.registries();
            conn.out.result(id, r.entities_partial());
        }),
        "config/entity_registry/get" => get(conn, id, msg),
        "config/entity_registry/get_entries" => get_entries(conn, id, msg),
        "config/entity_registry/get_automatic_entity_ids" => {
            get_automatic_entity_ids(conn, id, msg)
        }
        "config/entity_registry/update" => update(conn, id, msg),
        "config/category_registry/list" => categories(conn, id, msg),
        // `blueprint/list` (admin): every blueprint of a domain, by path.
        "blueprint/list" => only_keys(msg, &["domain"]).and_then(|()| {
            // `cv.string`.
            match msg.get("domain") {
                None => return Err("required key not provided @ data['domain']".into()),
                Some(Value::Null) => {
                    return Err("string value is None for dictionary value @ data['domain']".into());
                }
                Some(Value::Array(_) | Value::Object(_)) => {
                    return Err(
                        "value should be a string for dictionary value @ data['domain']".into(),
                    );
                }
                Some(_) => {}
            }
            if conn.principal.permission < Permission::Admin {
                conn.out.error(json!(id), "unauthorized", "Unauthorized");
            } else {
                conn.out.result(id, json!({}));
            }
            Ok(())
        }),
        _ => return None,
    })
}

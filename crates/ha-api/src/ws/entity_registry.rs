//! The entity registry's commands the Settings pages use
//! (`components/config/entity_registry.py`), and the category list
//! (`category_registry.py`):
//!
//! - `config/entity_registry/list`, `get` and `get_entries`: rows as HA's
//!   `as_partial_dict` and `extended_dict`.
//! - `config/entity_registry/update` (admin): the user's area, labels and
//!   categories. HA's other fields (a name, an icon, a new entity id,
//!   disabling, hiding, aliases, options) need the entity to follow the
//!   change, which hars's entities don't do yet: those are refused.
//! - `config/category_registry/list`: the stored categories of a scope.
//! - `blueprint/list`: hars has no blueprints, so none.
//!
//! Checked by `vectors/entity_registry.json`, made by
//! `scripts/vectors/entity_registry_ws.py`.

use super::{Conn, Invalid, only_keys};
use ha_auth::Permission;
use serde_json::{Map, Value, json};

/// `cv.entity_id`.
fn entity_id(msg: &Map<String, Value>) -> Result<&str, Invalid> {
    let at = "for dictionary value @ data['entity_id']";
    match msg.get("entity_id") {
        None => Err("required key not provided @ data['entity_id']".into()),
        Some(Value::String(s)) if ha_core::EntityId::parse(s).is_ok() => Ok(s),
        Some(Value::String(s)) => Err(format!("Entity ID {s} is an invalid entity ID {at}")),
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
        Some(e) => conn.out.result(id, Value::Object(e.extended_json())),
        None => not_found(conn, id),
    };
    Ok(())
}

fn get_entries(conn: &Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(msg, &["entity_ids"])?;
    let ids = match msg.get("entity_ids") {
        None => return Err("required key not provided @ data['entity_ids']".into()),
        Some(Value::Array(ids)) => ids,
        Some(_) => {
            return Err("expected a list for dictionary value @ data['entity_ids']".into());
        }
    };
    let r = conn.shared.config.registries();
    let mut out = Map::new();
    for (i, entity_id) in ids.iter().enumerate() {
        let Some(entity_id) = entity_id
            .as_str()
            .filter(|s| ha_core::EntityId::parse(s).is_ok())
        else {
            return Err(format!(
                "Entity ID {entity_id} is an invalid entity ID @ data['entity_ids'][{i}]"
            ));
        };
        let row = r.entities.iter().find(|e| e.entity_id == entity_id);
        out.insert(
            entity_id.to_owned(),
            row.map_or(Value::Null, |e| Value::Object(e.extended_json())),
        );
    }
    conn.out.result(id, Value::Object(out));
    Ok(())
}

/// What HA's schema takes that hars can't act on yet.
const UNSUPPORTED: [&str; 9] = [
    "aliases",
    "device_class",
    "icon",
    "name",
    "new_entity_id",
    "disabled_by",
    "hidden_by",
    "options_domain",
    "options",
];

fn update(conn: &Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    let mut allowed = vec!["entity_id", "area_id", "categories", "labels"];
    allowed.extend(UNSUPPORTED);
    only_keys(msg, &allowed)?;
    let entity_id = entity_id(msg)?;
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
    // `[str]`.
    let labels = match msg.get("labels") {
        None => None,
        Some(Value::Array(labels)) => Some(
            labels
                .iter()
                .enumerate()
                .map(|(i, l)| match l {
                    Value::String(l) => Ok(l.clone()),
                    _ => Err(format!("expected str @ data['labels'][{i}]")),
                })
                .collect::<Result<Vec<String>, Invalid>>()?,
        ),
        Some(_) => return Err("expected a list for dictionary value @ data['labels']".into()),
    };
    if conn.principal.permission < Permission::Admin {
        conn.out.error(json!(id), "unauthorized", "Unauthorized");
        return Ok(());
    }
    let Some(registry) = conn.shared.config.registry.clone() else {
        not_found(conn, id);
        return Ok(());
    };
    let find = |entity_id: &str| {
        registry
            .snapshot()
            .entities
            .iter()
            .find(|e| e.entity_id == entity_id)
            .cloned()
    };
    let Some(row) = find(entity_id) else {
        not_found(conn, id);
        return Ok(());
    };
    if let Some(key) = UNSUPPORTED.iter().find(|k| msg.contains_key(**k)) {
        conn.out.error(
            json!(id),
            "not_supported",
            &format!("hars can't change an entity's {key} yet"),
        );
        return Ok(());
    }
    // Only the scopes named change: a category sets its scope, `null`
    // clears it.
    let categories = categories.map(|named| {
        let mut all = row.categories.object();
        for (scope, category) in named {
            if category.is_null() {
                all.remove(scope);
            } else {
                all.insert(scope.clone(), category.clone());
            }
        }
        all
    });
    registry.update_entity(
        entity_id,
        ha_registry::EntityPatch {
            area_id,
            categories,
            labels,
            ..Default::default()
        },
    );
    match find(entity_id) {
        Some(row) => conn
            .out
            .result(id, json!({"entity_entry": row.extended_json()})),
        None => not_found(conn, id),
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
            let rows: Vec<Value> = r
                .entities
                .iter()
                .map(|e| Value::Object(e.partial_json()))
                .collect();
            conn.out.result(id, json!(rows));
        }),
        "config/entity_registry/get" => get(conn, id, msg),
        "config/entity_registry/get_entries" => get_entries(conn, id, msg),
        "config/entity_registry/update" => update(conn, id, msg),
        "config/category_registry/list" => categories(conn, id, msg),
        // `blueprint/list`: every blueprint of a domain, by path.
        "blueprint/list" => only_keys(msg, &["domain"]).map(|()| {
            conn.out.result(id, json!({}));
        }),
        _ => return None,
    })
}

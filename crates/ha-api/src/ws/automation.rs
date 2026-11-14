//! `automation/config` (`components/automation/__init__.py`): an
//! automation's raw config, as loaded.

use super::{Conn, Invalid};
use ha_auth::Permission;
use serde_json::{Map, Value, json};

pub(super) fn dispatch(
    conn: &mut Conn,
    id: u64,
    ty: &str,
    msg: &Map<String, Value>,
) -> Option<Result<(), Invalid>> {
    if ty != "automation/config" {
        return None;
    }
    let automations = conn.shared.config.automations.clone()?;
    Some((|| {
        for k in msg.keys() {
            if !matches!(k.as_str(), "id" | "type" | "entity_id") {
                return Err(format!("extra keys not allowed @ data['{k}']"));
            }
        }
        if conn.principal.permission < Permission::Admin {
            conn.out.error(json!(id), "unauthorized", "Unauthorized");
            return Ok(());
        }
        // HA's schema has `entity_id` optional, so a missing one is a
        // `KeyError` in the handler.
        let Some(entity_id) = msg.get("entity_id") else {
            conn.out.error(json!(id), "unknown_error", "Unknown error");
            return Ok(());
        };
        let Some(entity_id) = entity_id.as_str() else {
            return Err("expected str for dictionary value @ data['entity_id']".into());
        };
        match automations.by_entity(entity_id) {
            Some(a) => match &a.raw {
                Some(raw) => {
                    conn.out.result_raw(id, &format!(r#"{{"config":{raw}}}"#));
                }
                // HA's encoder fails on it too.
                None => {
                    conn.out
                        .error(json!(id), "unknown_error", "Invalid JSON in response");
                }
            },
            None => {
                conn.out.error(json!(id), "not_found", "Entity not found");
            }
        }
        Ok(())
    })())
}

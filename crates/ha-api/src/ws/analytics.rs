//! `analytics` and `analytics/preferences` (`components/analytics/__init__.py`),
//! both admin-only. The preferences are stored, never acted on: `hars`
//! sends no analytics ([`crate::analytics`]).

use super::{Conn, Invalid, only_keys};
use crate::analytics::PREFERENCES;
use ha_auth::Permission;
use serde_json::{Map, Value, json};

pub(super) fn dispatch(
    conn: &mut Conn,
    id: u64,
    ty: &str,
    msg: &Map<String, Value>,
) -> Option<Result<(), Invalid>> {
    Some(match ty {
        "analytics" => only_keys(msg, &[]).map(|()| {
            if admin(conn, id) {
                conn.out.result(id, conn.shared.config.analytics.json());
            }
        }),
        "analytics/preferences" => preferences(conn, id, msg),
        _ => return None,
    })
}

/// `@require_admin`.
fn admin(conn: &Conn, id: u64) -> bool {
    let ok = conn.principal.permission >= Permission::Admin;
    if !ok {
        conn.out.error(json!(id), "unauthorized", "Unauthorized");
    }
    ok
}

/// `vol.Required("preferences", default={}): PREFERENCE_SCHEMA`: a dict of
/// the five preferences, each a bool.
fn preferences(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(msg, &["preferences"])?;
    let chosen = match msg.get("preferences") {
        None => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => {
            return Err("expected a dictionary for dictionary value @ data['preferences']".into());
        }
    };
    for (key, value) in &chosen {
        if !PREFERENCES.contains(&key.as_str()) {
            return Err(format!(
                "extra keys not allowed @ data['preferences']['{key}']"
            ));
        }
        if !value.is_boolean() {
            return Err(format!(
                "expected bool for dictionary value @ data['preferences']['{key}']"
            ));
        }
    }
    if !admin(conn, id) {
        return Ok(());
    }
    let (analytics, out) = (conn.shared.config.analytics.clone(), conn.out.clone());
    tokio::spawn(async move {
        match analytics.save(chosen).await {
            Ok(answer) => out.result(id, answer),
            Err(e) => {
                tracing::error!("saving analytics preferences: {e}");
                out.error(json!(id), "unknown_error", "Unknown error")
            }
        };
    });
    Ok(())
}

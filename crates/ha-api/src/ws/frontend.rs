//! Commands the frontend sends while it starts, beyond the protocol core:
//! settings (`frontend/*_data`), themes, translations and icons, and the
//! registries, lovelace, repairs and notifications it lists. Where `hars`
//! has no such data yet the answer is HA's answer for an empty install.

use super::{Conn, Invalid, Subscription, TaskSub, only_keys};
use crate::frontend_data::Scope;
use ha_auth::Permission;
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// Handle a frontend command; `None` if `ty` isn't one.
pub(super) fn dispatch(
    conn: &mut Conn,
    id: u64,
    ty: &str,
    msg: &Map<String, Value>,
) -> Option<Result<(), Invalid>> {
    let empty = |conn: &mut Conn, result: Value| {
        only_keys(msg, &[]).map(|()| {
            conn.out.result(id, result);
        })
    };
    Some(match ty {
        "frontend/get_user_data" => get_data(conn, id, msg, user(conn), false),
        "frontend/get_system_data" => get_data(conn, id, msg, Scope::System, true),
        "frontend/set_user_data" => set_data(conn, id, msg, user(conn)),
        "frontend/set_system_data" => set_data(conn, id, msg, Scope::System),
        "frontend/subscribe_user_data" => subscribe_data(conn, id, msg, user(conn), false),
        "frontend/subscribe_system_data" => subscribe_data(conn, id, msg, Scope::System, true),
        // No themes in `hars` yet: HA's answer without a `frontend: themes:` config.
        "frontend/get_themes" => empty(
            conn,
            json!({"themes": {}, "default_theme": "default", "default_dark_theme": null}),
        ),
        // No translations bundled yet: the frontend shows raw states.
        "frontend/get_translations" => {
            only_keys(msg, &["language", "category", "integration", "config_flow"]).map(|()| {
                conn.out.result(id, json!({"resources": {}}));
            })
        }
        "frontend/get_icons" => only_keys(msg, &["category", "integration"]).map(|()| {
            conn.out.result(id, json!({"resources": {}}));
        }),
        // Registries are empty until the importer and integrations fill them.
        "config/entity_registry/list_for_display" => empty(
            conn,
            json!({"entity_categories": {"0": "config", "1": "diagnostic"}, "entities": []}),
        ),
        "config/device_registry/list"
        | "config/area_registry/list"
        | "config/floor_registry/list"
        | "config/label_registry/list" => empty(conn, json!([])),
        "lovelace/info" => empty(conn, json!({"resource_mode": "storage"})),
        "lovelace/config" => lovelace_config(conn, id, msg),
        "lovelace/resources" | "lovelace/resources/list" => empty(conn, json!([])),
        "lovelace/dashboards/list" => empty(conn, json!([])),
        "repairs/list_issues" => empty(conn, json!({"issues": []})),
        "persistent_notification/subscribe" => only_keys(msg, &[]).map(|()| {
            // Result first, then the current notifications (none), as HA.
            conn.out.result(id, Value::Null);
            conn.out
                .event(id, &json!({"type": "current", "notifications": {}}));
            conn.subs.insert(id, Subscription::Idle);
        }),
        _ => return None,
    })
}

fn user(conn: &Conn) -> Scope {
    Scope::User(conn.user.id.clone())
}

/// `key` is optional for user data (whole map), required for system data.
fn key_arg(msg: &Map<String, Value>, required: bool) -> Result<Option<String>, Invalid> {
    match msg.get("key") {
        Some(Value::String(k)) => Ok(Some(k.clone())),
        Some(_) => Err("expected str for dictionary value @ data['key']".into()),
        None if required => Err("required key not provided @ data['key']".into()),
        None => Ok(None),
    }
}

fn get_data(
    conn: &mut Conn,
    id: u64,
    msg: &Map<String, Value>,
    scope: Scope,
    key_required: bool,
) -> Result<(), Invalid> {
    only_keys(msg, &["key"])?;
    let key = key_arg(msg, key_required)?;
    let value = conn.shared.config.frontend_data.get(&scope, key.as_deref());
    conn.out.result(id, json!({ "value": value }));
    Ok(())
}

/// Saved before the result, like HA's user store. System data is admin-only.
fn set_data(
    conn: &mut Conn,
    id: u64,
    msg: &Map<String, Value>,
    scope: Scope,
) -> Result<(), Invalid> {
    only_keys(msg, &["key", "value"])?;
    let key = key_arg(msg, true)?.expect("required");
    let Some(value) = msg.get("value").cloned() else {
        return Err("required key not provided @ data['value']".into());
    };
    if scope == Scope::System && conn.principal.permission < Permission::Admin {
        conn.out.error(json!(id), "unauthorized", "Unauthorized");
        return Ok(());
    }
    let data = conn.shared.config.frontend_data.clone();
    data.set(&scope, &key, value);
    let out = conn.out.clone();
    tokio::spawn(async move {
        match data.save(&scope).await {
            Ok(()) => out.result(id, Value::Null),
            Err(e) => out.error(
                json!(id),
                "home_assistant_error",
                &format!("Saving failed: {e}"),
            ),
        };
    });
    Ok(())
}

/// The current value as an event, then the result; another event whenever
/// the key (any key, for a whole-map user subscription) is set.
fn subscribe_data(
    conn: &mut Conn,
    id: u64,
    msg: &Map<String, Value>,
    scope: Scope,
    key_required: bool,
) -> Result<(), Invalid> {
    only_keys(msg, &["key"])?;
    let key = key_arg(msg, key_required)?;
    let data = conn.shared.config.frontend_data.clone();
    // Subscribed before reading, so a change in between still arrives.
    let mut changes = data.subscribe();
    conn.out
        .event(id, &json!({ "value": data.get(&scope, key.as_deref()) }));
    conn.out.result(id, Value::Null);

    let out = conn.out.clone();
    let live = Arc::new(parking_lot::Mutex::new(true));
    let task_live = live.clone();
    let task = tokio::spawn(async move {
        loop {
            let change = match changes.recv().await {
                Ok(c) => c,
                // Missed some: re-send the current value to be safe.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    crate::frontend_data::Change {
                        scope: scope.clone(),
                        key: key.clone().unwrap_or_default(),
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            if change.scope != scope || key.as_ref().is_some_and(|k| *k != change.key) {
                continue;
            }
            let event = json!({ "value": data.get(&scope, key.as_deref()) });
            let live = task_live.lock();
            if !*live || !out.event(id, &event) {
                return;
            }
        }
    });
    conn.subs.insert(
        id,
        Subscription::Task(TaskSub {
            abort: task.abort_handle(),
            live,
        }),
    );
    Ok(())
}

/// `lovelace/config`: no dashboards are stored yet, so the default
/// dashboard has no config and any other `url_path` is unknown.
fn lovelace_config(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(msg, &["force", "url_path"])?;
    match msg.get("url_path") {
        None | Some(Value::Null) => {
            conn.out
                .error(json!(id), "config_not_found", "No config found.");
        }
        Some(Value::String(url)) => {
            conn.out.error(
                json!(id),
                "config_not_found",
                &format!("Unknown config specified: {url}"),
            );
        }
        Some(_) => return Err("expected str for dictionary value @ data['url_path']".into()),
    }
    Ok(())
}

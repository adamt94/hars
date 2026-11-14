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
        "frontend/get_translations" => get_translations(conn, id, msg),
        "frontend/get_icons" => get_icons(conn, id, msg),
        "config/entity_registry/list_for_display"
        | "config/device_registry/list"
        | "config/area_registry/list"
        | "config/floor_registry/list"
        | "config/label_registry/list" => {
            let r = &conn.shared.config.registries;
            let list = match ty {
                "config/entity_registry/list_for_display" => r.entities_for_display(),
                "config/device_registry/list" => r.devices_json(),
                "config/area_registry/list" => r.areas_json(),
                "config/floor_registry/list" => r.floors_json(),
                _ => r.labels_json(),
            };
            empty(conn, list)
        }
        "zone/list" => {
            let r = conn.shared.config.components.zones_json();
            empty(conn, r)
        }
        "person/list" => {
            let r = conn.shared.config.components.persons_json();
            empty(conn, r)
        }
        "lovelace/info" => empty(conn, json!({"resource_mode": "storage"})),
        "lovelace/config" => lovelace_config(conn, id, msg),
        "lovelace/config/save" => lovelace_save(conn, id, msg, false),
        "lovelace/config/delete" => lovelace_save(conn, id, msg, true),
        "lovelace/resources" | "lovelace/resources/list" => {
            let r = conn.shared.config.lovelace.resources_json();
            empty(conn, r)
        }
        "lovelace/dashboards/list" => {
            let r = conn.shared.config.lovelace.dashboards_json();
            empty(conn, r)
        }
        "repairs/list_issues" => empty(conn, json!({"issues": []})),
        // `require_admin`.
        "system_log/list" => super::sensor::validate(msg, false).map(|()| {
            if conn.principal.permission < Permission::Admin {
                conn.out.error(json!(id), "unauthorized", "Unauthorized");
                return;
            }
            conn.out.result(id, crate::system_log::list());
        }),
        "brands/access_token" => {
            let token = conn.shared.config.brands.as_ref()?.token();
            super::sensor::validate(msg, false).map(|()| {
                conn.out.result(id, json!({ "token": token }));
            })
        }
        "labs/list" => only_keys(msg, &[]).map(|()| {
            if conn.principal.permission < Permission::Admin {
                conn.out.error(json!(id), "unauthorized", "Unauthorized");
                return;
            }
            let r = conn
                .shared
                .config
                .labs
                .list(&crate::components(&conn.shared.config));
            conn.out.result(id, r);
        }),
        "labs/subscribe" => labs_subscribe(conn, id, msg),
        "labs/update" => labs_update(conn, id, msg),
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

/// `vol.Required(key): str`.
fn required_str<'a>(msg: &'a Map<String, Value>, key: &str) -> Result<&'a str, Invalid> {
    match msg.get(key) {
        Some(Value::String(s)) => Ok(s),
        Some(_) => Err(format!("expected str for dictionary value @ data['{key}']")),
        None => Err(format!("required key not provided @ data['{key}']")),
    }
}

/// `vol.Optional("integration"): vol.All(cv.ensure_list, [str])`.
fn integrations(msg: &Map<String, Value>) -> Result<Option<Vec<&str>>, Invalid> {
    match msg.get("integration") {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(vec![s.as_str()])),
        Some(Value::Null) => Ok(Some(Vec::new())),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, v)| {
                v.as_str()
                    .ok_or_else(|| format!("expected str @ data['integration'][{i}]"))
            })
            .collect::<Result<_, _>>()
            .map(Some),
        Some(_) => Err("expected str @ data['integration'][0]".into()),
    }
}

/// `frontend/get_translations`: English (HA's fallback language) for the
/// integrations asked for, else for those loaded. `config_flow` alone asks
/// for integrations with a config flow that aren't loaded: none in `hars`.
fn get_translations(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(msg, &["language", "category", "integration", "config_flow"])?;
    required_str(msg, "language")?;
    let category = required_str(msg, "category")?;
    let integrations = integrations(msg)?;
    let config_flow = match msg.get("config_flow") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err("expected bool for dictionary value @ data['config_flow']".into()),
    };
    let loaded;
    let domains: &[&str] = match &integrations {
        Some(list) => list,
        None if config_flow => &[],
        None => {
            loaded = crate::components(&conn.shared.config);
            &loaded
        }
    };
    conn.out
        .result_raw(id, &crate::i18n::translations(category, domains));
    Ok(())
}

/// `frontend/get_icons`, for the integrations asked for, else those loaded.
fn get_icons(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(msg, &["category", "integration"])?;
    const CATEGORIES: [&str; 5] = [
        "conditions",
        "entity",
        "entity_component",
        "services",
        "triggers",
    ];
    let category = match msg.get("category") {
        None => return Err("required key not provided @ data['category']".into()),
        Some(Value::String(c)) if CATEGORIES.contains(&c.as_str()) => c.as_str(),
        Some(_) => {
            return Err("value must be one of ['conditions', 'entity', 'entity_component', 'services', 'triggers'] for dictionary value @ data['category']".into());
        }
    };
    let loaded;
    let domains: &[&str] = match integrations(msg)? {
        Some(list) if !list.is_empty() => {
            loaded = list;
            &loaded
        }
        _ => {
            loaded = crate::components(&conn.shared.config);
            &loaded
        }
    };
    conn.out
        .result_raw(id, &crate::i18n::icons(category, domains));
    Ok(())
}

/// `labs/subscribe`: the result, the feature now as an event, then an
/// event on each change to it.
fn labs_subscribe(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(msg, &["domain", "preview_feature"])?;
    let domain = required_str(msg, "domain")?.to_owned();
    let feature = required_str(msg, "preview_feature")?.to_owned();
    let out = conn.out.clone();
    let live = Arc::new(parking_lot::Mutex::new(true));
    let listener_live = live.clone();
    let listener: crate::labs::Listener = Box::new(move |update: &Value| {
        let live = listener_live.lock();
        *live && out.event(id, update)
    });
    // Held until the first event is queued, so a change can't come first.
    let guard = live.lock();
    let Some(now) = conn
        .shared
        .config
        .labs
        .subscribe(&domain, &feature, live.clone(), listener)
    else {
        drop(guard);
        conn.out.error(
            json!(id),
            "not_found",
            &format!("Preview feature {domain}.{feature} not found"),
        );
        return Ok(());
    };
    conn.out.result(id, Value::Null);
    conn.out.event(id, &now);
    drop(guard);
    conn.subs.insert(id, Subscription::Listener(live));
    Ok(())
}

/// `labs/update` (admin): save, then fire `labs_updated`. `hars` makes no
/// backups yet, so asking for one before enabling is an error.
fn labs_update(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(
        msg,
        &["domain", "preview_feature", "enabled", "create_backup"],
    )?;
    let domain = required_str(msg, "domain")?.to_owned();
    let feature = required_str(msg, "preview_feature")?.to_owned();
    let bool_arg = |key: &str, required: bool| match msg.get(key) {
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!(
            "expected bool for dictionary value @ data['{key}']"
        )),
        None if required => Err(format!("required key not provided @ data['{key}']")),
        None => Ok(false),
    };
    let enabled = bool_arg("enabled", true)?;
    let backup = bool_arg("create_backup", false)?;
    if conn.principal.permission < Permission::Admin {
        conn.out.error(json!(id), "unauthorized", "Unauthorized");
        return Ok(());
    }
    let labs = conn.shared.config.labs.clone();
    let hass = conn.shared.hass.clone();
    let out = conn.out.clone();
    tokio::spawn(async move {
        if labs.describe(&domain, &feature, false).is_none() {
            out.error(
                json!(id),
                "not_found",
                &format!("Preview feature {domain}.{feature} not found"),
            );
            return;
        }
        if backup && enabled {
            out.error(
                json!(id),
                "unknown_error",
                "Error creating backup: backups are not available in hars yet",
            );
            return;
        }
        labs.update(&domain, &feature, enabled).await;
        let now = hass.clock().now();
        hass.bus().fire(ha_core::Event::new(
            ha_core::EventKind::Custom {
                name: "labs_updated".into(),
                data: json!({"domain": domain, "preview_feature": feature, "enabled": enabled}),
            },
            now,
            // HA fires it without the caller's context.
            ha_core::Context::at(now),
        ));
        out.result(id, Value::Null);
    });
    Ok(())
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

fn url_path(msg: &Map<String, Value>) -> Result<Option<String>, Invalid> {
    match msg.get("url_path") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(u)) => Ok(Some(u.clone())),
        Some(_) => Err("expected str for dictionary value @ data['url_path']".into()),
    }
}

fn lovelace_error(conn: &Conn, id: u64, e: crate::lovelace::ConfigError) {
    use crate::lovelace::ConfigError;
    let message = match e {
        ConfigError::UnknownDashboard(url) => format!(
            "Unknown config specified: {}",
            url.as_deref().unwrap_or("None")
        ),
        ConfigError::NoConfig => "No config found.".into(),
        ConfigError::Save(e) => {
            conn.out.error(json!(id), "error", &e);
            return;
        }
    };
    conn.out.error(json!(id), "config_not_found", &message);
}

/// `lovelace/config`.
fn lovelace_config(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(msg, &["force", "url_path"])?;
    let url = url_path(msg)?;
    match conn.shared.config.lovelace.config(url.as_deref()) {
        Ok(config) => {
            conn.out.result(id, config);
        }
        Err(e) => lovelace_error(conn, id, e),
    }
    Ok(())
}

/// `lovelace/config/save` and `lovelace/config/delete` (admin only): store
/// it, then fire `lovelace_updated` so open dashboards reload.
fn lovelace_save(
    conn: &mut Conn,
    id: u64,
    msg: &Map<String, Value>,
    delete: bool,
) -> Result<(), Invalid> {
    let config = if delete {
        only_keys(msg, &["url_path"])?;
        None
    } else {
        only_keys(msg, &["config", "url_path"])?;
        match msg.get("config") {
            Some(c @ (Value::String(_) | Value::Object(_))) => Some(c.clone()),
            Some(_) => return Err("expected dict for dictionary value @ data['config']".into()),
            None => return Err("required key not provided @ data['config']".into()),
        }
    };
    let url = url_path(msg)?;
    if conn.principal.permission < Permission::Admin {
        conn.out.error(json!(id), "unauthorized", "Unauthorized");
        return Ok(());
    }
    let lovelace = conn.shared.config.lovelace.clone();
    let hass = conn.shared.hass.clone();
    let out = conn.out.clone();
    let user = conn.user.id.clone();
    tokio::spawn(async move {
        match lovelace.save(url.as_deref(), config).await {
            Ok(saved) => {
                let now = hass.clock().now();
                hass.bus().fire(ha_core::Event::new(
                    ha_core::EventKind::Custom {
                        name: "lovelace_updated".into(),
                        data: json!({ "url_path": saved }),
                    },
                    now,
                    ha_core::Context::at(now).with_user(user.as_str()),
                ));
                out.result(id, Value::Null);
            }
            Err(e) => {
                use crate::lovelace::ConfigError;
                let message = match e {
                    ConfigError::UnknownDashboard(url) => format!(
                        "Unknown config specified: {}",
                        url.as_deref().unwrap_or("None")
                    ),
                    ConfigError::NoConfig => "No config found.".into(),
                    ConfigError::Save(e) => {
                        out.error(json!(id), "error", &e);
                        return;
                    }
                };
                out.error(json!(id), "config_not_found", &message);
            }
        }
    });
    Ok(())
}

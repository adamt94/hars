//! The WebSocket commands (`websocket_api/commands.py` and the components
//! that register commands the frontend needs at start-up).

use super::{Conn, Invalid, MAX_PENDING, Subscription, TaskSub, entities, only_keys};
use crate::rest::service_description;
use ha_auth::Permission;
use ha_core::{Context, Event, EventKind, ServiceData, ServiceError};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// `core/auth/permissions/events.py`: what non-admins may subscribe to.
const SUBSCRIBE_ALLOWLIST: &[&str] = &[
    "area_registry_updated",
    "component_loaded",
    "core_config_updated",
    "device_registry_updated",
    "entity_registry_updated",
    "repairs_issue_registry_updated",
    "lovelace_updated",
    "panels_updated",
    "recorder_5min_statistics_generated",
    "recorder_hourly_statistics_generated",
    "service_registered",
    "service_removed",
    "shopping_list_updated",
    "state_changed",
    "themes_updated",
    "label_registry_updated",
    "labs_updated",
    "category_registry_updated",
    "floor_registry_updated",
];

/// Run one command. `false` if the command doesn't exist.
pub(super) fn dispatch(conn: &mut Conn, id: u64, ty: &str, msg: &Map<String, Value>) -> bool {
    let res = match ty {
        "ping" => only_keys(msg, &[]).map(|()| {
            conn.out.send(json!({"id": id, "type": "pong"}).to_string());
        }),
        "supported_features" => supported_features(conn, id, msg),
        "get_states" => only_keys(msg, &[]).map(|()| {
            conn.out.result(id, json!(conn.shared.hass.states().all()));
        }),
        "get_config" => only_keys(msg, &[]).map(|()| {
            conn.out
                .result(id, crate::config_json(&conn.shared, conn.user.local_only));
        }),
        "get_services" => only_keys(msg, &[]).map(|()| {
            conn.out.result(id, services(conn));
        }),
        "get_panels" => only_keys(msg, &[]).map(|()| {
            let lovelace = conn.shared.config.lovelace.panels();
            conn.out.result(
                id,
                panels(conn.principal.permission == Permission::Admin, lovelace),
            );
        }),
        "auth/current_user" => only_keys(msg, &[]).map(|()| {
            conn.out.result(id, current_user(conn));
        }),
        "call_service" => call_service(conn, id, msg),
        "subscribe_events" => subscribe_events(conn, id, msg),
        "unsubscribe_events" => unsubscribe_events(conn, id, msg),
        "subscribe_entities" => subscribe_entities(conn, id, msg),
        _ => match super::frontend::dispatch(conn, id, ty, msg)
            .or_else(|| super::weather::dispatch(conn, id, ty, msg))
        {
            Some(res) => res,
            None => return false,
        },
    };
    if let Err(why) = res {
        conn.out.error(json!(id), "invalid_format", &why);
    }
    true
}

fn field<'a>(msg: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    msg.get(key)
}

fn required_str<'a>(msg: &'a Map<String, Value>, key: &str) -> Result<&'a str, Invalid> {
    match field(msg, key) {
        Some(Value::String(s)) => Ok(s),
        Some(_) => Err(format!("expected str for dictionary value @ data['{key}']")),
        None => Err(format!("required key not provided @ data['{key}']")),
    }
}

fn optional_dict<'a>(
    msg: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a Map<String, Value>>, Invalid> {
    match field(msg, key) {
        None => Ok(None),
        Some(Value::Object(m)) => Ok(Some(m)),
        Some(_) => Err(format!(
            "expected dict for dictionary value @ data['{key}']"
        )),
    }
}

/// `{"features": {str: int}}`; `coalesce_messages` turns on batching.
fn supported_features(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(msg, &["features"])?;
    let Some(Value::Object(features)) = field(msg, "features") else {
        return Err("required key not provided @ data['features']".into());
    };
    if let Some((k, _)) = features.iter().find(|(_, v)| !v.is_i64()) {
        return Err(format!(
            "expected int for dictionary value @ data['features']['{k}']"
        ));
    }
    conn.coalesce.store(
        features.contains_key("coalesce_messages"),
        Ordering::Relaxed,
    );
    conn.out.result(id, Value::Null);
    Ok(())
}

/// `{domain: {service: description}}` for every registered service.
fn services(conn: &Conn) -> Value {
    let mut domains = Map::new();
    for (domain, services) in conn.shared.hass.services().services() {
        let services: Map<String, Value> = services
            .into_iter()
            .map(|(name, supports)| (name.to_string(), service_description(supports)))
            .collect();
        domains.insert(domain.to_string(), Value::Object(services));
    }
    Value::Object(domains)
}

/// The panels a default HA 2026.6.1 install registers that `hars` can
/// serve. `home` is the frontend's default panel; `lovelace` without a
/// config redirects there. `config` arrives with the settings commands.
fn panels(is_admin: bool, dashboards: Vec<(String, Value)>) -> Value {
    let panel = |url: &str, title: Option<&str>, icon: Option<&str>, sidebar: bool, admin: bool| {
        (
            url.to_owned(),
            json!({
                "component_name": url,
                "icon": icon,
                "title": title,
                "default_visible": true,
                "config": null,
                "url_path": url,
                "require_admin": admin,
                "config_panel_domain": null,
                "show_in_sidebar": sidebar,
            }),
        )
    };
    let all = [
        panel("light", Some("light"), Some("mdi:lamps"), false, false),
        panel(
            "security",
            Some("security"),
            Some("mdi:security"),
            false,
            false,
        ),
        panel(
            "climate",
            Some("climate"),
            Some("mdi:home-thermometer"),
            false,
            false,
        ),
        panel("home", Some("home"), Some("mdi:home"), false, false),
        panel(
            "maintenance",
            Some("maintenance"),
            Some("mdi:wrench"),
            false,
            false,
        ),
        panel("profile", None, None, true, false),
        panel("notfound", None, None, true, false),
        panel("lovelace", None, None, true, false),
    ];
    // Each dashboard is a panel; one at `lovelace` replaces the default.
    let visible: Map<String, Value> = all
        .into_iter()
        .chain(dashboards)
        .filter(|(_, p)| is_admin || p["require_admin"] == false)
        .collect();
    Value::Object(visible)
}

fn current_user(conn: &Conn) -> Value {
    let credentials: Vec<Value> = conn
        .shared
        .auth
        .credentials_of(&conn.user.id)
        .into_iter()
        .map(|c| json!({"auth_provider_type": c.auth_provider_type, "auth_provider_id": c.auth_provider_id}))
        .collect();
    json!({
        "id": conn.user.id,
        "name": conn.user.name,
        "is_owner": conn.user.is_owner,
        "is_admin": conn.user.is_admin(),
        "credentials": credentials,
        "mfa_modules": [],
    })
}

/// `handle_call_service`: always blocking, with the user's context. Runs in
/// its own task so a slow service doesn't hold up pings.
fn call_service(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(
        msg,
        &[
            "domain",
            "service",
            "service_data",
            "target",
            "return_response",
        ],
    )?;
    let domain = required_str(msg, "domain")?.to_owned();
    let service = required_str(msg, "service")?.to_owned();
    let data: ServiceData = optional_dict(msg, "service_data")?
        .cloned()
        .unwrap_or_default();
    let target = optional_dict(msg, "target")?.cloned();
    let return_response = match field(msg, "return_response") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return Err("expected bool for dictionary value @ data['return_response']".into());
        }
    };
    if conn.principal.permission < Permission::Control {
        conn.out.error(json!(id), "unauthorized", "Unauthorized");
        return Ok(());
    }
    let hass = conn.shared.hass.clone();
    let out = conn.out.clone();
    let context = Context::at(hass.clock().now()).with_user(conn.user.id.as_str());
    tokio::spawn(async move {
        let result = hass
            .call_service(
                &domain,
                &service,
                data,
                target,
                return_response,
                Some(context.clone()),
            )
            .await;
        let rr = || json!({"return_response": "return_response=True"});
        match result {
            Ok(response) => {
                let mut result = json!({ "context": context });
                if return_response {
                    result["response"] = json!(response);
                }
                out.result(id, result)
            }
            Err(ServiceError::NotFound { domain, service }) => out.error_with(
                json!(id),
                json!({
                    "code": "not_found",
                    "message": format!("Service {domain}.{service} not found."),
                    "translation_key": "service_not_found",
                    "translation_placeholders": {"domain": domain, "service": service},
                    "translation_domain": "homeassistant",
                }),
            ),
            Err(ServiceError::Invalid(why)) => out.error(json!(id), "invalid_format", &why),
            Err(ServiceError::ResponseNotSupported) => out.error_with(
                json!(id),
                json!({
                    "code": "service_validation_error",
                    "message": "Validation error: An action which does not return responses can't be called with return_response=True.",
                    "translation_key": "service_does_not_support_response",
                    "translation_placeholders": rr(),
                    "translation_domain": "homeassistant",
                }),
            ),
            Err(ServiceError::ResponseRequired) => out.error_with(
                json!(id),
                json!({
                    "code": "service_validation_error",
                    "message": "Validation error: The action requires responses and must be called with return_response=True.",
                    "translation_key": "service_lacks_response_request",
                    "translation_placeholders": rr(),
                    "translation_domain": "homeassistant",
                }),
            ),
            Err(ServiceError::Failed(why)) => out.error(json!(id), "home_assistant_error", &why),
        };
    });
    Ok(())
}

fn subscribe_events(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(msg, &["event_type"])?;
    let event_type = match field(msg, "event_type") {
        None => "*",
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return Err("expected str for dictionary value @ data['event_type']".into()),
    };
    if conn.principal.permission < Permission::Admin && !SUBSCRIBE_ALLOWLIST.contains(&event_type) {
        conn.out.error(json!(id), "unauthorized", "Unauthorized");
        return Ok(());
    }
    // Listening before the result goes out, so nothing after it is missed;
    // under the listener's lock, so no event for `id` precedes the result.
    conn.ensure_event_listener();
    let events = conn.events.clone();
    let mut events = events.lock();
    events.entry(event_type.to_owned()).or_default().push(id);
    conn.out.result(id, Value::Null);
    drop(events);
    conn.subs.insert(id, Subscription::Events);
    Ok(())
}

fn unsubscribe_events(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(msg, &["subscription"])?;
    let sub = match field(msg, "subscription") {
        Some(v) if v.is_u64() || v.is_i64() => v.as_u64(),
        Some(_) => return Err("expected int for dictionary value @ data['subscription']".into()),
        None => return Err("required key not provided @ data['subscription']".into()),
    };
    let found = sub.is_some_and(|s| {
        conn.unsubscribe(s, |out| {
            out.result(id, Value::Null);
        })
    });
    if !found {
        conn.out
            .error(json!(id), "not_found", "Subscription not found.");
    }
    Ok(())
}

/// `handle_subscribe_entities`: the snapshot and the change feed are taken
/// atomically; the result goes out, then the snapshot, then one event per
/// change, in order.
fn subscribe_entities(conn: &mut Conn, id: u64, msg: &Map<String, Value>) -> Result<(), Invalid> {
    only_keys(msg, &["entity_ids"])?;
    let wanted: Option<Vec<String>> = match field(msg, "entity_ids") {
        None => None,
        Some(Value::String(s)) => Some(vec![s.clone()]),
        Some(Value::Array(a)) if a.iter().all(Value::is_string) => Some(
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
        ),
        Some(_) => return Err("expected a list for dictionary value @ data['entity_ids']".into()),
    };
    let wanted = wanted.filter(|w| !w.is_empty());
    let keep = move |entity_id: &str| {
        wanted
            .as_ref()
            .is_none_or(|w| w.iter().any(|e| e == entity_id))
    };

    let mut feed = conn.shared.hass.subscribe_state_changes();
    // Taken out of the feed so the task doesn't keep every initial state alive.
    let initial: Map<String, Value> = std::mem::take(&mut feed.snapshot)
        .iter()
        .filter(|s| keep(s.entity_id.as_str()))
        .map(|s| (s.entity_id.to_string(), entities::compressed(s)))
        .collect();
    conn.out.result(id, Value::Null);
    conn.out.event(id, &json!({ "a": initial }));

    let out = conn.out.clone();
    let live = Arc::new(parking_lot::Mutex::new(true));
    let task_live = live.clone();
    let task = tokio::spawn(async move {
        while let Some(event) = feed.recv().await {
            // The feed itself is unbounded: hold it to the outbox's limit.
            if feed.backlog() > MAX_PENDING {
                out.overflowed();
                return;
            }
            let EventKind::StateChanged { entity_id, .. } = &event.kind else {
                continue;
            };
            if !keep(entity_id.as_str()) {
                continue;
            }
            let Some(diff) = entities::diff_event(&event.kind) else {
                continue;
            };
            let live = task_live.lock();
            if !*live || !out.event(id, &diff) {
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

/// HA's `event_type` string.
pub(super) fn event_type(kind: &EventKind) -> &str {
    match kind {
        EventKind::StateChanged { .. } => "state_changed",
        EventKind::CallService { .. } => "call_service",
        EventKind::HomeassistantStart {} => "homeassistant_start",
        EventKind::HomeassistantStarted {} => "homeassistant_started",
        EventKind::HomeassistantStop {} => "homeassistant_stop",
        EventKind::HomeassistantFinalWrite {} => "homeassistant_final_write",
        EventKind::HomeassistantClose {} => "homeassistant_close",
        EventKind::Custom { name, .. } => name,
    }
}

/// `Event.as_dict()`: `{event_type, data, origin, time_fired, context}`.
pub(super) fn event_json(event: &Event) -> Value {
    let data = match &event.kind {
        EventKind::StateChanged {
            entity_id,
            old_state,
            new_state,
        } => json!({"entity_id": entity_id, "old_state": old_state, "new_state": new_state}),
        EventKind::CallService {
            domain,
            service,
            service_data,
        } => json!({"domain": domain, "service": service, "service_data": service_data}),
        EventKind::Custom { data, .. } => data.clone(),
        _ => json!({}),
    };
    json!({
        "event_type": event_type(&event.kind),
        "data": data,
        "origin": "LOCAL",
        "time_fired": event.time_fired,
        "context": event.context,
    })
}

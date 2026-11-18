//! The WebSocket commands (`websocket_api/commands.py` and the components
//! that register commands the frontend needs at start-up).

use super::{Conn, Invalid, MAX_PENDING, Subscription, TaskSub, entities, only_keys};
use crate::rest::service_description;
use ha_auth::Permission;
use ha_core::entityfilter::{EntityFilter, FilterLists};
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
                panels(
                    conn.principal.permission == Permission::Admin,
                    conn.shared.config.recorder.is_some(),
                    lovelace,
                ),
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
            .or_else(|| super::sensor::dispatch(conn, id, ty, msg))
            .or_else(|| super::manifest::dispatch(conn, id, ty, msg))
            .or_else(|| super::automation::dispatch(conn, id, ty, msg))
            .or_else(|| super::entity_registry::dispatch(conn, id, ty, msg))
            .or_else(|| super::mobile_app::dispatch(conn, id, ty, msg))
            .or_else(|| super::recorder::dispatch(conn, id, ty, msg))
            .or_else(|| super::history::dispatch(conn, id, ty, msg))
            .or_else(|| super::logbook::dispatch(conn, id, ty, msg))
            .or_else(|| super::config::dispatch(conn, id, ty, msg))
            .or_else(|| super::config_entries::dispatch(conn, id, ty, msg))
            .or_else(|| super::analytics::dispatch(conn, id, ty, msg))
            .or_else(|| super::template::dispatch(conn, id, ty, msg))
        {
            Some(res) => res,
            None => return false,
        },
    };
    if let Err(why) = res {
        let message = super::humanize(&Value::Object(msg.clone()), &why);
        conn.out.error(json!(id), "invalid_format", &message);
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
/// config redirects there.
fn panels(is_admin: bool, recording: bool, dashboards: Vec<(String, Value)>) -> Value {
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
        // Settings (`config/__init__.py`): for admins.
        panel("config", Some("config"), Some("mdi:cog"), true, true),
    ];
    // `history/__init__.py` and `logbook/__init__.py`: there with the
    // recorder, as the components `get_config` lists.
    let history = [
        panel(
            "history",
            Some("history"),
            Some("mdi:chart-box"),
            true,
            false,
        ),
        panel(
            "logbook",
            Some("logbook"),
            Some("mdi:format-list-bulleted-type"),
            true,
            false,
        ),
    ];
    // Each dashboard is a panel; one at `lovelace` replaces the default.
    let visible: Map<String, Value> = all
        .into_iter()
        .chain(history.into_iter().filter(|_| recording))
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
            // A service the called one called (HA swaps the names).
            Err(ServiceError::NotFound {
                domain: child_domain,
                service: child_service,
            }) if (child_domain.as_str(), child_service.as_str()) != (&*domain, &*service) => out
                .error_with(
                    json!(id),
                    json!({
                        "code": "home_assistant_error",
                        "message": format!("Service {child_domain}.{child_service} called service {domain}.{service} which was not found."),
                        "translation_key": "child_service_not_found",
                        "translation_placeholders": {"domain": domain, "service": service,
                            "child_domain": child_domain, "child_service": child_service},
                        "translation_domain": "homeassistant",
                    }),
                ),
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
            Err(ServiceError::Translated(r)) => out.error_with(
                json!(id),
                json!({
                    "code": "home_assistant_error",
                    "message": r.message,
                    "translation_key": r.key,
                    "translation_placeholders": r.placeholders,
                    "translation_domain": r.domain,
                }),
            ),
            // One raised with no translation carries only its message.
            Err(ServiceError::Validation(r)) if r.key.is_empty() => out.error(
                json!(id),
                "service_validation_error",
                &format!("Validation error: {}", r.message),
            ),
            Err(ServiceError::Validation(r)) => out.error_with(
                json!(id),
                json!({
                    "code": "service_validation_error",
                    "message": format!("Validation error: {}", r.message),
                    "translation_key": r.key,
                    "translation_placeholders": r.placeholders,
                    "translation_domain": r.domain,
                }),
            ),
            Err(ServiceError::Unknown(why)) => out.error(json!(id), "unknown_error", &why),
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
    let (wanted, filter) = match entities_filter(msg) {
        Ok(parsed) => parsed,
        Err(Bad::Invalid(errors)) => return Err(errors.join("\n")),
        // A `TypeError` in the schema: HA's catch-all.
        Err(Bad::TypeError) => {
            conn.out.error(json!(id), "unknown_error", "Unknown error");
            return Ok(());
        }
    };
    let keep = move |entity_id: &str| {
        (wanted.is_empty() || wanted.iter().any(|e| e == entity_id))
            && filter.as_ref().is_none_or(|f| f.matches(entity_id))
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

/// Why a schema rejected a message: voluptuous's errors, or a `TypeError`
/// from a validator, which HA answers with `unknown_error`.
pub(super) enum Bad {
    Invalid(Vec<String>),
    TypeError,
}

/// `subscribe_entities`'s schema: `entity_ids` (`cv.entity_ids`) and HA's
/// `INCLUDE_EXCLUDE_BASE_FILTER_SCHEMA`. Every error is collected, as
/// voluptuous does; a `TypeError` anywhere wins.
fn entities_filter(msg: &Map<String, Value>) -> Result<(Vec<String>, Option<EntityFilter>), Bad> {
    let mut errors = Vec::new();
    let mut type_error = false;
    let mut wanted = Vec::new();
    let mut sides = [FilterLists::default(), FilterLists::default()];
    for (key, value) in msg {
        let side = match key.as_str() {
            "id" | "type" => continue,
            "entity_ids" => {
                match cv_entity_ids(value, &format!("['{key}']")) {
                    Ok(ids) => wanted = ids,
                    Err(Bad::Invalid(e)) => errors.extend(e),
                    Err(Bad::TypeError) => type_error = true,
                }
                continue;
            }
            "include" => 0,
            "exclude" => 1,
            _ => {
                errors.push(format!("extra keys not allowed @ data['{key}']"));
                continue;
            }
        };
        match filter_lists(value, key) {
            Ok(lists) => sides[side] = lists,
            Err(Bad::Invalid(e)) => errors.extend(e),
            Err(Bad::TypeError) => type_error = true,
        }
    }
    if type_error {
        return Err(Bad::TypeError);
    }
    if !errors.is_empty() {
        return Err(Bad::Invalid(errors));
    }
    let [include, exclude] = sides;
    let filter = EntityFilter::new(include, exclude);
    Ok((wanted, (!filter.is_empty()).then_some(filter)))
}

/// `INCLUDE_EXCLUDE_FILTER_SCHEMA_INNER` for `data[key]`.
fn filter_lists(value: &Value, key: &str) -> Result<FilterLists, Bad> {
    let Value::Object(m) = value else {
        return Err(Bad::Invalid(vec![format!(
            "expected a dictionary for dictionary value @ data['{key}']"
        )]));
    };
    let mut errors = Vec::new();
    let mut type_error = false;
    let mut lists = FilterLists::default();
    for (inner, v) in m {
        let path = format!("['{key}']['{inner}']");
        match inner.as_str() {
            "domains" | "entity_globs" => {
                // `vol.All(cv.ensure_list, [cv.string])`.
                let items = match v {
                    Value::Null => Vec::new(),
                    Value::Array(a) => a.iter().collect(),
                    v => vec![v],
                };
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.into_iter().enumerate() {
                    match crate::cv::string(item) {
                        Ok(s) => out.push(s),
                        Err(why) => errors.push(format!("{why} @ data{path}[{i}]")),
                    }
                }
                if inner == "domains" {
                    lists.domains = out;
                } else {
                    lists.entity_globs = out;
                }
            }
            "entities" => match cv_entity_ids(v, &path) {
                Ok(ids) => lists.entities = ids,
                Err(Bad::Invalid(e)) => errors.extend(e),
                Err(Bad::TypeError) => type_error = true,
            },
            _ => errors.push(format!("extra keys not allowed @ data{path}")),
        }
    }
    if type_error {
        return Err(Bad::TypeError);
    }
    if errors.is_empty() {
        Ok(lists)
    } else {
        Err(Bad::Invalid(errors))
    }
}

/// `cv.entity_ids` at `data<path>`: a comma-separated string or a list
/// (a dict gives its keys), each lowercased and checked. The first bad id
/// is the error.
pub(super) fn cv_entity_ids(v: &Value, path: &str) -> Result<Vec<String>, Bad> {
    let fail = |why: String| Bad::Invalid(vec![format!("{why} for dictionary value @ data{path}")]);
    let items: Vec<Value> = match v {
        Value::Null => return Err(fail("Entity IDs cannot be None".into())),
        Value::String(s) => s
            .split(',')
            // `str.strip()`: Unicode whitespace and the separators U+1C–1F.
            .map(|id| {
                Value::String(
                    id.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
                        .to_owned(),
                )
            })
            .collect(),
        Value::Array(a) => a.clone(),
        Value::Object(m) => m.keys().map(|k| Value::String(k.clone())).collect(),
        Value::Bool(_) | Value::Number(_) => return Err(Bad::TypeError),
    };
    let mut ids = Vec::with_capacity(items.len());
    for item in &items {
        let s = crate::cv::string(item).map_err(|why| fail(why.into()))?;
        let lower = s.to_lowercase();
        if !ha_core::valid_entity_id(&lower) {
            return Err(fail(format!("Entity ID {s} is an invalid entity ID")));
        }
        ids.push(lower);
    }
    Ok(ids)
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
        "origin": event.origin.as_str(),
        "time_fired": ha_core::isoformat(event.time_fired),
        "context": event.context,
    })
}

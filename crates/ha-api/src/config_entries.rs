//! The config flow and config entry views
//! (`components/config/config_entries.py` on `helpers/data_entry_flow.py`'s
//! `FlowManagerIndexView` and `FlowManagerResourceView`): what the
//! frontend's Add integration dialog posts to, and the entry's delete.
//! The flows themselves are `ha_integrations::flow`'s.
//!
//! Where `hars` differs from HA: `flow_handlers` lists the integrations
//! `hars` has a flow for, not the integrations HA ships.
//!
//! As in HA, starting, reading and answering a flow need an admin, and
//! deleting one only a signed-in user (`FlowManagerResourceView.delete`
//! has no check).

use crate::Shared;
use crate::auth::json_message;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router, body::Bytes};
use ha_integrations::flow::{self, Error, FlowResult, Manager};
use serde_json::{Map, Value, json};
use std::sync::Arc;

pub(crate) const FLOW: &str = "/api/config/config_entries/flow";
pub(crate) const FLOW_ID: &str = "/api/config/config_entries/flow/{flow_id}";
pub(crate) const ENTRY_ID: &str = "/api/config/config_entries/entry/{entry_id}";
const HANDLERS: &str = "/api/config/config_entries/flow_handlers";

#[derive(Clone)]
struct Views {
    shared: Shared,
    flows: Arc<Manager>,
}

/// The views, when `hars` runs config flows (HA registers them with its
/// `config` integration).
pub(crate) fn routes(shared: Shared) -> Router {
    let Some(flows) = shared.config.flows.clone() else {
        return Router::new();
    };
    Router::new()
        .route(
            FLOW,
            post(init)
                // The view's `get` raises `HTTPMethodNotAllowed("GET", ["POST"])`.
                .get(|| async { not_allowed("POST") })
                .fallback(|| async { not_allowed("GET,OPTIONS,POST") }),
        )
        .route(
            FLOW_ID,
            get(show)
                // HA's view has no `head`, and axum would answer one with
                // `get`'s handler.
                .head(|| async { not_allowed("DELETE,GET,OPTIONS,POST") })
                .post(answer)
                .delete(abort)
                .fallback(|| async { not_allowed("DELETE,GET,OPTIONS,POST") }),
        )
        .route(
            HANDLERS,
            get(handlers).head(|| async { not_allowed("GET,OPTIONS") }),
        )
        .route(ENTRY_ID, delete(remove_entry))
        .with_state(Views { shared, flows })
}

/// aiohttp's `HTTPMethodNotAllowed`.
fn not_allowed(allow: &'static str) -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::ALLOW, allow),
        ],
        "405: Method Not Allowed",
    )
        .into_response()
}

/// `RequestDataValidator`'s refusal.
fn incorrect(why: &str) -> Response {
    json_message(
        StatusCode::BAD_REQUEST,
        &format!("Message format incorrect: {why}"),
    )
}

/// `_prepare_result_json` and `_prepare_config_flow_result_json`.
fn result_json(views: &Views, result: FlowResult) -> Value {
    match result {
        FlowResult::Form {
            flow_id,
            handler,
            step_id,
            schema,
            errors,
            description_placeholders,
        } => {
            let errors: Map<String, Value> = errors
                .into_iter()
                .map(|(field, error)| (field.to_owned(), json!(error)))
                .collect();
            json!({
                "type": "form",
                "flow_id": flow_id,
                "handler": handler,
                "data_schema": flow::schema_json(schema.as_deref()),
                "errors": errors,
                "description_placeholders": description_placeholders,
                "last_step": null,
                "preview": null,
                "step_id": step_id,
            })
        }
        FlowResult::CreateEntry {
            flow_id,
            handler,
            title,
            version,
            minor_version,
            entry,
            runtime,
        } => json!({
            "type": "create_entry",
            "flow_id": flow_id,
            "handler": handler,
            "description": null,
            "description_placeholders": null,
            "title": title,
            "minor_version": minor_version,
            "options": {},
            "subentries": [],
            "version": version,
            "result": crate::ws::config_entries::entry_json_at(
                &views.shared.config,
                &entry,
                runtime,
            ),
        }),
        FlowResult::Abort {
            flow_id,
            handler,
            reason,
        } => json!({
            "type": "abort",
            "flow_id": flow_id,
            "handler": handler,
            "reason": reason,
            "description_placeholders": null,
        }),
    }
}

fn reply(views: &Views, result: Result<FlowResult, Error>) -> Response {
    match result {
        Ok(result) => Json(result_json(views, result)).into_response(),
        Err(Error::UnknownHandler) => {
            json_message(StatusCode::NOT_FOUND, "Invalid handler specified")
        }
        Err(Error::UnknownFlow) => json_message(StatusCode::NOT_FOUND, "Invalid flow specified"),
        Err(Error::UnknownStep(message)) => json_message(StatusCode::BAD_REQUEST, &message),
        Err(Error::InvalidData(errors)) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "errors": errors }))).into_response()
        }
    }
}

/// `ConfigManagerFlowIndexView.post`: start a flow. The body is checked as
/// `{vol.Required("handler"): vol.Any(str, list), vol.Optional("entry_id"):
/// cv.string}` with other keys allowed (the frontend sends
/// `show_advanced_options`).
async fn init(State(views): State<Views>, body: Bytes) -> Response {
    let Ok(data) = serde_json::from_slice::<Value>(&body) else {
        return json_message(StatusCode::BAD_REQUEST, "Invalid JSON.");
    };
    let Value::Object(data) = data else {
        return incorrect("expected a dictionary");
    };
    let handler = match data.get("handler") {
        None => return incorrect("required key not provided @ data['handler']"),
        Some(Value::String(handler)) => handler,
        // A list passes the schema, then can't be a handler's key: HA's
        // view raises.
        Some(Value::Array(_)) => return crate::internal_error(),
        Some(_) => return incorrect("expected str for dictionary value @ data['handler']"),
    };
    // `get_context`: an entry id that isn't empty asks to reconfigure it.
    let source = match data.get("entry_id") {
        None => flow::SOURCE_USER,
        Some(Value::Null) => {
            return incorrect("string value is None for dictionary value @ data['entry_id']");
        }
        Some(Value::Array(_) | Value::Object(_)) => {
            return incorrect("value should be a string for dictionary value @ data['entry_id']");
        }
        Some(Value::String(id)) if id.is_empty() => flow::SOURCE_USER,
        Some(_) => flow::SOURCE_RECONFIGURE,
    };
    let result = views.flows.init(handler, source).await;
    reply(&views, result)
}

/// `FlowManagerResourceView.get`: the flow's current step, shown again.
async fn show(State(views): State<Views>, Path(flow_id): Path<String>) -> Response {
    let result = views.flows.configure(&flow_id, None).await;
    reply(&views, result)
}

/// A JSON object's members in the order sent (a `dict` in HA; a repeated
/// key keeps its first place and its last value), or anything else.
enum Body {
    Object(Vec<(String, Value)>),
    Other,
}

impl<'de> serde::Deserialize<'de> for Body {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Members;
        impl<'de> serde::de::Visitor<'de> for Members {
            type Value = Body;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("any JSON value")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Body, A::Error> {
                let mut members: Vec<(String, Value)> = Vec::new();
                // Where each key is, so a long body is read in one pass.
                let mut places: std::collections::HashMap<String, usize> = Default::default();
                while let Some((key, value)) = map.next_entry::<String, Value>()? {
                    match places.get(&key) {
                        Some(&i) => members[i].1 = value,
                        None => {
                            places.insert(key.clone(), members.len());
                            members.push((key, value));
                        }
                    }
                }
                Ok(Body::Object(members))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Body, A::Error> {
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(Body::Other)
            }

            fn visit_bool<E>(self, _: bool) -> Result<Body, E> {
                Ok(Body::Other)
            }

            fn visit_i64<E>(self, _: i64) -> Result<Body, E> {
                Ok(Body::Other)
            }

            fn visit_u64<E>(self, _: u64) -> Result<Body, E> {
                Ok(Body::Other)
            }

            fn visit_f64<E>(self, _: f64) -> Result<Body, E> {
                Ok(Body::Other)
            }

            fn visit_str<E>(self, _: &str) -> Result<Body, E> {
                Ok(Body::Other)
            }

            fn visit_unit<E>(self) -> Result<Body, E> {
                Ok(Body::Other)
            }
        }
        d.deserialize_any(Members)
    }
}

/// `FlowManagerResourceView.post`: a form's answer.
/// `RequestDataValidator(vol.Schema(dict), allow_empty=True)`: a body that
/// isn't JSON (or is empty) counts as `{}`.
async fn answer(State(views): State<Views>, Path(flow_id): Path<String>, body: Bytes) -> Response {
    let input = match serde_json::from_slice::<Body>(&body) {
        Ok(Body::Object(members)) => members,
        Ok(Body::Other) => return incorrect("expected dict"),
        Err(_) => Vec::new(),
    };
    let result = views.flows.configure(&flow_id, Some(input)).await;
    reply(&views, result)
}

/// `FlowManagerResourceView.delete`: the dialog closed.
async fn abort(State(views): State<Views>, Path(flow_id): Path<String>) -> Response {
    match views.flows.abort(&flow_id) {
        Ok(()) => json_message(StatusCode::OK, "Flow aborted"),
        Err(_) => json_message(StatusCode::NOT_FOUND, "Invalid flow specified"),
    }
}

/// `ConfigManagerAvailableFlowView`: the integrations with a flow, all or
/// of one of HA's two lists (`integration`, `helper`). Another `type` is a
/// `KeyError` in HA's view.
async fn handlers(Query(query): Query<Vec<(String, String)>>) -> Response {
    let kind = query.iter().find(|(k, _)| k == "type").map(|(_, v)| &**v);
    let helper =
        |domain: &str| crate::ws::manifest::integration_type(domain).as_deref() == Some("helper");
    let domains: Vec<&str> = match kind {
        None => flow::HANDLERS.iter().map(|h| h.domain).collect(),
        Some("integration") => flow::HANDLERS
            .iter()
            .map(|h| h.domain)
            .filter(|d| !helper(d))
            .collect(),
        Some("helper") => flow::HANDLERS
            .iter()
            .map(|h| h.domain)
            .filter(|d| helper(d))
            .collect(),
        Some(_) => return crate::internal_error(),
    };
    Json(domains).into_response()
}

/// `ConfigManagerEntryResourceView.delete`.
async fn remove_entry(State(views): State<Views>, Path(entry_id): Path<String>) -> Response {
    match views.flows.remove_entry(&entry_id).await {
        Some(require_restart) => {
            Json(json!({ "require_restart": require_restart })).into_response()
        }
        None => json_message(StatusCode::NOT_FOUND, "Invalid entry specified"),
    }
}

#[cfg(test)]
mod tests;

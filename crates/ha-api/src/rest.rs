//! Home Assistant's REST API (`/api/...`).

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use ha_core::{
    Attributes, Context, EntityId, EventKind, Hass, ServiceData, ServiceError, SupportsResponse,
};
use serde_json::{Value, json};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};

pub(crate) fn router(hass: Hass) -> Router {
    Router::new()
        .route(
            "/api/",
            get(|| async { Json(json!({ "message": "API running." })) }),
        )
        .route("/api/states", get(list_states))
        .route("/api/states/{entity_id}", get(get_state).post(post_state))
        .route("/api/services", get(list_services))
        .route("/api/services/{domain}/{service}", post(call_service))
        .with_state(hass)
}

#[cfg(test)]
mod post_state_tests;

async fn list_states(State(hass): State<Hass>) -> Json<Value> {
    Json(json!(hass.states().all()))
}

async fn get_state(
    State(hass): State<Hass>,
    Path(entity_id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let id = EntityId::parse(&entity_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    hass.states()
        .get(&id)
        .map(|s| Json(json!(s)))
        .ok_or(StatusCode::NOT_FOUND)
}

/// Home Assistant's `POST /api/states/<entity_id>` (`APIEntityStateView`).
/// The body is read as JSON whatever its `Content-Type`, as HA's
/// `request.text()` then `json_loads` does. Admins only (the auth layer).
async fn post_state(
    State(hass): State<Hass>,
    principal: Option<axum::Extension<ha_auth::Principal>>,
    Path(entity_id): Path<String>,
    body: Bytes,
) -> Response {
    // `request.text()` fails on bytes that aren't UTF-8, outside HA's try.
    let Ok(body) = std::str::from_utf8(&body) else {
        return crate::internal_error();
    };
    let data: Value = if body.is_empty() {
        Value::Null
    } else {
        match serde_json::from_str(body) {
            Ok(v) => v,
            Err(_) => return message(StatusCode::BAD_REQUEST, "Invalid JSON specified."),
        }
    };
    let Value::Object(data) = data else {
        return message(
            StatusCode::BAD_REQUEST,
            "State data should be a JSON object.",
        );
    };
    let new_state = match data.get("state") {
        None | Some(Value::Null) => {
            return message(StatusCode::BAD_REQUEST, "No state specified.");
        }
        Some(v) => ha_core::py::str_json(v),
    };
    // `hass.states.async_set`: the state first, then the id, lower-cased.
    if new_state.chars().count() > 255 {
        return message(StatusCode::BAD_REQUEST, "Invalid state specified.");
    }
    let Ok(id) = EntityId::parse(&entity_id.to_lowercase()) else {
        return message(StatusCode::BAD_REQUEST, "Invalid entity ID specified.");
    };
    // `attributes or {}`, then `ReadOnlyDict(attributes)`: a mapping, or
    // what `dict()` takes from a list; anything else raises (aiohttp's 500).
    let attributes: Attributes = match data.get("attributes") {
        Some(Value::Object(m)) => m.clone(),
        Some(Value::Array(items)) if !items.is_empty() => match dict_from_pairs(items) {
            Some(m) => m,
            None => return crate::internal_error(),
        },
        Some(v) if py_truthy(v) => return crate::internal_error(),
        _ => Attributes::new(),
    };
    let force_update = data.get("force_update").is_some_and(py_truthy);
    let mut context = Context::at(hass.clock().now());
    if let Some(axum::Extension(p)) = principal {
        context = context.with_user(p.user_id.as_str());
    }
    let is_new = hass.states().get(&id).is_none();
    let state = if force_update {
        hass.set_state_forced(id, new_state, attributes, Some(context))
    } else {
        hass.set_state_with_context(id, new_state, attributes, Some(context))
    };
    let status = if is_new {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let mut res = (status, Json(json!(state))).into_response();
    // The path as it was sent, as HA's `f"/api/states/{entity_id}"`.
    if let Ok(location) = HeaderValue::from_bytes(format!("/api/states/{entity_id}").as_bytes()) {
        res.headers_mut().insert(header::LOCATION, location);
    }
    res
}

/// Python's `dict(items)` for a JSON list: each item a `[key, value]` pair
/// or a two-character string, later keys winning. `None` where `dict()`
/// raises, and for keys that aren't strings (HA would keep `1` as a key;
/// hars's attributes are keyed by strings).
fn dict_from_pairs(items: &[Value]) -> Option<Attributes> {
    let mut out = Attributes::new();
    for item in items {
        match item {
            Value::Array(pair) => match pair.as_slice() {
                [Value::String(k), v] => {
                    out.insert(k.clone(), v.clone());
                }
                _ => return None,
            },
            Value::String(s) => {
                let mut chars = s.chars();
                let (Some(k), Some(v), None) = (chars.next(), chars.next(), chars.next()) else {
                    return None;
                };
                out.insert(k.to_string(), Value::String(v.to_string()));
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Python truthiness of a JSON value.
fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

/// Same shape as Home Assistant's `GET /api/services`.
async fn list_services(State(hass): State<Hass>) -> Json<Value> {
    let domains: Vec<Value> = hass
        .services()
        .services()
        .into_iter()
        .map(|(domain, services)| {
            let services: serde_json::Map<String, Value> = services
                .into_iter()
                .map(|(name, supports)| (name.to_string(), service_description(supports)))
                .collect();
            json!({ "domain": domain, "services": services })
        })
        .collect();
    Json(Value::Array(domains))
}

/// A service's description. `services.yaml` isn't loaded yet, so it is
/// `{"fields": {}}` plus `response` metadata, as HA reports for a service
/// without a description.
pub(crate) fn service_description(supports: SupportsResponse) -> Value {
    match supports {
        SupportsResponse::None => json!({ "fields": {} }),
        SupportsResponse::Optional => json!({ "fields": {}, "response": { "optional": true } }),
        SupportsResponse::Only => json!({ "fields": {}, "response": { "optional": false } }),
    }
}

fn message(status: StatusCode, message: impl std::fmt::Display) -> Response {
    (status, Json(json!({ "message": message.to_string() }))).into_response()
}

/// Same shape as Home Assistant's `POST /api/services/<domain>/<service>`:
/// returns the states the call changed (those carrying its context), or with
/// `?return_response` `{"changed_states": [...], "service_response": {...}}`.
async fn call_service(
    State(hass): State<Hass>,
    principal: Option<axum::Extension<ha_auth::Principal>>,
    Path((domain, service)): Path<(String, String)>,
    Query(query): Query<Vec<(String, String)>>,
    body: String,
) -> Response {
    let data: ServiceData = if body.is_empty() {
        ServiceData::new()
    } else {
        match serde_json::from_str(&body) {
            Ok(Value::Object(data)) => data,
            Ok(Value::Null) => ServiceData::new(),
            Ok(_) => return message(StatusCode::BAD_REQUEST, "Data should be a JSON object."),
            Err(_) => return message(StatusCode::BAD_REQUEST, "Data should be valid JSON."),
        }
    };
    let return_response = query.iter().any(|(key, _)| key == "return_response");
    // The caller's user, as HA's `self.context(request)`.
    let mut context = Context::at(hass.clock().now());
    if let Some(axum::Extension(p)) = principal {
        context = context.with_user(p.user_id.as_str());
    }

    // Subscribe before the call so no change it causes is missed.
    let mut rx = hass.bus().subscribe();
    // Like HA, finish the call even if the client disconnects mid-way.
    let mut task = tokio::spawn({
        let hass = hass.clone();
        let context = context.clone();
        async move {
            hass.call_service(
                &domain,
                &service,
                data,
                None,
                return_response,
                Some(context),
            )
            .await
        }
    });

    // Collect changes while the call runs, so a long call can't overflow the
    // bus and evict its own earliest changes before they are read.
    let mut changed_states = Vec::new();
    let mut collect = |event: &ha_core::Event| {
        if let EventKind::StateChanged {
            new_state: Some(state),
            ..
        } = &event.kind
            && event.context == context
        {
            changed_states.push(state.clone());
        }
    };
    let joined = loop {
        tokio::select! {
            joined = &mut task => break joined,
            event = rx.recv() => match event {
                Ok(event) => collect(&event),
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break (&mut task).await,
            },
        }
    };
    // Changes fired just before the call returned.
    loop {
        match rx.try_recv() {
            Ok(event) => collect(&event),
            Err(TryRecvError::Lagged(_)) => {}
            Err(_) => break,
        }
    }
    let result = match joined {
        Ok(result) => result,
        Err(e) => return message(StatusCode::INTERNAL_SERVER_ERROR, e),
    };

    match result {
        Ok(response) if return_response => Json(json!({
            "changed_states": changed_states,
            "service_response": response,
        }))
        .into_response(),
        Ok(_) => Json(json!(changed_states)).into_response(),
        Err(ServiceError::ResponseNotSupported) => message(
            StatusCode::BAD_REQUEST,
            "Service does not support responses. Remove return_response from request.",
        ),
        Err(ServiceError::ResponseRequired) => message(
            StatusCode::BAD_REQUEST,
            "Service call requires responses but caller did not ask for responses. Add ?return_response to query parameters.",
        ),
        Err(e @ (ServiceError::NotFound { .. } | ServiceError::Invalid(_))) => {
            message(StatusCode::BAD_REQUEST, e)
        }
        // HA's view lets these escape: aiohttp answers 500.
        Err(
            e @ (ServiceError::Failed(_)
            | ServiceError::Translated(_)
            | ServiceError::Validation(_)
            | ServiceError::Unknown(_)),
        ) => message(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use ha_core::{EntityTarget, Kind, Range, Schema, ServiceResponse, SupportsResponse};
    use tower::ServiceExt;

    /// A `light.turn_on` that turns on every targeted entity, like a real light platform.
    fn hass_with_services() -> Hass {
        let hass = Hass::new();
        let h = hass.clone();
        let schema =
            Schema::entity_service().optional("brightness", Kind::Int(Range::new(0.0, 255.0)));
        hass.services().register_with_schema(
            "light",
            "turn_on",
            Some(schema),
            SupportsResponse::None,
            move |call| {
                let h = h.clone();
                async move {
                    let EntityTarget::Ids(ids) = call.entity_ids()? else {
                        return Ok(None);
                    };
                    let mut attrs = Attributes::new();
                    if let Some(b) = call.data.get("brightness") {
                        attrs.insert("brightness".into(), b.clone());
                    }
                    for id in ids {
                        h.set_state_with_context(
                            id,
                            "on",
                            attrs.clone(),
                            Some(call.context.clone()),
                        );
                    }
                    Ok(None)
                }
            },
        );
        hass.services().register(
            "weather",
            "get_forecasts",
            SupportsResponse::Only,
            |_| async {
                let mut r = ServiceResponse::new();
                r.insert("weather.home".into(), json!({ "forecast": [] }));
                Ok(Some(r))
            },
        );
        hass
    }

    async fn send(hass: &Hass, method: &str, uri: &str, body: &str) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::from(body.to_owned()))
            .unwrap();
        let res = router(hass.clone()).oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn lists_services_in_ha_shape() {
        let (status, body) = send(&hass_with_services(), "GET", "/api/services", "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!([
                { "domain": "light", "services": { "turn_on": { "fields": {} } } },
                {
                    "domain": "weather",
                    "services": {
                        "get_forecasts": { "fields": {}, "response": { "optional": false } }
                    }
                },
            ])
        );
    }

    #[tokio::test]
    async fn call_returns_changed_states() {
        let hass = hass_with_services();
        let (status, body) = send(
            &hass,
            "POST",
            "/api/services/light/turn_on",
            r#"{"entity_id": "light.kitchen, light.hallway", "brightness": 200}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let changed: Vec<_> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["entity_id"].clone(),
                    s["state"].clone(),
                    s["attributes"].clone(),
                )
            })
            .collect();
        assert_eq!(
            changed,
            vec![
                (
                    json!("light.kitchen"),
                    json!("on"),
                    json!({ "brightness": 200 })
                ),
                (
                    json!("light.hallway"),
                    json!("on"),
                    json!({ "brightness": 200 })
                ),
            ]
        );
        let id = EntityId::parse("light.hallway").unwrap();
        assert_eq!(hass.states().get(&id).unwrap().state, "on");
    }

    #[tokio::test]
    async fn empty_body_is_empty_data() {
        let (status, body) = send(
            &hass_with_services(),
            "POST",
            "/api/services/weather/get_forecasts?return_response",
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["changed_states"], json!([]));
    }

    #[tokio::test]
    async fn bad_requests_are_400() {
        let hass = hass_with_services();
        for (uri, body, msg) in [
            ("/api/services/light/fly", "", None),
            (
                "/api/services/light/turn_on",
                "{not json",
                Some("Data should be valid JSON."),
            ),
            (
                "/api/services/light/turn_on",
                r#"{"entity_id": "kitchen"}"#,
                None,
            ),
            (
                "/api/services/light/turn_on",
                r#"{"entity_id": "light.a", "brightness": 300}"#,
                Some(
                    "invalid service data: value must be at most 255 for dictionary value @ data['brightness']",
                ),
            ),
            (
                "/api/services/light/turn_on",
                "",
                Some(
                    "invalid service data: must contain at least one of entity_id, device_id, area_id, floor_id, label_id.",
                ),
            ),
            (
                "/api/services/light/turn_on?return_response",
                "",
                Some("Service does not support responses. Remove return_response from request."),
            ),
            (
                "/api/services/weather/get_forecasts",
                "",
                Some(
                    "Service call requires responses but caller did not ask for responses. Add ?return_response to query parameters.",
                ),
            ),
        ] {
            let (status, res) = send(&hass, "POST", uri, body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} {body}");
            if let Some(msg) = msg {
                assert_eq!(res["message"], msg);
            }
        }
    }

    #[tokio::test]
    async fn return_response_wraps_the_result() {
        // `%72` is `r`: the query key is matched after URL decoding, like HA.
        for uri in [
            "/api/services/weather/get_forecasts?return_response",
            "/api/services/weather/get_forecasts?return_response=1",
            "/api/services/weather/get_forecasts?%72eturn_response",
        ] {
            let (status, body) =
                send(&hass_with_services(), "POST", uri, r#"{"type": "daily"}"#).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            assert_eq!(
                body,
                json!({
                    "changed_states": [],
                    "service_response": { "weather.home": { "forecast": [] } },
                }),
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn changed_states_survive_more_changes_than_the_bus_holds() {
        const N: usize = 5_000; // > the 4,096-event bus capacity
        let hass = Hass::new();
        let h = hass.clone();
        hass.services()
            .register("demo", "flood", SupportsResponse::None, move |call| {
                let h = h.clone();
                async move {
                    for i in 0..N {
                        let id = EntityId::parse(&format!("sensor.s{i}")).unwrap();
                        let ctx = Some(call.context.clone());
                        h.set_state_with_context(id, "on", Attributes::new(), ctx);
                        tokio::task::yield_now().await; // as a real I/O-bound handler would
                    }
                    Ok(None)
                }
            });
        let (status, body) = send(&hass, "POST", "/api/services/demo/flood", "").await;
        assert_eq!(status, StatusCode::OK);
        let changed = body.as_array().unwrap();
        assert_eq!(changed.len(), N);
        assert_eq!(changed[0]["entity_id"], "sensor.s0", "earliest change kept");
    }

    #[tokio::test]
    async fn changed_states_exclude_other_sources() {
        let hass = Hass::new();
        let h = hass.clone();
        hass.services()
            .register("demo", "mixed", SupportsResponse::None, move |call| {
                let h = h.clone();
                async move {
                    let mine = EntityId::parse("light.mine").unwrap();
                    let other = EntityId::parse("sensor.other").unwrap();
                    // A change from elsewhere (an integration push) lands mid-call.
                    h.set_state(other, "42", Attributes::new());
                    h.set_state_with_context(mine, "on", Attributes::new(), Some(call.context));
                    Ok(None)
                }
            });
        let (status, body) = send(&hass, "POST", "/api/services/demo/mixed", "").await;
        assert_eq!(status, StatusCode::OK);
        let changed = body.as_array().unwrap();
        assert_eq!(changed.len(), 1, "{body}");
        assert_eq!(changed[0]["entity_id"], "light.mine");
        assert_eq!(changed[0]["context"]["user_id"], Value::Null);
    }
}

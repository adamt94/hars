//! `/api/webhook/{webhook_id}` for the companion apps: HA's `webhook`
//! component (routing, methods) and `mobile_app/webhook.py` (the envelope
//! pipeline and the commands).
//!
//! The webhook id is the credential, so the route is outside
//! `require_auth`. Failures are almost always `200`: HA's webhook layer
//! turns any exception into an empty `200`.

use crate::Shared;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ha_core::{Context, EntityId, Event, EventKind, Origin};
use ha_integrations::mobile_app::{Opened, Registration, Webhook};
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// HA's `client_max_size` is 16 MiB; the app's largest request (a full
/// sensor update) is a few KiB.
const BODY_LIMIT: usize = 1 << 20;

/// `MANIFEST_JSON["theme_color"]`.
const THEME_COLOR: &str = "#2980b9";

pub(crate) fn routes(shared: Shared) -> Router {
    Router::new()
        .route("/api/webhook/{webhook_id}", axum::routing::any(webhook))
        .with_state(shared)
}

/// What a command answers.
#[derive(Debug)]
pub(crate) enum Reply {
    /// No `Content-Type`, no body.
    Bare(StatusCode),
    /// `application/json`, with `; charset=utf-8` when `charset` (aiohttp's
    /// `Response(text=…)` vs `Response(body=…)`).
    Json {
        status: StatusCode,
        charset: bool,
        body: String,
    },
}

impl IntoResponse for Reply {
    fn into_response(self) -> Response {
        match self {
            Reply::Bare(status) => status.into_response(),
            Reply::Json {
                status,
                charset,
                body,
            } => {
                let ct = if charset {
                    "application/json; charset=utf-8"
                } else {
                    "application/json"
                };
                (
                    status,
                    [(header::CONTENT_TYPE, HeaderValue::from_static(ct))],
                    body,
                )
                    .into_response()
            }
        }
    }
}

/// `empty_okay_response`: `{}`, never encrypted.
fn empty_okay(status: StatusCode) -> Reply {
    Reply::Json {
        status,
        charset: true,
        body: "{}".into(),
    }
}

/// `error_response`: aiohttp's `json_response`, which is `json.dumps` with
/// its default separators.
fn error_response(code: &str, message: &str) -> Reply {
    Reply::Json {
        status: StatusCode::BAD_REQUEST,
        charset: true,
        body: py_dumps(&json!({"success": false, "error": {"code": code, "message": message}})),
    }
}

/// `webhook_response`: compact JSON, sealed when the registration encrypts.
fn webhook_response(reg: &Registration, status: StatusCode, data: &Value) -> Reply {
    Reply::Json {
        status,
        charset: false,
        body: reg.response_body(data.to_string()),
    }
}

/// Python's `json.dumps` with default separators (`", "`, `": "`), ASCII
/// escaped.
fn py_dumps(v: &Value) -> String {
    fn go(out: &mut String, v: &Value) {
        match v {
            Value::Array(a) => {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    go(out, x);
                }
                out.push(']');
            }
            Value::Object(m) => {
                out.push('{');
                for (i, (k, x)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    ascii(out, k);
                    out.push_str(": ");
                    go(out, x);
                }
                out.push('}');
            }
            Value::String(s) => ascii(out, s),
            v => out.push_str(&v.to_string()),
        }
    }
    fn ascii(out: &mut String, s: &str) {
        let quoted = Value::String(s.to_owned()).to_string();
        for c in quoted.chars() {
            if c.is_ascii() {
                out.push(c);
            } else {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    let mut out = String::new();
    go(&mut out, v);
    out
}

/// HA's `async_handle_webhook`: aiohttp's router answers methods the route
/// doesn't have; an unknown id is an empty `200`; `HEAD` is `200` and a
/// method the webhook doesn't allow (`GET`) is `405`. The handler runs in
/// its own task, so a client that hangs up doesn't cancel it (HA's
/// `asyncio.shield`).
async fn webhook(
    State(shared): State<Shared>,
    Path(webhook_id): Path<String>,
    method: Method,
    body: Body,
) -> Response {
    if !matches!(
        method,
        Method::GET | Method::HEAD | Method::POST | Method::PUT
    ) {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (header::ALLOW, "GET,HEAD,OPTIONS,POST,PUT"),
            ],
            "405: Method Not Allowed",
        )
            .into_response();
    }
    let hook = match &shared.config.mobile_app {
        Some(app) => app.webhook(&webhook_id),
        None => Webhook::Unknown,
    };
    if matches!(hook, Webhook::Unknown) {
        tracing::warn!(
            "received message for unregistered webhook {}",
            prefix(&webhook_id)
        );
        return Reply::Bare(StatusCode::OK).into_response();
    }
    match method {
        Method::HEAD => return Reply::Bare(StatusCode::OK).into_response(),
        Method::GET => return Reply::Bare(StatusCode::METHOD_NOT_ALLOWED).into_response(),
        _ => {}
    }
    let Webhook::Registered(reg) = hook else {
        return Reply::Bare(StatusCode::GONE).into_response();
    };
    // Reading too much raises inside the handler: HA's catch-all.
    let Ok(body) = axum::body::to_bytes(body, BODY_LIMIT).await else {
        return Reply::Bare(StatusCode::OK).into_response();
    };
    let task = tokio::spawn(async move { handle(&shared, &reg, &body).await });
    task.await
        .unwrap_or(Reply::Bare(StatusCode::OK))
        .into_response()
}

/// Enough of a webhook id to tell them apart in logs; it is a credential.
fn prefix(id: &str) -> &str {
    id.get(..6).unwrap_or(id)
}

/// `handle_webhook`: the envelope, then the command.
pub(crate) async fn handle(shared: &Shared, reg: &Arc<Registration>, body: &[u8]) -> Reply {
    let Ok(req) = serde_json::from_slice::<Value>(body) else {
        return empty_okay(StatusCode::BAD_REQUEST);
    };
    // `ATTR_WEBHOOK_ENCRYPTED not in req_data`: Python's `in`, which
    // raises on numbers, booleans and None.
    let has_encrypted = match &req {
        Value::Object(m) => m.contains_key("encrypted"),
        Value::Array(a) => a.iter().any(|v| v == "encrypted"),
        Value::String(s) => s.contains("encrypted"),
        _ => return Reply::Bare(StatusCode::OK),
    };
    if !has_encrypted && reg.supports_encryption() {
        return error_response("encryption_required", "Encryption required");
    }
    let Some(envelope) = envelope(&req) else {
        return empty_okay(StatusCode::OK);
    };
    let payload = match envelope.body {
        Sent::Plain(data) => data,
        Sent::Encrypted(None) => return Reply::Bare(StatusCode::OK),
        Sent::Encrypted(Some(data)) => match reg.open(&data) {
            // A sealed `null` is `webhook_payload is None` (the flag is still set).
            (Opened::Json(Value::Null), _) | (Opened::Failed, _) => {
                return empty_okay(StatusCode::OK);
            }
            (Opened::Json(v), _) => v,
        },
    };
    command(shared, reg, &envelope.ty, payload).await
}

enum Sent {
    Plain(Value),
    /// `encrypted_data`, if present.
    Encrypted(Option<String>),
}

struct Envelope {
    ty: String,
    body: Sent,
}

/// `WEBHOOK_PAYLOAD_SCHEMA`: `{type, data?}` or `{type, encrypted: True,
/// encrypted_data?}`, nothing else.
fn envelope(req: &Value) -> Option<Envelope> {
    let m = req.as_object()?;
    let ty = crate::cv::string(m.get("type")?).ok()?;
    let plain = m.keys().all(|k| k == "type" || k == "data");
    if plain {
        let data = match m.get("data") {
            None => Value::Object(Map::new()),
            Some(d @ (Value::Object(_) | Value::Array(_))) => d.clone(),
            Some(_) => return None,
        };
        return Some(Envelope {
            ty,
            body: Sent::Plain(data),
        });
    }
    if !m
        .keys()
        .all(|k| matches!(k.as_str(), "type" | "encrypted" | "encrypted_data"))
    {
        return None;
    }
    // `vol.Required("encrypted"): True` compares with `==`: `1` passes.
    let is_true = match m.get("encrypted")? {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() == Some(1.0),
        _ => false,
    };
    if !is_true {
        return None;
    }
    let data = match m.get("encrypted_data") {
        None => None,
        Some(v) => Some(crate::cv::string(v).ok()?),
    };
    Some(Envelope {
        ty,
        body: Sent::Encrypted(data),
    })
}

/// The registration's context: its owner.
fn context(shared: &Shared, reg: &Registration) -> Context {
    let ctx = Context::at(shared.hass.clock().now());
    match reg.user_id() {
        Some(user) => ctx.with_user(user),
        None => ctx,
    }
}

/// A command's `vol.Schema(dict)`: an object with only `allowed` keys.
fn object<'a>(payload: &'a Value, allowed: &[&str]) -> Option<&'a Map<String, Value>> {
    let m = payload.as_object()?;
    m.keys().all(|k| allowed.contains(&k.as_str())).then_some(m)
}

/// A required `cv.string` field.
fn string(m: &Map<String, Value>, key: &str) -> Option<String> {
    crate::cv::string(m.get(key)?).ok()
}

/// An optional `cv.string` field: `Some(None)` when absent, `None` when bad.
fn opt_string(m: &Map<String, Value>, key: &str) -> Option<Option<String>> {
    match m.get(key) {
        None => Some(None),
        Some(v) => crate::cv::string(v).ok().map(Some),
    }
}

/// An optional `dict` field, `{}` by default.
fn dict(m: &Map<String, Value>, key: &str) -> Option<Map<String, Value>> {
    match m.get(key) {
        None => Some(Map::new()),
        Some(Value::Object(d)) => Some(d.clone()),
        Some(_) => None,
    }
}

async fn command(shared: &Shared, reg: &Arc<Registration>, ty: &str, payload: Value) -> Reply {
    let ok = || empty_okay(StatusCode::OK);
    match ty {
        "call_service" => {
            let Some((domain, service, data)) =
                object(&payload, &["domain", "service", "service_data"]).and_then(|m| {
                    Some((
                        string(m, "domain")?,
                        string(m, "service")?,
                        dict(m, "service_data")?,
                    ))
                })
            else {
                return ok();
            };
            let ctx = context(shared, reg);
            match shared
                .hass
                .call_service(&domain, &service, data, None, false, Some(ctx))
                .await
            {
                Ok(_) => ok(),
                // `HTTPBadRequest`, caught by the webhook layer.
                Err(e) => {
                    tracing::error!(
                        "Error when calling service during mobile_app webhook (device name: {}): {e}",
                        reg.device_name().unwrap_or_default()
                    );
                    Reply::Bare(StatusCode::OK)
                }
            }
        }
        "fire_event" => {
            let Some((event_type, data)) = object(&payload, &["event_type", "event_data"])
                .and_then(|m| Some((string(m, "event_type")?, dict(m, "event_data")?)))
            else {
                return ok();
            };
            let ctx = context(shared, reg);
            shared.hass.bus().fire(
                Event::new(
                    EventKind::Custom {
                        name: event_type.into(),
                        data: Value::Object(data),
                    },
                    shared.hass.clock().now(),
                    ctx,
                )
                .with_origin(Origin::Remote),
            );
            ok()
        }
        "scan_tag" => {
            let Some(tag_id) = object(&payload, &["tag_id"]).and_then(|m| string(m, "tag_id"))
            else {
                return ok();
            };
            // The `tag` integration's entity and store are not kept.
            let ctx = context(shared, reg);
            let device_id = device_registry_id(shared, reg);
            shared.hass.bus().fire(Event::new(
                EventKind::Custom {
                    name: "tag_scanned".into(),
                    data: json!({"tag_id": tag_id, "name": null, "device_id": device_id}),
                },
                shared.hass.clock().now(),
                ctx,
            ));
            ok()
        }
        "get_zones" => {
            let mut zones: Vec<_> = shared
                .hass
                .states()
                .all()
                .into_iter()
                .filter(|s| s.entity_id.domain() == "zone")
                .collect();
            zones.sort_by(|a, b| a.entity_id.as_str().cmp(b.entity_id.as_str()));
            webhook_response(reg, StatusCode::OK, &json!(zones))
        }
        "get_config" => webhook_response(reg, StatusCode::OK, &get_config(shared, reg)),
        "update_sensor_states" => {
            let Some(sensors) = shared.config.mobile_app.as_ref().and_then(|a| a.sensors()) else {
                return ok();
            };
            match sensors.update_states(reg, &payload) {
                Some(resp) => webhook_response(reg, StatusCode::OK, &Value::Object(resp)),
                None => ok(),
            }
        }
        "register_sensor" => {
            let Some(sensors) = shared.config.mobile_app.as_ref().and_then(|a| a.sensors()) else {
                return ok();
            };
            let device = device_registry_id(shared, reg);
            if sensors.register(reg, device.as_deref(), &payload) {
                webhook_response(reg, StatusCode::CREATED, &json!({"success": true}))
            } else {
                ok()
            }
        }
        "update_registration" => update_registration(reg, &payload),
        "enable_encryption" => {
            let secret: String = rand::random::<[u8; 32]>()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            if !reg.enable_encryption(secret.clone()) {
                return error_response("encryption_already_enabled", "Encryption already enabled");
            }
            Reply::Json {
                status: StatusCode::OK,
                charset: true,
                body: py_dumps(&json!({"secret": secret})),
            }
        }
        "render_template" => {
            // `{str: {template: cv.string, variables?: dict}}`; Jinja isn't
            // implemented, so every template answers an error.
            let Some(m) = payload.as_object() else {
                return ok();
            };
            let mut out = Map::new();
            for (key, item) in m {
                let valid = object(item, &["template", "variables"]).is_some_and(|t| {
                    string(t, "template").is_some() && dict(t, "variables").is_some()
                });
                if !valid {
                    return ok();
                }
                out.insert(
                    key.clone(),
                    json!({"error": "TemplateError: templates are not supported by hars yet"}),
                );
            }
            webhook_response(reg, StatusCode::OK, &Value::Object(out))
        }
        "stream_camera" => {
            let Some(id) =
                object(&payload, &["camera_entity_id"]).and_then(|m| string(m, "camera_entity_id"))
            else {
                return ok();
            };
            let state = EntityId::parse(&id.to_lowercase())
                .ok()
                .and_then(|e| shared.hass.states().get(&e));
            match state {
                None => webhook_response(reg, StatusCode::BAD_REQUEST, &json!({"success": false})),
                // `attributes[ATTR_SUPPORTED_FEATURES]` raises `KeyError`.
                Some(s) if !s.attributes.contains_key("supported_features") => {
                    Reply::Bare(StatusCode::OK)
                }
                Some(s) => webhook_response(
                    reg,
                    StatusCode::OK,
                    &json!({
                        "mjpeg_path": format!("/api/camera_proxy_stream/{}", s.entity_id),
                        "hls_path": null,
                    }),
                ),
            }
        }
        "conversation_process" => {
            let valid =
                object(&payload, &["text", "language", "conversation_id"]).is_some_and(|m| {
                    string(m, "text").is_some()
                        && opt_string(m, "language").is_some()
                        && opt_string(m, "conversation_id").is_some()
                });
            // `conversation` isn't loaded: the handler raises.
            if valid {
                Reply::Bare(StatusCode::OK)
            } else {
                ok()
            }
        }
        _ => {
            tracing::error!(
                "Received invalid webhook from {} of type: {ty}",
                reg.device_name().unwrap_or_default()
            );
            ok()
        }
    }
}

/// The phone's device registry id: the device whose identifiers include
/// `["mobile_app", <device_id>]`.
fn device_registry_id(shared: &Shared, reg: &Registration) -> Option<String> {
    let device_id = reg.device_id()?;
    let want = json!(["mobile_app", device_id]);
    shared
        .config
        .registries()
        .devices
        .iter()
        .find(|d| d.identifiers.contains(&want))
        .map(|d| d.id.clone())
}

/// `webhook_get_config`.
fn get_config(shared: &Shared, reg: &Registration) -> Value {
    let config = crate::config_json(shared, false);
    let webhook_id = &*reg.webhook_id;
    let mut entities = Map::new();
    for e in &shared.config.registries().entities {
        if e.config_entry_id.as_deref() != Some(reg.entry_id.as_str()) {
            continue;
        }
        let unique_id = match &e.unique_id {
            Some(Value::String(u)) => u.as_str(),
            _ => continue,
        };
        let domain = e.entity_id.split('.').next().unwrap_or_default();
        let key = if matches!(domain, "sensor" | "binary_sensor") {
            unique_id.get(webhook_id.len() + 1..).unwrap_or_default()
        } else {
            unique_id
        };
        entities.insert(key.to_owned(), json!({"disabled": e.disabled_by.is_some()}));
    }
    json!({
        "latitude": config["latitude"],
        "longitude": config["longitude"],
        "elevation": config["elevation"],
        "hass_device_id": device_registry_id(shared, reg),
        "unit_system": config["unit_system"],
        "location_name": config["location_name"],
        "time_zone": config["time_zone"],
        "components": config["components"],
        "version": config["version"],
        "theme_color": THEME_COLOR,
        "entities": entities,
    })
}

/// `webhook_update_registration`. The merged data is kept for this run.
fn update_registration(reg: &Registration, payload: &Value) -> Reply {
    let ok = || empty_okay(StatusCode::OK);
    let Some(m) = object(
        payload,
        &[
            "app_data",
            "app_version",
            "device_name",
            "manufacturer",
            "model",
            "os_version",
        ],
    ) else {
        return ok();
    };
    let mut update = Map::new();
    for key in ["app_version", "device_name", "manufacturer", "model"] {
        let Some(v) = string(m, key) else {
            return ok();
        };
        update.insert(key.into(), Value::String(v));
    }
    match opt_string(m, "os_version") {
        None => return ok(),
        Some(Some(v)) => {
            update.insert("os_version".into(), Value::String(v));
        }
        Some(None) => {}
    }
    if let Some(app_data) = m.get("app_data") {
        let Some(app_data) = schema_app_data(app_data) else {
            return ok();
        };
        update.insert("app_data".into(), Value::Object(app_data));
    }
    let new = reg.merge(update);
    // `safe_registration`: a missing key raises.
    let mut safe = Map::new();
    for key in [
        "app_data",
        "app_id",
        "app_name",
        "app_version",
        "device_name",
        "manufacturer",
        "model",
        "os_version",
        "supports_encryption",
    ] {
        let Some(v) = new.get(key) else {
            return Reply::Bare(StatusCode::OK);
        };
        safe.insert(key.into(), v.clone());
    }
    webhook_response(reg, StatusCode::OK, &Value::Object(safe))
}

/// `SCHEMA_APP_DATA`: `push_token` (`cv.string`) and `push_url`
/// (`cv.url`) together or not at all, `push_websocket_channel`
/// (`cv.boolean`), anything else kept as is.
fn schema_app_data(v: &Value) -> Option<Map<String, Value>> {
    let mut m = v.as_object()?.clone();
    if m.contains_key("push_token") != m.contains_key("push_url") {
        return None;
    }
    if let Some(t) = m.get("push_token") {
        let t = crate::cv::string(t).ok()?;
        m.insert("push_token".into(), Value::String(t));
    }
    if let Some(u) = m.get("push_url") {
        if !is_url(u.as_str()?) {
            return None;
        }
    }
    if let Some(b) = m.get("push_websocket_channel") {
        let b = crate::cv::boolean(b)?;
        m.insert("push_websocket_channel".into(), Value::Bool(b));
    }
    Some(m)
}

/// `cv.url`: an `http`/`https` URL with a host.
fn is_url(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    let Some(rest) = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
    else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let port_ok = match host.rsplit_once(':') {
        Some((_, port)) if !host.ends_with(']') => port.is_empty() || port.parse::<u16>().is_ok(),
        _ => true,
    };
    !host.is_empty() && port_ok
}

#[cfg(test)]
mod tests;

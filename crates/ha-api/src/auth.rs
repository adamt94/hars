//! HA's auth HTTP views (`components/auth/__init__.py`, `login_flow.py`)
//! and the `Bearer` check on `/api/*` (`components/http/auth.py`).

// Request-parsing helpers return the finished error `Response`: it is only
// built for a rejected request, so its size doesn't matter.
#![allow(clippy::result_large_err)]

use axum::{
    Json, Router,
    body::Bytes,
    extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts, MatchedPath, Path, Request, State},
    http::{HeaderMap, Method, StatusCode, header, request::Parts},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
};
use ha_auth::{Auth, LoginError, Permission, Principal, TokenError, Tokens, indieauth};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

pub(crate) fn routes(auth: Auth) -> Router {
    Router::new()
        .route("/auth/providers", get(providers))
        .route(
            "/auth/login_flow",
            get(|| async { StatusCode::METHOD_NOT_ALLOWED }).post(login_flow_start),
        )
        .route(
            "/auth/login_flow/{flow_id}",
            get(|| async { json_message(StatusCode::NOT_FOUND, "Invalid flow specified") })
                .post(login_flow_step)
                .delete(login_flow_abort),
        )
        // HA's `cors_allowed` views: any origin may call them.
        .route(
            "/auth/token",
            crate::cors::allow_all(axum::routing::post(token)),
        )
        .route(
            "/auth/revoke",
            crate::cors::allow_all(axum::routing::post(revoke)),
        )
        // These bodies are a few hundred bytes; don't buffer megabytes for
        // anonymous callers.
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(auth)
}

pub(crate) const MAX_BODY: usize = 16 * 1024;
/// Client ids and redirect URIs are kept while a flow or code is pending, so
/// their length is bounded (HA has no limit; real ones are under 100 bytes).
const MAX_URL: usize = 2048;

/// The client's address, if the server recorded it. IPv4-mapped IPv6
/// addresses from dual-stack sockets are unwrapped, as `ip_address()` sees
/// them behind aiohttp.
pub(crate) struct RemoteIp(pub Option<IpAddr>);

impl<S: Send + Sync> FromRequestParts<S> for RemoteIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(remote_ip(&parts.extensions)))
    }
}

fn remote_ip(ext: &axum::http::Extensions) -> Option<IpAddr> {
    ext.get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(a)| match a.ip() {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
            ip => ip,
        })
}

/// `HomeAssistantView.json_message`.
pub(crate) fn json_message(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "message": message }))).into_response()
}

/// `@log_invalid_auth` / `process_wrong_login`: a warning per failed attempt.
/// HA's IP bans are off by default, so `hars` doesn't ban.
pub(crate) fn log_invalid(ip: Option<IpAddr>, what: &str) {
    match ip {
        Some(ip) => {
            tracing::warn!("login attempt or request with invalid authentication from {ip}: {what}")
        }
        None => tracing::warn!("login attempt or request with invalid authentication: {what}"),
    }
}

/// Write changed auth state before answering, so an issued token survives
/// a crash. Off the async runtime: it's a file write and an fsync.
pub(crate) async fn persist(auth: &Auth) {
    if !auth.needs_save() {
        return;
    }
    let auth = auth.clone();
    match tokio::task::spawn_blocking(move || auth.save()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::error!("saving auth state: {e}"),
        Err(e) => tracing::error!("saving auth state: {e}"),
    }
}

// ----- Bearer -----

/// `/api/*` requires `Authorization: Bearer <access token>`. The scheme is
/// case-sensitive, as in HA. On success the [`Principal`] is added to the
/// request's extensions.
pub(crate) async fn require_auth(
    State(auth): State<Auth>,
    mut req: Request,
    next: Next,
) -> Response {
    let ip = remote_ip(req.extensions());
    let principal = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| *scheme == "Bearer")
        .and_then(|(_, token)| auth.validate_access_token(token, ip));
    let needed = required_permission(
        req.method(),
        req.extensions()
            .get::<MatchedPath>()
            .map(MatchedPath::as_str),
    );
    match principal {
        // HA raises `Unauthorized` for a missing permission too: also a 401.
        Some(p) if p.permission >= needed => {
            req.extensions_mut().insert::<Principal>(p);
            next.run(req).await
        }
        _ => unauthorized(),
    }
}

/// What each REST view checks in HA's `components/api`: writing a state is
/// admin-only, calling a service needs entity control (`system-read-only`
/// users have none), and reads need entity read access, which every built-in
/// group has. Admin-only *services* are checked when such services exist.
fn required_permission(method: &Method, path: Option<&str>) -> Permission {
    match (method, path) {
        (&Method::POST, Some("/api/states/{entity_id}")) => Permission::Admin,
        (&Method::POST, Some("/api/services/{domain}/{service}")) => Permission::Control,
        // `require_admin` on every method of the config views.
        (_, Some("/api/config/automation/config/{id}")) => Permission::Admin,
        _ => Permission::Read,
    }
}

/// aiohttp's `HTTPUnauthorized`.
pub(crate) fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "401: Unauthorized",
    )
        .into_response()
}

// ----- /auth/providers -----

async fn providers(State(auth): State<Auth>, RemoteIp(ip): RemoteIp) -> Response {
    if !auth.is_user_onboarded() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"message": "Onboarding not finished", "code": "onboarding_required"})),
        )
            .into_response();
    }
    let Some(ip) = ip else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"message": "Invalid remote IP", "code": "invalid_remote_ip"})),
        )
            .into_response();
    };
    Json(json!({
        "providers": [{"name": ha_auth::PROVIDER_NAME, "id": null, "type": ha_auth::PROVIDER_TYPE}],
        "preselect_remember_me": indieauth::is_local(ip),
    }))
    .into_response()
}

// ----- /auth/login_flow -----

/// `RequestDataValidator`: the body is JSON whatever the Content-Type (the
/// frontend's `fetch` sends none).
pub(crate) fn json_body(body: &[u8]) -> Result<Map<String, Value>, Response> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(format_error("expected a dictionary")),
        Err(_) => Err(json_message(StatusCode::BAD_REQUEST, "Invalid JSON.")),
    }
}

pub(crate) fn format_error(detail: &str) -> Response {
    json_message(
        StatusCode::BAD_REQUEST,
        &format!("Message format incorrect: {detail}"),
    )
}

fn required_str<'a>(data: &'a Map<String, Value>, key: &str) -> Result<&'a str, Response> {
    match data.get(key) {
        Some(Value::String(s)) if s.len() > MAX_URL => Err(format_error(&format!(
            "length of value must be at most {MAX_URL} for dictionary value @ data['{key}']"
        ))),
        Some(Value::String(s)) => Ok(s),
        Some(_) => Err(format_error(&format!(
            "expected str for dictionary value @ data['{key}']"
        ))),
        None => Err(format_error(&format!(
            "required key not provided @ data['{key}']"
        ))),
    }
}

/// The username/password form of HA's `homeassistant` provider.
fn login_form(flow_id: &str, errors: Value) -> Response {
    Json(json!({
        "type": "form",
        "flow_id": flow_id,
        "handler": [ha_auth::PROVIDER_TYPE, null],
        "data_schema": [
            {"type": "string", "name": "username", "required": true},
            {"type": "string", "name": "password", "required": true},
        ],
        "errors": errors,
        "description_placeholders": null,
        "last_step": null,
        "preview": null,
        "step_id": "init",
    }))
    .into_response()
}

async fn login_flow_start(
    State(auth): State<Auth>,
    RemoteIp(ip): RemoteIp,
    body: Bytes,
) -> Response {
    let res = (|| {
        let data = json_body(&body)?;
        let client_id = required_str(&data, "client_id")?;
        let redirect_uri = required_str(&data, "redirect_uri")?;
        let handler = match data.get("handler") {
            Some(Value::Array(h))
                if h.len() == 2 && h.iter().all(|v| v.is_string() || v.is_null()) =>
            {
                h
            }
            Some(_) => {
                return Err(format_error(
                    "invalid value for dictionary value @ data['handler']",
                ));
            }
            None => return Err(format_error("required key not provided @ data['handler']")),
        };
        if data.get("type").is_some_and(|t| !t.is_string()) {
            return Err(format_error(
                "expected str for dictionary value @ data['type']",
            ));
        }
        if !indieauth::verify_client_id(client_id) {
            return Err(json_message(StatusCode::BAD_REQUEST, "Invalid client id"));
        }
        if handler[0] != ha_auth::PROVIDER_TYPE || !handler[1].is_null() {
            return Err(json_message(
                StatusCode::NOT_FOUND,
                "Invalid handler specified",
            ));
        }
        let flow_id = auth.start_login_flow(redirect_uri, ip);
        Ok(login_form(&flow_id, json!({})))
    })();
    res.unwrap_or_else(|err| {
        log_invalid(ip, "login flow");
        err
    })
}

async fn login_flow_step(
    State(auth): State<Auth>,
    RemoteIp(ip): RemoteIp,
    Path(flow_id): Path<String>,
    body: Bytes,
) -> Response {
    let mut data = match json_body(&body) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let client_id = match data.remove("client_id") {
        Some(Value::String(s)) if s.len() > MAX_URL => {
            return format_error(&format!(
                "length of value must be at most {MAX_URL} for dictionary value @ data['client_id']"
            ));
        }
        Some(Value::String(s)) => s,
        Some(_) => return format_error("expected str for dictionary value @ data['client_id']"),
        None => return format_error("required key not provided @ data['client_id']"),
    };
    if !indieauth::verify_client_id(&client_id) {
        log_invalid(ip, "login flow");
        return json_message(StatusCode::BAD_REQUEST, "Invalid client id");
    }
    match auth.login_flow_ip(&flow_id) {
        None => return json_message(StatusCode::NOT_FOUND, "Invalid flow specified"),
        Some(flow_ip) if flow_ip != ip => {
            return json_message(StatusCode::BAD_REQUEST, "IP address changed");
        }
        Some(_) => {}
    }
    // The step's schema rejects extra and missing keys (voluptuous PREVENT_EXTRA).
    let (username, password) = match (data.len(), data.get("username"), data.get("password")) {
        (2, Some(Value::String(u)), Some(Value::String(p))) => (u.clone(), p.clone()),
        _ => return json_message(StatusCode::BAD_REQUEST, "User input malformed"),
    };

    let checker = auth.clone();
    let checked = tokio::task::spawn_blocking(move || checker.check_password(&username, &password))
        .await
        .ok()
        .flatten();
    let Some(username) = checked else {
        log_invalid(ip, "invalid username or password");
        return login_form(&flow_id, json!({"base": "invalid_auth"}));
    };
    match auth.finish_login_flow(&flow_id, &client_id, &username, ip) {
        Ok(code) => Json(json!({
            "type": "create_entry",
            "flow_id": flow_id,
            "handler": [ha_auth::PROVIDER_TYPE, null],
            "description": null,
            "description_placeholders": null,
            "result": code,
        }))
        .into_response(),
        Err(LoginError::UnknownFlow) => {
            json_message(StatusCode::NOT_FOUND, "Invalid flow specified")
        }
        Err(LoginError::InvalidRedirectUri) => {
            json_message(StatusCode::FORBIDDEN, "Invalid redirect URI")
        }
        Err(LoginError::Blocked(reason)) => {
            json_message(StatusCode::FORBIDDEN, &format!("Login blocked: {reason}"))
        }
        // The login was taken back or made again while the password was
        // checked: as a wrong password, on the same flow.
        Err(LoginError::InvalidAuth) => {
            log_invalid(ip, "invalid username or password");
            login_form(&flow_id, json!({"base": "invalid_auth"}))
        }
    }
}

async fn login_flow_abort(State(auth): State<Auth>, Path(flow_id): Path<String>) -> Response {
    if auth.abort_login_flow(&flow_id) {
        json_message(StatusCode::OK, "Flow aborted")
    } else {
        json_message(StatusCode::NOT_FOUND, "Invalid flow specified")
    }
}

// ----- /auth/token and /auth/revoke -----

/// aiohttp's `request.post()`: urlencoded or multipart form fields. The
/// frontend sends a `FormData`, i.e. multipart.
fn form_fields(headers: &HeaderMap, body: &[u8]) -> HashMap<String, String> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let mime = content_type.split(';').next().unwrap_or("").trim();
    if mime.eq_ignore_ascii_case("multipart/form-data") {
        multipart_fields(content_type, body).unwrap_or_default()
    } else if mime.eq_ignore_ascii_case("application/x-www-form-urlencoded") {
        serde_urlencoded::from_bytes(body).unwrap_or_default()
    } else {
        HashMap::new()
    }
}

/// The text fields of a `multipart/form-data` body. File parts are ignored.
fn multipart_fields(content_type: &str, body: &[u8]) -> Option<HashMap<String, String>> {
    let boundary = content_type.split(';').skip(1).find_map(|p| {
        let (k, v) = p.trim().split_once('=')?;
        k.eq_ignore_ascii_case("boundary")
            .then(|| v.trim_matches('"').to_owned())
    })?;
    let delim = format!("--{boundary}");
    let mut fields = HashMap::new();
    let mut rest = body;
    // Skip the preamble up to the first delimiter.
    let first = find(rest, delim.as_bytes())?;
    rest = &rest[first + delim.len()..];
    loop {
        if rest.starts_with(b"--") {
            return Some(fields); // closing delimiter
        }
        rest = rest.strip_prefix(b"\r\n")?;
        let end = find(rest, format!("\r\n{delim}").as_bytes())?;
        let part = &rest[..end];
        rest = &rest[end + 2 + delim.len()..];

        let split = find(part, b"\r\n\r\n")?;
        let (head, value) = (
            std::str::from_utf8(&part[..split]).ok()?,
            &part[split + 4..],
        );
        let disposition = head.split("\r\n").find_map(|line| {
            let (name, v) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-disposition")
                .then_some(v)
        })?;
        let mut name = None;
        let mut is_file = false;
        for param in disposition.split(';').skip(1) {
            match param.trim().split_once('=') {
                Some(("name", v)) => name = Some(v.trim_matches('"').to_owned()),
                Some(("filename", _)) => is_file = true,
                _ => {}
            }
        }
        if let (Some(name), false) = (name, is_file) {
            fields.insert(name, String::from_utf8(value.to_vec()).ok()?);
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn oauth_error(status: StatusCode, error: &str, description: Option<&str>) -> Response {
    let mut body = json!({ "error": error });
    if let Some(d) = description {
        body["error_description"] = json!(d);
    }
    (status, Json(body)).into_response()
}

async fn token(
    State(auth): State<Auth>,
    RemoteIp(ip): RemoteIp,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let form = form_fields(&headers, &body);
    let field = |k: &str| form.get(k).map(String::as_str);

    // Deprecated form of `/auth/revoke`.
    if field("action") == Some("revoke") {
        if let Some(t) = field("token") {
            auth.revoke(t);
            persist(&auth).await;
        }
        return StatusCode::OK.into_response();
    }
    let result = match field("grant_type") {
        Some("authorization_code") => auth.exchange_code(field("client_id"), field("code"), ip),
        Some("refresh_token") => auth.refresh(field("client_id"), field("refresh_token"), ip),
        _ => {
            log_invalid(ip, "unsupported grant type");
            return oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", None);
        }
    };
    persist(&auth).await;
    match result {
        Ok(tokens) => token_response(tokens),
        Err(e) => {
            log_invalid(ip, "token request");
            match e {
                TokenError::InvalidClient => oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    Some("Invalid client id"),
                ),
                TokenError::InvalidCode => oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    Some("Invalid code"),
                ),
                TokenError::InvalidRequest => {
                    oauth_error(StatusCode::BAD_REQUEST, "invalid_request", None)
                }
                TokenError::InvalidGrant => {
                    oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", None)
                }
                TokenError::AccessDenied(reason) => {
                    oauth_error(StatusCode::FORBIDDEN, "access_denied", Some(reason))
                }
            }
        }
    }
}

fn token_response(tokens: Tokens) -> Response {
    let mut body = json!({
        "access_token": tokens.access_token,
        "token_type": "Bearer",
        "expires_in": tokens.expires_in,
    });
    // Only the authorization_code grant returns these.
    if let Some(refresh) = tokens.refresh_token {
        body["refresh_token"] = json!(refresh);
        body["ha_auth_provider"] = json!(ha_auth::PROVIDER_TYPE);
    }
    (
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        Json(body),
    )
        .into_response()
}

/// Always `200` with an empty body, whatever the token.
async fn revoke(State(auth): State<Auth>, headers: HeaderMap, body: Bytes) -> StatusCode {
    if let Some(t) = form_fields(&headers, &body).get("token") {
        auth.revoke(t);
        persist(&auth).await;
    }
    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const CLIENT: &str = "http://192.168.1.5:8123/";
    const REDIRECT: &str = "http://192.168.1.5:8123/?auth_callback=1";

    struct Client {
        router: Router,
        ip: SocketAddr,
    }

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: String,
    }

    impl Reply {
        fn json(&self) -> Value {
            serde_json::from_str(&self.body).unwrap_or(Value::Null)
        }
    }

    impl Client {
        fn new(auth: Auth) -> Self {
            Self {
                router: crate::router(ha_core::Hass::new(), auth, &crate::ApiConfig::default()),
                ip: "192.168.1.20:50000".parse().unwrap(),
            }
        }

        async fn send(&self, req: axum::http::request::Builder, body: impl Into<Body>) -> Reply {
            let mut req = req.body(body.into()).unwrap();
            req.extensions_mut().insert(ConnectInfo(self.ip));
            let res = self.router.clone().oneshot(req).await.unwrap();
            let status = res.status();
            let headers = res.headers().clone();
            let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap();
            Reply {
                status,
                headers,
                body: String::from_utf8(bytes.to_vec()).unwrap(),
            }
        }

        /// The frontend's `fetch`: JSON without a Content-Type.
        async fn post_json(&self, uri: &str, body: Value) -> Reply {
            self.send(Request::post(uri), body.to_string()).await
        }

        async fn step(&self, flow_id: &str, body: Value) -> Reply {
            self.post_json(&format!("/auth/login_flow/{flow_id}"), body)
                .await
        }

        async fn post_multipart(&self, uri: &str, fields: &[(&str, &str)]) -> Reply {
            let mut body = String::new();
            for (k, v) in fields {
                body.push_str(&format!(
                    "------hars\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n"
                ));
            }
            body.push_str("------hars--\r\n");
            let req = Request::post(uri).header(
                header::CONTENT_TYPE,
                "multipart/form-data; boundary=----hars",
            );
            self.send(req, body).await
        }

        async fn post_form(&self, uri: &str, form: &str) -> Reply {
            let req = Request::post(uri)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            self.send(req, form.to_owned()).await
        }

        async fn get_api(&self, token: Option<&str>) -> Reply {
            let mut req = Request::get("/api/");
            if let Some(t) = token {
                req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
            }
            self.send(req, Body::empty()).await
        }

        async fn login(&self, password: &str) -> Reply {
            self.login_with("adam", password).await
        }

        /// `authorize.html`'s requests, then the token exchange `core.js` does.
        async fn login_with(&self, username: &str, password: &str) -> Reply {
            let flow = self
                .post_json(
                    "/auth/login_flow",
                    json!({"client_id": CLIENT, "handler": ["homeassistant", null], "redirect_uri": REDIRECT}),
                )
                .await
                .json();
            let id = flow["flow_id"].as_str().unwrap().to_owned();
            let done = self
                .post_json(
                    &format!("/auth/login_flow/{id}"),
                    json!({"client_id": CLIENT, "username": username, "password": password}),
                )
                .await;
            if done.json()["type"] != "create_entry" {
                return done;
            }
            let code = done.json()["result"].as_str().unwrap().to_owned();
            self.post_multipart(
                "/auth/token",
                &[
                    ("client_id", CLIENT),
                    ("code", &code),
                    ("grant_type", "authorization_code"),
                ],
            )
            .await
        }
    }

    #[tokio::test]
    async fn the_frontends_login_sequence_works() {
        let c = Client::new(crate::test_util::auth(true));

        let providers = c.send(Request::get("/auth/providers"), Body::empty()).await;
        assert_eq!(providers.status, StatusCode::OK);
        assert_eq!(
            providers.json(),
            json!({"providers": [{"name": "Home Assistant Local", "id": null, "type": "homeassistant"}],
                   "preselect_remember_me": true})
        );

        let form = c
            .post_json(
                "/auth/login_flow",
                json!({"client_id": CLIENT, "handler": ["homeassistant", null], "redirect_uri": REDIRECT}),
            )
            .await;
        assert_eq!(form.status, StatusCode::OK);
        let flow_id = form.json()["flow_id"].as_str().unwrap().to_owned();
        assert_eq!(flow_id.len(), 32);
        assert_eq!(
            form.json(),
            json!({"type": "form", "flow_id": flow_id, "handler": ["homeassistant", null],
                   "data_schema": [{"type": "string", "name": "username", "required": true},
                                   {"type": "string", "name": "password", "required": true}],
                   "errors": {}, "description_placeholders": null, "last_step": null,
                   "preview": null, "step_id": "init"})
        );

        // A wrong password re-shows the form on the same flow.
        let wrong = c
            .post_json(
                &format!("/auth/login_flow/{flow_id}"),
                json!({"client_id": CLIENT, "username": "adam", "password": "nope"}),
            )
            .await;
        assert_eq!(wrong.status, StatusCode::OK);
        assert_eq!(wrong.json()["errors"], json!({"base": "invalid_auth"}));
        assert_eq!(wrong.json()["flow_id"], flow_id);

        let done = c
            .post_json(
                &format!("/auth/login_flow/{flow_id}"),
                json!({"client_id": CLIENT, "username": "Adam", "password": "secret"}),
            )
            .await;
        let code = done.json()["result"].as_str().unwrap().to_owned();
        assert_eq!(
            done.json(),
            json!({"type": "create_entry", "flow_id": flow_id, "handler": ["homeassistant", null],
                   "description": null, "description_placeholders": null, "result": code})
        );

        let tokens = c
            .post_multipart(
                "/auth/token",
                &[
                    ("client_id", CLIENT),
                    ("code", &code),
                    ("grant_type", "authorization_code"),
                ],
            )
            .await;
        assert_eq!(tokens.status, StatusCode::OK);
        assert_eq!(tokens.headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(tokens.headers[header::PRAGMA], "no-cache");
        let t = tokens.json();
        assert_eq!(t["token_type"], "Bearer");
        assert_eq!(t["expires_in"], 1800);
        assert_eq!(t["ha_auth_provider"], "homeassistant");
        let access = t["access_token"].as_str().unwrap();
        let refresh = t["refresh_token"].as_str().unwrap();

        assert_eq!(
            c.get_api(Some(access)).await.json(),
            json!({"message": "API running."})
        );

        // Refresh with a urlencoded body: a new access token, no new refresh token.
        let again = c
            .post_form(
                "/auth/token",
                &format!(
                    "grant_type=refresh_token&client_id={}&refresh_token={refresh}",
                    "http%3A%2F%2F192.168.1.5%3A8123%2F"
                ),
            )
            .await;
        assert_eq!(again.status, StatusCode::OK);
        assert!(again.json().get("refresh_token").is_none());
        let access2 = again.json()["access_token"].as_str().unwrap().to_owned();
        assert_eq!(c.get_api(Some(&access2)).await.status, StatusCode::OK);

        // Logout: revoke, and every access token stops working.
        let revoked = c
            .post_multipart("/auth/revoke", &[("token", refresh)])
            .await;
        assert_eq!(
            (revoked.status, revoked.body.as_str()),
            (StatusCode::OK, "")
        );
        assert_eq!(
            c.get_api(Some(access)).await.status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            c.get_api(Some(&access2)).await.status,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn api_needs_a_bearer_token() {
        let c = Client::new(crate::test_util::auth(true));
        let none = c.get_api(None).await;
        assert_eq!(none.status, StatusCode::UNAUTHORIZED);
        assert_eq!(none.body, "401: Unauthorized");
        // The headers HA adds to every response are on errors too.
        assert_eq!(none.headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");

        let access = c.login("secret").await.json()["access_token"]
            .as_str()
            .unwrap()
            .to_owned();
        for header_value in [
            format!("bearer {access}"),
            format!("Basic {access}"),
            access.clone(),
        ] {
            let req = Request::get("/api/states").header(header::AUTHORIZATION, header_value);
            assert_eq!(
                c.send(req, Body::empty()).await.status,
                StatusCode::UNAUTHORIZED
            );
        }
        let req =
            Request::get("/api/states").header(header::AUTHORIZATION, format!("Bearer {access}"));
        assert_eq!(c.send(req, Body::empty()).await.status, StatusCode::OK);
        // Unknown API paths are 404, not 401.
        let req =
            Request::get("/api/nope").header(header::AUTHORIZATION, format!("Bearer {access}"));
        assert_eq!(
            c.send(req, Body::empty()).await.status,
            StatusCode::NOT_FOUND
        );
    }

    /// A non-admin user from an imported HA install, logged in.
    async fn login_as(c: &Client, auth: &Auth, username: &str, group: &str) -> String {
        auth.add_user(username, "pw").unwrap();
        // The first login creates the user (an admin, as in HA).
        let token = c.login_with(username, "pw").await.json()["access_token"]
            .as_str()
            .unwrap()
            .to_owned();
        let user_id = auth.validate_access_token(&token, None).unwrap().user_id;
        assert!(auth.set_user_groups(&user_id, &[group]));
        token
    }

    #[tokio::test]
    async fn non_admins_get_has_permissions() {
        let auth = crate::test_util::auth(true);
        let c = Client::new(auth.clone());
        let send = |token: &str, method: Method, uri: &str, body: &str| {
            let req = Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {token}"));
            c.send(req, body.to_owned())
        };
        let write = r#"{"state": "on"}"#;
        let call = "{}";

        let reader = login_as(&c, &auth, "reader", ha_auth::GROUP_ID_READ_ONLY).await;
        assert_eq!(
            send(&reader, Method::GET, "/api/states", "").await.status,
            StatusCode::OK
        );
        assert_eq!(
            send(&reader, Method::GET, "/api/services", "").await.status,
            StatusCode::OK
        );
        let r = send(&reader, Method::POST, "/api/states/sensor.x", write).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        let r = send(&reader, Method::POST, "/api/services/light/turn_on", call).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);

        let user = login_as(&c, &auth, "user", ha_auth::GROUP_ID_USER).await;
        let r = send(&user, Method::POST, "/api/states/sensor.x", write).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        // Allowed through; the (unregistered) service is then a 400.
        let r = send(&user, Method::POST, "/api/services/light/turn_on", call).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);

        let custom = login_as(&c, &auth, "custom", "some-custom-group").await;
        assert_eq!(
            send(&custom, Method::GET, "/api/states", "").await.status,
            StatusCode::UNAUTHORIZED
        );

        let owner = c.login("secret").await.json()["access_token"]
            .as_str()
            .unwrap()
            .to_owned();
        // A new entity: 201, as HA.
        let r = send(&owner, Method::POST, "/api/states/sensor.x", write).await;
        assert_eq!(r.status, StatusCode::CREATED);
    }

    #[tokio::test]
    async fn pending_flows_hold_bounded_memory() {
        let c = Client::new(crate::test_util::auth(true));
        let long = format!("http://192.168.1.5:8123/{}", "a".repeat(MAX_URL));
        let r = c
            .post_json(
                "/auth/login_flow",
                json!({"client_id": CLIENT, "handler": ["homeassistant", null], "redirect_uri": long}),
            )
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert!(
            r.json()["message"]
                .as_str()
                .unwrap()
                .contains("at most 2048")
        );
        let huge = "x".repeat(MAX_BODY + 1);
        let r = c.send(Request::post("/auth/login_flow"), huge).await;
        assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn token_errors_use_has_oauth_bodies() {
        let c = Client::new(crate::test_util::auth(true));
        type Case<'a> = (&'a [(&'a str, &'a str)], StatusCode, Value);
        let cases: [Case; 6] = [
            (
                &[("grant_type", "password")],
                StatusCode::BAD_REQUEST,
                json!({"error": "unsupported_grant_type"}),
            ),
            (
                &[],
                StatusCode::BAD_REQUEST,
                json!({"error": "unsupported_grant_type"}),
            ),
            (
                &[("grant_type", "authorization_code"), ("code", "x")],
                StatusCode::BAD_REQUEST,
                json!({"error": "invalid_request", "error_description": "Invalid client id"}),
            ),
            (
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", CLIENT),
                    ("code", "x"),
                ],
                StatusCode::BAD_REQUEST,
                json!({"error": "invalid_request", "error_description": "Invalid code"}),
            ),
            (
                &[("grant_type", "refresh_token"), ("client_id", CLIENT)],
                StatusCode::BAD_REQUEST,
                json!({"error": "invalid_request"}),
            ),
            (
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", CLIENT),
                    ("refresh_token", "x"),
                ],
                StatusCode::BAD_REQUEST,
                json!({"error": "invalid_grant"}),
            ),
        ];
        for (fields, status, body) in cases {
            let r = c.post_multipart("/auth/token", fields).await;
            assert_eq!((r.status, r.json()), (status, body), "{fields:?}");
        }
        // Revoke always answers 200 with no body, even for junk.
        let r = c.post_form("/auth/revoke", "token=junk").await;
        assert_eq!((r.status, r.body.as_str()), (StatusCode::OK, ""));
        let r = c.post_form("/auth/token", "action=revoke&token=junk").await;
        assert_eq!((r.status, r.body.as_str()), (StatusCode::OK, ""));
    }

    #[tokio::test]
    async fn login_flow_rejections() {
        let c = Client::new(crate::test_util::auth(true));
        let start = |body: Value| c.post_json("/auth/login_flow", body);
        let msg = |r: Reply| {
            (
                r.status,
                r.json()["message"].as_str().unwrap_or("").to_owned(),
            )
        };

        let bad_json = c.send(Request::post("/auth/login_flow"), "{").await;
        assert_eq!(
            msg(bad_json),
            (StatusCode::BAD_REQUEST, "Invalid JSON.".into())
        );
        let (s, m) =
            msg(start(json!({"handler": ["homeassistant", null], "redirect_uri": REDIRECT})).await);
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(m.starts_with("Message format incorrect: "), "{m}");
        assert_eq!(
            msg(start(json!({"client_id": "ftp://x/", "handler": ["homeassistant", null], "redirect_uri": REDIRECT})).await),
            (StatusCode::BAD_REQUEST, "Invalid client id".into())
        );
        assert_eq!(
            msg(start(json!({"client_id": CLIENT, "handler": ["insecure_example", null], "redirect_uri": REDIRECT})).await),
            (StatusCode::NOT_FOUND, "Invalid handler specified".into())
        );

        let flow = start(json!({"client_id": CLIENT, "handler": ["homeassistant", null], "redirect_uri": "http://evil.example/"}))
            .await
            .json()["flow_id"].as_str().unwrap().to_owned();
        assert_eq!(
            msg(c
                .step(
                    &flow,
                    json!({"client_id": CLIENT, "username": "adam", "password": "secret", "x": 1})
                )
                .await),
            (StatusCode::BAD_REQUEST, "User input malformed".into())
        );
        assert_eq!(
            msg(c
                .step(
                    &flow,
                    json!({"client_id": CLIENT, "username": "adam", "password": "secret"})
                )
                .await),
            (StatusCode::FORBIDDEN, "Invalid redirect URI".into())
        );
        assert_eq!(
            msg(c
                .step(
                    &"0".repeat(32),
                    json!({"client_id": CLIENT, "username": "adam", "password": "secret"})
                )
                .await),
            (StatusCode::NOT_FOUND, "Invalid flow specified".into())
        );

        // A flow can't be finished from another address.
        let flow = start(json!({"client_id": CLIENT, "handler": ["homeassistant", null], "redirect_uri": REDIRECT}))
            .await
            .json()["flow_id"].as_str().unwrap().to_owned();
        let other = Client {
            router: c.router.clone(),
            ip: "192.168.1.99:1".parse().unwrap(),
        };
        let r = other
            .post_json(
                &format!("/auth/login_flow/{flow}"),
                json!({"client_id": CLIENT, "username": "adam", "password": "secret"}),
            )
            .await;
        assert_eq!(
            msg(r),
            (StatusCode::BAD_REQUEST, "IP address changed".into())
        );

        let del = c
            .send(
                Request::delete(format!("/auth/login_flow/{flow}")),
                Body::empty(),
            )
            .await;
        assert_eq!(msg(del), (StatusCode::OK, "Flow aborted".into()));
        let del = c
            .send(
                Request::delete(format!("/auth/login_flow/{flow}")),
                Body::empty(),
            )
            .await;
        assert_eq!(
            msg(del),
            (StatusCode::NOT_FOUND, "Invalid flow specified".into())
        );
        assert_eq!(
            c.send(Request::get("/auth/login_flow"), Body::empty())
                .await
                .status,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    #[tokio::test]
    async fn fresh_install_reports_onboarding() {
        let c = Client::new(crate::test_util::auth(false));
        let p = c.send(Request::get("/auth/providers"), Body::empty()).await;
        assert_eq!(
            (p.status, p.json()),
            (
                StatusCode::BAD_REQUEST,
                json!({"message": "Onboarding not finished", "code": "onboarding_required"})
            )
        );
    }

    #[test]
    fn multipart_parsing() {
        let body = "preamble\r\n--b\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n1\r\n\
                    --b\r\ncontent-disposition: form-data; name=\"f\"; filename=\"x.txt\"\r\nContent-Type: text/plain\r\n\r\nfile\r\n\
                    --b\r\nContent-Disposition: form-data; name=\"b\"\r\n\r\nline1\r\nline2\r\n--b--\r\n";
        let f = multipart_fields("multipart/form-data; boundary=\"b\"", body.as_bytes()).unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(f["a"], "1");
        assert_eq!(f["b"], "line1\r\nline2");
        assert!(multipart_fields("multipart/form-data", body.as_bytes()).is_none());
        assert!(multipart_fields("multipart/form-data; boundary=b", b"--b\r\nbroken").is_none());
    }
}

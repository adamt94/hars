//! Onboarding's HTTP views (`components/onboarding/views.py`): how the
//! official frontend's `/onboarding.html` sets up a fresh install. The
//! steps are HA's, in HA's order, kept in HA's `.storage/onboarding` (4.1):
//!
//! 1. `user` (`POST /api/onboarding/users`, anonymous): the owner, their
//!    person, the three default areas named in the chosen language; answers
//!    an auth code the page logs in with.
//! 2. `core_config` (`POST /api/onboarding/core_config`): after the page set
//!    the location with `config/core/update`; starts met's onboarding flow.
//! 3. `analytics` (`POST /api/onboarding/analytics`): after the page saved
//!    the preferences with `analytics/preferences`.
//! 4. `integration` (`POST /api/onboarding/integration`): answers an auth
//!    code for the page's own client, which then logs in on the dashboard.
//!
//! Each step has its own lock and refuses to run twice ("… already done",
//! 403); the store is saved as each step is done, and a save that fails is
//! HA's 500 with no auth code. An owner whose logins can't be saved is
//! taken back, so the user step can run again (logins are written before
//! users, so disk never holds an owner without a password). An owner closes
//! the user step whatever the store says, and auth codes are bound to the
//! login they were issued for, so a login taken back or made again
//! invalidates them. The views exist only
//! while a step was left when `hars` started: after a restart with all of
//! them done they are 404s, as in HA (until then they answer as done).
//!
//! Where `hars` differs from HA:
//! - The user step checks the username and the name before creating
//!   anything. HA creates the user first, so a username that isn't
//!   normalized (HA's 500) or an empty name (HA's 400 from the person's
//!   schema) leaves a user behind in HA; here it leaves nothing.
//! - With several mistakes in a body, which one the 400 names can differ:
//!   voluptuous keeps the client's key order and an unordered set of the
//!   missing keys; `hars` reports the first by name.
//! - Bodies are capped at 16 KiB, as on the auth views (these are anonymous
//!   until the owner exists).
//! - Areas whose name has letters `hars`'s `slugify` doesn't transliterate
//!   (Cyrillic, Greek, CJK, Hebrew, some Latin) get a different id than
//!   HA's (the roadmap's `slugify` item); their names are HA's.

// Body checks return the finished error `Response`, as `auth.rs`'s do: it
// is only built for a rejected request, so its size doesn't matter.
#![allow(clippy::result_large_err)]

use crate::Shared;
use crate::auth::{MAX_BODY, format_error, json_body, json_message};
use crate::onboarding_data::{AREA_KEYS, AREA_NAMES};
use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use ha_auth::{Principal, indieauth};
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// The onboarding views and their per-step locks (`_BaseOnboardingStepView`).
#[derive(Clone)]
pub(crate) struct Views(Arc<Inner>);

struct Inner {
    shared: Shared,
    user: tokio::sync::Mutex<()>,
    core_config: tokio::sync::Mutex<()>,
    analytics: tokio::sync::Mutex<()>,
    integration: tokio::sync::Mutex<()>,
}

/// What `async_get_system_info` makes of where `hars` runs, by HA's own
/// rules: on Linux in a container (`/.dockerenv`, `/run/.containerenv` or
/// Kubernetes), "Home Assistant Container" only for HA's official image
/// (`/OFFICIAL_IMAGE`, run as root), else "Unsupported Third Party
/// Container" (what `hars`'s image is to HA); elsewhere "Unknown", as HA
/// says outside a Python virtualenv. Blocking (it looks at files): call it
/// once at start-up.
pub fn installation_type() -> &'static str {
    let exists = |p: &str| std::path::Path::new(p).exists();
    // `cached_get_user() == "root"`.
    let root = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Uid:"))
                .and_then(|uids| uids.split_whitespace().next().map(|u| u == "0"))
        })
        .unwrap_or(false);
    installation_type_of(
        cfg!(target_os = "linux"),
        &exists,
        std::env::var_os("KUBERNETES_SERVICE_HOST").is_some(),
        root,
    )
}

/// [`installation_type`]'s rules, given what it looks at.
fn installation_type_of(
    linux: bool,
    exists: &dyn Fn(&str) -> bool,
    kubernetes: bool,
    root: bool,
) -> &'static str {
    let official = exists("/OFFICIAL_IMAGE");
    // `is_docker_env`, looked at on Linux only.
    let container =
        linux && (exists("/.dockerenv") || exists("/run/.containerenv") || kubernetes || official);
    match (container, root && official) {
        (false, _) => "Unknown",
        (true, true) => "Home Assistant Container",
        (true, false) => "Unsupported Third Party Container",
    }
}

impl Views {
    pub(crate) fn new(shared: Shared) -> Self {
        Self(Arc::new(Inner {
            shared,
            user: Default::default(),
            core_config: Default::default(),
            analytics: Default::default(),
            integration: Default::default(),
        }))
    }

    /// The views that need no login (`requires_auth = False`).
    pub(crate) fn anonymous_routes(&self) -> Router {
        Router::new()
            .route("/api/onboarding", get(status))
            .route("/api/onboarding/installation_type", get(installation))
            .route("/api/onboarding/users", post(user_step))
            .route("/api/onboarding/integration/wait", post(wait))
            .layer(DefaultBodyLimit::max(MAX_BODY))
            .with_state(self.clone())
    }

    /// The step views that need a `Bearer` token: merged under
    /// `auth::require_auth`. Any logged-in user may call them, as in HA.
    pub(crate) fn authed_routes(&self) -> Router {
        Router::new()
            .route("/api/onboarding/core_config", post(core_config_step))
            .route("/api/onboarding/analytics", post(analytics_step))
            .route("/api/onboarding/integration", post(integration_step))
            .layer(DefaultBodyLimit::max(MAX_BODY))
            .with_state(self.clone())
    }
}

/// `RequestDataValidator` with a schema of required `str` keys: the body
/// is JSON whatever its `Content-Type`. Errors as voluptuous reports the
/// first: a wrong or unknown key, else a missing one.
fn strs<'a>(body: &'a Map<String, Value>, keys: &[&str]) -> Result<Vec<&'a str>, Response> {
    for (k, v) in body {
        if !keys.contains(&k.as_str()) {
            return Err(format_error(&format!(
                "extra keys not allowed @ data['{k}']"
            )));
        }
        if !v.is_string() {
            return Err(format_error(&format!(
                "expected str for dictionary value @ data['{k}']"
            )));
        }
    }
    keys.iter()
        .map(|k| {
            body.get(*k)
                .and_then(Value::as_str)
                .ok_or_else(|| format_error(&format!("required key not provided @ data['{k}']")))
        })
        .collect()
}

/// aiohttp's text answers (`HTTPBadRequest`, an unhandled exception).
fn text(status: StatusCode, body: &'static str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body,
    )
        .into_response()
}

/// HA's answer to an exception it doesn't handle, such as a store that
/// can't be written.
fn server_error() -> Response {
    text(
        StatusCode::INTERNAL_SERVER_ERROR,
        "500 Internal Server Error\n\nServer got itself in trouble",
    )
}

/// Write what changed in the auth stores now, off the async threads; the
/// error is the caller's (HA awaits its saves here and an error becomes a
/// 500).
async fn save(shared: &Shared) -> Result<(), ()> {
    let auth = shared.auth.clone();
    match tokio::task::spawn_blocking(move || auth.save()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            tracing::error!("onboarding: saving auth state: {e}");
            Err(())
        }
        Err(e) => {
            tracing::error!("onboarding: saving auth state: {e}");
            Err(())
        }
    }
}

/// `_async_mark_done`: the step joins the store's `done` list, saved
/// before answering. As in HA, a failed save leaves the step done in
/// memory (the views then say so) and the request fails.
async fn mark_done(shared: &Shared, step: &str) -> Result<(), ()> {
    shared.auth.mark_step_done(step);
    save(shared).await
}

/// `OnboardingStatusView`: each step and whether it's done.
async fn status(State(v): State<Views>) -> Response {
    let steps: Vec<Value> =
        v.0.shared
            .auth
            .onboarding_steps()
            .into_iter()
            .map(|(step, done)| json!({"step": step, "done": done}))
            .collect();
    Json(steps).into_response()
}

/// `InstallationTypeOnboardingView`: only before any step is done.
async fn installation(State(v): State<Views>) -> Response {
    let s = &v.0.shared;
    if s.auth.onboarding_steps().iter().any(|(_, done)| *done) {
        return crate::auth::unauthorized();
    }
    Json(json!({"installation_type": s.config.installation_type})).into_response()
}

/// The default areas' names in `language` (`async_get_translations`):
/// that language's, else English.
fn area_names(language: &str) -> [&'static str; 3] {
    let find = |lang: &str| {
        AREA_NAMES
            .binary_search_by(|(l, _)| (*l).cmp(lang))
            .ok()
            .map(|i| AREA_NAMES[i].1)
    };
    find(language)
        .or_else(|| find("en"))
        .expect("English names are generated")
}

/// `UserOnboardingView`.
async fn user_step(State(v): State<Views>, body: Bytes) -> Response {
    let body = match json_body(&body) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let [name, username, password, client_id, language] = match strs(
        &body,
        &["name", "username", "password", "client_id", "language"],
    ) {
        Ok(f) => [f[0], f[1], f[2], f[3], f[4]],
        Err(e) => return e,
    };
    let s = &v.0.shared;
    let _step = v.0.user.lock().await;
    // An owner closes the step even if the store doesn't say so (HA infers
    // it at start; `Auth::load` does too).
    if s.auth.is_step_done("user") || s.auth.has_owner() {
        return json_message(StatusCode::FORBIDDEN, "User step already done");
    }
    // The person's schema (`vol.Length(min=1)`), checked first.
    if name.is_empty() {
        return text(StatusCode::BAD_REQUEST, "400: Bad Request");
    }
    let created = {
        let (auth, name, username, password) = (
            s.auth.clone(),
            name.to_owned(),
            username.to_owned(),
            password.to_owned(),
        );
        tokio::task::spawn_blocking(move || auth.create_owner(&name, &username, &password)).await
    };
    // A username that isn't normalized or is taken is HA's unhandled
    // `InvalidUsername`: a 500.
    let owner = match created
        .map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| e.to_string()))
    {
        Ok(owner) => owner,
        Err(e) => {
            tracing::error!("onboarding: {e}");
            return server_error();
        }
    };
    // HA awaits the login's save here (`async_add_auth`). If it fails, the
    // owner is taken back, so a retry makes one owner, not a second user;
    // the passwords are written first, so disk never has an owner without
    // one.
    if save(s).await.is_err() {
        s.auth.remove_owner(&owner);
        return server_error();
    }
    let registry = s.config.registry.as_deref();
    s.config
        .components
        .create_person(&s.hass, registry, name, Some(&owner.user_id));
    // Areas an integration already made (by name) are left alone.
    if let Some(registry) = registry {
        for (name, (_, icon)) in area_names(language).into_iter().zip(AREA_KEYS) {
            // An area of that name is left as it is.
            let _ = registry.create_area(ha_registry::NewArea {
                name: (*name).to_owned(),
                icon: Some((*icon).to_owned()),
                ..Default::default()
            });
        }
    }
    if mark_done(s, "user").await.is_err() {
        // The owner and their password are saved: the page's reload finds
        // the step done and logs in, and so does a restart.
        return server_error();
    }
    // The login was just saved; it can only be gone if something else
    // took it back, and then there's nothing to log in to.
    let Some(code) = s.auth.create_auth_code(client_id, &owner.username) else {
        return server_error();
    };
    Json(json!({ "auth_code": code })).into_response()
}

/// `CoreConfigOnboardingView`: the location is already set
/// (`config/core/update`); start the integrations HA starts for a new
/// home (met's forecast).
async fn core_config_step(State(v): State<Views>) -> Response {
    let s = &v.0.shared;
    let _step = v.0.core_config.lock().await;
    if s.auth.is_step_done("core_config") {
        return json_message(StatusCode::FORBIDDEN, "Core config step already done");
    }
    if mark_done(s, "core_config").await.is_err() {
        return server_error();
    }
    if let Some(flows) = &s.config.onboarding {
        flows.core_config_done();
    }
    Json(json!({})).into_response()
}

/// `AnalyticsOnboardingView`: the preferences are already saved
/// (`analytics/preferences`).
async fn analytics_step(State(v): State<Views>) -> Response {
    let s = &v.0.shared;
    let _step = v.0.analytics.lock().await;
    if s.auth.is_step_done("analytics") {
        return json_message(StatusCode::FORBIDDEN, "Analytics config step already done");
    }
    if mark_done(s, "analytics").await.is_err() {
        return server_error();
    }
    Json(json!({})).into_response()
}

/// `IntegrationOnboardingView`: the last step, then an auth code for the
/// page's own client (as HA, the step is done before the redirect URI is
/// checked).
async fn integration_step(
    State(v): State<Views>,
    principal: Option<Extension<Principal>>,
    body: Bytes,
) -> Response {
    let body = match json_body(&body) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let [client_id, redirect_uri] = match strs(&body, &["client_id", "redirect_uri"]) {
        Ok(f) => [f[0], f[1]],
        Err(e) => return e,
    };
    let Some(Extension(principal)) = principal else {
        return json_message(StatusCode::FORBIDDEN, "Refresh token not available");
    };
    let s = &v.0.shared;
    let _step = v.0.integration.lock().await;
    if s.auth.is_step_done("integration") {
        return json_message(StatusCode::FORBIDDEN, "Integration step already done");
    }
    if mark_done(s, "integration").await.is_err() {
        return server_error();
    }
    if !indieauth::verify_redirect_uri(client_id, redirect_uri) {
        return json_message(StatusCode::BAD_REQUEST, "invalid client id or redirect uri");
    }
    let Some(username) = s.auth.refresh_token_username(&principal.refresh_token_id) else {
        return json_message(StatusCode::FORBIDDEN, "Credentials for user not available");
    };
    let Some(code) = s.auth.create_auth_code(client_id, &username) else {
        return json_message(StatusCode::FORBIDDEN, "Credentials for user not available");
    };
    Json(json!({ "auth_code": code })).into_response()
}

/// `WaitIntegrationOnboardingView`: whether an integration is loaded.
/// `hars` starts integrations at once, so there's nothing to wait for.
async fn wait(State(v): State<Views>, body: Bytes) -> Response {
    let body = match json_body(&body) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let domain = match strs(&body, &["domain"]) {
        Ok(f) => f[0],
        Err(e) => return e,
    };
    let loaded = crate::components(&v.0.shared.config)
        .iter()
        .any(|c| c.as_ref() == domain);
    Json(json!({ "integration_loaded": loaded })).into_response()
}

#[cfg(test)]
mod tests;

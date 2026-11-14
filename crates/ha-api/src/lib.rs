//! The HTTP surface of `hars`: Home Assistant's REST API and the official
//! frontend, served unchanged from disk.

mod auth;
mod frontend;
mod rest;

pub use frontend::Frontend;

use axum::{
    Router,
    http::{HeaderName, HeaderValue, header},
    response::Response,
};
use ha_auth::Auth;
use ha_core::Hass;
use std::sync::Arc;

/// What the API serves besides the REST endpoints.
#[derive(Clone, Default)]
pub struct ApiConfig {
    /// The `hass_frontend` directory from the pinned
    /// `home-assistant-frontend` release. `None` serves the API only.
    pub frontend: Option<Arc<Frontend>>,
}

/// Every route, with the headers HA adds to every response. `/api/*`
/// needs a `Bearer` access token; `/auth/*` is how a client gets one.
///
/// Serve it with `into_make_service_with_connect_info::<SocketAddr>()`:
/// login flows and `local_only` users depend on the client's address.
pub fn router(hass: Hass, auth: Auth, config: &ApiConfig) -> Router {
    let api = rest::router(hass).route_layer(axum::middleware::from_fn_with_state(
        auth.clone(),
        auth::require_auth,
    ));
    let mut router = api.merge(auth::routes(auth.clone()));
    if let Some(fe) = &config.frontend {
        router = router.merge(frontend::routes(fe.clone(), auth));
    }
    router.layer(axum::middleware::map_response(security_headers))
}

/// HA's `headers.py`: added to every response, errors included.
async fn security_headers(mut res: Response) -> Response {
    const HEADERS: [(HeaderName, &str); 3] = [
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "SAMEORIGIN"),
    ];
    let headers = res.headers_mut();
    for (name, value) in HEADERS {
        headers.insert(name, HeaderValue::from_static(value));
    }
    res
}

#[cfg(test)]
mod test_util {
    use ha_auth::Auth;

    /// An `Auth` in a fresh temp dir. `onboarded` adds the owner `adam` /
    /// `secret` (bcrypt cost 4), which completes onboarding.
    pub(crate) fn auth(onboarded: bool) -> Auth {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "hars-api-auth-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let auth = Auth::load_with_cost(&dir, ha_core::Clock::system(), 4).unwrap();
        if onboarded {
            auth.add_user("adam", "secret").unwrap();
        }
        auth
    }
}

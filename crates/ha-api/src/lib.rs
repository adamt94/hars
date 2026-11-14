//! The HTTP surface of `hars`: Home Assistant's REST API and the official
//! frontend, served unchanged from disk.

mod frontend;
mod rest;

pub use frontend::Frontend;

use axum::{
    Router,
    http::{HeaderName, HeaderValue, header},
    response::Response,
};
use ha_core::Hass;
use std::sync::Arc;

/// What the API serves besides the REST endpoints.
#[derive(Clone, Default)]
pub struct ApiConfig {
    /// The `hass_frontend` directory from the pinned
    /// `home-assistant-frontend` release. `None` serves the API only.
    pub frontend: Option<Arc<Frontend>>,
}

/// Every route, with the headers HA adds to every response.
pub fn router(hass: Hass, config: &ApiConfig) -> Router {
    let mut router = rest::router(hass);
    if let Some(fe) = &config.frontend {
        router = router.merge(frontend::routes(fe.clone()));
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

//! The HTTP surface of `hars`: Home Assistant's REST API and the official
//! frontend, served unchanged from disk.

mod analytics;
mod auth;
mod automation;
pub mod brands;
mod cors;
mod cv;
mod frontend;
mod frontend_data;
mod i18n;
mod labs;
#[rustfmt::skip]
mod i18n_data;
mod lovelace;
pub mod map_tiles;
mod mobile_app;
#[rustfmt::skip]
mod manifests_data;
mod onboarding;
#[rustfmt::skip]
mod onboarding_data;
mod rest;
pub mod system_log;
pub(crate) mod ws;

pub use analytics::Analytics;
pub use frontend::Frontend;
pub use frontend_data::FrontendData;
pub use labs::Labs;
pub use lovelace::Lovelace;
pub use onboarding::installation_type;
pub use ws::WHOAMI_URL;

use axum::{
    Router,
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
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
    /// The config directory, as `get_config` reports it.
    pub config_dir: String,
    /// Where settings changed through the API (`.storage/core.config`) are
    /// written; `None` keeps them in memory (tests).
    pub storage: Option<std::path::PathBuf>,
    /// What onboarding says this install is ([`installation_type`]).
    pub installation_type: &'static str,
    /// HA's location service for `config/core/detect`; `None` is
    /// [`WHOAMI_URL`] (tests use a local server).
    pub whoami_url: Option<String>,
    /// The analytics preferences (stored, never sent).
    pub analytics: Arc<Analytics>,
    /// `core.config_entries`, as `config_entries/subscribe` lists them.
    pub config_entries: Option<Arc<ha_integrations::config_entries::ConfigEntries>>,
    /// The config flows onboarding starts after its core config step
    /// (`met`), and what they started.
    pub onboarding: Option<Arc<ha_integrations::onboarding::Onboarding>>,
    /// The frontend's per-user and system settings.
    pub frontend_data: Arc<FrontendData>,
    /// Home Assistant Labs' preview features.
    pub labs: Arc<Labs>,
    /// Entity, device, area, floor and label registries, as they change.
    pub registry: Option<Arc<ha_registry::Registry>>,
    /// Lovelace dashboards.
    pub lovelace: Arc<Lovelace>,
    /// Stored zones and people.
    pub components: Arc<ha_components::Components>,
    /// Integrations running (`sun`, `sensor`, ...), for `components`.
    pub integrations: Vec<&'static str>,
    /// Weather entities' forecasts.
    pub weather: Arc<ha_integrations::weather::Weather>,
    /// Brand images and their access tokens.
    pub brands: Option<Arc<brands::Brands>>,
    /// The map's background tiles, relayed from OpenStreetMap.
    pub map_tiles: Option<Arc<map_tiles::MapTiles>>,
    /// The automations, from `configuration.yaml`.
    pub automations: Option<ha_automation::Automations>,
    /// The companion apps' registrations (`mobile_app`).
    pub mobile_app: Option<Arc<ha_integrations::mobile_app::MobileApp>>,
    /// State and event history, when recording.
    pub recorder: Option<Arc<ha_recorder::Recorder>>,
}

impl ApiConfig {
    /// The registries as they are now (empty without a registry).
    pub fn registries(&self) -> Arc<ha_registry::Registries> {
        self.registry
            .as_ref()
            .map(|r| r.snapshot())
            .unwrap_or_default()
    }
}

/// What the WebSocket and `/api/config` need.
#[derive(Clone)]
pub(crate) struct Shared {
    pub hass: Hass,
    pub auth: Auth,
    pub config: Arc<ApiConfig>,
    /// `render_template`'s compiled templates.
    pub templates: Arc<ws::template::Cache>,
    /// Held while the core config is read, changed and saved, so one
    /// update can't undo another.
    pub core_config: Arc<tokio::sync::Mutex<()>>,
}

/// The integrations `hars` implements, by HA domain: `get_config`'s
/// `components`. The frontend gates features on this list, so it only names
/// what really works.
pub(crate) fn components(config: &ApiConfig) -> Vec<&'static str> {
    let mut c = vec![
        "api",
        "auth",
        "http",
        "labs",
        "lovelace",
        "onboarding",
        "persistent_notification",
        "person",
        "repairs",
        "websocket_api",
        "zone",
    ];
    if config.frontend.is_some() {
        c.push("frontend");
    }
    if config.brands.is_some() {
        c.push("brands");
    }
    c.extend(&config.integrations);
    if let Some(o) = &config.onboarding {
        c.extend(o.started());
    }
    c.sort_unstable();
    c.dedup();
    c
}

/// `hass.config.as_dict()`, for `get_config` and `GET /api/config`.
/// `external_url` is left out for `local_only` users, as HA does.
pub(crate) fn config_json(shared: &Shared, local_only: bool) -> serde_json::Value {
    let c = shared.hass.config();
    let mut v = serde_json::json!({
        "allowlist_external_dirs": [],
        "allowlist_external_urls": [],
        "components": components(&shared.config),
        "config_dir": shared.config.config_dir,
        "config_source": c.source.as_str(),
        "country": c.country,
        "currency": c.currency,
        "debug": false,
        "elevation": c.elevation,
        "external_url": c.external_url,
        "internal_url": c.internal_url,
        "language": c.language,
        "latitude": c.latitude,
        "location_name": c.location_name,
        "longitude": c.longitude,
        "radius": c.radius,
        "recovery_mode": false,
        "safe_mode": false,
        "state": shared.hass.core_state(),
        "time_zone": c.time_zone,
        "unit_system": c.unit_system(),
        "version": ha_auth::HA_VERSION,
        "whitelist_external_dirs": [],
    });
    if local_only {
        v.as_object_mut().expect("object").remove("external_url");
    }
    v
}

async fn api_config(
    axum::extract::State(shared): axum::extract::State<Shared>,
    axum::Extension(principal): axum::Extension<ha_auth::Principal>,
) -> axum::Json<serde_json::Value> {
    let local_only = shared
        .auth
        .user(&principal.user_id)
        .is_some_and(|u| u.local_only);
    axum::Json(config_json(&shared, local_only))
}

/// Every route, with the headers HA adds to every response. `/api/*`
/// needs a `Bearer` access token; `/auth/*` is how a client gets one.
///
/// Serve it with `into_make_service_with_connect_info::<SocketAddr>()`:
/// login flows and `local_only` users depend on the client's address.
pub fn router(hass: Hass, auth: Auth, config: &ApiConfig) -> Router {
    let shared = Shared {
        hass: hass.clone(),
        auth: auth.clone(),
        config: Arc::new(config.clone()),
        templates: Default::default(),
        core_config: Arc::default(),
    };
    // HA registers the onboarding views only while a step is left
    // (`onboarding.async_setup`): once every step was done they are 404s.
    let onboarding = (!auth.is_onboarded()).then(|| onboarding::Views::new(shared.clone()));
    let config_route = Router::new()
        .route("/api/config", axum::routing::get(api_config))
        .with_state(shared.clone());
    let mut api = rest::router(hass)
        .merge(config_route)
        .merge(automation::routes(config.config_dir.clone().into()))
        .merge(mobile_app::registration_routes(shared.clone()));
    if let Some(views) = &onboarding {
        api = api.merge(views.authed_routes());
    }
    let api = api.route_layer(axum::middleware::from_fn_with_state(
        auth.clone(),
        auth::require_auth,
    ));
    // The WebSocket authenticates inside the protocol, not over HTTP.
    let mut router = api
        .merge(auth::routes(auth.clone()))
        .merge(onboarding.map(|v| v.anonymous_routes()).unwrap_or_default())
        .merge(mobile_app::routes(shared.clone()))
        .merge(ws::routes(shared));
    if let Some(b) = &config.brands {
        router = router.merge(brands::routes(b.clone(), auth.clone()));
    }
    if let Some(t) = &config.map_tiles {
        router = router.merge(map_tiles::routes(t.clone()));
    }
    if let Some(fe) = &config.frontend {
        router = router.merge(frontend::routes(fe.clone(), auth, config.lovelace.clone()));
    }
    router.layer(axum::middleware::map_response(security_headers))
}

/// aiohttp's answer when a view raises.
pub(crate) fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "500 Internal Server Error\n\nServer got itself in trouble",
    )
        .into_response()
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

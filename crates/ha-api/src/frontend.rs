//! Serves the official frontend (`home-assistant-frontend`) from disk, the
//! way HA 2026.6.1's `frontend` component does.
//!
//! - `index.html` is a template. It is rendered once at start-up and served
//!   for `/` and every registered panel path. Any other path is a 404; there
//!   is no catch-all.
//! - `/frontend_latest`, `/frontend_es5`, `/static` and `/onboarding.html`
//!   send `Cache-Control: public, max-age=2678400`. HA never sends
//!   `immutable`.
//! - Precompressed `.br` / `.gz` siblings are served when the client accepts
//!   them.

use axum::{
    Router,
    body::Bytes,
    extract::Path,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use ha_auth::Auth;
use serde_json::{Value, json};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use tower_http::services::{ServeDir, ServeFile};

/// HA's `CACHE_TIME`: 31 days.
const CACHE_CONTROL: &str = "public, max-age=2678400";

/// HA's default `theme_color` (no custom default theme).
const THEME_COLOR: &str = "#2980b9";

/// The first path segments that serve `index.html`: the frontend's built-in
/// panels plus those the MVP's components register.
pub(crate) const PANEL_PATHS: &[&str] = &[
    // Built into the frontend (`frontend/__init__.py`).
    "light",
    "security",
    "climate",
    "home",
    "maintenance",
    "profile",
    "notfound",
    // Registered by components.
    "lovelace",
    "map",
    "config",
    "history",
    "logbook",
];

/// A loaded frontend: the directory plus the pre-rendered dynamic files.
pub struct Frontend {
    root: PathBuf,
    index_html: Bytes,
    manifest_json: Bytes,
}

impl Frontend {
    /// Load the `hass_frontend` directory and render `index.html`.
    pub fn load(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        let template = std::fs::read_to_string(root.join("index.html"))?;
        let index_html = render_index(&template).map_err(std::io::Error::other)?;
        Ok(Self {
            root,
            index_html: Bytes::from(index_html),
            manifest_json: Bytes::from(manifest().to_string()),
        })
    }

    pub fn root(&self) -> &FsPath {
        &self.root
    }
}

/// Render the `index.html` template with HA's defaults: the theme colour,
/// and no `extra_module_url` / `extra_js_url_es5` entries. Only the three
/// spans HA's template uses are understood; anything else is an error, so
/// a frontend upgrade that changes the template is caught at start-up.
fn render_index(template: &str) -> Result<String, String> {
    let mut out = template.replace("{{ theme_color }}", THEME_COLOR);
    // `{%- for x in xs -%} ... {%- endfor -%}` renders to nothing when the
    // list is empty, and `{%-` / `-%}` also eat the whitespace around it.
    while let Some(start) = out.find("{%- for ") {
        const END: &str = "{%- endfor -%}";
        let end = out[start..]
            .find(END)
            .map(|i| start + i + END.len())
            .ok_or("index.html: unterminated {% for %}")?;
        let before = out[..start].trim_end().len();
        let after = end + (out[end..].len() - out[end..].trim_start().len());
        out.replace_range(before..after, "");
    }
    if out.contains("{{") || out.contains("{%") {
        return Err("index.html: template syntax hars does not render".into());
    }
    Ok(out)
}

/// HA's `MANIFEST_JSON` with the default theme colour. `serde_json` maps
/// are sorted, matching HA's `json_dumps_sorted`.
fn manifest() -> Value {
    let any = [192, 384, 512, 1024].map(|s| {
        json!({
            "purpose": "any",
            "sizes": format!("{s}x{s}"),
            "src": format!("/static/icons/favicon-{s}x{s}.png"),
            "type": "image/png",
        })
    });
    let maskable = [48, 72, 96, 128, 192, 384, 512].map(|s| {
        json!({
            "purpose": "maskable",
            "sizes": format!("{s}x{s}"),
            "src": format!("/static/icons/maskable_icon-{s}x{s}.png"),
            "type": "image/png",
        })
    });
    let icons: Vec<Value> = any.into_iter().chain(maskable).collect();
    json!({
        "background_color": "#FFFFFF",
        "description": "Home automation platform that puts local control and privacy first.",
        "dir": "ltr",
        "display": "standalone",
        "icons": icons,
        "id": "/?homescreen=1",
        "lang": "en-US",
        "name": "Home Assistant",
        "prefer_related_applications": true,
        "related_applications": [
            { "id": "io.homeassistant.companion.android", "platform": "play" }
        ],
        "screenshots": [{
            "sizes": "413x792",
            "src": "/static/images/screenshots/screenshot-1.png",
            "type": "image/png",
        }],
        "short_name": "Home Assistant",
        "start_url": "/?homescreen=1",
        "theme_color": THEME_COLOR,
    })
}

pub(crate) fn routes(fe: Arc<Frontend>, auth: Auth) -> Router {
    let root = fe.root.clone();
    let file = |name: &str| {
        ServeFile::new(root.join(name))
            .precompressed_br()
            .precompressed_gzip()
    };
    let dir = |name: &str| {
        ServeDir::new(root.join(name))
            .precompressed_br()
            .precompressed_gzip()
    };

    let mut router = Router::new();
    // Uncached single files.
    for name in [
        "service_worker.js",
        "sw-modern.js",
        "sw-modern.js.map",
        "sw-legacy.js",
        "sw-legacy.js.map",
        "robots.txt",
    ] {
        router = router.route_service(&format!("/{name}"), file(name));
    }
    router = router.route_service("/auth/authorize", file("authorize.html"));

    // Cached files and directories.
    let cached = Router::new()
        .route_service("/onboarding.html", file("onboarding.html"))
        .nest_service("/static", dir("static"))
        .nest_service("/frontend_latest", dir("frontend_latest"))
        .nest_service("/frontend_es5", dir("frontend_es5"))
        .layer(axum::middleware::map_response(cache_headers));

    // `IndexView.get`: until onboarding is done, everything goes there.
    let page = {
        let fe = fe.clone();
        move || {
            if auth.is_onboarded() {
                html(&fe.index_html)
            } else {
                redirect(StatusCode::FOUND, "/onboarding.html")
            }
        }
    };
    let index = {
        let page = page.clone();
        move || {
            let res = page();
            async move { res }
        }
    };
    let panel = move |Path(segments): Path<Vec<String>>| {
        let res = match segments.first() {
            Some(first) if PANEL_PATHS.contains(&first.as_str()) => page(),
            _ => StatusCode::NOT_FOUND.into_response(),
        };
        async move { res }
    };
    let manifest = move || {
        let body = fe.manifest_json.clone();
        async move { ([(header::CONTENT_TYPE, "application/manifest+json")], body).into_response() }
    };

    router
        .merge(cached)
        .layer(axum::middleware::map_response(vary_on_encoding))
        .route("/", get(index))
        .route("/{panel}", get(panel.clone()))
        // `{*rest}` needs a non-empty tail; HA also serves `/lovelace/`.
        .route("/{panel}/", get(panel.clone()))
        .route("/{panel}/{*rest}", get(panel))
        .route("/manifest.json", get(manifest))
        .route(
            "/.well-known/change-password",
            get(|| async { redirect(StatusCode::FOUND, "/profile") }),
        )
        .route(
            "/shopping-list",
            get(|| async { redirect(StatusCode::MOVED_PERMANENTLY, "/todo") }),
        )
        .route(
            "/developer-tools",
            get(|| async { redirect(StatusCode::MOVED_PERMANENTLY, "/config/developer-tools") }),
        )
        .route("/developer-tools/{tool}", get(developer_tools))
}

/// HA moved developer tools under `/config`; the old paths redirect.
async fn developer_tools(Path(tool): Path<String>) -> Response {
    const TOOLS: [&str; 8] = [
        "yaml",
        "state",
        "action",
        "template",
        "event",
        "statistics",
        "assist",
        "debug",
    ];
    if TOOLS.contains(&tool.as_str()) {
        redirect(
            StatusCode::MOVED_PERMANENTLY,
            &format!("/config/developer-tools/{tool}"),
        )
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

/// HA's redirects are 301 (`HTTPMovedPermanently`) or 302, not axum's
/// 303/308.
fn redirect(status: StatusCode, to: &str) -> Response {
    match HeaderValue::from_str(to) {
        Ok(location) => (status, [(header::LOCATION, location)]).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn html(body: &Bytes) -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body.clone(),
    )
        .into_response()
}

/// A precompressed file depends on `Accept-Encoding`, so shared caches must
/// key on it (aiohttp's `FileResponse` adds this; tower-http does not).
async fn vary_on_encoding(mut res: Response) -> Response {
    if res.headers().contains_key(header::CONTENT_ENCODING) {
        res.headers_mut()
            .insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    }
    res
}

/// HA's `CachingStaticResource`: files served from these paths are cached
/// for 31 days. Errors are not.
async fn cache_headers(mut res: Response) -> Response {
    if res.status().is_success() || res.status() == StatusCode::NOT_MODIFIED {
        res.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(CACHE_CONTROL),
        );
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    /// The template spans of `home-assistant-frontend` 20260527.4's
    /// `index.html`, verbatim.
    const TEMPLATE: &str = concat!(
        r#"<meta name="theme-color" content="{{ theme_color }}"><script>"#,
        "{%- for extra_module in extra_modules -%}\n",
        "        import(\"{{ extra_module }}\");\n",
        "        {%- endfor -%}</script><script>if (!window.latestJS) {\n",
        "          {%- for extra_script in extra_js_es5 -%}\n",
        "          _ls(\"{{ extra_script }}\");\n",
        "          {%- endfor -%}\n",
        "        }</script></body></html>",
    );

    #[test]
    fn renders_index_like_jinja() {
        assert_eq!(
            render_index(TEMPLATE).unwrap(),
            concat!(
                r##"<meta name="theme-color" content="#2980b9">"##,
                "<script></script><script>if (!window.latestJS) {}</script></body></html>",
            )
        );
        assert!(render_index("{{ unknown }}").is_err());
        assert!(render_index("{%- for x in y -%}").is_err());
    }

    /// A tiny `hass_frontend` directory in the system temp dir.
    fn fake_frontend() -> Arc<Frontend> {
        let root = std::env::temp_dir().join(format!(
            "hars-frontend-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        for (path, body) in [
            ("index.html", TEMPLATE),
            ("authorize.html", "<authorize>"),
            ("onboarding.html", "<onboarding>"),
            ("robots.txt", "User-agent: *\nDisallow: /\n"),
            ("frontend_latest/core.abc.js", "plain js"),
            ("frontend_latest/core.abc.js.br", "brotli js"),
            ("frontend_latest/core.abc.js.gz", "gzip js"),
            ("static/icons/favicon.png", "png"),
        ] {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        Arc::new(Frontend::load(&root).unwrap())
    }

    async fn get(path: &str, accept_encoding: Option<&str>) -> Response {
        get_with(crate::test_util::auth(true), path, accept_encoding).await
    }

    async fn get_with(auth: Auth, path: &str, accept_encoding: Option<&str>) -> Response {
        let config = crate::ApiConfig {
            frontend: Some(fake_frontend()),
        };
        let mut req = Request::get(path);
        if let Some(enc) = accept_encoding {
            req = req.header(header::ACCEPT_ENCODING, enc);
        }
        crate::router(ha_core::Hass::new(), auth, &config)
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn index_sends_a_fresh_install_to_onboarding() {
        let auth = crate::test_util::auth(false);
        for path in ["/", "/lovelace/0", "/config/"] {
            let res = get_with(auth.clone(), path, None).await;
            assert_eq!(res.status(), StatusCode::FOUND, "{path}");
            assert_eq!(header_of(&res, header::LOCATION), Some("/onboarding.html"));
        }
        // Unknown paths are still 404, and the onboarding page itself loads.
        assert_eq!(
            get_with(auth.clone(), "/nope", None).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get_with(auth, "/onboarding.html", None).await.status(),
            StatusCode::OK
        );
    }

    fn header_of(res: &Response, name: header::HeaderName) -> Option<&str> {
        res.headers().get(name).map(|v| v.to_str().unwrap())
    }

    async fn body(res: Response) -> String {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn index_is_served_for_root_and_panels_only() {
        for path in [
            "/",
            "/home",
            "/lovelace/0",
            "/lovelace/",
            "/config/",
            "/map",
            "/config/integrations/dashboard",
        ] {
            let res = get(path, None).await;
            assert_eq!(res.status(), StatusCode::OK, "{path}");
            assert_eq!(
                header_of(&res, header::CONTENT_TYPE),
                Some("text/html; charset=utf-8")
            );
            assert_eq!(header_of(&res, header::CACHE_CONTROL), None, "{path}");
            assert!(body(res).await.contains("#2980b9"));
        }
        for path in ["/nope", "/index.html", "/nope/deeper", "/nope/"] {
            assert_eq!(
                get(path, None).await.status(),
                StatusCode::NOT_FOUND,
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn assets_are_precompressed_and_cached() {
        let res = get("/frontend_latest/core.abc.js", Some("gzip, br")).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(header_of(&res, header::CONTENT_ENCODING), Some("br"));
        assert_eq!(header_of(&res, header::VARY), Some("accept-encoding"));
        assert_eq!(
            header_of(&res, header::CONTENT_TYPE),
            Some("text/javascript")
        );
        assert_eq!(header_of(&res, header::CACHE_CONTROL), Some(CACHE_CONTROL));
        assert_eq!(body(res).await, "brotli js");

        let res = get("/frontend_latest/core.abc.js", Some("gzip")).await;
        assert_eq!(header_of(&res, header::CONTENT_ENCODING), Some("gzip"));
        let res = get("/frontend_latest/core.abc.js", None).await;
        assert_eq!(header_of(&res, header::CONTENT_ENCODING), None);
        assert_eq!(body(res).await, "plain js");

        let res = get("/static/icons/favicon.png", None).await;
        assert_eq!(header_of(&res, header::CACHE_CONTROL), Some(CACHE_CONTROL));
        let res = get("/onboarding.html", None).await;
        assert_eq!(header_of(&res, header::CACHE_CONTROL), Some(CACHE_CONTROL));

        // Missing files and traversal are 404s, and errors are not cached.
        for path in [
            "/static/nope.png",
            "/static/../index.html",
            "/static/%2e%2e/index.html",
        ] {
            let res = get(path, None).await;
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path}");
            assert_eq!(header_of(&res, header::CACHE_CONTROL), None, "{path}");
        }
    }

    #[tokio::test]
    async fn uncached_files_and_manifest() {
        let res = get("/auth/authorize", None).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(header_of(&res, header::CACHE_CONTROL), None);
        assert_eq!(body(res).await, "<authorize>");
        let res = get("/robots.txt", None).await;
        assert_eq!(header_of(&res, header::CACHE_CONTROL), None);

        let res = get("/manifest.json", None).await;
        assert_eq!(
            header_of(&res, header::CONTENT_TYPE),
            Some("application/manifest+json")
        );
        let manifest: Value = serde_json::from_str(&body(res).await).unwrap();
        assert_eq!(manifest["theme_color"], THEME_COLOR);
        assert_eq!(manifest["icons"].as_array().unwrap().len(), 11);
        assert_eq!(manifest["start_url"], "/?homescreen=1");
    }

    #[tokio::test]
    async fn redirects_use_ha_status_codes() {
        for (path, status, location) in [
            (
                "/.well-known/change-password",
                StatusCode::FOUND,
                "/profile",
            ),
            ("/shopping-list", StatusCode::MOVED_PERMANENTLY, "/todo"),
            (
                "/developer-tools",
                StatusCode::MOVED_PERMANENTLY,
                "/config/developer-tools",
            ),
            (
                "/developer-tools/state",
                StatusCode::MOVED_PERMANENTLY,
                "/config/developer-tools/state",
            ),
        ] {
            let res = get(path, None).await;
            assert_eq!(res.status(), status, "{path}");
            assert_eq!(header_of(&res, header::LOCATION), Some(location), "{path}");
        }
        assert_eq!(
            get("/developer-tools/nope", None).await.status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn security_headers_on_every_response() {
        for path in ["/", "/nope", "/api/", "/static/nope.png"] {
            let res = get(path, None).await;
            assert_eq!(
                header_of(&res, header::REFERRER_POLICY),
                Some("no-referrer")
            );
            assert_eq!(
                header_of(&res, header::X_CONTENT_TYPE_OPTIONS),
                Some("nosniff")
            );
            assert_eq!(header_of(&res, header::X_FRAME_OPTIONS), Some("SAMEORIGIN"));
        }
    }
}

//! The `brands` integration (`components/brands`): integration and
//! hardware brand images, proxied from HA's brands CDN and cached on disk
//! (`<config>/.cache/brands`), and `brands/access_token`, the rotating
//! token the frontend puts in image URLs instead of a bearer header.

use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use ha_auth::Auth;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// HA's brands CDN.
pub const CDN: &str = "https://brands.home-assistant.io";
const CDN_TIMEOUT: Duration = Duration::from_secs(10);
/// A cached image older than this is refreshed in the background.
const CACHE_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const TOKEN_CHANGE_INTERVAL: Duration = Duration::from_secs(30 * 60);
const PLACEHOLDER: &str = "_placeholder";

const ALLOWED_IMAGES: [&str; 8] = [
    "icon.png",
    "logo.png",
    "icon@2x.png",
    "logo@2x.png",
    "dark_icon.png",
    "dark_logo.png",
    "dark_icon@2x.png",
    "dark_logo@2x.png",
];

pub struct Brands {
    /// The two newest tokens (an image URL made just before a rotation
    /// still works).
    tokens: Mutex<VecDeque<String>>,
    cache_dir: PathBuf,
    cdn: String,
    client: ha_http::Client,
}

/// `hex(SystemRandom().getrandbits(256))[2:]`: no leading zeros.
fn new_token() -> String {
    let bytes: [u8; 32] = rand::random();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    match hex.trim_start_matches('0') {
        "" => "0".into(),
        t => t.to_owned(),
    }
}

impl Brands {
    /// Brands cached under `config_dir/.cache/brands`, fetched from `cdn`,
    /// with a token rotating every 30 minutes.
    pub fn new(config_dir: &std::path::Path, cdn: &str) -> Result<Arc<Self>, ha_http::Error> {
        let client = ha_http::Client::builder(ha_integrations::USER_AGENT)
            .build()?
            .with_timeouts(CDN_TIMEOUT, CDN_TIMEOUT);
        let b = Arc::new(Self {
            tokens: Mutex::new(VecDeque::from([new_token()])),
            cache_dir: config_dir.join(".cache").join("brands"),
            cdn: cdn.trim_end_matches('/').to_owned(),
            client,
        });
        let weak = Arc::downgrade(&b);
        tokio::spawn(async move {
            let mut every = tokio::time::interval(TOKEN_CHANGE_INTERVAL);
            every.tick().await;
            loop {
                every.tick().await;
                let Some(b) = weak.upgrade() else { return };
                b.rotate();
            }
        });
        Ok(b)
    }

    fn rotate(&self) {
        let mut t = self.tokens.lock();
        t.push_back(new_token());
        while t.len() > 2 {
            t.pop_front();
        }
    }

    /// `brands/access_token`.
    pub fn token(&self) -> String {
        self.tokens.lock().back().cloned().unwrap_or_default()
    }

    fn valid(&self, token: &str) -> bool {
        self.tokens.lock().iter().any(|t| t == token)
    }

    /// Fetch from the CDN and cache: a 404 is cached as an empty file,
    /// other failures aren't cached.
    async fn fetch(&self, cdn_path: &str, cache: PathBuf) -> Option<Vec<u8>> {
        let url = format!("{}/{cdn_path}", self.cdn);
        let res = self.client.get(&url, &[]).await.ok()?;
        let data = match res.status.as_u16() {
            404 => Vec::new(),
            200 => res.body.to_vec(),
            other => {
                tracing::debug!("Unexpected CDN response {other} for {cdn_path}");
                return None;
            }
        };
        let to_write = data.clone();
        // Written aside and renamed in, so a reader sees the old file or
        // the new one, never part of one.
        let _ = tokio::task::spawn_blocking(move || {
            let dir = cache.parent().ok_or(std::io::ErrorKind::NotFound)?;
            std::fs::create_dir_all(dir)?;
            let tmp = dir.join(format!(
                ".{}.{:x}.tmp",
                cache
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("image"),
                rand::random::<u64>()
            ));
            std::fs::write(&tmp, to_write)?;
            std::fs::rename(&tmp, &cache).inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            })
        })
        .await;
        (!data.is_empty()).then_some(data)
    }

    /// `_serve_from_cache_or_cdn`, with HA's placeholder for a missing
    /// image.
    async fn serve(self: &Arc<Self>, cdn_path: String, sub: String, placeholder: bool) -> Response {
        let path = self.cache_dir.join(&sub);
        let p = path.clone();
        let cached = tokio::task::spawn_blocking(move || {
            let meta = std::fs::metadata(&p).ok().filter(|m| m.is_file())?;
            let age = meta
                .modified()
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
                .unwrap_or_default();
            Some((std::fs::read(&p).ok()?, age))
        })
        .await
        .ok()
        .flatten();
        let data = match cached {
            Some((data, age)) => {
                if age > CACHE_TTL {
                    let me = self.clone();
                    tokio::spawn(async move { me.fetch(&cdn_path, path).await });
                }
                (!data.is_empty()).then_some(data)
            }
            None => self.fetch(&cdn_path, path).await,
        };
        match data {
            Some(d) => ([(header::CONTENT_TYPE, "image/png")], d).into_response(),
            None if placeholder => {
                let image = sub.rsplit('/').next().unwrap_or_default().to_owned();
                Box::pin(self.serve(
                    format!("_/{PLACEHOLDER}/{image}"),
                    format!("integrations/{PLACEHOLDER}/{image}"),
                    false,
                ))
                .await
            }
            None => StatusCode::NOT_FOUND.into_response(),
        }
    }
}

/// `valid_domain`: lower-case letters, digits and single underscores, not
/// starting or ending with one.
fn valid_domain(d: &str) -> bool {
    !d.is_empty()
        && !d.starts_with('_')
        && !d.ends_with('_')
        && !d.contains("__")
        && d.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

#[derive(Clone)]
struct BrandsState {
    brands: Arc<Brands>,
    auth: Auth,
}

/// A bearer access token (checked against the client's address, as
/// `local_only` users need), or one of the brand tokens in `?token=`.
fn authorised(
    s: &BrandsState,
    headers: &HeaderMap,
    ip: Option<std::net::IpAddr>,
    q: &HashMap<String, String>,
) -> bool {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| s.auth.validate_access_token(t, ip).is_some());
    bearer || q.get("token").is_some_and(|t| s.brands.valid(t))
}

async fn integration(
    State(s): State<BrandsState>,
    Path((domain, image)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    crate::auth::RemoteIp(ip): crate::auth::RemoteIp,
    headers: HeaderMap,
) -> Response {
    if !authorised(&s, &headers, ip, &q) {
        return crate::auth::unauthorized();
    }
    if !valid_domain(&domain) || !ALLOWED_IMAGES.contains(&image.as_str()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let placeholder = q.get("placeholder").map(String::as_str) != Some("no");
    s.brands
        .serve(
            format!("brands/{domain}/{image}"),
            format!("integrations/{domain}/{image}"),
            placeholder,
        )
        .await
}

async fn hardware(
    State(s): State<BrandsState>,
    Path((category, image)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    crate::auth::RemoteIp(ip): crate::auth::RemoteIp,
    headers: HeaderMap,
) -> Response {
    if !authorised(&s, &headers, ip, &q) {
        return crate::auth::unauthorized();
    }
    let category_ok = !category.is_empty()
        && category
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    let image_ok = image.strip_suffix(".png").is_some_and(|stem| {
        !stem.is_empty()
            && stem
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    });
    if !category_ok || !image_ok {
        return StatusCode::NOT_FOUND.into_response();
    }
    let sub = format!("hardware/{category}/{image}");
    s.brands.serve(sub.clone(), sub, true).await
}

pub(crate) fn routes(brands: Arc<Brands>, auth: Auth) -> Router {
    Router::new()
        .route("/api/brands/integration/{domain}/{image}", get(integration))
        .route("/api/brands/hardware/{category}/{*image}", get(hardware))
        .with_state(BrandsState { brands, auth })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tower::ServiceExt;

    /// A CDN with one icon; everything else is a 404. Counts requests.
    async fn cdn() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                h.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let path = req.split(' ').nth(1).unwrap_or("").to_owned();
                    let (status, body): (&str, &[u8]) = match path.as_str() {
                        "/brands/hue/icon.png" => ("200 OK", b"HUEPNG"),
                        "/_/_placeholder/icon.png" => ("200 OK", b"PLACEHOLDER"),
                        _ => ("404 Not Found", b""),
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(body).await;
                });
            }
        });
        (base, hits)
    }

    async fn get(router: &Router, uri: &str) -> (StatusCode, Vec<u8>) {
        let res = router
            .clone()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, body.to_vec())
    }

    #[tokio::test]
    async fn proxies_and_caches_as_ha() {
        let (base, hits) = cdn().await;
        let dir = std::env::temp_dir().join(format!("hars-brands-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let brands = Brands::new(&dir, &base).unwrap();
        let auth = crate::test_util::auth(true);
        let router = routes(brands.clone(), auth.clone());
        let token = brands.token();
        assert!(!token.starts_with('0') && token.len() <= 64);

        // No token, or a wrong one: HA's 401.
        let (s, body) = get(&router, "/api/brands/integration/hue/icon.png").await;
        assert_eq!(
            (s, body),
            (StatusCode::UNAUTHORIZED, b"401: Unauthorized".to_vec())
        );
        let (s, _) = get(&router, "/api/brands/integration/hue/icon.png?token=nope").await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);

        let url = |d: &str, i: &str| format!("/api/brands/integration/{d}/{i}?token={token}");
        assert_eq!(
            get(&router, &url("hue", "icon.png")).await,
            (StatusCode::OK, b"HUEPNG".to_vec())
        );
        // Cached: no second fetch.
        let n = hits.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(get(&router, &url("hue", "icon.png")).await.1, b"HUEPNG");
        assert_eq!(hits.load(std::sync::atomic::Ordering::Relaxed), n);
        assert_eq!(
            std::fs::read(dir.join(".cache/brands/integrations/hue/icon.png")).unwrap(),
            b"HUEPNG"
        );

        // A missing brand: the placeholder, unless asked not to; the 404 is
        // cached as an empty file.
        assert_eq!(
            get(&router, &url("nope", "icon.png")).await.1,
            b"PLACEHOLDER"
        );
        assert_eq!(
            std::fs::read(dir.join(".cache/brands/integrations/nope/icon.png")).unwrap(),
            b""
        );
        let (s, _) = get(
            &router,
            &format!("{}&placeholder=no", url("nope", "icon.png")),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        // Not an image HA serves, or not a domain.
        assert_eq!(
            get(&router, &url("hue", "x.png")).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get(&router, &url("Hue", "icon.png")).await.0,
            StatusCode::NOT_FOUND
        );

        // A bearer token, checked from the client's address.
        let access = {
            const CLIENT: &str = "http://127.0.0.1:8123/";
            let flow = auth.start_login_flow(CLIENT, None);
            let user = auth.check_password("adam", "secret").unwrap();
            let code = auth.finish_login_flow(&flow, CLIENT, &user, None).unwrap();
            auth.exchange_code(Some(CLIENT), Some(&code), None)
                .unwrap()
                .access_token
        };
        let mut req = Request::get("/api/brands/integration/hue/icon.png")
            .header("authorization", format!("Bearer {access}"))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [192, 168, 1, 10],
                50000,
            ))));
        assert_eq!(
            router.clone().oneshot(req).await.unwrap().status(),
            StatusCode::OK
        );
        // No temp files left beside the cache.
        let dir_entries: Vec<_> = std::fs::read_dir(dir.join(".cache/brands/integrations/hue"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(dir_entries, ["icon.png"]);

        // The previous token keeps working after one rotation, not two.
        brands.rotate();
        assert_eq!(
            get(&router, &url("hue", "icon.png")).await.0,
            StatusCode::OK
        );
        brands.rotate();
        assert_eq!(
            get(&router, &url("hue", "icon.png")).await.0,
            StatusCode::UNAUTHORIZED
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn domains_as_ha() {
        for (d, ok) in [
            ("hue", true),
            ("switchbot_cloud", true),
            ("a__b", false),
            ("_a", false),
            ("a_", false),
            ("A", false),
            ("", false),
        ] {
            assert_eq!(valid_domain(d), ok, "{d}");
        }
    }
}

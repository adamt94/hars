//! The map's background tiles, relayed from OpenStreetMap. Not part of HA:
//! the pinned frontend draws its maps on CARTO raster tiles, which now
//! carry an "API KEY REQUIRED" overlay. The image build points the
//! frontend's tile layer here instead (`scripts/patch_frontend_map.py`).
//!
//! The relay exists because OSM's tile servers block a browser that sends
//! no `Referer`, and HA's `index.html` sets `referrer: same-origin`, so the
//! browser sends none to another site. It does send one to `hars` (same
//! origin), and OSM's tile policy asks a relay to keep it, so its origin
//! goes upstream along with `hars`'s own `User-Agent`. Tiles aren't
//! stored: the upstream caching headers are passed on, so the browser
//! caches them, and its revalidations are passed upstream.

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use std::sync::Arc;
use std::time::Duration;

/// OSM's standard tile layer.
pub const UPSTREAM: &str = "https://tile.openstreetmap.org";
/// The highest zoom OSM serves; the frontend scales up past it.
const MAX_ZOOM: u8 = 19;
/// OSM's tile usage policy allows two connections at once.
const MAX_CONNECTIONS: usize = 2;
const TIMEOUT: Duration = Duration::from_secs(10);

/// The request headers passed upstream, and the response headers passed back.
const REQUEST_HEADERS: [header::HeaderName; 2] = [header::IF_NONE_MATCH, header::IF_MODIFIED_SINCE];
/// `Age` and `Date` too, so the tile's freshness counts from when OSM made it.
const RESPONSE_HEADERS: [header::HeaderName; 7] = [
    header::CONTENT_TYPE,
    header::CACHE_CONTROL,
    header::EXPIRES,
    header::ETAG,
    header::LAST_MODIFIED,
    header::AGE,
    header::DATE,
];

pub struct MapTiles {
    upstream: String,
    client: ha_http::Client,
}

impl MapTiles {
    /// Tiles fetched from `upstream` (`{upstream}/{z}/{x}/{y}.png`).
    pub fn new(upstream: &str) -> Result<Arc<Self>, ha_http::Error> {
        let client = ha_http::Client::builder(ha_integrations::USER_AGENT)
            .max_connections(MAX_CONNECTIONS)
            .build()?
            .with_timeouts(TIMEOUT, TIMEOUT);
        Ok(Arc::new(Self {
            upstream: upstream.trim_end_matches('/').to_owned(),
            client,
        }))
    }
}

/// A tile that exists at zoom `z`: `x` and `y` below `2^z`.
fn tile(z: &str, x: &str, y: &str) -> Option<(u8, u32, u32)> {
    // Digits only: `u32::from_str` would also take a leading `+`.
    let num = |s: &str| {
        (!s.is_empty() && s.len() <= 10 && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u64>().ok())
            .flatten()
    };
    let z = u8::try_from(num(z)?).ok().filter(|z| *z <= MAX_ZOOM)?;
    let (x, y) = (num(x)?, num(y)?);
    let side = 1u64 << z;
    (x < side && y < side).then_some((z, x as u32, y as u32))
}

/// The page's origin from its `Referer` (`http://host:8123/`): what OSM asks
/// a website to send, without the page's path.
fn referer_origin(headers: &HeaderMap) -> Option<String> {
    let referer = headers.get(header::REFERER)?.to_str().ok()?;
    let (scheme, rest) = referer.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let ok =
        matches!(scheme, "http" | "https") && !authority.is_empty() && !authority.contains('@');
    ok.then(|| format!("{scheme}://{authority}/"))
}

async fn serve(
    State(tiles): State<Arc<MapTiles>>,
    Path((z, x, y)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let Some((z, x, y)) = tile(&z, &x, &y) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let url = format!("{}/{z}/{x}/{y}.png", tiles.upstream);
    let mut forward: Vec<(header::HeaderName, String)> = REQUEST_HEADERS
        .iter()
        .filter_map(|name| {
            let value = headers.get(name)?.to_str().ok()?;
            Some((name.clone(), value.to_owned()))
        })
        .collect();
    if let Some(origin) = referer_origin(&headers) {
        forward.push((header::REFERER, origin));
    }
    let res = match tiles.client.get(&url, &forward).await {
        Ok(res) => res,
        Err(e) => {
            tracing::debug!("map tile {z}/{x}/{y}: {e}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let status = match res.status.as_u16() {
        200 => StatusCode::OK,
        304 => StatusCode::NOT_MODIFIED,
        404 => StatusCode::NOT_FOUND,
        other => {
            tracing::debug!("map tile {z}/{x}/{y}: upstream answered {other}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let mut out = (status, res.body).into_response();
    for name in RESPONSE_HEADERS {
        if let Some(value) = res.headers.get(&name) {
            out.headers_mut().insert(name, value.clone());
        }
    }
    out
}

/// `GET /hars/map_tiles/{z}/{x}/{y}`. No login: the frontend loads tiles as
/// plain images, which carry no bearer token, and they are public map data.
/// Only OSM's tile URLs can be reached through it.
pub(crate) fn routes(tiles: Arc<MapTiles>) -> Router {
    Router::new()
        .route("/hars/map_tiles/{z}/{x}/{y}", get(serve))
        .with_state(tiles)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use parking_lot::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tower::ServiceExt;

    /// An OSM stand-in: `/1/0/1.png` is a tile (a 304 when revalidated with
    /// its ETag), `/2/0/0.png` a server error, anything else a 404. Keeps
    /// each request's head.
    async fn upstream() -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let log = log.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                    log.lock().push(req.clone());
                    let path = req.split(' ').nth(1).unwrap_or("").to_owned();
                    let (status, extra, body): (&str, &str, &[u8]) = match path.as_str() {
                        "/1/0/1.png" if req.contains("if-none-match: \"t1\"") => {
                            ("304 Not Modified", "ETag: \"t1\"\r\n", b"")
                        }
                        "/1/0/1.png" => (
                            "200 OK",
                            "Content-Type: image/png\r\nCache-Control: max-age=600\r\n\
                             ETag: \"t1\"\r\nAge: 590\r\nDate: Tue, 17 Nov 2026 10:00:00 GMT\r\n\
                             Set-Cookie: a=b\r\n",
                            b"PNG",
                        ),
                        "/2/0/0.png" => ("503 Service Unavailable", "", b""),
                        _ => ("404 Not Found", "", b""),
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(body).await;
                });
            }
        });
        (base, seen)
    }

    async fn get(router: &Router, uri: &str, etag: Option<&str>) -> Response {
        let mut req = Request::get(uri).header("referer", "http://192.168.1.10:8123/map?x=1");
        if let Some(e) = etag {
            req = req.header("if-none-match", e);
        }
        router
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn relays_osm_tiles_with_their_caching_headers() {
        let (base, seen) = upstream().await;
        let router = routes(MapTiles::new(&base).unwrap());

        let res = get(&router, "/hars/map_tiles/1/0/1", None).await;
        assert_eq!(res.status(), StatusCode::OK);
        let h = res.headers().clone();
        assert_eq!(h["content-type"], "image/png");
        assert_eq!(h["cache-control"], "max-age=600");
        assert_eq!(h["etag"], "\"t1\"");
        assert_eq!(h["age"], "590");
        assert_eq!(h["date"], "Tue, 17 Nov 2026 10:00:00 GMT");
        assert!(h.get("set-cookie").is_none());
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(&body[..], b"PNG");
        // Asked for as hars, which OSM accepts in place of a referrer.
        let first = seen.lock()[0].clone();
        assert!(first.starts_with("get /1/0/1.png http/1.1"), "{first}");
        assert!(first.contains("user-agent: hars/"), "{first}");
        // The page's origin, as OSM asks a relay to keep it; not its path.
        assert!(
            first.contains("referer: http://192.168.1.10:8123/\r\n"),
            "{first}"
        );

        // The browser's revalidation goes upstream; a 304 comes back.
        let res = get(&router, "/hars/map_tiles/1/0/1", Some("\"t1\"")).await;
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(res.headers()["etag"], "\"t1\"");

        // A missing tile stays a 404; an upstream failure is a 502.
        assert_eq!(
            get(&router, "/hars/map_tiles/1/1/1", None).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get(&router, "/hars/map_tiles/2/0/0", None).await.status(),
            StatusCode::BAD_GATEWAY
        );

        // Not a tile: answered here, never asked upstream.
        let asked = seen.lock().len();
        for bad in [
            "/hars/map_tiles/1/2/0",
            "/hars/map_tiles/1/0/2",
            "/hars/map_tiles/20/0/0",
            "/hars/map_tiles/0/0/0.png",
            "/hars/map_tiles/0/+0/0",
            "/hars/map_tiles/0/-1/0",
            "/hars/map_tiles/0/0/%2e%2e",
            "/hars/map_tiles/99999999999/0/0",
        ] {
            assert_eq!(
                get(&router, bad, None).await.status(),
                StatusCode::NOT_FOUND,
                "{bad}"
            );
        }
        assert_eq!(seen.lock().len(), asked);
    }

    #[test]
    fn referers_are_cut_to_their_origin() {
        let origin = |r: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::REFERER, r.parse().unwrap());
            referer_origin(&h)
        };
        assert_eq!(
            origin("https://ha.example:8123/lovelace/0#x").as_deref(),
            Some("https://ha.example:8123/")
        );
        assert_eq!(
            origin("http://192.168.1.10:8123?a").as_deref(),
            Some("http://192.168.1.10:8123/")
        );
        assert_eq!(origin("http://user:pw@host/"), None);
        assert_eq!(origin("file:///x"), None);
        assert_eq!(origin("nonsense"), None);
        assert_eq!(referer_origin(&HeaderMap::new()), None);
    }

    #[test]
    fn tiles_exist_up_to_osms_zoom() {
        assert_eq!(tile("0", "0", "0"), Some((0, 0, 0)));
        assert_eq!(tile("19", "524287", "524287"), Some((19, 524287, 524287)));
        assert_eq!(tile("19", "524288", "0"), None);
        assert_eq!(tile("20", "0", "0"), None);
        assert_eq!(tile("", "0", "0"), None);
        assert_eq!(tile("1", "1 ", "0"), None);
    }
}

//! CORS for the views HA marks `cors_allowed` (`/auth/token`,
//! `/auth/revoke`): aiohttp_cors 0.8.1 as HA's `http/cors.py` sets it up for
//! them, any origin (`"*"`), any method, and HA's five request headers.

use axum::{
    extract::Request,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{Next, from_fn},
    response::{IntoResponse, Response},
    routing::MethodRouter,
};

/// HA's `ALLOWED_CORS_HEADERS`, upper-cased as aiohttp_cors compares them.
const ALLOWED_HEADERS: [&str; 5] = [
    "ORIGIN",
    "ACCEPT",
    "X-REQUESTED-WITH",
    "CONTENT-TYPE",
    "AUTHORIZATION",
];

/// A `POST`-only view with CORS from any origin: `OPTIONS` is the
/// preflight, a `POST` from an `Origin` gets `Access-Control-Allow-Origin`
/// set to it (errors included), and any other method is aiohttp's 405,
/// which aiohttp_cors leaves alone.
pub(crate) fn allow_all<S>(post: MethodRouter<S>) -> MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    post.route_layer(from_fn(allow_origin))
        .options(|headers: HeaderMap| async move { preflight(&headers) })
        .fallback(|| async { method_not_allowed() })
}

async fn allow_origin(req: Request, next: Next) -> Response {
    let origin = req.headers().get(header::ORIGIN).cloned();
    let mut res = next.run(req).await;
    if let Some(origin) = origin {
        res.headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    res
}

/// aiohttp_cors's `_preflight_handler`. Header sets are joined in sorted
/// order (aiohttp_cors joins a `frozenset`, in hash order).
fn preflight(headers: &HeaderMap) -> Response {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return forbidden("origin header is not specified in the request");
    };
    let Some(method) = headers.get(header::ACCESS_CONTROL_REQUEST_METHOD) else {
        return forbidden("'Access-Control-Request-Method' header is not specified");
    };
    let requested: std::collections::BTreeSet<String> = headers
        .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
        .map(|v| {
            String::from_utf8_lossy(v.as_bytes())
                .split(',')
                .map(|h| h.trim_matches([' ', '\t']).to_uppercase())
                .filter(|h| !h.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let disallowed: Vec<&str> = requested
        .iter()
        .map(String::as_str)
        .filter(|h| !ALLOWED_HEADERS.contains(h))
        .collect();
    if !disallowed.is_empty() {
        return forbidden(&format!(
            "headers are not allowed: {}",
            disallowed.join(", ")
        ));
    }
    let mut res = StatusCode::OK.into_response();
    let out = res.headers_mut();
    out.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    out.insert(header::ACCESS_CONTROL_ALLOW_METHODS, method.clone());
    if !requested.is_empty() {
        let joined = requested.into_iter().collect::<Vec<_>>().join(",");
        if let Ok(v) = HeaderValue::from_str(&joined) {
            out.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, v);
        }
    }
    res
}

/// aiohttp's `HTTPMethodNotAllowed` for a view with only `post` (and the
/// preflight route aiohttp_cors adds).
fn method_not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::ALLOW, "OPTIONS,POST"),
        ],
        "405: Method Not Allowed",
    )
        .into_response()
}

/// aiohttp's `HTTPForbidden(text=...)`.
fn forbidden(why: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!("CORS preflight request failed: {why}"),
    )
        .into_response()
}

#[cfg(test)]
mod tests;

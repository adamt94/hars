//! The one outbound HTTP client integrations share: HTTP/1.1, TLS with
//! Mozilla's CA roots compiled in (as HA ships `certifi`; no CA files are
//! read, so making a client is cheap and does no I/O), a request timeout
//! and a cap on how much of a response is read. One pooled connection per
//! host is kept only while it's in use, so an idle `hars` holds no sockets.

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Limited};
use hyper::{Request, Uri, header::HeaderMap};
use hyper_util::client::legacy::{Client as Hyper, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use std::time::Duration;

pub use hyper::{StatusCode, header};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid URL: {0}")]
    Url(String),
    #[error("request failed: {0}")]
    Request(String),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    #[error("response larger than {0} bytes")]
    TooLarge(usize),
}

/// A response, read in full.
#[derive(Debug)]
pub struct Response {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

#[derive(Clone)]
pub struct Client {
    inner: Hyper<hyper_rustls::HttpsConnector<HttpConnector>, Empty<Bytes>>,
    user_agent: header::HeaderValue,
    headers_timeout: Duration,
    total_timeout: Duration,
    max_body: usize,
}

/// aiohttp's `max_redirects`.
const MAX_REDIRECTS: usize = 10;

impl Client {
    /// `user_agent` identifies `hars` to the services it calls (api.met.no
    /// requires one). Timeouts are HA's: 30 s until the response headers
    /// (pyMetno's own timeout, redirects included), 300 s for everything
    /// (aiohttp's session default).
    pub fn new(user_agent: &str) -> Result<Self, Error> {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        let inner = Hyper::builder(TokioExecutor::new())
            // Don't keep connections open between the (rare) requests.
            .pool_idle_timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(1)
            .build(https);
        Ok(Self {
            inner,
            user_agent: header::HeaderValue::from_str(user_agent)
                .map_err(|e| Error::Request(e.to_string()))?,
            headers_timeout: Duration::from_secs(30),
            total_timeout: Duration::from_secs(300),
            max_body: 4 * 1024 * 1024,
        })
    }

    /// How long to wait for the response headers, and for the whole
    /// exchange.
    pub fn with_timeouts(mut self, headers: Duration, total: Duration) -> Self {
        self.headers_timeout = headers;
        self.total_timeout = total;
        self
    }

    /// `GET url` with extra headers, following redirects as aiohttp does
    /// (up to 10, never from a public host to a loopback one, as HA's
    /// guard). The body is capped in size.
    pub async fn get(
        &self,
        url: &str,
        headers: &[(header::HeaderName, String)],
    ) -> Result<Response, Error> {
        let start = tokio::time::Instant::now();
        let headers_by = start + self.headers_timeout;
        let total_by = start + self.total_timeout;
        let mut uri: Uri = url.parse().map_err(|_| Error::Url(url.to_owned()))?;
        let loopback = is_loopback(&uri);
        let mut redirects = 0;
        let res = loop {
            let mut req = Request::get(uri.clone())
                .header(header::USER_AGENT, self.user_agent.clone())
                .body(Empty::<Bytes>::new())
                .map_err(|e| Error::Request(e.to_string()))?;
            for (name, value) in headers {
                let value = header::HeaderValue::from_str(value)
                    .map_err(|e| Error::Request(e.to_string()))?;
                req.headers_mut().insert(name.clone(), value);
            }
            let res = tokio::time::timeout_at(headers_by.min(total_by), self.inner.request(req))
                .await
                .map_err(|_| Error::Timeout(self.headers_timeout))?
                .map_err(|e| Error::Request(e.to_string()))?;
            let location = res
                .headers()
                .get(header::LOCATION)
                .and_then(|l| l.to_str().ok());
            let redirect = matches!(res.status().as_u16(), 301 | 302 | 303 | 307 | 308);
            let Some(location) = location.filter(|_| redirect) else {
                break res;
            };
            redirects += 1;
            if redirects > MAX_REDIRECTS {
                return Err(Error::Request(format!(
                    "more than {MAX_REDIRECTS} redirects"
                )));
            }
            let next = resolve(&uri, location).ok_or_else(|| Error::Url(location.to_owned()))?;
            if !loopback && is_loopback(&next) {
                return Err(Error::Request(format!("redirect to loopback {next}")));
            }
            uri = next;
        };
        let (parts, body) = res.into_parts();
        let body = tokio::time::timeout_at(total_by, Limited::new(body, self.max_body).collect())
            .await
            .map_err(|_| Error::Timeout(self.total_timeout))?
            .map_err(|e| {
                if e.downcast_ref::<http_body_util::LengthLimitError>()
                    .is_some()
                {
                    Error::TooLarge(self.max_body)
                } else {
                    Error::Request(e.to_string())
                }
            })?
            .to_bytes();
        Ok(Response {
            status: parts.status,
            headers: parts.headers,
            body,
        })
    }
}

/// A redirect's `Location`, against the URL that answered with it.
fn resolve(base: &Uri, location: &str) -> Option<Uri> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return location.parse().ok();
    }
    let scheme = base.scheme_str()?;
    if let Some(rest) = location.strip_prefix("//") {
        return format!("{scheme}://{rest}").parse().ok();
    }
    let authority = base.authority()?;
    let path = if location.starts_with('/') {
        location.to_owned()
    } else {
        let dir = base.path().rsplit_once('/').map_or("", |(d, _)| d);
        format!("{dir}/{location}")
    };
    format!("{scheme}://{authority}{path}").parse().ok()
}

fn is_loopback(uri: &Uri) -> bool {
    let host = uri.host().unwrap_or_default().trim_matches(['[', ']']);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A one-shot HTTP server: returns what it received and answers with
    /// `reply`.
    async fn serve_once(reply: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/path?q=1", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = s.read(&mut buf).await.unwrap();
            s.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        (url, task)
    }

    #[tokio::test]
    async fn sends_the_user_agent_and_headers_and_reads_the_body() {
        let (url, server) =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nExpires: Thu, 01 Jan 2026 00:00:00 GMT\r\n\r\nhello").await;
        let client = Client::new("hars/0.1 test").unwrap();
        let res = client
            .get(
                &url,
                &[(
                    header::IF_MODIFIED_SINCE,
                    "Wed, 31 Dec 2025 00:00:00 GMT".into(),
                )],
            )
            .await
            .unwrap();
        assert_eq!(res.status, StatusCode::OK);
        assert_eq!(&res.body[..], b"hello");
        assert_eq!(
            res.headers[header::EXPIRES],
            "Thu, 01 Jan 2026 00:00:00 GMT"
        );
        let request = server.await.unwrap().to_lowercase();
        assert!(request.starts_with("get /path?q=1 http/1.1"), "{request}");
        assert!(request.contains("user-agent: hars/0.1 test"));
        assert!(request.contains("if-modified-since: wed, 31 dec 2025 00:00:00 gmt"));
    }

    #[tokio::test]
    async fn times_out_and_caps_the_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        // Accepts but never answers.
        let _hold = tokio::spawn(async move {
            let (_s, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let client = Client::new("t")
            .unwrap()
            .with_timeouts(Duration::from_millis(200), Duration::from_secs(5));
        assert!(matches!(
            client.get(&url, &[]).await,
            Err(Error::Timeout(_))
        ));

        let (url, _server) =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n0123456789").await;
        let mut client = Client::new("t").unwrap();
        client.max_body = 4;
        assert!(matches!(
            client.get(&url, &[]).await,
            Err(Error::TooLarge(4))
        ));
    }

    /// Headers in time, then the body slower than the headers timeout but
    /// within the total: that works (as in HA); past the total it doesn't.
    #[tokio::test]
    async fn the_body_has_the_total_timeout() {
        async fn slow(delay: Duration) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/", listener.local_addr().unwrap());
            tokio::spawn(async move {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let _ = s.read(&mut buf).await.unwrap();
                s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nab")
                    .await
                    .unwrap();
                tokio::time::sleep(delay).await;
                let _ = s.write_all(b"cd").await;
            });
            url
        }
        let client = Client::new("t")
            .unwrap()
            .with_timeouts(Duration::from_millis(100), Duration::from_millis(1500));
        let res = client
            .get(&slow(Duration::from_millis(400)).await, &[])
            .await
            .unwrap();
        assert_eq!(&res.body[..], b"abcd");
        let client = client.with_timeouts(Duration::from_millis(100), Duration::from_millis(300));
        assert!(matches!(
            client.get(&slow(Duration::from_secs(5)).await, &[]).await,
            Err(Error::Timeout(_))
        ));
    }

    /// A server answering each request from `replies` in turn; returns the
    /// request lines it saw.
    async fn serve(replies: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut seen = Vec::new();
            for reply in replies {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                seen.push(req.lines().next().unwrap_or_default().to_owned());
                s.write_all(reply.as_bytes()).await.unwrap();
            }
            seen
        });
        (base, task)
    }

    #[tokio::test]
    async fn follows_redirects() {
        let moved = |to: &str| {
            format!(
                "HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
        };
        let ok = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_owned();
        let (base, server) = serve(vec![moved("/b/c?x=1"), moved("d"), ok]).await;
        let client = Client::new("t").unwrap();
        let res = client.get(&format!("{base}/a"), &[]).await.unwrap();
        assert_eq!((res.status, &res.body[..]), (StatusCode::OK, &b"ok"[..]));
        assert_eq!(
            server.await.unwrap(),
            [
                "GET /a HTTP/1.1",
                "GET /b/c?x=1 HTTP/1.1",
                "GET /b/d HTTP/1.1"
            ]
        );

        // More than 10 is an error.
        let (base, _server) = serve((0..11).map(|_| moved("/again")).collect()).await;
        assert!(matches!(
            client.get(&format!("{base}/a"), &[]).await,
            Err(Error::Request(e)) if e.contains("redirects")
        ));
    }

    #[test]
    fn redirects_resolve_and_loopback_is_spotted() {
        let base: Uri = "https://api.met.no/weatherapi/x/complete?lat=1"
            .parse()
            .unwrap();
        let r = |l: &str| resolve(&base, l).unwrap().to_string();
        assert_eq!(r("https://other.example/y"), "https://other.example/y");
        assert_eq!(r("//cdn.example/z"), "https://cdn.example/z");
        assert_eq!(r("/abs?q=2"), "https://api.met.no/abs?q=2");
        assert_eq!(r("rel"), "https://api.met.no/weatherapi/x/rel");
        for (u, lo) in [
            ("http://localhost:8123/", true),
            ("http://127.0.0.2/", true),
            ("http://[::1]/", true),
            ("https://api.met.no/", false),
            ("http://10.0.0.2/", false),
        ] {
            assert_eq!(is_loopback(&u.parse().unwrap()), lo, "{u}");
        }
    }
}

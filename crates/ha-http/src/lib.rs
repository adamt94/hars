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

/// aiohttp's `max_redirects`: the 10th redirect is an error.
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
    /// (the 10th is an error) and refusing one from a public host to a
    /// loopback or unspecified address, as HA's SSRF guard. The body is
    /// capped in size.
    pub async fn get(
        &self,
        url: &str,
        headers: &[(header::HeaderName, String)],
    ) -> Result<Response, Error> {
        let start = tokio::time::Instant::now();
        let headers_by = start + self.headers_timeout;
        let total_by = start + self.total_timeout;
        let mut uri: Uri = url.parse().map_err(|_| Error::Url(url.to_owned()))?;
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
            if redirects >= MAX_REDIRECTS {
                return Err(Error::Request(format!("{MAX_REDIRECTS} redirects")));
            }
            let next = resolve(&uri, location).ok_or_else(|| Error::Url(location.to_owned()))?;
            // HA's guard, per hop: a redirect from a host that isn't
            // blocked may not lead to one that is. Name lookups count
            // against the headers deadline.
            let check = async { !blocked(&uri).await && blocked(&next).await };
            if tokio::time::timeout_at(headers_by.min(total_by), check)
                .await
                .map_err(|_| Error::Timeout(self.headers_timeout))?
            {
                return Err(Error::Request(format!(
                    "redirect to a blocked address: {}",
                    next.host().unwrap_or_default()
                )));
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

/// A redirect's `Location`, resolved against the URL that answered with
/// it (RFC 3986 section 5.2). The fragment is dropped: it is never sent.
fn resolve(base: &Uri, location: &str) -> Option<Uri> {
    let r = location.split('#').next().unwrap_or_default();
    // A scheme: a letter, then letters, digits or `+-.`, then `:`.
    let has_scheme = r.find(':').is_some_and(|i| {
        r.as_bytes()[0].is_ascii_alphabetic()
            && r[..i]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    });
    let scheme = base.scheme_str()?;
    let absolute = if has_scheme {
        Some(r.to_owned())
    } else {
        r.strip_prefix("//")
            .map(|rest| format!("{scheme}://{rest}"))
    };
    if let Some(absolute) = absolute {
        // Its own path still loses its dot segments.
        let uri: Uri = absolute.parse().ok()?;
        let (Some(scheme), Some(authority)) = (uri.scheme_str(), uri.authority()) else {
            return Some(uri);
        };
        let query = uri.query().map_or(String::new(), |q| format!("?{q}"));
        let path = remove_dot_segments(uri.path());
        return format!("{scheme}://{authority}{path}{query}").parse().ok();
    }
    let authority = base.authority()?;
    let (path, query) = match r.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (r, None),
    };
    let (path, query) = if path.is_empty() {
        (base.path().to_owned(), query.or(base.query()))
    } else if path.starts_with('/') {
        (remove_dot_segments(path), query)
    } else {
        let dir = base.path().rsplit_once('/').map_or("", |(d, _)| d);
        (remove_dot_segments(&format!("{dir}/{path}")), query)
    };
    let query = query.map_or(String::new(), |q| format!("?{q}"));
    format!("{scheme}://{authority}{path}{query}").parse().ok()
}

/// RFC 3986 section 5.2.4, for an absolute path.
fn remove_dot_segments(path: &str) -> String {
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    let last = segments.len().saturating_sub(1);
    let mut out: Vec<&str> = Vec::new();
    for (i, seg) in segments.iter().enumerate() {
        match *seg {
            "." | ".." => {
                if *seg == ".." {
                    out.pop();
                }
                // A trailing dot segment leaves the path ending in `/`.
                if i == last {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

/// HA's `_async_is_blocked_host`: `localhost` (or a `.localhost` name,
/// trailing dot ignored), a loopback or unspecified address, or a name
/// that resolves to one. A name that doesn't resolve isn't blocked.
async fn blocked(uri: &Uri) -> bool {
    let Some(host) = uri.host() else {
        return false;
    };
    let host = host.trim_matches(['[', ']']);
    let name = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if name == "localhost" || name.ends_with(".localhost") {
        return true;
    }
    let bad = |ip: std::net::IpAddr| {
        let ip = match ip {
            std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, Into::into),
            v4 => v4,
        };
        ip.is_loopback() || ip.is_unspecified()
    };
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return bad(ip);
    }
    match tokio::net::lookup_host((host, 0)).await {
        Ok(mut addrs) => addrs.any(|a| bad(a.ip())),
        Err(_) => false,
    }
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

        // 9 redirects are followed; the 10th is an error, as aiohttp.
        let mut replies: Vec<String> = (0..9).map(|_| moved("/again")).collect();
        replies
            .push("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_owned());
        let (base, _server) = serve(replies).await;
        assert!(client.get(&format!("{base}/a"), &[]).await.is_ok());
        let (base, _server) = serve((0..10).map(|_| moved("/again")).collect()).await;
        assert!(matches!(
            client.get(&format!("{base}/a"), &[]).await,
            Err(Error::Request(e)) if e.contains("redirects")
        ));
    }

    #[tokio::test]
    async fn redirects_resolve_and_blocked_hosts_are_spotted() {
        // RFC 3986 section 5.4's examples.
        let base: Uri = "http://a/b/c/d;p?q".parse().unwrap();
        for (r, want) in [
            ("g", "http://a/b/c/g"),
            ("./g", "http://a/b/c/g"),
            ("g/", "http://a/b/c/g/"),
            ("/g", "http://a/g"),
            ("//g", "http://g/"),
            ("?y", "http://a/b/c/d;p?y"),
            ("g?y", "http://a/b/c/g?y"),
            ("#s", "http://a/b/c/d;p?q"),
            ("", "http://a/b/c/d;p?q"),
            (".", "http://a/b/c/"),
            ("./", "http://a/b/c/"),
            ("..", "http://a/b/"),
            ("../", "http://a/b/"),
            ("../g", "http://a/b/g"),
            ("../..", "http://a/"),
            ("../../g", "http://a/g"),
            ("../../../g", "http://a/g"),
            ("/./g", "http://a/g"),
            ("/../g", "http://a/g"),
            ("g.", "http://a/b/c/g."),
            ("..g", "http://a/b/c/..g"),
            ("./../g", "http://a/b/g"),
            ("g/./h", "http://a/b/c/g/h"),
            ("g/../h", "http://a/b/c/h"),
            ("https://other.example/x?y", "https://other.example/x?y"),
            (
                "https://other.example/x/../f?y",
                "https://other.example/f?y",
            ),
            ("//other.example/x/./../f", "http://other.example/f"),
        ] {
            let got = resolve(&base, r).map(|u| u.to_string());
            assert_eq!(got.as_deref(), Some(want), "{r:?}");
        }
        for (u, is) in [
            ("http://localhost:8123/", true),
            ("http://LOCALHOST./", true),
            ("http://foo.localhost/", true),
            ("http://127.0.0.2/", true),
            ("http://127.1/", true),
            ("http://0.0.0.0/", true),
            ("http://[::1]/", true),
            ("http://[::ffff:127.0.0.1]/", true),
            ("http://10.0.0.2/", false),
            ("http://does-not-resolve.invalid/", false),
        ] {
            assert_eq!(blocked(&u.parse().unwrap()).await, is, "{u}");
        }
    }

    #[tokio::test]
    async fn a_public_host_may_not_redirect_to_loopback() {
        // Served on a non-loopback address of this machine, if it has one.
        let Some(ip) = std::net::UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| s.connect("192.0.2.1:9").map(|()| s))
            .and_then(|s| s.local_addr())
            .ok()
            .map(|a| a.ip())
            .filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
        else {
            return;
        };
        /// Answers each connection with a redirect to `to(n)`.
        async fn redirector(
            listener: tokio::net::TcpListener,
            to: impl Fn(usize) -> String + Send + 'static,
        ) {
            for n in 0.. {
                let Ok((mut s, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 4096];
                let _ = s.read(&mut buf).await;
                let reply = format!(
                    "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    to(n)
                );
                let _ = s.write_all(reply.as_bytes()).await;
            }
        }
        let Ok(public) = tokio::net::TcpListener::bind((ip, 0)).await else {
            return;
        };
        let local = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (public_at, local_port) = (
            public.local_addr().unwrap(),
            local.local_addr().unwrap().port(),
        );
        // The public host sends everyone to `127.1` (loopback, spelt so
        // only a name lookup sees it); the local one sends its first
        // visitor to the public host.
        tokio::spawn(redirector(public, move |_| {
            format!("http://127.1:{local_port}/")
        }));
        tokio::spawn(redirector(local, move |_| format!("http://{public_at}/")));
        let client = Client::new("t").unwrap();
        let blocked = |r: Result<Response, Error>| matches!(r, Err(Error::Request(e)) if e.contains("blocked"));
        assert!(blocked(
            client.get(&format!("http://{public_at}/"), &[]).await
        ));
        // Loopback to public is allowed, but the public host's redirect
        // back to loopback is not: each hop is judged by its own origin.
        assert!(blocked(
            client
                .get(&format!("http://127.0.0.1:{local_port}/"), &[])
                .await
        ));
    }
}

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
    timeout: Duration,
    max_body: usize,
}

impl Client {
    /// `user_agent` identifies `hars` to the services it calls (api.met.no
    /// requires one).
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
            timeout: Duration::from_secs(30),
            max_body: 4 * 1024 * 1024,
        })
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// `GET url` with extra headers; the whole exchange is bounded by the
    /// timeout, and the body by the size cap.
    pub async fn get(
        &self,
        url: &str,
        headers: &[(header::HeaderName, String)],
    ) -> Result<Response, Error> {
        let uri: Uri = url.parse().map_err(|_| Error::Url(url.to_owned()))?;
        let mut req = Request::get(uri)
            .header(header::USER_AGENT, self.user_agent.clone())
            .body(Empty::<Bytes>::new())
            .map_err(|e| Error::Request(e.to_string()))?;
        for (name, value) in headers {
            let value =
                header::HeaderValue::from_str(value).map_err(|e| Error::Request(e.to_string()))?;
            req.headers_mut().insert(name.clone(), value);
        }
        let exchange = async {
            let res = self
                .inner
                .request(req)
                .await
                .map_err(|e| Error::Request(e.to_string()))?;
            let (parts, body) = res.into_parts();
            let body = Limited::new(body, self.max_body)
                .collect()
                .await
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
        };
        tokio::time::timeout(self.timeout, exchange)
            .await
            .map_err(|_| Error::Timeout(self.timeout))?
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
            .with_timeout(Duration::from_millis(200));
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
}

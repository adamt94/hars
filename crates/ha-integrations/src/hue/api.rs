//! The bridge's CLIP v2 API, as aiohue calls it: one client with at most 3
//! connections, no certificate check (HA's `ssl=False`), 429/503 retried 25
//! times, and the errors HA shows a service caller.

use ha_http::{EventStream, StatusCode, header::HeaderName};
use serde_json::Value;
use std::time::Duration;

/// aiohue's `MAX_RETRIES`.
const MAX_RETRIES: u32 = 25;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// A 403: the app key was refused.
    #[error("")]
    Unauthorized,
    /// 25 attempts met 429/503.
    #[error("25 requests to the bridge failed, its probably overloaded. Giving up.")]
    Busy,
    /// The bridge answered with an `errors` entry.
    #[error("{0}")]
    Bridge(String),
    /// Any other status of 400 or more (aiohttp's `ClientResponseError`).
    #[error("{status}, message='{reason}', url='{url}'")]
    Status {
        status: u16,
        reason: String,
        url: String,
    },
    /// Connecting or reading failed, or the body wasn't JSON.
    #[error("{0}")]
    Connection(String),
}

impl ApiError {
    /// HA's `async_request_call` wording for a failed service call; `None`
    /// for the errors it swallows (`may not have effect`).
    pub fn service_message(&self) -> Option<String> {
        match self {
            Self::Bridge(d) if d.contains("may not have effect") => None,
            Self::Unauthorized | Self::Busy | Self::Bridge(_) => {
                Some(format!("Request failed: {self}"))
            }
            Self::Status { .. } | Self::Connection(_) => {
                Some(format!("Request failed due connection error: {self}"))
            }
        }
    }
}

pub struct Api {
    client: ha_http::Client,
    /// `https://<host>`.
    base: String,
    key: String,
}

impl Api {
    pub fn new(host: &str, key: &str) -> Result<Self, ha_http::Error> {
        let client = ha_http::Client::builder(crate::USER_AGENT)
            .danger_accept_any_certificate()
            .max_connections(3)
            .tcp_keepalive()
            .build()?;
        Ok(Self {
            client,
            base: format!("https://{host}"),
            key: key.to_owned(),
        })
    }

    /// Against a test server (`http://127.0.0.1:port`).
    #[cfg(test)]
    pub fn at(base: &str, key: &str) -> Self {
        Self {
            client: ha_http::Client::builder("hars-test")
                .max_connections(3)
                .build()
                .unwrap(),
            base: base.to_owned(),
            key: key.to_owned(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base)
    }

    fn headers(&self) -> Vec<(HeaderName, String)> {
        vec![(
            HeaderName::from_static("hue-application-key"),
            self.key.clone(),
        )]
    }

    /// aiohue's `request`: retry 429/503 (sleeping 0.25·n s before attempt
    /// n), then map errors; returns the `data`.
    async fn request(&self, path: &str, body: Option<&Value>) -> Result<Value, ApiError> {
        let url = self.url(path);
        for attempt in 1..=MAX_RETRIES {
            if attempt > 1 {
                tokio::time::sleep(Duration::from_millis(250 * u64::from(attempt))).await;
            }
            let res = match body {
                Some(b) => self.client.put_json(&url, &self.headers(), b).await,
                None => self.client.get(&url, &self.headers()).await,
            }
            .map_err(|e| ApiError::Connection(e.to_string()))?;
            match res.status {
                StatusCode::SERVICE_UNAVAILABLE | StatusCode::TOO_MANY_REQUESTS => continue,
                StatusCode::FORBIDDEN => return Err(ApiError::Unauthorized),
                s if s.as_u16() >= 400 => {
                    return Err(ApiError::Status {
                        status: s.as_u16(),
                        reason: s.canonical_reason().unwrap_or_default().to_owned(),
                        url,
                    });
                }
                _ => {}
            }
            let mut json: Value = serde_json::from_slice(&res.body)
                .map_err(|e| ApiError::Connection(format!("invalid JSON from the bridge: {e}")))?;
            if let Some(first) = json["errors"].as_array().and_then(|e| e.first()) {
                let d = first["description"].as_str().unwrap_or_default();
                return Err(ApiError::Bridge(d.to_owned()));
            }
            return Ok(json["data"].take());
        }
        Err(ApiError::Busy)
    }

    /// `GET clip/v2/resource`: every resource.
    pub async fn fetch_all(&self) -> Result<Vec<Value>, ApiError> {
        match self.request("clip/v2/resource", None).await? {
            Value::Array(items) => Ok(items),
            _ => Ok(Vec::new()),
        }
    }

    /// `PUT clip/v2/resource/{type}/{id}`.
    pub async fn put(&self, ty: &str, id: &str, body: &Value) -> Result<(), ApiError> {
        self.request(&format!("clip/v2/resource/{ty}/{id}"), Some(body))
            .await
            .map(|_| ())
    }

    /// The event stream, resuming after `last_event_id` if there is one.
    /// As aiohue, its status isn't checked.
    pub async fn events(&self, last_event_id: Option<&str>) -> Result<EventStream, ha_http::Error> {
        let mut headers = self.headers();
        headers.push((ha_http::header::ACCEPT, "text/event-stream".into()));
        headers.push((ha_http::header::CACHE_CONTROL, "no-cache".into()));
        if let Some(id) = last_event_id {
            headers.push((
                HeaderName::from_static("last-event-id"),
                id.trim().to_owned(),
            ));
        }
        self.client
            .stream(&self.url("eventstream/clip/v2"), &headers)
            .await
    }
}

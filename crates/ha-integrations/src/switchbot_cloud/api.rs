//! The SwitchBot Cloud API v1.1, as `switchbot-api` 2.11.0 speaks it:
//! every request signed, one attempt each, the `statusCode` envelope
//! classified into the library's errors.
//!
//! Where `hars` differs on purpose: an error is logged with its HTTP
//! status and API `statusCode` only. The library logs the whole reply,
//! which can carry the account's token back.

use base64::Engine;
use ha_http::{Method, header::HeaderName};
use hmac::{Hmac, Mac};
use serde_json::{Map, Value};
use std::time::Duration;

/// The API's host (`switchbot_api._API_HOST`).
pub const HOST: &str = "https://api.switch-bot.com";

/// The coordinator's `asyncio.timeout(10)` around a status fetch.
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(10);

/// What the library raises, by class.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    /// `SwitchBotConnectionError`: a transport failure (with the library's
    /// message), or an error status or `statusCode` (empty message).
    #[error("{0}")]
    Connection(String),
    /// `SwitchBotAuthenticationError` (HTTP 403).
    #[error("")]
    Auth,
    /// `SwitchBotDeviceOfflineError` (`statusCode` 161 or 171).
    #[error("")]
    Offline,
    /// Anything the library lets escape: a body that isn't JSON (aiohttp's
    /// `ContentTypeError`), invalid JSON, or JSON of the wrong shape.
    #[error("{0}")]
    Other(String),
    /// The status fetch's 10 s timeout.
    #[error("")]
    Timeout,
}

const CONNECTION: &str = "Error occurred while communicating with the SwitchBot API";

/// `make_headers`: `(t, sign, nonce)` for a token and secret at `t` ms.
pub fn sign(token: &str, secret: &str, t_ms: i64, nonce: &str) -> String {
    let mut mac =
        Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key length");
    mac.update(token.as_bytes());
    mac.update(t_ms.to_string().as_bytes());
    mac.update(nonce.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// `str(uuid.uuid4())`.
pub fn uuid4() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// aiohttp's JSON content-type check (`application/json` or `*/*+json`).
fn is_json(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    mime == "application/json"
        || mime
            .split_once('/')
            .is_some_and(|(_, sub)| sub.ends_with("+json"))
}

pub struct Api {
    client: ha_http::Client,
    host: String,
    token: String,
    secret: String,
}

impl Api {
    pub fn new(client: ha_http::Client, host: String, token: String, secret: String) -> Self {
        Self {
            client,
            host,
            token,
            secret,
        }
    }

    fn headers(&self) -> Vec<(HeaderName, String)> {
        let t = jiff::Timestamp::now().as_millisecond();
        let nonce = uuid4();
        let sign = sign(&self.token, &self.secret, t, &nonce);
        let h = |n: &'static str| HeaderName::from_static(n);
        vec![
            (h("authorization"), self.token.clone()),
            (h("content-type"), "application/json".into()),
            (h("charset"), "utf8".into()),
            (h("t"), t.to_string()),
            (h("sign"), sign),
            (h("nonce"), nonce),
        ]
    }

    /// `_request`: one attempt, the envelope's `body` on `statusCode` 100.
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<String>,
    ) -> Result<Value, ApiError> {
        let url = format!("{}/v1.1/{path}", self.host);
        let res = self
            .client
            .request(method, &url, &self.headers(), body.map(Into::into))
            .await
            .map_err(|_| ApiError::Connection(CONNECTION.into()))?;
        let status = res.status.as_u16();
        if status == 403 {
            return Err(ApiError::Auth);
        }
        let content_type = res
            .headers
            .get(ha_http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !is_json(content_type) {
            let mime = content_type.split(';').next().unwrap_or("").trim();
            let mime = if mime.is_empty() { "none" } else { mime };
            return Err(ApiError::Other(format!(
                "{status}, message='Attempt to decode JSON with unexpected mimetype: {mime}', url='{url}'"
            )));
        }
        let json: Value = serde_json::from_slice(&res.body)
            .map_err(|e| ApiError::Other(format!("invalid JSON: {e}")))?;
        let Value::Object(envelope) = json else {
            return Err(ApiError::Other(format!(
                "'{}' object has no attribute 'get'",
                py_type(&json)
            )));
        };
        if status >= 400 {
            log_error(status, &envelope);
            return Err(ApiError::Connection(String::new()));
        }
        match envelope.get("statusCode").and_then(Value::as_i64) {
            // `body["body"]`.
            Some(100) => envelope
                .get("body")
                .cloned()
                .ok_or_else(|| ApiError::Other("KeyError: 'body'".into())),
            Some(161 | 171) => Err(ApiError::Offline),
            _ => {
                log_error(status, &envelope);
                Err(ApiError::Connection(String::new()))
            }
        }
    }

    /// `GET devices`: the devices, then the remotes (but `Others`).
    pub async fn list_devices(&self) -> Result<Vec<Device>, ApiError> {
        let body = self.request(Method::GET, "devices", None).await?;
        let mut out = Vec::new();
        for (key, remote) in [("deviceList", false), ("infraredRemoteList", true)] {
            // `for device in body.get(key)`: a missing list can't be walked.
            let list = body.get(key).and_then(Value::as_array).ok_or_else(|| {
                ApiError::Other("TypeError: 'NoneType' object is not iterable".into())
            })?;
            for d in list {
                let field = |k: &str| {
                    d.get(k)
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .ok_or_else(|| ApiError::Other(format!("KeyError: '{k}'")))
                };
                let kind = match remote {
                    false => d
                        .get("deviceType")
                        .and_then(Value::as_str)
                        .unwrap_or("-")
                        .to_owned(),
                    true => field("remoteType")?,
                };
                if remote && kind == "Others" {
                    continue;
                }
                out.push(Device {
                    id: field("deviceId")?,
                    name: field("deviceName")?,
                    kind,
                    remote,
                });
            }
        }
        Ok(out)
    }

    /// `GET devices/{id}/status`, within the coordinator's 10 s.
    pub async fn status(&self, id: &str) -> Result<Map<String, Value>, ApiError> {
        let path = format!("devices/{id}/status");
        match tokio::time::timeout(STATUS_TIMEOUT, self.request(Method::GET, &path, None)).await {
            Err(_) => Err(ApiError::Timeout),
            // `response["deviceType"]`.
            Ok(Ok(Value::Object(m))) if m.contains_key("deviceType") => Ok(m),
            // An empty body: the library's keypad check reads `deviceType`.
            Ok(Ok(_)) => Err(ApiError::Other("KeyError: 'deviceType'".into())),
            Ok(Err(e)) => Err(e),
        }
    }

    /// `POST devices/{id}/commands`, the body as the library sends it.
    pub async fn command(&self, id: &str, command: &str, parameter: &str) -> Result<(), ApiError> {
        let body = format!(
            r#"{{"commandType":"command","command":{},"parameter":{}}}"#,
            Value::from(command),
            Value::from(parameter)
        );
        let path = format!("devices/{id}/commands");
        self.request(Method::POST, &path, Some(body))
            .await
            .map(drop)
    }
}

/// The library's `_LOGGER.error("Error %d: %s", status, body)`, without
/// the body: an API's error can echo the request back, the token with it,
/// and a log is read by more people than a config entry. What is kept is
/// the HTTP status and the envelope's `statusCode` when it is a number.
fn log_error(status: u16, envelope: &Map<String, Value>) {
    match envelope.get("statusCode").and_then(Value::as_i64) {
        Some(code) => tracing::error!("Error {status}: statusCode {code}"),
        None => tracing::error!("Error {status}"),
    }
}

fn py_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// A device or remote from the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// `deviceType`, or a remote's `remoteType`.
    pub kind: String,
    pub remote: bool,
}

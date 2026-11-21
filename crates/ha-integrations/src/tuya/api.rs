//! The Tuya "device sharing" cloud API, as `tuya-device-sharing-sdk`
//! (`tuya_sharing/customerapi.py`) speaks it: every request carries an
//! HMAC-SHA256 signature, query and body are AES-128-GCM encrypted, and the
//! `result` of every answer comes back encrypted with the same key.
//!
//! The access token lasts two hours. It is refreshed with the refresh token
//! a minute before it runs out, and the new pair is handed to
//! [`Api::on_token`] so the config entry keeps it (Tuya rotates the refresh
//! token: an old one stops working).

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ha_http::{Method, header::HeaderName};
use hmac::{Hmac, Mac};
use md5::Digest;
use parking_lot::Mutex;
use ring::aead::{AES_128_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// `const.TUYA_CLIENT_ID` in HA's integration.
pub const CLIENT_ID: &str = "HA_3y9q4ak7g4ephrvke";

#[derive(Debug, Clone, thiserror::Error)]
pub enum ApiError {
    /// The request never got an answer, or an HTTP error status.
    #[error("{0}")]
    Connection(String),
    /// `success: false`: Tuya's code and message.
    #[error("{code}: {msg}")]
    Api { code: String, msg: String },
    /// An answer that can't be read.
    #[error("{0}")]
    Other(String),
}

/// `CustomerTokenInfo`, as stored in the entry's `token_info`.
#[derive(Debug, Clone, Default)]
pub struct Token {
    pub t: i64,
    pub expire_time: i64,
    pub uid: String,
    pub access_token: String,
    pub refresh_token: String,
}

impl Token {
    pub fn from_value(v: &Value) -> Self {
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
        let n = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
        Self {
            t: n("t"),
            expire_time: n("expire_time"),
            uid: s("uid"),
            access_token: s("access_token"),
            refresh_token: s("refresh_token"),
        }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "t": self.t,
            "expire_time": self.expire_time,
            "uid": self.uid,
            "access_token": self.access_token,
            "refresh_token": self.refresh_token,
        })
    }

    /// When the access token runs out, in ms since the epoch.
    fn expires_at(&self) -> i64 {
        self.t + self.expire_time * 1000
    }
}

type TokenListener = Box<dyn Fn(&Token) + Send + Sync>;

pub struct Api {
    client: ha_http::Client,
    /// `hass.clock()`: token expiry and request times follow it.
    clock: ha_core::Clock,
    endpoint: String,
    token: Mutex<Token>,
    /// One refresh at a time.
    refreshing: tokio::sync::Mutex<()>,
    on_token: Mutex<Option<TokenListener>>,
}

/// `_random_nonce`.
fn nonce12() -> [u8; 12] {
    const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTWXYZabcdefhijkmnprstwxyz2345678";
    let mut out = [0u8; 12];
    for b in &mut out {
        *b = ALPHABET[rand::random_range(0..ALPHABET.len())];
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `_secret_generating` with an empty `sid`: the first 16 hex characters
/// of HMAC-SHA256(rid, hash_key), used as an AES-128 key.
pub fn secret(rid: &str, hash_key: &str) -> String {
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(rid.as_bytes()).expect("any key length");
    mac.update(hash_key.as_bytes());
    hex(&mac.finalize().into_bytes())[..16].to_owned()
}

/// `_aes_gcm_encrypt`: base64(nonce) followed by base64(ciphertext+tag).
pub fn encrypt(plain: &str, secret: &str) -> String {
    let nonce = nonce12();
    let key = LessSafeKey::new(UnboundKey::new(&AES_128_GCM, secret.as_bytes()).expect("16 bytes"));
    let mut data = plain.as_bytes().to_vec();
    key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut data)
        .expect("seal");
    format!("{}{}", B64.encode(nonce), B64.encode(data))
}

/// `_aex_gcm_decrypt`: base64 of nonce followed by ciphertext+tag.
pub fn decrypt(cipher: &str, secret: &str) -> Result<String, ApiError> {
    let raw = B64
        .decode(cipher)
        .map_err(|e| ApiError::Other(format!("result is not base64: {e}")))?;
    if raw.len() < 12 + 16 {
        return Err(ApiError::Other("result too short".into()));
    }
    let (nonce, rest) = raw.split_at(12);
    let key = LessSafeKey::new(UnboundKey::new(&AES_128_GCM, secret.as_bytes()).expect("16 bytes"));
    let mut data = rest.to_vec();
    let nonce =
        Nonce::try_assume_unique_for_key(nonce).map_err(|_| ApiError::Other("nonce".into()))?;
    let plain = key
        .open_in_place(nonce, Aad::empty(), &mut data)
        .map_err(|_| ApiError::Other("result does not decrypt".into()))?;
    String::from_utf8(plain.to_vec()).map_err(|e| ApiError::Other(e.to_string()))
}

/// `_restful_sign`.
pub fn sign(hash_key: &str, headers: &[(&str, &str)], query: &str, body: &str) -> String {
    let mut s = headers
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("||");
    s.push_str(query);
    s.push_str(body);
    let mut mac =
        Hmac::<sha2::Sha256>::new_from_slice(hash_key.as_bytes()).expect("any key length");
    mac.update(s.as_bytes());
    hex(&mac.finalize().into_bytes())
}

fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3 / 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl Api {
    fn now_ms(&self) -> i64 {
        self.clock.now().as_millisecond()
    }

    pub fn new(
        client: ha_http::Client,
        clock: ha_core::Clock,
        endpoint: String,
        token: Token,
    ) -> Arc<Self> {
        Arc::new(Self {
            client,
            clock,
            endpoint,
            token: Mutex::new(token),
            refreshing: tokio::sync::Mutex::new(()),
            on_token: Mutex::new(None),
        })
    }

    /// Called with every new token pair.
    pub fn on_token(&self, f: impl Fn(&Token) + Send + Sync + 'static) {
        *self.on_token.lock() = Some(Box::new(f));
    }

    pub fn token(&self) -> Token {
        self.token.lock().clone()
    }

    pub async fn get(&self, path: &str, params: Option<Value>) -> Result<Value, ApiError> {
        self.refresh_if_needed().await;
        self.request(Method::GET, path, params, None).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value, ApiError> {
        self.refresh_if_needed().await;
        self.request(Method::POST, path, None, Some(body)).await
    }

    /// `refresh_access_token_if_need`: a failure is logged and the old
    /// token tried anyway, as the SDK does.
    async fn refresh_if_needed(&self) {
        if self.token.lock().expires_at() - 60_000 > self.now_ms() {
            return;
        }
        let _one = self.refreshing.lock().await;
        if self.token.lock().expires_at() - 60_000 > self.now_ms() {
            return;
        }
        let previous = self.token.lock().clone();
        let refresh = previous.refresh_token.clone();
        match self
            .request(Method::GET, &format!("/v1.0/m/token/{refresh}"), None, None)
            .await
        {
            Ok(ret) => {
                let r = &ret["result"];
                let token = Token {
                    t: ret
                        .get("t")
                        .and_then(Value::as_i64)
                        .unwrap_or_else(|| self.now_ms()),
                    expire_time: r.get("expireTime").and_then(Value::as_i64).unwrap_or(7200),
                    // A refresh that leaves the uid out keeps the old one.
                    uid: r
                        .get("uid")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or(previous.uid),
                    access_token: r
                        .get("accessToken")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    refresh_token: r
                        .get("refreshToken")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                };
                if token.access_token.is_empty() || token.refresh_token.is_empty() {
                    tracing::error!("tuya: the token refresh answered without a token");
                    return;
                }
                *self.token.lock() = token.clone();
                tracing::debug!("tuya: access token refreshed");
                if let Some(f) = &*self.on_token.lock() {
                    f(&token);
                }
            }
            Err(e) => tracing::error!("tuya: token refresh failed: {e}"),
        }
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        params: Option<Value>,
        body: Option<Value>,
    ) -> Result<Value, ApiError> {
        let (access, refresh) = {
            let t = self.token.lock();
            (t.access_token.clone(), t.refresh_token.clone())
        };
        let rid = crate::switchbot_cloud::api::uuid4();
        let hash_key = hex(&md5::Md5::digest(format!("{rid}{refresh}").as_bytes()));
        let secret = secret(&rid, &hash_key);
        let nonempty = |v: &Option<Value>| {
            v.as_ref()
                .and_then(Value::as_object)
                .is_some_and(|m| !m.is_empty())
        };
        let query = if nonempty(&params) {
            encrypt(&params.unwrap().to_string(), &secret)
        } else {
            String::new()
        };
        let body_enc = if nonempty(&body) {
            encrypt(&body.unwrap().to_string(), &secret)
        } else {
            String::new()
        };
        let t = self.now_ms().to_string();
        let sign = sign(
            &hash_key,
            &[
                ("X-appKey", CLIENT_ID),
                ("X-requestId", &rid),
                ("X-sid", ""),
                ("X-time", &t),
                ("X-token", &access),
            ],
            &query,
            &body_enc,
        );
        let h = |name: &'static str| HeaderName::from_static(name);
        let mut headers = vec![
            (h("x-appkey"), CLIENT_ID.to_owned()),
            (h("x-requestid"), rid.clone()),
            (h("x-sid"), String::new()),
            (h("x-time"), t),
            (h("x-sign"), sign),
        ];
        if !access.is_empty() {
            headers.push((h("x-token"), access));
        }
        let mut url = format!("{}{path}", self.endpoint);
        if !query.is_empty() {
            url.push_str("?encdata=");
            url.push_str(&form_encode(&query));
        }
        let payload = if body_enc.is_empty() {
            None
        } else {
            headers.push((ha_http::header::CONTENT_TYPE, "application/json".into()));
            Some(json!({"encdata": body_enc}).to_string().into())
        };
        let res = self
            .client
            .request(method, &url, &headers, payload)
            .await
            .map_err(|e| ApiError::Connection(e.to_string()))?;
        if !res.status.is_success() {
            return Err(ApiError::Connection(format!("HTTP {}", res.status)));
        }
        let mut ret: Map<String, Value> =
            serde_json::from_slice(&res.body).map_err(|e| ApiError::Other(e.to_string()))?;
        if ret.get("success").and_then(Value::as_bool) != Some(true) {
            let s = |k: &str| match ret.get(k) {
                Some(Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => String::new(),
            };
            return Err(ApiError::Api {
                code: s("code"),
                msg: s("msg"),
            });
        }
        if let Some(Value::String(enc)) = ret.get("result")
            && !enc.is_empty()
        {
            let plain = decrypt(enc, &secret)?;
            let result = serde_json::from_str(&plain).unwrap_or(Value::String(plain));
            ret.insert("result".into(), result);
        }
        Ok(Value::Object(ret))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let s = secret("rid", "key");
        assert_eq!(s.len(), 16);
        let enc = encrypt(r#"{"a":1}"#, &s);
        // The SDK sends b64(nonce)+b64(ct) and reads b64(nonce+ct).
        let (n, c) = enc.split_at(16);
        let mut raw = B64.decode(n).unwrap();
        raw.extend(B64.decode(c).unwrap());
        assert_eq!(decrypt(&B64.encode(raw), &s).unwrap(), r#"{"a":1}"#);
    }

    /// Vectors from `tuya_sharing.customerapi` itself.
    #[test]
    fn matches_the_sdk() {
        let rid = "11111111-2222-4333-8444-555555555555";
        let hk = hex(&md5::Md5::digest(format!("{rid}REFRESH").as_bytes()));
        assert_eq!(hk, "2f7fe36b7eba60cbd7414a7af3eab191");
        let sec = secret(rid, &hk);
        assert_eq!(sec, "f32d29d3ebbe30bd");
        let headers = [
            ("X-appKey", CLIENT_ID),
            ("X-requestId", rid),
            ("X-sid", ""),
            ("X-time", "123"),
            ("X-token", "TOK"),
        ];
        assert_eq!(
            sign(&hk, &headers, "QQ", "BB"),
            "e31a9bdce906b4dc674f57bb2bb9ad30b2f0d47f13f0ec4ff3a8e551b652c9fc"
        );
        assert_eq!(
            decrypt("QUJDREVGR0hKS01OMWmAdJbx69lWu83TjvRuhWe6nNUrImCEy7st", &sec).unwrap(),
            r#"{"ok":true}"#
        );
    }

    #[test]
    fn sign_skips_empty_headers() {
        let a = sign("k", &[("X-a", "1"), ("X-sid", ""), ("X-b", "2")], "q", "b");
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(b"k").unwrap();
        mac.update(b"X-a=1||X-b=2qb");
        assert_eq!(a, hex(&mac.finalize().into_bytes()));
    }
}

//! The HS256 access tokens HA issues (`auth/__init__.py`, `auth/jwt_wrapper.py`).
//!
//! Claims are exactly `{"iss": <refresh token id>, "iat", "exp"}`. The key is
//! the refresh token's `jwt_key`: a hex string whose ASCII bytes are the HMAC
//! key (not hex-decoded), as PyJWT uses it.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// PyJWT's header for HS256, serialized with sorted keys.
const HEADER: &str = r#"{"alg":"HS256","typ":"JWT"}"#;
/// `jwt_wrapper.py`: longer tokens are rejected before any parsing.
pub(crate) const MAX_TOKEN_SIZE: usize = 8192;
/// PyJWT `leeway` HA passes when verifying `exp` and `iat`.
const LEEWAY_SECS: i64 = 10;

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Claims {
    pub iss: String,
    pub iat: i64,
    pub exp: i64,
}

pub(crate) fn encode(claims: &Claims, key: &str) -> String {
    let header = B64.encode(HEADER);
    let payload = B64.encode(serde_json::to_vec(claims).expect("claims serialize"));
    let mut token = format!("{header}.{payload}");
    let sig = mac(key, token.as_bytes()).finalize().into_bytes();
    token.push('.');
    token.push_str(&B64.encode(sig));
    token
}

/// The `iss` claim, read without checking the signature: it names the
/// refresh token whose key verifies the rest.
pub(crate) fn unverified_issuer(token: &str) -> Option<String> {
    if token.len() > MAX_TOKEN_SIZE {
        return None;
    }
    let mut parts = token.split('.');
    let (_, payload) = (parts.next()?, parts.next()?);
    #[derive(serde::Deserialize)]
    struct Iss {
        iss: String,
    }
    let iss: Iss = serde_json::from_slice(&B64.decode(payload).ok()?).ok()?;
    Some(iss.iss)
}

/// Checks the header, signature, `exp` and `iat` (both required), with HA's
/// 10 s leeway. Returns the claims of a valid token.
pub(crate) fn verify(token: &str, key: &str, now: i64) -> Option<Claims> {
    if token.len() > MAX_TOKEN_SIZE {
        return None;
    }
    let mut parts = token.splitn(3, '.');
    let (header, payload, sig) = (parts.next()?, parts.next()?, parts.next()?);

    #[derive(serde::Deserialize)]
    struct Header {
        alg: String,
    }
    let h: Header = serde_json::from_slice(&B64.decode(header).ok()?).ok()?;
    if h.alg != "HS256" {
        return None;
    }
    let sig = B64.decode(sig).ok()?;
    let signed = &token[..header.len() + 1 + payload.len()];
    mac(key, signed.as_bytes()).verify_slice(&sig).ok()?;

    let claims: Claims = serde_json::from_slice(&B64.decode(payload).ok()?).ok()?;
    if now > claims.exp + LEEWAY_SECS || claims.iat > now + LEEWAY_SECS {
        return None;
    }
    Some(claims)
}

fn mac(key: &str, data: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC takes any key");
    mac.update(data);
    mac
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Made with Python's `hmac`/`base64`, independently of this code.
    const PYTHON_TOKEN: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJhYmMxMjMiLCJpYXQiOjE3MDAwMDAwMDAsImV4cCI6MTcwMDAwMTgwMH0.ox-bO6wH4JX6aKZRFUPJIEiZXkioIPaLTh8Zwvfqx88";

    fn key() -> String {
        "k".repeat(128)
    }

    fn claims() -> Claims {
        Claims {
            iss: "abc123".into(),
            iat: 1_700_000_000,
            exp: 1_700_001_800,
        }
    }

    #[test]
    fn matches_an_independent_implementation() {
        assert_eq!(encode(&claims(), &key()), PYTHON_TOKEN);
        let c = verify(PYTHON_TOKEN, &key(), 1_700_000_100).unwrap();
        assert_eq!(
            (c.iss.as_str(), c.iat, c.exp),
            ("abc123", 1_700_000_000, 1_700_001_800)
        );
        assert_eq!(unverified_issuer(PYTHON_TOKEN).as_deref(), Some("abc123"));
    }

    #[test]
    fn rejects_wrong_key_tampering_and_bad_times() {
        let t = encode(&claims(), &key());
        let ok_now = 1_700_000_100;
        assert!(verify(&t, &"j".repeat(128), ok_now).is_none());

        // Same signature over a different payload.
        let mut parts: Vec<&str> = t.split('.').collect();
        let other = B64.encode(br#"{"iss":"other","iat":1700000000,"exp":1700001800}"#);
        parts[1] = &other;
        assert!(verify(&parts.join("."), &key(), ok_now).is_none());

        // `exp` with HA's 10 s leeway.
        assert!(verify(&t, &key(), 1_700_001_810).is_some());
        assert!(verify(&t, &key(), 1_700_001_811).is_none());
        // `iat` in the future beyond the leeway.
        assert!(verify(&t, &key(), 1_699_999_990).is_some());
        assert!(verify(&t, &key(), 1_699_999_989).is_none());

        // Missing `exp` is invalid even with a good signature.
        let no_exp = {
            let h = B64.encode(HEADER);
            let p = B64.encode(br#"{"iss":"abc123","iat":1700000000}"#);
            let s = B64.encode(
                mac(&key(), format!("{h}.{p}").as_bytes())
                    .finalize()
                    .into_bytes(),
            );
            format!("{h}.{p}.{s}")
        };
        assert!(verify(&no_exp, &key(), ok_now).is_none());

        // `alg: none` and oversized tokens.
        let none = format!(
            "{}.{}.",
            B64.encode(r#"{"alg":"none"}"#),
            t.split('.').nth(1).unwrap()
        );
        assert!(verify(&none, &key(), ok_now).is_none());
        let huge = format!("{t}{}", "A".repeat(MAX_TOKEN_SIZE));
        assert!(verify(&huge, &key(), ok_now).is_none());
        assert!(unverified_issuer(&huge).is_none());
    }
}

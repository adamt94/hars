//! `Context`: who or what caused a state change, event or service call.
//!
//! Mirrors HA's `homeassistant.core.Context`: a ULID `id`, the `parent_id` of
//! the context that triggered it (an automation run's parent is its trigger
//! event's context) and the `user_id` behind it, if any. Two contexts are the
//! same context when their ids are equal.

use jiff::Timestamp;
use serde::{Serialize, Serializer};
use smol_str::SmolStr;
use std::fmt;

/// A ULID: 48-bit Unix milliseconds then 80 random bits, kept as a `u128`
/// so it is `Copy` and never allocates. It serializes as the usual
/// 26-character Crockford base32 string, as HA's `ulid_now()` does.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContextId(u128);

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

impl ContextId {
    /// A fresh id stamped with `now`.
    pub fn at(now: Timestamp) -> Self {
        let ms = u128::try_from(now.as_millisecond()).unwrap_or(0) & ((1 << 48) - 1);
        let random = rand::random::<u128>() & ((1 << 80) - 1);
        Self((ms << 80) | random)
    }

    /// The 16 big-endian bytes HA stores (`ulid_to_bytes`).
    pub fn to_bytes(self) -> [u8; 16] {
        self.0.to_be_bytes()
    }

    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(u128::from_be_bytes(bytes))
    }

    fn encode(self) -> [u8; 26] {
        let mut out = [0u8; 26];
        for (i, c) in out.iter_mut().enumerate() {
            // 26 × 5 = 130 bits: the first character carries the top 3.
            *c = CROCKFORD[((self.0 >> (125 - 5 * i)) & 31) as usize];
        }
        out
    }
}

impl fmt::Display for ContextId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let buf = self.encode();
        // Crockford base32 is ASCII.
        f.write_str(std::str::from_utf8(&buf).unwrap_or_default())
    }
}

/// Not a 26-character ULID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid ULID")]
pub struct InvalidUlid;

impl std::str::FromStr for ContextId {
    type Err = InvalidUlid;

    /// The 26-character Crockford form, either case. The first character
    /// carries 3 bits, so it is at most `7`.
    fn from_str(s: &str) -> Result<Self, InvalidUlid> {
        let bytes = s.as_bytes();
        if bytes.len() != 26 {
            return Err(InvalidUlid);
        }
        let mut v: u128 = 0;
        for (i, &b) in bytes.iter().enumerate() {
            let d = CROCKFORD
                .iter()
                .position(|&c| c == b.to_ascii_uppercase())
                .ok_or(InvalidUlid)? as u128;
            if i == 0 && d > 7 {
                return Err(InvalidUlid);
            }
            v = (v << 5) | d;
        }
        Ok(Self(v))
    }
}

impl fmt::Debug for ContextId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContextId({self})")
    }
}

impl Serialize for ContextId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let buf = self.encode();
        s.serialize_str(std::str::from_utf8(&buf).unwrap_or_default())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Context {
    pub id: ContextId,
    pub parent_id: Option<ContextId>,
    pub user_id: Option<SmolStr>,
}

impl Context {
    /// A new root context with no user, like HA's `Context()`.
    pub fn new() -> Self {
        Self::at(Timestamp::now())
    }

    /// A new root context whose id is stamped with `now`.
    pub fn at(now: Timestamp) -> Self {
        Self {
            id: ContextId::at(now),
            parent_id: None,
            user_id: None,
        }
    }

    pub fn with_user(mut self, user_id: impl Into<SmolStr>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    pub fn with_parent(mut self, parent: &Context) -> Self {
        self.parent_id = Some(parent.id);
        self
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

/// Same rule as HA: contexts are equal when their ids are.
impl PartialEq for Context {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Context {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn id_is_a_ulid_stamped_with_the_time() {
        // 1_469_918_176_385 ms is the ULID spec's example: "01ARYZ6S41…".
        let now = Timestamp::from_millisecond(1_469_918_176_385).unwrap();
        let id = ContextId::at(now).to_string();
        assert_eq!(id.len(), 26);
        assert!(id.starts_with("01ARYZ6S41"), "{id}");
        assert!(id.bytes().all(|b| CROCKFORD.contains(&b)));
        assert_ne!(
            ContextId::at(now),
            ContextId::at(now),
            "random part differs"
        );
    }

    #[test]
    fn encodes_every_bit() {
        assert_eq!(ContextId(0).to_string(), "0".repeat(26));
        assert_eq!(
            ContextId(u128::MAX).to_string(),
            format!("7{}", "Z".repeat(25))
        );
        assert_eq!(
            ContextId(1 << 125).to_string(),
            format!("1{}", "0".repeat(25))
        );
    }

    #[test]
    fn parses_and_stores_as_ha_does() {
        let id: ContextId = "01KF0CD6T0000000000000002E".parse().unwrap();
        assert_eq!(id.to_string(), "01KF0CD6T0000000000000002E");
        assert_eq!(
            "01kf0cd6t0000000000000002e".parse::<ContextId>(),
            Ok(id),
            "either case"
        );
        // `ulid_to_bytes("01KF0CD6T0000000000000002E").hex()` in HA's venv.
        let hex: String = id.to_bytes().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "019bc0c69b400000000000000000004e");
        assert_eq!(ContextId::from_bytes(id.to_bytes()), id);
        for bad in [
            "",
            "01KF0CD6T0000000000000002",
            "81KF0CD6T0000000000000002E",
            "01KF0CD6T000000000000000UE",
        ] {
            assert_eq!(bad.parse::<ContextId>(), Err(InvalidUlid), "{bad}");
        }
        let fresh = ContextId::at(Timestamp::now());
        assert_eq!(fresh.to_string().parse::<ContextId>(), Ok(fresh));
    }

    #[test]
    fn serializes_like_ha() {
        let parent = Context::new();
        let ctx = Context::new().with_user("abc123").with_parent(&parent);
        assert_eq!(
            serde_json::to_value(&ctx).unwrap(),
            json!({
                "id": ctx.id.to_string(),
                "parent_id": parent.id.to_string(),
                "user_id": "abc123",
            })
        );
        assert_eq!(
            serde_json::to_value(Context::new()).unwrap()["parent_id"],
            json!(null)
        );
    }

    #[test]
    fn equality_is_by_id() {
        let a = Context::new();
        let b = a.clone().with_user("someone");
        assert_eq!(a, b);
        assert_ne!(a, Context::new());
    }
}

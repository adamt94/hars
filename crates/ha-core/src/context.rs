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

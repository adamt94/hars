use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use std::fmt;

/// `domain.object_id`, e.g. `light.kitchen`.
///
/// Backed by [`SmolStr`]: ids up to 23 bytes are stored inline (no heap),
/// and clones of longer ids are a refcount bump.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "SmolStr", into = "SmolStr")]
pub struct EntityId {
    raw: SmolStr,
    dot: u8,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EntityIdError {
    #[error("entity id must be `domain.object_id`: {0:?}")]
    Malformed(String),
}

impl EntityId {
    pub fn parse(s: &str) -> Result<Self, EntityIdError> {
        let err = || EntityIdError::Malformed(s.to_owned());
        let dot = s.find('.').ok_or_else(err)?;
        let (domain, object_id) = (&s[..dot], &s[dot + 1..]);
        let valid = |p: &str| {
            !p.is_empty()
                && !p.starts_with('_')
                && !p.ends_with('_')
                && p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        };
        // `(?!.+__)`: no double underscore anywhere (HA's `VALID_ENTITY_ID`).
        if !valid(domain) || !valid(object_id) || s.contains("__") || dot > u8::MAX as usize {
            return Err(err());
        }
        Ok(Self {
            raw: SmolStr::new(s),
            dot: dot as u8,
        })
    }

    pub fn domain(&self) -> &str {
        &self.raw[..self.dot as usize]
    }

    pub fn object_id(&self) -> &str {
        &self.raw[self.dot as usize + 1..]
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl TryFrom<SmolStr> for EntityId {
    type Error = EntityIdError;
    fn try_from(s: SmolStr) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<EntityId> for SmolStr {
    fn from(id: EntityId) -> Self {
        id.raw
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl fmt::Debug for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EntityId({})", self.raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_ids() {
        let id = EntityId::parse("light.kitchen_ceiling").unwrap();
        assert_eq!(id.domain(), "light");
        assert_eq!(id.object_id(), "kitchen_ceiling");
    }

    #[test]
    fn rejects_invalid_ids() {
        for bad in [
            "light",
            ".x",
            "light.",
            "Light.x",
            "light.a-b",
            "_light.x",
            "light.a__b",
            "li__ght.a",
            "light.a.b",
        ] {
            assert!(EntityId::parse(bad).is_err(), "{bad} should be rejected");
        }
    }
}

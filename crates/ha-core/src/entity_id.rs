use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use std::fmt;

/// HA's `valid_entity_id` (`VALID_ENTITY_ID.match`), for validating what
/// clients send (`cv.entity_id`). Looser than [`EntityId::parse`]: Python's
/// `\\d` takes any Unicode decimal digit, `$` allows one trailing newline,
/// and the domain has no length limit.
pub fn valid_entity_id(s: &str) -> bool {
    let s = s.strip_suffix('\n').unwrap_or(s);
    let part = |p: &str| {
        !p.is_empty()
            && !p.starts_with('_')
            && !p.ends_with('_')
            && p.chars()
                .all(|c| c.is_ascii_lowercase() || c == '_' || is_decimal(c))
    };
    match s.split_once('.') {
        Some((domain, object_id)) => part(domain) && part(object_id) && !s.contains("__"),
        None => false,
    }
}

/// Unicode category `Nd` (Unicode 16.0, as Python 3.14's `re` reads `\\d`).
fn is_decimal(c: char) -> bool {
    decimal_value(c).is_some()
}

/// A Unicode decimal digit's value (Python's `int()` of one character):
/// every `Nd` run is consecutive blocks of `0`-`9`.
pub fn decimal_value(c: char) -> Option<u32> {
    DECIMAL
        .iter()
        .find(|&&(lo, hi)| lo <= c && c <= hi)
        .map(|&(lo, _)| (c as u32 - lo as u32) % 10)
}

const DECIMAL: [(char, char); 71] = [
    ('\u{30}', '\u{39}'),
    ('\u{660}', '\u{669}'),
    ('\u{6F0}', '\u{6F9}'),
    ('\u{7C0}', '\u{7C9}'),
    ('\u{966}', '\u{96F}'),
    ('\u{9E6}', '\u{9EF}'),
    ('\u{A66}', '\u{A6F}'),
    ('\u{AE6}', '\u{AEF}'),
    ('\u{B66}', '\u{B6F}'),
    ('\u{BE6}', '\u{BEF}'),
    ('\u{C66}', '\u{C6F}'),
    ('\u{CE6}', '\u{CEF}'),
    ('\u{D66}', '\u{D6F}'),
    ('\u{DE6}', '\u{DEF}'),
    ('\u{E50}', '\u{E59}'),
    ('\u{ED0}', '\u{ED9}'),
    ('\u{F20}', '\u{F29}'),
    ('\u{1040}', '\u{1049}'),
    ('\u{1090}', '\u{1099}'),
    ('\u{17E0}', '\u{17E9}'),
    ('\u{1810}', '\u{1819}'),
    ('\u{1946}', '\u{194F}'),
    ('\u{19D0}', '\u{19D9}'),
    ('\u{1A80}', '\u{1A89}'),
    ('\u{1A90}', '\u{1A99}'),
    ('\u{1B50}', '\u{1B59}'),
    ('\u{1BB0}', '\u{1BB9}'),
    ('\u{1C40}', '\u{1C49}'),
    ('\u{1C50}', '\u{1C59}'),
    ('\u{A620}', '\u{A629}'),
    ('\u{A8D0}', '\u{A8D9}'),
    ('\u{A900}', '\u{A909}'),
    ('\u{A9D0}', '\u{A9D9}'),
    ('\u{A9F0}', '\u{A9F9}'),
    ('\u{AA50}', '\u{AA59}'),
    ('\u{ABF0}', '\u{ABF9}'),
    ('\u{FF10}', '\u{FF19}'),
    ('\u{104A0}', '\u{104A9}'),
    ('\u{10D30}', '\u{10D39}'),
    ('\u{10D40}', '\u{10D49}'),
    ('\u{11066}', '\u{1106F}'),
    ('\u{110F0}', '\u{110F9}'),
    ('\u{11136}', '\u{1113F}'),
    ('\u{111D0}', '\u{111D9}'),
    ('\u{112F0}', '\u{112F9}'),
    ('\u{11450}', '\u{11459}'),
    ('\u{114D0}', '\u{114D9}'),
    ('\u{11650}', '\u{11659}'),
    ('\u{116C0}', '\u{116C9}'),
    ('\u{116D0}', '\u{116E3}'),
    ('\u{11730}', '\u{11739}'),
    ('\u{118E0}', '\u{118E9}'),
    ('\u{11950}', '\u{11959}'),
    ('\u{11BF0}', '\u{11BF9}'),
    ('\u{11C50}', '\u{11C59}'),
    ('\u{11D50}', '\u{11D59}'),
    ('\u{11DA0}', '\u{11DA9}'),
    ('\u{11F50}', '\u{11F59}'),
    ('\u{16130}', '\u{16139}'),
    ('\u{16A60}', '\u{16A69}'),
    ('\u{16AC0}', '\u{16AC9}'),
    ('\u{16B50}', '\u{16B59}'),
    ('\u{16D70}', '\u{16D79}'),
    ('\u{1CCF0}', '\u{1CCF9}'),
    ('\u{1D7CE}', '\u{1D7FF}'),
    ('\u{1E140}', '\u{1E149}'),
    ('\u{1E2F0}', '\u{1E2F9}'),
    ('\u{1E4F0}', '\u{1E4F9}'),
    ('\u{1E5F1}', '\u{1E5FA}'),
    ('\u{1E950}', '\u{1E959}'),
    ('\u{1FBF0}', '\u{1FBF9}'),
];

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

    /// Cases checked against HA 2026.6.1's `valid_entity_id`.
    #[test]
    fn validates_as_ha() {
        let long = format!("{}.a", "d".repeat(300));
        for good in [
            "sensor.\u{661}",
            "sensor.a\n",
            long.as_str(),
            "light.a_1",
            "a.b",
        ] {
            assert!(valid_entity_id(good), "{good:?}");
        }
        for bad in [
            "light",
            "light.a.b",
            "light.a__b",
            "li__ght.a",
            "light._a",
            "light.a_",
            "Light.a",
            "light.a\n\n",
            "light.\u{e9}",
            "light.a-b",
            "light.",
            ".a",
            "light.\u{b2}",
        ] {
            assert!(!valid_entity_id(bad), "{bad:?}");
        }
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

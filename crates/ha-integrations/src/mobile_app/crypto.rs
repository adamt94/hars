//! `mobile_app`'s encryption (`helpers.py`): libsodium's `crypto_secretbox`
//! (XSalsa20-Poly1305), sent as `base64(nonce ‖ MAC ‖ ciphertext)`.

use base64::Engine;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD};
use crypto_secretbox::aead::Aead;
use crypto_secretbox::{KeyInit, Nonce, XSalsa20Poly1305};

const NONCE: usize = 24;
const MAC: usize = 16;

/// A secretbox key.
#[derive(Clone)]
pub struct Key([u8; 32]);

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(..)")
    }
}

impl Key {
    /// `_decrypt_payload_helper`'s key: the 64-character hex secret,
    /// decoded. `None` if it isn't 32 bytes of hex.
    pub fn hex(secret: &str) -> Option<Self> {
        let b = secret.as_bytes();
        if b.len() != 64 {
            return None;
        }
        let mut key = [0; 32];
        for (i, pair) in b.chunks_exact(2).enumerate() {
            let s = std::str::from_utf8(pair).ok()?;
            key[i] = u8::from_str_radix(s, 16).ok()?;
        }
        Some(Self(key))
    }

    /// `_decrypt_payload_legacy`'s key: the secret's first 32 bytes,
    /// NUL-padded.
    pub fn legacy(secret: &str) -> Self {
        let mut key = [0; 32];
        let b = secret.as_bytes();
        let n = b.len().min(32);
        key[..n].copy_from_slice(&b[..n]);
        Self(key)
    }

    #[cfg(test)]
    pub(crate) fn bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Seal `plaintext` under a fresh random nonce, as `webhook_response`.
    pub fn seal(&self, plaintext: &[u8]) -> String {
        self.seal_with(&rand::random::<[u8; NONCE]>(), plaintext)
    }

    pub(crate) fn seal_with(&self, nonce: &[u8; NONCE], plaintext: &[u8]) -> String {
        let cipher = XSalsa20Poly1305::new(&self.0.into());
        // Sealing can't fail for in-memory buffers of this size.
        let sealed = cipher
            .encrypt(Nonce::from_slice(nonce), plaintext)
            .unwrap_or_default();
        let mut out = Vec::with_capacity(NONCE + sealed.len());
        out.extend_from_slice(nonce);
        out.extend_from_slice(&sealed);
        STANDARD.encode(out)
    }

    /// Open an `encrypted_data` string: `None` for bad base64, a short box
    /// or a failed MAC.
    pub fn open(&self, b64: &str) -> Option<Vec<u8>> {
        self.open_raw(&b64decode(b64)?)
    }

    /// Open `nonce ‖ MAC ‖ ciphertext`.
    pub fn open_raw(&self, raw: &[u8]) -> Option<Vec<u8>> {
        if raw.len() < NONCE + MAC {
            return None;
        }
        let cipher = XSalsa20Poly1305::new(&self.0.into());
        cipher
            .decrypt(Nonce::from_slice(&raw[..NONCE]), &raw[NONCE..])
            .ok()
    }
}

/// Python 3.14's `base64.b64decode` (non-strict): characters outside the
/// alphabet are dropped and `=` is skipped wherever it is; only the end is
/// checked: a lone trailing character is an error, as is a final group of 2
/// (3) characters without 2 (1) `=` after it.
pub fn b64decode(s: &str) -> Option<Vec<u8>> {
    const LENIENT: GeneralPurpose = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(DecodePaddingMode::RequireNone),
    );
    if !s.is_ascii() {
        return None;
    }
    let mut data = Vec::with_capacity(s.len());
    let mut pads = 0;
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' => {
                data.push(b);
                pads = 0;
            }
            b'=' => pads += 1,
            _ => {}
        }
    }
    let ok = match data.len() % 4 {
        0 => true,
        2 => pads >= 2,
        3 => pads >= 1,
        _ => false,
    };
    if !ok {
        return None;
    }
    LENIENT.decode(&data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// HA's own helpers with fixed nonces (`vectors/crypto.json`).
    #[test]
    fn seals_and_opens_as_pynacl() {
        let v: Value = serde_json::from_str(include_str!("vectors/crypto.json")).unwrap();
        let secret = v["secret"].as_str().unwrap();
        assert_eq!(
            Key::hex(secret).unwrap().bytes().to_vec(),
            unhex(v["hex_key_hex"].as_str().unwrap())
        );
        assert_eq!(
            Key::legacy(secret).bytes().to_vec(),
            unhex(v["legacy_key_hex_of_SECRET"].as_str().unwrap())
        );
        let legacy = v["legacy_secret"].as_str().unwrap();
        assert_eq!(
            Key::legacy(legacy).bytes().to_vec(),
            unhex(v["legacy_key_hex_of_LEGACY_SECRET"].as_str().unwrap())
        );
        let seals = v["seal"].as_array().unwrap();
        assert_eq!(seals.len(), 8);
        for s in seals {
            let key = match s["key"].as_str().unwrap() {
                "hex" => Key::hex(secret).unwrap(),
                _ => Key::legacy(secret),
            };
            let nonce: [u8; 24] = unhex(s["nonce_hex"].as_str().unwrap()).try_into().unwrap();
            let pt = s["plaintext_utf8"].as_str().unwrap().as_bytes();
            let sealed = s["sealed_b64"].as_str().unwrap();
            assert_eq!(key.seal_with(&nonce, pt), sealed, "{s}");
            assert_eq!(key.open(sealed).unwrap(), pt);
        }
        // Fresh nonces differ and still open.
        let key = Key::hex(secret).unwrap();
        let (a, b) = (key.seal(b"{}"), key.seal(b"{}"));
        assert_ne!(a, b);
        assert_eq!(key.open(&a).unwrap(), b"{}");
        assert!(Key::legacy(secret).open(&a).is_none());
    }

    #[test]
    fn keys_need_64_hex_characters() {
        assert!(Key::hex("00").is_none());
        assert!(Key::hex(&"zz".repeat(32)).is_none());
        assert!(Key::hex(&"é".repeat(32)).is_none());
    }

    /// Python 3.14's `base64.b64decode`, run on each input.
    #[test]
    fn b64decode_is_pythons_non_strict_decoder() {
        let cases: [(&str, Option<&[u8]>); 26] = [
            ("QUJD", Some(b"ABC")),
            ("QUJ", None),
            ("QQ==", Some(b"A")),
            ("QQ=", None),
            ("QQ", None),
            ("Q", None),
            ("QQ==QUJD", None),
            ("QU=JD", Some(b"ABC")),
            ("Q!U J\nD", Some(b"ABC")),
            ("QUJD====", Some(b"ABC")),
            ("QQ===", Some(b"A")),
            ("=QUJD", Some(b"ABC")),
            ("QUJDQQ==x", None),
            ("QQ==\n", Some(b"A")),
            ("QQ==!", Some(b"A")),
            ("QQ==Q", None),
            ("QQ==QQ==", Some(b"A\x04\x10")),
            ("QUJD=", Some(b"ABC")),
            ("QUJD=Q", None),
            ("Q=Q==", Some(b"A")),
            ("QUI=QQ", None),
            ("QUI==", Some(b"AB")),
            ("Q===", None),
            ("QUJDQ===", None),
            ("QUJ=D", Some(b"ABC")),
            ("QUJ\u{e9}D", None),
        ];
        for (input, want) in cases {
            assert_eq!(b64decode(input).as_deref(), want, "{input:?}");
        }
    }
}

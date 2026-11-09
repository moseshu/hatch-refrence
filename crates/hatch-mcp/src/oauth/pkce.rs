//! PKCE (RFC 7636) + `state` for the MCP OAuth authorization-code flow.
//!
//! * `code_verifier`: 32 random bytes, base64url-encoded without padding
//!   (43 characters — inside the RFC 43–128 range).
//! * `code_challenge`: `BASE64URL(SHA256(verifier))`, method `S256`.
//! * `state`: 16 random bytes as hex — a CSRF check for the loopback
//!   redirect, not a secret.
//!
//! Randomness comes from `/dev/urandom`, falling back to a pid + time
//! hash (same pattern as the vault OAuth scaffold, which this module
//! deliberately does not depend on: that scaffold has no PKCE and stores
//! into the vault helper, while MCP OAuth keeps token and config in
//! separate files — see `store.rs`).

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};
use std::io::Read;

/// A fresh PKCE verifier: 43 base64url characters from 32 random bytes.
pub fn new_verifier() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes(32))
}

/// The S256 challenge for a verifier: `BASE64URL(SHA256(verifier))`.
pub fn challenge_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// A fresh `state` value: 16 random bytes as lowercase hex.
pub fn new_state() -> String {
    random_bytes(16).iter().map(|b| format!("{b:02x}")).collect()
}

/// `n` random bytes: `/dev/urandom` first, pid + time hash as fallback.
/// The fallback is only for environments without urandom; it is not
/// claimed to be cryptographic-grade.
fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok()
    {
        return buf;
    }
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        ^ ((std::process::id() as u128) << 64);
    for b in buf.iter_mut() {
        // SplitMix64-style stir: cheap, deterministic, good enough for a
        // fallback when urandom is missing.
        x = x.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        *b = (z ^ (z >> 31)) as u8;
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_is_43_chars_of_base64url() {
        let v = new_verifier();
        assert_eq!(v.len(), 43, "32 bytes -> 43 base64url chars");
        assert!(
            v.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "verifier must be base64url without padding: {v}"
        );
        assert_ne!(new_verifier(), v, "two verifiers must differ");
    }

    #[test]
    fn challenge_is_s256_not_the_verifier() {
        // RFC 7636 appendix B test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            challenge_s256(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let v = new_verifier();
        let c = challenge_s256(&v);
        assert_eq!(c.len(), 43);
        assert_ne!(c, v, "an S256 challenge is never the verifier itself");
        assert_eq!(challenge_s256(&v), c, "challenge is deterministic");
    }

    #[test]
    fn state_is_32_hex_chars() {
        let s = new_state();
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(new_state(), s);
    }
}

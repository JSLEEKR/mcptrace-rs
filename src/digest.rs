//! sha256-based digests used to redact tool arguments and error messages.
//!
//! The golden rule in mcptrace is: we never log tool arguments verbatim, because
//! agent tool calls routinely carry secrets (API keys in URLs, user PII in
//! search queries, credit-card numbers in payment-tool args, etc). Instead
//! we record a stable sha256 hex digest. Identical arguments still deduplicate
//! to the same digest, so repeat-call patterns remain visible; different
//! arguments produce different digests with negligible collision probability.
//!
//! This module wraps the [`sha2::Sha256`] crate behind a tiny, dependency-
//! minimal API so callers don't have to `use sha2::Digest` at every call site.

use sha2::{Digest, Sha256};

/// Compute the lowercase hex sha256 of arbitrary bytes.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    hex::encode(out)
}

/// Compute a digest for tool-call arguments. Accepts a raw byte slice of
/// the JSON `params.arguments` field as it appeared on the wire. We do
/// **not** re-serialize — hashing the canonical wire bytes means digests
/// are stable across mcptrace versions regardless of serde internals.
#[must_use]
pub fn arg_digest(raw: &[u8]) -> String {
    sha256_hex(raw)
}

/// Compute a digest for an error message string. Error messages often
/// echo input; we treat them as potentially sensitive and never persist
/// them in plaintext.
#[must_use]
pub fn error_digest(msg: &str) -> String {
    sha256_hex(msg.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector_empty() {
        // sha256("") is a well-known vector
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_known_vector_abc() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn arg_digest_same_input_same_hash() {
        let a = arg_digest(br#"{"foo":"bar"}"#);
        let b = arg_digest(br#"{"foo":"bar"}"#);
        assert_eq!(a, b);
    }

    #[test]
    fn arg_digest_different_input_different_hash() {
        let a = arg_digest(br#"{"foo":"bar"}"#);
        let b = arg_digest(br#"{"foo":"baz"}"#);
        assert_ne!(a, b);
    }

    #[test]
    fn arg_digest_whitespace_sensitive() {
        // We hash canonical wire bytes — whitespace counts. This is by
        // design: we don't want to risk parsing before hashing.
        let a = arg_digest(br#"{"foo":"bar"}"#);
        let b = arg_digest(br#"{ "foo":"bar" }"#);
        assert_ne!(a, b);
    }

    #[test]
    fn arg_digest_is_64_hex_chars() {
        let a = arg_digest(b"anything");
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn error_digest_roundtrip() {
        let a = error_digest("connection refused");
        let b = error_digest("connection refused");
        assert_eq!(a, b);
    }

    #[test]
    fn error_digest_distinguishes() {
        let a = error_digest("connection refused");
        let b = error_digest("connection reset");
        assert_ne!(a, b);
    }

    #[test]
    fn digests_are_case_stable() {
        // Hex output should be lowercase.
        let h = sha256_hex(b"test");
        assert_eq!(h, h.to_lowercase());
    }

    #[test]
    fn long_input_still_64_chars() {
        let big = vec![0u8; 1_000_000];
        let h = sha256_hex(&big);
        assert_eq!(h.len(), 64);
    }
}

//! Trace / span id generation.
//!
//! We don't pull in `uuid` or `ulid` — those are huge transitive deps for
//! what amounts to "hex-encode some entropy + monotonic counter". Our
//! approach is:
//!
//! - Trace id: 16 bytes = 32 hex chars = W3C traceparent size
//! - Span id: 8 bytes = 16 hex chars = W3C traceparent size
//! - Entropy source: process-start seed (`UNIX_EPOCH` nanos) XOR-mixed with
//!   a monotonic counter using a simple `splitmix64` finalizer. This is
//!   *not* cryptographically random — trace ids are not secrets — but
//!   collisions within a proxy session are essentially impossible.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

fn next() -> u64 {
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    splitmix64(seed().wrapping_add(c))
}

/// Generate a new 128-bit (32 hex char) trace id.
#[must_use]
pub fn new_trace_id() -> String {
    let hi = next();
    let lo = next();
    format!("{hi:016x}{lo:016x}")
}

/// Generate a new 64-bit (16 hex char) span id.
#[must_use]
pub fn new_span_id() -> String {
    format!("{:016x}", next())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn trace_id_is_32_hex_chars() {
        let id = new_trace_id();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn span_id_is_16_hex_chars() {
        let id = new_span_id();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn trace_ids_are_unique_across_1k_calls() {
        let mut set = HashSet::new();
        for _ in 0..1000 {
            set.insert(new_trace_id());
        }
        assert_eq!(set.len(), 1000);
    }

    #[test]
    fn span_ids_are_unique_across_1k_calls() {
        let mut set = HashSet::new();
        for _ in 0..1000 {
            set.insert(new_span_id());
        }
        assert_eq!(set.len(), 1000);
    }

    #[test]
    fn splitmix_is_deterministic_function() {
        assert_eq!(splitmix64(1), splitmix64(1));
        assert_ne!(splitmix64(1), splitmix64(2));
    }

    #[test]
    fn trace_id_lowercase() {
        let id = new_trace_id();
        assert_eq!(id, id.to_lowercase());
    }
}

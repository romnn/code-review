//! Random identifiers, for a clone's identity and a running app's session.

use std::hash::{BuildHasher as _, Hasher as _, RandomState};
use std::io::Read as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::analysis::hex_digest;

/// Distinguishes identifiers made in the same process at the same instant.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Returns `digits` (at most 64) random lowercase hexadecimal digits.
///
/// The bytes come from the operating system's generator where there is one, mixed with the
/// standard library's per-process random hash keys, the time, the process and a counter, so an
/// identifier is unique even where the generator cannot be read.
pub(crate) fn hex(digits: usize) -> String {
    let mut entropy = Vec::with_capacity(64);
    let mut system = [0_u8; 32];
    if let Ok(mut file) = std::fs::File::open("/dev/urandom")
        && file.read_exact(&mut system).is_ok()
    {
        entropy.extend_from_slice(&system);
    }
    let mut keyed = RandomState::new().build_hasher();
    keyed.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    entropy.extend_from_slice(&keyed.finish().to_le_bytes());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    entropy.extend_from_slice(&nanos.to_le_bytes());
    entropy.extend_from_slice(&std::process::id().to_le_bytes());
    let mut text = hex_digest(&entropy);
    text.truncate(digits);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Identifiers have the length asked for, use only lowercase hex digits, and differ.
    #[test]
    fn identifiers_are_hex_and_distinct() {
        let first = hex(16);
        let second = hex(16);
        assert_eq!(first.len(), 16);
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert_ne!(first, second);
    }
}

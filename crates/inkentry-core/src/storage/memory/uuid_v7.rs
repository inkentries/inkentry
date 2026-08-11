//! Minting the exported identity of a memory entry.

use uuid::{Builder, Uuid};

/// Mint a UUIDv7 whose embedded timestamp is `created_at` (unix seconds),
/// never the wall clock.
///
/// An import replays an entire back catalogue in a single pass. Minting from
/// the wall clock would stamp every historical entry with one instant and
/// destroy the ordering v7 exists to carry — for all of history at once, and
/// irreversibly. Seeding from each entry's own creation time preserves it.
///
/// The low bits are random, so entries sharing a `created_at` still receive
/// distinct identifiers.
pub fn uuid_v7_at(created_at: i64) -> String {
    let millis = created_at.max(0).saturating_mul(1_000) as u64;
    // v4's version and variant nibbles sit in bytes 6 and 8; taking only
    // fully-random bytes keeps all 80 bits here unbiased.
    let entropy = *Uuid::new_v4().as_bytes();
    let mut tail = [0u8; 10];
    tail[..6].copy_from_slice(&entropy[..6]);
    tail[6..].copy_from_slice(&entropy[9..13]);
    Builder::from_unix_timestamp_millis(millis, &tail)
        .into_uuid()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn carries_version_and_variant_bits() {
        let id = Uuid::from_str(&uuid_v7_at(1_786_300_000)).unwrap();
        assert_eq!(id.get_version_num(), 7);
        assert_eq!(id.get_variant(), uuid::Variant::RFC4122);
    }

    #[test]
    fn timestamp_comes_from_created_at_not_the_wall_clock() {
        let id = Uuid::from_str(&uuid_v7_at(1_000_000_000)).unwrap();
        let (secs, _) = id.get_timestamp().unwrap().to_unix();
        assert_eq!(secs, 1_000_000_000);
    }

    #[test]
    fn older_created_at_sorts_before_newer() {
        let old = uuid_v7_at(1_000);
        let new = uuid_v7_at(2_000_000_000);
        assert!(old < new, "{old} should sort before {new}");
    }

    #[test]
    fn entries_sharing_a_created_at_get_distinct_ids() {
        let a = uuid_v7_at(1_786_300_000);
        let b = uuid_v7_at(1_786_300_000);
        assert_ne!(a, b);
    }

    #[test]
    fn a_negative_created_at_is_clamped_rather_than_wrapping() {
        let id = Uuid::from_str(&uuid_v7_at(-5)).unwrap();
        let (secs, _) = id.get_timestamp().unwrap().to_unix();
        assert_eq!(secs, 0);
    }
}

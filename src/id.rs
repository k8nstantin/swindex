//! Stable intrinsic identity for nodes and edges.
//!
//! All identifiers in swindex are `UUIDv7` — a 128-bit identifier whose leading
//! 48 bits are a millisecond Unix timestamp, so IDs sort chronologically. This
//! is the property that makes intrinsic identity survive administrative
//! events (subdivision, merger, re-plat, rename) without breaking references.
//!
//! A [`Uuid7`] is a newtype over [`uuid::Uuid`] that enforces the version-7
//! invariant: any value held in a `Uuid7` is guaranteed to be a v7 UUID. Code
//! that wants to accept "any UUID" should use [`uuid::Uuid`] directly;
//! everything in swindex's data model uses `Uuid7`.

use core::fmt;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A UUID version 7 — time-ordered 128-bit identifier.
///
/// `Uuid7` is the identity type for every node and edge in swindex. The
/// underlying bytes are layout-compatible with [`uuid::Uuid`]; serialization
/// uses the canonical hyphenated string form.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Uuid7(Uuid);

impl Uuid7 {
    /// Mint a fresh `Uuid7` from the system clock.
    ///
    /// Two calls in the same millisecond produce monotonically increasing
    /// values; the time component is taken from [`uuid::Uuid::now_v7`].
    #[must_use]
    pub fn now() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wrap an existing v7 `Uuid` after verifying its version field.
    ///
    /// Returns `None` if `uuid` is not a version-7 UUID.
    #[must_use]
    pub fn from_uuid(uuid: Uuid) -> Option<Self> {
        if uuid.get_version_num() == 7 {
            Some(Self(uuid))
        } else {
            None
        }
    }

    /// The underlying `Uuid` value.
    #[must_use]
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// The 16 raw bytes, big-endian.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }
}

impl fmt::Debug for Uuid7 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Uuid7({})", self.0)
    }
}

impl fmt::Display for Uuid7 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::Uuid7;
    use uuid::Uuid;

    #[test]
    fn now_yields_version_seven() {
        let id = Uuid7::now();
        assert_eq!(id.as_uuid().get_version_num(), 7);
    }

    #[test]
    fn now_is_monotonic_within_the_same_thread() {
        let a = Uuid7::now();
        let b = Uuid7::now();
        let c = Uuid7::now();
        assert!(a <= b);
        assert!(b <= c);
    }

    #[test]
    fn from_uuid_rejects_non_v7() {
        // The nil UUID has version 0; from_uuid must reject anything that is
        // not version 7 regardless of which non-v7 family it belongs to.
        let nil = Uuid::nil();
        assert_ne!(nil.get_version_num(), 7);
        assert!(Uuid7::from_uuid(nil).is_none());
    }

    #[test]
    fn from_uuid_accepts_v7() {
        let raw = Uuid::now_v7();
        let wrapped = Uuid7::from_uuid(raw).expect("v7 input must be accepted");
        assert_eq!(wrapped.as_uuid(), &raw);
    }

    #[test]
    fn display_matches_canonical_form() {
        let raw = Uuid::now_v7();
        let wrapped = Uuid7::from_uuid(raw).unwrap();
        assert_eq!(wrapped.to_string(), raw.to_string());
    }

    #[test]
    fn json_round_trip_preserves_value() {
        let original = Uuid7::now();
        let encoded = serde_json::to_string(&original).unwrap();
        let decoded: Uuid7 = serde_json::from_str(&encoded).unwrap();
        assert_eq!(original, decoded);
        // serde(transparent) means the JSON is the inner Uuid's string form.
        assert_eq!(encoded, format!("\"{original}\""));
    }
}

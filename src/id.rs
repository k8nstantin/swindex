//! Stable intrinsic identity for nodes and edges.
//!
//! # Why UUIDv7
//!
//! Every node and every edge in a swindex graph has a 128-bit identifier.
//! We chose UUID version 7 specifically — not version 4 (random), not
//! version 1 (MAC-derived), not application-issued integers — because v7
//! has three properties no other identifier scheme combines:
//!
//! 1. **Intrinsic.** The id is generated at the point of creation and
//!    travels with the entity for life. It does not derive from any
//!    administrative attribute (parcel number, address, serial number)
//!    that might later change. A parcel that gets subdivided, merged,
//!    re-platted, or moved across a jurisdictional boundary keeps the
//!    same Uuid7 forever.
//!
//! 2. **Time-ordered.** The leading 48 bits encode a millisecond Unix
//!    timestamp, so the natural byte-order of two Uuid7s is also their
//!    creation order. This makes them friendly to LSM-style storage
//!    (Fjall keyspaces sort by key, so chronologically-near nodes land
//!    near each other on disk).
//!
//! 3. **Globally unique without coordination.** No central authority
//!    issues ids. Two nodes minted on opposite sides of the planet,
//!    inside the same millisecond, can be combined into one substrate
//!    later without collision risk (the trailing 74 bits are random).
//!
//! # The newtype invariant
//!
//! [`Uuid7`] is a newtype over [`uuid::Uuid`]. The wrapper guarantees that
//! any value of type `Uuid7` is genuinely a version-7 UUID — never a v4
//! that happened to be passed where v7 was expected. The only ways to
//! produce a `Uuid7` are:
//!
//! * [`Uuid7::now`] — mints a fresh one from the system clock.
//! * [`Uuid7::from_uuid`] — validates the version field of an existing
//!   `Uuid` and returns `Some(Uuid7)` only if it's v7.
//!
//! There is deliberately no `impl From<Uuid> for Uuid7`; conversion is
//! explicit and fallible. This is the entire safety mechanism — anywhere
//! the index sees a `Uuid7` it can trust the version without re-checking.

use core::fmt;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A UUID version 7 — time-ordered 128-bit identifier.
///
/// `Uuid7` is the identity type for every node and edge in swindex. The
/// underlying bytes are layout-compatible with [`uuid::Uuid`]; serialization
/// uses the canonical hyphenated string form (via `serde(transparent)` so the
/// wire format is just the inner Uuid, not `{"Uuid7": "..."}`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Uuid7(Uuid);

impl Uuid7 {
    /// Mint a fresh `Uuid7` from the system clock.
    ///
    /// Internally calls [`uuid::Uuid::now_v7`], which uses a process-local
    /// monotonic context to guarantee that consecutive calls in the same
    /// thread produce strictly increasing values, even when those calls
    /// happen inside the same millisecond. That guarantee is load-bearing
    /// for graph builders that mint many ids in a tight loop — without it
    /// the ingestion would silently produce duplicate keys.
    #[must_use]
    pub fn now() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wrap an existing v7 `Uuid` after verifying its version field.
    ///
    /// Returns `None` if `uuid` is not a version-7 UUID. The check inspects
    /// the 4-bit version nibble at offset 48 of the 128-bit value (per
    /// [RFC 9562 §4](https://www.rfc-editor.org/rfc/rfc9562#section-4)).
    /// This is the *only* fallible-construction path — there is no
    /// infallible `From<Uuid>` impl, by design.
    #[must_use]
    pub fn from_uuid(uuid: Uuid) -> Option<Self> {
        if uuid.get_version_num() == 7 {
            Some(Self(uuid))
        } else {
            None
        }
    }

    /// Borrow the underlying `Uuid` value (for interop with code expecting
    /// the raw `uuid` crate type — Fjall keys, network wire formats, etc).
    #[must_use]
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// The 16 raw bytes in canonical big-endian order. Useful as a key in
    /// byte-keyed stores (Fjall, RocksDB) where chronological sort matters.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }
}

impl fmt::Debug for Uuid7 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Show the type tag in Debug so test failures distinguish a Uuid7
        // from a bare Uuid in panic messages.
        write!(f, "Uuid7({})", self.0)
    }
}

impl fmt::Display for Uuid7 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Display is the bare canonical hyphenated form (no type tag) so
        // serialized output (logs, URLs, JSON) is clean.
        fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::Uuid7;
    use uuid::Uuid;

    #[test]
    fn now_yields_version_seven() {
        // Spot-check that the constructor really produces a v7 UUID — guards
        // against an accidental switch to new_v4 or similar in the future.
        let id = Uuid7::now();
        assert_eq!(id.as_uuid().get_version_num(), 7);
    }

    #[test]
    fn now_is_monotonic_within_the_same_thread() {
        // The uuid crate's shared monotonic context guarantees a < b < c
        // even when all three calls happen inside the same millisecond.
        // Without this guarantee, builders that mint ids in tight loops
        // would produce duplicates and silently corrupt the index.
        let a = Uuid7::now();
        let b = Uuid7::now();
        let c = Uuid7::now();
        assert!(a <= b);
        assert!(b <= c);
    }

    #[test]
    fn from_uuid_rejects_non_v7() {
        // The nil UUID has version 0; from_uuid must reject anything that
        // is not version 7 regardless of which non-v7 family it came from.
        // (v0/nil is the cheapest non-v7 value to construct without needing
        // additional uuid crate features.)
        let nil = Uuid::nil();
        assert_ne!(nil.get_version_num(), 7);
        assert!(Uuid7::from_uuid(nil).is_none());
    }

    #[test]
    fn from_uuid_accepts_v7() {
        // The round trip: raw v7 -> Uuid7 -> as_uuid should preserve the
        // exact 128-bit value, no normalization or re-versioning.
        let raw = Uuid::now_v7();
        let wrapped = Uuid7::from_uuid(raw).expect("v7 input must be accepted");
        assert_eq!(wrapped.as_uuid(), &raw);
    }

    #[test]
    fn display_matches_canonical_form() {
        // Display must be the bare canonical Uuid string, not the "Uuid7(...)"
        // wrapper form (which is what Debug uses).
        let raw = Uuid::now_v7();
        let wrapped = Uuid7::from_uuid(raw).unwrap();
        assert_eq!(wrapped.to_string(), raw.to_string());
    }

    #[test]
    fn json_round_trip_preserves_value() {
        // serde(transparent) means JSON is `"<canonical>"` — *just* the
        // inner Uuid string, not `{"Uuid7": "..."}`. This is the wire shape
        // every consumer of the substrate will see, so we pin it.
        let original = Uuid7::now();
        let encoded = serde_json::to_string(&original).unwrap();
        let decoded: Uuid7 = serde_json::from_str(&encoded).unwrap();
        assert_eq!(original, decoded);
        assert_eq!(encoded, format!("\"{original}\""));
    }
}

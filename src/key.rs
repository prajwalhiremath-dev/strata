//! Internal keys: how strata tells versions of the same key apart.
//!
//! The user only sees `user_key -> value`. Internally every write is stamped
//! with a global, ever-increasing **sequence number** (seqno) and a **kind**
//! (`Put` or `Delete`). The triple `(user_key, seqno, kind)` is the
//! *internal key*.
//!
//! Ordering: user key ascending, then seqno **descending**, then kind
//! descending. So for one user key the newest version comes first, which lets
//! a lookup stop at the first entry it finds (see chapter 01).
//!
//! Encoding (used by the WAL and SSTables in later milestones):
//!
//! ```text
//! +------------------+--------------------------------------+
//! | user_key bytes   | trailer: u64 little-endian           |
//! | (variable)       | = (seqno << 8) | kind   (8 bytes)    |
//! +------------------+--------------------------------------+
//! ```

use std::cmp::Ordering;

use crate::{Error, Result};

/// A global sequence number. Every write gets the next one.
pub type SeqNo = u64;

/// Largest sequence number: 56 bits, because the low 8 bits of the trailer
/// hold the kind.
pub const MAX_SEQNO: SeqNo = (1 << 56) - 1;

const TRAILER_LEN: usize = 8;

/// What an internal key records about its user key.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ValueKind {
    /// A tombstone: the key was deleted at this seqno.
    Delete = 0,
    /// A value was written at this seqno.
    Put = 1,
}

impl ValueKind {
    fn from_u8(b: u8) -> Result<Self> {
        match b {
            0 => Ok(ValueKind::Delete),
            1 => Ok(ValueKind::Put),
            other => Err(Error::Corruption(format!("unknown value kind {other}"))),
        }
    }
}

/// `(user_key, seqno, kind)`: one version of one user key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InternalKey {
    pub user_key: Vec<u8>,
    pub seqno: SeqNo,
    pub kind: ValueKind,
}

impl InternalKey {
    pub fn new(user_key: &[u8], seqno: SeqNo, kind: ValueKind) -> Self {
        debug_assert!(seqno <= MAX_SEQNO, "seqno {seqno} exceeds 56 bits");
        InternalKey {
            user_key: user_key.to_vec(),
            seqno,
            kind,
        }
    }

    fn trailer(&self) -> u64 {
        (self.seqno << 8) | self.kind as u64
    }

    /// Serialise as `user_key ++ trailer(le u64)`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.user_key.len() + TRAILER_LEN);
        out.extend_from_slice(&self.user_key);
        out.extend_from_slice(&self.trailer().to_le_bytes());
        out
    }

    /// Parse bytes produced by [`InternalKey::encode`].
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let (user_key, trailer) = split(buf)?;
        Ok(InternalKey {
            user_key: user_key.to_vec(),
            seqno: trailer >> 8,
            kind: ValueKind::from_u8((trailer & 0xff) as u8)?,
        })
    }
}

impl Ord for InternalKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.user_key
            .cmp(&other.user_key)
            .then_with(|| other.seqno.cmp(&self.seqno)) // descending
            .then_with(|| other.kind.cmp(&self.kind)) // descending
    }
}

impl PartialOrd for InternalKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn split(buf: &[u8]) -> Result<(&[u8], u64)> {
    if buf.len() < TRAILER_LEN {
        return Err(Error::Corruption(format!(
            "internal key too short: {} bytes",
            buf.len()
        )));
    }
    let (user_key, t) = buf.split_at(buf.len() - TRAILER_LEN);
    Ok((user_key, u64::from_le_bytes(t.try_into().expect("8 bytes"))))
}

/// Compare two *encoded* internal keys without decoding (no allocation).
/// Must agree with `InternalKey::cmp`; the property test below checks this.
///
/// Comparing the whole trailer in reverse handles both "seqno descending" and
/// "kind descending" at once, because seqno occupies the high bits.
///
/// # Panics
/// If either input is shorter than 8 bytes (callers only pass keys they
/// encoded or already validated).
pub fn compare_encoded(a: &[u8], b: &[u8]) -> Ordering {
    let (ua, ta) = split(a).expect("valid encoded key");
    let (ub, tb) = split(b).expect("valid encoded key");
    ua.cmp(ub).then_with(|| tb.cmp(&ta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use proptest::prelude::*;

    #[test]
    fn newer_seqno_sorts_first() {
        assert!(
            InternalKey::new(b"a", 5, ValueKind::Put) < InternalKey::new(b"a", 3, ValueKind::Put)
        );
    }

    #[test]
    fn user_key_dominates() {
        assert!(
            InternalKey::new(b"a", 1, ValueKind::Put) < InternalKey::new(b"b", 9, ValueKind::Put)
        );
    }

    #[test]
    fn roundtrip() {
        let k = InternalKey::new(b"key", 42, ValueKind::Delete);
        assert_eq!(InternalKey::decode(&k.encode()).unwrap(), k);
    }

    #[test]
    fn decode_rejects_short() {
        assert!(matches!(
            InternalKey::decode(&[1, 2, 3]),
            Err(Error::Corruption(_))
        ));
    }

    #[test]
    fn decode_rejects_bad_kind() {
        let mut buf = b"k".to_vec();
        buf.extend_from_slice(&((9u64 << 8) | 7).to_le_bytes());
        assert!(matches!(
            InternalKey::decode(&buf),
            Err(Error::Corruption(_))
        ));
    }

    fn arb_key() -> impl Strategy<Value = InternalKey> {
        (
            proptest::collection::vec(any::<u8>(), 0..=8),
            0..=MAX_SEQNO,
            prop_oneof![Just(ValueKind::Put), Just(ValueKind::Delete)],
        )
            .prop_map(|(k, s, kind)| InternalKey::new(&k, s, kind))
    }

    proptest! {
        #[test]
        fn encoded_order_matches_struct_order(a in arb_key(), b in arb_key()) {
            prop_assert_eq!(compare_encoded(&a.encode(), &b.encode()), a.cmp(&b));
        }
    }
}

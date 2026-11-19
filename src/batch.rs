//! `WriteBatch`: several puts/deletes applied atomically.
//!
//! Atomic means two things: readers see either none or all of the batch, and
//! (from milestone 2) after a crash either none or all of it is recovered.

use crate::key::{SeqNo, ValueKind};
use crate::{Error, Result};

/// Longest accepted value: lengths are stored as `u32` in the WAL.
pub const MAX_VALUE_LEN: usize = u32::MAX as usize;

/// One operation inside a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BatchOp {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
}

impl BatchOp {
    pub(crate) fn key(&self) -> &[u8] {
        match self {
            BatchOp::Put { key, .. } | BatchOp::Delete { key } => key,
        }
    }
}

/// An ordered list of writes to apply atomically with [`crate::Db::write`].
/// If the same key appears twice, the later operation wins.
#[derive(Debug, Clone, Default)]
pub struct WriteBatch {
    pub(crate) ops: Vec<BatchOp>,
}

impl WriteBatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> &mut Self {
        self.ops.push(BatchOp::Put {
            key: key.to_vec(),
            value: value.to_vec(),
        });
        self
    }

    pub fn delete(&mut self, key: &[u8]) -> &mut Self {
        self.ops.push(BatchOp::Delete { key: key.to_vec() });
        self
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Serialise for the WAL. `first_seqno` is the sequence number of the
    /// first op; op `i` gets `first_seqno + i`.
    ///
    /// ```text
    /// first_seqno u64 | count u32 | op*
    /// op = kind u8 | key_len u32 | key | (Put only) val_len u32 | val
    /// ```
    /// All integers little-endian. Lengths must already be validated
    /// (`Db::write` checks `MAX_KEY_LEN` / `MAX_VALUE_LEN`).
    pub(crate) fn encode(&self, first_seqno: SeqNo) -> Vec<u8> {
        let size: usize = 12
            + self
                .ops
                .iter()
                .map(|op| match op {
                    BatchOp::Put { key, value } => 9 + key.len() + value.len(),
                    BatchOp::Delete { key } => 5 + key.len(),
                })
                .sum::<usize>();
        let mut out = Vec::with_capacity(size);
        out.extend_from_slice(&first_seqno.to_le_bytes());
        out.extend_from_slice(&(self.ops.len() as u32).to_le_bytes());
        for op in &self.ops {
            match op {
                BatchOp::Put { key, value } => {
                    out.push(ValueKind::Put as u8);
                    put_bytes(&mut out, key);
                    put_bytes(&mut out, value);
                }
                BatchOp::Delete { key } => {
                    out.push(ValueKind::Delete as u8);
                    put_bytes(&mut out, key);
                }
            }
        }
        out
    }

    /// Parse bytes produced by [`WriteBatch::encode`]. Returns `Corruption`
    /// (never panics) for anything malformed.
    pub(crate) fn decode(buf: &[u8]) -> Result<(SeqNo, WriteBatch)> {
        let mut r = Reader { buf };
        let first_seqno = u64::from_le_bytes(r.take(8)?.try_into().expect("8 bytes"));
        let count = r.u32()?;
        // Don't trust `count` for preallocation: a corrupt value could ask
        // for gigabytes. Each op needs at least 5 bytes, which bounds it.
        let mut ops = Vec::with_capacity((count as usize).min(r.buf.len() / 5));
        for _ in 0..count {
            let kind = r.take(1)?[0];
            let key = r.bytes()?.to_vec();
            ops.push(match kind {
                k if k == ValueKind::Put as u8 => BatchOp::Put {
                    key,
                    value: r.bytes()?.to_vec(),
                },
                k if k == ValueKind::Delete as u8 => BatchOp::Delete { key },
                other => return Err(corrupt(format!("unknown op kind {other}"))),
            });
        }
        if !r.buf.is_empty() {
            return Err(corrupt(format!("{} trailing bytes", r.buf.len())));
        }
        Ok((first_seqno, WriteBatch { ops }))
    }
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

fn corrupt(msg: String) -> Error {
    Error::Corruption(format!("write batch: {msg}"))
}

/// Bounds-checked cursor over a byte slice.
struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() < n {
            return Err(corrupt(format!(
                "needed {n} bytes, {} left",
                self.buf.len()
            )));
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    /// A u32 length followed by that many bytes.
    fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use proptest::prelude::*;

    fn sample() -> WriteBatch {
        let mut b = WriteBatch::new();
        b.put(b"alpha", b"1").delete(b"beta").put(b"gamma", b"");
        b
    }

    #[test]
    fn batch_roundtrip() {
        let b = sample();
        let (seq, back) = WriteBatch::decode(&b.encode(42)).unwrap();
        assert_eq!(seq, 42);
        assert_eq!(back.ops, b.ops);
    }

    #[test]
    fn empty_batch_roundtrip() {
        let (seq, back) = WriteBatch::decode(&WriteBatch::new().encode(7)).unwrap();
        assert_eq!(seq, 7);
        assert!(back.is_empty());
    }

    #[test]
    fn decode_rejects_truncated() {
        let enc = sample().encode(1);
        for cut in 0..enc.len() {
            assert!(
                matches!(WriteBatch::decode(&enc[..cut]), Err(Error::Corruption(_))),
                "prefix of length {cut} must be rejected"
            );
        }
    }

    #[test]
    fn decode_rejects_trailing_bytes() {
        let mut enc = sample().encode(1);
        enc.push(0);
        assert!(matches!(
            WriteBatch::decode(&enc),
            Err(Error::Corruption(_))
        ));
    }

    #[test]
    fn decode_rejects_unknown_kind() {
        let mut enc = sample().encode(1);
        enc[12] = 9; // first op's kind byte
        assert!(matches!(
            WriteBatch::decode(&enc),
            Err(Error::Corruption(_))
        ));
    }

    proptest! {
        #[test]
        fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = WriteBatch::decode(&bytes);
        }
    }
}

//! `WriteBatch`: several puts/deletes applied atomically.
//!
//! Atomic means two things: readers see either none or all of the batch, and
//! (from milestone 2) after a crash either none or all of it is recovered.

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
}

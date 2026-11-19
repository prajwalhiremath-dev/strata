//! The public `Db` handle.
//!
//! Milestone 1: everything lives in one memtable. The write path is already
//! shaped like the final engine:
//!
//! 1. validate the whole batch (nothing is applied if any key is bad)
//! 2. take the write lock and assign consecutive sequence numbers
//! 3. (milestone 2: append the batch to the WAL here)
//! 4. insert every entry into the memtable
//! 5. **publish** by storing the last seqno into `visible_seqno`
//!
//! Readers load `visible_seqno` once and ignore anything newer. Entries of a
//! batch in progress have seqnos above `visible_seqno`, so a reader can never
//! see half a batch. Chapter 01 walks through why the Release/Acquire pair on
//! `visible_seqno` is needed.

use std::ops::Bound;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::batch::{BatchOp, WriteBatch};
use crate::key::{MAX_SEQNO, SeqNo, ValueKind};
use crate::memtable::{Lookup, Memtable};
use crate::options::Options;
use crate::{Error, Result};

/// Longest accepted key. SSTables (milestone 3) store key lengths as `u16`.
pub const MAX_KEY_LEN: usize = 65_535;

/// A handle to an open database. Cheap to clone; clones share the same data
/// and can be sent to other threads.
#[derive(Clone)]
pub struct Db {
    inner: Arc<DbInner>,
}

struct DbInner {
    #[allow(dead_code)] // memtable_size is used once flushing exists (milestone 4)
    options: Options,
    memtable: Memtable,
    /// Serialises writers. Holds the last *assigned* sequence number.
    write_lock: Mutex<SeqNo>,
    /// Last sequence number whose batch is fully applied. Readers use it as
    /// their snapshot.
    visible_seqno: AtomicU64,
    closed: AtomicBool,
}

impl Db {
    /// Open a database that lives only in memory (no files yet: the WAL and
    /// `Db::open(path)` arrive in milestone 2).
    pub fn open_in_memory(options: Options) -> Result<Db> {
        Ok(Db {
            inner: Arc::new(DbInner {
                options,
                memtable: Memtable::new(),
                write_lock: Mutex::new(0),
                visible_seqno: AtomicU64::new(0),
                closed: AtomicBool::new(false),
            }),
        })
    }

    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let mut b = WriteBatch::new();
        b.put(key, value);
        self.write(b)
    }

    /// Delete `key`. Deleting a key that does not exist is not an error.
    pub fn delete(&self, key: &[u8]) -> Result<()> {
        let mut b = WriteBatch::new();
        b.delete(key);
        self.write(b)
    }

    /// Apply every operation in `batch` atomically.
    pub fn write(&self, batch: WriteBatch) -> Result<()> {
        self.check_open()?;
        for op in &batch.ops {
            validate_key(op.key())?;
        }
        if batch.is_empty() {
            return Ok(());
        }

        let mut last = self.inner.write_lock.lock();
        let count = batch.len() as u64;
        if *last + count > MAX_SEQNO {
            return Err(Error::InvalidArgument("sequence numbers exhausted".into()));
        }
        let first = *last + 1;
        for (i, op) in batch.ops.iter().enumerate() {
            let seqno = first + i as u64;
            match op {
                BatchOp::Put { key, value } => {
                    self.inner
                        .memtable
                        .insert(seqno, ValueKind::Put, key, value)
                }
                BatchOp::Delete { key } => {
                    self.inner
                        .memtable
                        .insert(seqno, ValueKind::Delete, key, &[])
                }
            }
        }
        *last += count;
        // Publish: everything inserted above "happens before" any reader that
        // observes this value with Acquire.
        self.inner.visible_seqno.store(*last, Ordering::Release);
        Ok(())
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.check_open()?;
        let snapshot = self.inner.visible_seqno.load(Ordering::Acquire);
        Ok(match self.inner.memtable.get(key, snapshot) {
            Lookup::Found(v) => Some(v),
            // Later milestones keep searching disk on NotFound, but stop on Deleted.
            Lookup::Deleted | Lookup::NotFound => None,
        })
    }

    /// All live key/value pairs with keys inside the bounds, in key order.
    pub fn scan(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.check_open()?;
        let snapshot = self.inner.visible_seqno.load(Ordering::Acquire);
        Ok(self
            .inner
            .memtable
            .scan(start, end, snapshot)
            .into_iter()
            .filter_map(|(k, v)| v.map(|v| (k, v))) // hide tombstones
            .collect())
    }

    /// Close the database. Further operations return [`Error::Closed`].
    /// Closing twice is allowed.
    pub fn close(&self) -> Result<()> {
        self.inner.closed.store(true, Ordering::Release);
        Ok(())
    }

    fn check_open(&self) -> Result<()> {
        if self.inner.closed.load(Ordering::Acquire) {
            Err(Error::Closed)
        } else {
            Ok(())
        }
    }
}

fn validate_key(key: &[u8]) -> Result<()> {
    if key.is_empty() {
        return Err(Error::InvalidArgument("key must not be empty".into()));
    }
    if key.len() > MAX_KEY_LEN {
        return Err(Error::InvalidArgument(format!(
            "key is {} bytes; the limit is {MAX_KEY_LEN}",
            key.len()
        )));
    }
    Ok(())
}

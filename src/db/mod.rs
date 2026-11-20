//! The public `Db` handle.
//!
//! **Where data lives.** Newest first:
//!
//! - `mem`: the memtable new writes go into;
//! - `imm`: a full memtable being written to disk (only during a flush);
//! - `tables`: L0 tables on disk, newest first. Tables may overlap in key
//!   range, so a read checks each in turn (compaction, milestone 7, fixes
//!   that).
//!
//! These three form one [`ReadState`], swapped as a whole behind an
//! `RwLock<Arc<_>>`. A reader clones the `Arc` and searches without holding
//! any lock; a flush builds a new `ReadState` and swaps it in.
//!
//! **Write path** (under the writer mutex), in this exact order:
//!
//! 1. validate the whole batch (nothing is applied if any key/value is bad)
//! 2. refuse if closed or poisoned
//! 3. if the memtable is full, flush it first ([`flush`])
//! 4. assign consecutive sequence numbers
//! 5. append the encoded batch to the WAL, and sync per [`SyncPolicy`]
//! 6. insert every entry into the memtable
//! 7. **publish** by storing the last seqno into `visible_seqno`
//!
//! The log comes first so that anything a reader could ever see is already in
//! the log. If step 5 fails the database is *poisoned*: the log may now end in
//! a partial record, and a later write appended after it would be lost on
//! recovery, so further writes are refused (chapter 02).
//!
//! **Open path**: see [`recovery`]. Old logs are replayed, written to a table
//! and deleted; the database then starts a fresh log.

mod files;
mod flush;
mod recovery;

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::{Mutex, RwLock};

use crate::batch::{BatchOp, MAX_VALUE_LEN, WriteBatch};
use crate::fs::{FileLock, FileSystem, OsFs, SimFs};
use crate::key::{MAX_SEQNO, SeqNo, ValueKind};
use crate::memtable::{Lookup, Memtable};
use crate::options::{Options, SyncPolicy};
use crate::sstable::Table;
use crate::wal::WalWriter;
use crate::{Error, Result};

/// Longest accepted key. Tables could store keys up to `u32::MAX` bytes, but
/// every index entry holds a whole key, so huge keys make huge indexes; 64 KiB
/// is far beyond any sensible key.
pub const MAX_KEY_LEN: usize = 65_535;

const LOCK_FILE: &str = "LOCK";

/// A handle to an open database. Cheap to clone; clones share the same data
/// and can be sent to other threads.
#[derive(Clone)]
pub struct Db {
    inner: Arc<DbInner>,
}

/// Every place a read must look, newest first. Immutable once published.
pub(crate) struct ReadState {
    pub(crate) mem: Arc<Memtable>,
    pub(crate) imm: Option<Arc<Memtable>>,
    /// Newest first.
    pub(crate) tables: Vec<Arc<Table>>,
}

struct DbInner {
    options: Options,
    fs: Arc<dyn FileSystem>,
    dir: PathBuf,
    /// Replaced (never mutated) under the writer lock.
    state: RwLock<Arc<ReadState>>,
    /// Serialises writers and owns the log.
    writer: Mutex<Writer>,
    /// Last sequence number whose batch is fully applied. Readers use it as
    /// their snapshot.
    visible_seqno: AtomicU64,
    closed: AtomicBool,
}

struct Writer {
    /// Last *assigned* sequence number.
    last_seqno: SeqNo,
    wal: WalWriter,
    /// The current log's file number (deleted after the next flush).
    log_number: u64,
    /// Next unused file number, shared by logs and tables.
    next_file_number: u64,
    writes_since_sync: u32,
    /// Set after a failed log append/sync; further writes are refused.
    poisoned: Option<String>,
    /// Held while open; dropping it releases the directory lock.
    lock: Option<Box<dyn FileLock>>,
}

impl Db {
    /// Open (or create) the database in directory `path` on the real disk.
    pub fn open(path: impl AsRef<Path>, options: Options) -> Result<Db> {
        Self::open_with_fs(path, options, Arc::new(OsFs))
    }

    /// Open a database that lives only in memory. Nothing is ever written to
    /// disk; everything is lost when the `Db` is dropped.
    pub fn open_in_memory(options: Options) -> Result<Db> {
        Self::open_with_fs("/strata-in-memory", options, Arc::new(SimFs::new()))
    }

    /// Open using a specific [`FileSystem`] (tests use [`SimFs`] to simulate
    /// crashes).
    pub fn open_with_fs(
        path: impl AsRef<Path>,
        options: Options,
        fs: Arc<dyn FileSystem>,
    ) -> Result<Db> {
        if options.sync == SyncPolicy::EveryN(0) {
            return Err(Error::InvalidArgument("SyncPolicy::EveryN(0)".into()));
        }
        // Checked now, not at the first flush: a bad option there would
        // poison a database that had been accepting writes.
        options.table.validate()?;
        let dir = path.as_ref().to_path_buf();
        fs.create_dir_all(&dir)?;
        let lock = fs.lock(&dir.join(LOCK_FILE))?;
        let r = recovery::recover(fs.as_ref(), &dir, &options)?;

        Ok(Db {
            inner: Arc::new(DbInner {
                options,
                fs,
                dir,
                state: RwLock::new(Arc::new(r.state)),
                writer: Mutex::new(Writer {
                    last_seqno: r.last_seqno,
                    wal: r.wal,
                    log_number: r.log_number,
                    next_file_number: r.next_file_number,
                    writes_since_sync: 0,
                    poisoned: None,
                    lock: Some(lock),
                }),
                visible_seqno: AtomicU64::new(r.last_seqno),
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

    /// Apply every operation in `batch` atomically and durably (per the
    /// configured [`SyncPolicy`]).
    pub fn write(&self, batch: WriteBatch) -> Result<()> {
        for op in &batch.ops {
            validate(op)?;
        }

        let mut w = self.inner.writer.lock();
        // Checked under the lock: `close` takes the same lock.
        self.check_open()?;
        if let Some(reason) = &w.poisoned {
            return Err(Error::Poisoned(reason.clone()));
        }
        if batch.is_empty() {
            return Ok(());
        }
        // Flush *before* logging: if the flush fails, this batch was never
        // logged or applied, and the caller gets the error.
        let mem_size = self.inner.state.read().mem.approximate_size();
        if mem_size > 0 && mem_size >= self.inner.options.memtable_size {
            flush::flush_locked(&self.inner, &mut w)?;
        }
        let count = batch.len() as u64;
        if w.last_seqno + count > MAX_SEQNO {
            return Err(Error::InvalidArgument("sequence numbers exhausted".into()));
        }
        let first = w.last_seqno + 1;

        // Log first. Any failure poisons the database.
        let logged = w.wal.add_record(&batch.encode(first)).and_then(|()| {
            let sync_now = match self.inner.options.sync {
                SyncPolicy::Always => true,
                SyncPolicy::EveryN(n) => {
                    w.writes_since_sync += 1;
                    w.writes_since_sync >= n
                }
                SyncPolicy::Never => false,
            };
            if sync_now {
                w.writes_since_sync = 0;
                w.wal.sync()?;
            }
            Ok(())
        });
        if let Err(e) = logged {
            w.poisoned = Some(e.to_string());
            return Err(e);
        }

        // The memtable only changes under the writer lock, which we hold.
        let mem = self.inner.state.read().mem.clone();
        for (i, op) in batch.ops.iter().enumerate() {
            apply(&mem, first + i as u64, op);
        }
        w.last_seqno += count;
        // Publish: everything inserted above "happens before" any reader that
        // observes this value with Acquire.
        self.inner
            .visible_seqno
            .store(w.last_seqno, Ordering::Release);
        Ok(())
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.check_open()?;
        let (snapshot, state) = self.read_view();
        for mem in std::iter::once(&state.mem).chain(&state.imm) {
            match mem.get(key, snapshot) {
                Lookup::Found(v) => return Ok(Some(v)),
                Lookup::Deleted => return Ok(None), // hides every older source
                Lookup::NotFound => {}
            }
        }
        for table in &state.tables {
            match table.get(key, snapshot)? {
                Lookup::Found(v) => return Ok(Some(v)),
                Lookup::Deleted => return Ok(None),
                Lookup::NotFound => {}
            }
        }
        Ok(None)
    }

    /// All live key/value pairs with keys inside the bounds, in key order.
    ///
    /// Collects every source's matches into memory, then merges; streaming
    /// iterators replace this in milestone 6.
    pub fn scan(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.check_open()?;
        let (snapshot, state) = self.read_view();
        // Sources hold disjoint seqno ranges, newer sources higher ones, so
        // the first source (newest first) that has a key has its newest
        // visible version. Tombstones take part in the merge, so an older
        // value under a newer tombstone stays hidden; drop them only after.
        let mut merged: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
        let mut take = |rows: Vec<crate::memtable::ScanRow>| {
            for (k, v) in rows {
                merged.entry(k).or_insert(v);
            }
        };
        take(state.mem.scan(start, end, snapshot));
        if let Some(imm) = &state.imm {
            take(imm.scan(start, end, snapshot));
        }
        for table in &state.tables {
            take(table.scan(start, end, snapshot)?);
        }
        Ok(merged
            .into_iter()
            .filter_map(|(k, v)| v.map(|v| (k, v)))
            .collect())
    }

    /// A snapshot seqno and the sources to search.
    ///
    /// The seqno is loaded **first**. If the state were taken first, a flush
    /// could swap in a new memtable, a writer could put seqno `s` into it and
    /// publish `s`, and we would then read with snapshot `s` from the old
    /// state, which doesn't contain it. The other way round is safe: entries
    /// only move from newer to older sources, and every state keeps `imm`
    /// until its table is in `tables`, so anything at or below the snapshot
    /// is in the state we take.
    fn read_view(&self) -> (SeqNo, Arc<ReadState>) {
        let snapshot = self.inner.visible_seqno.load(Ordering::Acquire);
        let state = self.inner.state.read().clone();
        (snapshot, state)
    }

    /// Write the memtable to an L0 table now, and delete its log. Does
    /// nothing if the memtable is empty. A failure poisons the database, as a
    /// failed log write does.
    pub fn flush(&self) -> Result<()> {
        let mut w = self.inner.writer.lock();
        self.check_open()?;
        if let Some(reason) = &w.poisoned {
            return Err(Error::Poisoned(reason.clone()));
        }
        if self.inner.state.read().mem.is_empty() {
            return Ok(());
        }
        flush::flush_locked(&self.inner, &mut w)
    }

    /// Sync the log, release the directory lock and close. Further operations
    /// return [`Error::Closed`]. Closing twice is allowed.
    pub fn close(&self) -> Result<()> {
        let mut w = self.inner.writer.lock();
        if self.inner.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let synced = if w.poisoned.is_none() {
            w.wal.sync()
        } else {
            Ok(())
        };
        self.inner.closed.store(true, Ordering::Release);
        w.lock = None; // release the directory lock
        synced
    }

    fn check_open(&self) -> Result<()> {
        if self.inner.closed.load(Ordering::Acquire) {
            Err(Error::Closed)
        } else {
            Ok(())
        }
    }
}

fn validate(op: &BatchOp) -> Result<()> {
    let key = op.key();
    if key.is_empty() {
        return Err(Error::InvalidArgument("key must not be empty".into()));
    }
    if key.len() > MAX_KEY_LEN {
        return Err(Error::InvalidArgument(format!(
            "key is {} bytes; the limit is {MAX_KEY_LEN}",
            key.len()
        )));
    }
    if let BatchOp::Put { value, .. } = op
        && value.len() > MAX_VALUE_LEN
    {
        return Err(Error::InvalidArgument(format!(
            "value is {} bytes; the limit is {MAX_VALUE_LEN}",
            value.len()
        )));
    }
    Ok(())
}

pub(crate) fn apply(memtable: &Memtable, seqno: SeqNo, op: &BatchOp) {
    match op {
        BatchOp::Put { key, value } => memtable.insert(seqno, ValueKind::Put, key, value),
        BatchOp::Delete { key } => memtable.insert(seqno, ValueKind::Delete, key, &[]),
    }
}

#[cfg(test)]
mod tests {
    use super::files::log_name;
    use super::*;

    /// A record whose checksum is valid but whose sequence numbers are out of
    /// range must be reported as corruption, not overflow / panic.
    #[test]
    fn replay_rejects_out_of_range_seqno() {
        let fs = SimFs::new();
        let dir = Path::new("/db");
        fs.create_dir_all(dir).unwrap();
        let mut wal = WalWriter::new(fs.create(&dir.join(log_name(1))).unwrap());
        let mut batch = WriteBatch::new();
        batch.put(b"a", b"1").put(b"b", b"2");
        wal.add_record(&batch.encode(u64::MAX - 1)).unwrap();
        wal.sync().unwrap();
        fs.sync_dir(dir).unwrap();

        let result = Db::open_with_fs(dir, Options::default(), Arc::new(fs.clone()));
        assert!(
            matches!(result, Err(Error::Corruption(_))),
            "{:?}",
            result.err()
        );
    }
}

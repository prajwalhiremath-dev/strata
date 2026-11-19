//! The public `Db` handle.
//!
//! **Write path** (under the writer mutex), in this exact order:
//!
//! 1. validate the whole batch (nothing is applied if any key/value is bad)
//! 2. refuse if closed or poisoned
//! 3. assign consecutive sequence numbers
//! 4. append the encoded batch to the WAL, and sync per [`SyncPolicy`]
//! 5. insert every entry into the memtable
//! 6. **publish** by storing the last seqno into `visible_seqno`
//!
//! The log comes first so that anything a reader could ever see is already in
//! the log. If step 4 fails the database is *poisoned*: the log may now end in
//! a partial record, and a later write appended after it would be lost on
//! recovery, so further writes are refused (chapter 02).
//!
//! **Open path**: lock the directory, replay every log in file-number order,
//! then *checkpoint*: write the recovered state into a brand-new log, sync it
//! and the directory, and delete the old logs. We never append to a log that
//! existed before `open`. (From milestone 4 a flush to disk replaces this.)

use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::batch::{BatchOp, MAX_VALUE_LEN, WriteBatch};
use crate::fs::{FileLock, FileSystem, OsFs, SimFs};
use crate::key::{MAX_SEQNO, SeqNo, ValueKind};
use crate::memtable::{Lookup, Memtable};
use crate::options::{Options, SyncPolicy};
use crate::wal::{WalEnd, WalWriter, read_records};
use crate::{Error, Result};

/// Longest accepted key. SSTables (milestone 3) store key lengths as `u16`.
pub const MAX_KEY_LEN: usize = 65_535;

const LOCK_FILE: &str = "LOCK";

fn log_name(number: u64) -> String {
    format!("{number:06}.log")
}

fn parse_log_name(name: &str) -> Option<u64> {
    name.strip_suffix(".log")?.parse().ok()
}

/// A handle to an open database. Cheap to clone; clones share the same data
/// and can be sent to other threads.
#[derive(Clone)]
pub struct Db {
    inner: Arc<DbInner>,
}

struct DbInner {
    options: Options,
    memtable: Memtable,
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
        let dir = path.as_ref().to_path_buf();
        fs.create_dir_all(&dir)?;
        let lock = fs.lock(&dir.join(LOCK_FILE))?;

        let mut logs: Vec<(u64, String)> = fs
            .list(&dir)?
            .into_iter()
            .filter_map(|name| parse_log_name(&name).map(|n| (n, name)))
            .collect();
        logs.sort();

        let memtable = Memtable::new();
        let max_seqno = replay(fs.as_ref(), &dir, &logs, &memtable, &options)?;
        let next_number = logs.last().map_or(1, |(n, _)| n + 1);
        let wal = checkpoint(fs.as_ref(), &dir, &logs, &memtable, next_number)?;

        Ok(Db {
            inner: Arc::new(DbInner {
                options,
                memtable,
                writer: Mutex::new(Writer {
                    last_seqno: max_seqno,
                    wal,
                    writes_since_sync: 0,
                    poisoned: None,
                    lock: Some(lock),
                }),
                visible_seqno: AtomicU64::new(max_seqno),
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

        for (i, op) in batch.ops.iter().enumerate() {
            apply(&self.inner.memtable, first + i as u64, op);
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

fn apply(memtable: &Memtable, seqno: SeqNo, op: &BatchOp) {
    match op {
        BatchOp::Put { key, value } => memtable.insert(seqno, ValueKind::Put, key, value),
        BatchOp::Delete { key } => memtable.insert(seqno, ValueKind::Delete, key, &[]),
    }
}

/// Replay `logs` (sorted by number) into `memtable`; returns the highest
/// sequence number seen.
///
/// Entries keep their *original* seqnos, so replaying the same record twice
/// (an old log and its checkpoint copy) inserts identical internal keys:
/// replay is idempotent.
fn replay(
    fs: &dyn FileSystem,
    dir: &Path,
    logs: &[(u64, String)],
    memtable: &Memtable,
    options: &Options,
) -> Result<SeqNo> {
    let mut max_seqno = 0;
    for (_, name) in logs {
        let buf = fs.read(&dir.join(name))?;
        let (records, end) = read_records(&buf);
        for record in records {
            // The CRC matched, so undecodable contents are real corruption.
            let (first, batch) = WriteBatch::decode(&record)?;
            let in_range = first >= 1
                && first
                    .checked_add(batch.len() as u64)
                    .is_some_and(|end| end - 1 <= MAX_SEQNO);
            if !in_range {
                return Err(Error::Corruption(format!(
                    "{name}: batch with out-of-range sequence number {first}"
                )));
            }
            for (i, op) in batch.ops.iter().enumerate() {
                let seqno = first + i as u64;
                apply(memtable, seqno, op);
                max_seqno = max_seqno.max(seqno);
            }
        }
        match end {
            // A torn tail is what a crash mid-write looks like: fine.
            WalEnd::Clean | WalEnd::TornTail { .. } => {}
            WalEnd::Corrupt { offset } if options.paranoid_checks => {
                return Err(Error::Corruption(format!(
                    "{name}: damaged record at offset {offset} followed by valid records"
                )));
            }
            // Point-in-time recovery: keep what came before the damage and
            // ignore everything after it, in this log and later ones.
            WalEnd::Corrupt { .. } => break,
        }
    }
    Ok(max_seqno)
}

/// Write the recovered state into a new log, make it durable, then delete
/// the old logs. Safe to crash at any step:
///
/// - before the new log is durable: old logs are intact, replay them again;
/// - after, but before all old logs are gone: old + new logs replay to the
///   same state (identical seqnos).
fn checkpoint(
    fs: &dyn FileSystem,
    dir: &Path,
    old_logs: &[(u64, String)],
    memtable: &Memtable,
    number: u64,
) -> Result<WalWriter> {
    let path: PathBuf = dir.join(log_name(number));
    let mut wal = WalWriter::new(fs.create(&path)?);
    for (ikey, value) in memtable.entries() {
        let mut b = WriteBatch::new();
        match ikey.kind {
            ValueKind::Put => b.put(&ikey.user_key, &value),
            ValueKind::Delete => b.delete(&ikey.user_key),
        };
        wal.add_record(&b.encode(ikey.seqno))?;
    }
    wal.sync()?;
    fs.sync_dir(dir)?; // the new log's directory entry is durable
    for (_, name) in old_logs {
        fs.remove(&dir.join(name))?;
    }
    if !old_logs.is_empty() {
        fs.sync_dir(dir)?; // the removals are durable
    }
    Ok(wal)
}

#[cfg(test)]
mod tests {
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

//! Tuning knobs passed to [`crate::Db::open`].

use crate::sstable::TableOptions;

/// When the write-ahead log is synced (made durable) during writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPolicy {
    /// Sync after every write call. A write that returns `Ok` survives a
    /// crash. Slowest: every write waits for the disk (see chapter 00).
    Always,
    /// Sync after every `n`-th write call. A crash can lose up to `n - 1`
    /// acknowledged writes (but never part of a batch). `n` must be ≥ 1.
    EveryN(u32),
    /// Never sync during writes (only on `close`). A crash can lose any
    /// writes since the last sync; what survives is still a prefix of the
    /// history with whole batches. Fastest.
    Never,
}

/// Database options. Use `Options::default()` and override fields as needed.
#[derive(Debug, Clone)]
pub struct Options {
    /// Approximate memtable size (bytes) at which it is frozen and flushed to
    /// disk as an L0 table. Bigger means fewer, larger files but more memory
    /// and a longer WAL replay after a crash. Default: 4 MiB.
    pub memtable_size: usize,

    /// Durability vs speed trade-off for writes. Default: `Always`.
    pub sync: SyncPolicy,

    /// If true, `open` fails with `Error::Corruption` when a log is damaged
    /// *in the middle* (valid records after the damage). If false, recovery
    /// keeps everything before the damage and discards the rest
    /// ("point-in-time recovery"). A torn tail from a crash is never an
    /// error either way. Default: false.
    pub paranoid_checks: bool,

    /// How tables are laid out: block size, restart interval, bloom filter
    /// bits per key (chapters 03 and 04).
    pub table: TableOptions,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            memtable_size: 4 * 1024 * 1024,
            sync: SyncPolicy::Always,
            paranoid_checks: false,
            table: TableOptions::default(),
        }
    }
}

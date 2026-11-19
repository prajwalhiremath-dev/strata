//! Tuning knobs passed to [`crate::Db::open_in_memory`] (and `Db::open` later).

/// Database options. Use `Options::default()` and override fields as needed.
#[derive(Debug, Clone)]
pub struct Options {
    /// Approximate memtable size (bytes) at which it is frozen and flushed to
    /// disk. Bigger means fewer, larger files but more memory and a longer
    /// WAL replay after a crash. Used from milestone 4 (flush).
    pub memtable_size: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            memtable_size: 4 * 1024 * 1024,
        }
    }
}

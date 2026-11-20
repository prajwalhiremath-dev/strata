//! The filesystem abstraction every byte of strata's I/O goes through.
//!
//! Two implementations:
//!
//! - [`OsFs`]: the real disk, via `std::fs`.
//! - [`SimFs`]: an in-memory filesystem that remembers which bytes and which
//!   directory entries have been *synced*, and can simulate a crash (power
//!   loss) by throwing away everything that was not. Crash tests run against
//!   it, so we can "pull the plug" thousands of times per second.
//!
//! The trait is deliberately small: only what the engine needs. Each method
//! maps to one OS concept explained in chapter 02.

use std::path::Path;

use crate::Result;

mod os;
mod sim;

pub use os::OsFs;
pub use sim::SimFs;

/// A file opened for appending (the WAL, and SSTables while being written).
pub trait WritableFile: Send {
    /// Append bytes at the end. They may sit in OS buffers until [`sync`].
    ///
    /// [`sync`]: WritableFile::sync
    fn append(&mut self, data: &[u8]) -> Result<()>;

    /// Make everything appended so far durable (survives power loss).
    fn sync(&mut self) -> Result<()>;
}

/// A file opened for reading at arbitrary offsets (SSTables).
///
/// `&self`, not `&mut self`: there is no shared cursor, so many readers can
/// use one handle at once (POSIX `pread`).
pub trait RandomAccessFile: Send + Sync {
    /// Exactly `len` bytes starting at `offset`. Reading past the end is an
    /// error, never a short read.
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>>;

    /// Current length of the file in bytes.
    fn size(&self) -> Result<u64>;
}

/// An exclusive lock on a file; released when dropped.
pub trait FileLock: Send {}

/// Filesystem operations used by strata.
pub trait FileSystem: Send + Sync + std::fmt::Debug {
    fn create_dir_all(&self, dir: &Path) -> Result<()>;

    /// Create `path` (truncating it if it exists) for appending.
    fn create(&self, path: &Path) -> Result<Box<dyn WritableFile>>;

    /// Read a whole file.
    fn read(&self, path: &Path) -> Result<Vec<u8>>;

    /// Open an existing file for random-access reads.
    fn open_random(&self, path: &Path) -> Result<Box<dyn RandomAccessFile>>;

    /// Names (not paths) of the files in `dir`, sorted.
    fn list(&self, dir: &Path) -> Result<Vec<String>>;

    fn remove(&self, path: &Path) -> Result<()>;

    /// Atomically rename `from` to `to`, replacing `to` if it exists.
    /// Like a create or remove, it is durable only after [`sync_dir`].
    ///
    /// [`sync_dir`]: FileSystem::sync_dir
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;

    /// Make the directory's entries durable: files created in or removed from
    /// `dir` are only guaranteed to stay created/removed/renamed after this.
    fn sync_dir(&self, dir: &Path) -> Result<()>;

    /// Take an exclusive lock on `path` (creating it if needed). Fails with
    /// [`crate::Error::AlreadyOpen`] if someone else holds it.
    fn lock(&self, path: &Path) -> Result<Box<dyn FileLock>>;
}

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

/// An exclusive lock on a file; released when dropped.
pub trait FileLock: Send {}

/// Filesystem operations used by strata.
pub trait FileSystem: Send + Sync + std::fmt::Debug {
    fn create_dir_all(&self, dir: &Path) -> Result<()>;

    /// Create `path` (truncating it if it exists) for appending.
    fn create(&self, path: &Path) -> Result<Box<dyn WritableFile>>;

    /// Read a whole file.
    fn read(&self, path: &Path) -> Result<Vec<u8>>;

    /// Names (not paths) of the files in `dir`, sorted.
    fn list(&self, dir: &Path) -> Result<Vec<String>>;

    fn remove(&self, path: &Path) -> Result<()>;

    /// Make the directory's entries durable: files created in or removed from
    /// `dir` are only guaranteed to stay created/removed after this.
    fn sync_dir(&self, dir: &Path) -> Result<()>;

    /// Take an exclusive lock on `path` (creating it if needed). Fails with
    /// [`crate::Error::AlreadyOpen`] if someone else holds it.
    fn lock(&self, path: &Path) -> Result<Box<dyn FileLock>>;
}

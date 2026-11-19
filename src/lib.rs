//! # strata
//!
//! strata is an embedded key-value storage engine built on a log-structured
//! merge-tree (LSM). Writes go to an append-only log and an in-memory sorted
//! table; full tables are flushed to immutable sorted files on disk, and a
//! background process merges those files level by level. It is written from
//! scratch as a learning project: every layer has a matching chapter in
//! `docs/learn/` explaining what it does and why it is built that way.
//!
//! ```
//! use strata::{Db, Options};
//!
//! let db = Db::open_in_memory(Options::default())?;
//! db.put(b"hello", b"world")?;
//! assert_eq!(db.get(b"hello")?, Some(b"world".to_vec()));
//! # Ok::<(), strata::Error>(())
//! ```

mod batch;
mod db;
mod error;
pub mod fs;
pub mod key;
mod memtable;
mod options;
#[allow(dead_code)] // wired into Db in Task 4 of milestone 2
mod wal;

pub use batch::{MAX_VALUE_LEN, WriteBatch};
pub use db::{Db, MAX_KEY_LEN};
pub use error::{Error, Result};
pub use options::Options;

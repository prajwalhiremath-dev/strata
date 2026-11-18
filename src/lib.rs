//! # strata
//!
//! strata is an embedded key-value storage engine built on a log-structured
//! merge-tree (LSM). Writes go to an append-only log and an in-memory sorted
//! table; full tables are flushed to immutable sorted files on disk, and a
//! background process merges those files level by level. It is written from
//! scratch as a learning project: every layer has a matching chapter in
//! `docs/learn/` explaining what it does and why it is built that way.

mod error;

pub use error::{Error, Result};

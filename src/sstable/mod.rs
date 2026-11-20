//! SSTables: immutable, sorted, checksummed files of internal keys.
//!
//! ```text
//! [data block][crc] ... [data block][crc] [index block][crc] [footer: 48 bytes]
//! ```
//!
//! - A **data block** (about `block_size` bytes) holds sorted entries with
//!   prefix-compressed keys and restart points for binary search.
//! - The **index block** has one entry per data block: that block's last key
//!   -> where the block is in the file.
//! - The **footer** says where the index is, and is checked by its own CRC
//!   and a magic number.
//!
//! Chapter 03 walks through every byte.

// Used by the table builder and reader (next tasks); tests only until then.
#[allow(dead_code)]
pub(crate) mod block;
mod builder;
#[allow(dead_code)] // the decoding half is used by the reader (next task)
pub(crate) mod format;

pub use builder::{TableBuilder, TableMeta, TableOptions};

//! SSTables: immutable, sorted, checksummed files of internal keys.
//!
//! ```text
//! [data block][crc] ... [data block][crc] [filter][crc] [index block][crc] [footer: 56 bytes]
//! ```
//!
//! - A **data block** (about `block_size` bytes) holds sorted entries with
//!   prefix-compressed keys and restart points for binary search.
//! - The **index block** has one entry per data block: that block's last key
//!   -> where the block is in the file.
//! - The **filter** block is a bloom filter over the table's user keys
//!   (chapter 04), so a lookup for an absent key usually reads no data block.
//! - The **footer** says where the index and filter are, records the table's
//!   highest sequence number, and is checked by its own CRC and a magic
//!   number.
//!
//! Chapter 03 walks through every byte.

pub(crate) mod block;
pub(crate) mod bloom;
mod builder;
pub(crate) mod format;
mod table;
#[cfg(test)]
mod tests;

pub use builder::{MAX_BLOCK_SIZE, TableBuilder, TableMeta, TableOptions};
pub use table::{Table, TableIter};

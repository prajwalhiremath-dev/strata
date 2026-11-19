//! WAL on-disk format (LevelDB-style).
//!
//! The file is a sequence of 32 KiB **blocks**. Each block holds one or more
//! **records**; a payload too big for the rest of a block is split into
//! fragments across blocks.
//!
//! ```text
//! record:  +-----------+------------+----------+-----------------+
//!          | crc32 u32 | length u16 | type u8  | payload (length)|
//!          +-----------+------------+----------+-----------------+
//!            4 bytes      2 bytes     1 byte     ≤ 32 KiB − 7
//!            little-endian; crc covers type byte ++ payload
//!
//! block:   [record][record][record]...[zero padding if < 7 bytes left]
//! ```
//!
//! Why blocks: a reader can always find the start of the next block, so
//! damage is contained and we can look *past* it to classify it (chapter 02).

/// Size of one block.
pub(crate) const BLOCK_SIZE: usize = 32 * 1024;

/// crc (4) + length (2) + type (1).
pub(crate) const HEADER_SIZE: usize = 7;

/// How a payload was split across blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum RecordType {
    /// The whole payload in one fragment.
    Full = 1,
    /// First fragment of a payload that continues in the next block.
    First = 2,
    /// A fragment in the middle.
    Middle = 3,
    /// The last fragment.
    Last = 4,
}

impl RecordType {
    pub(crate) fn from_u8(b: u8) -> Option<Self> {
        match b {
            1 => Some(RecordType::Full),
            2 => Some(RecordType::First),
            3 => Some(RecordType::Middle),
            4 => Some(RecordType::Last),
            _ => None,
        }
    }
}

/// Checksum over `type ++ payload`. Including the type byte means a flipped
/// type (e.g. Full→First) is detected too.
pub(crate) fn checksum(kind: u8, payload: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(&[kind]);
    h.update(payload);
    h.finalize()
}

//! Fixed pieces of the table format: block handles, block trailers and the
//! footer.
//!
//! ```text
//! footer (48 bytes, little-endian):
//! index_offset u64 | index_size u64 | filter_offset u64 | filter_size u64
//! | version u32 | crc32 u32 (over bytes 0..36) | magic u64
//! ```

use crate::coding::{get_varint64, put_varint64};
use crate::{Error, Result};

pub(crate) const FOOTER_LEN: usize = 48;
pub(crate) const FORMAT_VERSION: u32 = 1;
pub(crate) const MAGIC: u64 = u64::from_le_bytes(*b"strataT1");
/// Every block is followed by a CRC32 of its contents.
pub(crate) const BLOCK_TRAILER_LEN: usize = 4;

/// Where a block lives in the file. `size` excludes the 4-byte trailer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlockHandle {
    pub(crate) offset: u64,
    pub(crate) size: u64,
}

impl BlockHandle {
    pub(crate) fn encode_to(&self, dst: &mut Vec<u8>) {
        put_varint64(dst, self.offset);
        put_varint64(dst, self.size);
    }

    pub(crate) fn decode(src: &[u8]) -> Result<(BlockHandle, usize)> {
        let (offset, a) = get_varint64(src)?;
        let (size, b) = get_varint64(&src[a..])?;
        Ok((BlockHandle { offset, size }, a + b))
    }
}

/// The last 48 bytes of every table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Footer {
    pub(crate) index: BlockHandle,
    /// Reserved for the bloom filter (milestone 4); always (0, 0) for now.
    pub(crate) filter: BlockHandle,
}

fn u64_at(buf: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(buf[at..at + 8].try_into().expect("8 bytes"))
}

fn u32_at(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(buf[at..at + 4].try_into().expect("4 bytes"))
}

impl Footer {
    pub(crate) fn encode(&self) -> [u8; FOOTER_LEN] {
        let mut buf = [0u8; FOOTER_LEN];
        buf[0..8].copy_from_slice(&self.index.offset.to_le_bytes());
        buf[8..16].copy_from_slice(&self.index.size.to_le_bytes());
        buf[16..24].copy_from_slice(&self.filter.offset.to_le_bytes());
        buf[24..32].copy_from_slice(&self.filter.size.to_le_bytes());
        buf[32..36].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        let crc = crc32fast::hash(&buf[..36]);
        buf[36..40].copy_from_slice(&crc.to_le_bytes());
        buf[40..48].copy_from_slice(&MAGIC.to_le_bytes());
        buf
    }

    /// Checks, in order: length, magic (is this a strata table at all?),
    /// CRC (is the footer intact?), version, reserved filter handle.
    pub(crate) fn decode(buf: &[u8]) -> Result<Footer> {
        let corrupt = |m: &str| Error::Corruption(format!("footer: {m}"));
        if buf.len() != FOOTER_LEN {
            return Err(corrupt("wrong length"));
        }
        if u64_at(buf, 40) != MAGIC {
            return Err(corrupt("bad magic number (not a strata table?)"));
        }
        if u32_at(buf, 36) != crc32fast::hash(&buf[..36]) {
            return Err(corrupt("checksum mismatch"));
        }
        let version = u32_at(buf, 32);
        if version != FORMAT_VERSION {
            return Err(corrupt(&format!("unsupported format version {version}")));
        }
        let filter = BlockHandle {
            offset: u64_at(buf, 16),
            size: u64_at(buf, 24),
        };
        if filter != (BlockHandle { offset: 0, size: 0 }) {
            return Err(corrupt("filter handle is reserved and must be zero"));
        }
        Ok(Footer {
            index: BlockHandle {
                offset: u64_at(buf, 0),
                size: u64_at(buf, 8),
            },
            filter,
        })
    }
}

/// Append the CRC32 of `buf`'s current contents.
pub(crate) fn append_trailer(buf: &mut Vec<u8>) {
    let crc = crc32fast::hash(buf);
    buf.extend_from_slice(&crc.to_le_bytes());
}

/// Split `contents ++ crc` and verify the CRC; returns the contents.
pub(crate) fn check_trailer(buf: &[u8]) -> Result<&[u8]> {
    let split = buf
        .len()
        .checked_sub(BLOCK_TRAILER_LEN)
        .ok_or_else(|| Error::Corruption("block shorter than its trailer".into()))?;
    let (contents, crc) = buf.split_at(split);
    if u32_at(crc, 0) != crc32fast::hash(contents) {
        return Err(Error::Corruption("block checksum mismatch".into()));
    }
    Ok(contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    fn is_corrupt<T>(r: Result<T>) -> bool {
        matches!(r, Err(Error::Corruption(_)))
    }

    fn footer() -> Footer {
        Footer {
            index: BlockHandle {
                offset: 1234,
                size: 567,
            },
            filter: BlockHandle { offset: 0, size: 0 },
        }
    }

    /// Re-seal bytes 0..36 with a fresh CRC, so only the field we changed is wrong.
    fn reseal(buf: &mut [u8; FOOTER_LEN]) {
        let crc = crc32fast::hash(&buf[..36]);
        buf[36..40].copy_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn footer_round_trip() {
        let f = Footer::decode(&footer().encode()).unwrap();
        assert_eq!(f.index, footer().index);
        assert_eq!(f.filter, footer().filter);
    }

    #[test]
    fn footer_is_48_bytes_and_ends_with_magic() {
        let buf = footer().encode();
        assert_eq!(buf.len(), 48);
        assert_eq!(&buf[40..], b"strataT1");
        assert_eq!(MAGIC.to_le_bytes(), *b"strataT1");
    }

    #[test]
    fn footer_rejects() {
        let good = footer().encode();
        assert!(is_corrupt(Footer::decode(&good[1..])), "wrong length");

        let mut bad = good;
        bad[47] ^= 1;
        assert!(is_corrupt(Footer::decode(&bad)), "magic");

        let mut bad = good;
        bad[32..36].copy_from_slice(&2u32.to_le_bytes());
        reseal(&mut bad);
        assert!(is_corrupt(Footer::decode(&bad)), "version");

        for i in 0..40 {
            let mut bad = good;
            bad[i] ^= 1;
            assert!(is_corrupt(Footer::decode(&bad)), "flip at {i}");
        }

        let mut bad = good;
        bad[16..24].copy_from_slice(&7u64.to_le_bytes());
        reseal(&mut bad);
        assert!(is_corrupt(Footer::decode(&bad)), "nonzero filter handle");
    }

    #[test]
    fn handle_round_trip_and_truncated() {
        for h in [
            BlockHandle { offset: 0, size: 0 },
            BlockHandle {
                offset: 300,
                size: 4096,
            },
            BlockHandle {
                offset: u64::MAX,
                size: u64::MAX,
            },
        ] {
            let mut buf = Vec::new();
            h.encode_to(&mut buf);
            assert_eq!(BlockHandle::decode(&buf).unwrap(), (h, buf.len()));
            assert!(is_corrupt(BlockHandle::decode(&buf[..buf.len() - 1])));
        }
    }

    #[test]
    fn block_trailer_round_trip() {
        let mut buf = b"contents".to_vec();
        append_trailer(&mut buf);
        assert_eq!(buf.len(), 8 + BLOCK_TRAILER_LEN);
        assert_eq!(check_trailer(&buf).unwrap(), b"contents");
        buf[0] ^= 1;
        assert!(is_corrupt(check_trailer(&buf)));
        assert!(is_corrupt(check_trailer(&[1, 2, 3])));
    }
}

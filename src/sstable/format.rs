//! Fixed pieces of the table format: block handles, block trailers and the
//! footer.
//!
//! ```text
//! footer (56 bytes, little-endian):
//! index_offset u64 | index_size u64 | filter_offset u64 | filter_size u64
//! | max_seqno u64 | version u32 | crc32 u32 (over bytes 0..44) | magic u64
//! ```
//!
//! Version 1 (milestone 3) was 48 bytes, without `max_seqno`. It is no longer
//! read: there are no version 1 tables worth keeping.

use crate::coding::{get_varint64, put_varint64};
use crate::key::SeqNo;
use crate::{Error, Result};

pub(crate) const FOOTER_LEN: usize = 56;
pub(crate) const FORMAT_VERSION: u32 = 2;
/// Bytes covered by the footer's CRC: everything before it.
const CRC_AT: usize = 44;
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

/// The last 56 bytes of every table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Footer {
    pub(crate) index: BlockHandle,
    /// The bloom filter block, or (0, 0) when the table has none.
    pub(crate) filter: BlockHandle,
    /// Highest sequence number of any entry (0 for an empty table). Lets
    /// `open` restore the sequence counter after every log is gone.
    pub(crate) max_seqno: SeqNo,
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
        buf[32..40].copy_from_slice(&self.max_seqno.to_le_bytes());
        buf[40..44].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        let crc = crc32fast::hash(&buf[..CRC_AT]);
        buf[44..48].copy_from_slice(&crc.to_le_bytes());
        buf[48..56].copy_from_slice(&MAGIC.to_le_bytes());
        buf
    }

    /// Checks, in order: length, magic (is this a strata table at all?),
    /// CRC (is the footer intact?), version.
    pub(crate) fn decode(buf: &[u8]) -> Result<Footer> {
        let corrupt = |m: &str| Error::Corruption(format!("footer: {m}"));
        if buf.len() != FOOTER_LEN {
            return Err(corrupt("wrong length"));
        }
        if u64_at(buf, 48) != MAGIC {
            return Err(corrupt("bad magic number (not a strata table?)"));
        }
        if u32_at(buf, 44) != crc32fast::hash(&buf[..CRC_AT]) {
            if is_v1_footer(&buf[FOOTER_LEN - V1_FOOTER_LEN..]) {
                return Err(corrupt("unsupported format version 1"));
            }
            return Err(corrupt("checksum mismatch"));
        }
        let version = u32_at(buf, 40);
        if version != FORMAT_VERSION {
            return Err(corrupt(&format!("unsupported format version {version}")));
        }
        Ok(Footer {
            index: BlockHandle {
                offset: u64_at(buf, 0),
                size: u64_at(buf, 8),
            },
            filter: BlockHandle {
                offset: u64_at(buf, 16),
                size: u64_at(buf, 24),
            },
            max_seqno: u64_at(buf, 32),
        })
    }
}

const V1_FOOTER_LEN: usize = 48;

/// A milestone 3 footer is shorter, so its fields sit at different offsets
/// and the v2 CRC check fails. Recognise it to give a useful error instead of
/// "checksum mismatch".
fn is_v1_footer(buf: &[u8]) -> bool {
    u32_at(buf, 32) == 1 && u32_at(buf, 36) == crc32fast::hash(&buf[..36])
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

    fn message<T: std::fmt::Debug>(r: Result<T>) -> String {
        r.unwrap_err().to_string()
    }

    fn footer() -> Footer {
        Footer {
            index: BlockHandle {
                offset: 1234,
                size: 567,
            },
            filter: BlockHandle {
                offset: 900,
                size: 126,
            },
            max_seqno: 0x0123_4567_89ab,
        }
    }

    /// Re-seal bytes 0..44 with a fresh CRC, so only the field we changed is wrong.
    fn reseal(buf: &mut [u8; FOOTER_LEN]) {
        let crc = crc32fast::hash(&buf[..CRC_AT]);
        buf[44..48].copy_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn footer_round_trip() {
        assert_eq!(Footer::decode(&footer().encode()).unwrap(), footer());
        let empty = Footer {
            filter: BlockHandle { offset: 0, size: 0 },
            max_seqno: 0,
            ..footer()
        };
        assert_eq!(Footer::decode(&empty.encode()).unwrap(), empty);
    }

    #[test]
    fn footer_is_56_bytes_and_ends_with_magic() {
        let buf = footer().encode();
        assert_eq!(buf.len(), 56);
        assert_eq!(&buf[32..40], &0x0123_4567_89abu64.to_le_bytes());
        assert_eq!(&buf[40..44], &2u32.to_le_bytes());
        assert_eq!(&buf[44..48], &crc32fast::hash(&buf[..44]).to_le_bytes());
        assert_eq!(&buf[48..], b"strataT1");
        assert_eq!(MAGIC.to_le_bytes(), *b"strataT1");
    }

    #[test]
    fn footer_rejects() {
        let good = footer().encode();
        assert!(is_corrupt(Footer::decode(&good[1..])), "wrong length");

        let mut bad = good;
        bad[55] ^= 1;
        assert!(is_corrupt(Footer::decode(&bad)), "magic");

        let mut bad = good;
        bad[40..44].copy_from_slice(&3u32.to_le_bytes());
        reseal(&mut bad);
        assert!(message(Footer::decode(&bad)).contains("unsupported format version 3"));

        for i in 0..48 {
            let mut bad = good;
            bad[i] ^= 1;
            assert!(is_corrupt(Footer::decode(&bad)), "flip at {i}");
        }
    }

    #[test]
    fn version_1_footer_is_rejected() {
        // The milestone 3 layout: no max_seqno, version at 32, CRC over 0..36.
        let mut v1 = [0u8; V1_FOOTER_LEN];
        v1[0..8].copy_from_slice(&1234u64.to_le_bytes());
        v1[8..16].copy_from_slice(&567u64.to_le_bytes());
        v1[32..36].copy_from_slice(&1u32.to_le_bytes());
        let crc = crc32fast::hash(&v1[..36]);
        v1[36..40].copy_from_slice(&crc.to_le_bytes());
        v1[40..48].copy_from_slice(&MAGIC.to_le_bytes());
        // A v1 table's last 56 bytes: 8 bytes of block data, then the footer.
        let mut tail = vec![0xAB; 8];
        tail.extend_from_slice(&v1);
        let err = message(Footer::decode(&tail));
        assert!(err.contains("unsupported format version 1"), "{err}");
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

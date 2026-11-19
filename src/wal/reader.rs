//! Reading a WAL back after a restart.

use super::format::{BLOCK_SIZE, HEADER_SIZE, RecordType, checksum};

/// Why reading stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalEnd {
    /// Reached the end of the file at a record boundary.
    Clean,
    /// The log ends with an incomplete or damaged record and nothing valid
    /// after it: the signature of a crash mid-write. Expected; not an error.
    TornTail { offset: usize },
    /// Damage *followed by* valid records. Logs are never appended to after
    /// a restart, so this cannot be a torn write: the data was damaged.
    Corrupt { offset: usize },
}

/// One parsed fragment header + payload.
struct Fragment<'a> {
    kind: RecordType,
    payload: &'a [u8],
}

enum Parse<'a> {
    Ok(Fragment<'a>),
    /// All-zero rest of block: padding or preallocated space.
    Padding,
    Bad,
}

/// Parse the fragment starting at `pos` (which must have at least
/// `HEADER_SIZE` bytes left in its block).
fn parse_at(buf: &[u8], pos: usize) -> Parse<'_> {
    let block_end = (pos / BLOCK_SIZE + 1) * BLOCK_SIZE;
    if buf.len() - pos < HEADER_SIZE {
        return if buf[pos..].iter().all(|&b| b == 0) {
            Parse::Padding
        } else {
            Parse::Bad
        };
    }
    let header = &buf[pos..pos + HEADER_SIZE];
    if header.iter().all(|&b| b == 0) {
        // A zero header is padding only if the rest of the block is zero too.
        let end = block_end.min(buf.len());
        return if buf[pos..end].iter().all(|&b| b == 0) {
            Parse::Padding
        } else {
            Parse::Bad
        };
    }
    let crc = u32::from_le_bytes(header[0..4].try_into().expect("4 bytes"));
    let len = u16::from_le_bytes(header[4..6].try_into().expect("2 bytes")) as usize;
    let type_byte = header[6];
    let start = pos + HEADER_SIZE;
    let end = start + len;
    if end > block_end || end > buf.len() {
        return Parse::Bad;
    }
    let Some(kind) = RecordType::from_u8(type_byte) else {
        return Parse::Bad;
    };
    let payload = &buf[start..end];
    if checksum(type_byte, payload) != crc {
        return Parse::Bad;
    }
    Parse::Ok(Fragment { kind, payload })
}

/// Does a valid record *start* (Full or First fragment) appear in any block
/// after the one containing `offset`? Used to tell corruption from a torn tail.
///
/// Only record starts count. A valid Middle/Last fragment may belong to the
/// damaged record itself: disks can persist a multi-block write out of order,
/// so a crash can leave an early fragment damaged and a later one intact.
/// That is still a torn write of the *last* record, not corruption.
///
/// From each later block boundary we walk the fragments in that block, so a
/// record starting mid-block (right after a Last fragment) is found too.
fn valid_record_after(buf: &[u8], offset: usize) -> bool {
    let mut block = (offset / BLOCK_SIZE + 1) * BLOCK_SIZE;
    while block < buf.len() {
        let block_end = (block + BLOCK_SIZE).min(buf.len());
        let mut pos = block;
        while block_end - pos >= HEADER_SIZE {
            match parse_at(buf, pos) {
                Parse::Ok(f) => {
                    if matches!(f.kind, RecordType::Full | RecordType::First) {
                        return true;
                    }
                    pos += HEADER_SIZE + f.payload.len();
                }
                Parse::Padding | Parse::Bad => break,
            }
        }
        block += BLOCK_SIZE;
    }
    false
}

/// Read every complete record from the start of `buf`, stopping at the first
/// problem. Never panics, whatever `buf` contains.
pub(crate) fn read_records(buf: &[u8]) -> (Vec<Vec<u8>>, WalEnd) {
    let mut records = Vec::new();
    // A multi-fragment record being reassembled, and where it started.
    let mut partial: Option<(usize, Vec<u8>)> = None;
    let mut pos = 0;

    let bad_at = loop {
        if pos >= buf.len() {
            match partial {
                // File ended in the middle of a fragmented record.
                Some((start, _)) => break start,
                None => return (records, WalEnd::Clean),
            }
        }
        let left_in_block = BLOCK_SIZE - pos % BLOCK_SIZE;
        if left_in_block < HEADER_SIZE {
            // Block trailer: must be zero padding.
            let end = (pos + left_in_block).min(buf.len());
            if buf[pos..end].iter().any(|&b| b != 0) {
                break partial.as_ref().map_or(pos, |(s, _)| *s);
            }
            pos = end;
            continue;
        }
        let frag = match parse_at(buf, pos) {
            Parse::Ok(f) => f,
            Parse::Padding => {
                if partial.is_some() {
                    break partial.as_ref().map_or(pos, |(s, _)| *s);
                }
                pos = ((pos / BLOCK_SIZE + 1) * BLOCK_SIZE).min(buf.len());
                continue;
            }
            Parse::Bad => break partial.as_ref().map_or(pos, |(s, _)| *s),
        };
        let here = pos;
        pos += HEADER_SIZE + frag.payload.len();
        match (frag.kind, partial.take()) {
            (RecordType::Full, None) => records.push(frag.payload.to_vec()),
            (RecordType::First, None) => partial = Some((here, frag.payload.to_vec())),
            (RecordType::Middle, Some((s, mut data))) => {
                data.extend_from_slice(frag.payload);
                partial = Some((s, data));
            }
            (RecordType::Last, Some((_, mut data))) => {
                data.extend_from_slice(frag.payload);
                records.push(data);
            }
            // Out-of-sequence fragment.
            (_, Some((s, _))) => break s,
            (_, None) => break here,
        }
    };

    let end = if valid_record_after(buf, bad_at) {
        WalEnd::Corrupt { offset: bad_at }
    } else {
        WalEnd::TornTail { offset: bad_at }
    };
    (records, end)
}

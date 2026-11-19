//! Appending records to a WAL file.

use super::format::{BLOCK_SIZE, HEADER_SIZE, RecordType, checksum};
use crate::Result;
use crate::fs::WritableFile;

pub(crate) struct WalWriter {
    file: Box<dyn WritableFile>,
    /// Bytes already used in the current block.
    block_offset: usize,
}

impl WalWriter {
    /// Start writing a *new, empty* log file.
    pub(crate) fn new(file: Box<dyn WritableFile>) -> Self {
        WalWriter {
            file,
            block_offset: 0,
        }
    }

    /// Append one record, fragmenting across blocks as needed.
    ///
    /// The whole record (all fragments and padding) is built in memory and
    /// handed to the OS in one `append`, so a failed append adds nothing.
    /// It is *not* durable until [`WalWriter::sync`].
    pub(crate) fn add_record(&mut self, payload: &[u8]) -> Result<()> {
        let mut out = Vec::with_capacity(payload.len() + 2 * HEADER_SIZE);
        let mut offset = self.block_offset;
        let mut rest = payload;
        let mut first = true;
        loop {
            let left_in_block = BLOCK_SIZE - offset;
            if left_in_block < HEADER_SIZE {
                // Not even room for a header: zero-fill, start a new block.
                out.resize(out.len() + left_in_block, 0);
                offset = 0;
            }
            let room = BLOCK_SIZE - offset - HEADER_SIZE;
            let n = rest.len().min(room);
            let last = n == rest.len();
            let kind = match (first, last) {
                (true, true) => RecordType::Full,
                (true, false) => RecordType::First,
                (false, true) => RecordType::Last,
                (false, false) => RecordType::Middle,
            } as u8;
            let fragment = &rest[..n];
            out.extend_from_slice(&checksum(kind, fragment).to_le_bytes());
            out.extend_from_slice(&(n as u16).to_le_bytes());
            out.push(kind);
            out.extend_from_slice(fragment);
            offset += HEADER_SIZE + n;
            rest = &rest[n..];
            first = false;
            if last {
                break;
            }
        }
        self.file.append(&out)?;
        // Only advance our position once the bytes were accepted.
        self.block_offset = offset;
        Ok(())
    }

    /// Make every record appended so far durable.
    pub(crate) fn sync(&mut self) -> Result<()> {
        self.file.sync()
    }
}

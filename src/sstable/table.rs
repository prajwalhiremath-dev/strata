//! [`Table`]: reads an SSTable written by [`TableBuilder`](super::TableBuilder).

use super::block::{Block, BlockIter};
use super::format::{BLOCK_TRAILER_LEN, BlockHandle, FOOTER_LEN, Footer, check_trailer};
use crate::fs::RandomAccessFile;
use crate::key::{InternalKey, MAX_SEQNO, SeqNo, ValueKind};
use crate::memtable::Lookup;
use crate::{Error, Result};

/// An open, immutable SSTable. The index block stays in memory; data blocks
/// are read from the file on demand, one read per block.
pub struct Table {
    file: Box<dyn RandomAccessFile>,
    /// Blocks live in `[0, data_end)`; the footer follows.
    data_end: u64,
    index: Block,
}

impl Table {
    /// Read and verify the footer and the index block.
    pub fn open(file: Box<dyn RandomAccessFile>) -> Result<Table> {
        let size = file.size()?;
        let data_end = size
            .checked_sub(FOOTER_LEN as u64)
            .ok_or_else(|| Error::Corruption("table: file shorter than a footer".into()))?;
        let footer = Footer::decode(&file.read_at(data_end, FOOTER_LEN)?)?;
        let index = read_block(file.as_ref(), data_end, footer.index)?;
        Ok(Table {
            file,
            data_end,
            index,
        })
    }

    /// A cursor over every entry, in internal-key order. Starts not valid:
    /// call `seek_to_first` or `seek`.
    pub fn iter(&self) -> TableIter<'_> {
        TableIter {
            table: self,
            index: self.index.iter(),
            data: None,
        }
    }

    /// Number of data blocks (= index entries). Walks the index.
    pub fn num_data_blocks(&self) -> Result<usize> {
        let mut it = self.index.iter();
        it.seek_to_first()?;
        let mut n = 0;
        while it.valid() {
            n += 1;
            it.next()?;
        }
        Ok(n)
    }

    /// The newest version of `user_key` with seqno <= `snapshot`.
    #[allow(dead_code)] // the read path calls it from milestone 4
    pub(crate) fn get(&self, user_key: &[u8], snapshot: SeqNo) -> Result<Lookup> {
        // (user_key, snapshot, Put) sorts before every version of user_key
        // that the snapshot can see, and after every newer one. So the first
        // entry >= it is the newest visible version, if user_key has one.
        let target = InternalKey::new(user_key, snapshot.min(MAX_SEQNO), ValueKind::Put).encode();
        let mut it = self.iter();
        it.seek(&target)?;
        if !it.valid() {
            return Ok(Lookup::NotFound);
        }
        let found = InternalKey::decode(it.key())?;
        if found.user_key != user_key {
            return Ok(Lookup::NotFound);
        }
        Ok(match found.kind {
            ValueKind::Put => Lookup::Found(it.value().to_vec()),
            ValueKind::Delete => Lookup::Deleted,
        })
    }

    fn read_block(&self, handle: BlockHandle) -> Result<Block> {
        read_block(self.file.as_ref(), self.data_end, handle)
    }
}

/// Read the block at `handle`, check it lies inside `[0, data_end)`, verify
/// its CRC and decode it. The bounds check comes first, so a corrupt handle
/// can never make us allocate or read more than the file holds.
fn read_block(file: &dyn RandomAccessFile, data_end: u64, handle: BlockHandle) -> Result<Block> {
    let in_bounds = handle
        .offset
        .checked_add(handle.size)
        .and_then(|e| e.checked_add(BLOCK_TRAILER_LEN as u64))
        .is_some_and(|end| end <= data_end);
    if !in_bounds {
        return Err(Error::Corruption(format!(
            "table: block handle ({}, {}) outside the file",
            handle.offset, handle.size
        )));
    }
    let len = handle.size as usize + BLOCK_TRAILER_LEN; // fits: <= data_end, which was read
    let mut buf = file.read_at(handle.offset, len)?;
    check_trailer(&buf)?;
    buf.truncate(handle.size as usize);
    Block::new(buf)
}

/// A two-level cursor: the index iterator picks the data block, the data
/// iterator walks inside it.
pub struct TableIter<'a> {
    table: &'a Table,
    index: BlockIter,
    data: Option<BlockIter>,
}

impl TableIter<'_> {
    pub fn valid(&self) -> bool {
        self.data.as_ref().is_some_and(BlockIter::valid)
    }

    /// The current encoded internal key. Only call when `valid()`.
    pub fn key(&self) -> &[u8] {
        self.data.as_ref().expect("iterator is not valid").key()
    }

    /// The current value. Only call when `valid()`.
    pub fn value(&self) -> &[u8] {
        self.data.as_ref().expect("iterator is not valid").value()
    }

    pub fn seek_to_first(&mut self) -> Result<()> {
        self.data = None;
        self.index.seek_to_first()?;
        if self.load_block()? {
            self.data.as_mut().expect("loaded").seek_to_first()?;
        }
        self.skip_exhausted_blocks()
    }

    /// Position at the first entry >= `target` (internal-key order).
    pub fn seek(&mut self, target: &[u8]) -> Result<()> {
        self.data = None;
        // The first block whose last key is >= target is the only block that
        // can hold the answer's start.
        self.index.seek(target)?;
        if self.load_block()? {
            self.data.as_mut().expect("loaded").seek(target)?;
        }
        self.skip_exhausted_blocks()
    }

    /// Move to the next entry. A cursor rather than an `Iterator`: moving can
    /// fail (a block read or checksum), and `key`/`value` borrow from the
    /// cursor without copying.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<()> {
        if !self.valid() {
            return Ok(());
        }
        self.data.as_mut().expect("valid").next()?;
        self.skip_exhausted_blocks()
    }

    /// Load the block the index iterator points at. Returns false (and
    /// clears `data`) when the index is exhausted.
    fn load_block(&mut self) -> Result<bool> {
        self.data = None;
        if !self.index.valid() {
            return Ok(false);
        }
        let (handle, _) = BlockHandle::decode(self.index.value())?;
        self.data = Some(self.table.read_block(handle)?.iter());
        Ok(true)
    }

    /// While the current block is used up, move to the start of the next.
    fn skip_exhausted_blocks(&mut self) -> Result<()> {
        while self.data.as_ref().is_some_and(|d| !d.valid()) {
            self.index.next()?;
            if self.load_block()? {
                self.data.as_mut().expect("loaded").seek_to_first()?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use crate::fs::{FileSystem, SimFs};
    use crate::key::{InternalKey, MAX_SEQNO, ValueKind};
    use crate::memtable::Lookup;
    use crate::sstable::format::{BlockHandle, Footer};
    use crate::sstable::{TableBuilder, TableOptions};
    use std::path::Path;

    fn key(user: &[u8], seq: u64, kind: ValueKind) -> Vec<u8> {
        InternalKey::new(user, seq, kind).encode()
    }

    fn put(user: &[u8], seq: u64) -> Vec<u8> {
        key(user, seq, ValueKind::Put)
    }

    fn build(entries: &[(Vec<u8>, Vec<u8>)], opts: TableOptions) -> Table {
        let fs = SimFs::default();
        fs.create_dir_all(Path::new("/t")).unwrap();
        let mut b = TableBuilder::new(fs.create(Path::new("/t/1.sst")).unwrap(), opts).unwrap();
        for (k, v) in entries {
            b.add(k, v).unwrap();
        }
        b.finish().unwrap();
        Table::open(fs.open_random(Path::new("/t/1.sst")).unwrap()).unwrap()
    }

    fn open_bytes(bytes: &[u8]) -> Result<Table> {
        let fs = SimFs::default();
        fs.create_dir_all(Path::new("/t")).unwrap();
        fs.create(Path::new("/t/x.sst"))
            .unwrap()
            .append(bytes)
            .unwrap();
        Table::open(fs.open_random(Path::new("/t/x.sst")).unwrap())
    }

    fn scan(t: &Table) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut it = t.iter();
        it.seek_to_first()?;
        let mut out = Vec::new();
        while it.valid() {
            out.push((it.key().to_vec(), it.value().to_vec()));
            it.next()?;
        }
        Ok(out)
    }

    fn opts(block_size: usize, restart_interval: usize) -> TableOptions {
        TableOptions {
            block_size,
            restart_interval,
        }
    }

    #[test]
    fn empty_table() {
        let t = build(&[], TableOptions::default());
        assert!(scan(&t).unwrap().is_empty());
        let mut it = t.iter();
        it.seek(&put(b"a", 1)).unwrap();
        assert!(!it.valid());
        assert_eq!(t.get(b"a", MAX_SEQNO).unwrap(), Lookup::NotFound);
    }

    #[test]
    fn get_basic() {
        let t = build(
            &[
                (put(b"a", 3), b"x".to_vec()),
                (key(b"b", 5, ValueKind::Delete), vec![]),
                (put(b"b", 2), b"y".to_vec()),
            ],
            TableOptions::default(),
        );
        assert_eq!(t.get(b"a", 10).unwrap(), Lookup::Found(b"x".to_vec()));
        assert_eq!(t.get(b"a", 3).unwrap(), Lookup::Found(b"x".to_vec()));
        assert_eq!(
            t.get(b"a", 2).unwrap(),
            Lookup::NotFound,
            "too old a snapshot"
        );
        assert_eq!(t.get(b"b", 10).unwrap(), Lookup::Deleted);
        assert_eq!(t.get(b"b", 4).unwrap(), Lookup::Found(b"y".to_vec()));
        assert_eq!(t.get(b"c", 10).unwrap(), Lookup::NotFound);
        assert_eq!(t.get(b"", 10).unwrap(), Lookup::NotFound);
        assert_eq!(
            t.get(b"aa", 10).unwrap(),
            Lookup::NotFound,
            "user keys must match"
        );
    }

    #[test]
    fn versions_straddle_blocks() {
        let entries: Vec<_> = (1..=200u64)
            .rev()
            .map(|s| (put(b"k", s), format!("value-{s}").into_bytes()))
            .collect();
        for interval in [1, 16] {
            let t = build(&entries, opts(64, interval));
            assert!(t.num_data_blocks().unwrap() > 10, "spans many blocks");
            for s in 1..=200u64 {
                assert_eq!(
                    t.get(b"k", s).unwrap(),
                    Lookup::Found(format!("value-{s}").into_bytes()),
                    "snapshot {s}"
                );
            }
            assert_eq!(t.get(b"k", 0).unwrap(), Lookup::NotFound);
        }
    }

    #[test]
    fn huge_entry() {
        let big = vec![7u8; 10 * 1024];
        let entries = vec![
            (put(b"a", 1), b"small".to_vec()),
            (put(b"b", 1), big.clone()),
            (put(b"c", 1), b"small".to_vec()),
        ];
        let t = build(&entries, TableOptions::default());
        assert_eq!(scan(&t).unwrap(), entries);
        assert_eq!(t.get(b"b", 1).unwrap(), Lookup::Found(big));
        // The block closes as soon as it passes block_size: [a, b] then [c].
        assert_eq!(t.num_data_blocks().unwrap(), 2);
    }

    #[test]
    fn seek_edges() {
        let entries: Vec<_> = (0..50u32)
            .map(|i| (put(format!("k{:03}", i * 2).as_bytes(), 1), vec![b'v'; 10]))
            .collect();
        for interval in [1, 16] {
            let t = build(&entries, opts(96, interval));
            assert!(t.num_data_blocks().unwrap() > 3);
            let mut it = t.iter();
            let mut seek = |user: &str| {
                it.seek(&put(user.as_bytes(), 1)).unwrap();
                it.valid().then(|| it.key().to_vec())
            };
            assert_eq!(seek("a"), Some(put(b"k000", 1)), "before the first key");
            assert_eq!(seek("k000"), Some(put(b"k000", 1)), "exactly the first");
            assert_eq!(seek("k041"), Some(put(b"k042", 1)), "between keys");
            assert_eq!(seek("k098"), Some(put(b"k098", 1)), "exactly the last");
            assert_eq!(seek("k099"), None, "past the last key");
            assert_eq!(seek("z"), None);
            // Every gap, which crosses every block boundary.
            for i in 0..49u32 {
                let between = format!("k{:03}", i * 2 + 1);
                let next = format!("k{:03}", i * 2 + 2);
                assert_eq!(seek(&between), Some(put(next.as_bytes(), 1)));
            }
        }
    }

    #[test]
    fn iteration_crosses_blocks() {
        let entries: Vec<_> = (0..500u32)
            .map(|i| {
                (
                    put(format!("{i:05}").as_bytes(), 1),
                    i.to_le_bytes().to_vec(),
                )
            })
            .collect();
        let t = build(&entries, opts(100, 4));
        assert_eq!(scan(&t).unwrap(), entries);
    }

    #[test]
    fn file_too_short() {
        assert!(matches!(open_bytes(&[0; 47]), Err(Error::Corruption(_))));
        assert!(matches!(open_bytes(&[]), Err(Error::Corruption(_))));
    }

    #[test]
    fn handle_out_of_range() {
        for (offset, size) in [
            (u64::MAX, u64::MAX),
            (0, u64::MAX),
            (u64::MAX, 0),
            (0, 1 << 40),
            (10, 0),
        ] {
            let footer = Footer {
                index: BlockHandle { offset, size },
                filter: BlockHandle { offset: 0, size: 0 },
            };
            let mut file = vec![0u8; 8];
            file.extend_from_slice(&footer.encode());
            assert!(
                matches!(open_bytes(&file), Err(Error::Corruption(_))),
                "handle ({offset}, {size})"
            );
        }
    }
}

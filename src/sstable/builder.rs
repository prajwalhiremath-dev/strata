//! [`TableBuilder`]: writes sorted entries into a new SSTable file.

use std::cmp::Ordering;

use super::block::BlockBuilder;
use super::format::{BlockHandle, Footer, append_trailer};
use crate::fs::WritableFile;
use crate::key::{InternalKey, compare_encoded};
use crate::{Error, Result};

/// How a table is laid out.
#[derive(Clone, Debug)]
pub struct TableOptions {
    /// Target size of a data block's contents, in bytes. A block is closed as
    /// soon as it reaches this size, so one entry larger than it gets a block
    /// of its own.
    pub block_size: usize,
    /// Store a full key every this many entries (chapter 03).
    pub restart_interval: usize,
}

impl Default for TableOptions {
    fn default() -> Self {
        TableOptions {
            block_size: 4096,
            restart_interval: 16,
        }
    }
}

/// What a finished table contains. Milestone 5 records this in the manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableMeta {
    pub file_size: u64,
    pub num_entries: u64,
    /// First and last encoded internal key; empty when `num_entries == 0`.
    pub smallest: Vec<u8>,
    pub largest: Vec<u8>,
}

/// Writes a table from entries added in increasing internal-key order.
///
/// `finish` syncs the file. Syncing the *directory* (so the file's name
/// survives a crash) is the caller's job, as with the WAL.
pub struct TableBuilder {
    file: Box<dyn WritableFile>,
    opts: TableOptions,
    data: BlockBuilder,
    /// One entry per data block: its last key -> its handle. Restart interval
    /// 1: index keys are stored whole, so the binary search over restarts
    /// lands directly on the right entry.
    index: BlockBuilder,
    /// Bytes written so far = offset of the next block.
    offset: u64,
    num_entries: u64,
    smallest: Vec<u8>,
    last_key: Vec<u8>,
    /// Set by a failed write: the file may end mid-block, so refuse to go on.
    failed: bool,
}

impl TableBuilder {
    pub fn new(file: Box<dyn WritableFile>, opts: TableOptions) -> Result<Self> {
        if opts.block_size == 0 || opts.restart_interval == 0 {
            return Err(Error::InvalidArgument(
                "block_size and restart_interval must be at least 1".into(),
            ));
        }
        Ok(TableBuilder {
            data: BlockBuilder::new(opts.restart_interval),
            index: BlockBuilder::new(1),
            file,
            opts,
            offset: 0,
            num_entries: 0,
            smallest: Vec::new(),
            last_key: Vec::new(),
            failed: false,
        })
    }

    /// Add one entry. `key` is an encoded internal key and must be strictly
    /// greater (in internal-key order) than the previous one.
    pub fn add(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.check_usable()?;
        InternalKey::decode(key)
            .map_err(|_| Error::InvalidArgument("not an encoded internal key".into()))?;
        if self.num_entries > 0 && compare_encoded(&self.last_key, key) != Ordering::Less {
            return Err(Error::InvalidArgument(
                "keys must be added in strictly increasing internal-key order".into(),
            ));
        }
        if self.num_entries == 0 {
            self.smallest = key.to_vec();
        }
        self.data.add(key, value);
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.num_entries += 1;
        if self.data.estimated_size() >= self.opts.block_size {
            self.flush_data_block()?;
        }
        Ok(())
    }

    /// Write the last data block, the (empty) filter, the index and the
    /// footer, then sync the file.
    pub fn finish(mut self) -> Result<TableMeta> {
        self.check_usable()?;
        if !self.data.is_empty() {
            self.flush_data_block()?;
        }
        let index_contents = self.index.finish();
        let index = self.write_block(index_contents)?;
        let footer = Footer {
            index,
            filter: BlockHandle { offset: 0, size: 0 },
        };
        self.append(&footer.encode())?;
        self.file.sync()?;
        Ok(TableMeta {
            file_size: self.offset,
            num_entries: self.num_entries,
            smallest: self.smallest,
            largest: if self.num_entries == 0 {
                Vec::new()
            } else {
                self.last_key
            },
        })
    }

    fn check_usable(&self) -> Result<()> {
        if self.failed {
            return Err(Error::InvalidArgument(
                "table builder failed an earlier write; discard it".into(),
            ));
        }
        Ok(())
    }

    fn flush_data_block(&mut self) -> Result<()> {
        let contents = self.data.finish();
        let handle = self.write_block(contents)?;
        // The index key is this block's last key: every key in the block is
        // <= it, and every key in the next block is > it.
        let mut encoded = Vec::new();
        handle.encode_to(&mut encoded);
        self.index.add(&self.last_key, &encoded);
        Ok(())
    }

    fn write_block(&mut self, mut contents: Vec<u8>) -> Result<BlockHandle> {
        let handle = BlockHandle {
            offset: self.offset,
            size: contents.len() as u64,
        };
        append_trailer(&mut contents);
        self.append(&contents)?;
        Ok(handle)
    }

    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        if let Err(e) = self.file.append(bytes) {
            self.failed = true;
            return Err(e);
        }
        self.offset += bytes.len() as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use crate::fs::{FileSystem, SimFs};
    use crate::key::{InternalKey, ValueKind};
    use crate::sstable::block::Block;
    use crate::sstable::format::{BlockHandle, FOOTER_LEN, Footer};
    use std::path::Path;

    fn ik(user: &[u8], seq: u64) -> Vec<u8> {
        InternalKey::new(user, seq, ValueKind::Put).encode()
    }

    fn setup(opts: TableOptions) -> (SimFs, TableBuilder) {
        let fs = SimFs::default();
        fs.create_dir_all(Path::new("/t")).unwrap();
        let f = fs.create(Path::new("/t/1.sst")).unwrap();
        fs.sync_dir(Path::new("/t")).unwrap();
        let b = TableBuilder::new(f, opts).unwrap();
        (fs, b)
    }

    fn index_entries(file: &[u8]) -> usize {
        let footer = Footer::decode(&file[file.len() - FOOTER_LEN..]).unwrap();
        let BlockHandle { offset, size } = footer.index;
        let block = Block::new(file[offset as usize..(offset + size) as usize].to_vec()).unwrap();
        let mut it = block.iter();
        it.seek_to_first().unwrap();
        let mut n = 0;
        while it.valid() {
            n += 1;
            it.next().unwrap();
        }
        n
    }

    #[test]
    fn default_options() {
        let o = TableOptions::default();
        assert_eq!((o.block_size, o.restart_interval), (4096, 16));
    }

    #[test]
    fn rejects_bad_options() {
        let fs = SimFs::default();
        fs.create_dir_all(Path::new("/t")).unwrap();
        for opts in [
            TableOptions {
                block_size: 0,
                ..Default::default()
            },
            TableOptions {
                restart_interval: 0,
                ..Default::default()
            },
        ] {
            let f = fs.create(Path::new("/t/x.sst")).unwrap();
            assert!(matches!(
                TableBuilder::new(f, opts),
                Err(Error::InvalidArgument(_))
            ));
        }
    }

    #[test]
    fn rejects_out_of_order_and_duplicates() {
        let invalid = |r: Result<()>| matches!(r, Err(Error::InvalidArgument(_)));
        let (_fs, mut b) = setup(TableOptions::default());
        b.add(&ik(b"b", 1), b"").unwrap();
        assert!(invalid(b.add(&ik(b"a", 1), b"")), "smaller user key");
        assert!(invalid(b.add(&ik(b"b", 1), b"")), "duplicate");
        b.add(&ik(b"k", 1), b"").unwrap();
        assert!(
            invalid(b.add(&ik(b"k", 5), b"")),
            "newer version after older"
        );
        b.add(&ik(b"z", 1), b"").unwrap(); // still usable after a rejection
    }

    #[test]
    fn rejects_non_internal_key() {
        let (_fs, mut b) = setup(TableOptions::default());
        assert!(matches!(b.add(b"abc", b""), Err(Error::InvalidArgument(_))));
    }

    #[test]
    fn empty_table_layout() {
        let (fs, b) = setup(TableOptions::default());
        let meta = b.finish().unwrap();
        assert_eq!(meta.file_size, 8 + 4 + 48);
        assert_eq!(meta.num_entries, 0);
        assert!(meta.smallest.is_empty() && meta.largest.is_empty());
        let file = fs.read(Path::new("/t/1.sst")).unwrap();
        assert_eq!(file.len(), 60);
        assert_eq!(index_entries(&file), 0);
    }

    #[test]
    fn meta_reports_bounds() {
        let (fs, mut b) = setup(TableOptions::default());
        let keys: Vec<_> = (0..1000)
            .map(|i| ik(format!("k{i:04}").as_bytes(), 7))
            .collect();
        for k in &keys {
            b.add(k, b"value").unwrap();
        }
        let meta = b.finish().unwrap();
        assert_eq!(meta.num_entries, 1000);
        assert_eq!(meta.smallest, keys[0]);
        assert_eq!(meta.largest, keys[999]);
        assert_eq!(
            meta.file_size,
            fs.file_len(Path::new("/t/1.sst")).unwrap() as u64
        );
    }

    #[test]
    fn finish_syncs() {
        let (fs, mut b) = setup(TableOptions::default());
        for i in 0..100 {
            b.add(&ik(format!("k{i:03}").as_bytes(), 1), b"v").unwrap();
        }
        let meta = b.finish().unwrap();
        fs.crash();
        assert_eq!(
            fs.read(Path::new("/t/1.sst")).unwrap().len() as u64,
            meta.file_size
        );
    }

    #[test]
    fn failed_write_stops_builder() {
        let (fs, mut b) = setup(TableOptions {
            block_size: 16,
            ..Default::default()
        });
        fs.fail_next_append();
        assert!(b.add(&ik(b"a", 1), b"0123456789abcdef").is_err());
        assert!(
            b.add(&ik(b"b", 1), b"x").is_err(),
            "refuses after a failed write"
        );
        assert!(b.finish().is_err());
    }

    #[test]
    fn small_blocks_split() {
        let (fs, mut b) = setup(TableOptions {
            block_size: 64,
            ..Default::default()
        });
        for i in 0..100 {
            b.add(&ik(format!("k{i:03}").as_bytes(), 1), b"value")
                .unwrap();
        }
        b.finish().unwrap();
        let file = fs.read(Path::new("/t/1.sst")).unwrap();
        assert!(index_entries(&file) > 1);
    }
}

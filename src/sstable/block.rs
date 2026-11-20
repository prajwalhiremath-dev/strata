//! Blocks: the unit an SSTable is read in.
//!
//! ```text
//! entry 0 | entry 1 | ... | entry n-1 | restart[0] .. restart[r-1] | r
//!                                       (u32 LE each)                (u32 LE)
//!
//! entry = varint32 shared | varint32 non_shared | varint32 value_len
//!         | key[shared..] (non_shared bytes) | value
//! ```
//!
//! **Prefix compression.** Sorted keys share long prefixes (`user:00000041`,
//! `user:00000042`...), so each entry stores only how many bytes it shares
//! with the previous key and the bytes that differ.
//!
//! **Restart points.** Every `restart_interval`-th entry stores its whole key
//! (`shared = 0`) and its offset goes in the restart array. A seek
//! binary-searches the restart keys, then walks forward at most
//! `restart_interval` entries. Without restarts you'd have to decode from the
//! start of the block to rebuild any key.
//!
//! All keys are encoded internal keys and are compared with
//! [`compare_encoded`], never bytewise.

use std::cmp::Ordering;
use std::ops::Range;

use crate::coding::{get_varint32, put_varint32};
use crate::key::compare_encoded;
use crate::{Error, Result};

fn corrupt(msg: &str) -> Error {
    Error::Corruption(format!("block: {msg}"))
}

fn read_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(buf[at..at + 4].try_into().expect("4 bytes"))
}

/// Builds one block's contents. Keys must arrive in internal-key order; the
/// table builder checks that before calling [`BlockBuilder::add`].
pub(crate) struct BlockBuilder {
    buf: Vec<u8>,
    restarts: Vec<u32>,
    restart_interval: usize,
    /// Entries since the last restart point.
    counter: usize,
    num_entries: usize,
    last_key: Vec<u8>,
}

impl BlockBuilder {
    pub(crate) fn new(restart_interval: usize) -> Self {
        assert!(restart_interval >= 1, "restart_interval must be >= 1");
        BlockBuilder {
            buf: Vec::new(),
            restarts: vec![0],
            restart_interval,
            counter: 0,
            num_entries: 0,
            last_key: Vec::new(),
        }
    }

    pub(crate) fn add(&mut self, key: &[u8], value: &[u8]) {
        debug_assert!(
            self.num_entries == 0 || compare_encoded(&self.last_key, key) == Ordering::Less,
            "keys must be added in increasing order"
        );
        let shared = if self.counter < self.restart_interval {
            self.last_key
                .iter()
                .zip(key)
                .take_while(|(a, b)| a == b)
                .count()
        } else {
            // Start a new restart point: store this key in full.
            self.restarts.push(self.buf.len() as u32);
            self.counter = 0;
            0
        };
        put_varint32(&mut self.buf, shared as u32);
        put_varint32(&mut self.buf, (key.len() - shared) as u32);
        put_varint32(&mut self.buf, value.len() as u32);
        self.buf.extend_from_slice(&key[shared..]);
        self.buf.extend_from_slice(value);
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.counter += 1;
        self.num_entries += 1;
    }

    /// Exactly the length [`finish`](Self::finish) would return now.
    pub(crate) fn estimated_size(&self) -> usize {
        self.buf.len() + 4 * self.restarts.len() + 4
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.num_entries == 0
    }

    /// Append the restart array and return the contents. Resets the builder.
    pub(crate) fn finish(&mut self) -> Vec<u8> {
        let mut out = std::mem::take(&mut self.buf);
        for r in &self.restarts {
            out.extend_from_slice(&r.to_le_bytes());
        }
        out.extend_from_slice(&(self.restarts.len() as u32).to_le_bytes());
        self.restarts = vec![0];
        self.counter = 0;
        self.num_entries = 0;
        self.last_key.clear();
        out
    }
}

/// A decoded block: the raw contents plus where the restart array starts.
/// `new` checks the restart array; entries are checked as they're decoded.
pub(crate) struct Block {
    data: Vec<u8>,
    /// Entries live in `data[..restarts_offset]`.
    restarts_offset: usize,
    num_restarts: usize,
}

impl Block {
    pub(crate) fn new(data: Vec<u8>) -> Result<Block> {
        if data.len() < 4 {
            return Err(corrupt("shorter than its restart count"));
        }
        let n = read_u32(&data, data.len() - 4) as usize;
        if n == 0 {
            return Err(corrupt("zero restart points"));
        }
        // 4 bytes per restart + 4 for the count, computed without overflow.
        let tail = n
            .checked_add(1)
            .and_then(|x| x.checked_mul(4))
            .filter(|&t| t <= data.len())
            .ok_or_else(|| corrupt("restart count larger than the block"))?;
        let restarts_offset = data.len() - tail;
        let block = Block {
            data,
            restarts_offset,
            num_restarts: n,
        };
        if restarts_offset == 0 {
            if n != 1 || block.restart_point(0) != 0 {
                return Err(corrupt("an empty block must have restarts [0]"));
            }
        } else {
            if block.restart_point(0) != 0 {
                return Err(corrupt("first restart is not 0"));
            }
            for i in 1..n {
                let (prev, cur) = (block.restart_point(i - 1), block.restart_point(i));
                if cur <= prev || cur >= restarts_offset {
                    return Err(corrupt("restart points out of order or past the data"));
                }
            }
        }
        Ok(block)
    }

    fn restart_point(&self, i: usize) -> usize {
        read_u32(&self.data, self.restarts_offset + 4 * i) as usize
    }

    pub(crate) fn iter(&self) -> BlockIter<'_> {
        BlockIter {
            block: self,
            next: self.restarts_offset,
            restart_index: 0,
            key: Vec::new(),
            value: 0..0,
            valid: false,
        }
    }

    /// Decode the entry header at `at`: (shared, key bytes, value range, next).
    fn decode_entry(&self, at: usize) -> Result<(usize, Range<usize>, Range<usize>, usize)> {
        let region = &self.data[..self.restarts_offset];
        let mut pos = at;
        let mut field = || -> Result<usize> {
            let (v, n) = get_varint32(region.get(pos..).unwrap_or_default())?;
            pos += n;
            Ok(v as usize)
        };
        let shared = field()?;
        let non_shared = field()?;
        let value_len = field()?;
        let key_end = pos
            .checked_add(non_shared)
            .filter(|&e| e <= region.len())
            .ok_or_else(|| corrupt("key runs past the data"))?;
        let value_end = key_end
            .checked_add(value_len)
            .filter(|&e| e <= region.len())
            .ok_or_else(|| corrupt("value runs past the data"))?;
        Ok((shared, pos..key_end, key_end..value_end, value_end))
    }

    /// The full key stored at restart point `i`.
    fn restart_key(&self, i: usize) -> Result<&[u8]> {
        let (shared, key, _, _) = self.decode_entry(self.restart_point(i))?;
        if shared != 0 {
            return Err(corrupt("restart entry has a shared prefix"));
        }
        Ok(&self.data[key])
    }
}

/// A cursor over one block's entries.
pub(crate) struct BlockIter<'a> {
    block: &'a Block,
    /// Offset of the entry after the current one.
    next: usize,
    /// Index of the restart region the cursor is in.
    restart_index: usize,
    /// Full key of the current entry, rebuilt from the shared prefixes.
    key: Vec<u8>,
    value: Range<usize>,
    valid: bool,
}

impl BlockIter<'_> {
    pub(crate) fn valid(&self) -> bool {
        self.valid
    }

    pub(crate) fn key(&self) -> &[u8] {
        debug_assert!(self.valid);
        &self.key
    }

    pub(crate) fn value(&self) -> &[u8] {
        debug_assert!(self.valid);
        &self.block.data[self.value.clone()]
    }

    pub(crate) fn seek_to_first(&mut self) -> Result<()> {
        self.seek_to_restart(0);
        self.parse_next()
    }

    /// Position at the first entry with key >= `target`; not valid if none.
    pub(crate) fn seek(&mut self, target: &[u8]) -> Result<()> {
        // Binary search: the last restart whose key is < target. Everything
        // before it is smaller, and the answer is at most one region later.
        let (mut lo, mut hi) = (0, self.block.num_restarts - 1);
        if self.block.restarts_offset == 0 {
            hi = 0; // empty block: no restart keys to look at
        }
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            match self.block.restart_key(mid) {
                Ok(k) if compare_encoded(k, target) == Ordering::Less => lo = mid,
                Ok(_) => hi = mid - 1,
                Err(e) => {
                    self.valid = false;
                    return Err(e);
                }
            }
        }
        // Then walk forward to the first key >= target.
        self.seek_to_restart(lo);
        loop {
            self.parse_next()?;
            if !self.valid || compare_encoded(&self.key, target) != Ordering::Less {
                return Ok(());
            }
        }
    }

    pub(crate) fn next(&mut self) -> Result<()> {
        if !self.valid {
            return Ok(());
        }
        self.parse_next()
    }

    fn seek_to_restart(&mut self, i: usize) {
        self.key.clear();
        self.restart_index = i;
        self.next = self.block.restart_point(i);
        self.valid = false;
    }

    /// Decode the entry at `self.next` and make it current.
    fn parse_next(&mut self) -> Result<()> {
        let at = self.next;
        if at >= self.block.restarts_offset {
            self.valid = false;
            return Ok(());
        }
        let b = self.block;
        while self.restart_index + 1 < b.num_restarts
            && b.restart_point(self.restart_index + 1) <= at
        {
            self.restart_index += 1;
        }
        let result = b.decode_entry(at).and_then(|(shared, key, value, next)| {
            let at_restart = b.restart_point(self.restart_index) == at;
            if at_restart && shared != 0 {
                return Err(corrupt("restart entry has a shared prefix"));
            }
            if shared > self.key.len() {
                return Err(corrupt("shared prefix longer than the previous key"));
            }
            Ok((shared, key, value, next))
        });
        match result {
            Ok((shared, key, value, next)) => {
                self.key.truncate(shared);
                self.key.extend_from_slice(&b.data[key]);
                self.value = value;
                self.next = next;
                self.valid = true;
                Ok(())
            }
            Err(e) => {
                self.valid = false;
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use crate::key::{InternalKey, MAX_SEQNO, ValueKind};
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    fn ik(user: &[u8], seq: u64) -> Vec<u8> {
        InternalKey::new(user, seq, ValueKind::Put).encode()
    }

    fn build(keys: &[Vec<u8>], interval: usize) -> Vec<u8> {
        let mut b = BlockBuilder::new(interval);
        for (i, k) in keys.iter().enumerate() {
            b.add(k, format!("v{i}").as_bytes());
        }
        b.finish()
    }

    fn collect(block: &Block) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut it = block.iter();
        it.seek_to_first().unwrap();
        let mut out = Vec::new();
        while it.valid() {
            out.push((it.key().to_vec(), it.value().to_vec()));
            it.next().unwrap();
        }
        out
    }

    fn num_restarts(contents: &[u8]) -> u32 {
        u32::from_le_bytes(contents[contents.len() - 4..].try_into().unwrap())
    }

    fn is_corrupt<T>(r: Result<T>) -> bool {
        matches!(r, Err(Error::Corruption(_)))
    }

    fn keys(n: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|i| ik(format!("key{i:03}").as_bytes(), 1))
            .collect()
    }

    #[test]
    fn empty_block_is_8_bytes() {
        let mut b = BlockBuilder::new(16);
        assert!(b.is_empty());
        assert_eq!(b.estimated_size(), 8);
        let contents = b.finish();
        assert_eq!(contents, [0, 0, 0, 0, 1, 0, 0, 0]);
        let block = Block::new(contents).unwrap();
        let mut it = block.iter();
        it.seek_to_first().unwrap();
        assert!(!it.valid());
        it.seek(&ik(b"a", 1)).unwrap();
        assert!(!it.valid());
    }

    #[test]
    fn round_trip_iteration() {
        let ks = keys(100);
        let contents = build(&ks, 16);
        let got = collect(&Block::new(contents).unwrap());
        let want: Vec<_> = ks
            .iter()
            .enumerate()
            .map(|(i, k)| (k.clone(), format!("v{i}").into_bytes()))
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn estimated_size_matches_finish() {
        let mut b = BlockBuilder::new(4);
        for k in keys(37) {
            b.add(&k, b"value");
        }
        let est = b.estimated_size();
        assert_eq!(b.finish().len(), est);
        assert!(b.is_empty(), "finish resets the builder");
    }

    #[test]
    fn restart_count() {
        assert_eq!(num_restarts(&build(&keys(33), 16)), 3);
        assert_eq!(num_restarts(&build(&keys(33), 1)), 33);
        assert_eq!(num_restarts(&build(&keys(16), 16)), 1);
    }

    #[test]
    fn prefix_compression_shrinks() {
        let ks = keys(100);
        let compressed = build(&ks, 16).len();
        let uncompressed = build(&ks, 1).len();
        assert!(
            compressed < uncompressed,
            "{compressed} should be < {uncompressed}"
        );
    }

    #[test]
    fn seek_cases() {
        let ks: Vec<Vec<u8>> = [b"b", b"d", b"f", b"h", b"j"]
            .iter()
            .map(|k| ik(*k, 1))
            .collect();
        for interval in [1, 2, 16] {
            let block = Block::new(build(&ks, interval)).unwrap();
            let mut it = block.iter();
            let seek = |it: &mut BlockIter<'_>, k: &[u8]| {
                it.seek(&ik(k, 1)).unwrap();
                it.valid().then(|| it.key().to_vec())
            };
            assert_eq!(seek(&mut it, b"a"), Some(ik(b"b", 1)), "before first");
            assert_eq!(seek(&mut it, b"d"), Some(ik(b"d", 1)), "exact");
            assert_eq!(seek(&mut it, b"e"), Some(ik(b"f", 1)), "between");
            assert_eq!(seek(&mut it, b"j"), Some(ik(b"j", 1)), "last");
            assert_eq!(seek(&mut it, b"k"), None, "past the end");
            // seeking backwards works too: seek is absolute
            assert_eq!(seek(&mut it, b"c"), Some(ik(b"d", 1)));
        }
    }

    #[test]
    fn seek_uses_internal_order() {
        // Newest first: (k,9) < (k,5) < (k,1) in internal order. Bytewise,
        // the little-endian trailers would sort the other way round.
        let ks = vec![ik(b"k", 9), ik(b"k", 5), ik(b"k", 1), ik(b"m", 1)];
        for interval in [1, 16] {
            let block = Block::new(build(&ks, interval)).unwrap();
            let mut it = block.iter();
            it.seek(&ik(b"k", 6)).unwrap();
            assert_eq!(it.key(), ik(b"k", 5));
            it.seek(&ik(b"k", MAX_SEQNO)).unwrap();
            assert_eq!(it.key(), ik(b"k", 9));
            it.seek(&ik(b"k", 0)).unwrap();
            assert_eq!(it.key(), ik(b"m", 1));
        }
    }

    #[test]
    fn rejects_bad_restarts() {
        let restarts = |entries: &[u8], rs: &[u32]| {
            let mut v = entries.to_vec();
            for r in rs {
                v.extend_from_slice(&r.to_le_bytes());
            }
            v.extend_from_slice(&(rs.len() as u32).to_le_bytes());
            v
        };
        let data = build(&keys(3), 1); // three entries, three restarts
        let entries_len = data.len() - 4 * 4;
        let entries = &data[..entries_len];

        assert!(is_corrupt(Block::new(vec![1, 0, 0])), "too short");
        let mut huge = entries.to_vec();
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(is_corrupt(Block::new(huge)), "restart count too big");
        assert!(is_corrupt(Block::new(vec![0, 0, 0, 0])), "zero restarts");
        assert!(
            is_corrupt(Block::new(restarts(entries, &[1]))),
            "first != 0"
        );
        assert!(
            is_corrupt(Block::new(restarts(entries, &[0, 5, 5]))),
            "not increasing"
        );
        assert!(
            is_corrupt(Block::new(restarts(entries, &[0, entries_len as u32]))),
            "points past data"
        );
        assert!(
            is_corrupt(Block::new(restarts(&[], &[0, 0]))),
            "empty block needs [0]"
        );
        Block::new(data).unwrap();
    }

    /// Hand-encode one entry.
    fn entry(shared: u32, key: &[u8], value_len: u32, value: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        crate::coding::put_varint32(&mut v, shared);
        crate::coding::put_varint32(&mut v, key.len() as u32);
        crate::coding::put_varint32(&mut v, value_len);
        v.extend_from_slice(key);
        v.extend_from_slice(value);
        v
    }

    fn with_restarts(mut entries: Vec<u8>, rs: &[u32]) -> Vec<u8> {
        for r in rs {
            entries.extend_from_slice(&r.to_le_bytes());
        }
        entries.extend_from_slice(&(rs.len() as u32).to_le_bytes());
        entries
    }

    #[test]
    fn rejects_bad_entries() {
        let k = ik(b"abc", 1);
        // shared > 0 at a restart
        let b = Block::new(with_restarts(entry(2, &k, 1, b"x"), &[0])).unwrap();
        assert!(is_corrupt(b.iter().seek_to_first()));
        // shared larger than the previous key
        let mut e = entry(0, &k, 1, b"x");
        e.extend(entry(100, b"zz", 1, b"y"));
        let b = Block::new(with_restarts(e, &[0])).unwrap();
        let mut it = b.iter();
        it.seek_to_first().unwrap();
        assert!(is_corrupt(it.next()));
        // value_len runs past the data
        let b = Block::new(with_restarts(entry(0, &k, 500, b"x"), &[0])).unwrap();
        assert!(is_corrupt(b.iter().seek_to_first()));
        assert!(is_corrupt(b.iter().seek(&k)));
        // truncated varint
        let b = Block::new(with_restarts(vec![0x80], &[0])).unwrap();
        assert!(is_corrupt(b.iter().seek_to_first()));
    }

    proptest! {
        #[test]
        fn arbitrary_bytes_never_panic(
            bytes in proptest::collection::vec(any::<u8>(), 0..512),
            target in proptest::collection::vec(any::<u8>(), 0..16),
        ) {
            if let Ok(block) = Block::new(bytes) {
                let mut it = block.iter();
                if it.seek_to_first().is_ok() {
                    let mut steps = 0;
                    while it.valid() && steps < 1000 {
                        let _ = (it.key(), it.value());
                        if it.next().is_err() { break; }
                        steps += 1;
                    }
                }
                let _ = it.seek(&target);
            }
        }

        #[test]
        fn block_model(
            users in proptest::collection::btree_set(
                proptest::collection::vec(b'a'..=b'e', 0..6), 0..200),
            interval in 1usize..=20,
            targets in proptest::collection::vec(
                proptest::collection::vec(b'a'..=b'f', 0..7), 0..20),
        ) {
            let users: BTreeSet<Vec<u8>> = users;
            let ks: Vec<Vec<u8>> = users.iter().map(|u| ik(u, 0)).collect();
            let block = Block::new(build(&ks, interval)).unwrap();
            let got: Vec<Vec<u8>> = collect(&block).into_iter().map(|(k, _)| k).collect();
            prop_assert_eq!(&got, &ks);
            let mut it = block.iter();
            for t in targets {
                it.seek(&ik(&t, 0)).unwrap();
                let want = users.range(t.clone()..).next().map(|u| ik(u, 0));
                prop_assert_eq!(it.valid().then(|| it.key().to_vec()), want);
            }
        }
    }
}

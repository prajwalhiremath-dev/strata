//! The memtable: the in-memory, sorted home of the most recent writes.
//!
//! Every write lands here (after the WAL, from milestone 2). Because entries
//! are kept sorted by [`InternalKey`], the memtable can later be written out
//! to disk as a sorted file in one sequential pass.
//!
//! Implementation: a `BTreeMap` behind a read-write lock. Chapter 01 explains
//! why this simple choice comes first and when a skiplist would be better.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::RwLock;

use crate::key::{InternalKey, MAX_SEQNO, SeqNo, ValueKind};

/// Rough per-entry bookkeeping cost (seqno, kind, map node overhead) used for
/// the size estimate that decides when the memtable is "full".
const ENTRY_OVERHEAD: usize = 16;

/// Result of looking up one user key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Lookup {
    /// The newest visible version is a value.
    Found(Vec<u8>),
    /// The newest visible version is a tombstone. Callers must stop searching
    /// older data: the key is deleted.
    Deleted,
    /// No visible version here; older data (on disk, later) must be checked.
    NotFound,
}

/// One row of a scan: a user key and its newest visible value, or `None`
/// for a tombstone.
pub(crate) type ScanRow = (Vec<u8>, Option<Vec<u8>>);

pub(crate) struct Memtable {
    map: RwLock<BTreeMap<InternalKey, Vec<u8>>>,
    size: AtomicUsize,
}

impl Memtable {
    pub(crate) fn new() -> Self {
        Memtable {
            map: RwLock::new(BTreeMap::new()),
            size: AtomicUsize::new(0),
        }
    }

    /// Record one version. `value` is ignored (stored empty) for `Delete`.
    pub(crate) fn insert(&self, seqno: SeqNo, kind: ValueKind, user_key: &[u8], value: &[u8]) {
        let value = match kind {
            ValueKind::Put => value.to_vec(),
            ValueKind::Delete => Vec::new(),
        };
        let added = user_key.len() + value.len() + ENTRY_OVERHEAD;
        self.map
            .write()
            .insert(InternalKey::new(user_key, seqno, kind), value);
        self.size.fetch_add(added, Ordering::Relaxed);
    }

    /// Newest version of `user_key` with `seqno <= snapshot`.
    pub(crate) fn get(&self, user_key: &[u8], snapshot: SeqNo) -> Lookup {
        // Versions of one key are sorted newest-first, so the first entry at
        // or after (user_key, snapshot) is the newest one we may see.
        let seek = InternalKey::new(user_key, snapshot, ValueKind::Put);
        let map = self.map.read();
        match map.range(seek..).next() {
            Some((k, v)) if k.user_key == user_key => match k.kind {
                ValueKind::Put => Lookup::Found(v.clone()),
                ValueKind::Delete => Lookup::Deleted,
            },
            _ => Lookup::NotFound,
        }
    }

    /// Newest visible version of every user key in `[start, end]` (bounds as
    /// given), in ascending key order. `None` marks a tombstone; the caller
    /// decides whether to hide it (Db) or keep it (merging with disk, later).
    pub(crate) fn scan(
        &self,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        snapshot: SeqNo,
    ) -> Vec<ScanRow> {
        // (k, MAX_SEQNO, Put) is the smallest internal key for k and
        // (k, 0, Delete) the largest, so these map user-key bounds onto
        // internal-key bounds.
        let lo = match start {
            Bound::Included(k) => Bound::Included(InternalKey::new(k, MAX_SEQNO, ValueKind::Put)),
            Bound::Excluded(k) => Bound::Excluded(InternalKey::new(k, 0, ValueKind::Delete)),
            Bound::Unbounded => Bound::Unbounded,
        };
        let past_end = |k: &[u8]| match end {
            Bound::Included(e) => k > e,
            Bound::Excluded(e) => k >= e,
            Bound::Unbounded => false,
        };

        let map = self.map.read();
        let mut out: Vec<ScanRow> = Vec::new();
        // Walk forward from `lo` and stop at the end bound ourselves:
        // BTreeMap::range panics if start > end, and a reversed user range
        // should simply be empty.
        for (k, v) in map.range((lo, Bound::Unbounded)) {
            if past_end(&k.user_key) {
                break;
            }
            if k.seqno > snapshot {
                continue; // written after the snapshot: invisible
            }
            if out.last().is_some_and(|(prev, _)| *prev == k.user_key) {
                continue; // an older version of a key we already emitted
            }
            let value = match k.kind {
                ValueKind::Put => Some(v.clone()),
                ValueKind::Delete => None,
            };
            out.push((k.user_key.clone(), value));
        }
        out
    }

    /// Every version of every key, in internal-key order (what a flush
    /// writes to a table).
    pub(crate) fn entries(&self) -> Vec<(InternalKey, Vec<u8>)> {
        self.map
            .read()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.map.read().is_empty()
    }

    /// Approximate bytes held, used to decide when to freeze and flush.
    #[allow(dead_code)] // used to trigger flushes in milestone 4
    pub(crate) fn approximate_size(&self) -> usize {
        self.size.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{MAX_SEQNO, ValueKind::*};
    use std::ops::Bound::*;

    fn some(v: &[u8]) -> Option<Vec<u8>> {
        Some(v.to_vec())
    }

    #[test]
    fn get_returns_newest_visible() {
        let m = Memtable::new();
        m.insert(1, Put, b"k", b"v1");
        m.insert(2, Put, b"k", b"v2");
        assert_eq!(m.get(b"k", 2), Lookup::Found(b"v2".to_vec()));
        assert_eq!(m.get(b"k", 1), Lookup::Found(b"v1".to_vec()));
        assert_eq!(m.get(b"k", 0), Lookup::NotFound);
        assert_eq!(m.get(b"other", MAX_SEQNO), Lookup::NotFound);
    }

    #[test]
    fn tombstone_hides_older_put() {
        let m = Memtable::new();
        m.insert(1, Put, b"k", b"v");
        m.insert(2, Delete, b"k", b"");
        assert_eq!(m.get(b"k", 2), Lookup::Deleted);
        assert_eq!(m.get(b"k", 1), Lookup::Found(b"v".to_vec()));
    }

    #[test]
    fn scan_newest_per_key_in_range() {
        let m = Memtable::new();
        m.insert(1, Put, b"a", b"1");
        m.insert(2, Put, b"b", b"2");
        m.insert(3, Delete, b"b", b"");
        m.insert(4, Put, b"c", b"3");
        m.insert(5, Put, b"d", b"4");
        let got = m.scan(Included(b"a"), Excluded(b"d"), MAX_SEQNO);
        assert_eq!(
            got,
            vec![
                (b"a".to_vec(), some(b"1")),
                (b"b".to_vec(), None),
                (b"c".to_vec(), some(b"3")),
            ]
        );
        // At snapshot 2 the delete of b (seqno 3) is invisible.
        let got = m.scan(Included(b"b"), Included(b"b"), 2);
        assert_eq!(got, vec![(b"b".to_vec(), some(b"2"))]);
    }

    #[test]
    fn scan_bound_kinds() {
        let m = Memtable::new();
        for (i, k) in [b"a", b"b", b"c", b"d", b"e"].iter().enumerate() {
            m.insert(i as u64 + 1, Put, *k, *k);
        }
        let keys = |s: Bound<&[u8]>, e: Bound<&[u8]>| -> Vec<Vec<u8>> {
            m.scan(s, e, MAX_SEQNO)
                .into_iter()
                .map(|(k, _)| k)
                .collect()
        };
        let v = |ks: &[&[u8]]| ks.iter().map(|k| k.to_vec()).collect::<Vec<_>>();
        assert_eq!(keys(Included(b"b"), Included(b"d")), v(&[b"b", b"c", b"d"]));
        assert_eq!(keys(Included(b"b"), Excluded(b"d")), v(&[b"b", b"c"]));
        assert_eq!(keys(Excluded(b"b"), Included(b"d")), v(&[b"c", b"d"]));
        assert_eq!(keys(Excluded(b"b"), Excluded(b"d")), v(&[b"c"]));
        assert_eq!(keys(Unbounded, Unbounded).len(), 5);
        assert!(
            keys(Included(b"d"), Excluded(b"b")).is_empty(),
            "reversed range is empty"
        );
    }

    #[test]
    fn size_grows() {
        let m = Memtable::new();
        let before = m.approximate_size();
        m.insert(1, Put, b"key", b"value");
        assert!(m.approximate_size() > before);
    }
}

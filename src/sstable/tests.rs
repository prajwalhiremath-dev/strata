//! Whole-table tests: the memtable is the oracle for what a table built from
//! it must answer, and every damaged byte must be caught.

use std::cmp::Ordering;
use std::ops::Bound;
use std::path::Path;

use proptest::prelude::*;

use super::format::{BlockHandle, Footer};
use super::{Table, TableBuilder, TableOptions};
use crate::Result;
use crate::fs::{FileSystem, SimFs};
use crate::key::{InternalKey, ValueKind, compare_encoded};
use crate::memtable::Memtable;

fn write_table(fs: &SimFs, path: &str, entries: &[(Vec<u8>, Vec<u8>)], opts: TableOptions) {
    let mut b = TableBuilder::new(fs.create(Path::new(path)).unwrap(), opts).unwrap();
    for (k, v) in entries {
        b.add(k, v).unwrap();
    }
    b.finish().unwrap();
}

fn new_fs() -> SimFs {
    let fs = SimFs::default();
    fs.create_dir_all(Path::new("/t")).unwrap();
    fs
}

fn open(fs: &SimFs, path: &str) -> Result<Table> {
    Table::open(fs.open_random(Path::new(path))?)
}

/// Open and read every entry; the first error wins.
fn open_and_scan(fs: &SimFs, path: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let t = open(fs, path)?;
    let mut it = t.iter();
    it.seek_to_first()?;
    let mut out = Vec::new();
    while it.valid() {
        out.push((it.key().to_vec(), it.value().to_vec()));
        it.next()?;
    }
    Ok(out)
}

fn user_key() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(b'a'..=b'd', 1..=3)
}

fn bound() -> impl Strategy<Value = Bound<Vec<u8>>> {
    prop_oneof![
        Just(Bound::Unbounded),
        user_key().prop_map(Bound::Included),
        user_key().prop_map(Bound::Excluded),
    ]
}

fn as_ref(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    b.as_ref().map(Vec::as_slice)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn table_matches_memtable(
        ops in proptest::collection::vec(
            (user_key(), proptest::option::of(proptest::collection::vec(any::<u8>(), 0..40))),
            0..300),
        block_size in 16usize..=512,
        restart_interval in 1usize..=8,
        seeks in proptest::collection::vec((user_key(), 0u64..320), 0..30),
        ranges in proptest::collection::vec((bound(), bound(), 0u64..320), 0..10),
        bloom_bits_per_key in prop_oneof![Just(0usize), 1usize..=12],
    ) {
        let mem = Memtable::new();
        for (i, (k, v)) in ops.iter().enumerate() {
            let seq = i as u64 + 1;
            match v {
                Some(v) => mem.insert(seq, ValueKind::Put, k, v),
                None => mem.insert(seq, ValueKind::Delete, k, b""),
            }
        }
        let entries: Vec<(Vec<u8>, Vec<u8>)> =
            mem.entries().into_iter().map(|(k, v)| (k.encode(), v)).collect();
        let fs = new_fs();
        write_table(&fs, "/t/1.sst", &entries, TableOptions { block_size, restart_interval, bloom_bits_per_key });

        // 1. Full iteration gives back exactly what went in.
        prop_assert_eq!(&open_and_scan(&fs, "/t/1.sst").unwrap(), &entries);

        // 2. Point lookups agree with the memtable at every snapshot.
        let t = open(&fs, "/t/1.sst").unwrap();
        let n = ops.len() as u64;
        prop_assert_eq!(t.max_seqno(), n, "seqnos are 1..=n");
        prop_assert_eq!(t.has_filter(), bloom_bits_per_key > 0);
        let mut users: Vec<Vec<u8>> = ops.iter().map(|(k, _)| k.clone()).collect();
        users.extend([b"".to_vec(), b"dddd".to_vec(), b"ab\0".to_vec()]); // never written
        users.sort();
        users.dedup();
        for u in &users {
            // A key's answer only changes at seqnos where it was written, so
            // checking each of those, the one just below, and both ends
            // covers every boundary.
            let mut snaps = vec![0, n + 1];
            for (i, (k, _)) in ops.iter().enumerate() {
                if k == u {
                    snaps.extend([i as u64, i as u64 + 1]);
                }
            }
            for snap in snaps {
                prop_assert_eq!(t.get(u, snap).unwrap(), mem.get(u, snap), "key {:?} snapshot {}", u, snap);
            }
        }

        // 3. Seeks land where a lower-bound search over the entries does.
        let mut it = t.iter();
        for (u, seq) in seeks {
            let target = InternalKey::new(&u, seq, ValueKind::Put).encode();
            it.seek(&target).unwrap();
            let want = entries
                .iter()
                .find(|(k, _)| compare_encoded(k, &target) != Ordering::Less)
                .map(|(k, _)| k.clone());
            prop_assert_eq!(it.valid().then(|| it.key().to_vec()), want);
        }

        // 4. Range scans agree with the memtable.
        for (lo, hi, snap) in &ranges {
            prop_assert_eq!(
                t.scan(as_ref(lo), as_ref(hi), *snap).unwrap(),
                mem.scan(as_ref(lo), as_ref(hi), *snap),
                "scan {:?}..{:?} at {}", lo, hi, snap
            );
        }
    }

    #[test]
    fn arbitrary_file_never_panics(
        mut bytes in proptest::collection::vec(any::<u8>(), 0..2048),
        footer in proptest::option::of((any::<u64>(), any::<u64>())),
        probes in proptest::collection::vec(user_key(), 0..4),
    ) {
        if let Some((off, size)) = footer {
            // Sometimes a valid footer pointing somewhere inside the garbage.
            let len = bytes.len() as u64 + 1;
            let f = Footer {
                index: BlockHandle { offset: off % len, size: size % len },
                filter: BlockHandle { offset: 0, size: 0 },
                max_seqno: 0,
            };
            bytes.extend_from_slice(&f.encode());
        }
        let fs = new_fs();
        fs.create(Path::new("/t/x.sst")).unwrap().append(&bytes).unwrap();
        let _ = open_and_scan(&fs, "/t/x.sst");
        if let Ok(t) = open(&fs, "/t/x.sst") {
            for p in probes {
                let _ = t.get(&p, u64::MAX);
            }
        }
    }
}

#[test]
fn every_byte_flip_is_detected() {
    let entries: Vec<(Vec<u8>, Vec<u8>)> = (0..300u32)
        .map(|i| {
            let k = InternalKey::new(format!("key{i:04}").as_bytes(), 1, ValueKind::Put);
            (k.encode(), format!("v{i}").into_bytes())
        })
        .collect();
    let fs = new_fs();
    write_table(
        &fs,
        "/t/good.sst",
        &entries,
        TableOptions {
            block_size: 128,
            restart_interval: 16,
            ..Default::default() // with the bloom filter, so its CRC is covered too
        },
    );
    assert_eq!(open_and_scan(&fs, "/t/good.sst").unwrap(), entries);
    let good = fs.read(Path::new("/t/good.sst")).unwrap();

    let mut detected = 0;
    for i in 0..good.len() {
        let mut bad = good.clone();
        bad[i] ^= 0x01;
        fs.create(Path::new("/t/bad.sst"))
            .unwrap()
            .append(&bad)
            .unwrap();
        match open_and_scan(&fs, "/t/bad.sst") {
            Err(_) => detected += 1,
            Ok(got) => panic!(
                "flip at byte {i} of {} went unnoticed (data equal: {})",
                good.len(),
                got == entries
            ),
        }
    }
    assert_eq!(detected, good.len(), "every position was covered");
}

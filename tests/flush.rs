//! Flushing to L0 tables and recovering from them (milestone 4).
//!
//! These tests use the public API on `SimFs` and open the `.sst` files
//! directly with `strata::sstable::Table` to check what's on disk, for
//! example that no entry was written to two tables.

use std::ops::Bound::Unbounded;
use std::path::Path;
use std::sync::Arc;

use strata::fs::{FileSystem, SimFs};
use strata::sstable::Table;
use strata::{Db, Error, Options, SyncPolicy, WriteBatch};

const DIR: &str = "/db";

fn open(fs: &SimFs) -> strata::Result<Db> {
    Db::open_with_fs(DIR, Options::default(), Arc::new(fs.clone()))
}

fn all(db: &Db) -> Vec<(Vec<u8>, Vec<u8>)> {
    db.scan(Unbounded, Unbounded).unwrap()
}

fn files(fs: &SimFs, suffix: &str) -> Vec<String> {
    fs.list(Path::new(DIR))
        .unwrap()
        .into_iter()
        .filter(|n| n.ends_with(suffix))
        .collect()
}

/// The file number in a name like "000007.log".
fn number(name: &str) -> u64 {
    name[..6].parse().unwrap()
}

/// Entries (every version, tombstones included) across all `.sst` files.
fn table_entries(fs: &SimFs) -> usize {
    let mut n = 0;
    for name in files(fs, ".sst") {
        let t = Table::open(fs.open_random(&Path::new(DIR).join(&name)).unwrap()).unwrap();
        let mut it = t.iter();
        it.seek_to_first().unwrap();
        while it.valid() {
            n += 1;
            it.next().unwrap();
        }
    }
    n
}

fn key(i: usize) -> Vec<u8> {
    format!("key{i:05}").into_bytes()
}

fn reopen(fs: &SimFs, db: Db) -> Db {
    db.close().unwrap();
    drop(db);
    open(fs).unwrap()
}

#[test]
fn reopen_moves_data_into_a_table() {
    let fs = SimFs::new();
    let db = open(&fs).unwrap();
    for i in 0..100 {
        db.put(&key(i), b"v").unwrap();
    }
    let db = reopen(&fs, db);
    assert_eq!(files(&fs, ".sst").len(), 1);
    let logs = files(&fs, ".log");
    assert_eq!(logs.len(), 1);
    assert_eq!(
        fs.file_len(&Path::new(DIR).join(&logs[0])),
        Some(0),
        "the new log is empty: everything is in the table"
    );
    for i in 0..100 {
        assert_eq!(db.get(&key(i)).unwrap(), Some(b"v".to_vec()));
    }
    let tables = files(&fs, ".sst");
    let _db = reopen(&fs, db);
    assert_eq!(files(&fs, ".sst"), tables, "nothing to flush, no new table");
}

#[test]
fn tombstone_hides_flushed_value() {
    let fs = SimFs::new();
    let db = open(&fs).unwrap();
    db.put(b"k", b"v").unwrap();
    db.put(b"other", b"x").unwrap();
    let db = reopen(&fs, db); // k=v is now in a table
    db.delete(b"k").unwrap(); // the tombstone is in the memtable
    assert_eq!(db.get(b"k").unwrap(), None);
    assert_eq!(all(&db), vec![(b"other".to_vec(), b"x".to_vec())]);

    let db = reopen(&fs, db); // the tombstone is now in a newer table
    assert_eq!(files(&fs, ".sst").len(), 2);
    assert_eq!(db.get(b"k").unwrap(), None);
    assert_eq!(all(&db), vec![(b"other".to_vec(), b"x".to_vec())]);
}

#[test]
fn newest_version_wins_across_tables() {
    let fs = SimFs::new();
    let db = open(&fs).unwrap();
    db.put(b"k", b"1").unwrap();
    let db = reopen(&fs, db);
    db.put(b"k", b"2").unwrap();
    let db = reopen(&fs, db);
    assert_eq!(files(&fs, ".sst").len(), 2);
    assert_eq!(db.get(b"k").unwrap(), Some(b"2".to_vec()));

    // Reopen with nothing written: this open replays nothing, so only the
    // tables' max_seqno can tell it where the sequence counter stands. A new
    // write must be newer than both versions.
    let db = reopen(&fs, db);
    db.put(b"k", b"3").unwrap();
    assert_eq!(db.get(b"k").unwrap(), Some(b"3".to_vec()));
    let db = reopen(&fs, db);
    assert_eq!(db.get(b"k").unwrap(), Some(b"3".to_vec()));
    assert_eq!(all(&db), vec![(b"k".to_vec(), b"3".to_vec())]);
}

#[test]
fn leftover_temp_table_is_removed() {
    let fs = SimFs::new();
    fs.create_dir_all(Path::new(DIR)).unwrap();
    let tmp = Path::new(DIR).join("000099.sst.tmp");
    let mut f = fs.create(&tmp).unwrap();
    f.append(b"half a table").unwrap();
    f.sync().unwrap();
    fs.sync_dir(Path::new(DIR)).unwrap();

    let db = open(&fs).unwrap();
    assert!(files(&fs, ".tmp").is_empty(), "the leftover is gone");
    let logs = files(&fs, ".log");
    assert!(number(&logs[0]) > 99, "new files never reuse its number");
    db.put(b"k", b"v").unwrap();
    let db = reopen(&fs, db);
    assert!(files(&fs, ".sst").iter().all(|n| number(n) > 99));
    assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
}

#[test]
fn version_1_table_is_rejected() {
    // A milestone 3 table: an empty index block, its CRC, and a 48-byte
    // version 1 footer.
    let mut file = vec![0, 0, 0, 0, 1, 0, 0, 0];
    let crc = crc32(&file);
    file.extend_from_slice(&crc.to_le_bytes());
    let mut footer = Vec::new();
    footer.extend_from_slice(&0u64.to_le_bytes()); // index offset
    footer.extend_from_slice(&8u64.to_le_bytes()); // index size
    footer.extend_from_slice(&[0; 16]); // filter handle
    footer.extend_from_slice(&1u32.to_le_bytes()); // version 1
    let crc = crc32(&footer);
    footer.extend_from_slice(&crc.to_le_bytes());
    footer.extend_from_slice(b"strataT1");
    file.extend_from_slice(&footer);

    let fs = SimFs::new();
    fs.create_dir_all(Path::new(DIR)).unwrap();
    let mut f = fs.create(&Path::new(DIR).join("000001.sst")).unwrap();
    f.append(&file).unwrap();
    f.sync().unwrap();
    fs.sync_dir(Path::new(DIR)).unwrap();

    match open(&fs) {
        Err(Error::Corruption(msg)) => assert!(msg.contains("version 1"), "{msg}"),
        other => panic!("expected Corruption, got {:?}", other.err()),
    }
}

/// CRC-32 (IEEE), bitwise: small enough to not need the crate in tests.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// 100 batches of two keys, plus one delete: 201 entries.
fn write_known_data(db: &Db) {
    for i in 0..100 {
        let mut b = WriteBatch::new();
        b.put(&key(i), b"a").put(&key(1000 + i), b"b");
        db.write(b).unwrap();
    }
    db.delete(&key(3)).unwrap();
}
const KNOWN_ENTRIES: usize = 201;

#[test]
fn crash_at_every_step_of_open() {
    for n in 0.. {
        assert!(n < 10_000, "open never finished");
        // The same starting point every time: data in a log, no tables.
        let fs = SimFs::new();
        let db = open(&fs).unwrap();
        write_known_data(&db);
        let expected = all(&db);
        db.close().unwrap();
        drop(db);

        fs.kill_after(n);
        let finished = match open(&fs) {
            // Don't `close`: its sync could be one step too many.
            Ok(db) => {
                assert_eq!(all(&db), expected, "uninterrupted open");
                true
            }
            Err(_) => false,
        };
        // Power fails after dying at step n (or right after a full open).
        fs.crash();
        let db = open(&fs).unwrap();
        assert_eq!(all(&db), expected, "after dying at step {n} of open");
        assert!(
            table_entries(&fs) <= KNOWN_ENTRIES,
            "step {n}: {} entries on disk, so some were flushed twice",
            table_entries(&fs)
        );
        if finished {
            break;
        }
    }
}

#[test]
fn table_and_old_log_together_replay_same_state() {
    let fs = SimFs::new();
    let db = open(&fs).unwrap();
    write_known_data(&db);
    db.close().unwrap();
    drop(db);
    let old = files(&fs, ".log").pop().unwrap();
    fs.create_dir_all(Path::new("/saved")).unwrap();
    fs.copy_file(&Path::new(DIR).join(&old), Path::new("/saved/old.log"));

    let db = open(&fs).unwrap(); // flushes the log into a table, deletes it
    let expected = all(&db);
    db.close().unwrap();
    drop(db);
    assert!(!files(&fs, ".log").contains(&old));
    assert_eq!(table_entries(&fs), KNOWN_ENTRIES);

    // As if the crash came after the table was durable but before the old
    // log was removed: both exist. Every entry in the log is already in the
    // table, so replay must skip all of it.
    fs.copy_file(Path::new("/saved/old.log"), &Path::new(DIR).join(&old));
    let db = open(&fs).unwrap();
    assert_eq!(all(&db), expected);
    assert_eq!(files(&fs, ".sst").len(), 1, "nothing new to flush");
    assert_eq!(table_entries(&fs), KNOWN_ENTRIES, "no duplicates");
}

#[test]
fn bad_table_options_are_rejected_at_open() {
    let fs = SimFs::new();
    let mut o = Options {
        sync: SyncPolicy::Always,
        ..Options::default()
    };
    o.table.block_size = 0;
    assert!(matches!(
        Db::open_with_fs(DIR, o, Arc::new(fs.clone())),
        Err(Error::InvalidArgument(_))
    ));
}

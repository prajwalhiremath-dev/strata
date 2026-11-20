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
    open_with(fs, Options::default())
}

fn open_with(fs: &SimFs, o: Options) -> strata::Result<Db> {
    Db::open_with_fs(DIR, o, Arc::new(fs.clone()))
}

/// Options whose memtable fills after a few dozen small writes.
fn small(memtable_size: usize) -> Options {
    Options {
        memtable_size,
        ..Options::default()
    }
}

fn all(db: &Db) -> Rows {
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

/// Like `crash_at_every_step_of_open`, but the disk may persist unsynced
/// directory changes in any order.
#[test]
fn reordered_crash_at_every_step_of_open() {
    for n in 0.. {
        assert!(n < 10_000, "open never finished");
        let mut finished = false;
        for seed in 0..8 {
            let fs = SimFs::new();
            let db = open(&fs).unwrap();
            write_known_data(&db);
            let expected = all(&db);
            db.close().unwrap();
            drop(db);
            fs.kill_after(n);
            finished = open(&fs).is_ok();
            fs.crash_reordered(seed);
            let db = open(&fs).unwrap();
            assert_eq!(all(&db), expected, "step {n}, seed {seed}");
            assert!(table_entries(&fs) <= KNOWN_ENTRIES, "step {n}, seed {seed}");
        }
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

// ---- Flushing on the write path (Task 5) ----

#[test]
fn writes_trigger_flushes() {
    let fs = SimFs::new();
    let db = open_with(&fs, small(4096)).unwrap();
    for i in 0..2000 {
        db.put(&key(i), format!("value{i}").as_bytes()).unwrap();
    }
    assert!(files(&fs, ".sst").len() > 1, "{:?}", files(&fs, ".sst"));
    assert_eq!(files(&fs, ".log").len(), 1, "each flush deletes its log");
    let check = |db: &Db| {
        for i in 0..2000 {
            assert_eq!(
                db.get(&key(i)).unwrap(),
                Some(format!("value{i}").into_bytes())
            );
        }
        assert_eq!(all(db).len(), 2000);
    };
    check(&db);
    db.close().unwrap();
    drop(db);
    check(&open_with(&fs, small(4096)).unwrap());
}

#[test]
fn explicit_flush() {
    let fs = SimFs::new();
    let db = open(&fs).unwrap();
    db.flush().unwrap();
    assert!(files(&fs, ".sst").is_empty(), "empty memtable: no table");
    db.put(b"k", b"v").unwrap();
    db.flush().unwrap();
    assert_eq!(files(&fs, ".sst").len(), 1);
    assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
    db.flush().unwrap();
    assert_eq!(files(&fs, ".sst").len(), 1, "nothing new to flush");
}

#[test]
fn tombstone_flushed_then_reopen() {
    let fs = SimFs::new();
    let db = open(&fs).unwrap();
    db.put(b"k", b"v").unwrap();
    db.flush().unwrap();
    db.delete(b"k").unwrap();
    db.flush().unwrap();
    assert_eq!(db.get(b"k").unwrap(), None);
    let db = reopen(&fs, db);
    assert_eq!(db.get(b"k").unwrap(), None);
    assert!(all(&db).is_empty());
}

type Rows = Vec<(Vec<u8>, Vec<u8>)>;

fn fifty_keys(fs: &SimFs) -> (Db, Rows) {
    let db = open(fs).unwrap();
    for i in 0..50 {
        db.put(&key(i), format!("v{i}").as_bytes()).unwrap();
    }
    let expected = all(&db);
    (db, expected)
}

#[test]
fn crash_at_every_step_of_flush() {
    for n in 0.. {
        assert!(n < 10_000, "flush never finished");
        // Each crash flavour gets the same starting point.
        let crashes: [&dyn Fn(&SimFs); 5] = [
            &|fs| fs.crash(),
            &|fs| fs.crash_reordered(n as u64),
            &|fs| fs.crash_reordered(n as u64 + 1000),
            &|fs| fs.crash_reordered(n as u64 + 2000),
            &|fs| fs.crash_reordered(n as u64 + 3000),
        ];
        let mut finished = false;
        for crash in crashes {
            let fs = SimFs::new();
            let (db, expected) = fifty_keys(&fs);
            fs.kill_after(n);
            finished = db.flush().is_ok();
            crash(&fs);
            drop(db);
            let db = open(&fs).unwrap();
            assert_eq!(all(&db), expected, "after dying at step {n} of flush");
            assert!(
                table_entries(&fs) <= 50,
                "step {n}: {} entries on disk",
                table_entries(&fs)
            );
        }
        if finished {
            break;
        }
    }
}

#[test]
fn failed_flush_poisons() {
    let fs = SimFs::new();
    let (db, expected) = fifty_keys(&fs);
    fs.fail_next_sync();
    assert!(matches!(db.flush(), Err(Error::Io(_))));
    assert!(matches!(db.put(b"later", b"x"), Err(Error::Poisoned(_))));
    assert!(matches!(db.flush(), Err(Error::Poisoned(_))));
    assert_eq!(all(&db), expected, "reads still work");
    assert_eq!(db.get(&key(7)).unwrap(), Some(b"v7".to_vec()));
    fs.crash();
    drop(db);
    let db = open(&fs).unwrap();
    assert_eq!(all(&db), expected);
}

#[test]
fn write_that_triggers_failed_flush_is_not_applied() {
    let fs = SimFs::new();
    let db = open_with(&fs, small(1024)).unwrap();
    let mut i = 0;
    // Fill the memtable up to the size that makes the next write flush first.
    while files(&fs, ".sst").is_empty() {
        db.put(&key(i), b"0123456789").unwrap();
        i += 1;
    }
    // The write numbered i - 1 found the memtable full (holding i - 1
    // entries), flushed it, and went into the new one. Refill it to i - 1.
    for j in i..i + (i - 2) {
        db.put(&key(j), b"0123456789").unwrap();
    }
    assert_eq!(files(&fs, ".sst").len(), 1, "no second flush yet");
    let expected = all(&db);
    // The next put must flush first; make that flush fail.
    fs.fail_next_sync();
    assert!(db.put(b"trigger", b"x").is_err());
    assert_eq!(db.get(b"trigger").unwrap(), None);
    assert_eq!(all(&db), expected);
    assert_eq!(
        files(&fs, ".sst").len(),
        1,
        "the failed flush published nothing"
    );
    fs.crash();
    drop(db);
    let db = open(&fs).unwrap();
    assert_eq!(db.get(b"trigger").unwrap(), None, "never logged");
    assert_eq!(all(&db), expected);
}

#[test]
fn reader_during_flushes() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let fs = SimFs::new();
    let db = open_with(&fs, small(4096)).unwrap();
    for i in 0..100 {
        db.put(&key(i), b"early").unwrap();
    }
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let (db, stop) = (db.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut rounds = 0u64;
            while !stop.load(Ordering::Relaxed) {
                for i in 0..100 {
                    assert_eq!(db.get(&key(i)).unwrap(), Some(b"early".to_vec()), "key {i}");
                }
                rounds += 1;
            }
            rounds
        })
    };
    for i in 100..5100 {
        db.put(&key(i), b"late").unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    let rounds = reader.join().expect("reader saw a missing key");
    assert!(files(&fs, ".sst").len() > 5, "many flushes happened");
    assert!(rounds > 0);
}

#[test]
fn huge_file_number_is_corruption_not_overflow() {
    let fs = SimFs::new();
    fs.create_dir_all(Path::new(DIR)).unwrap();
    for n in [u64::MAX, u64::MAX - 1, (1 << 48) + 1] {
        let name = Path::new(DIR).join(format!("{n}.log"));
        let mut f = fs.create(&name).unwrap();
        f.sync().unwrap();
        fs.sync_dir(Path::new(DIR)).unwrap();
        assert!(matches!(open(&fs), Err(Error::Corruption(_))), "{n}");
        fs.remove(&name).unwrap();
    }
}

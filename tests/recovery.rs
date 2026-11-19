//! Durability and crash-recovery tests (milestone 2).
//!
//! Most tests run on `SimFs`, which can "pull the plug" and keep only what
//! was synced (or, with `crash_torn`, a torn version of the unsynced tail).

use std::ops::Bound::Unbounded;
use std::path::Path;
use std::sync::Arc;

use strata::fs::{FileSystem, SimFs};
use strata::{Db, Error, Options, SyncPolicy, WriteBatch};

const DIR: &str = "/db";

fn opts(sync: SyncPolicy) -> Options {
    Options {
        sync,
        ..Options::default()
    }
}

fn paranoid(sync: SyncPolicy) -> Options {
    Options {
        sync,
        paranoid_checks: true,
        ..Options::default()
    }
}

fn open(fs: &SimFs, o: Options) -> strata::Result<Db> {
    Db::open_with_fs(DIR, o, Arc::new(fs.clone()))
}

fn all(db: &Db) -> Vec<(Vec<u8>, Vec<u8>)> {
    db.scan(Unbounded, Unbounded).unwrap()
}

fn key(prefix: &str, i: usize) -> Vec<u8> {
    format!("{prefix}{i:05}").into_bytes()
}

/// Batch i writes a{i} and b{i} with value i: both or neither must survive.
fn write_pairs(db: &Db, range: std::ops::Range<usize>) {
    for i in range {
        let v = i.to_string();
        let mut b = WriteBatch::new();
        b.put(&key("a", i), v.as_bytes())
            .put(&key("b", i), v.as_bytes());
        db.write(b).unwrap();
    }
}

/// Recovered pairs must be exactly batches 0..k for some k. Returns k.
fn assert_pair_prefix(db: &Db) -> usize {
    // Only the a/b pair keys (letter + 5 digits); ignore other test keys.
    let rows: Vec<_> = all(db)
        .into_iter()
        .filter(|(k, _)| k.len() == 6 && (k[0] == b'a' || k[0] == b'b') && k[1].is_ascii_digit())
        .collect();
    assert_eq!(rows.len() % 2, 0, "a batch was half-recovered: {rows:?}");
    let k = rows.len() / 2;
    for i in 0..k {
        let v = i.to_string().into_bytes();
        assert_eq!(db.get(&key("a", i)).unwrap(), Some(v.clone()), "a{i}");
        assert_eq!(db.get(&key("b", i)).unwrap(), Some(v), "b{i}");
    }
    k
}

fn log_files(fs: &SimFs) -> Vec<String> {
    fs.list(Path::new(DIR))
        .unwrap()
        .into_iter()
        .filter(|n| n.ends_with(".log"))
        .collect()
}

#[test]
fn reopen_recovers_writes_os() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), Options::default()).unwrap();
    db.put(b"a", b"1").unwrap();
    db.put(b"b", b"2").unwrap();
    db.delete(b"a").unwrap();
    let mut batch = WriteBatch::new();
    batch.put(b"c", b"3").put(b"d", b"4");
    db.write(batch).unwrap();
    let before = all(&db);
    db.close().unwrap();
    drop(db);

    let db = Db::open(dir.path(), Options::default()).unwrap();
    assert_eq!(all(&db), before);
    assert_eq!(db.get(b"a").unwrap(), None, "the delete survived too");
}

#[test]
fn acknowledged_writes_survive_crash() {
    let fs = SimFs::new();
    let db = open(&fs, opts(SyncPolicy::Always)).unwrap();
    write_pairs(&db, 0..200);
    fs.crash();
    drop(db);
    let db = open(&fs, opts(SyncPolicy::Always)).unwrap();
    assert_eq!(assert_pair_prefix(&db), 200);
}

#[test]
fn never_policy_recovers_a_prefix() {
    for seed in 0..50 {
        let fs = SimFs::new();
        let db = open(&fs, opts(SyncPolicy::Never)).unwrap();
        write_pairs(&db, 0..200);
        fs.crash_torn(seed);
        drop(db);
        let db = open(&fs, opts(SyncPolicy::Never)).unwrap();
        assert_pair_prefix(&db);
    }
}

#[test]
fn every_n_policy_syncs_periodically() {
    let fs = SimFs::new();
    let db = open(&fs, opts(SyncPolicy::EveryN(10))).unwrap();
    write_pairs(&db, 0..25);
    fs.crash();
    drop(db);
    let db = open(&fs, opts(SyncPolicy::Always)).unwrap();
    let k = assert_pair_prefix(&db);
    assert!((20..25).contains(&k), "synced at 10 and 20; got {k}");
}

#[test]
fn close_syncs_even_with_never_policy() {
    let fs = SimFs::new();
    let db = open(&fs, opts(SyncPolicy::Never)).unwrap();
    write_pairs(&db, 0..10);
    db.close().unwrap();
    fs.crash();
    drop(db);
    let db = open(&fs, opts(SyncPolicy::Never)).unwrap();
    assert_eq!(assert_pair_prefix(&db), 10);
}

/// The bug this guards against: appending new records after a torn tail.
/// Recovery stops at the torn record, so anything after it would be lost.
fn torn_then_write_then_crash(seed: u64, o: Options) {
    let fs = SimFs::new();
    let db = open(&fs, opts(SyncPolicy::Never)).unwrap();
    write_pairs(&db, 0..50);
    fs.crash_torn(seed);
    drop(db);

    let db = open(&fs, o.clone()).unwrap();
    let k = assert_pair_prefix(&db);
    db.put(b"z-after-recovery", b"must survive").unwrap();
    fs.crash();
    drop(db);

    let db = open(&fs, o).unwrap();
    assert_eq!(
        db.get(b"z-after-recovery").unwrap(),
        Some(b"must survive".to_vec()),
        "seed {seed}"
    );
    assert_eq!(assert_pair_prefix(&db), k, "seed {seed}");
}

#[test]
fn torn_tail_then_more_writes_survive() {
    for seed in 0..50 {
        torn_then_write_then_crash(seed, opts(SyncPolicy::Always));
    }
}

#[test]
fn torn_tail_opens_in_paranoid_mode() {
    for seed in 0..50 {
        torn_then_write_then_crash(seed, paranoid(SyncPolicy::Always));
    }
}

#[test]
fn mid_log_corruption() {
    let fs = SimFs::new();
    let db = open(&fs, opts(SyncPolicy::Always)).unwrap();
    let big = vec![b'v'; 4000];
    for i in 0..40 {
        db.put(&key("k", i), &big).unwrap(); // ~160 KB: about 5 blocks
    }
    db.close().unwrap();
    drop(db);

    let logs = log_files(&fs);
    assert_eq!(logs.len(), 1);
    fs.corrupt_byte(&Path::new(DIR).join(&logs[0]), 40_000);

    assert!(matches!(
        open(&fs, paranoid(SyncPolicy::Always)),
        Err(Error::Corruption(_))
    ));
    // Point-in-time recovery: keep everything before the damage.
    let db = open(&fs, opts(SyncPolicy::Always)).unwrap();
    let n = all(&db).len();
    assert!(n > 0 && n < 40, "recovered {n}");
    for i in 0..n {
        assert!(
            db.get(&key("k", i)).unwrap().is_some(),
            "prefix contains k{i}"
        );
    }
}

#[test]
fn crash_at_every_step_of_open() {
    let fs = SimFs::new();
    let db = open(&fs, opts(SyncPolicy::Always)).unwrap();
    write_pairs(&db, 0..100);
    db.delete(&key("a", 3)).unwrap();
    let expected = all(&db);
    db.close().unwrap();
    drop(db);

    for n in 0.. {
        assert!(n < 10_000, "open never finished");
        fs.kill_after(n);
        match open(&fs, opts(SyncPolicy::Always)) {
            Ok(db) => {
                // Open needed exactly `n` mutating steps. Don't `close`: its
                // sync would be step n+1 and hit the simulated death.
                assert_eq!(all(&db), expected, "uninterrupted open");
                // Power fails right after a successful open, before any write:
                // the checkpoint must already be durable.
                fs.crash();
                drop(db);
                let db = open(&fs, opts(SyncPolicy::Always)).unwrap();
                assert_eq!(all(&db), expected, "crash right after open");
                break;
            }
            Err(_) => {
                fs.crash();
                let db = open(&fs, opts(SyncPolicy::Always)).unwrap();
                assert_eq!(all(&db), expected, "after dying at step {n} of open");
                db.close().unwrap();
            }
        }
    }
}

#[test]
fn old_and_new_logs_together_replay_same_state() {
    let fs = SimFs::new();
    let db = open(&fs, opts(SyncPolicy::Always)).unwrap();
    write_pairs(&db, 0..50);
    write_pairs(&db, 0..25); // overwrite with identical values but newer seqnos
    db.delete(&key("b", 7)).unwrap();
    db.close().unwrap();
    drop(db);

    let old = log_files(&fs).pop().unwrap();
    fs.create_dir_all(Path::new("/saved")).unwrap();
    fs.copy_file(&Path::new(DIR).join(&old), Path::new("/saved/old.log"));

    let db = open(&fs, opts(SyncPolicy::Always)).unwrap(); // checkpoint: old log replaced
    let expected = all(&db);
    db.close().unwrap();
    drop(db);
    assert!(!log_files(&fs).contains(&old));

    // Simulate a crash after the new log was written but before the old one
    // was deleted: both exist. Replaying both must give the same state.
    fs.copy_file(Path::new("/saved/old.log"), &Path::new(DIR).join(&old));
    let db = open(&fs, opts(SyncPolicy::Always)).unwrap();
    assert_eq!(all(&db), expected);
}

fn poison_case(fail: impl Fn(&SimFs)) {
    let fs = SimFs::new();
    let db = open(&fs, opts(SyncPolicy::Always)).unwrap();
    db.put(b"before", b"ok").unwrap();
    fail(&fs);
    assert!(matches!(db.put(b"failed", b"x"), Err(Error::Io(_))));
    assert!(matches!(db.put(b"later", b"y"), Err(Error::Poisoned(_))));
    assert!(matches!(db.delete(b"before"), Err(Error::Poisoned(_))));
    assert_eq!(
        db.get(b"before").unwrap(),
        Some(b"ok".to_vec()),
        "reads still work"
    );

    fs.crash();
    drop(db);
    let db = open(&fs, opts(SyncPolicy::Always)).unwrap();
    assert_eq!(db.get(b"before").unwrap(), Some(b"ok".to_vec()));
    assert_eq!(db.get(b"later").unwrap(), None);
    db.put(b"after-reopen", b"z").unwrap();
}

#[test]
fn failed_append_poisons() {
    poison_case(|fs| fs.fail_next_append());
}

#[test]
fn failed_sync_poisons() {
    poison_case(|fs| fs.fail_next_sync());
}

#[test]
fn second_open_fails_while_open() {
    let fs = SimFs::new();
    let db = open(&fs, Options::default()).unwrap();
    assert!(matches!(
        open(&fs, Options::default()),
        Err(Error::AlreadyOpen(_))
    ));
    db.close().unwrap();
    let _again = open(&fs, Options::default()).unwrap();
}

#[test]
fn second_open_fails_while_open_os() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path(), Options::default()).unwrap();
    assert!(matches!(
        Db::open(dir.path(), Options::default()),
        Err(Error::AlreadyOpen(_))
    ));
    db.close().unwrap();
    Db::open(dir.path(), Options::default()).unwrap();
}

#[test]
fn seqno_continues_after_reopen() {
    let fs = SimFs::new();
    let db = open(&fs, Options::default()).unwrap();
    db.put(b"k", b"v1").unwrap();
    db.close().unwrap();
    drop(db);
    let db = open(&fs, Options::default()).unwrap();
    db.put(b"k", b"v2").unwrap();
    assert_eq!(db.get(b"k").unwrap(), Some(b"v2".to_vec()));
    db.close().unwrap();
    drop(db);
    let db = open(&fs, Options::default()).unwrap();
    assert_eq!(db.get(b"k").unwrap(), Some(b"v2".to_vec()));
}

#[test]
fn every_n_zero_is_rejected() {
    let fs = SimFs::new();
    assert!(matches!(
        open(&fs, opts(SyncPolicy::EveryN(0))),
        Err(Error::InvalidArgument(_))
    ));
}

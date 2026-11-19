use std::ops::Bound::Unbounded;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use strata::{Db, Error, MAX_KEY_LEN, Options, WriteBatch};

fn db() -> Db {
    Db::open_in_memory(Options::default()).unwrap()
}

fn all(db: &Db) -> Vec<(Vec<u8>, Vec<u8>)> {
    db.scan(Unbounded, Unbounded).unwrap()
}

#[test]
fn put_get() {
    let db = db();
    db.put(b"hello", b"world").unwrap();
    assert_eq!(db.get(b"hello").unwrap(), Some(b"world".to_vec()));
}

#[test]
fn get_missing_is_none() {
    assert_eq!(db().get(b"nope").unwrap(), None);
}

#[test]
fn empty_value_is_some() {
    let db = db();
    db.put(b"k", b"").unwrap();
    assert_eq!(db.get(b"k").unwrap(), Some(vec![]));
}

#[test]
fn overwrite_returns_latest() {
    let db = db();
    db.put(b"k", b"v1").unwrap();
    db.put(b"k", b"v2").unwrap();
    assert_eq!(db.get(b"k").unwrap(), Some(b"v2".to_vec()));
}

#[test]
fn delete_then_reput() {
    let db = db();
    db.put(b"k", b"v1").unwrap();
    db.delete(b"k").unwrap();
    assert_eq!(db.get(b"k").unwrap(), None);
    db.put(b"k", b"v2").unwrap();
    assert_eq!(db.get(b"k").unwrap(), Some(b"v2".to_vec()));
}

#[test]
fn delete_missing_ok() {
    let db = db();
    db.delete(b"ghost").unwrap();
    assert_eq!(db.get(b"ghost").unwrap(), None);
}

#[test]
fn batch_applies_all() {
    let db = db();
    db.put(b"c", b"old").unwrap();
    let mut b = WriteBatch::new();
    b.put(b"a", b"1").put(b"b", b"2").delete(b"c");
    assert_eq!(b.len(), 3);
    db.write(b).unwrap();
    assert_eq!(
        all(&db),
        vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec())
        ]
    );
}

#[test]
fn batch_later_op_wins_on_same_key() {
    let db = db();
    let mut b = WriteBatch::new();
    b.put(b"k", b"first").put(b"k", b"second");
    db.write(b).unwrap();
    assert_eq!(db.get(b"k").unwrap(), Some(b"second".to_vec()));
}

#[test]
fn empty_batch_ok() {
    let db = db();
    db.write(WriteBatch::new()).unwrap();
    assert!(all(&db).is_empty());
}

#[test]
fn scan_skips_deleted() {
    let db = db();
    db.put(b"a", b"1").unwrap();
    db.put(b"b", b"2").unwrap();
    db.put(b"c", b"3").unwrap();
    db.delete(b"b").unwrap();
    let keys: Vec<_> = all(&db).into_iter().map(|(k, _)| k).collect();
    assert_eq!(keys, vec![b"a".to_vec(), b"c".to_vec()]);
}

#[test]
fn empty_key_rejected() {
    let db = db();
    assert!(matches!(db.put(b"", b"v"), Err(Error::InvalidArgument(_))));
    assert!(matches!(db.delete(b""), Err(Error::InvalidArgument(_))));
    assert!(all(&db).is_empty());
}

#[test]
fn oversized_key_rejected() {
    let db = db();
    let big = vec![b'x'; MAX_KEY_LEN + 1];
    assert!(matches!(db.put(&big, b"v"), Err(Error::InvalidArgument(_))));
    // A key of exactly the limit is fine.
    db.put(&vec![b'y'; MAX_KEY_LEN], b"v").unwrap();
    assert_eq!(all(&db).len(), 1);
}

#[test]
fn invalid_key_rejects_whole_batch() {
    let db = db();
    let mut b = WriteBatch::new();
    b.put(b"good", b"1").put(b"", b"bad");
    assert!(matches!(db.write(b), Err(Error::InvalidArgument(_))));
    assert!(
        all(&db).is_empty(),
        "nothing from a rejected batch is applied"
    );
}

#[test]
fn closed_db_errors() {
    let db = db();
    db.put(b"k", b"v").unwrap();
    db.close().unwrap();
    assert!(matches!(db.put(b"k", b"v"), Err(Error::Closed)));
    assert!(matches!(db.delete(b"k"), Err(Error::Closed)));
    assert!(matches!(db.get(b"k"), Err(Error::Closed)));
    assert!(matches!(db.scan(Unbounded, Unbounded), Err(Error::Closed)));
    assert!(matches!(db.write(WriteBatch::new()), Err(Error::Closed)));
    db.close().unwrap(); // closing twice is fine
}

#[test]
fn clones_share_state() {
    let db = db();
    let db2 = db.clone();
    db.put(b"k", b"v").unwrap();
    assert_eq!(db2.get(b"k").unwrap(), Some(b"v".to_vec()));
}

/// A reader must never observe half of a batch. Each batch sets x and y to
/// the same value, so any scan seeing x != y caught a partial batch.
#[test]
fn batch_is_atomic_to_readers() {
    let db = db();
    let done = Arc::new(AtomicBool::new(false));

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let db = db.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                let mut checks = 0u64;
                while !done.load(Ordering::Relaxed) {
                    let rows = db.scan(Unbounded, Unbounded).unwrap();
                    let x = rows.iter().find(|(k, _)| k == b"x").map(|(_, v)| v.clone());
                    let y = rows.iter().find(|(k, _)| k == b"y").map(|(_, v)| v.clone());
                    assert_eq!(x, y, "saw a half-applied batch");
                    checks += 1;
                }
                checks
            })
        })
        .collect();

    for i in 0u32..1000 {
        let v = i.to_be_bytes();
        let mut b = WriteBatch::new();
        b.put(b"x", &v).put(b"y", &v);
        db.write(b).unwrap();
    }
    done.store(true, Ordering::Relaxed);
    let checks: u64 = readers.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(checks > 0);
    assert_eq!(db.get(b"x").unwrap(), Some(999u32.to_be_bytes().to_vec()));
}

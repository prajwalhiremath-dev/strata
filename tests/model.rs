//! Model-based test: strata must behave exactly like a `BTreeMap`.
//!
//! proptest generates random sequences of operations, runs them against both
//! strata and a plain `BTreeMap` (the "model" or "oracle"), and checks every
//! read returns the same answer. When it finds a failing sequence it
//! *shrinks* it to the smallest sequence that still fails.
//!
//! Later milestones add operations here (Flush, Reopen, Snapshot, Crash), so
//! every new layer is tested against the same simple truth.

use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Included};

use proptest::prelude::*;
use strata::{Db, Options, WriteBatch};

#[derive(Debug, Clone)]
enum Op {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
    Get(Vec<u8>),
    /// Scan the half-open range [start, end).
    Scan(Vec<u8>, Vec<u8>),
    /// `None` value = delete.
    Batch(Vec<(Vec<u8>, Option<Vec<u8>>)>),
}

/// Tiny key alphabet (1–3 bytes from a..=e) so the same keys are hit often:
/// overwrites and delete-then-put are where bugs live.
fn key() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(b'a'..=b'e', 1..=3)
}

fn value() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..=8)
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (key(), value()).prop_map(|(k, v)| Op::Put(k, v)),
        2 => key().prop_map(Op::Delete),
        3 => key().prop_map(Op::Get),
        1 => (key(), key()).prop_map(|(a, b)| Op::Scan(a, b)),
        1 => proptest::collection::vec((key(), proptest::option::of(value())), 0..6)
            .prop_map(Op::Batch),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn db_matches_btreemap(ops in proptest::collection::vec(op(), 1..200)) {
        let db = Db::open_in_memory(Options::default()).unwrap();
        let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

        for op in ops {
            match op {
                Op::Put(k, v) => {
                    db.put(&k, &v).unwrap();
                    model.insert(k, v);
                }
                Op::Delete(k) => {
                    db.delete(&k).unwrap();
                    model.remove(&k);
                }
                Op::Get(k) => {
                    prop_assert_eq!(db.get(&k).unwrap(), model.get(&k).cloned(), "get {:?}", k);
                }
                Op::Scan(a, b) => {
                    let got = db.scan(Included(&a[..]), Excluded(&b[..])).unwrap();
                    let want: Vec<_> = if a < b {
                        model.range(a.clone()..b.clone()).map(|(k, v)| (k.clone(), v.clone())).collect()
                    } else {
                        Vec::new() // empty or reversed range
                    };
                    prop_assert_eq!(got, want, "scan [{:?}, {:?})", a, b);
                }
                Op::Batch(entries) => {
                    let mut batch = WriteBatch::new();
                    for (k, v) in &entries {
                        match v {
                            Some(v) => { batch.put(k, v); }
                            None => { batch.delete(k); }
                        }
                    }
                    db.write(batch).unwrap();
                    for (k, v) in entries {
                        match v {
                            Some(v) => { model.insert(k, v); }
                            None => { model.remove(&k); }
                        }
                    }
                }
            }
        }

        // Final full comparison.
        let everything = db.scan(std::ops::Bound::Unbounded, std::ops::Bound::Unbounded).unwrap();
        let want: Vec<_> = model.into_iter().collect();
        prop_assert_eq!(everything, want);
    }
}

# Interview notes 01: Internal keys, memtables, MVCC

Answer each one out loud before reading the model answer.

---

### 1. How does an LSM implement delete?

It writes a **tombstone**: an entry with the same user key, a new sequence number and `kind = Delete`. Reads search newest to oldest, so they see the tombstone first and return not-found.

The old value is physically removed later, by compaction. The tombstone itself can be dropped only when it reaches the bottom level and no snapshot still needs the older value.

---

### 2. What is a sequence number, and why not use a timestamp?

It's a global counter, incremented for every write under the write lock. Every version of every key gets a unique, strictly increasing number, so "newest" is always well defined.

Wall-clock timestamps can go backwards (NTP adjustments) and can collide, which would make "which version wins" ambiguous.

---

### 3. Why sort versions of a key newest-first?

So that one seek to `(key, snapshot_seqno)` lands directly on the newest visible version. With oldest-first ordering you'd scan every version of the key.

It also simplifies merging sorted runs later: the first entry seen for each key wins, and the rest are skipped.

---

### 4. What is MVCC, and how do sequence numbers enable it?

**Multi-version concurrency control** keeps several versions of data so readers can see a consistent point in time without blocking writers.

In an LSM every version carries its seqno. A reader (or a snapshot) picks a seqno N and ignores every entry with a seqno above N. That's a consistent view as of N, with no locks held on the data while reading.

---

### 5. How would you make a multi-key batch atomic for concurrent readers?

1. Assign the batch's entries seqnos above the currently visible seqno.
2. Insert them all.
3. Publish the batch's last seqno with a single atomic store (Release).

Readers load the visible seqno once (Acquire) and ignore anything newer. Until the publish, none of the batch is visible. After it, all of it is.

Writers are serialised so seqnos are published in order. Group commit keeps that ordering while sharing the work.

---

### 6. Why does a lookup need three outcomes (found / deleted / not found)?

"Not found here" means *keep searching older data*. "Deleted here" means *stop: the key is gone*. Treating a tombstone as "not here" lets an older value from disk come back to life, which is a classic resurrection bug.

---

### 7. Skiplist vs balanced tree for a memtable?

Both are sorted and O(log n).

A **concurrent skiplist** supports lock-free reads alongside inserts. It's simple to make concurrent (CAS on next pointers), and it's arena-friendly. That's why LevelDB, RocksDB and Pebble use one.

A **B-tree map behind a lock** is simpler and has better cache locality for single-threaded use, but readers block while a writer holds the lock.

We started with the B-tree map for correctness and will measure before switching.

---

### 8. What is model-based (property-based) testing, and why use it for a storage engine?

You generate random operation sequences, run them against your system and against a trivially correct model (a hash map or sorted map), and compare every result. The framework shrinks failures to a minimal reproduction.

Storage engines have huge state spaces: overwrite, delete, flush, compaction and reopen, in any order. Hand-written tests miss interleavings that random sequences find. It's the same spirit as Jepsen, and as FoundationDB's simulation testing.

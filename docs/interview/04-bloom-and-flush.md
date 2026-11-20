# Interview notes 04: bloom filters and flushing

Answer each one out loud before reading the model answer.

---

### 1. How does a bloom filter's false-positive rate depend on its size, and how do you choose bits per key?

With `b` bits per key and `k` hash probes, the rate is about `(1 − e^(−k/b))^k`. It's minimised at `k = b · ln 2`, where it becomes about `0.6185^b`: each extra bit per key multiplies the rate by about 0.62, and halving it costs about 1.44 bits.

Ten bits per key (k = 7) gives about 0.8%. strata measured 0.83% against a theoretical 0.82% on 100,000 keys.

Choose from what a false positive costs: here, one wasted 4 KiB block read. Ten bits per key is 1.25 bytes of memory per key to avoid 99% of those reads, which is why it's the LevelDB/RocksDB default.

---

### 2. Why can't a bloom filter have false negatives, and how do deletes work with one?

Adding a key only ever **sets** bits, and a query for that key tests the same bits. So once a key is added, its query sees all its bits set. For the same reason you can't remove a key: clearing its bits could clear bits other keys rely on, which would create false negatives.

An LSM tree doesn't need removal. Tables are immutable, and a delete is a **tombstone entry**, so the deleted key *is* in the newer table's filter. The lookup passes that filter, finds the tombstone, and stops with "not found" before reaching the older table that still holds the value. Counting or cuckoo filters support deletion, but no table here ever needs it.

---

### 3. Walk through flushing a memtable. What makes it crash-safe?

Under the writer lock:

1. Create a new log and **sync the directory**.
2. **Rotate**: the full memtable becomes the immutable `imm`, an empty one takes its place, and writes go to the new log.
3. Write `imm` to `N.sst.tmp`, sync it, **rename** it to `N.sst`, and sync the directory.
4. **Publish**: the table joins the read state, and `imm` goes away.
5. Delete the old log and sync the directory.

The invariant: the old log is deleted only **after** the table is durable, so every acknowledged write is in at least one of them at every instant.

If both exist after a crash, recovery replays the log but skips every seqno at or below the tables' highest. Those entries are already in a table, so nothing is written twice.

A leftover `.tmp` is deleted at open; nothing relied on it, because the log was still there.

---

### 4. Why write to a temp file and rename? Why sync the directory afterwards?

**Rename is atomic**: after a crash you see the old name or the new one, never a half-written file under the final name. So "is there a `.sst`?" really means "is there a complete table?". Without it, a crash mid-write would leave a truncated table that open treats as live.

**Syncing the directory** makes the *name* durable: a file's contents and its directory entry are synced separately.

The sync after the rename matters even though a later step syncs the directory again. POSIX doesn't order unsynced directory changes, so the later deletion of the log could reach the disk while the rename doesn't, and then the data is in neither place.

strata's first crash simulator couldn't see this, because it treated a directory sync as an atomic snapshot. Removing that sync passed every test until a "reordered" crash mode was added.

---

### 5. Why does the immutable memtable exist?

Between "this memtable is full" and "its table is on disk", the data has to stay readable. `imm` keeps the old memtable in the read state, so readers search `mem → imm → tables` and miss nothing during a flush.

It also decouples writers from the flush. With a background flush, writes continue into the new memtable while `imm` is written. strata's flush is still synchronous; that's milestone 8. If `imm` is still being flushed when the new memtable fills, writes have to wait. That's a **write stall**, and RocksDB lets several immutable memtables queue up to absorb bursts.

---

### 6. What is read amplification in L0, and what do filters do and not do about it?

L0 tables are flushed memtables, so their key ranges **overlap**: any table might hold any key. A point lookup must check tables newest-first until it finds the key or a tombstone. With `T` tables, a key that's absent everywhere costs `T` checks. That's read amplification.

Filters turn most of those checks into an in-memory bit test (about 100 ns, against about 3 µs for a block read in strata's measurements). But the cost is still linear in `T`, and **range scans get no help at all**: a filter answers "is this exact key here?", not "are any keys in this range here?".

The real fix is **compaction**, which merges L0 into sorted, non-overlapping levels, so each level has at most one candidate table per key.

---

### 7. A filter per table, per block, or partitioned: what's the trade?

- **One filter per table** (what strata does, RocksDB's "full filter"): it lives in memory, and a negative answer costs zero I/O. But a huge table needs a huge filter loaded at open.
- **One filter per block range** (LevelDB's original format): you first find the block, then check its filter. Each piece is small, but the index lookup comes first, and caching the pieces individually adds bookkeeping.
- **Partitioned filters** (RocksDB): a full filter split into blocks with a top-level index. Only the partitions you touch need to be in memory or cache. That suits very large tables or many tables, where all filters don't fit in RAM.

The general trade is memory against I/O and lookup steps.

---

### 8. Logs get deleted after flushes. How do sequence numbers survive a restart?

After a flush and a clean reopen, every log can be empty, so replay alone would restart the counter at 0. New writes would then get seqnos *older* than versions already on disk, and reads would return stale data.

strata stores each table's highest seqno (`max_seqno`) in its footer, and open sets the counter to `max(highest in any table, highest replayed)`. LevelDB keeps the last sequence number in its MANIFEST instead. Either way, the counter must be recoverable from what survives, not only from the logs.

A test for this has to reopen with **no** writes in between. strata's first version reopened straight after each write, so replay always restored the counter, and the bug it was meant to catch slipped past.

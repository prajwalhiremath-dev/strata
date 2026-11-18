# Interview notes 00: LSM fundamentals

Answer each one out loud before reading the model answer.

---

### 1. What is an LSM tree, in one or two sentences?

A storage structure that never updates data in place. Writes are appended to a log and inserted into a sorted in-memory table. That table is periodically written out as an immutable sorted file, and the files are merged in the background. It turns random writes into sequential ones.

---

### 2. When would you pick a B-tree over an LSM?

- **Read-heavy workloads with predictable latency.** A B-tree point read is a few page reads, with no bloom filters or multi-level checks.
- **When background work is unacceptable.** An LSM needs compaction to keep up. If it falls behind, reads slow down and writes may stall.
- **Lots of small in-place updates to hot data that fits in cache** (classic OLTP). The B-tree's random writes are absorbed by the buffer pool.

Pick an LSM for write-heavy or ingest-heavy workloads (logs, time series, event stores), when you want good compression (immutable sorted files compress well), or on storage where sequential writes are much cheaper.

---

### 3. What is write amplification, and where does it come from in an LSM?

The ratio of bytes physically written to bytes the user wrote. In an LSM:

- the value goes to the WAL (1×)
- it's flushed to L0 (1×)
- it's rewritten each time compaction moves it down a level, merging with overlapping data in the next level, which is about 10× larger

Leveled compaction typically lands at 10–30×. Tiered or size-tiered compaction lowers write amplification but increases space and read amplification.

---

### 4. How does an LSM delete a key?

It writes a **tombstone**: a special entry meaning "deleted as of sequence number N". Reads see the newest entry first, so they see the tombstone and return not-found. The old value is physically removed only when compaction merges the tombstone with it.

The tombstone itself can be dropped only once it reaches the bottom level, because older copies might still exist below it until then.

---

### 5. Why does a read check the memtable first, then L0, then L1…?

The newest data is in memory and older data sits deeper. The first match found when searching newest to oldest is the current value, so the search can stop there.

---

### 6. Why are SSTables immutable? What does that buy you?

- **Crash safety:** a file is either completely written and recorded in the manifest, or it's ignored. There's no half-updated state.
- **Concurrency:** readers never need locks on file contents, because nothing changes under them.
- **Caching and compression:** blocks never change, so they can be cached and compressed freely.

The cost is that updates and deletes need compaction to reclaim space.

---

### 7. Why is batching syncs so effective? (Use numbers.)

`fsync` has a fixed cost of milliseconds that barely depends on how many bytes you wrote. In our experiment, syncing every 100-byte record gave about 340 records/s. Syncing once per 100 records gave about 24,000 records/s, roughly 70× faster.

Group commit applies this to concurrent writers: one thread syncs on behalf of everyone waiting.

---

### 8. What is the RUM conjecture?

A storage structure can't simultaneously minimise **R**ead overhead, **U**pdate (write) overhead and **M**emory (space) overhead. Optimising two costs you the third. B-trees favour reads. LSMs favour writes. Leveled vs tiered compaction moves an LSM along the same trade-off curve.

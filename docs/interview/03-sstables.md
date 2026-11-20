# Interview notes 03: SSTables

Answer each one out loud before reading the model answer.

---

### 1. Describe the layout of an SSTable.

A file of sorted key-value entries, written once, never modified:

- **Data blocks** of about 4 KiB, each followed by a checksum. Keys are prefix-compressed, with restart points for binary search.
- An **index block**: one entry per data block, mapping that block's last key to its offset and size.
- Optionally a **filter block** (a bloom filter) and metadata blocks.
- A fixed-size **footer** at the very end: where the index (and filter) are, a format version, and a magic number.

A reader opens it from the end: footer → index (kept in memory) → one data block per lookup.

---

### 2. What is prefix compression, and why do you need restart points?

Sorted neighbours share long prefixes, so each entry stores `(shared length, the differing suffix, value)` instead of the whole key. In strata's experiment that makes the file about a third smaller.

But then rebuilding any key needs the previous one, all the way back to the start of the block, so you can't binary-search. **Restart points** store every N-th key in full and record their offsets at the end of the block. A seek binary-searches the restart keys, then walks forward at most N entries.

---

### 3. What does the restart interval trade off?

Space against CPU per lookup. Interval 1 means no compression, with pure binary search. A large interval means a smaller block but a longer linear walk after the binary search.

Measured in strata (100,000 keys, 4 KiB blocks):
- interval 1 → 16 saved 34% of the file, with no measurable lookup cost, because the block read dominates;
- 16 → 64 saved only 1.6% more and made lookups slower.

16 is the LevelDB/RocksDB default for that reason.

---

### 4. Walk through a point lookup in one SSTable.

1. Seek the in-memory index to the target internal key `(user_key, snapshot, Put)`. The first index entry whose *last key* is ≥ target names the only block that can hold the answer.
2. Read that block (one I/O), verify its checksum.
3. Binary-search its restart points, then scan forward to the first key ≥ target.
4. If that entry's **user key** matches, it's the newest version visible at the snapshot: a value, or a tombstone meaning "deleted, stop looking". Otherwise the key isn't in this file, so check older files.

With a bloom filter (next milestone), a definite "not here" skips steps 2–4 entirely.

---

### 5. How do multiple versions of one key coexist in a file, and how does a lookup find the right one?

Internal keys sort by user key ascending, then **sequence number descending**, so the newest version comes first. To read at snapshot `s`, seek to `(key, s)`. Everything newer sorts before that target, so the first entry at or after it is the newest version with seqno ≤ s.

Watch out: this order isn't byte order (the seqno is stored little-endian, and descending), so every comparison must use the internal-key comparator. Byte order lands on the wrong version.

---

### 6. How does the reader detect corruption? What could go wrong without these checks?

- A **CRC per block**, verified on every read before decoding.
- A **footer with its own CRC and a magic number**, so a bad handle can't send you to read garbage, and a non-table file is rejected.
- **Bounds checks on everything read from disk:** block handles inside the file (with overflow-checked arithmetic), restart counts, entry lengths, and shared prefix lengths.

Without them, a flipped bit could make you silently return wrong data, or try to allocate 2⁶³ bytes and abort. strata's test flips every single byte of a table in turn and requires an error every time.

---

### 7. Why are SSTables immutable? What are the costs?

Benefits:
- readers need no locks;
- no torn in-place writes, so crash safety is just "sync the file, then publish it";
- cached blocks never go stale;
- files are deleted whole.

Costs: updates and deletes write new versions elsewhere, so old versions and tombstones take space until **compaction** rewrites the files. That's the write amplification and space amplification of LSM trees, the trade made for fast sequential writes.

---

### 8. The index has one entry per block. How do real engines keep it small, and what makes lookups fast in practice?

- **Shortened separators** (LevelDB): store the shortest key between one block's last key and the next block's first key, not the full key.
- **Partitioned or two-level indexes** (RocksDB) for very large files, so the whole index needn't sit in memory.
- **Block compression** (Snappy, LZ4, Zstd) shrinks I/O.
- A **block cache** keeps hot, uncompressed blocks in memory, so a hit costs no read and no checksum. In strata's experiment, about 4 µs per lookup was almost all block read and checksum work, which is exactly what a cache removes.
- **Bloom filters** skip files that don't contain the key at all.

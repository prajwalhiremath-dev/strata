# Chapter 04: Bloom filters, and flushing the memtable to disk

> **Goal of this chapter:** understand how strata stops keeping everything in memory, by writing full memtables out as SSTables (a **flush**) and deleting their logs, without ever losing an acknowledged write at any crash point; and how a small **bloom filter** per table lets a lookup skip tables that can't hold the key.
>
> **Code:** [`src/sstable/bloom.rs`](../../src/sstable/bloom.rs) (the filter) · [`src/sstable/format.rs`](../../src/sstable/format.rs) (footer v2) · [`src/db/mod.rs`](../../src/db/mod.rs) (read state, read path, write trigger) · [`src/db/flush.rs`](../../src/db/flush.rs) (writing a table, the flush steps) · [`src/db/recovery.rs`](../../src/db/recovery.rs) (open) · [`src/db/files.rs`](../../src/db/files.rs) (file names) · [`src/fs/sim.rs`](../../src/fs/sim.rs) (`rename`, `crash_reordered`) · [`tests/flush.rs`](../../tests/flush.rs) · [`examples/bloom_demo.rs`](../../examples/bloom_demo.rs)

---

## 1. What it is

Until now, strata's data lived in the memtable, and the write-ahead log grew forever. Milestone 4 adds the step that makes it an LSM tree:

```text
write ──► log + memtable ──(memtable full)──► flush ──► 000007.sst  (L0)
                                                   └──► delete the log
```

When the memtable reaches `Options::memtable_size` (4 MiB by default), the next write first **flushes** it: the memtable becomes read-only, a new empty one takes its place, the old one is written as an SSTable (chapter 03), and the log that held its writes is deleted. Every flush adds one table to **level 0 (L0)**. A read then searches, newest first:

```text
mem  →  imm (only during a flush)  →  newest table  →  …  →  oldest table
```

Searching many tables is slow if every table costs a disk read. So each table now carries a **bloom filter** over its user keys. It can answer "this key is **definitely not** in this table" without touching the data blocks.

```rust
let db = Db::open(path, Options { memtable_size: 64 * 1024, ..Options::default() })?;
for i in 0..20_000 { db.put(format!("key{i:06}").as_bytes(), b"v")?; } // flushes along the way
db.flush()?;                     // or flush explicitly
db.get(b"key000042")?;           // memtable → tables, newest first
```

---

## 2. Why it's built this way

### Why flush at all?

Memory is bounded and logs are slow to replay. The memtable can't grow without limit, and a log that holds every write since the beginning would take minutes to replay at startup. A flush turns "the last 4 MiB of writes" into a sorted, indexed, checksummed file, and then lets the log go.

### Why one table per flush, in L0?

A flush must be **cheap and simple**, because writes wait behind it: it writes one file sequentially, with no reading and no merging. The price is that L0 tables **overlap**. Table 7 and table 9 can both hold `key000042`, so a read has to check them newest first. Merging overlapping tables into non-overlapping levels is **compaction** (milestone 7).

### Why a bloom filter?

Most lookups in a multi-table LSM are for keys that a given table doesn't hold. Either the key lives in another table, or it doesn't exist at all. Without a filter, each such lookup reads and checksums a 4 KiB block per table. With a 10-bits-per-key filter, 99% of those reads disappear, for 1.25 bytes per key of memory.

---

## 3. How ours works

### 3.1 A bloom filter in one paragraph

An array of `m` bits, all zero. To **add** a key, hash it to `k` bit positions and set them. To **query**, hash the same way and look at those `k` bits:

- any bit clear → the key was **never added** (certain);
- all bits set → **maybe**: either it was added, or other keys happened to set all `k` bits (a **false positive**).

There are **no false negatives**: an added key's bits stay set, because bits are only ever set. That's also why you **can't delete** from a bloom filter. Clearing a bit might clear it for another key. strata never needs to: tables are immutable, and a deleted key is a *tombstone entry*, so its user key **is** in the filter. A lookup passes the filter, finds the tombstone, and answers "deleted".

### 3.2 The maths: false-positive rate and the best `k`

After adding `n` keys to `m` bits with `k` hashes, a given bit is still zero with probability about `e^(−kn/m)`. A false positive needs all `k` probes to hit set bits:

```text
p ≈ (1 − e^(−k/b))^k        where b = m/n = bits per key
```

Too few probes and each key's fingerprint is weak; too many and the array fills up. The minimum is at

```text
k = b · ln 2        (≈ 0.69 · b)         giving  p ≈ 0.5^k ≈ 0.6185^b
```

At **b = 10**: k = 6.93 → 7 probes, p ≈ **0.82%**. Each extra bit per key cuts p by a factor of about 0.62, and each halving of p costs about 1.44 bits per key. `num_probes` computes `k` with exactly this formula, clamped to `1..=30`.

### 3.3 Hashing: one 64-bit hash makes all `k` probes

Computing `k` independent hashes per key would be slow. **Kirsch and Mitzenmacher** showed that `g_i = h1 + i·h2` for `i = 0..k` is as good as `k` independent hashes for a bloom filter. strata takes both halves from one 64-bit hash:

```rust
let h = hash64(key);                 // fmix64(fnv1a64(key))
let delta = (h >> 32) | 1;           // odd, so it never gets stuck on one bit
for i in 0..k { test bit (h + i·delta) mod m_bits }
```

`hash64` is FNV-1a, which is simple but mixes its high bits poorly for short keys, followed by MurmurHash3's 64-bit finalizer `fmix64`, which spreads every input bit over every output bit. Mutating the probe step to `h >> 63` (so all probes use only two positions) pushes the false-positive rate past the test's 2% limit. `false_positive_rate` catches it.

**The hash is part of the file format.** A filter written today is read by every later build. If the hash changed, old filters would answer "definitely not" for keys they contain: false negatives, which means wrong answers. So the plan fixes the hash with test vectors (`hash64(b"strata") == 0x5a4f965e8a9b0ac3`), and changing it would need a new format version.

### 3.4 Why the filter covers user keys, not internal keys

A table stores internal keys: `user_key ++ (seqno, kind)`. But `get(user_key)` doesn't know which seqnos exist; it asks "the newest version at or below my snapshot". It can only query the filter with what it has, the **user key**. So the builder hashes each distinct user key once. Versions of one key are adjacent in sorted order, so "distinct" just means "different from the previous entry's user key".

The mutation check: hash the encoded internal key instead. Every present key then misses the filter, and five tests fail, including `present_keys_still_found`.

### 3.5 The filter on disk, and the footer v2

```text
[data blocks…] [filter: bit array (m bytes) | k u8][crc] [index][crc] [footer: 56 bytes]
```

- `m = max(ceil(n · bits_per_key / 8), 8)`; 100 keys at 10 bits per key → 125 bytes + `k`.
- `bloom_bits_per_key = 0` writes no filter; the footer's filter handle is then `(0, 0)`.
- `Table::open` reads the filter, verifies its CRC, checks `k ∈ 1..=30` and `m > 0`, and keeps it in memory. `get` asks it first and returns `NotFound` without reading a block when it says no.

The **footer grew from 48 to 56 bytes**, to hold `max_seqno` (§3.9). That's format version 2, and version 1 tables are rejected. Chapter 03 had claimed reserving the filter handle would avoid a format change. That was true for the filter, but not for the field we didn't predict.

### 3.6 The flush, step by step

`flush_locked` runs on the writer's thread, holding the writer lock:

```text
            ┌─ 1. create 000010.log, sync_dir ───────────── new log's name is durable
            │
 writer ────┼─ 2. ROTATE  state = {mem: new, imm: old mem, tables}
 lock held  │              writer now appends to 000010.log
            │
            ├─ 3. write imm → 000011.sst.tmp, finish (sync)
            │              rename → 000011.sst, sync_dir ─── table is durable
            │
            ├─ 4. PUBLISH state = {mem, imm: none, tables: [000011] + tables}
            │
            └─ 5. remove 000008.log (the old log), sync_dir
```

Three rules make this safe:

1. **The table is durable before the log goes.** At every point, the data is in the old log, in the finished table, or in both.
2. **A table becomes live atomically.** It's written under a temporary name, and `rename` is atomic, so `open` never sees a half-written `.sst`. A leftover `.tmp` is simply deleted at open: nothing relied on it, because the log was still there.
3. **The new log's name is durable before any write goes into it.** Without step 1's `sync_dir`, a write could be synced into `000010.log` and acknowledged, then a crash could lose the *name*, and the write with it.

**Why `imm` exists.** Between rotation and publish, the old memtable's data is in neither `mem` nor `tables`. Keeping it as `imm` in the read state means readers never miss it. In milestone 8 the flush moves to a background thread. Then `imm` is also what lets writers continue into the new memtable while the old one is being written.

The trigger is in `write`: **before logging the batch**, if the memtable is non-empty and at least `memtable_size`, flush first. If the flush fails, the batch was never logged or applied, and the caller gets the error. Any flush failure **poisons** the database, as a failed log write does (chapter 02), but reads keep working, because `imm` stays in the read state until a table replaces it.

### 3.7 What a crash does at each step

| crash after… | on disk | next `open` |
|---|---|---|
| 1 (new log created) | old log (full), maybe an empty new log | replays the old log, flushes it |
| 3, mid-write | old log, maybe a `.tmp` | deletes the `.tmp`, replays the old log |
| 3, renamed but dir not synced | old log, and `.tmp` *or* `.sst` | either way: the `.sst` is complete, or the `.tmp` is deleted |
| 3, done | old log + durable table | replays the log but **skips** every entry (§3.8) |
| 5, removed but not synced | maybe the old log again | skips it again |

`crash_at_every_step_of_flush` checks exactly this. It writes 50 keys, kills the simulated process after `n` filesystem steps of a flush for every `n`, crashes, reopens, and requires all 50 keys, nothing else, and at most 50 entries across all tables (no entry written twice).

**The sync nobody noticed.** In `write_table`, the `sync_dir` right after the rename looks redundant: step 5 syncs the directory anyway. And with plain `SimFs::crash()` it *is* invisible. Deleting it passes every test, because `crash()` treats `sync_dir` as an atomic snapshot of all pending name changes, so step 5's sync makes the rename durable too.

But POSIX makes no promise about the *order* in which unsynced directory changes reach the disk. Without that sync, the old log's removal could become durable while the rename doesn't, and then both copies of the data are gone. Journaling filesystems usually do keep metadata changes in order, so this is rare in practice. But "usually" is not what a database promises.

So milestone 4 added `SimFs::crash_reordered(seed)`, a crash that keeps an arbitrary subset of unsynced directory changes. With it, the missing sync fails `crash_at_every_step_of_flush`. Without it, the test stays green. A crash simulator only finds the bugs its model of the disk can express.

### 3.8 Replay, and the skip invariant

At open, the old logs are replayed into a memtable, written to a table, and deleted. That replaces milestone 2's checkpoint, which copied the memtable into a fresh log. But consider the crash in row 4 above: the table is durable and the old log still exists. Replaying the log again would write the same entries into a **second** table. Reads would still be correct, but the duplicates would be on disk forever.

So replay **skips every entry with seqno ≤ `table_max`**, the largest `max_seqno` of any table. That's safe because of an invariant, written as a comment on `replay`:

> Every seqno ≤ `table_max` is in some table. A flush always writes the **whole** memtable, and a memtable holds every seqno since the previous flush, because rotation happens only under the writer lock, between batches.

So the newest table holds everything since the flush before it, that table everything before that, and so on down to seqno 1. Mutating the skip away fails both duplicate counts: `table_and_old_log_together_replay_same_state` and `crash_at_every_step_of_open`.

### 3.9 Why `max_seqno` lives in the footer

Once flushes delete logs, it's normal to reopen a database whose logs are **all empty**. Where does the sequence counter restart? If it restarted at 0, the next write would get seqno 1, *older* than versions already in tables, and reads would return the stale table version.

Each table therefore records its highest seqno in the footer, and open sets `last_seqno = max(table_max, highest seqno replayed)`. The test `newest_version_wins_across_tables` reopens once with nothing written in between, exactly to hit this case.

That reopen was added during this milestone. The test first reopened right after each write, so every open had something to replay, and the mutation "ignore `table_max`" slipped past it. Only the duplicate-count tests caught it.

### 3.10 The read path and the read state

Readers never take the writer lock. All the places to search form one immutable `ReadState { mem, imm, tables }`, held as `RwLock<Arc<ReadState>>`. A reader clones the `Arc` (a few nanoseconds under a read lock) and then searches with no lock held. A flush builds a new `ReadState` and swaps it in; readers holding the old one finish undisturbed.

```rust
let snapshot = visible_seqno.load(Acquire);   // FIRST
let state = state.read().clone();             // THEN
for source in [mem, imm, tables newest-first] {
    match source.get(key, snapshot) { Found(v) => return Some(v), Deleted => return None, NotFound => continue }
}
```

- **`Deleted` stops the search.** A tombstone in a newer source hides every older version. `tombstone_hides_flushed_value` pins this, including after the tombstone itself has been flushed into a newer table.
- **The order of the two loads matters.** If the state were cloned first, a flush could rotate in a new memtable, a writer could put seqno `s` into it and publish `s`, and the reader would then search the *old* state with snapshot `s`. The old state doesn't contain `s`. Loading the seqno first is safe, because data only moves from newer sources to older ones, and `imm` stays in the state until its table is published. `reader_during_flushes` hammers this: one thread reads 100 keys in a loop while another thread's writes cause dozens of flushes.
- **`scan` merges.** It collects each source's rows, then keeps the first one per user key (sources are newest first and hold disjoint seqno ranges). Tombstones take part in the merge and are dropped only afterwards. Otherwise an older table's value would reappear from under a newer delete.

### 3.11 L0 overlap and read amplification

Every flush adds a table, and nothing removes them yet. A lookup for a key that exists only in the oldest table checks `mem`, then every table's filter. With filters, each table that doesn't have it costs one in-memory filter probe, plus a block read 0.8% of the time. A **scan** has no filter to help: it reads every table. This growth of work per read with the number of files is **read amplification**. Compaction (milestone 7) bounds it by merging L0 into sorted, non-overlapping levels.

### 3.12 The directory listing as the source of truth

Which tables are live? For now, **every `{n}.sst` in the directory**. That works because a flush adds exactly one file, and `rename` makes that atomic. Compaction will need to replace five tables with three as **one** atomic step, and a directory can't do that: there's no "rename five files at once". That's why milestone 5 introduces a **manifest**, a log of "these files are live" edits.

---

## 4. How we know it works

- **The bloom filter:**
  - fixed test vectors for the hash;
  - `no_false_negatives` (proptest, 0–500 random keys, 1–20 bits per key);
  - `false_positive_rate`: under 2% on 10,000 fixed keys (theory 0.82%);
  - exact layout sizes;
  - corrupt contents rejected, and arbitrary bytes never panic.
- **The table with a filter:**
  - `filter_skips_missing_keys`: 1,000 absent probes inside the key range read fewer than 50 data blocks;
  - `no_filter_reads_blocks`: without a filter, every one of them reads a block;
  - `table_matches_memtable` now also checks `scan` against the memtable at random bounds and snapshots, and checks `max_seqno`;
  - `every_byte_flip_is_detected` covers the filter block's CRC too (6,835 positions).
- **Recovery and flush** (`tests/flush.rs`):
  - a crash at every step of `open` and of a flush, both as a plain crash and reordered, with exact contents and duplicate counts;
  - tombstones over flushed values;
  - sequence numbers continuing above tables;
  - leftover temp files removed;
  - version 1 tables rejected;
  - a failed flush poisons, but reads keep working;
  - a write whose flush fails is never applied, even after a crash;
  - a reader running during dozens of flushes.
- **The model test** now uses a 256-byte memtable (a flush every dozen writes), an explicit `Flush` op, and reordered crashes. Random sequences of operations must match a `BTreeMap` throughout.
- **Mutation checks**: each breakage was made on purpose, then reverted.

  | mutation | caught by |
  |---|---|
  | filter over encoded internal keys | `present_keys_still_found`, `get_basic`, `table_matches_memtable` and two more |
  | probe step `h >> 63` | `false_positive_rate` |
  | `get` treats a memtable `Deleted` as `NotFound` | `tombstone_hides_flushed_value` |
  | tables searched oldest first | `newest_version_wins_across_tables`, `tombstone_hides_flushed_value` |
  | replay doesn't skip seqnos ≤ `table_max` | `table_and_old_log_together_replay_same_state`, `crash_at_every_step_of_open` |
  | `last_seqno` ignores `table_max` | `newest_version_wins_across_tables` (after its fix, §3.9) |
  | no `sync_dir` after the rename | `crash_at_every_step_of_flush`, **only with `crash_reordered`** |
  | delete the old log before writing the table | `crash_at_every_step_of_flush`, `failed_flush_poisons` |
  | flush *after* logging the triggering batch | `write_that_triggers_failed_flush_is_not_applied` |

---

## 5. Alternatives we rejected, and why

- **Keep the checkpoint-to-a-new-log from milestone 2.** It rewrites the whole recovered state at every open and never bounds the log. A flush writes it once, into a format reads can use.
- **One filter per data block** (LevelDB's original design). The filter block is split per 2 KiB of data. That gives smaller filter reads, but more bookkeeping and the same false-positive rate. With one in-memory filter per table, a negative answer costs no I/O at all.
- **A counting bloom filter or a cuckoo filter**, which support deletion. We never delete from a table's key set; tables are immutable.
- **Flush without `imm`** (hold every reader while the table is written). That's simpler, but reads would stall for the whole flush.
- **Skipping the temp name** (write `{n}.sst` directly). A crash mid-write would leave a truncated `.sst` that `open` treats as live, and then fail on a bad footer.

**Deviations from the original design**, each fixed by a later milestone:

| deviation | why for now | fixed in |
|---|---|---|
| The flush runs on the writer's thread; a write that triggers it waits | simplest correct version; makes every crash point testable | M8 (background flush, with group commit) |
| `scan` collects every source's results into memory, then merges | correct and simple | M6 (streaming merging iterators) |
| L0 grows without bound; every read checks every table, and each table is an open file (on macOS the default shell limit is 256 descriptors: check with `ulimit -n`) | no compaction yet | M7 (compaction) |
| The directory listing decides which tables are live | enough for adding one file at a time | M5 (manifest) |
| The footer grew from 48 to 56 bytes; format version 2 | `max_seqno` had nowhere else to go | (permanent) |

---

## 6. How real systems do it

- **LevelDB** flushes on a background thread. It keeps one `imm`; a writer that finds `imm` still busy waits for it. Which files are live is recorded in the `MANIFEST`, and the sequence counter is stored there too, instead of in each table's footer.
- **RocksDB** allows several immutable memtables queued for flush (`max_write_buffer_number`), uses whole-table ("full") filters by default, and offers **partitioned filters** for huge tables, so that only the part of the filter you need is in memory. Newer versions also offer **ribbon filters**: about 30% smaller than bloom filters at the same false-positive rate, at more CPU to build.
- **Filters per level** are tunable in RocksDB. The last level holds most of the data, and most lookups that reach it are for keys that *do* exist, so `optimize_filters_for_hits` skips that level's filters entirely and saves most of the filter memory.
- **fsync of the directory** after a rename or create is standard practice in every serious engine. SQLite, PostgreSQL and RocksDB all do it, because POSIX doesn't promise that metadata reaches the disk in order.

---

## 7. Try it yourself

1. **Run the experiment.**
   ```bash
   cargo run --release --example bloom_demo
   ```
   On the same Mac as chapter 03 (100,000 keys, 100,000 absent probes, all inside the key range):

   | bits/key | filter bytes | k | measured FP | theory | blocks per probe | ns per probe |
   |---|---|---|---|---|---|---|
   | 0 | 0 | – | 100% | – | 1.000 | 3,298 |
   | 2 | 25,005 | 1 | 39.41% | 39.35% | 0.394 | 1,663 |
   | 5 | 62,505 | 3 | 9.21% | 9.18% | 0.092 | 462 |
   | 10 | 125,005 | 7 | 0.83% | 0.82% | 0.008 | 131 |
   | 20 | 250,005 | 14 | 0.00% | 0.01% | 0.000 | 78 |

   The measured rates match the formula to within a few hundredths of a percent. A probe that the filter rejects costs about 80–130 ns. One that reads a block costs about 3.3 µs, 25–40 times more. Timings vary from run to run; the rates and block counts don't. Part 2 prints the directory as 20,000 writes flush a 64 KiB memtable 17 times: one log at a time, each table a new L0 file.
2. **Put the filter on the wrong key.** In `builder.rs`, hash `key` (the encoded internal key) instead of `parsed.user_key`. Run `cargo test --lib sstable::` and explain why *present* keys now go missing.
3. **Remove the sync nobody noticed.** Delete the `sync_dir` after the rename in `src/db/flush.rs`. Run `cargo test --test flush`. Then change `crash_at_every_step_of_flush` to use only `fs.crash()`, and run it again. Explain the difference.
4. **Break the read order.** In `read_view`, clone the state *before* loading `visible_seqno`. Can you make `reader_during_flushes` fail? (The race window is tiny. Think about why a test that passes is not proof here, and what the comment on `read_view` argues instead.)
5. **Count L0.** Run part 2 of the experiment with a 4 KiB memtable. How many tables does it make, and how many filter probes does a `get` for the very first key now cost?

---

## Check yourself

- Why can a bloom filter have false positives but not false negatives? Why can't you delete from one, and why doesn't strata need to?
- Derive the best `k` for `b` bits per key. What false-positive rate do 10 bits per key give?
- Why does the filter cover user keys and not internal keys?
- Walk through the five steps of a flush. For each, what's on disk if the power fails right after it?
- Why is a table written under a temporary name and renamed? Why sync the directory after the rename?
- What is the replay skip invariant, and what would go wrong without it?
- Why does each table's footer record `max_seqno`?
- Why must a reader load `visible_seqno` before cloning the read state?
- Why does `scan` drop tombstones only *after* merging the sources?

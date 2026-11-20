# strata Milestone 4: Bloom filters and flushing to L0. Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make strata's memory use bounded and its data live on disk. When the memtable is full it is **flushed** into an SSTable in level 0 (L0), its log is deleted, and reads search memtable → immutable memtable → L0 tables, newest first. Each table carries a **bloom filter** over its user keys, so a lookup for a key a table doesn't hold usually skips reading that table's data blocks entirely.

**Architecture:**
- **Read state.** One `Arc<ReadState { mem, imm, tables }>` behind an `RwLock`. A reader takes a clone of the `Arc` and searches it without holding any lock. Each change (rotate the memtable, publish a table) swaps in a whole new `ReadState`.
- **Flush happens on the writer's thread.** Before logging a batch, the write path checks whether the memtable is full. If it is, the write path flushes first:
  1. Rotate to a new log and memtable. The old memtable becomes `imm`.
  2. Write `imm` to `{n:06}.sst.tmp`, sync it, rename it to `{n:06}.sst`, and sync the directory.
  3. Publish the table.
  4. Delete the old log.

  Readers keep reading throughout. A background flush thread is milestone 8, together with group commit, so all the concurrency work lands in one milestone.
- **Which tables are live.** Until the manifest (M5), the directory listing decides: every `{n:06}.sst` is live, and leftover `*.tmp` files are deleted at `open`. Rename is atomic, so a half-written table is never mistaken for a live one.
- **Table format v2.** The footer gains `max_seqno`: the highest sequence number in the table. `open` can then restore the sequence counter even when every log is gone. Separately, the footer's filter handle now points to a real bloom filter block.
- **Recovery.** `open` does the following:
  1. Open every table and take `table_max = max(max_seqno)`.
  2. Replay the logs, **skipping entries with seqno ≤ `table_max`**. Those entries are already in a table.
  3. If anything was replayed, flush it to a table. This replaces M2's checkpoint.
  4. Start a fresh, empty log and delete the old ones.

**Tech Stack:** Rust ≥ 1.89, `crc32fast` 1, `parking_lot` 0.12. Dev: `proptest` 1, `tempfile` 3. No new dependencies.

**Spec:** `docs/specs/2026-11-18-lsm-engine-design.md`. Task 1 amends it with this milestone's additions and the deviations below.

## Global Constraints

- **File names.** All files share one number space: next = max(number of every `*.log`, `*.sst`, `*.sst.tmp` in the directory) + 1.
  - Logs are `{n:06}.log`.
  - Tables are `{n:06}.sst`.
  - A table is written as `{n:06}.sst.tmp` first.
  - `LOCK` is unchanged.
- **Making a table live** follows exactly this order:
  1. build `{n}.sst.tmp`
  2. `finish` (which syncs the file)
  3. `rename` it to `{n}.sst`
  4. `sync_dir`

  Only after that may the table be published to readers, and only after that may the log it came from be removed (followed by another `sync_dir`).
- **No empty tables.** A table is never written from an empty memtable.
- **Bloom filter.**
  - It covers **user keys**. Consecutive duplicate user keys are added once.
  - **Hash:** `hash64(key) = fmix64(fnv1a64(key))`.
    - FNV-1a 64: offset basis `0xcbf29ce484222325`, prime `0x100000001b3`.
    - `fmix64` is MurmurHash3's finalizer: `k ^= k>>33; k *= 0xff51afd7ed558ccd; k ^= k>>33; k *= 0xc4ceb9fe1a85ec53; k ^= k>>33`.
    - Test vectors: `hash64(b"") = 0xefd01f60ba992926`, `hash64(b"a") = 0x82a2a958a9bece5b`, `hash64(b"strata") = 0x5a4f965e8a9b0ac3`.
  - **Probes (double hashing):** `h = hash64(key)`, `delta = (h >> 32) | 1`. For `i in 0..k`, test bit `(h.wrapping_add(i * delta)) % m_bits`, using wrapping `u64` arithmetic.
  - **Number of probes:** `k = clamp(round(bits_per_key × ln 2), 1, 30)`. At 10 bits per key, k = 7.
  - **Filter block layout:** `bit array (m bytes) | k (u8)`.
    - `m = max(ceil(n × bits_per_key / 8), 8)`.
    - Bit `b` lives in byte `b / 8`, mask `1 << (b % 8)`, so `m_bits = 8m`.
    - The block is stored with the usual 4-byte CRC trailer.
    - When reading, `k` outside `1..=30` or `m == 0` is `Corruption`.
  - `bloom_bits_per_key = 0` means no filter: the footer's filter handle is `(0, 0)`.
- **Footer v2.** Exactly 56 bytes, little-endian:
  ```text
  index_offset u64 | index_size u64 | filter_offset u64 | filter_size u64 | max_seqno u64 | version u32 | crc32 u32 | magic u64
  ```
  - The CRC covers bytes 0..44.
  - `FORMAT_VERSION = 2`. Version 1 files are `Corruption` ("unsupported format version").
  - The magic is unchanged (`strataT1`).
  - The filter handle is `(0, 0)` or a block lying inside the data region, which `read_block` checks.
  - The filter block is read and its CRC verified in `Table::open`.
- **Read order.**
  - A reader loads `visible_seqno` (Acquire) **first**, then clones the read state.
  - `get` searches mem → imm → tables newest-first. It stops at the first `Found` or `Deleted`, and only `NotFound` continues.
  - `scan` merges every source, newest wins per user key, and drops tombstones **after** merging.
- **Replay skip invariant (write it in the code).** Every seqno ≤ `table_max` is in some table. This holds because a flush always writes the *whole* memtable, and a memtable holds every seqno since the previous flush. Rotation happens only under the writer lock, between batches. Therefore replay may skip any entry with seqno ≤ `table_max`.
- **Seqno at open:** `last_seqno = visible_seqno = max(table_max, highest seqno replayed)`.
- **Failures poison.**
  - Any failure while flushing or rotating poisons the writer. The batch that triggered it is **not** logged or applied, and `write` returns the error.
  - Reads keep working: `imm` stays in the read state until its table is published.
- Carried over from earlier milestones:
  - `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test --all-targets` pass before every commit, chained with `&&`.
  - Decoders never panic.
  - Every key comparison on internal keys uses `compare_encoded`.
  - No time estimates. No co-author trailer on commits.
  - The standard chapter sections and interview format.

## Review Focus

1. **A crash at every filesystem step of a flush, and of `open`.** All acknowledged writes are present, nothing unwritten appears, and no table holds duplicate entries (counted by opening the `.sst` files directly). Pinned in Task 4 for `open` and in Task 5 for a write-triggered flush.
2. **A tombstone in a newer source hides a value in an older table.** Put, flush, delete, then `get` and `scan` return nothing. The same holds after the tombstone itself is flushed, and after a reopen. Pinned in Task 4 and Task 5.
3. **Sequence numbers continue above flushed data when no log has entries.** Two versions flushed, reopen, `get` gives the newest; a new write is newer than both, including after another reopen. Pinned in Task 4.
4. **A reader running during repeated flushes** never misses a key that was written before it started. Pinned in Task 5.
5. **Bloom filters:** no false negatives (proptest), a false-positive rate under 2% at 10 bits per key on fixed keys, and a lookup for a missing key reads zero data blocks most of the time. Pinned in Task 2 and Task 3.

## Deviations from the spec (tell the user; recorded in Task 1)

- **Flush runs on the writer's thread.** A write that triggers it waits for it. The background flush thread arrives in M8 with group commit.
- **`scan` collects the results from every source into memory** before returning them. Streaming merging iterators are M6.
- **L0 grows without bound.** Every flush adds a table and reads check all of them. Compaction is M7.
- **The directory listing is the source of truth for live tables** until the manifest in M5. It works for adding one file at a time. It can't express "replace these five files with these three" atomically, which compaction needs. That's the motivation for M5.
- **The table footer grows from 48 to 56 bytes, and the format version goes from 1 to 2.** Chapter 03 said M4 wouldn't need a new format. That was true for the filter, but storing `max_seqno` does need one, and the chapter gets a correction.

## Not in this milestone

Background threads, group commit, manifest/`CURRENT`, compaction, a block cache, snapshots in the public API, streaming iterators.

---

### Task 1: `FileSystem::rename`, spec amendment

**Files:**
- Modify: `src/fs/mod.rs`, `src/fs/os.rs`, `src/fs/sim.rs`, `docs/specs/2026-11-18-lsm-engine-design.md`

**Interfaces:**
- Produces, as a new `FileSystem` method:
  ```rust
  /// Atomically rename `from` to `to`, replacing `to` if it exists. Durable only after `sync_dir`.
  fn rename(&self, from: &Path, to: &Path) -> Result<()>;
  ```

- [ ] **Step 1: Write the failing tests.**
  - In `sim.rs`:
    - **`rename_moves_contents`.** After the rename, `read(to)` returns the old contents, `read(from)` is `Err`, and `list` shows only the new name.
    - **`unsynced_rename_undone_by_crash`.** Create, sync, `sync_dir`, rename, crash. The old name is back with its contents, and the new name is absent.
    - **`synced_rename_survives_crash`.** Same, with `sync_dir` after the rename: only the new name exists.
    - **`rename_replaces_target`.** Renaming onto an existing file leaves the source's contents under the target name.
    - **`rename_counts_for_kill_after`.** With `kill_after(0)`, `rename` returns `Err`.
    - **`rename_missing_is_err`.**
  - In `os.rs`: **`os_rename`**, a round trip on a tempdir.
- [ ] **Step 2: Run them.** Run `cargo test --lib fs::`. Expected: compile failure.
- [ ] **Step 3: Implement.**
  - **`OsFs`:** `std::fs::rename`.
  - **`SimFs`:** inside `mutate()`, the source must be live.
    1. Copy the `SimFile` (data and `synced_len`) to `to`.
    2. Update the live name sets: remove `from`, insert `to`.
    3. Keep `from`'s contents in `files`, so a crash that restores the durable names can bring it back. That's the same approach `remove` already uses.
  - Document in `sim.rs` that renaming a file that still has an open writer isn't modelled. strata only renames finished tables.
- [ ] **Step 4: Run them again.** Run `cargo test --lib fs::`. Expected: PASS.
- [ ] **Step 5: Amend the spec.** Add a "Milestone 4 additions" section with:
  - the file naming and the temp-then-rename rule
  - the footer v2 layout and `max_seqno`
  - the bloom hash and layout
  - the replay skip invariant
  - the read order
  - the five deviations above, verbatim
- [ ] **Step 6: Commit.**
  ```bash
  cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets && \
  git add -A && git commit -m "feat(fs): atomic rename, durable after sync_dir"
  ```

---

### Task 2: Bloom filter

**Files:**
- Create: `src/sstable/bloom.rs`
- Modify: `src/sstable/mod.rs`

**Interfaces:**
- Produces in `crate::sstable::bloom`, all `pub(crate)`:
  ```rust
  fn hash64(key: &[u8]) -> u64;
  fn num_probes(bits_per_key: usize) -> u32;            // clamp(round(bpk * ln2), 1, 30)
  /// Filter block contents (bits ++ k) for keys with these hashes.
  fn build(hashes: &[u64], bits_per_key: usize) -> Vec<u8>;
  struct BloomFilter { .. }
  impl BloomFilter {
      fn new(contents: Vec<u8>) -> Result<BloomFilter>;  // validates k and length
      fn may_contain(&self, key: &[u8]) -> bool;
  }
  ```

- [ ] **Step 1: Write the failing tests in `bloom.rs`.**
  - **`hash_test_vectors`.** The three vectors from Global Constraints.
  - **`probes`.** `num_probes` gives 10 → 7, 1 → 1, 0 → 1, 20 → 14, and 100 → 30.
  - **`no_false_negatives`** (proptest). For 0–500 random keys and `bits_per_key` in 1..=20, every inserted key satisfies `may_contain`.
  - **`false_positive_rate`.** Insert 10,000 keys `key{i}` at 10 bits per key and probe 10,000 keys `miss{i}`. The rate must be < 2%; theory says 0.82%.
  - **`layout`.** 100 keys at 10 bits per key give `m = 125` bytes plus 1 byte, so `len == 126` and the last byte is 7. With 0 keys, `len == 9` (the 8-byte minimum plus k).
  - **`rejects_bad_contents`.** Each must be `Corruption`: an empty buffer, `[k]` alone (`m == 0`), `k == 0`, and `k == 31`.
  - **`arbitrary_bytes_never_panic`** (proptest).
- [ ] **Step 2: Run them.** Run `cargo test --lib sstable::bloom`. Expected: compile failure.
- [ ] **Step 3: Implement** exactly as Global Constraints specify. The probe arithmetic is the part the format depends on:
  ```rust
  let (mut g, delta) = (h, (h >> 32) | 1);
  for _ in 0..k { let bit = g % m_bits; /* set or test */ g = g.wrapping_add(delta); }
  ```
- [ ] **Step 4: Run the tests.** Run `cargo test --lib sstable::bloom`. Expected: PASS.
- [ ] **Step 5: Commit.**
  ```bash
  cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets && \
  git add -A && git commit -m "feat(sstable): bloom filter over user keys"
  ```

---

### Task 3: Table format v2: filter block, `max_seqno`

**Files:**
- Modify: `src/sstable/format.rs`, `src/sstable/builder.rs`, `src/sstable/table.rs`, `src/sstable/tests.rs`, `examples/sst_layout.rs` (`TableOptions` gains a field), `docs/learn/03-sstables.md` (the footer section and a "corrected in M4" note), `docs/specs/...` (the footer text)

**Interfaces:**
- Consumes: `bloom::{hash64, build, BloomFilter}`.
- Produces:
  ```rust
  // format.rs
  const FOOTER_LEN: usize = 56; const FORMAT_VERSION: u32 = 2;
  struct Footer { index: BlockHandle, filter: BlockHandle, max_seqno: SeqNo }
  // builder.rs (public)
  pub struct TableOptions { pub block_size: usize, pub restart_interval: usize, pub bloom_bits_per_key: usize } // default 4096, 16, 10
  pub struct TableMeta { /* M3 fields */ pub max_seqno: SeqNo }                // 0 for an empty table
  // table.rs
  impl Table {
      pub fn max_seqno(&self) -> SeqNo;
      pub fn has_filter(&self) -> bool;
      /// Data blocks read from the file since open (for tests and the experiment).
      pub fn data_block_reads(&self) -> u64;
      /// Like Memtable::scan: newest visible version per user key in range; None = tombstone.
      pub(crate) fn scan(&self, start: Bound<&[u8]>, end: Bound<&[u8]>, snapshot: SeqNo)
          -> Result<Vec<(Vec<u8>, Option<Vec<u8>>)>>;
  }
  ```
  `get` gains a first step: if the table has a filter and `!may_contain(user_key)`, return `NotFound` without reading a data block.

- [ ] **Step 1: Update and add the failing tests.**
  - In `format.rs`:
    - The footer is 56 bytes, the CRC covers 0..44, and the magic sits at 48..56.
    - `max_seqno` round-trips.
    - A version-1 footer is rejected.
    - The byte-flip loop covers 0..48.
    - A nonzero filter handle is now **accepted**; remove the M3 rejection test.
  - In `builder.rs`:
    - `empty_table_layout` with `bloom_bits_per_key: 0` gives `8 + 4 + 56`.
    - With the default options, the size is `+ (9 + 4)` for the filter block.
    - **`meta_max_seqno`.** Entries with seqnos 5, 9 and 3 (on different user keys) give `max_seqno == 9`.
  - In `table.rs`:
    - **`filter_skips_missing_keys`.** Build 1000 keys at 10 bits per key. Probing 1000 absent keys must increase `data_block_reads` by fewer than 50.
    - **`present_keys_still_found`** with the filter on.
    - **`no_filter_reads_blocks`.** With `bloom_bits_per_key: 0`, `has_filter()` is false and every absent probe that falls inside the key range reads a block.
    - **`scan_ranges`.** Bounds `Included`, `Excluded` and `Unbounded`, a reversed range (empty), snapshot filtering, and tombstones returned as `None`.
  - In `tests.rs`:
    - `table_matches_memtable` also checks `table.scan` against `memtable.scan` for random bounds and snapshots, and checks `max_seqno` against the highest seqno inserted.
    - `every_byte_flip_is_detected` builds **with** the default filter, so the filter block's CRC is covered.
- [ ] **Step 2: Run them.** Run `cargo test --lib sstable::`. Expected: compile failure, or the new tests fail.
- [ ] **Step 3: Implement.**
  - **Builder:** collect `hash64(user_key)` once per new user key. At `finish`, if `bloom_bits_per_key > 0`, write the filter block, then the index, then the footer. Track `max_seqno`.
  - **`Table::open`:** if the filter handle isn't `(0, 0)`, call `read_block`, check the trailer, and run `BloomFilter::new`.
  - **`scan`:** seek to `(start, MAX_SEQNO, Put)` for `Included`. For `Excluded`, seek to `(start, 0, Delete)` and skip entries whose user key equals `start`. Stop at the end bound, and keep the first visible version per user key.
  - **`data_block_reads`:** an `AtomicU64` bumped in `read_block` calls made for data blocks.
  - **Docs:** update the M3 chapter's footer text with a short "Changed in milestone 4" note, and update the experiment's struct literal.
- [ ] **Step 4: Run the tests.** Run `cargo test --lib sstable::`. Expected: PASS.
- [ ] **Step 5: Mutation check.** Build the filter over the **encoded internal key** instead of the user key. Expected: `present_keys_still_found` or `table_matches_memtable` fails. Revert from a scratch copy and record the result in the ledger.
- [ ] **Step 6: Commit.**
  ```bash
  cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets && \
  git add -A && git commit -m "feat(sstable): format v2 with bloom filter block and max seqno"
  ```

---

### Task 4: Read state across tables, and recovery that flushes

**Files:**
- Move: `src/db.rs` → `src/db/mod.rs` (public API, write path, read path)
- Create: `src/db/files.rs` (names and parsing), `src/db/flush.rs` (`write_table`), `src/db/recovery.rs` (open procedure: `replay` and friends move here)
- Modify: `src/options.rs`, `src/lib.rs` (re-export `TableOptions`), `tests/recovery.rs`
- Create: `tests/flush.rs`

**Interfaces:**
- Consumes: `TableBuilder`, `Table`, `TableOptions`, `FileSystem::rename`.
- Produces:
  ```rust
  // options.rs
  pub struct Options { /* existing */ pub table: TableOptions }      // default TableOptions::default()
  // db/files.rs (pub(crate))
  enum FileKind { Log, Table, TempTable }
  fn parse_file_name(name: &str) -> Option<(u64, FileKind)>;          // "000007.log", "000007.sst", "000007.sst.tmp"
  fn log_name(n: u64) -> String; fn table_name(n: u64) -> String; fn temp_table_name(n: u64) -> String;
  // db/flush.rs (pub(crate))
  /// Build a table from a non-empty memtable; tmp -> finish -> rename -> sync_dir; returns the open table.
  fn write_table(fs: &dyn FileSystem, dir: &Path, number: u64, mem: &Memtable, opts: &TableOptions) -> Result<Arc<Table>>;
  // db/mod.rs (crate-internal)
  struct ReadState { mem: Arc<Memtable>, imm: Option<Arc<Memtable>>, tables: Vec<Arc<Table>> /* newest first */ }
  // DbInner gains: fs, dir, state: RwLock<Arc<ReadState>>; Writer gains: log_number, next_file_number
  ```

- [ ] **Step 1: Write the failing tests** in `tests/flush.rs`. These use the public API, `SimFs`, and `strata::sstable::Table` to inspect files. Until Task 5 the only flush is the one at `open`, so a reopen flushes:
  - **`reopen_moves_data_into_a_table`.** Write 100 keys, close, reopen.
    - The directory holds exactly one `.sst` and one `.log`, and that log replays to nothing.
    - All 100 keys read back.
    - Reopening again adds no new table.
  - **`tombstone_hides_flushed_value`.** Put `k`, reopen so it's in a table, delete `k`.
    - `get(k)` is `None`, and `scan` doesn't show `k`.
    - After another reopen (the tombstone is now in a newer table), the same holds.
  - **`newest_version_wins_across_tables`.**
    - Put `k=1`, reopen, put `k=2`, reopen. `get` returns 2.
    - Put `k=3`, reopen. `get` returns 3, which shows sequence numbers continued above the tables.
  - **`leftover_temp_table_is_removed`.** Plant `000099.sst.tmp` with garbage, then open. The open succeeds, the file is gone, and the next file number is above 99.
  - **`version_1_table_is_rejected`.** Plant a `.sst` with a valid v1-style footer. `open` returns `Corruption`.
  - **`crash_at_every_step_of_open`** (moved from `tests/recovery.rs` and extended). For n = 0, 1, 2, …:
    1. Write acknowledged data and close.
    2. Set `kill_after(n)`, open (it may fail), crash, then reopen normally.
    3. All acknowledged data is present.
    4. The total number of entries across all `.sst` files, counted through `Table::iter`, is ≤ the number of writes, so there are no duplicates.

    Stop once an open succeeds within the budget.
  - Update the checkpoint-era tests in `tests/recovery.rs`. `old_and_new_logs_together_replay_same_state` becomes **`table_and_old_log_together_replay_same_state`**: crash right after the table is durable but before the log is removed, then reopen. The data is equal and there are no duplicates. Keep every other recovery test passing unchanged.
- [ ] **Step 2: Run them.** Run `cargo test --test flush --test recovery`. Expected: FAIL. No `.sst` files are produced yet.
- [ ] **Step 3: Implement.**
  - **`Options.table`.**
  - **`files.rs`.**
  - **`write_table`.** It never gets called with an empty memtable; use `debug_assert` for that.
  - **Read path:** `get` and `scan` across `ReadState` per Global Constraints.
  - **Recovery:**
    1. lock
    2. list, and remove `*.sst.tmp`
    3. open the tables, newest first
    4. compute `table_max`
    5. replay the logs, skipping seqnos ≤ `table_max`, with the invariant written as a comment
    6. if the memtable isn't empty, call `write_table` with the next number
    7. create a new log and `sync_dir`
    8. remove the old logs, then `sync_dir`
    9. `last_seqno = max(table_max, replayed max)`
  - **Cleanup:** delete `checkpoint`. Fix the stale `MAX_KEY_LEN` comment (it mentions `u16`). Update the module docs.
- [ ] **Step 4: Run the tests.** Run `cargo test --all-targets`. Expected: PASS, including `tests/model.rs`, whose `Reopen` and `Crash` ops now go through tables.
- [ ] **Step 5: Mutation checks.** Revert each from a scratch copy and record each result in the ledger:
  - (a) `get` treats `Deleted` from the memtable as `NotFound` and keeps searching. Expected: `tombstone_hides_flushed_value` fails.
  - (b) Tables are searched oldest first. Expected: `newest_version_wins_across_tables` fails.
  - (c) Don't skip seqnos ≤ `table_max` during replay. Expected: the duplicate count in `table_and_old_log_together_replay_same_state` or `crash_at_every_step_of_open` fails.
  - (d) `last_seqno` ignores `table_max`. Expected: `newest_version_wins_across_tables` fails.
- [ ] **Step 6: Commit.**
  ```bash
  cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets && \
  git add -A && git commit -m "feat(db): read across L0 tables; recovery flushes to a table"
  ```

---

### Task 5: Flush on the write path

**Files:**
- Modify: `src/db/mod.rs`, `src/db/flush.rs`, `tests/flush.rs`, `tests/model.rs`

**Interfaces:**
- Consumes: `write_table`, `ReadState`.
- Produces:
  ```rust
  impl Db {
      /// Flush the memtable to an L0 table now (no-op if it is empty).
      pub fn flush(&self) -> Result<()>;
  }
  // db/flush.rs: the steps below, called with the writer lock held
  fn flush_locked(inner: &DbInner, w: &mut Writer) -> Result<()>;
  ```

- [ ] **Step 1: Write the failing tests.**
  - In `tests/flush.rs`:
    - **`writes_trigger_flushes`.** With `memtable_size: 4096`, write 2000 keys.
      - There is more than one `.sst` and exactly one `.log`.
      - Every key reads back, before a reopen and after one.
    - **`explicit_flush`.**
      - `flush()` on an empty memtable creates no file.
      - After one put, `flush()` creates one `.sst`, and `get` still works.
    - **`tombstone_flushed_then_reopen`.** Put, flush, delete, flush, reopen. `get` is `None`, and `scan` is empty.
    - **`crash_at_every_step_of_flush`.** For n = 0, 1, 2, …:
      1. Use a fresh `SimFs`. Write 50 acknowledged keys with `SyncPolicy::Always`.
      2. Set `kill_after(n)` and call `flush()`.
      3. Crash and reopen.
      4. Every acknowledged key is present with its value, nothing extra appears, and the entry count across all `.sst` files is ≤ 50.

      Stop at the first n where `flush()` returns `Ok`.
    - **`failed_flush_poisons`.** `fail_next_sync()`, then `flush()` returns `Err`.
      - Then `put` returns `Poisoned`, and `get` of existing keys still works.
      - After crash and reopen, all keys acknowledged before the flush are present.
    - **`write_that_triggers_failed_flush_is_not_applied`.** Fill the memtable to its size, then `fail_next_sync()` and `put(k, v)`, which returns `Err`. `get(k)` is `None`, and still `None` after crash and reopen.
    - **`reader_during_flushes`.** A reader thread loops over `get` for 100 pre-written keys while the main thread writes 5000 more keys with `memtable_size: 4096`, causing many flushes. The reader never sees `None` for a pre-written key.
  - In `tests/model.rs`:
    - Add `Op::Flush`, weight 1.
    - Run with `memtable_size: 256` so the write path flushes often.
    - Keep `Reopen` and `Crash`.
- [ ] **Step 2: Run them.** Run `cargo test --test flush --test model`. Expected: compile failure, because `Db::flush` doesn't exist yet.
- [ ] **Step 3: Implement `flush_locked`.**
  1. Allocate a log number, create the log, and `sync_dir`, so the new log's name is durable before any write lands in it.
  2. Swap the state to `{mem: new, imm: Some(old mem), tables}`, and swap the writer's `wal`.
  3. Allocate a table number and call `write_table` with `imm`.
  4. Swap the state to `{mem, imm: None, tables: [new] + tables}`.
  5. Remove the old log, then `sync_dir`.

  Any error poisons the writer and is returned.
  - **In `write`:** after the closed and poisoned checks and before logging, if `mem.approximate_size() >= options.memtable_size`, call `flush_locked`, so the triggering batch is never logged if the flush fails.
  - **`Db::flush`:** lock, check open and poisoned, no-op if the memtable is empty, otherwise `flush_locked`.
  - Remove the `#[allow(dead_code)]` on `approximate_size`.
- [ ] **Step 4: Run the tests.** Run `cargo test --all-targets`. Expected: PASS.
- [ ] **Step 5: Mutation checks.** Revert each from a scratch copy and record each result in the ledger:
  - (a) Skip the `sync_dir` after the rename in `write_table`. Expected: `crash_at_every_step_of_flush` fails.
  - (b) Remove the old log **before** writing the table. Expected: `crash_at_every_step_of_flush` fails, because acknowledged writes are lost.
  - (c) Flush *after* logging the triggering batch. Expected: `write_that_triggers_failed_flush_is_not_applied` fails.
- [ ] **Step 6: Commit.**
  ```bash
  cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets && \
  git add -A && git commit -m "feat(db): flush full memtables to L0 on the write path"
  ```

---

### Task 6: Experiment, chapter 04, interview notes, README

**Files:**
- Create: `examples/bloom_demo.rs`, `docs/learn/04-bloom-and-flush.md`, `docs/interview/04-bloom-and-flush.md`
- Modify: `README.md`

- [ ] **Step 1: Write `examples/bloom_demo.rs`.**
  - **Filter rates.** Build a table of 100,000 keys at `bloom_bits_per_key` 0, 2, 5, 10 and 20. Probe 100,000 keys that fall inside the key range but are absent. For each setting, print:
    - the filter size in bytes and bits per key
    - k
    - the measured false-positive rate against the theoretical `(1 - e^(-k/b))^k`
    - data blocks read per probe
    - ns per probe
  - **Flush lifecycle.** Open a `Db` on a tempdir with `memtable_size` 64 KiB, write 20,000 keys, and print the directory listing after each flush.
  - **Run:** `cargo run --release --example bloom_demo`. Expected: the measured false-positive rate is close to the theoretical one, and blocks read per probe falls as the bits per key grow.
- [ ] **Step 2: Write chapter 04.** Use the standard sections. Cover:
  - **Bloom basics:** what a bloom filter is, and that it gives no false negatives.
  - **The maths:** the false-positive formula, the optimal k = b·ln 2, and the ~1% at 10 bits per key figure.
  - **Hashing:** double hashing (Kirsch–Mitzenmacher), and why the hash is part of the file format.
  - **User keys:** why the filter covers user keys rather than internal keys.
  - **The flush lifecycle**, as a diagram: memtable, immutable memtable, temporary table, rename, publish, delete log.
  - **Crash ordering:** what happens at each crash point.
  - **Replay:** the replay skip invariant.
  - **`max_seqno`:** why it sits in the footer.
  - **L0 overlap and read amplification:** why reads check every table.
  - **The directory listing's limits**, which motivate M5.
  - **The deviations:** each one, with the milestone that fixes it.
  - **Real numbers** from the experiment.
- [ ] **Step 3: Write the interview notes.** 8 Q&As in the established format. Topics:
  - bloom false-positive maths and choosing bits per key
  - why bloom filters can't have false negatives, and what happens when you delete
  - the flush lifecycle and crash safety
  - why rename-after-sync
  - why the immutable memtable exists
  - L0 read amplification
  - what a whole-table filter versus partitioned filters trade
  - how sequence numbers survive when the logs are gone
- [ ] **Step 4: Update `README.md`.** Set the status to milestone 4 and add the `bloom_demo` command.
- [ ] **Step 5: Commit and push.**
  ```bash
  cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets && \
  git add -A && git commit -m "docs: chapter 04 bloom filters and flush, bloom experiment" && git push
  ```
  Expected: CI is green on ubuntu-latest, macos-latest and msrv 1.89.
- [ ] **Step 6: Stop.** Ask the user for questions on chapter 04 before planning milestone 5.

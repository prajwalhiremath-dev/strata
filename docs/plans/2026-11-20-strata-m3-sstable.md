# strata Milestone 3: SSTable writer and reader. Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the immutable on-disk sorted file (SSTable). A builder writes sorted internal keys into checksummed, prefix-compressed blocks plus an index and footer. A reader opens the file, seeks and iterates, and answers point lookups at a snapshot. It returns `Corruption`, never wrong data, when any byte is damaged.

**Architecture:**
- **Standalone module.** `src/sstable/` is not wired into `Db` yet. Flushing the memtable into a table is milestone 4. This milestone produces the format and proves it on its own.
- **Layout of one file:**
  ```text
  [data block 0][crc] [data block 1][crc] ... [index block][crc] [footer: 48 bytes]
  ```
- **Data blocks.** About 4 KiB of entries. Keys are prefix-compressed against the previous key. Every 16th entry is a *restart point* that stores its full key, and the block ends with the restart offsets so a seek can binary-search them.
- **Index block.** Same block format. One entry per data block: key = that block's last internal key, value = its `BlockHandle` (offset, size).
- **Footer.** A fixed 48 bytes at the end of the file: index handle, filter handle (reserved for M4's bloom filter, zero for now), format version, a CRC over the footer, and a magic number.
- **Reads.** Seek the index to find the block, read that block with one random-access read, verify its CRC, then seek inside it.
- **New filesystem call.** `FileSystem::open_random` returns a read-at handle, because a table must be read piece by piece, not whole.

**Tech Stack:** Rust ≥ 1.89, `crc32fast` 1. Dev: `proptest` 1, `tempfile` 3. No new dependencies.

**Spec:** `docs/specs/2026-11-18-lsm-engine-design.md`. Task 1 amends it with this milestone's additions.

## Global Constraints

- **Key order.** Every key comparison in this milestone uses `key::compare_encoded`, never bytewise `<` or `Ord` on `[u8]`. This covers the builder's ordering check, the restart binary search, the in-block seek and the index seek. Under bytewise order, `k@5` would sort before `k@9`, which is the wrong way round.
- **Defaults.** `TableOptions { block_size: 4096, restart_interval: 16 }`. `restart_interval` must be ≥ 1 and `block_size` ≥ 1, otherwise `InvalidArgument`.
- **Varints.** Unsigned LEB128: 7 bits per byte, high bit = "more follows". A `u32` takes at most 5 bytes, a `u64` at most 10. Decoders reject truncated, overlong and overflowing input with `Corruption`.
- **Block entry:**
  ```text
  varint32 shared | varint32 non_shared | varint32 value_len | key[shared..] | value
  ```
  `shared` is the number of bytes the key shares with the previous key. It is 0 at every restart point.
- **Block tail:** restart offsets as `u32 LE` each, then `num_restarts u32 LE`. `num_restarts ≥ 1` always. An empty block is exactly the 8 bytes `[0u32 LE][1u32 LE]`.
- **Block trailer:** `crc32 u32 LE` (crc32fast, IEEE) over the block contents. It is stored right after the block. `BlockHandle.size` excludes the 4 trailer bytes.
- **BlockHandle encoding:** `varint64 offset | varint64 size`.
- **Footer.** Exactly 48 bytes, all little-endian:
  ```text
  index_offset u64 | index_size u64 | filter_offset u64 | filter_size u64 | version u32 | crc32 u32 | magic u64
  ```
  - The CRC covers bytes 0..36.
  - `version = 1`.
  - `magic = u64::from_le_bytes(*b"strataT1")`.
  - In M3 the filter handle is `(0, 0)`. The reader requires exactly that, and anything else is `Corruption`. M4 relaxes this.
- **Untrusted input.** Nothing read from a file is trusted before it is checked:
  - Every handle needs `offset.checked_add(size)` and then `.checked_add(4)` to succeed and be ≤ the start of the footer before any read or allocation.
  - The restart count must satisfy `4 * (n + 1) ≤ block_len`, using checked arithmetic.
  - Restart offsets: the first is 0, they strictly increase, and each is less than the data length. Or, for an empty block, the restarts are exactly `[0]`.
  - `shared ≤ previous key length`. `shared == 0` at a restart.
  - Key and value lengths must fit in the bytes that remain.
  - Every violation returns `Corruption`. Nothing panics, and nothing allocates more than the file holds.
- **Durability.** `TableBuilder::finish` syncs the file. Syncing the directory is the caller's job (M4), as in M2.
- Carried over from earlier milestones:
  - `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test --all-targets` pass before every commit, chained with `&&`.
  - No time estimates. No co-author trailer on commits.
  - The standard chapter sections and interview format from milestone 1.

## Review Focus

1. **Versions of one user key straddling a block boundary.** Make `get(k, s)` at every snapshot `s` return what the memtable returns. A seek that lands at the end of one block must continue into the next. Pinned in Task 4.
2. **Any single damaged byte, anywhere in the file**, makes `open` or a full scan return `Err`. A table never silently returns different data. Pinned in Task 4.
3. **A zero-entry table.** It builds, opens, iterates as empty, and `get` returns `NotFound`. Pinned in Task 3 and Task 4.
4. **An entry bigger than `block_size`** goes in a block of its own and round-trips. Pinned in Task 4.
5. **Seek before the first key, between keys, and past the last key.** These land on the first key, the next key, and not-valid respectively, with `restart_interval` 1 as well as 16. Pinned in Task 2 and Task 4.

## Not in this milestone

- Bloom filters and flush: M4.
- File numbers in a manifest, and which tables are live: M5.
- Merging across tables: M6.
- Block cache and compression. They're mentioned in the chapter as what real engines add.
- Key shortening in the index. LevelDB stores a short separator instead of the full last key. The chapter explains it, and we keep the full key for simplicity.

---

### Task 1: Varints and random-access reads

**Files:**
- Create: `src/coding.rs`
- Modify: `src/lib.rs` (`mod coding;`), `src/fs/mod.rs`, `src/fs/os.rs`, `src/fs/sim.rs`, `docs/specs/2026-11-18-lsm-engine-design.md`

**Interfaces:**
- Produces in `crate::coding`, all `pub(crate)`:
  ```rust
  fn put_varint32(dst: &mut Vec<u8>, v: u32);
  fn put_varint64(dst: &mut Vec<u8>, v: u64);
  /// Decodes from the front of `src`; returns (value, bytes consumed).
  fn get_varint32(src: &[u8]) -> Result<(u32, usize)>;
  fn get_varint64(src: &[u8]) -> Result<(u64, usize)>;
  ```
- Produces in `strata::fs`:
  ```rust
  pub trait RandomAccessFile: Send + Sync {
      /// Exactly `len` bytes at `offset`; reading past the end is an error.
      fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>>;
      fn size(&self) -> Result<u64>;
  }
  // new FileSystem method:
  fn open_random(&self, path: &Path) -> Result<Box<dyn RandomAccessFile>>;
  ```

- [ ] **Step 1: Write the failing varint tests in `src/coding.rs`.** Cover:
  - **`varint_round_trips`.** Use the values `0, 1, 127, 128, 16383, 16384, u32::MAX` and, for the 64-bit version, `u64::MAX`. Assert the encoded lengths: 127 takes 1 byte, 128 takes 2, `u32::MAX` takes 5, `u64::MAX` takes 10. Assert the consumed count equals the encoded length.
  - **`varint_rejects_bad_input`.** Each case must return `Corruption`:
    - empty input
    - `[0x80]`, which is truncated
    - six continuation bytes for `u32`, which is overlong
    - `[0xff,0xff,0xff,0xff,0x1f]` for `u32`, which overflows
    - eleven bytes for `u64`
  - **proptest `varint_decode_never_panics`.** Random `Vec<u8>`; the call returns without panicking.
- [ ] **Step 2: Run them.** Run `cargo test coding`. Expected: compile failure, because the functions aren't defined yet.
- [ ] **Step 3: Implement the four functions.** For the final allowed byte, overflow means that byte has bits set beyond the type's width (for `u32`, the 5th byte must be ≤ 0x0f).
- [ ] **Step 4: Run them again.** Run `cargo test coding`. Expected: PASS.
- [ ] **Step 5: Write the failing random-access tests.**
  - In `src/fs/sim.rs`:
    - `sim_read_at`: write `b"hello world"`, open it random-access, and check that `read_at(6, 5) == b"world"`, `size() == 11`, and that `read_at(8, 5)` is `Err`.
    - `sim_read_at_dies_after_crash`: a handle opened before `crash()` fails on `read_at` afterwards.
    - `open_random` on a missing file is `Err`.
  - In `src/fs/os.rs`: `os_read_at`, the same checks on a `tempfile` directory.
- [ ] **Step 6: Run them.** Run `cargo test fs::`. Expected: compile failure, because `open_random` doesn't exist yet.
- [ ] **Step 7: Implement `open_random`.**
  - **`OsFs`:** under `#[cfg(unix)]`, use `std::os::unix::fs::FileExt::read_exact_at`. Otherwise, use a `Mutex<File>` with seek plus `read_exact`. `size` comes from `metadata().len()`. A short read maps to `Error::Io(UnexpectedEof)`.
  - **`SimFs`:** the handle holds the shared state, the path and the epoch. It reads the live contents. A stale epoch or out-of-range read returns `Err`. `size` returns the live length.
- [ ] **Step 8: Run the tests.** Run `cargo test fs::`. Expected: PASS.
- [ ] **Step 9: Amend the spec.** Add "Milestone 3 additions" after the M2 additions:
  - `RandomAccessFile` and `open_random`.
  - The trailer is only a CRC. There's no compression byte, and compression is out of scope.
  - The exact footer layout, with its CRC and magic. The filter handle is reserved until M4.
  - The index stores the full last key (no shortening).
  - No block cache.
  - Varint encoding for the block format.
- [ ] **Step 10: Commit.**
  ```bash
  cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets && \
  git add -A && git commit -m "feat(fs): random-access reads; varint coding"
  ```

---

### Task 2: Blocks: builder, reader, iterator

**Files:**
- Create: `src/sstable/mod.rs`, `src/sstable/block.rs`
- Modify: `src/lib.rs` (`pub mod sstable;`)

**Interfaces:**
- Consumes: `coding::{put_varint32, get_varint32}`, `key::compare_encoded`.
- Produces in `crate::sstable::block`, all `pub(crate)`:
  ```rust
  struct BlockBuilder { .. }
  impl BlockBuilder {
      fn new(restart_interval: usize) -> Self;
      /// Caller guarantees keys arrive in compare_encoded order.
      fn add(&mut self, key: &[u8], value: &[u8]);
      fn estimated_size(&self) -> usize;   // bytes finish() would return now
      fn is_empty(&self) -> bool;
      fn finish(&mut self) -> Vec<u8>;     // contents (no trailer); resets the builder
  }
  struct Block { .. }                      // owns the contents
  impl Block {
      fn new(contents: Vec<u8>) -> Result<Block>;  // validates the restart array
      fn iter(&self) -> BlockIter<'_>;
  }
  struct BlockIter<'a> { .. }
  impl BlockIter<'_> {
      fn valid(&self) -> bool;
      fn key(&self) -> &[u8];              // full reconstructed key
      fn value(&self) -> &[u8];
      fn seek_to_first(&mut self) -> Result<()>;
      /// First entry with key >= target (compare_encoded); not valid if none.
      fn seek(&mut self, target: &[u8]) -> Result<()>;
      fn next(&mut self) -> Result<()>;
  }
  ```
  `key()` and `value()` may panic only when called on an iterator that isn't valid. That is caller misuse, not bad input.

- [ ] **Step 1: Write the failing tests in `block.rs`.** Use a test helper `ik(user: &[u8], seq: u64) -> Vec<u8>` that builds an encoded `Put` internal key. The tests:
  - **`empty_block_is_8_bytes`.** `finish()` on a new builder returns `[0,0,0,0,1,0,0,0]`. `Block::new` accepts it, and `seek_to_first` leaves the iterator not valid.
  - **`round_trip_iteration`.** 100 keys `key{i:03}` with restart interval 16. The iterator yields exactly the inserted pairs in order.
  - **`restart_count`.** 33 entries at interval 16 give 3 restarts (read from the last 4 bytes). With interval 1, 33 entries give 33 restarts.
  - **`prefix_compression_shrinks`.** The same 100 keys at interval 16 make a block smaller than at interval 1.
  - **`seek_cases`.** For intervals 1 and 16, with keys `b, d, f, …`:
    - seek(`a`) gives `b`
    - seek(`d`) gives `d`
    - seek(`e`) gives `f`
    - seek past the last key gives not valid
  - **`seek_uses_internal_order`.** Add `ik(k,9), ik(k,5), ik(k,1)`, in internal order with the newest first. Then:
    - seek(`ik(k,6)`) lands on `ik(k,5)`
    - seek(`ik(k,u56max)`) lands on `ik(k,9)`

    Bytewise order would land on the wrong ones.
  - **`rejects_bad_restarts`.** Each must be `Corruption`:
    - fewer than 4 bytes
    - `num_restarts` larger than fits in the block
    - a first restart other than 0
    - restarts that don't increase
    - a restart pointing past the data
  - **`rejects_bad_entries`.** Hand-build these contents; each must be `Corruption` from `seek_to_first`/`next`/`seek`:
    - `shared > 0` at a restart
    - `shared` larger than the previous key
    - `value_len` running past the data
  - **proptest `arbitrary_bytes_never_panic`.** Random `Vec<u8>` (up to 512 bytes) through `Block::new`. If that is `Ok`, call `seek_to_first`, then `next` until it is not valid or returns `Err` (cap at 1000 steps), then `seek(random)`. Nothing panics.
  - **proptest `block_model`.** A random set of 0–200 user keys over a small alphabet, plus a random restart interval in 1..=20. Store each key as `ik(user, 0)`; with equal trailers, internal order is user-key order, so a `BTreeSet` of the user keys is the oracle.
    - Iteration matches the set.
    - `seek(ik(t, 0))` matches `BTreeSet::range(t..).next()`.
- [ ] **Step 2: Run them.** Run `cargo test sstable::block`. Expected: compile failure.
- [ ] **Step 3: Implement.**
  - **Seek:** binary-search the restart points. At each restart, decode only the full key and find the last restart whose key is < target. Then scan forward linearly to the first key ≥ target.
  - **Validation:** checks on the restart array happen in `Block::new`. Entry checks happen as entries are decoded.
- [ ] **Step 4: Run the tests.** Run `cargo test sstable::block`. Expected: PASS.
- [ ] **Step 5: Commit.**
  ```bash
  cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets && \
  git add -A && git commit -m "feat(sstable): prefix-compressed blocks with restart points"
  ```

---

### Task 3: Table format and builder

**Files:**
- Create: `src/sstable/format.rs`, `src/sstable/builder.rs`
- Modify: `src/sstable/mod.rs`

**Interfaces:**
- Consumes: `BlockBuilder`, `WritableFile`, `coding`, `key::{decode, compare_encoded}`.
- Produces in `crate::sstable::format`, all `pub(crate)`:
  ```rust
  const FOOTER_LEN: usize = 48; const FORMAT_VERSION: u32 = 1;
  const MAGIC: u64 = u64::from_le_bytes(*b"strataT1"); const BLOCK_TRAILER_LEN: usize = 4;
  #[derive(Clone, Copy, Debug, PartialEq, Eq)] struct BlockHandle { offset: u64, size: u64 }
  impl BlockHandle { fn encode_to(&self, dst: &mut Vec<u8>); fn decode(src: &[u8]) -> Result<(BlockHandle, usize)>; }
  struct Footer { index: BlockHandle, filter: BlockHandle }
  impl Footer { fn encode(&self) -> [u8; FOOTER_LEN]; fn decode(buf: &[u8]) -> Result<Footer>; }
  ```
- Produces publicly in `strata::sstable`:
  ```rust
  #[derive(Clone, Debug)] pub struct TableOptions { pub block_size: usize, pub restart_interval: usize }  // Default: 4096, 16
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub struct TableMeta { pub file_size: u64, pub num_entries: u64, pub smallest: Vec<u8>, pub largest: Vec<u8> }
      // smallest/largest: encoded internal keys, empty when num_entries == 0
  pub struct TableBuilder { .. }
  impl TableBuilder {
      pub fn new(file: Box<dyn WritableFile>, opts: TableOptions) -> Result<Self>;   // validates opts
      /// `key` is an encoded internal key, strictly greater (compare_encoded) than the previous one.
      pub fn add(&mut self, key: &[u8], value: &[u8]) -> Result<()>;
      /// Writes the last data block, an empty filter (handle 0,0), the index, the footer; syncs the file.
      pub fn finish(self) -> Result<TableMeta>;
  }
  ```

- [ ] **Step 1: Write the failing tests.**
  - In `format.rs`:
    - **`footer_round_trip`.** Encode a footer and decode it back to the same handles.
    - **`footer_is_48_bytes_and_ends_with_magic`.**
    - **`footer_rejects`.** Each must be `Corruption`:
      - the wrong length
      - a bad magic
      - a bad version, with the CRC recomputed so that only the version is wrong
      - a flipped byte in the first 36 bytes
      - a nonzero filter handle
    - **`handle_round_trip_and_truncated`.**
  - In `builder.rs`, using `SimFs`:
    - **`rejects_bad_options`.** `block_size: 0` or `restart_interval: 0` gives `InvalidArgument`.
    - **`rejects_out_of_order_and_duplicates`.** Each must be `InvalidArgument`:
      - `add(ik(b,1))` then `add(ik(a,1))`
      - adding the same key twice
      - adding `ik(k,1)` then `ik(k,5)`, because a newer version must come *before* an older one
    - **`rejects_non_internal_key`.** A 3-byte key gives `InvalidArgument`.
    - **`empty_table_layout`.** `finish()` with no entries gives `file_size == 8 + 4 + 48` (an empty index block, its trailer and the footer), `num_entries == 0`, and empty smallest and largest.
    - **`meta_reports_bounds`.** After 1000 entries, `smallest` and `largest` equal the first and last key added, and `file_size` equals the SimFs file length.
    - **`finish_syncs`.** After `finish()`, `crash()`, the file length is unchanged.
    - **`small_blocks_split`.** With `block_size: 64`, 100 entries produce a file whose index holds more than one entry. Check by decoding the footer and index with `Block` directly.
- [ ] **Step 2: Run them.** Run `cargo test sstable::`. Expected: compile failure.
- [ ] **Step 3: Implement.**
  - **Flushing:** after each `add`, if `estimated_size() >= block_size`, flush the block. Write the contents plus the CRC, then add an index entry (that block's last key → handle).
  - **`finish`:** flush the remaining block if it isn't empty, then write the index block plus its CRC, then the footer, then call `sync`.
  - **Offset:** track the current file offset in the builder. Don't query the file for it.
- [ ] **Step 4: Run the tests.** Run `cargo test sstable::`. Expected: PASS.
- [ ] **Step 5: Commit.**
  ```bash
  cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets && \
  git add -A && git commit -m "feat(sstable): table builder, block handles and footer"
  ```

---

### Task 4: Table reader: open, iterate, seek, get

**Files:**
- Create: `src/sstable/table.rs`, `src/sstable/tests.rs` (`#[cfg(test)]` model and corruption tests; they're crate-internal so they can use `Memtable` as the oracle)
- Modify: `src/sstable/mod.rs`

**Interfaces:**
- Consumes: `Block`, `BlockIter`, `Footer`, `BlockHandle`, `RandomAccessFile`, `memtable::Lookup`.
- Produces publicly in `strata::sstable`:
  ```rust
  pub struct Table { .. }   // holds the file, its size, and the decoded index Block
  impl Table {
      /// Reads + verifies the footer and the index block.
      pub fn open(file: Box<dyn RandomAccessFile>) -> Result<Table>;
      pub fn iter(&self) -> TableIter<'_>;
      /// Newest version of `user_key` with seqno <= snapshot.
      pub(crate) fn get(&self, user_key: &[u8], snapshot: SeqNo) -> Result<Lookup>;
  }
  pub struct TableIter<'a> { .. }   // index BlockIter + current data Block
  impl TableIter<'_> {
      pub fn valid(&self) -> bool;
      pub fn key(&self) -> &[u8];      // encoded internal key
      pub fn value(&self) -> &[u8];
      pub fn seek_to_first(&mut self) -> Result<()>;
      pub fn seek(&mut self, target: &[u8]) -> Result<()>;
      pub fn next(&mut self) -> Result<()>;
  }
  ```

- [ ] **Step 1: Write the failing tests.**
  - In `table.rs`:
    - **`empty_table`.** `Table::open` on an empty table succeeds. `seek_to_first` leaves the iterator not valid. `get` returns `NotFound`.
    - **`get_basic`.** With `a@3 Put "x"`, `b@5 Delete` and `b@2 Put "y"`:
      - `get(a, 10)` gives `Found("x")`
      - `get(a, 2)` gives `NotFound`
      - `get(b, 10)` gives `Deleted`
      - `get(b, 4)` gives `Found("y")`
      - `get(c, 10)` gives `NotFound`
      - `get(aa, 10)` gives `NotFound`, which is the user-key equality check
    - **`versions_straddle_blocks`.** One user key with 200 versions (seqnos 1..=200), `block_size: 64`. For every s in 1..=200, `get(k, s)` returns `Found(value_s)`.
    - **`huge_entry`.** A 10 KiB value with `block_size: 4096`, between two small entries. All three round-trip. The big entry sits alone in its block, which you check through the index entry count.
    - **`seek_edges`.** For intervals 1 and 16: seek before the first key, between keys, exactly on a key, and past the last key, including across a block boundary.
    - **`file_too_short`.** Open a file shorter than 48 bytes: `Corruption`.
    - **`handle_out_of_range`.** Write a footer whose index handle points beyond the file, with a valid CRC: `Corruption`, and no huge allocation. Use `u64::MAX` for the offset and for the size.
  - In `tests.rs`:
    - **proptest `table_matches_memtable`.**
      - **Input:** 0–300 random ops over user keys from a small alphabet (1–3 bytes from `a..=d`, so many versions collide). Each op is Put (with a random value of 0–40 bytes) or Delete, with seqnos 1, 2, 3 and so on. A random `block_size` in 16..=512 and `restart_interval` in 1..=8.
      - **Setup:** insert every op into a `Memtable`, then build a table from `memtable.entries()`.
      - **Check:**
        - the full table iteration equals `entries()` (encoded keys and values)
        - for each user key in the alphabet and each snapshot in 0..=n+1, `table.get` equals `memtable.get`
        - `seek(random encoded internal key)` lands on the same entry as a `BTreeMap` lower-bound search over `entries()`
    - **`every_byte_flip_is_detected`.**
      - **Setup:** build a table with about 300 entries, `block_size: 128`. For every byte position `i` in the file, copy the file, XOR byte `i` with `0x01`, then `open`, and if that is `Ok`, `seek_to_first` plus `next` until the end.
      - **Assert:** some step returns `Err`. Count the cases and assert every position was covered.
    - **proptest `arbitrary_file_never_panics`.** Random bytes, 0–2048 long, sometimes ending in a valid footer pointing inside them. Run open, a full scan and a few `get`s. Results may be `Ok` or `Err` but must never panic.
- [ ] **Step 2: Run them.** Run `cargo test sstable::`. Expected: compile failure.
- [ ] **Step 3: Implement.**
  - **`open`:**
    1. Check `size ≥ FOOTER_LEN`.
    2. Read and decode the footer.
    3. Bounds-check the index handle against `size - FOOTER_LEN` with `checked_add`.
    4. Read the block plus trailer and verify the CRC.
    5. Call `Block::new`.
  - **Reading a data block:** the same bounds check, CRC check and `Block::new`.
  - **`TableIter::seek`:**
    1. Seek the index iterator to the target.
    2. If it's valid, decode its handle, load the block, and seek inside it.
    3. If the block iterator is then not valid, advance the index and seek to the first entry of the next block.
  - **`next`:** advance in the block. At the end of the block, move to the next index entry.
  - **`get`:**
    1. Seek to `InternalKey::new(user_key, snapshot, Put).encode()`.
    2. If the iterator is valid and the decoded key's user key equals `user_key`, return `Found(value)` or `Deleted` according to the kind.
    3. Otherwise, return `NotFound`.
- [ ] **Step 4: Run the tests.** Run `cargo test sstable::`. Expected: PASS.
- [ ] **Step 5: Mutation checks.** Apply each mutation, run `cargo test sstable::`, confirm a named test fails, then revert with `cp` from a scratch backup, not `git checkout`:
  - (a) Skip the CRC comparison on data blocks. Expected: `every_byte_flip_is_detected` fails.
  - (b) The index stores each block's *first* key. Expected: `table_matches_memtable` or `seek_edges` fails.
  - (c) `get` drops the user-key equality check. Expected: `get_basic` fails.
  - (d) The block's in-block seek uses bytewise `<`. Expected: `seek_uses_internal_order` or `versions_straddle_blocks` fails.

  Record each result in the ledger.
- [ ] **Step 6: Commit.**
  ```bash
  cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets && \
  git add -A && git commit -m "feat(sstable): table reader with seek, iteration and snapshot get"
  ```

---

### Task 5: Experiment, chapter 03, interview notes, README

**Files:**
- Create: `examples/sst_layout.rs`, `docs/learn/03-sstables.md`, `docs/interview/03-sstables.md`
- Modify: `README.md`

**Interfaces:**
- Consumes: `TableBuilder`, `TableOptions`, `Table`, `TableIter`, `fs::OsFs`.

- [ ] **Step 1: Write `examples/sst_layout.rs`.**
  - **Data:** 100,000 sorted keys of the form `user:{i:08}` with 16-byte values.
  - **Builds:** write a table into a temporary directory for each restart interval in 1, 4, 16 and 64, all at `block_size` 4096.
  - **For each build, print:**
    - file size
    - bytes saved versus interval 1, as a percentage
    - the number of data blocks, counted from the index
    - the average time per random `get` over 10,000 lookups
  - **Hex dump:** also print a hex dump of the first 3 entries of the first block. It should show `shared`/`non_shared` in action.
  - **Run:** `cargo run --release --example sst_layout`. Expected: file size falls as the interval grows, and lookup cost rises slightly.
- [ ] **Step 2: Write `docs/learn/03-sstables.md`.** Use the standard sections. Cover:
  - why the file is immutable: no in-place updates, safe concurrent reads, and the file can be deleted as a unit
  - the layout diagram
  - varints
  - prefix compression and restart points, with a worked example of three keys
  - binary search over restarts and then a linear scan
  - why the index stores the *last* key
  - why seeks use internal-key order
  - CRC per block and why the footer has its own CRC and magic
  - why untrusted lengths are checked before allocating
  - what real engines add: compression, a block cache, key shortening, partitioned indexes
  - the experiment's real numbers from the user's machine
- [ ] **Step 3: Write `docs/interview/03-sstables.md`.** 8 Q&As in the established format. Topics:
  - the SSTable layout
  - the restart interval trade-off
  - how a point lookup works
  - why the newest version sorts first, and how a seek finds it
  - detecting corruption
  - why the files are immutable
  - index size and how to shrink it
  - compression with a block cache
- [ ] **Step 4: Update `README.md`.** Set the status to milestone 3 and add the `sst_layout` command.
- [ ] **Step 5: Commit and push.**
  ```bash
  cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets && \
  git add -A && git commit -m "docs: chapter 03 SSTables, layout experiment" && git push
  ```
  Expected: CI is green on ubuntu-latest, macos-latest and msrv 1.89.
- [ ] **Step 6: Stop.** Ask the user for questions on chapter 03 before planning milestone 4.

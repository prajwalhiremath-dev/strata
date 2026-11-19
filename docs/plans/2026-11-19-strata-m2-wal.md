# strata Milestone 2: WAL, FileSystem trait and crash recovery. Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make writes durable. Every batch is appended to a write-ahead log (WAL) before it reaches the memtable, and `Db::open(path)` recovers everything acknowledged after a crash, including from torn writes. A simulated filesystem proves it.

**Architecture:**
- **File I/O:** all of it goes through a `FileSystem` trait. There are two implementations: `OsFs` (real disk) and `SimFs` (in memory). `SimFs` tracks synced vs unsynced bytes and directory entries, and can "crash".
- **WAL format:** LevelDB-style 32 KiB blocks of checksummed records (Full/First/Middle/Last fragments). Each record's payload is one encoded `WriteBatch`.
- **Opening a database:**
  1. Lock the directory.
  2. Replay the logs in file-number order.
  3. **Checkpoint:** write the recovered memtable into a brand-new log, sync it, sync the directory, delete the old logs, sync the directory again.
  4. Accept writes.
- **No old files are reused.** We never append to a log that existed before `open`. This is the stand-in for flush (M4), which will replace the checkpoint.

**Tech Stack:** Rust ≥ 1.89 (for `File::try_lock`), `crc32fast` 1, dev: `tempfile` 3, `proptest` 1.

**Spec:** `docs/specs/2026-11-18-lsm-engine-design.md` (amended in Task 1 with this milestone's additions)

## Global Constraints

- `rust-version = "1.89"` in `Cargo.toml`, and CI adds a job pinned to `1.89.0`.
- WAL block size `32 * 1024`. Record header 7 bytes: `crc32 (u32 LE) | length (u16 LE) | type (u8)`. The CRC (crc32fast, IEEE) covers `type byte ++ payload`. Types: `Full=1, First=2, Middle=3, Last=4`. A block remainder under 7 bytes is zero-filled.
- Log file names `{number:06}.log`. Lock file `LOCK`. Numbers start at 1 and only grow.
- `SyncPolicy` default is `Always`.
- **Write order is fixed:** validate → lock → closed/poisoned check → encode → WAL append → (sync per policy) → memtable insert → publish `visible_seqno`.
- **Decoders never panic** on any input. They return `Error::Corruption`.
- Standard chapter sections and interview format from milestone 1. No time estimates. No co-author trailer.
- `cargo fmt --check`, `clippy -D warnings` and `cargo test` must pass before every commit.

## Review Focus

1. **Torn tail then new writes.** Crash with a torn tail, reopen, write, crash again, reopen. The second round of acknowledged writes must be present. (This is the "appended after garbage" bug.) Pinned in Task 4.
2. **Full-length record with garbage payload at the tail** (length metadata landed, data didn't). It's a torn write, not corruption. It must open even with `paranoid_checks: true`. Pinned in Tasks 2 and 4.
3. **A write after a failed append or sync** must not be acknowledged: the Db is poisoned. Pinned in Task 4.
4. **A crash between checkpoint steps** (new log written but old ones not deleted, or the reverse) recovers the same state. Pinned in Task 4.
5. **Arbitrary bytes fed to the WAL reader or batch decoder** give `Corruption` or a clean end, never a panic. Pinned in Tasks 2 and 3.

## Not in this milestone

Torn writes inside *synced* data, bit rot in synced data beyond what's checked above, random I/O-error schedules, and disk-full handling. All of those are milestone 9. Group commit is milestone 8.

---

### Task 1: `FileSystem` trait, `OsFs`, `SimFs`

**Files:**
- Create: `src/fs/mod.rs`, `src/fs/os.rs`, `src/fs/sim.rs`
- Modify: `src/lib.rs` (`pub mod fs;`), `src/error.rs`, `Cargo.toml`, `.github/workflows/ci.yml`, `docs/specs/2026-11-18-lsm-engine-design.md`

**Interfaces:**
- Produces in `strata::fs`:
  ```rust
  pub trait WritableFile: Send {
      fn append(&mut self, data: &[u8]) -> Result<()>;
      fn sync(&mut self) -> Result<()>;           // contents durable
  }
  pub trait FileLock: Send {}                     // released on drop
  pub trait FileSystem: Send + Sync + std::fmt::Debug {
      fn create_dir_all(&self, dir: &Path) -> Result<()>;
      fn create(&self, path: &Path) -> Result<Box<dyn WritableFile>>; // create or truncate
      fn read(&self, path: &Path) -> Result<Vec<u8>>;
      fn list(&self, dir: &Path) -> Result<Vec<String>>;   // file names, sorted
      fn remove(&self, path: &Path) -> Result<()>;
      fn sync_dir(&self, dir: &Path) -> Result<()>;        // directory entries durable
      fn lock(&self, path: &Path) -> Result<Box<dyn FileLock>>; // Err(AlreadyOpen) if held
  }
  pub struct OsFs;                                // Default, Debug, Clone
  pub struct SimFs { /* Arc<Mutex<..>> */ }      // Default, Debug, Clone (clones share state)
  ```
- `SimFs` test hooks: `crash()` (drop all unsynced data), `crash_torn(seed: u64)` (see below), `corrupt_byte(&self, path: &Path, offset: usize)`, `fail_next_append()`, `fail_next_sync()`, `kill_after(&self, n: usize)` (after `n` more mutating calls, meaning `create`, `append`, `sync`, `sync_dir` or `remove`, every call returns `Error::Io` and has no effect: the "process" is dead until `crash*()` is called), `file_len(&self, path: &Path) -> Option<usize>`, `copy_file(&self, from: &Path, to: &Path)` (durable copy, test helper).
- `Error` gains `AlreadyOpen(String)` and `Poisoned(String)`.

**`SimFs` semantics:**
- Each file has `data` and `synced_len`.
- Each directory has a *live* set of names and a *durable* set. `sync_dir` copies live to durable.
- **On any crash:**
  - names revert to the durable set (files whose creation wasn't dir-synced vanish, and removals that weren't dir-synced are undone)
  - locks are released
  - `crash()` truncates each file to `synced_len`
  - `crash_torn(seed)` picks, per file and deterministically from the seed, one of: (a) keep a random prefix of the unsynced bytes, or (b) keep the full length but overwrite the unsynced suffix with zeros or pseudo-random bytes
- `fail_next_*` makes the next call return `Error::Io(..)`, and that call writes nothing (for append).

- [ ] **Step 1:** Update `Cargo.toml`: `rust-version = "1.89"`, `crc32fast = "1"`, dev `tempfile = "3"`. In CI add a `msrv` job: `dtolnay/rust-toolchain@1.89.0`, then `cargo test`. Add the error variants.
- [ ] **Step 2: Failing tests** in `src/fs/sim.rs`:
  - `unsynced_data_lost_on_crash`: append "abc", sync, append "def", `crash()` → `read` gives `"abc"`
  - `unsynced_create_lost_on_crash`: create without `sync_dir`, then crash → `list` doesn't contain it
  - `unsynced_remove_undone_on_crash`
  - `torn_crash_is_deterministic`: same seed gives the same bytes
  - `torn_crash_keeps_synced_prefix`: for 100 seeds, the first `synced_len` bytes are unchanged
  - `lock_is_exclusive_until_dropped`
  - `crash_releases_locks`
  - `fail_next_append_writes_nothing`
  - `kill_after_stops_all_mutation`: `kill_after(1)`, then two appends → the first succeeds, the second is `Err`, and after `crash()` mutation works again

  In `src/fs/os.rs` (with `tempfile`): `os_roundtrip_and_list` and `os_lock_is_exclusive`.
- [ ] **Step 3:** Run `cargo test fs::`. Expected: FAIL (missing items).
- [ ] **Step 4: Implement.**
  - `OsFs` uses `std::fs`.
  - `sync` is `File::sync_all`. On macOS that's `F_FULLFSYNC`, verified to work on directories too.
  - `sync_dir` is `File::open(dir)?.sync_all()`.
  - `lock` creates the file and calls `try_lock`, mapping `TryLockError::WouldBlock` to `AlreadyOpen`.
  - `SimFs` uses a small xorshift PRNG for `crash_torn`, the same as in the chapter 00 example.
- [ ] **Step 5:** Run `cargo test fs::`. Expected: PASS.
- [ ] **Step 6:** Amend the spec. Add a "Milestone 2 additions" note to §5 covering: the `LOCK` file, `AlreadyOpen`/`Poisoned` errors, checkpoint-at-open (temporary until M4), MSRV 1.89, and `crc32fast` + `tempfile`. Commit `feat(fs): FileSystem trait with OS and simulated crash-capable implementations`.

### Task 2: WAL writer and reader

**Files:** Create `src/wal/mod.rs`, `src/wal/format.rs`, `src/wal/writer.rs`, `src/wal/reader.rs`.

**Interfaces:**
- Consumes: `WritableFile` (Task 1).
- Produces:
  ```rust
  pub(crate) struct WalWriter { /* Box<dyn WritableFile>, block_offset: usize */ }
  impl WalWriter {
      pub(crate) fn new(file: Box<dyn WritableFile>) -> Self;
      pub(crate) fn add_record(&mut self, payload: &[u8]) -> Result<()>; // fragments across blocks
      pub(crate) fn sync(&mut self) -> Result<()>;
  }
  pub(crate) enum WalEnd { Clean, TornTail { offset: usize }, Corrupt { offset: usize } }
  pub(crate) fn read_records(buf: &[u8]) -> (Vec<Vec<u8>>, WalEnd);
  ```

**Reader rules:**
- Reassemble the fragments. Stop at the first bad spot. "Bad" means any of these:
  - a header that runs past EOF
  - a length that runs past the block or past EOF
  - a CRC mismatch
  - an unknown type
  - a fragment sequence error (Middle/Last without First, or a First while one is in progress)
- An all-zero 7-byte header means "rest of block is padding or preallocated zeros". Skip to the next block. If nothing valid follows, it's `Clean`.
- **Classifying a bad spot at offset X:** try to parse a valid *Full* or *First* record at each later block boundary.
  - If any parses → `Corrupt { offset: X }`.
  - Otherwise → `TornTail { offset: X }`.
  - Logs are never reused, so a valid record after the bad spot can only mean mid-log damage.

- [ ] **Step 1: Failing tests** in `src/wal/`:
  - `roundtrip_small`: 3 records
  - `roundtrip_spans_blocks`: 100 KiB record + small records
  - `record_ends_within_7_bytes_of_block_end`: payload sized so the remaining block space is 0–6 bytes, then another record. Both round-trip, and the padding is zeros.
  - `truncated_at_every_offset`: for a 3-record log, truncating at every byte length gives only whole records from the front and `TornTail` or `Clean`, never `Corrupt`, never a panic
  - `garbage_suffix_is_torn_tail`: valid records, then a full-length last record with its payload bytes replaced → `TornTail`
  - `damage_before_valid_record_is_corrupt`: 3 records, each in its own block. Flip a payload byte in record 2 → returns `[record 1]` and `Corrupt`.
  - proptest `reader_never_panics`: arbitrary `Vec<u8>` (0..70_000 bytes) → no panic
- [ ] **Step 2:** `cargo test wal::`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test wal::`. Expected: PASS.
- [ ] **Step 5:** Commit `feat(wal): block-based write-ahead log with checksummed fragments`.

### Task 3: Batch encoding

**Files:** Modify `src/batch.rs`.

**Interfaces:**
- Produces:
  - `pub(crate) fn encode(&self, first_seqno: SeqNo) -> Vec<u8>`
  - `pub(crate) fn decode(buf: &[u8]) -> Result<(SeqNo, WriteBatch)>`
  - `pub const MAX_VALUE_LEN: usize = u32::MAX as usize` (re-exported from the crate root). `Db::write` rejects longer values with `InvalidArgument`.

**Layout:** `first_seqno u64 LE | count u32 LE`, then per op: `kind u8 | key_len u32 LE | key`, and for Put `| val_len u32 LE | val`. Any length running past the buffer, trailing bytes, `count` not matching, or an unknown kind → `Corruption`.

- [ ] **Step 1: Failing tests:**
  - `batch_roundtrip` (puts, deletes, empty value)
  - `decode_rejects_truncated` (every prefix of a valid encoding is `Err`)
  - `decode_rejects_trailing_bytes`
  - proptest `decode_never_panics` on arbitrary bytes
- [ ] **Step 2:** `cargo test batch`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test batch`. Expected: PASS.
- [ ] **Step 5:** Commit `feat(batch): binary encoding for WAL payloads`.

### Task 4: Durable `Db`: open, recovery, checkpoint, sync policy, poisoning

**Files:** Modify `src/db.rs`, `src/options.rs`, `src/memtable.rs`, `src/lib.rs`. Create `tests/recovery.rs`.

**Interfaces:**
- Consumes: Tasks 1–3.
- Produces:
  - `pub enum SyncPolicy { Always, EveryN(u32), Never }`. The default is `Always`. `EveryN(n)` syncs on every n-th write call.
  - `Options` gains `pub sync: SyncPolicy` and `pub paranoid_checks: bool` (default `false`).
  - `Db::open(path: impl AsRef<Path>, opts: Options) -> Result<Db>`
  - `Db::open_with_fs(path: impl AsRef<Path>, opts: Options, fs: Arc<dyn FileSystem>) -> Result<Db>`
  - `Db::open_in_memory(opts)` keeps working. It is now `open_with_fs("/mem", opts, Arc::new(SimFs::default()))`.
  - `pub(crate) fn Memtable::entries(&self) -> Vec<(InternalKey, Vec<u8>)>` (all versions, sorted)

**`open` sequence:**
1. `create_dir_all`, then `lock(dir/LOCK)` (`AlreadyOpen` if it's held).
2. List the `*.log` files and sort them by number.
3. For each log, `read_records` and decode each payload as a batch. Insert the entries with their **original** seqnos and track the max seqno.
   - `Corrupt` with `paranoid_checks` → return `Corruption`.
   - `Corrupt` without it → stop replaying *all* logs at that point (point-in-time).
   - `TornTail` → continue to the next log.
4. **Checkpoint:**
   1. Create `{max_number+1}.log`.
   2. Write every memtable entry as a one-op batch with its seqno.
   3. Sync the log, then `sync_dir`.
   4. Remove the old logs, then `sync_dir`.
5. Set `last` and `visible_seqno` to the max seqno, and keep the new log's writer.

**`write`:**
- Under the write lock: check closed, then poisoned. Encode, `add_record`, and sync per policy.
- On an append or sync error, set `poisoned = Some(message)` and return `Error::Io`. From then on, every write returns `Error::Poisoned`. Reads still work.
- Insert, then publish.

**`close`:** under the write lock, sync the log (any policy), mark closed, and drop the lock. A second `close` is `Ok`.

- [ ] **Step 1: Failing tests** in `tests/recovery.rs` (`SimFs` unless noted):
  - `reopen_recovers_writes_os`: tempdir and `OsFs`. Put, delete, batch, close, reopen → same `scan`.
  - `acknowledged_writes_survive_crash`: `Always`, 200 batches, `crash()`, reopen → all present.
  - `never_policy_recovers_a_prefix`: `Never`, 200 single-key batches (key i → i), `crash_torn(seed)` for 50 seeds. The recovered keys must be exactly `0..k` for some k, so batches are never partial.
  - `torn_tail_then_more_writes_survive`, for 50 seeds:
    1. Open with `Never`, write 50 batches, `crash_torn(seed)`.
    2. Reopen with `Always`, write batch B, `crash()`.
    3. Reopen → B is present.
  - `torn_tail_opens_in_paranoid_mode`: as above with `paranoid_checks: true`, for 50 seeds → open succeeds.
  - `mid_log_corruption`:
    1. `Always`, then write enough data for 3+ blocks. Use the data-file path from `fs.list`.
    2. Write batches, close, then `corrupt_byte` inside the first record.
    3. Paranoid → `Err(Corruption)`.
    4. Non-paranoid → opens, with a prefix of the batches present.
  - `crash_at_every_step_of_open`:
    1. With `Always`, write 100 batches and `close`. Record the expected `scan`.
    2. For `n` in `0..` until open succeeds: `kill_after(n)`, `open` (expect `Err`), `crash()`, then reopen normally → `scan` equals expected.
    3. This covers a crash between every checkpoint step.
  - `old_and_new_logs_together_replay_same_state`: after a reopen, `copy_file` a saved copy of the pre-checkpoint log back into the dir, then reopen → same `scan` (replay is idempotent).
  - `failed_append_poisons`: `fail_next_append()` → `write` is `Err(Io)`, the next `write` is `Err(Poisoned)`, `get` still works, and after `crash()` + reopen, writes work again.
  - `failed_sync_poisons`: the same with `fail_next_sync()`.
  - `second_open_fails_while_open`: `Err(AlreadyOpen)`. After `close`, open succeeds.
  - `seqno_continues_after_reopen`: after reopen, an overwrite of an old key wins.
- [ ] **Step 2:** `cargo test --test recovery`. Expected: FAIL.
- [ ] **Step 3:** Implement. Keep the M1 tests green (`open_in_memory`).
- [ ] **Step 4:** `cargo test`. Expected: all PASS. Run `--test recovery` 3 times.
- [ ] **Step 5:** Commit `feat(db): durable writes via WAL, crash recovery, checkpoint at open`.

### Task 5: Model test gains `Reopen` and `Crash`

**Files:** Modify `tests/model.rs`.

- [ ] **Step 1:** Add `Op::Reopen` (close and reopen on the same `SimFs`) and `Op::Crash` (`fs.crash()`, then reopen). Run the db with `SyncPolicy::Always`, so the model is unchanged by both ops. Weights: 1 each.
- [ ] **Step 2:** `cargo test --test model`. Expected: PASS.
- [ ] **Step 3: Mutation check.** Temporarily skip `sync` in `write`. Expected: FAIL after a `Crash` op. Revert.
- [ ] **Step 4:** Commit `test: model test covers reopen and crash`.

### Task 6: Crash demo, chapter 02, interview 02, publish → STOP

**Files:**
- Create: `examples/crash_demo.rs`, `docs/learn/02-wal-and-recovery.md`, `docs/interview/02-wal-and-recovery.md`
- Modify: `README.md`

- [ ] **Step 1: Write `crash_demo`.**
  - `write <dir> [always|never]` loops forever writing key `n` → `n`. After each acknowledged write it prints `acked n` (flushed stdout) every 1,000 writes.
  - `verify <dir> <n>` checks that keys `0..=n` are present.
  - Run it: start `write`, `kill -9` after about 1 s, then `verify` with the last printed number. It must pass for `always`. For `never`, report what happens honestly.
- [ ] **Step 2: Write chapter 02** with the standard sections, covering:
  - what "durable" means
  - the page cache and `fsync`, plus `F_FULLFSYNC` on macOS
  - why the *directory* must be synced
  - the PostgreSQL "fsyncgate" (2018) lesson and why we poison after a failed sync
  - the record and block format diagram
  - CRCs
  - torn writes vs corruption, and why we classify by position
  - why logs are never reused (with the torn-tail-then-write scenario)
  - checkpoint-at-open and its idempotent replay
  - sync policies and their trade-offs, with numbers from `write_patterns`
  - the `LOCK` file
  - how LevelDB and RocksDB (WAL recovery modes) and PostgreSQL (WAL + full-page writes) do it

  Include *Try it yourself* steps: the crash demo, plus mutation exercises (skip sync; append to the old log instead of a new one).
- [ ] **Step 3:** Write 6–8 interview Q&As, including:
  - "What does fsync guarantee and what doesn't it?"
  - "How do you tell a torn write from corruption?"
  - "What should a database do when fsync fails?"
- [ ] **Step 4:** Update the README: set status to milestone 2, show `Db::open`, and add the crash demo to the commands. Run fmt/clippy/test. Commit `docs: chapter 02 WAL and crash recovery`.
- [ ] **Step 5:** `git push`. Wait for CI (stable on Linux and macOS, plus 1.89) to pass. **STOP** for the user's questions.

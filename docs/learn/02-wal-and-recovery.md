# Chapter 02: The write-ahead log and crash recovery

> **Goal of this chapter:** understand what "durable" really means, why it's so easy to get wrong, and how strata guarantees that a write which returned `Ok` survives a crash. The guarantee covers torn writes, failing disks, and a crash in the middle of recovery itself.
>
> **Code:** [`src/fs/`](../../src/fs/) (the `FileSystem` trait, `OsFs`, `SimFs`) · [`src/wal/`](../../src/wal/) (format, writer, reader) · [`src/batch.rs`](../../src/batch.rs) (encoding) · [`src/db.rs`](../../src/db.rs) (open, write, recovery) · [`tests/recovery.rs`](../../tests/recovery.rs) · [`examples/crash_demo.rs`](../../examples/crash_demo.rs)

---

## 1. What it is

After milestone 1, strata forgot everything when the process exited. Now:

```rust
let db = Db::open("/var/data/mydb", Options::default())?;  // replays the log
db.put(b"k", b"v")?;          // Ok ⇒ survives a crash (with SyncPolicy::Always)
```

A **write-ahead log (WAL)** is an append-only file. Every change is written there *before* it's applied anywhere else. If we crash, we **replay** the log on restart to rebuild the in-memory state.

"Write-ahead" is the whole rule: **log first, then apply.** Anything a reader could ever have seen is already in the log.

---

## 2. Why we need it: what "durable" really means

### The journey of a byte

```
your process ──write()──► OS page cache ──(later, or fsync)──► drive cache ──► stable media
   (RAM)                      (RAM)                              (RAM!)        (flash/platter)
```

- `write()` returning means the bytes are in the **OS page cache**. That's RAM. If the *process* dies (`kill -9`, a panic), they're safe, because the OS still has them. If the *machine* loses power, they're gone.
- `fsync()` asks the OS to push the file's data to the device and wait.
- The drive has its **own** RAM cache. On Linux, `fsync` also asks the drive to flush it. On **macOS, plain `fsync` doesn't**. You need `fcntl(F_FULLFSYNC)`. Rust's `File::sync_all` uses `F_FULLFSYNC` on Apple platforms, which is why a sync costs about 3–4 ms on your Mac (chapter 00).

So there are two kinds of crash, and they lose different things:

| Crash | What survives |
|---|---|
| **Process crash** (`kill -9`, panic, OOM kill) | everything passed to `write()`, even unsynced |
| **System crash** (power loss, kernel panic) | only what was **synced** |

The crash demo below shows the first kind. The `SimFs` tests simulate the second.

### The directory is a file too

Creating a file adds a *name* to a directory. That's an update to the directory's own data, and it can be lost in a power cut **even if the file's contents were synced**. To make a new file's existence durable, you must also sync the **directory** (`FileSystem::sync_dir`). The same goes for deleting or renaming files. Forgetting this is one of the most common durability bugs (`SimFs` test: `unsynced_create_lost_on_crash`).

### Torn writes

A write isn't atomic. A crash in the middle of appending a 4 KB record can leave:

- **a short write:** only the first part of the record reached the disk
- **full length, wrong contents:** the file size was updated, but some data blocks never landed. You see zeros or old garbage.
- **out-of-order sectors:** a later part of the record landed but an earlier part didn't

Our log has to *recognise* all of these as "the record that was being written when we crashed", throw that record away, and keep everything before it.

### When `fsync` fails: "fsyncgate"

In 2018 PostgreSQL developers found that on Linux, when `fsync` reports an I/O error, the kernel may **drop the dirty pages and mark them clean**. A retried `fsync` then *succeeds*, even though the data never reached the disk. PostgreSQL had been retrying, which meant silently losing data. Its fix was to treat an `fsync` failure as fatal: crash, then recover from the WAL. (See also Rebello et al., *"Can Applications Recover from fsync Failures?"*, USENIX ATC 2020.)

The lesson: **after a failed append or sync, you no longer know what's in your log.** strata **poisons** the database. Further writes return `Error::Poisoned`, reads keep working, and you must reopen to recover (§3.6).

---

## 3. How ours works

### 3.1 The `FileSystem` trait, and why it exists

All I/O goes through a small trait with methods like `create`, `append`, `sync`, `sync_dir`, `remove` and `lock`. There are two implementations:

- **`OsFs`:** the real disk.
- **`SimFs`:** in memory. It remembers, per file, how many bytes are **synced**, and per directory, which names are **durable**. `crash()` throws away everything that wasn't synced. `crash_torn(seed)` produces each kind of torn write. `fail_next_append/sync()` injects I/O errors, and `kill_after(n)` makes the "process" die after `n` more operations.

This lets the tests pull the plug thousands of times per second, deterministically. If a test fails with seed 17, it fails with seed 17 every time.

### 3.2 The log format

The log is a series of **32 KiB blocks**. Each block holds **records**:

```
record:  ┌───────────┬────────────┬─────────┬──────────────────────┐
         │ crc32 u32 │ length u16 │ type u8 │ payload (length B)   │
         └───────────┴────────────┴─────────┴──────────────────────┘
           CRC covers type ++ payload; integers little-endian

block (32 KiB):
         ┌────────┬────────┬───────────────────┬──────┐
         │ rec    │ rec    │ rec (First frag.) │ 0000 │ ← < 7 bytes left: zero padding
         └────────┴────────┴───────────────────┴──────┘
         ┌──────────────────────────┬──────────┬──────┐
         │ rec (Last fragment)      │ rec      │ ...  │
         └──────────────────────────┴──────────┴──────┘
```

- **Payload:** one encoded `WriteBatch`: `first_seqno u64 | count u32 | ops…`. The whole batch is one record, so after recovery **a batch is all-or-nothing**.
- **Fragments:** a payload bigger than the rest of the block is split into `First`, `Middle`… and `Last` fragments, with `Full` for a payload that fits.
- **The CRC** (a checksum, from `crc32fast`) detects any damaged byte. It covers the *type* byte too, so a damaged type is caught.
- **Why blocks?** The reader always knows where the next block starts. That lets it look *past* damage to decide what kind of damage it is (§3.4).
- **One `append` call per record:** the writer builds all fragments and padding in memory and calls `append` once. A failed append writes nothing.

### 3.3 The write path

```
write(batch)
  1. validate every key/value          (nothing applied on error)
  2. lock writer; refuse if closed or poisoned
  3. assign seqnos first..=last
  4. WAL append + sync per SyncPolicy  ← failure ⇒ poison, return Err
  5. insert into memtable
  6. publish visible_seqno             (chapter 01)
```

**Sync policies** (`Options::sync`):

| Policy | Guarantee | Cost (your M3 Mac) |
|---|---|---|
| `Always` (default) | `Ok` ⇒ survives power loss | every write waits for a ~3–4 ms full flush: **~230 writes/s** in the crash demo |
| `EveryN(n)` | lose at most `n-1` acknowledged writes | 1 flush per `n` writes |
| `Never` | survives process crashes; power loss can lose recent writes (always a whole-batch prefix) | **~215,000 writes/s** in the crash demo |

That ~940× gap is why **group commit** (milestone 8) matters: many concurrent writers share one sync, giving `Always`-level safety at a fraction of the cost.

### 3.4 Reading the log back: torn tail or corruption?

`read_records` returns every complete record from the front, plus how reading ended:

| Ending | Meaning | What recovery does |
|---|---|---|
| `Clean` | ended exactly at a record boundary (or in zero padding) | continue |
| `TornTail` | the last record is incomplete or damaged, and **nothing valid comes after it** | normal after a crash: drop it and continue |
| `Corrupt` | damage, **followed by a valid record start** | `paranoid_checks: true` → `Error::Corruption`; otherwise keep everything before the damage (point-in-time recovery) |

**Why position decides.** A torn write can produce *any* bytes, including a full-length record with a wrong CRC. So "the CRC failed" alone can't tell a crash apart from disk damage. Position can. A crash only damages the **last** record being written, and we **never append to a log after a restart** (§3.5). So if a valid record *start* (`Full` or `First`) appears after the damage, something damaged data that was already written. That's corruption.

**Why only record *starts* count.** Disks can persist sectors out of order. A crash in the middle of a multi-block record can leave its *first* fragment broken and a *later* fragment intact. That later fragment belongs to the same, torn, last record. Test: `last_record_damaged_early_but_intact_later_is_torn`.

The reader also never panics on garbage. Property tests feed it arbitrary bytes and randomly damaged logs.

### 3.5 Opening: lock, replay, checkpoint

```
open(dir)
  1. lock dir/LOCK            (second open → Error::AlreadyOpen)
  2. list *.log, sort by number, replay each into the memtable,
     keeping every entry's ORIGINAL seqno
  3. checkpoint:
       a. create a NEW log (number = highest + 1)
       b. write the whole recovered memtable into it
       c. sync the log, sync the directory   ← the new log is now durable
       d. delete the old logs, sync the directory
  4. continue sequence numbers from the highest seqno recovered
```

**Why never append to an old log?** Suppose the old log ended in a torn record, and we kept appending to it:

```
000003.log:  [batch 1][batch 2][batch 3 — TORN][batch 4 (new!)][batch 5 (new!)]
                                     ▲
                       next recovery stops here: batches 4 and 5,
                       both acknowledged as durable, are silently lost
```

Starting a fresh log makes that impossible. Test: `torn_tail_then_more_writes_survive`, run for 50 different torn-crash seeds.

**Why is the checkpoint safe to crash in?** Look at each step:

- **Before (c) completes:** the new log might vanish or be partial, but the old logs are untouched. Replay them again.
- **After (c), before (d) completes:** old and new logs both exist and get replayed in order. Every entry keeps its original seqno, so replaying the same record twice inserts the *same* internal key. Replay is **idempotent**.

Test `crash_at_every_step_of_open` uses `kill_after(n)` to kill `open` after 0, 1, 2, … filesystem operations, and checks the data after each one. It also crashes right after a *successful* open. That case caught a real hole during development (§4).

**Temporary design.** Rewriting the whole memtable on every open is fine while everything fits in memory. In milestone 4, step 3 becomes "flush the memtable to an SSTable", which is exactly what LevelDB does on open.

### 3.6 Poisoning

If `append` or `sync` fails, the log may now end in a partial record, or (fsyncgate) the kernel may have discarded the data. If we allowed another write, it would be appended **after** that partial record, acknowledged, and lost on recovery, the same failure as §3.5. So the `Writer` records `poisoned = Some(reason)`:

- the failing write returns `Error::Io`
- every later write returns `Error::Poisoned`
- reads still work
- reopening runs recovery and clears it

### 3.7 The `LOCK` file

Two `Db`s writing the same directory would interleave records and delete each other's logs. `open` takes an exclusive OS lock (`File::try_lock`, stable since Rust 1.89, hence our minimum Rust version) on `dir/LOCK`. `close` (or dropping the `Db`) releases it, and so does the OS if the process dies.

---

## 4. How we know it works

| Test | What it proves |
|---|---|
| `acknowledged_writes_survive_crash` | `Always` + power loss: all 200 acknowledged batches present |
| `never_policy_recovers_a_prefix` (50 torn seeds) | without sync you lose writes, but always a **prefix of whole batches** |
| `torn_tail_then_more_writes_survive` (50 seeds) | the never-append-to-old-logs rule |
| `torn_tail_opens_in_paranoid_mode` (50 seeds) | ordinary crashes are never reported as corruption |
| `mid_log_corruption` | real damage: paranoid refuses; default keeps the prefix |
| `crash_at_every_step_of_open` | recovery is safe to interrupt anywhere |
| `old_and_new_logs_together_replay_same_state` | replay is idempotent |
| `failed_append_poisons`, `failed_sync_poisons` | fsyncgate handling |
| model test with `Crash` / `Reopen` ops | random operation histories, with crashes anywhere, match a `BTreeMap` |

**Mutation checks done while building:** we broke the code on purpose and confirmed the tests catch it.

| Planted bug | Caught by |
|---|---|
| `SyncPolicy::Always` doesn't actually sync | model test, shrunk to the minimal case `[Put("a", ""), Crash]` |
| checkpoint doesn't sync the new log | `crash_at_every_step_of_open`, but **only after we fixed the test**. The first version never checked "crash right after a successful open", so this bug passed. Mutation testing found the gap in the test. |
| failed writes don't poison | both poisoning tests |

---

## 5. Alternatives we rejected, and why

| Choice | Alternative | Why not |
|---|---|---|
| Fresh log on every open | Keep appending to the last log | Appending after a torn tail loses acknowledged writes (§3.5). LevelDB's `reuse_logs` option does this carefully, by first truncating to the last good record. |
| Classify damage by position | Treat every CRC mismatch as corruption | Torn writes produce full-length records with bad CRCs, so paranoid mode would refuse to open after every ordinary power cut |
| Poison after a failed sync | Retry the sync | fsyncgate: the retry can "succeed" after the data is gone |
| 32 KiB blocks with fragments | Length-prefixed records with no blocks | Without block boundaries the reader can't skip past damage to see whether valid data follows it |
| `crc32fast` (CRC-32, IEEE) | CRC-32C (hardware-accelerated, LevelDB's choice) | Equivalent error detection. `crc32fast` is a tiny, ubiquitous dependency. |
| `Always` by default | `Never` by default (RocksDB's default) | For a learning engine, the safe default is the right default. You opt into speed and should know what you're giving up. |

---

## 6. How real systems do it

- **LevelDB**: the same 32 KiB block format with `FULL/FIRST/MIDDLE/LAST` fragments and a 7-byte header. On open it replays the logs and writes the recovered memtable to a level-0 table, our checkpoint's future (M4). It takes a `LOCK` file.
- **RocksDB**: the same format, plus **WAL recovery modes**:
  - `kTolerateCorruptedTailRecords`
  - `kAbsoluteConsistency` (like our paranoid mode)
  - `kPointInTimeRecovery`, the default and like ours
  - `kSkipAnyCorruptedRecords`

  Its default is `sync = false`, with group commit for applications that want sync.
- **PostgreSQL**: WAL plus **full-page writes**. After each checkpoint, the first change to a data page logs the *whole page*, so a torn 8 KiB page write can be repaired. Since 2018–2019 it PANICs on `fsync` failure.
- **SQLite**: either a rollback journal (copy old pages first) or WAL mode. It documents many of these pitfalls in *"Atomic Commit in SQLite"* and *"How To Corrupt An SQLite Database File"*.
- **etcd / Raft logs** (project 2): every entry is checksummed and synced before it's acknowledged to the leader.

---

## 7. Try it yourself

1. **Kill a real writer.**
   ```bash
   cargo run --release --example crash_demo -- write /tmp/strata-demo always
   # in another terminal, after a second or two:
   kill -9 <pid printed on the first line>
   cargo run --release --example crash_demo -- verify /tmp/strata-demo <last "acked" number>
   ```
   Then repeat with `never` in a new directory. It passes too, but why? (§2: process crash vs system crash.) Which `SimFs` test shows what `never` would lose on a power cut?
2. **Measure the policies.** Compare how many writes each policy manages before you kill it. Where does the ~940× gap come from?
3. **Remove poisoning.** In `db.rs`, delete `w.poisoned = Some(e.to_string());`. Run `cargo test --test recovery`. Read the failing assertion, and explain the data-loss scenario in your own words. Then revert (`git checkout src/db.rs`).
4. **Skip the checkpoint sync.** In `checkpoint()`, delete `wal.sync()?;`. Which test fails, and at which crash point?
5. **Appending after a torn tail.** The safest way to see the bug is on paper. Draw `000003.log` after a `crash_torn`, then imagine `open` appending to it instead of creating `000004.log`. Find the record where `read_records` would stop.
6. **Torn vs corrupt.** In `src/wal/mod.rs`, read `damage_then_record_starting_mid_block_is_corrupt` and `last_record_damaged_early_but_intact_later_is_torn`. Explain why the first is corruption and the second isn't.

## Check yourself

- What's the difference between `write()` returning and `fsync()` returning? What does `F_FULLFSYNC` add on macOS?
- Why must the *directory* be synced after creating a log file?
- Give three shapes a torn write can take. Why can't a CRC alone tell a torn write from corruption?
- Why does strata never append to a log that existed before `open`?
- Why is it safe to crash in the middle of `open`?
- What should a database do after `fsync` fails, and why not retry?

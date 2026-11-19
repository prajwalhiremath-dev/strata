# Chapter 01: Internal keys, the memtable, and atomic batches

> **Goal of this chapter:** understand how strata keeps *many versions* of the same key, how a read finds the right one, why a delete is a write, and how a batch of writes becomes visible all at once. You'll also see how a model-based test checks all of it.
>
> **Code:** [`src/key.rs`](../../src/key.rs) · [`src/memtable.rs`](../../src/memtable.rs) · [`src/db.rs`](../../src/db.rs) · [`src/batch.rs`](../../src/batch.rs) · [`tests/db_basic.rs`](../../tests/db_basic.rs) · [`tests/model.rs`](../../tests/model.rs)

---

## 1. What it is

After this milestone strata is a working, **in-memory** key-value store:

```rust
let db = Db::open_in_memory(Options::default())?;
db.put(b"user:1", b"alice")?;
db.delete(b"user:1")?;
let mut batch = WriteBatch::new();
batch.put(b"x", b"1").put(b"y", b"1");
db.write(batch)?;                       // x and y appear together, or not at all
let rows = db.scan(Unbounded, Unbounded)?;
```

Nothing touches disk yet. That's milestone 2. But the core *ideas* are the final ones, and every later layer builds on them:

1. **Internal keys:** every write is stamped with a sequence number.
2. **Newest-first ordering**, so the first match is the answer.
3. **Tombstones:** delete is a write.
4. **Publish-after-apply:** batches appear atomically.

---

## 2. Why we need it

### Problem 1: the same key has many versions

In an LSM nothing is updated in place (chapter 00). After `put(k, v1)`, `put(k, v2)`, `delete(k)`, all three facts exist somewhere: in the memtable, or later in different files on disk. We need a rule for **which one wins**.

The rule is **the newest one wins**. To know which is newest, every write gets a number from a global counter: the **sequence number** (seqno). It goes 1, 2, 3… and never repeats.

### Problem 2: deleting from things you can't modify

A value already flushed to an immutable file on disk can't be removed from that file. So a delete must be a *new fact* that's newer than the value: a **tombstone** (`kind = Delete`). Readers that see the tombstone first report "not found".

### Problem 3: readers running during writes

A batch `{x=5, y=5}` is applied one entry at a time. A reader that runs in the middle must not see `x=5, y=old`. Applications rely on batches for things like "debit account A *and* credit account B".

---

## 3. How ours works

### 3.1 The internal key

The user sees `key → value`. Internally we store `(user_key, seqno, kind) → value`:

```rust
pub struct InternalKey { user_key: Vec<u8>, seqno: u64, kind: ValueKind }
pub enum ValueKind { Delete = 0, Put = 1 }
```

**Sort order:** user key ascending, then **seqno descending**, then kind descending.

After `put(b, 1)`, `put(a, 2)`, `put(b, 3)`, `delete(b)`, the memtable holds (seqnos 1–4):

```
  internal key (user_key, seqno, kind)    value
  ─────────────────────────────────────   ─────
  ("a", 2, Put)                           "2"
  ("b", 4, Delete)                        ""      ← newest version of b comes FIRST
  ("b", 3, Put)                           "3"
  ("b", 1, Put)                           "1"
```

**On-disk encoding** (used from milestone 2):

```
  ┌──────────────────┬─────────────────────────────────────────┐
  │ user_key bytes   │ 8-byte trailer, little-endian u64:      │
  │                  │   (seqno << 8) | kind                   │
  └──────────────────┴─────────────────────────────────────────┘
```

The seqno has 56 bits. The low 8 bits hold the kind. 2⁵⁶ ≈ 7×10¹⁶ writes is more than enough: at a million writes per second, that's over 2,000 years.

`compare_encoded()` compares two *encoded* keys without decoding them: user key bytes first, then the trailer **reversed**. Seqno sits in the high bits of the trailer, so reversing the trailer comparison gives "seqno descending, then kind descending" in a single integer comparison. A property test (`encoded_order_matches_struct_order`) checks thousands of random key pairs to confirm that the byte comparison and the struct ordering always agree. If they ever disagreed, files on disk would sort differently from the memtable, a very nasty bug.

### 3.2 Why seqno is *descending* (the key design decision)

Look at `Memtable::get`:

```rust
let seek = InternalKey::new(user_key, snapshot, ValueKind::Put);
map.range(seek..).next()   // first entry at or after the seek key
```

Because versions of one key are sorted newest-first, **"the first entry at or after `(k, snapshot)`" is exactly "the newest version of k that's not newer than the snapshot".** That's one seek with no loop.

Get `"b"` with snapshot 3, using the table above:

```
  ("a", 2, Put)
  ("b", 4, Delete)     ← skipped: seek key (b, 3, Put) sorts AFTER this (4 is newer than 3)
▶ ("b", 3, Put) "3"    ← first entry ≥ seek key, user_key matches → Found("3")
  ("b", 1, Put)
```

With snapshot 4 (the current state), the seek lands on `("b", 4, Delete)`, which gives `Deleted`. That's correct.

**What breaks if seqno were ascending?** Versions would sort `b1, b3, b4`. To find the newest visible version you'd have to scan *all* versions of `b` and keep the last one ≤ snapshot. That costs O(versions) per lookup instead of one seek. Later, when we merge sorted files (milestone 6), "newest first" also means "the first entry for each key wins, skip the rest". Merging stays simple.

**Why does the seek key use `Put` and not `Delete`?** Kind sorts descending, so `Put (1)` sorts *before* `Delete (0)` at the same seqno. `(k, snapshot, Put)` is therefore the smallest possible internal key for `(k, snapshot)`, and the seek starts before *any* entry with that seqno. (In practice one seqno has only one entry. The rule just guarantees we never skip one.)

### 3.3 Lookup has three answers, not two

```rust
enum Lookup { Found(value), Deleted, NotFound }
```

Why not `Option<value>`? Because from milestone 4 on, a read continues to older data (disk) when the memtable has *nothing* for the key. If the memtable has a *tombstone*, the read must **stop**: the key is deleted, even if an old value sits in an SSTable. Merging `Deleted` and `NotFound` into `None` would bring deleted data back from the dead. This is a classic LSM bug, and naming it in the type makes it hard to write.

### 3.4 The memtable: `BTreeMap` behind a lock

```rust
struct Memtable { map: RwLock<BTreeMap<InternalKey, Vec<u8>>>, size: AtomicUsize }
```

- **Sorted:** `BTreeMap` keeps keys ordered, which we need for range scans and for writing sorted files later.
- **`RwLock`:** many readers *or* one writer at a time.
- **`approximate_size`:** a running estimate (key + value + 16 bytes overhead per entry) that will decide when the memtable is "full" and must be flushed (milestone 4).

**Scan** walks forward from the start bound and stops at the end bound *itself*. We don't hand both bounds to `BTreeMap::range`, because that method **panics** if start > end. A user calling `scan("d", "b")` should get an empty result, not a crash (test: `scan_bound_kinds`). The bounds are mapped like this:

| User bound | Internal bound | Why |
|---|---|---|
| `Included(k)` start | `(k, MAX_SEQNO, Put)` | smallest internal key for k, so every version of k is included |
| `Excluded(k)` start | after `(k, 0, Delete)` | largest internal key for k, so every version of k is skipped |
| end bounds | compared on the user key directly | simpler, and avoids the panic |

While scanning, entries newer than the snapshot are skipped. After the first (newest) visible entry for a key, older versions are skipped too.

### 3.5 Writes: assign seqnos, apply, then *publish*

`Db::write` does this:

```
1. validate every key (empty or > 65,535 bytes → InvalidArgument; nothing applied)
2. lock the write mutex                     ← one writer at a time
3. seqnos first..=last for the batch's ops  ← later op in batch wins on same key
4. (milestone 2: append to the WAL here)
5. insert each op into the memtable
6. visible_seqno.store(last, Release)       ← PUBLISH
```

Readers do:

```
snapshot = visible_seqno.load(Acquire)      ← once, at the start
read memtable, ignoring entries with seqno > snapshot
```

**Why this makes batches atomic:** while steps 5–6 are in progress, the batch's entries are already in the map, but their seqnos are *greater than `visible_seqno`*, so every reader skips them. Step 6 makes the whole batch visible in **one atomic store**. There's no moment where half of it is visible.

**Why `Release` / `Acquire`?** Modern CPUs and compilers may reorder memory operations. Release/Acquire is a promise: *if a reader sees the value stored with `Release`, it also sees everything the writer did before that store.* Here the map is also protected by its `RwLock`, which provides its own ordering. But relying on that would be an accident of the current design. From milestone 8 we may move to a lock-free memtable, where Release/Acquire is the *only* thing that makes this correct. Using the right ordering now documents the real contract: **"seqno N is visible" implies "all writes ≤ N are visible."**

**Why the write mutex?** Seqnos must be assigned in the same order the batches are applied and published. Otherwise batch 2 could publish `visible_seqno = 10` while batch 1 (seqnos 5–7) is still inserting, which would expose a half-applied batch 1. One writer at a time is the simplest correct answer. Milestone 8's **group commit** keeps this ordering but lets one leader write many batches together.

### 3.6 Validation and closing

- **Empty keys are rejected.** An empty key is almost always a caller bug, and rejecting it keeps later formats simple.
- **Keys longer than 65,535 bytes are rejected** (`MAX_KEY_LEN`). The SSTable format in milestone 3 stores key lengths in 2 bytes.
- **The whole batch is validated before anything is applied.** One bad key means nothing from the batch is written (`invalid_key_rejects_whole_batch`).
- **An empty value is not the same as a missing key.** `put(k, b"")` then `get(k)` returns `Some([])` (`empty_value_is_some`). Tombstones are marked by `kind`, never by an empty value.
- **After `close()`** every call returns `Error::Closed`. Closing twice is fine.

---

## 4. Testing: how we know it's right

### Unit and edge-case tests (`tests/db_basic.rs`)

These cover one behaviour each: overwrite, delete-then-reput, empty value, limits, closed DB, and batch semantics.

### The concurrency test: `batch_is_atomic_to_readers`

One writer applies 1,000 batches, each setting `x` and `y` to the same value. Four reader threads scan continuously and assert `x == y`. Any half-applied batch shows up as `x != y`.

### Model-based testing (`tests/model.rs`)

Unit tests check the cases *you thought of*. A model test checks the cases *you didn't*.

1. **The model** is something obviously correct: here, a `BTreeMap<key, value>`.
2. `proptest` generates **random sequences** of 1–200 operations (`Put`, `Delete`, `Get`, `Scan`, `Batch`) over a tiny key alphabet (`a`–`e`, 1–3 bytes), so keys collide often.
3. Each op runs against **both** strata and the model. Every read must agree.
4. If something fails, proptest **shrinks** the sequence to the smallest one that still fails.

Every later milestone adds operations to the same test (`Flush`, `Reopen`, `Crash`…). As the engine gets complicated, it's always checked against the same simple truth.

### Proving the tests can fail

A test that has never failed might not be testing anything. During development we planted two bugs on purpose:

| Planted bug | Caught by | What the failure looked like |
|---|---|---|
| Store deletes as `Put` with an empty value | model test | proptest shrank a random 200-op run down to just `[Delete("a")]`: strata returned `("a", [])` from a scan, and the model returned nothing |
| Publish `visible_seqno` after *each op* instead of after the batch | `batch_is_atomic_to_readers` | `saw a half-applied batch`, 3 out of 3 runs |

You'll reproduce both in *Try it yourself*.

---

## 5. Alternatives we rejected, and why

| Choice | Alternative | Why we didn't (yet) |
|---|---|---|
| `BTreeMap` + `RwLock` memtable | **Skiplist** (LevelDB, RocksDB, Pebble) | A concurrent skiplist lets readers proceed *without any lock* while a writer inserts, and allocates from an arena for speed. It's also much harder to get right in Rust: it needs unsafe code or crossbeam's epoch GC. We start simple and correct, and measure in milestone 10 before optimising. |
| Seqno in the key | Timestamps | Clocks go backwards and two writes can share a timestamp. A counter under the write lock is strictly increasing and unique. |
| Tombstones | Remove from the memtable | Removing from memory doesn't remove the older value on disk, which would reappear. Tombstones are the only option once data is immutable. |
| Global write mutex | Lock-free seqno via `fetch_add` | Two writers could then insert out of order, and publishing correctly would need extra machinery. Group commit (milestone 8) is the standard way to get concurrency here. |
| `Vec<(k, v)>` results from `scan` | A streaming iterator | Simpler for now. Merging iterators over many files arrive in milestone 6, and `scan` will become an iterator then. |

---

## 6. How real systems do it

- **LevelDB** uses exactly this internal key idea: `user_key + 8-byte trailer = (sequence << 8) | type`, with a 56-bit sequence, `kTypeDeletion = 0`, `kTypeValue = 1`, and a comparator that sorts the sequence descending. Its memtable is a skiplist in an arena, and lookups seek to a "LookupKey" built from the user key and snapshot. That's the same trick as our `seek` key.
- **RocksDB** keeps the same format and adds alternative memtables (skiplist by default, plus hash-based and vector variants for special workloads) and concurrent memtable inserts.
- **Pebble** (CockroachDB) uses an arena-based lock-free skiplist with the same trailer layout.
- **PostgreSQL** solves the "many versions" problem differently. Each row version carries the transaction IDs that created and deleted it (`xmin`/`xmax`), and `VACUUM` cleans up dead versions. That's the same MVCC idea, applied to a heap file instead of an LSM.

---

## 7. Try it yourself

1. **Run all tests:** `cargo test`.
2. **Break deletes and watch the model test shrink the failure.** In `src/db.rs`, inside `write`, change `ValueKind::Delete` in the `BatchOp::Delete` branch to `ValueKind::Put`. Run `cargo test --test model`. Read the "minimal failing input". Why is that the smallest possible failing case? Then revert with `git checkout src/db.rs` and delete `tests/model.proptest-regressions`.
3. **Break atomicity.** In the loop inside `write`, right after `let seqno = first + i as u64;`, add `self.inner.visible_seqno.store(seqno, Ordering::Release);`. Run `cargo test --test db_basic batch_is_atomic`. Explain *exactly* which interleaving of writer and reader produced the failure. Revert.
4. **Break the ordering.** In `key.rs`, change `other.seqno.cmp(&self.seqno)` to `self.seqno.cmp(&other.seqno)` (ascending). Which tests fail, and why? (Hint: read §3.2 again.) Revert.
5. **Predict, then check.** After `put(a,1)`, `put(a,2)`, `delete(a)`, `put(a,3)`, write down the memtable's internal keys in sorted order. Then add a temporary `println!("{:?}", map)` in `Memtable::get` to verify.

## Check yourself

- Why is seqno sorted *descending*? What would `get` need to do otherwise?
- Why does `Lookup` have `Deleted` *and* `NotFound`? What bug would merging them cause later?
- Walk through a reader that runs while a 2-key batch is half-inserted. Why doesn't it see the first key?
- What does `Release`/`Acquire` guarantee, in one sentence?
- What's a model-based test, and what does "shrinking" give you?

If any answer feels shaky, ask before milestone 2 (WAL + crash recovery).

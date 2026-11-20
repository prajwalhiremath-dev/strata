# Chapter 03: SSTables, the sorted files on disk

> **Goal of this chapter:** understand how an LSM engine stores sorted data on disk so that a lookup reads one small block instead of the whole file, how keys are compressed without losing the ability to binary-search, and how a reader protects itself from a damaged file.
>
> **Code:** [`src/sstable/block.rs`](../../src/sstable/block.rs) (blocks, prefix compression, restart points) · [`src/sstable/format.rs`](../../src/sstable/format.rs) (handles, trailer, footer) · [`src/sstable/builder.rs`](../../src/sstable/builder.rs) · [`src/sstable/table.rs`](../../src/sstable/table.rs) (reader) · [`src/sstable/tests.rs`](../../src/sstable/tests.rs) · [`src/coding.rs`](../../src/coding.rs) (varints) · [`examples/sst_layout.rs`](../../examples/sst_layout.rs)

---

## 1. What it is

An **SSTable** ("sorted string table") is a file of key-value entries **sorted by key**, written once and **never modified**. In strata the keys are the internal keys from chapter 01, `user_key ++ (seqno, kind)`, so one file can hold several versions of the same user key, plus tombstones.

```rust
let mut b = TableBuilder::new(fs.create(path)?, TableOptions::default())?;
for (key, value) in memtable_entries_in_order {
    b.add(&key, &value)?;           // must be strictly increasing
}
let meta = b.finish()?;             // writes index + footer, syncs the file

let table = Table::open(fs.open_random(path)?)?;
let mut it = table.iter();
it.seek(&target)?;                  // first entry >= target
```

Where this fits: right now the memtable only lives in memory and the WAL (chapter 02) grows forever. From milestone 4, a full memtable gets written out as one of these files. That's a **flush**. Its WAL can then be deleted. This chapter builds and proves the file format on its own first.

---

## 2. Why it's built this way

### Why immutable?

Once `finish` returns, nobody writes to the file again. That one rule buys a lot:

- **No locks for readers.** A file that can't change can be read by any number of threads with no coordination.
- **No torn in-place updates.** Chapter 02 showed how hard it is to make *appends* crash-safe. Overwriting data in place, as a B-tree does, is harder still: PostgreSQL needs full-page writes for it. An SSTable is either completely written and synced, or it's garbage we never point to.
- **Simple cleanup.** When compaction (milestone 7) merges files, the old ones are deleted whole. There's no free-space map inside a file.
- **Caching is easy.** A cached block can never go stale.

The price is that updates and deletes don't change old files. They write *new* versions to newer files, and compaction later throws the old versions away. That's the "log-structured" in LSM.

### Why blocks?

A table can be megabytes or gigabytes. To find one key we don't want to read it all, and we don't want one disk read per key either. So entries are grouped into **blocks** of about 4 KiB, the size of a page in the OS page cache and of a typical SSD read. A lookup:

1. uses an in-memory **index** to find the one block that could hold the key,
2. reads that block with **one** read,
3. searches inside it.

---

## 3. How ours works

### 3.1 The layout of one file

```text
offset 0
┌────────────────────────┐
│ data block 0           │  sorted entries, prefix-compressed
│ crc32                  │  4 bytes
├────────────────────────┤
│ data block 1           │
│ crc32                  │
├────────────────────────┤
│ ...                    │
├────────────────────────┤
│ index block            │  one entry per data block: last key -> (offset, size)
│ crc32                  │
├────────────────────────┤
│ footer (48 bytes)      │  where the index is, version, crc, magic
└────────────────────────┘
end of file
```

The reader starts at the **end**: the footer has a fixed size, so it's always the last 48 bytes. The footer says where the index is. The index says where each data block is.

> **Changed in milestone 4.** A filter block now sits between the last data block and the index, and the footer is 56 bytes. Section 3.8 has the details.

### 3.2 Varints

Most lengths in a block are small: a key of 21 bytes, a value of 16. Storing each as a 4-byte `u32` would waste space, so we use **varints** (LEB128). Each byte carries 7 bits of the number, and its high bit means "more bytes follow":

```text
      5 -> 05                    (1 byte)
    300 -> ac 02                 (2 bytes: 0101100 + 0000010<<7)
u32::MAX -> ff ff ff ff 0f       (5 bytes, the most a u32 can need)
```

The decoder treats its input as hostile. Too few bytes, more bytes than the type can need, or bits beyond the type's width are all `Corruption`.

### 3.3 Inside a block: prefix compression

Sorted keys share long prefixes. Here are three neighbouring internal keys from the experiment:

```text
user:00000000 + trailer
user:00000001 + trailer
user:00000002 + trailer
```

Each **entry** stores only what's new compared with the previous key:

```text
entry = shared | non_shared | value_len | key bytes [shared..] | value
        varint   varint       varint
```

The experiment's hex dump shows exactly this:

```text
shared=0  non_shared=21 value_len=16  key bytes stored: 75 73 65 72 3a 30 ... 30 01 01 00 00 00 00 00 00
shared=12 non_shared=9  value_len=16  key bytes stored: 31 01 01 00 00 00 00 00 00
shared=12 non_shared=9  value_len=16  key bytes stored: 32 01 01 00 00 00 00 00 00
```

The second key shares `user:0000000` (12 bytes) with the first, so it stores only `1` plus its 8-byte trailer: 9 bytes instead of 21. To rebuild a key, keep the previous key's first `shared` bytes and append the stored bytes.

### 3.4 Restart points: compression *and* binary search

Prefix compression has a catch. To rebuild key #500 you need key #499, which needs #498, and so on back to the start of the block. That's no good for searching.

The fix is **restart points**. Every `restart_interval`-th entry (16 by default) stores its key **in full** (`shared = 0`), and its offset goes into an array at the end of the block:

```text
entry 0 (full) | e1 | e2 | ... | e15 | entry 16 (full) | e17 | ... | restart[0]=0 | restart[1]=off16 | ... | count
```

A seek inside a block works in two steps:

1. **Binary-search the restart points.** Each one has a full key, so it can be decoded on its own. Find the last restart whose key is < target.
2. **Walk forward** from there, rebuilding keys, to the first key ≥ target. That's at most 16 entries.

So the restart interval is a dial:

| interval | space | seek cost inside a block |
|---|---|---|
| 1 | every key stored in full, no compression | pure binary search |
| 16 | most keys compressed | binary search, then ≤ 16 steps |
| 64 | slightly smaller still | binary search, then ≤ 64 steps |

### 3.5 Seeks use internal-key order, not byte order

This is the subtle part. Every comparison in the table code goes through `key::compare_encoded`, never plain `<` on bytes. Why? Take two versions of key `k`:

```text
(k, seqno 9)  ->  6b | 01 09 00 00 00 00 00 00     (trailer = 9<<8 | Put, little-endian)
(k, seqno 5)  ->  6b | 01 05 00 00 00 00 00 00
```

Internal order puts seqno 9 **first**, because the newest version comes first (chapter 01). Byte order puts it second. A seek that used byte order would land on the wrong version, and a `get` at a snapshot would return stale data. The test `seek_uses_internal_order` pins this, and the mutation check (swap in byte order) makes four tests fail.

### 3.6 The index block: one entry per data block

The index is itself a block with the same format, using restart interval 1. For each data block it stores:

```text
key   = the LAST internal key in that data block
value = BlockHandle { offset, size }   (two varints)
```

Why the *last* key? Seeking the index to the target finds the first entry whose key is ≥ target. That's the first block whose last key is ≥ target, and therefore the first block that can contain anything ≥ target. Had we stored the *first* key, the seek would land one block too late. The mutation check that does exactly that breaks five tests.

The table iterator is two cursors stacked: the index cursor picks a block, and a block cursor walks inside it. When `next` reaches the end of a block, the iterator advances the index cursor and starts at the first entry of the next block. That's how `versions_straddle_blocks` passes when 200 versions of one key span dozens of blocks: a lookup lands in the right block, and a scan flows across block boundaries without noticing them.

### 3.7 A point lookup at a snapshot

`get(user_key, snapshot)`:

1. Build the target `(user_key, snapshot, Put)`. In internal order it sits just before every version of `user_key` that the snapshot can see, and after every newer one.
2. `seek(target)` gives the first entry ≥ target.
3. If that entry's **user key** equals `user_key`, it's the newest visible version: return `Found(value)` for a Put, or `Deleted` for a tombstone. Otherwise the key isn't in this table, so return `NotFound`.

Step 3's equality check matters: without it, `get("aa")` would happily return the value of `"ab"`. Like the memtable, a table answers `Deleted` and `NotFound` differently. From milestone 6 a `Deleted` stops the search, while a `NotFound` means "look in older tables".

### 3.8 Checksums, and a footer that checks itself

Every block, the index included, is followed by a CRC32 of its contents. The reader verifies it on **every** block read, before decoding anything.

The footer is 48 fixed bytes:

```text
index_offset u64 | index_size u64 | filter_offset u64 | filter_size u64 | version u32 | crc32 u32 | magic u64
```

- The **magic** (`strataT1`) answers "is this a strata table at all?"
- The footer's own **CRC** covers the four handles and the version. Without it, a flipped bit in `index_offset` would send us to read garbage.
- The **filter handle** is reserved for milestone 4's bloom filter. It's zero for now, and the reader rejects anything else, so no byte of the file goes unchecked. Reserving it now means M4 won't need a new file format.

> **Changed in milestone 4: that last sentence turned out wrong.** The filter itself did fit in the reserved handle. But M4 also needs each table to record its highest sequence number (`max_seqno`). Once flushing deletes the logs, the footer is the only place `open` can learn where the sequence counter stands. That's a new field, so the footer became format **version 2**, 56 bytes:
>
> ```text
> index handle (16) | filter handle (16) | max_seqno u64 | version u32 = 2 | crc32 (over 0..44) | magic
> ```
>
> The filter handle is now either `(0, 0)` (no filter) or points at a real block with its own CRC. Version 1 tables are rejected with "unsupported format version 1". The lesson: reserving a slot only saves a format change for the feature you predicted. A version number is what saves you from the ones you didn't. Chapter 04 covers the filter and `max_seqno`.

Every byte of a table is therefore covered by a CRC or a fixed value. `every_byte_flip_is_detected` proves it: it flips each byte of a real table in turn (about 6,400 positions) and requires that opening plus a full scan returns an error, every time.

### 3.9 Never trust a length from disk

A damaged file can contain *any* number where a length should be. If the reader did `vec![0; size]` with a garbage `size` of 2⁶³, the process would abort. So, before any read or allocation:

- every block handle is checked with `checked_add` to lie entirely inside the file,
- the restart count is checked against the block's length,
- each restart offset must be inside the data and increasing,
- each entry's `shared` must be ≤ the previous key's length, and its key and value must fit in what's left.

All of these return `Corruption`. A property test feeds random bytes, sometimes with a valid footer pointing into them, to the reader, and nothing may panic.

### 3.10 A cursor, not an `Iterator`

`TableIter` has `seek`, `next`, `valid`, `key` and `value`, like LevelDB's iterators, rather than implementing Rust's `Iterator`. Two reasons:

- moving can **fail** (a block read, a bad checksum), so `next` returns `Result`;
- `key()` and `value()` **borrow** the current entry instead of copying it, which an `Iterator` can't do.

Milestone 6 builds merging iterators on top of this interface.

---

## 4. How we know it works

- **The memtable is the oracle.** `table_matches_memtable` applies random puts and deletes over a tiny key alphabet (so there are many versions per key) to a memtable, builds a table from it with a random block size (16–512 bytes) and restart interval (1–8), then checks:
  - a full scan returns exactly the memtable's entries,
  - `table.get` equals `memtable.get` at every snapshot where the answer can change,
  - random seeks land where a lower-bound search does.
- **Every damaged byte is caught**: see §3.8.
- **Hostile input never panics**: random blocks and random files.
- **Edge cases, pinned:** an empty table, a 10 KiB value with 4 KiB blocks, seeks before, between and after keys across block boundaries, and 200 versions of one key spanning blocks.
- **Mutation checks.** I broke the code on purpose and confirmed the tests notice:

  | mutation | caught by |
  |---|---|
  | skip the CRC check | `every_byte_flip_is_detected`, `block_trailer_round_trip` |
  | index stores each block's *first* key | `get_basic`, `huge_entry`, `seek_edges`, `versions_straddle_blocks`, `table_matches_memtable` |
  | `get` skips the user-key equality check | `get_basic`, `table_matches_memtable` |
  | block seek uses byte order | `seek_uses_internal_order`, `get_basic`, `versions_straddle_blocks`, `table_matches_memtable` |

---

## 5. Alternatives we rejected, and why

- **One sorted file without blocks** (binary search over the whole file): each probe is a random disk read, about 20 for a million keys. With blocks it's one read plus in-memory work.
- **Fixed-width lengths (`u32`) instead of varints**: simpler, but 3 wasted bytes per field on every entry.
- **No prefix compression**: interval 1 in the experiment. The file is 51% bigger than at interval 16.
- **Implementing `Iterator`**: see §3.10.
- **A checksum per entry**: more overhead, and a block is read whole anyway, so one CRC per block is the natural unit.

---

## 6. How real systems do it

What production engines add on top of this design:

- **Compression per block** (Snappy, LZ4, Zstd). That's why LevelDB's trailer has a compression-type byte before the CRC. We left it out.
- **A block cache.** Recently read blocks stay in memory, uncompressed, so hot lookups skip the read and the CRC. Our experiment's ~4 µs per lookup is almost entirely the read, checksum and allocation that a cache would remove.
- **Shorter index keys.** LevelDB doesn't store the full last key. It stores the *shortest* key between this block's last key and the next block's first key, e.g. `user:00000140` / `user:00000141` → `user:0000014`. That's a smaller index for free.
- **Partitioned indexes** (RocksDB). For huge files the index itself is split into blocks with a top-level index, so it doesn't all have to sit in memory.
- **Bloom filters** in the filter block, so a lookup for a missing key usually skips the data block read entirely. That's milestone 4.
- **Format versions.** RocksDB's block-based table format has evolved through several versions. A version field in the footer is what makes that possible.

---

## 7. Try it yourself

1. **Run the experiment.**
   ```bash
   cargo run --release --example sst_layout
   ```
   On an Apple M3 Mac (APFS, 100,000 keys, 4 KiB blocks):

   | interval | file bytes | vs interval 1 | blocks | ns per lookup (3 runs) |
   |---|---|---|---|---|
   | 1 | 4,444,731 | baseline | 1076 | 3,930 – 5,222 |
   | 4 | 3,237,384 | −27.2% | 782 | 3,745 – 5,214 |
   | 16 | 2,941,888 | −33.8% | 710 | 3,570 – 6,119 |
   | 64 | 2,872,647 | −35.4% | 695 | 5,514 – 7,191 |

   Most of the saving comes by interval 16, and going to 64 saves only 1.6% more while making every lookup slower. Up to 16, the differences in lookup time are within run-to-run noise, because the cost is dominated by reading and checksumming a 4 KiB block. That's what a block cache attacks.
2. **Read the hex dump** the experiment prints. Find the shared prefix length and the trailer in each entry.
3. **Break the index.** In `builder.rs`, make `flush_data_block` record the block's *first* key instead of its last. (You'll need to remember it in `add`.) Run `cargo test --lib sstable::` and read why `seek_edges` fails.
4. **Use byte order.** In `block.rs`'s `seek`, replace `compare_encoded(k, target) == Ordering::Less` with `k < target`. Which tests fail? Work out by hand where `seek(k@6)` lands among `k@9, k@5, k@1`.
5. **Count what's checked.** Pick any byte offset in the footer and say which check catches a flip there.

## Check yourself

- Why are SSTables immutable, and what does that buy readers and compaction?
- How does a restart point let you binary-search prefix-compressed keys? What does the restart interval trade?
- Why does the index store each block's *last* key?
- Walk through `get(user_key, snapshot)`. Why seek to `(user_key, snapshot, Put)`, and why check the user key afterwards?
- Why must seeks use internal-key order? Give an example where byte order is wrong.
- Name three things a reader must check before trusting a length read from the file.

# Interview notes 02: Durability, WAL and crash recovery

Answer each one out loud before reading the model answer.

---

### 1. What does `fsync` guarantee, and what doesn't it?

**It guarantees:** when it returns successfully, the file's data and the metadata needed to read it (like its size) are on stable storage. On Linux that includes flushing the drive's write cache. On macOS you need `F_FULLFSYNC` for that.

**It doesn't guarantee:**
- that the *directory entry* is durable. A new file needs an `fsync` of its directory.
- any ordering between files.
- atomicity of a write. A crash during a write can tear it.
- that a *retry* after a failure means anything (see question 4).

---

### 2. What is a write-ahead log, and why "write-ahead"?

An append-only file where every change is recorded *before* it's applied to the main data structures. After a crash, replaying the log rebuilds any state that wasn't yet persisted elsewhere.

"Ahead" is the ordering rule: the log record must be durable before the change becomes visible or acknowledged. Otherwise a crash could lose something a client or reader already saw.

---

### 3. How do you tell a torn write from corruption?

By **position**, not by the checksum alone. A torn write can produce a full-length record with a bad CRC, so the CRC can't tell them apart.

A crash only damages the *last* record being written, as long as you never append to a log after recovery. So if damage is followed by a valid record start, the damage was to data already written: that's corruption. If nothing valid follows, it's a torn tail, and you drop it.

Be careful with multi-fragment records: sectors can persist out of order, so a valid *continuation* fragment after the damage can still belong to the torn last record.

---

### 4. What should a database do when `fsync` fails?

Stop accepting writes, and recover by restarting and replaying the WAL. **Don't retry the `fsync`.**

After an `fsync` error, Linux may have dropped the dirty pages and marked them clean, so a retry can succeed with the data gone. This is "fsyncgate", found by PostgreSQL in 2018. PostgreSQL now PANICs on `fsync` failure. strata *poisons*: writes return `Poisoned`, reads keep working, and reopening recovers.

---

### 5. Why must a WAL record be checksummed? Isn't the filesystem reliable?

Torn writes after power loss are normal behaviour, not a filesystem bug, and media can silently flip bits. Without a checksum, replay could apply garbage as if it were a real write.

The checksum should cover everything that's interpreted: the record type as well as the payload.

---

### 6. What's the cost of `sync = always`, and how do real systems reduce it?

Each sync is a full round trip to stable storage: milliseconds on many devices, about 3–4 ms on an M3 Mac with `F_FULLFSYNC`. In our demo that was ~230 durable writes/s against ~215,000/s unsynced.

Systems reduce the cost with **group commit**: concurrent writers queue up, one leader writes everyone's records and syncs once, and then all of them are acknowledged. Other options are batching in the application, sync every N writes or every N ms (accepting a bounded loss window), and fast NVMe or battery-backed write caches.

---

### 7. Recovery itself can crash. How do you make it safe?

Make every step either redo-able or idempotent, and make the new state durable *before* deleting the old.

In strata: write the recovered state into a new log, sync it and the directory, and only then delete the old logs and sync the directory again. Entries keep their original sequence numbers, so replaying both old and new logs gives the same state. Test it by killing recovery after every single filesystem operation.

---

### 8. Why does LevelDB, like strata, start a new log after recovery instead of appending to the old one?

The old log may end in a torn record. Appending after it means the next recovery stops at the torn record and drops everything written after it, including acknowledged writes.

Starting a fresh log (or carefully truncating to the last good record first, as LevelDB's `reuse_logs` does) avoids that.

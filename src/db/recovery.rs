//! What `open` does before the database accepts reads and writes.
//!
//! 1. Delete leftover `*.sst.tmp` files: tables a crash interrupted before
//!    they were renamed, so nothing ever relied on them.
//! 2. Open every `.sst` table and take `table_max`, the largest `max_seqno`.
//! 3. Replay the logs in number order into a fresh memtable, **skipping every
//!    entry with seqno <= `table_max`** (see [`replay`] for why that's safe).
//! 4. If anything was replayed, write it to a new table.
//! 5. Create a new, empty log and sync the directory.
//! 6. Delete the old logs and sync the directory again.
//!
//! A crash at any step leaves the data intact: until step 4's table is
//! durable the old logs are untouched, and after it the old logs only hold
//! entries the skip in step 3 ignores.

use std::path::Path;
use std::sync::Arc;

use super::ReadState;
use super::files::{FileKind, log_name, parse_file_name, table_name};
use super::flush::write_table;
use crate::batch::WriteBatch;
use crate::fs::FileSystem;
use crate::key::{MAX_SEQNO, SeqNo};
use crate::memtable::Memtable;
use crate::options::Options;
use crate::sstable::Table;
use crate::wal::{WalEnd, WalWriter, read_records};
use crate::{Error, Result};

/// Everything `open` needs to build the `Db`.
pub(crate) struct Recovered {
    pub(crate) state: ReadState,
    pub(crate) wal: WalWriter,
    pub(crate) log_number: u64,
    pub(crate) next_file_number: u64,
    pub(crate) last_seqno: SeqNo,
}

pub(crate) fn recover(fs: &dyn FileSystem, dir: &Path, options: &Options) -> Result<Recovered> {
    let mut logs = Vec::new();
    let mut table_numbers = Vec::new();
    let mut next_file_number = 1;
    for name in fs.list(dir)? {
        let Some((number, kind)) = parse_file_name(&name) else {
            continue;
        };
        next_file_number = next_file_number.max(number + 1);
        match kind {
            FileKind::Log => logs.push((number, name)),
            FileKind::Table => table_numbers.push(number),
            // No directory sync needed: if the removal is lost in a crash,
            // the next open removes it again.
            FileKind::TempTable => fs.remove(&dir.join(&name))?,
        }
    }
    logs.sort();
    table_numbers.sort_unstable_by(|a, b| b.cmp(a)); // newest first

    let mut tables = Vec::with_capacity(table_numbers.len());
    for number in table_numbers {
        let file = fs.open_random(&dir.join(table_name(number)))?;
        tables.push(Arc::new(Table::open(file)?));
    }
    let table_max = tables.iter().map(|t| t.max_seqno()).max().unwrap_or(0);

    let mem = Memtable::new();
    let replayed_max = replay(fs, dir, &logs, &mem, table_max, options)?;

    if !mem.is_empty() {
        let table = write_table(fs, dir, next_file_number, &mem, &options.table)?;
        next_file_number += 1;
        tables.insert(0, table);
    }

    // The new log's *name* must be durable before any write is synced into
    // it, or a crash could lose an acknowledged write along with the name.
    let log_number = next_file_number;
    next_file_number += 1;
    let wal = WalWriter::new(fs.create(&dir.join(log_name(log_number)))?);
    fs.sync_dir(dir)?;
    for (_, name) in &logs {
        fs.remove(&dir.join(name))?;
    }
    if !logs.is_empty() {
        fs.sync_dir(dir)?;
    }

    Ok(Recovered {
        state: ReadState {
            mem: Arc::new(Memtable::new()),
            imm: None,
            tables,
        },
        wal,
        log_number,
        next_file_number,
        last_seqno: table_max.max(replayed_max),
    })
}

/// Replay `logs` (sorted by number) into `mem`, skipping entries already in a
/// table; returns the highest sequence number replayed.
///
/// **Why skipping seqno <= `table_max` is safe:** every seqno <= `table_max`
/// is in some table. A flush always writes the *whole* memtable, and a
/// memtable holds every seqno since the previous flush, because rotation
/// happens only under the writer lock, between batches. So the table with
/// `max_seqno == table_max` holds everything since the flush before it, that
/// one everything before, and so on down to seqno 1. Without the skip, a
/// crash after a table became durable but before its log was deleted would
/// copy the same entries into a second table.
fn replay(
    fs: &dyn FileSystem,
    dir: &Path,
    logs: &[(u64, String)],
    mem: &Memtable,
    table_max: SeqNo,
    options: &Options,
) -> Result<SeqNo> {
    let mut max_seqno = 0;
    for (_, name) in logs {
        let buf = fs.read(&dir.join(name))?;
        let (records, end) = read_records(&buf);
        for record in records {
            // The CRC matched, so undecodable contents are real corruption.
            let (first, batch) = WriteBatch::decode(&record)?;
            let in_range = first >= 1
                && first
                    .checked_add(batch.len() as u64)
                    .is_some_and(|end| end - 1 <= MAX_SEQNO);
            if !in_range {
                return Err(Error::Corruption(format!(
                    "{name}: batch with out-of-range sequence number {first}"
                )));
            }
            for (i, op) in batch.ops.iter().enumerate() {
                let seqno = first + i as u64;
                if seqno <= table_max {
                    continue; // already in a table
                }
                super::apply(mem, seqno, op);
                max_seqno = max_seqno.max(seqno);
            }
        }
        match end {
            // A torn tail is what a crash mid-write looks like: fine.
            WalEnd::Clean | WalEnd::TornTail { .. } => {}
            WalEnd::Corrupt { offset } if options.paranoid_checks => {
                return Err(Error::Corruption(format!(
                    "{name}: damaged record at offset {offset} followed by valid records"
                )));
            }
            // Point-in-time recovery: keep what came before the damage and
            // ignore everything after it, in this log and later ones.
            WalEnd::Corrupt { .. } => break,
        }
    }
    Ok(max_seqno)
}

//! Writing a memtable out as an L0 table.

use std::path::Path;
use std::sync::Arc;

use super::files::{log_name, table_name, temp_table_name};
use super::{DbInner, ReadState, Writer};
use crate::Result;
use crate::fs::FileSystem;
use crate::memtable::Memtable;
use crate::sstable::{Table, TableBuilder, TableOptions};
use crate::wal::WalWriter;

/// Build table `number` from every version in `mem` and make it live:
///
/// 1. write `{number}.sst.tmp`
/// 2. `finish`, which syncs the file's contents
/// 3. rename it to `{number}.sst`
/// 4. sync the directory, so the new name survives a crash
///
/// Only after step 4 may the caller publish the table to readers or delete
/// the log its data came from. A crash before the rename leaves a `.tmp`
/// file, which `open` deletes; a crash between 3 and 4 may lose the rename,
/// which is fine because the log is still there.
pub(crate) fn write_table(
    fs: &dyn FileSystem,
    dir: &Path,
    number: u64,
    mem: &Memtable,
    opts: &TableOptions,
) -> Result<Arc<Table>> {
    let entries = mem.entries();
    debug_assert!(!entries.is_empty(), "never write an empty table");
    let tmp = dir.join(temp_table_name(number));
    let path = dir.join(table_name(number));

    let mut builder = TableBuilder::new(fs.create(&tmp)?, opts.clone())?;
    for (key, value) in &entries {
        builder.add(&key.encode(), value)?;
    }
    builder.finish()?;
    fs.rename(&tmp, &path)?;
    fs.sync_dir(dir)?;
    Ok(Arc::new(Table::open(fs.open_random(&path)?)?))
}

/// Flush the current memtable, with the writer lock held (`w`):
///
/// 1. create a new log and sync the directory, so its name is durable before
///    any write is synced into it;
/// 2. **rotate**: the memtable becomes `imm` and an empty one takes its place
///    (readers now search both), and writes go to the new log;
/// 3. write `imm` to a table ([`write_table`]);
/// 4. **publish**: the table joins `tables` and `imm` goes away;
/// 5. delete the old log and sync the directory.
///
/// The old log is deleted only after the table is durable and published, so
/// at every step one of them holds the data. Any failure poisons the writer:
/// the in-memory state no longer says which files exist. `imm` stays
/// readable until a table replaces it, so reads keep working.
pub(crate) fn flush_locked(inner: &DbInner, w: &mut Writer) -> Result<()> {
    let result = flush_steps(inner, w);
    if let Err(e) = &result {
        w.poisoned = Some(format!("flush failed: {e}"));
    }
    result
}

fn flush_steps(inner: &DbInner, w: &mut Writer) -> Result<()> {
    let (fs, dir) = (inner.fs.as_ref(), inner.dir.as_path());

    let new_log = w.next_file_number;
    w.next_file_number += 1;
    let wal = WalWriter::new(fs.create(&dir.join(log_name(new_log)))?);
    fs.sync_dir(dir)?;

    let imm = {
        let mut state = inner.state.write();
        let imm = state.mem.clone();
        *state = Arc::new(ReadState {
            mem: Arc::new(Memtable::new()),
            imm: Some(imm.clone()),
            tables: state.tables.clone(),
        });
        imm
    };
    w.wal = wal;
    w.writes_since_sync = 0;
    let old_log = std::mem::replace(&mut w.log_number, new_log);

    let number = w.next_file_number;
    w.next_file_number += 1;
    let table = write_table(fs, dir, number, &imm, &inner.options.table)?;

    {
        let mut state = inner.state.write();
        let mut tables = Vec::with_capacity(state.tables.len() + 1);
        tables.push(table);
        tables.extend(state.tables.iter().cloned());
        *state = Arc::new(ReadState {
            mem: state.mem.clone(),
            imm: None,
            tables,
        });
    }

    fs.remove(&dir.join(log_name(old_log)))?;
    fs.sync_dir(dir)?;
    Ok(())
}

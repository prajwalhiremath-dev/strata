//! Writing a memtable out as an L0 table.

use std::path::Path;
use std::sync::Arc;

use super::files::{table_name, temp_table_name};
use crate::Result;
use crate::fs::FileSystem;
use crate::memtable::Memtable;
use crate::sstable::{Table, TableBuilder, TableOptions};

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

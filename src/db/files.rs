//! File names in a database directory.
//!
//! Logs and tables share one number space, so a number identifies a file
//! regardless of its kind and the next number is simply one more than the
//! largest in the directory:
//!
//! ```text
//! 000007.log       a write-ahead log
//! 000008.sst       a live table
//! 000009.sst.tmp   a table being written (deleted at open if left over)
//! LOCK             held while the database is open
//! ```

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileKind {
    Log,
    Table,
    TempTable,
}

/// Parse a file name strata created. Anything else (including `LOCK`) is
/// `None` and left alone.
pub(crate) fn parse_file_name(name: &str) -> Option<(u64, FileKind)> {
    // ".sst.tmp" before ".sst" is not needed (".sst.tmp" doesn't end in
    // ".sst"), but listing it first keeps the cases obviously disjoint.
    const KINDS: [(&str, FileKind); 3] = [
        (".sst.tmp", FileKind::TempTable),
        (".sst", FileKind::Table),
        (".log", FileKind::Log),
    ];
    let (stem, kind) = KINDS
        .iter()
        .find_map(|&(suffix, kind)| Some((name.strip_suffix(suffix)?, kind)))?;
    // `u64::from_str` accepts a leading '+'; we only ever write digits.
    if stem.is_empty() || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((stem.parse().ok()?, kind))
}

pub(crate) fn log_name(n: u64) -> String {
    format!("{n:06}.log")
}

pub(crate) fn table_name(n: u64) -> String {
    format!("{n:06}.sst")
}

pub(crate) fn temp_table_name(n: u64) -> String {
    format!("{n:06}.sst.tmp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        assert_eq!(parse_file_name(&log_name(7)), Some((7, FileKind::Log)));
        assert_eq!(parse_file_name(&table_name(8)), Some((8, FileKind::Table)));
        assert_eq!(
            parse_file_name(&temp_table_name(9)),
            Some((9, FileKind::TempTable))
        );
        assert_eq!(log_name(7), "000007.log");
        assert_eq!(table_name(1_234_567), "1234567.sst");
    }

    #[test]
    fn foreign_names_are_ignored() {
        for name in [
            "LOCK",
            ".log",
            "+1.log",
            "a1.sst",
            "1.sst.bak",
            "1.tmp",
            "",
            "000001.sst.tmp.tmp",
            "99999999999999999999999.log",
        ] {
            assert_eq!(parse_file_name(name), None, "{name:?}");
        }
    }
}

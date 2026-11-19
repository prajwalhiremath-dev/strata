//! The write-ahead log (WAL).
//!
//! Every batch is appended here *before* it is applied to the memtable, so a
//! crash can be recovered from by replaying the log. Format details are in
//! [`format`]; chapter 02 explains every choice.

pub(crate) mod format;
mod reader;
mod writer;

pub(crate) use reader::{WalEnd, read_records};
pub(crate) use writer::WalWriter;

#[cfg(test)]
mod tests {
    use super::format::{BLOCK_SIZE, HEADER_SIZE};
    use super::*;
    use crate::fs::{FileSystem, SimFs};
    use proptest::prelude::*;
    use std::path::Path;

    /// Write `records` through a WalWriter and return the raw file bytes.
    fn write_log(records: &[Vec<u8>]) -> Vec<u8> {
        let fs = SimFs::default();
        fs.create_dir_all(Path::new("/w")).unwrap();
        let p = Path::new("/w/1.log");
        let mut w = WalWriter::new(fs.create(p).unwrap());
        for r in records {
            w.add_record(r).unwrap();
        }
        w.sync().unwrap();
        fs.read(p).unwrap()
    }

    fn rec(len: usize, fill: u8) -> Vec<u8> {
        vec![fill; len]
    }

    #[test]
    fn roundtrip_small() {
        let recs = vec![b"one".to_vec(), Vec::new(), b"three".to_vec()];
        let (got, end) = read_records(&write_log(&recs));
        assert_eq!(got, recs);
        assert_eq!(end, WalEnd::Clean);
    }

    #[test]
    fn roundtrip_spans_blocks() {
        let recs = vec![rec(10, 1), rec(100 * 1024, 2), rec(5, 3), rec(70_000, 4)];
        let bytes = write_log(&recs);
        assert!(bytes.len() > 4 * BLOCK_SIZE);
        let (got, end) = read_records(&bytes);
        assert_eq!(got, recs);
        assert_eq!(end, WalEnd::Clean);
    }

    #[test]
    fn record_ends_within_7_bytes_of_block_end() {
        for gap in 0..HEADER_SIZE {
            // First record fills the block up to `gap` bytes before its end.
            let first = rec(BLOCK_SIZE - HEADER_SIZE - gap, 7);
            let recs = vec![first, b"next".to_vec()];
            let bytes = write_log(&recs);
            let padding = &bytes[BLOCK_SIZE - gap..BLOCK_SIZE];
            assert!(
                padding.iter().all(|&b| b == 0),
                "gap {gap}: padding is zeros"
            );
            let (got, end) = read_records(&bytes);
            assert_eq!(got, recs, "gap {gap}");
            assert_eq!(end, WalEnd::Clean, "gap {gap}");
        }
    }

    #[test]
    fn truncated_at_every_offset() {
        let recs = vec![rec(40, 1), rec(300, 2), rec(9, 3)];
        let bytes = write_log(&recs);
        for cut in 0..=bytes.len() {
            let (got, end) = read_records(&bytes[..cut]);
            assert_eq!(
                got,
                recs[..got.len()],
                "cut {cut}: only whole records, from the front"
            );
            assert!(
                matches!(end, WalEnd::Clean | WalEnd::TornTail { .. }),
                "cut {cut}: got {end:?}"
            );
        }
    }

    #[test]
    fn truncated_multi_block_record_is_torn() {
        let recs = vec![rec(10, 1), rec(80_000, 2)];
        let bytes = write_log(&recs);
        let (got, end) = read_records(&bytes[..BLOCK_SIZE + 100]);
        assert_eq!(got, recs[..1]);
        assert!(matches!(end, WalEnd::TornTail { .. }), "{end:?}");
    }

    #[test]
    fn garbage_suffix_is_torn_tail() {
        let recs = vec![rec(20, 1), rec(20, 2), rec(50, 3)];
        let mut bytes = write_log(&recs);
        let n = bytes.len();
        for b in &mut bytes[n - 50..] {
            *b = 0x5A; // file length landed, data did not
        }
        let (got, end) = read_records(&bytes);
        assert_eq!(got, recs[..2]);
        assert!(matches!(end, WalEnd::TornTail { .. }), "{end:?}");
    }

    #[test]
    fn zeroed_suffix_is_clean_or_torn() {
        let recs = vec![rec(20, 1), rec(20, 2), rec(50, 3)];
        let mut bytes = write_log(&recs);
        let start = 2 * (HEADER_SIZE + 20); // start of record 3
        for b in &mut bytes[start..] {
            *b = 0;
        }
        let (got, end) = read_records(&bytes);
        assert_eq!(got, recs[..2]);
        assert!(
            matches!(end, WalEnd::Clean | WalEnd::TornTail { .. }),
            "{end:?}"
        );
    }

    #[test]
    fn damage_before_valid_record_is_corrupt() {
        // Each record exactly fills a block, so record i starts block i.
        let big = BLOCK_SIZE - HEADER_SIZE;
        let recs = vec![rec(big, 1), rec(big, 2), rec(big, 3)];
        let mut bytes = write_log(&recs);
        bytes[BLOCK_SIZE + HEADER_SIZE + 10] ^= 0xFF; // payload of record 2
        let (got, end) = read_records(&bytes);
        assert_eq!(got, recs[..1]);
        assert_eq!(end, WalEnd::Corrupt { offset: BLOCK_SIZE });
    }

    #[test]
    fn damage_then_record_starting_mid_block_is_corrupt() {
        // Record 2 spans blocks 0-1; record 3 starts in the middle of block 1.
        let recs = vec![rec(100, 1), rec(BLOCK_SIZE, 2), rec(50, 3)];
        let mut bytes = write_log(&recs);
        bytes[HEADER_SIZE + 10] ^= 0xFF; // payload of record 1
        let (got, end) = read_records(&bytes);
        assert!(got.is_empty());
        assert_eq!(end, WalEnd::Corrupt { offset: 0 });
    }

    #[test]
    fn last_record_damaged_early_but_intact_later_is_torn() {
        // Out-of-order sector writes: the first fragment of the final record
        // never landed, its later fragments did. Still a torn write.
        let recs = vec![rec(100, 1), rec(80_000, 2)];
        let mut bytes = write_log(&recs);
        let second = HEADER_SIZE + 100;
        bytes[second + HEADER_SIZE + 5] ^= 0xFF;
        let (got, end) = read_records(&bytes);
        assert_eq!(got, recs[..1]);
        assert_eq!(end, WalEnd::TornTail { offset: second });
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn reader_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..70_000)) {
            let _ = read_records(&bytes);
        }

        #[test]
        fn reader_never_panics_on_damaged_logs(
            sizes in proptest::collection::vec(0usize..40_000, 1..6),
            flips in proptest::collection::vec(any::<usize>(), 0..4),
            cut in any::<usize>(),
        ) {
            let recs: Vec<_> = sizes.iter().enumerate().map(|(i, &n)| rec(n, i as u8)).collect();
            let mut bytes = write_log(&recs);
            for f in flips {
                let i = f % bytes.len();
                bytes[i] ^= 0xFF;
            }
            let cut = cut % (bytes.len() + 1);
            let (got, _) = read_records(&bytes[..cut]);
            // Whatever comes back is a prefix of what was written.
            prop_assert!(got.len() <= recs.len());
        }
    }
}

//! Chapter 03 experiment: what the restart interval does to file size and
//! lookup cost.
//!
//! ```text
//! cargo run --release --example sst_layout
//! ```
//!
//! Writes the same 100,000 sorted keys four times (restart interval 1, 4, 16,
//! 64), then prints each file's size, its block count and the average cost of
//! a random lookup. Finally it hex-dumps the first entries of a data block so
//! you can see prefix compression byte by byte.

use std::time::Instant;

use strata::fs::{FileSystem, OsFs};
use strata::key::{InternalKey, ValueKind};
use strata::sstable::{Table, TableBuilder, TableOptions};

const N: u32 = 100_000;
const LOOKUPS: u32 = 10_000;

fn user_key(i: u32) -> Vec<u8> {
    format!("user:{i:08}").into_bytes()
}

fn key(i: u32) -> Vec<u8> {
    InternalKey::new(&user_key(i), 1, ValueKind::Put).encode()
}

/// xorshift: deterministic "random" lookups without a dependency.
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn main() -> strata::Result<()> {
    let dir = tempfile::tempdir()?;
    let fs = OsFs;
    println!(
        "{N} keys like {:?}, 16-byte values, block_size 4096\n",
        "user:00000042"
    );
    println!(
        "{:>8} {:>12} {:>10} {:>8} {:>14}",
        "interval", "file bytes", "vs 1", "blocks", "ns per lookup"
    );
    let mut baseline = 0u64;
    for interval in [1, 4, 16, 64] {
        let path = dir.path().join(format!("{interval}.sst"));
        let mut b = TableBuilder::new(
            fs.create(&path)?,
            TableOptions {
                block_size: 4096,
                restart_interval: interval,
            },
        )?;
        for i in 0..N {
            b.add(&key(i), &[b'v'; 16])?;
        }
        let meta = b.finish()?;
        if interval == 1 {
            baseline = meta.file_size;
        }

        let table = Table::open(fs.open_random(&path)?)?;
        let mut rng = 0x2545_F491_4F6C_DD1D;
        let mut it = table.iter();
        let start = Instant::now();
        for _ in 0..LOOKUPS {
            let i = (next(&mut rng) % u64::from(N)) as u32;
            it.seek(&key(i))?;
            assert!(it.valid() && it.key() == key(i));
        }
        let ns = start.elapsed().as_nanos() / u128::from(LOOKUPS);
        let change = 100.0 * (meta.file_size as f64 - baseline as f64) / baseline as f64;
        let vs = if interval == 1 {
            "baseline".to_string()
        } else {
            format!("{change:.1}%")
        };
        println!(
            "{interval:>8} {:>12} {vs:>10} {:>8} {ns:>14}",
            meta.file_size,
            table.num_data_blocks()?
        );
    }

    // The first three entries of the interval-16 file's first block.
    let bytes = fs.read(&dir.path().join("16.sst"))?;
    println!("\nfirst entries of the first data block (interval 16):");
    let mut pos = 0;
    for _ in 0..3 {
        // Every length here is < 128, so each varint is a single byte.
        let (shared, non_shared, value_len) = (bytes[pos], bytes[pos + 1], bytes[pos + 2]);
        let delta = &bytes[pos + 3..pos + 3 + non_shared as usize];
        println!(
            "  shared={shared:<2} non_shared={non_shared:<2} value_len={value_len:<2} key bytes stored: {}",
            delta
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        pos += 3 + non_shared as usize + value_len as usize;
    }
    println!("  (the last 8 key bytes of each entry are the seqno/kind trailer)");
    Ok(())
}

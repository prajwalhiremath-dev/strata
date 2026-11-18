//! Chapter 00 experiment: why LSM trees write the way they do.
//!
//! Stores N small (100-byte) records durably, three different ways:
//!
//! 1. `in-place`   - B-tree style: each record lives in a 4 KiB page somewhere
//!    in the file, so updating it rewrites the whole page at a random offset,
//!    then syncs.
//! 2. `append`     - log style: append the 100-byte record to the end of a file,
//!    then sync. One sync per record.
//! 3. `append+batch` - LSM/WAL with group commit: append records and sync once
//!    per batch of 100.
//!
//! Run: `cargo run --release --example write_patterns`
//!
//! Numbers depend heavily on your disk and OS. On macOS `sync_all` issues
//! F_FULLFSYNC (a real flush to stable storage), which is slow - that cost is
//! exactly what batching amortises.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::Instant;

const N: usize = 500;
const RECORD: usize = 100;
const PAGE: usize = 4096;
const PAGES: u64 = 16_384; // 64 MiB file for the in-place case
const BATCH: usize = 100;

fn path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("strata-ch00-{name}-{}", std::process::id()))
}

/// Tiny deterministic PRNG (xorshift) so the experiment needs no dependencies.
fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn report(name: &str, start: Instant, bytes_written: usize, syncs: usize) {
    let secs = start.elapsed().as_secs_f64();
    println!(
        "{name:<14} {:>10.0} records/s   {:>8.2} ms total/record   bytes written: {:>9}  ({:>4.0}x the data)   syncs: {syncs}",
        N as f64 / secs,
        secs * 1000.0 / N as f64,
        bytes_written,
        bytes_written as f64 / (N * RECORD) as f64,
    );
}

fn in_place() -> std::io::Result<()> {
    let p = path("inplace");
    let mut f = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&p)?;
    f.set_len(PAGES * PAGE as u64)?;
    f.sync_all()?;
    let page = [7u8; PAGE];
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    let start = Instant::now();
    for _ in 0..N {
        let offset = (next(&mut rng) % PAGES) * PAGE as u64;
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(&page)?; // a 100-byte change still rewrites the whole page
        f.sync_all()?;
    }
    report("in-place", start, N * PAGE, N);
    std::fs::remove_file(p)
}

fn append(batch: usize, name: &str) -> std::io::Result<()> {
    let p = path(name);
    let mut f = File::create(&p)?;
    let record = [7u8; RECORD];
    let start = Instant::now();
    let mut syncs = 0;
    for i in 1..=N {
        f.write_all(&record)?;
        if i % batch == 0 || i == N {
            f.sync_all()?;
            syncs += 1;
        }
    }
    report(name, start, N * RECORD, syncs);
    std::fs::remove_file(p)
}

fn main() -> std::io::Result<()> {
    println!("Storing {N} records of {RECORD} bytes durably, three ways:\n");
    in_place()?;
    append(1, "append")?;
    append(BATCH, "append+batch")?;
    println!(
        "\nNote: the OS and SSD do their own work underneath (page cache, flash erase blocks), \
         so 'bytes written' is what we asked for, not what the hardware did."
    );
    Ok(())
}

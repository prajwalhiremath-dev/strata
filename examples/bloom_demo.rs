//! Chapter 04 experiment: what a bloom filter buys, and a flush lifecycle.
//!
//! ```text
//! cargo run --release --example bloom_demo
//! ```
//!
//! Part 1 builds the same 100,000-key table at several filter sizes and
//! probes it with 100,000 keys that are absent but sort *inside* the table's
//! key range (keys past either end are ruled out by the index alone, so they
//! would make every filter look perfect). For each size it prints the
//! measured false-positive rate next to the theoretical one, and how many
//! data blocks a probe reads.
//!
//! Part 2 opens a database with a 64 KiB memtable, writes 20,000 keys, and
//! prints the directory after each flush.

use std::path::Path;
use std::time::Instant;

use strata::fs::{FileSystem, OsFs};
use strata::key::{InternalKey, ValueKind};
use strata::sstable::{Table, TableBuilder, TableOptions};
use strata::{Db, Options};

const N: u32 = 100_000;

/// Present keys end in "0", absent ones in "5": every absent key sorts
/// between two present ones.
fn present(i: u32) -> Vec<u8> {
    format!("user:{i:08}0").into_bytes()
}

fn absent(i: u32) -> Vec<u8> {
    format!("user:{i:08}5").into_bytes()
}

fn main() -> strata::Result<()> {
    let dir = tempfile::tempdir()?;
    filter_rates(dir.path())?;
    flush_lifecycle(&dir.path().join("db"))?;
    Ok(())
}

fn filter_rates(dir: &Path) -> strata::Result<()> {
    let fs = OsFs;
    println!("Part 1: {N} keys, {N} absent probes inside the key range\n");
    println!(
        "{:>4} {:>12} {:>3} {:>10} {:>10} {:>12} {:>9}",
        "bpk", "filter bytes", "k", "measured", "theory", "blocks/probe", "ns/probe"
    );
    // The last absent key sorts past the last present one: leave it out.
    let probes: Vec<Vec<u8>> = (0..N - 1).map(absent).collect();
    for bpk in [0, 2, 5, 10, 20] {
        let path = dir.join(format!("bpk{bpk}.sst"));
        let opts = TableOptions {
            bloom_bits_per_key: bpk,
            ..Default::default()
        };
        let mut b = TableBuilder::new(fs.create(&path)?, opts)?;
        for i in 0..N {
            let k = InternalKey::new(&present(i), 1, ValueKind::Put).encode();
            b.add(&k, &[b'v'; 16])?;
        }
        b.finish()?;
        let without = file_len(&fs, &dir.join("bpk0.sst"))?;
        let filter_bytes = file_len(&fs, &path)? - without;

        let table = Table::open(fs.open_random(&path)?)?;
        let start = Instant::now();
        for p in &probes {
            // Through the public cursor, the same work a lookup does after
            // the filter: seek, then compare user keys.
            if table.may_contain(p) {
                let mut it = table.iter();
                it.seek(&InternalKey::new(p, 1, ValueKind::Put).encode())?;
                assert!(!it.valid() || InternalKey::decode(it.key())?.user_key != *p);
            }
        }
        let ns = start.elapsed().as_nanos() / probes.len() as u128;
        let reads = table.data_block_reads() as f64 / probes.len() as f64;
        let (measured, theory, k) = rates(&table, &probes, bpk)?;
        println!(
            "{bpk:>4} {filter_bytes:>12} {k:>3} {:>9.2}% {:>9.2}% {reads:>12.3} {ns:>9}",
            measured * 100.0,
            theory * 100.0,
        );
    }
    println!("\n(bpk 0 = no filter: every probe reads the block its key would be in.");
    println!(" Filter bytes include the k byte and the 4-byte CRC.)");
    Ok(())
}

fn file_len(fs: &OsFs, path: &Path) -> strata::Result<u64> {
    fs.open_random(path)?.size()
}

/// Measured and theoretical false-positive rate, and k.
fn rates(table: &Table, probes: &[Vec<u8>], bpk: usize) -> strata::Result<(f64, f64, u32)> {
    if bpk == 0 {
        return Ok((1.0, 1.0, 0));
    }
    let hits = probes.iter().filter(|p| table.may_contain(p)).count();
    let k = table.filter_probes().expect("has a filter");
    let theory = (1.0 - (-(f64::from(k)) / bpk as f64).exp()).powi(k as i32);
    Ok((hits as f64 / probes.len() as f64, theory, k))
}

fn flush_lifecycle(dir: &Path) -> strata::Result<()> {
    println!("\nPart 2: 20,000 writes with a 64 KiB memtable\n");
    let db = Db::open(
        dir,
        Options {
            memtable_size: 64 * 1024,
            ..Options::default()
        },
    )?;
    let fs = OsFs;
    let mut tables_seen = 0;
    for i in 0..20_000u32 {
        db.put(format!("key{i:06}").as_bytes(), &[b'x'; 32])?;
        let names = fs.list(dir)?;
        let tables = names.iter().filter(|n| n.ends_with(".sst")).count();
        if tables != tables_seen {
            tables_seen = tables;
            if tables <= 3 {
                println!("after write {:>6}: {}", i + 1, names.join(" "));
            }
        }
    }
    println!("... {tables_seen} flushes in all, one log at a time, each table a new L0 file");
    db.close()?;
    drop(db);
    let db = Db::open(dir, Options::default())?;
    let names = fs.list(dir)?;
    println!("after reopen      : {}", names.join(" "));
    assert_eq!(db.get(b"key019999")?, Some(vec![b'x'; 32]));
    Ok(())
}

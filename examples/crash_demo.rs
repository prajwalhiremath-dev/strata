//! Chapter 02 experiment: kill a writer with `kill -9`, then check that every
//! acknowledged write survived.
//!
//! ```text
//! cargo run --release --example crash_demo -- write  /tmp/strata-demo always
//! # ...let it run, then from another terminal:  kill -9 <pid>
//! cargo run --release --example crash_demo -- verify /tmp/strata-demo <last acked n>
//! ```
//!
//! `write` puts key n -> n for n = 0, 1, 2, ... forever and prints
//! `acked n` every 100 writes, *after* `put` returned Ok. `verify` reopens the
//! database (running crash recovery) and checks keys 0..=n are all there.
//!
//! Note: `kill -9` kills the *process*, not the machine. Bytes handed to the OS
//! (even unsynced) survive in the page cache, so even `never` will usually
//! pass. Only power loss / kernel crash loses unsynced data - which is what
//! the `SimFs` tests simulate.

use std::io::Write;

use strata::{Db, Options, SyncPolicy};

fn key(n: u64) -> Vec<u8> {
    format!("{n:010}").into_bytes()
}

fn main() -> strata::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("write") => {
            let dir = &args[2];
            let sync = match args.get(3).map(String::as_str) {
                Some("never") => SyncPolicy::Never,
                _ => SyncPolicy::Always,
            };
            let db = Db::open(
                dir,
                Options {
                    sync,
                    ..Options::default()
                },
            )?;
            println!("pid {} writing with {sync:?}", std::process::id());
            let mut out = std::io::stdout();
            for n in 0u64.. {
                db.put(&key(n), n.to_string().as_bytes())?;
                if n % 100 == 0 {
                    writeln!(out, "acked {n}").ok();
                    out.flush().ok();
                }
            }
            Ok(())
        }
        Some("verify") => {
            let dir = &args[2];
            let n: u64 = args[3].parse().expect("n must be a number");
            let db = Db::open(dir, Options::default())?;
            let missing: Vec<u64> = (0..=n)
                .filter(|&i| db.get(&key(i)).ok().flatten().is_none())
                .collect();
            let total = db
                .scan(std::ops::Bound::Unbounded, std::ops::Bound::Unbounded)?
                .len();
            if missing.is_empty() {
                println!(
                    "OK: all {} acknowledged keys present ({total} keys in total, the rest were written after the last 'acked' line)",
                    n + 1
                );
                Ok(())
            } else {
                println!(
                    "LOST {} acknowledged keys, first: {:?}",
                    missing.len(),
                    &missing[..missing.len().min(5)]
                );
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!(
                "usage: crash_demo write <dir> [always|never]  |  crash_demo verify <dir> <n>"
            );
            std::process::exit(2);
        }
    }
}

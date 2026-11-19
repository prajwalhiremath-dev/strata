# strata

An embedded key-value storage engine in Rust, built from scratch on a **log-structured merge-tree (LSM)**: the same family of designs behind LevelDB, RocksDB and Cassandra. It's a library you link into your program, not a server.

I'm building it in small milestones. Each milestone has its own code, tests and a learning chapter that explains what the layer does, why it's designed that way, and which alternatives were rejected.

> **Status:** milestone 2 of 11. Durable writes through a checksummed write-ahead log, crash recovery tested against a simulated filesystem that pulls the plug (torn writes, failed syncs, crashes mid-recovery). Next: on-disk sorted tables (SSTables). See the [milestone table](docs/specs/2026-11-18-lsm-engine-design.md#8-milestones-each-ends-at-a-stop-point).

![strata architecture](docs/specs/strata-architecture.svg)

## Where to read

| | |
|---|---|
| [Design](docs/specs/2026-11-18-lsm-engine-design.md) | goals, architecture, testing and benchmark strategy |
| [Learning chapters](docs/learn/) | one chapter per milestone: what, why, alternatives, how real systems do it |
| [Interview notes](docs/interview/) | questions and answers per chapter |

## Quick look

```rust
use strata::{Db, Options, WriteBatch};

let db = Db::open("my-data", Options::default())?; // recovers after a crash
db.put(b"user:1", b"alice")?;

let mut batch = WriteBatch::new();
batch.put(b"x", b"1").put(b"y", b"1"); // applied atomically
db.write(batch)?;

assert_eq!(db.get(b"user:1")?, Some(b"alice".to_vec()));
```

## Build and test

```bash
cargo test
cargo run --release --example write_patterns   # chapter 00 experiment
cargo run --release --example crash_demo -- write /tmp/strata-demo always   # chapter 02: kill -9 it, then `verify`
```

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

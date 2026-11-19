# strata

An embedded key-value storage engine in Rust, built from scratch on a **log-structured merge-tree (LSM)**: the same family of designs behind LevelDB, RocksDB and Cassandra. It's a library you link into your program, not a server.

I'm building it in small milestones. Each milestone has its own code, tests and a learning chapter that explains what the layer does, why it's designed that way, and which alternatives were rejected.

> **Status:** milestone 1 of 11. In-memory engine: internal keys, memtable, atomic batches, model-tested against `BTreeMap`. Disk (WAL) comes next. See the [milestone table](docs/specs/2026-11-18-lsm-engine-design.md#8-milestones-each-ends-at-a-stop-point).

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

let db = Db::open_in_memory(Options::default())?;
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
```

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

# jellyfish-rs

A binary Merkle-Patricia trie over 256-bit keys with a RocksDB backend, optimised for batch
insertion. Every key carries a hash chain of the values written under it, and the Merkle root
commits to the head of every chain.

This code was written to evaluate different strategies for performant MPT implementations. The final design achieves an insertion rate of over 100k/s at a tree size of 1 billion entries on a commodity SSD whilst using only 8GB of RAM. The implementation ensures inserted nodes are persisted to disk, is crash-safe and can cold-start in under 5 seconds. In one image: 

<img width="2803" height="1350" alt="image" src="https://github.com/user-attachments/assets/86ffa088-98b8-4257-9e66-6d65ee38ca7e" />

[DESIGN.md](docs/DESIGN.md) explains the design in more detail; [HASHCHAINS.md](docs/HASHCHAINS.md) the per-key history. [bigdb-report.html](docs/report-bigdb-6f4ad7.html) reports benchmarking results for a MPT with 2 billion entries. 

## Usage

```rust
use jellyfish_rs::{Entry, Key, Record, RocksFrontierConfig, RocksFrontierMPT, Value};

let mut tree = RocksFrontierMPT::open("tree.db", RocksFrontierConfig::default())?;
let key = Key([1u8; 32]);
tree.batch_upsert(&[(key, Value([2u8; 32]))]);
tree.batch_upsert(&[(key, Value([3u8; 32]))]);   // a second link in the key's chain
let root = tree.get_root_hash();                  // Option<Digest>
let value = tree.get_leaf_value(key);             // Some(Value([3; 32]))
let head = tree.get_record(key);                  // Some((1, Record { value, link }))
let history = tree.get_history(key);              // both records, oldest first
assert!(Record::verify_chain(key, &history));
```

## Tooling

```sh
cargo build --release
cargo test
cargo run --release --bin bench -- --help                 # the benchmark harness; logs to <db>/bench-log.jsonl
uv run --no-project tools/bench_plot.py <db> -o report.html # Visualise the results
```

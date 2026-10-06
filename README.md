# jellyfish-rs

A binary Merkle-Patricia trie over 256-bit keys with a RocksDB backend, optimised for batch
insertion.

This code was written to evaluate different strategies for performant MPT implementations. The final design achieves an insertion rate of over 100k/s at a tree size of 1 billion entries on a commodity SSD whilst using only 8GB of RAM. The implementation ensures inserted nodes are persisted to disk, is crash-safe and can cold-start in under 5 seconds. In one image: 

<img width="2803" height="1350" alt="image" src="https://github.com/user-attachments/assets/1dbf8a3f-e9b3-4cf5-a7c4-8857129f0f94" />


[DESIGN.md](docs/DESIGN.md) explains the design in more detail. [bigdb-report.html](docs/report-bigdb-6f4ad7.html) reports benchmarking results for a MPT with 2 billion entries.

## Usage

```rust
use jellyfish_rs::{Entry, Key, RocksFrontierConfig, RocksFrontierMPT, Value};

let mut tree = RocksFrontierMPT::open("tree.db", RocksFrontierConfig::default())?;
tree.batch_upsert(&[(Key([1u8; 32]), Value([2u8; 32]))]);
let root = tree.get_root_hash();      // Option<Digest>
let value = tree.get_leaf_value(Key([1u8; 32]));
```

## Tooling

```sh
cargo build --release
cargo test
cargo run --release --bin bench -- --help                 # the benchmark harness; logs to <db>/bench-log.jsonl
uv run --no-project tools/bench_plot.py <db> -o report.html # Visualise the results
```

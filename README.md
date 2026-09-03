# jellyfish-rs

A binary Merkle-Patricia trie over 256-bit keys with a RocksDB backend, optimised for batch
insertion. Design and rationale: [DESIGN.md](DESIGN.md). Measurement record:
[docs/REVIEW.md](docs/REVIEW.md), [docs/BENCHMARK-BASELINE.md](docs/BENCHMARK-BASELINE.md).

> [!WARNING]
> This crate was written to benchmark different strategies for managing an MPT.
>
> The code quality is poor, in part due to reckless use of Claude.
>
> It is NOT suitable for any wider purpose.

## Usage

```rust
use jellyfish_rs::{Entry, Key, RocksFrontierConfig, RocksFrontierMPT, Value};

let mut tree = RocksFrontierMPT::open("tree.db", RocksFrontierConfig::default())?;
tree.batch_upsert(&[(Key([1u8; 32]), Value([2u8; 32]))]);
let root = tree.get_root_hash();      // Option<Digest>
let value = tree.get_leaf_value(Key([1u8; 32]));
```

The public surface is `open`, `batch_upsert`, `get_root_hash`, `get_leaf_value`,
`leaf_count`, `frontier_depth`, `levels_depth`, `sorted_runs` and the census. Nothing is
stable. Dependencies: `rocksdb` (snappy only), `rayon`, `sha2`, `log`. No `unsafe`.

## Tooling

```sh
cargo build --release
cargo test                                                # databases left under target/ for inspection
cargo run --release --bin bench -- --help                 # the measurement harness; logs to <db>/bench-log.jsonl
cargo run --release --bin compact-db -- <db>              # collapse the LSM to one sorted run
uv run --no-project tools/bench_plot.py <db> -o report.html
tools/ab.py --ref <db> a=target/release/bench b=other/bench   # interleaved fixed-work A/B
samply record cargo run --release --bin bench -- -t 30 tempdb/profile
```

`tools/bench_loop.py` grows one database across repeated `bench` runs; `tools/memhog.c`
pins RAM to reproduce larger-than-RAM residency (`clang -O2 -o /tmp/memhog tools/memhog.c`).

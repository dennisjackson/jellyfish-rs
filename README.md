# jellyfish-rs

A Rust implementation of the Jellyfish Merkle Patricia Tree, with multiple storage backends and batch insertion support.

## Features

* **Merkle Patricia Tree** with SHA-256 hashing
  * `SimpleMPT` -- single-key in-memory implementation
  * `BatchMPT` -- batch-optimized in-memory implementation using concurrent DashMap
  * `DurableBatchMPT` -- batch-optimized, crash-safe implementation backed by SQLite
  * `RocksTransRelMPT` -- batch-optimized, crash-safe implementation backed by RocksDB with frontier-based memory management and parallel insertion via Rayon
* Common `MerklePatriciaTree` trait across all implementations

### Planned

* Hashchains
* Tree heads and client proofs
* HTTP API
* Witnessing proofs and API

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
jellyfish-rs = { path = "." }
```

Basic example:

```rust
use jellyfish_rs::{MerklePatriciaTree, RocksTransRelMPT, Hash};

let mut tree = RocksTransRelMPT::new(); // temporary RocksDB
tree.batch_upsert(&entries);
let root = tree.get_root_hash();
```

## Tooling

### Binaries

Build with `cargo build --release --features bins`.

* **bench** -- Configurable insertion benchmark supporting all three backends.

  ```
  cargo run --release --features bins --bin bench -- [OPTIONS] [db_path]

  Options:
    -b, --backend <rocks|sqlite|memory>  Storage backend (default: rocks)
    -t, --timeout <seconds>              Run duration (default: 30)
    -w, --window-size <n>                Entries generated per window (default: 10,000)
    -c, --batch-size <n>                 Entries per batch_upsert call (default: 1,000)
    -l, --log-file <path>                Write insertion log (CSV) to file
  ```

* **test-mermaid** -- Builds a small tree step-by-step and outputs Mermaid diagrams to `mpt_diagrams.md`.

* **dump-sqlite** -- Creates a small SQLite-backed tree and dumps its schema and contents.

### Plotting (`tools/`)

Requires Python 3 and matplotlib.

* **bench_plot.py** -- Runs benchmarks across backends and produces performance charts (insertion count over time, insertion speed over time, speed vs tree size). Can also plot from existing CSV logs or log directories.

  ```
  python3 tools/bench_plot.py --timeout 60 --output chart.png
  python3 tools/bench_plot.py --input-dir rocks=bench_logs/ --output chart.png
  ```

* **bench_loop.py** -- Runs the bench binary in an infinite loop against a persistent RocksDB, growing the database across runs. Logs CSV data, CLI output, and peak RSS per run.

### Testing

```
cargo test
```

Test quality can be evaluated with [`cargo mutants`](https://mutants.rs).

### Profiling

Profiling is best handled with [samply](https://github.com/mstange/samply):

```
samply record cargo run --release --features bins --bin bench -- -t 30
```

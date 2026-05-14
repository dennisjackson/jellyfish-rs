# jellyfish-rs

A Rust implementation of a Merkle Patricia Tree, with multiple storage backends and batch insertion support.

> [!WARNING]
> This crate was written to benchmark different strategies for managing an MPT.
> 
> The code quality is poor, in part due to reckless use of Claude.
>
> It is NOT suitable for any wider purpose.

## Design Notes 

My initial goal was to pursue an implementation where all state lived in a single KV store, outsourcing all complexity around atomicity and durability to it. I also wanted to optimize for batch insertions, where new records might be read in from another source, or arrive on the fly but only need to be committed with relatively high latency (e.g. 1 second).

I first wrote an implementation which inserts one node at a time to an in-memory tree, then generalized it to an implementation which inserted a batch concurrently. This works through recursive descent, where inserts to the left children of a node are entirely independent of inserts to the right children. It also saves on recomputing hashes higher up the tree, e.g. inserting 1000 nodes sequentially requires recomputing the root node 1000 times, but as a batch of 1000 nodes need be done only once. 

The next implementation uses SQLite to provide durability. The model is intentionally simple, with the entire tree being committed to disk, resulting in a substantial write amplification at higher tree sizes (O log N) which is only partially mitigated by the batch-insert approach. 

Finally, I switched to using RocksDB as a KV store and began evaluating different strategies for managing the write amplification. At the other end of the spectrum from the SQLite approach would be to store only the leaf key-value pairs to disk, meaning O(1) writes, but requiring holding the entire tree in memory and long restarts caused by the need to read the whole of the database back in. 

After some experiments, I settled on a frontier-based approach. An MPT of any size has a depth where it is complete, that is, every node above that level is an interior node. We call this depth the frontier. We hold the MPT in-memory above this frontier. When performing insert/updates, we update the leaf node and any mutated frontier nodes to disk. We also check if we can move the frontier down one level. 

This design means that we only ever do at most 2 inserts per insert, one to the leaf, one to the corresponding frontier. In practice, we might do less as batched inserts may share frontier nodes. When restarting, we need only read in the frontier nodes from disk and can rebuild the rest of the tree from them. However, we do have to read in all the leaf nodes below a frontier node in order to make an insert. 

We optimize for this by ensuring that leaf nodes are stored in a format that makes reading from a prefix efficient. We can also perform a trade-off between disk read bandwidth and disk write bandwidth. If we write additional nodes below the frontier to disk, then we need fewer reads to perform an insert. Every additional write halves the number of leaf nodes we need to read. Experimenting with different disk layouts (e.g. ZFS RAID 10 style)  is future work and would guide tuning this aspect. In theory, these reads/writes can be partitioned perfectly. 

The current implementation targets durability after a batch insert completes. This is achieved via RocksDB's transactions with a manual call to fsync. 

## Features

* **Merkle Patricia Tree** with SHA-256 hashing
  * `SimpleMPT` -- single-key in-memory implementation
  * `BatchMPT` -- batch-optimized in-memory implementation using concurrent DashMap
  * `DurableBatchMPT` -- batch-optimized, crash-safe implementation backed by SQLite
  * `RocksTransRelMPT` -- batch-optimized, crash-safe implementation backed by RocksDB with frontier-based memory management and parallel insertion via Rayon
* Common `MerklePatriciaTree` trait across all implementations

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

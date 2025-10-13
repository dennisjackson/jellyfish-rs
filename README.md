# jellyfish-rs

A Rust crate for the jellyfish data structure.

## Planned Features

* [ ] Merkle Patricia Tree
  * [x] A simple in-memory implementation.
  * [x] A batched in-memory implementation
  * [x] A batched persistent implementation (Sqlite)
  * [ ] A batched persistent implementation (RocksDB)
* [ ] Hashchains
* [ ] Tree Heads and Client Proofs
* [ ] HTTP API
* [ ] Witnessing Proofs and API

## Current Status

Pretty messy, pre-alpha code.

The batched persistent implementation runs >1000 insertions / second with full crash-safety / durability. Currently 90% of CPU-time is spent inside sqlite functions for reading / persisting MPT values. This doesn't happen in parallel currently - the code follows a fetch - compute - store cycle.

## Tooling

### Demo Binaries

* build-durable-tree - Allows large trees to be built and reports the performance.
* test_mermaid - Dumps a mermaid representation of an in-memory tree.
* dump-sqlite - Generates a small tree and dumps its SQL representation.

### Benchmarking

Benchmarks can be run with `cargo bench` or `cargo criterion` and are stored in `benches/mpt_benchmark.rs`. The benchmarks evaluate insertion performance in various scenarios:

* Empty or full trees
* Cold or warm caches
* SQLite tuned for durability or default settings

### Profiling

Profiling is best handled with [samply](https://github.com/mstange/samply). Invoke `samply record <command>` e.g. for a test binary or benchmark and it will open a profile in the Firefox Profiler.

### Testing

Tests can be ran with `cargo test`. Test quality can be evaluated with `cargo mutants` ([mutants.rs](https://mutants.rs)).

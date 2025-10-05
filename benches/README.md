# Merkle Patricia Tree Benchmarks

This benchmark compares the performance of `SimpleMPT` vs `BatchMPT` implementations with different tree sizes.

## Running the Benchmark

```bash
cargo bench
```

## What It Tests

The benchmark inserts key-value pairs using the `batch_upsert` method for both implementations at two scales:

- **1,000 nodes**: Medium-sized tree comparison
- **10,000 nodes**: Large tree comparison

For each size:

- **SimpleMPT**: The baseline implementation
- **BatchMPT**: The optimized batch implementation

## Viewing Results

After running the benchmark, you'll find:

- Console output with timing comparison
- HTML reports in `target/criterion/` directory

To open the HTML reports:

```bash
open target/criterion/simple_vs_batch_1000_nodes/report/index.html
open target/criterion/simple_vs_batch_10000_nodes/report/index.html
```

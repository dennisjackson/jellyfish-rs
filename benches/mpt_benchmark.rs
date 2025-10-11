use std::hash::RandomState;

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use jellyfish_rs::Hash;
use jellyfish_rs::mpt::{BatchMPT, DurableBatchMPT, MerklePatriciaTree, SimpleMPT};
use rand;
use rayon::range;
use sha2::{Digest, Sha256};

/// Generate deterministic test data
fn generate_test_data(count: usize) -> Vec<(Hash, Hash)> {
    let start = rand::random::<u32>();
    (start..start + count as u32)
        .map(|i| {
            let mut key_hasher = Sha256::new();
            key_hasher.update(b"key");
            key_hasher.update(i.to_le_bytes());
            let key: Hash = key_hasher.finalize().into();

            let mut value_hasher = Sha256::new();
            value_hasher.update(b"value");
            value_hasher.update(i.to_le_bytes());
            let value: Hash = value_hasher.finalize().into();

            (key, value)
        })
        .collect()
}

fn benchmark_fresh_1000(c: &mut Criterion) {
    let mut group = c.benchmark_group("fresh_1000_nodes");
    let data = generate_test_data(1000);

    // Configure throughput to report insertions per second
    group.throughput(Throughput::Elements(1000));

    group.bench_function("simple_implementation", |b| {
        b.iter(|| {
            let mut tree = SimpleMPT::new();
            tree.batch_upsert(black_box(&data));
            tree
        });
    });

    group.bench_function("batch_implementation", |b| {
        b.iter(|| {
            let mut tree = BatchMPT::new();
            tree.batch_upsert(black_box(&data));
            tree
        });
    });

    group.bench_function("durable_batch_implementation", |b| {
        b.iter(|| {
            let mut tree = DurableBatchMPT::new();
            tree.batch_upsert(black_box(&data));
            tree
        });
    });

    group.finish();
}

fn benchmark_fresh_10_000(c: &mut Criterion) {
    let mut group = c.benchmark_group("fresh_10_000_nodes");
    let data = generate_test_data(10_000);

    // Configure throughput to report insertions per second
    group.throughput(Throughput::Elements(10_000));

    group.bench_function("simple_implementation", |b| {
        b.iter(|| {
            let mut tree = SimpleMPT::new();
            tree.batch_upsert(black_box(&data));
            tree
        });
    });

    group.bench_function("batch_implementation", |b| {
        b.iter(|| {
            let mut tree = BatchMPT::new();
            tree.batch_upsert(black_box(&data));
            tree
        });
    });

    group.bench_function("durable_batch_implementation", |b| {
        b.iter(|| {
            let mut tree = DurableBatchMPT::new();
            tree.batch_upsert(black_box(&data));
            tree
        });
    });

    group.finish();
}

fn benchmark_incremental_on_large_tree(c: &mut Criterion) {
    let mut group = c.benchmark_group("incremental_10000_on_100000_base");
    group.sample_size(10);

    // Generate initial 100,000 nodes
    let base_data = generate_test_data(100_000);
    // Generate additional 10,000 nodes to insert
    let incremental_data: Vec<(Hash, Hash)> = (100_000usize..110_000usize)
        .map(|i| {
            let mut key_hasher = Sha256::new();
            key_hasher.update(b"key");
            key_hasher.update(i.to_le_bytes());
            let key: Hash = key_hasher.finalize().into();

            let mut value_hasher = Sha256::new();
            value_hasher.update(b"value");
            value_hasher.update(i.to_le_bytes());
            let value: Hash = value_hasher.finalize().into();

            (key, value)
        })
        .collect();

    // Configure throughput to report insertions per second
    group.throughput(Throughput::Elements(10_000));

    group.bench_function("simple_implementation", |b| {
        b.iter_batched(
            || {
                let mut tree = SimpleMPT::new();
                tree.batch_upsert(&base_data);
                tree
            },
            |mut tree| {
                tree.batch_upsert(black_box(&incremental_data));
                tree
            },
            criterion::BatchSize::LargeInput,
        );
    });

    group.bench_function("batch_implementation", |b| {
        b.iter_batched(
            || {
                let mut tree = BatchMPT::new();
                tree.batch_upsert(&base_data);
                tree
            },
            |mut tree| {
                tree.batch_upsert(black_box(&incremental_data));
                tree
            },
            criterion::BatchSize::LargeInput,
        );
    });
    group.finish();
}

fn benchmark_durable_incremental_on_large_tree(c: &mut Criterion) {
    let mut group = c.benchmark_group("durable_incremental_1000_on_10000_base");
    group.sample_size(10);
    // Generate initial 100,000 nodes
    let base_data = generate_test_data(10_000);
    // Generate additional 1,000 nodes to insert
    let incremental_data: Vec<(Hash, Hash)> = (10000usize..11000usize)
        .map(|i| {
            let mut key_hasher = Sha256::new();
            key_hasher.update(b"key");
            key_hasher.update(i.to_le_bytes());
            let key: Hash = key_hasher.finalize().into();

            let mut value_hasher = Sha256::new();
            value_hasher.update(b"value");
            value_hasher.update(i.to_le_bytes());
            let value: Hash = value_hasher.finalize().into();

            (key, value)
        })
        .collect();

    // Configure throughput to report insertions per second
    group.throughput(Throughput::Elements(10_000));

    //TODO: Performance regression here
    group.bench_function("durable_batch_implementation", |b| {
        b.iter_batched(
            || {
                let mut tree = DurableBatchMPT::new();
                tree.batch_upsert(&base_data);
                tree
            },
            |mut tree| {
                tree.batch_upsert(black_box(&incremental_data));
                tree
            },
            criterion::BatchSize::LargeInput,
        );
    });
    group.finish();
}

fn benchmark_batch_sizes_10k(c: &mut Criterion) {
    let mut group = c.benchmark_group("durable_batch_sizes_10k_nodes");
    group.sample_size(10);

    // Generate all 100,000 nodes once
    let all_data = generate_test_data(10_000);

    // Configure throughput to report insertions per second
    group.throughput(Throughput::Elements(10_000));

    // Benchmark with batch size 500
    group.bench_function("batch_size_500", |b| {
        b.iter(|| {
            let mut tree = DurableBatchMPT::new();
            for chunk in all_data.chunks(500) {
                tree.batch_upsert(black_box(chunk));
            }
            tree
        });
    });

    // Benchmark with batch size 1000
    group.bench_function("batch_size_1000", |b| {
        b.iter(|| {
            let mut tree = DurableBatchMPT::new();
            for chunk in all_data.chunks(1000) {
                tree.batch_upsert(black_box(chunk));
            }
            tree
        });
    });

    // Benchmark with batch size 10,000
    group.bench_function("batch_size_10000", |b| {
        b.iter(|| {
            let mut tree = DurableBatchMPT::new();
            for chunk in all_data.chunks(10_000) {
                tree.batch_upsert(black_box(chunk));
            }
            tree
        });
    });

    group.finish();
}

fn benchmark_batch_sizes_1_000_000(c: &mut Criterion) {
    // return;
    let mut group = c.benchmark_group("durable_batch_sizes_1m_nodes");
    group.sample_size(10);

    let all_data = generate_test_data(10_000);

    let mut tree = DurableBatchMPT::new();
    for i in 0..100 {
        // println!("Inserting batch {}/100", i + 1);
        tree.batch_upsert(generate_test_data(1_000_000).as_slice());
    }
    tree.batch_upsert(generate_test_data(1).as_slice());
    // println!("Finished persisting");
    // Configure throughput to report insertions per second
    group.throughput(Throughput::Elements(10_000));

    // Benchmark with batch size 500
    group.bench_function("batch_size_500", |b| {
        b.iter(|| {
            for chunk in all_data.chunks(500) {
                tree.batch_upsert(black_box(chunk));
            }
        });
    });

    group.bench_function("batch_size_1000", |b| {
        b.iter(|| {
            for chunk in all_data.chunks(1000) {
                tree.batch_upsert(black_box(chunk));
            }
        });
    });

    group.bench_function("batch_size_10000", |b| {
        b.iter(|| {
            for chunk in all_data.chunks(10_000) {
                tree.batch_upsert(black_box(chunk));
            }
        });
    });

    // println!("Done");
    group.finish();
}

criterion_group!(
    benches,
    benchmark_fresh_1000,
    benchmark_fresh_10_000,
    benchmark_incremental_on_large_tree,
    benchmark_durable_incremental_on_large_tree,
    benchmark_batch_sizes_10k,
    benchmark_batch_sizes_1_000_000,
);

criterion_main!(benches);

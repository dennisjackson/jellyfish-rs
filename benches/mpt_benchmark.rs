use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use jellyfish_rs::Hash;
use jellyfish_rs::mpt::{BatchMPT, DurableBatchMPT, MerklePatriciaTree, SimpleMPT};
use sha2::{Digest, Sha256};

/// Generate deterministic test data
fn generate_test_data(count: usize) -> Vec<(Hash, Hash)> {
    (0..count)
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

fn benchmark_simple_vs_batch_1000(c: &mut Criterion) {
    let mut group = c.benchmark_group("simple_vs_batch_1000_nodes");
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

fn benchmark_simple_vs_batch_10000(c: &mut Criterion) {
    let mut group = c.benchmark_group("simple_vs_batch_10000_nodes");
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

criterion_group!(
    benches,
    benchmark_simple_vs_batch_1000,
    benchmark_simple_vs_batch_10000,
    benchmark_incremental_on_large_tree
);
criterion_main!(benches);

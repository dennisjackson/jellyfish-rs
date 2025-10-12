use std::sync::Arc;

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use jellyfish_rs::Hash;
use jellyfish_rs::mpt::{BatchMPT, DurableBatchMPT, MerklePatriciaTree, SimpleMPT};
use rand;
use sha2::{Digest, Sha256};

/// Generate deterministic test data
fn generate_test_data(count: usize) -> Vec<(Hash, Hash)> {
    let start = rand::random::<u32>();
    generate_range_test_data(start, count)
}

fn generate_range_test_data(start: u32, count: usize) -> Vec<(Hash, Hash)> {
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

struct FreshScenario {
    group_name: &'static str,
    element_count: usize,
    sample_size: Option<usize>,
}

const FRESH_SCENARIOS: [FreshScenario; 2] = [
    FreshScenario {
        group_name: "fresh_1000_nodes",
        element_count: 1_000,
        sample_size: None,
    },
    FreshScenario {
        group_name: "fresh_10_000_nodes",
        element_count: 10_000,
        sample_size: None,
    },
];

fn benchmark_fresh(c: &mut Criterion) {
    for scenario in FRESH_SCENARIOS {
        let mut group = c.benchmark_group(scenario.group_name);
        if let Some(sample_size) = scenario.sample_size {
            group.sample_size(sample_size);
        }
        group.throughput(Throughput::Elements(scenario.element_count as u64));

        let data = Arc::new(generate_test_data(scenario.element_count));

        {
            let data = Arc::clone(&data);
            group.bench_function("simple_implementation", move |b| {
                let data = Arc::clone(&data);
                b.iter(|| {
                    let mut tree = SimpleMPT::new();
                    tree.batch_upsert(black_box(data.as_slice()));
                    tree
                });
            });
        }

        {
            let data = Arc::clone(&data);
            group.bench_function("batch_implementation", move |b| {
                let data = Arc::clone(&data);
                b.iter(|| {
                    let mut tree = BatchMPT::new();
                    tree.batch_upsert(black_box(data.as_slice()));
                    tree
                });
            });
        }

        {
            let data = Arc::clone(&data);
            group.bench_function("durable_batch_implementation", move |b| {
                let data = Arc::clone(&data);
                b.iter(|| {
                    let mut tree = DurableBatchMPT::new();
                    tree.batch_upsert(black_box(data.as_slice()));
                    tree
                });
            });
        }

        group.finish();
    }
}

struct DurableBatchScenario {
    group_name: &'static str,
    sample_size: usize,
    base_tree_size: usize,
    incremental_count: usize,
}

const DURABLE_BATCH_SCENARIOS: [DurableBatchScenario; 3] = [
    DurableBatchScenario {
        group_name: "durable_batch_sizes_10k_nodes",
        sample_size: 10,
        base_tree_size: 0,
        incremental_count: 10_000,
    },
    DurableBatchScenario {
        group_name: "durable_batch_sizes_100k_nodes",
        sample_size: 10,
        base_tree_size: 100_000,
        incremental_count: 10_000,
    },
    DurableBatchScenario {
        group_name: "durable_batch_sizes_1m_nodes",
        sample_size: 10,
        base_tree_size: 1_000_000,
        incremental_count: 10_000,
    },
];

const BATCH_SIZES: [usize; 3] = [500, 1000, 10_000];

fn benchmark_durable_batch_sizes(c: &mut Criterion) {
    for scenario in DURABLE_BATCH_SCENARIOS {
        let mut group = c.benchmark_group(scenario.group_name);
        group.sample_size(scenario.sample_size);
        group.throughput(Throughput::Elements(scenario.incremental_count as u64));

        let incremental_data = Arc::new(generate_test_data(scenario.incremental_count));
        let base_data = if scenario.base_tree_size > 0 {
            Some(Arc::new(generate_test_data(scenario.base_tree_size)))
        } else {
            None
        };

        for &chunk_size in &BATCH_SIZES {
            let bench_name = format!("batch_size_{}", chunk_size);
            let incremental_data = Arc::clone(&incremental_data);
            let base_data = base_data.as_ref().map(Arc::clone);

            group.bench_function(bench_name, move |b| {
                let incremental_data = Arc::clone(&incremental_data);
                let base_data = base_data.as_ref().map(Arc::clone);

                b.iter_batched(
                    {
                        let base_data = base_data.clone();
                        move || {
                            let mut tree = DurableBatchMPT::new();
                            if let Some(base_data) = base_data.as_ref() {
                                tree.set_safety_mode(false);
                                tree.batch_upsert(base_data.as_slice());
                                tree.set_safety_mode(true);
                            }
                            tree
                        }
                    },
                    {
                        let incremental_data = Arc::clone(&incremental_data);
                        move |mut tree| {
                            for chunk in incremental_data.as_slice().chunks(chunk_size) {
                                tree.batch_upsert(black_box(chunk));
                            }
                            tree
                        }
                    },
                    criterion::BatchSize::LargeInput,
                );
            });
        }

        group.finish();
    }
}

criterion_group!(
    benches,
    benchmark_fresh,
    benchmark_durable_batch_sizes,
);

criterion_main!(benches);

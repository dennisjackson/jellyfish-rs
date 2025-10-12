use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use jellyfish_rs::Hash;
use jellyfish_rs::mpt::{BatchMPT, DurableBatchMPT, MerklePatriciaTree, SimpleMPT};
use log::debug;
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
    sample_size: usize,
}

const FRESH_SCENARIOS: [FreshScenario; 3] = [
    FreshScenario {
        group_name: "fresh_1k_nodes",
        element_count: 1_000,
        sample_size: 10,
    },
    FreshScenario {
        group_name: "fresh_10k_nodes",
        element_count: 10_000,
        sample_size: 10,
    },
    FreshScenario {
        group_name: "fresh_100k_nodes",
        element_count: 100_000,
        sample_size: 10,
    },
];

fn benchmark_fresh(c: &mut Criterion) {
    for scenario in FRESH_SCENARIOS {
        let mut group = c.benchmark_group(scenario.group_name);
        group.sample_size(scenario.sample_size);
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
    sample_size: usize,
    base_tree_size: usize,
    safety_mode_enabled: bool,
    cold_cache: bool,
}

const DURABLE_BATCH_SCENARIOS: [DurableBatchScenario; 5] = [
    DurableBatchScenario {
        sample_size: 10,
        base_tree_size: 0,
        safety_mode_enabled: true,
        cold_cache: false,
    },
    DurableBatchScenario {
        sample_size: 10,
        base_tree_size: 1_000_000,
        safety_mode_enabled: true,
        cold_cache: false,
    },
    DurableBatchScenario {
        sample_size: 10,
        base_tree_size: 1_000_000,
        safety_mode_enabled: true,
        cold_cache: true,
    },
    DurableBatchScenario {
        sample_size: 10,
        base_tree_size: 0,
        safety_mode_enabled: false,
        cold_cache: false,
    },
    DurableBatchScenario {
        sample_size: 10,
        base_tree_size: 1_000_000,
        safety_mode_enabled: false,
        cold_cache: false,
    },
];

const BATCH_SIZES: [usize; 4] = [1_000, 5_000, 10_000,20_000];

fn benchmark_durable_batch_sizes(c: &mut Criterion) {
    let _ = env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Warn)
        .try_init();
    for scenario in DURABLE_BATCH_SCENARIOS {
        let mut group = c.benchmark_group(format!(
            "durable_batch_base_{}_safety_{}_cold_cache_{}",
            scenario.base_tree_size,
            scenario.safety_mode_enabled,
            scenario.cold_cache
        ));
        group.sample_size(scenario.sample_size);

        let base_data = if scenario.base_tree_size > 0 {
            Some(Arc::new(generate_test_data(scenario.base_tree_size)))
        } else {
            None
        };

        let base_template_path = base_data.as_ref().map(|data| {
            let template_path = unique_db_path("durable_batch_base_template");
            let template_path_str = template_path.to_string_lossy().to_string();

            {
                let mut tree = DurableBatchMPT::new_with_path(&template_path_str)
                    .expect("Failed to create base DurableBatch template");
                tree.set_safety_mode(false);
                tree.batch_upsert(data.as_slice());
                tree.set_safety_mode(scenario.safety_mode_enabled);
            }

            template_path
        });
        debug!("Base template path: {:?}", base_template_path);
        let base_template_arc = base_template_path
            .as_ref()
            .map(|path| Arc::new(path.clone()));

        for &chunk_size in &BATCH_SIZES {
            let incremental_data = Arc::new(generate_test_data(chunk_size*5));
            group.throughput(Throughput::Elements(chunk_size as u64));
            let bench_name = format!("batch_size_{}", chunk_size);
            let incremental_data = Arc::clone(&incremental_data);
            let base_template_arc = base_template_arc.clone();
            let safety_mode_enabled = scenario.safety_mode_enabled;

            group.bench_function(bench_name, move |b| {
                let incremental_data = Arc::clone(&incremental_data);
                let setup_template = base_template_arc.clone();

                b.iter_batched(
                    {
                        let setup_template = setup_template.clone();
                        move || {
                            let db_path = unique_db_path("durable_batch_iteration");
                            if let Some(template_path) = setup_template.as_ref() {
                                copy_sqlite_database(template_path.as_path(), &db_path)
                                    .expect("Failed to copy base database for benchmark iteration");
                            }

                            let db_path_str = db_path.to_string_lossy().to_string();
                            if scenario.base_tree_size == 0 {
                                let mut tree = DurableBatchMPT::new_with_path(&db_path_str)
                                    .expect("Failed to open existing DurableBatch tree for benchmark iteration");
                                tree.set_safety_mode(safety_mode_enabled);
                                if !scenario.cold_cache && scenario.base_tree_size > 0 {
                                    tree.batch_upsert(&generate_test_data(1_000));
                                }
                                (tree, db_path)
                            }
                            else {
                            let mut tree =
                                DurableBatchMPT::new_existing_with_path(&db_path_str)
                                    .expect("Failed to open existing DurableBatch tree for benchmark iteration");
                            tree.set_safety_mode(safety_mode_enabled);
                                if !scenario.cold_cache && scenario.base_tree_size > 0  {
                                    tree.batch_upsert(&generate_test_data(1_000));
                                }
                            (tree, db_path)
                        }}
                    },
                    {
                        let incremental_data = Arc::clone(&incremental_data);
                        move |(mut tree, db_path)| {
                            if scenario.cold_cache {
                                tree.clear_cache();
                            }
                            for chunk in incremental_data.as_slice().chunks(chunk_size) {
                                tree.batch_upsert(black_box(chunk));
                            }
                            drop(tree);
                            cleanup_sqlite_database(&db_path);
                        }
                    },
                    criterion::BatchSize::LargeInput,
                );
            });
        }

        group.finish();

        if let Some(path) = base_template_path {
            cleanup_sqlite_database(&path);
        }
    }
}

criterion_group!(benches, benchmark_fresh, benchmark_durable_batch_sizes,);

criterion_main!(benches);

fn unique_db_path(prefix: &str) -> PathBuf {
    let temp_dir = std::env::temp_dir();
    let random: u64 = rand::random();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("System time before UNIX_EPOCH")
        .as_nanos();
    temp_dir.join(format!("{prefix}_{timestamp}_{random:016x}.db"))
}

fn copy_sqlite_database(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::copy(src, dst)?;
    for extension in ["db-wal", "db-shm"] {
        let src_sidecar = src.with_extension(extension);
        if src_sidecar.exists() {
            let dst_sidecar = dst.with_extension(extension);
            fs::copy(src_sidecar, dst_sidecar)?;
        }
    }
    Ok(())
}

fn cleanup_sqlite_database(path: &Path) {
    let _ = fs::remove_file(path);
    for extension in ["db-wal", "db-shm"] {
        let _ = fs::remove_file(path.with_extension(extension));
    }
}

use divan::{self, black_box, counter::ItemsCount};
use jellyfish_rs::Hash;
use jellyfish_rs::mpt::{
    MerklePatriciaTree, RockLeafMPT, RockSparseMPT, RocksParTransMPT, RocksTransRelMPT,
};
use rand::{RngCore, SeedableRng, rngs::StdRng};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir; // Reintroduce TempDir for automatic cleanup

// Produce a unique suffix for a directory based on bench name, time and pid.
// Avoid external RNG dependencies to keep benches lean and deterministic-ish while
// still vanishingly unlikely to collide (time + pid + address entropy).
fn unique_dir_name(base: &str) -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // Mix in pid and an address of a stack value for a touch more variance.
    let pid = std::process::id() as u128;
    let addr = (&ts as *const u128 as usize) as u128; // not cryptographically strong; fine here.
    let mix = ts ^ pid ^ addr;
    format!("{base}-{:x}", mix)
}

// #[global_allocator]
// static GLOBAL_ALLOC: AllocProfiler = AllocProfiler::system();

// Number of records per phase for large inserts.
const RECORDS_PER_PHASE: usize = 5_000_000;
// Chunk size for streaming generation to avoid allocating gigantic vectors.
const GEN_CHUNK_SIZE: usize = 10_000;
// Small loading benchmark size.
const SECOND_LOAD: usize = 1000;

// Expensive benches use tiny sample sizes to avoid hours of runtime.
const LARGE_SAMPLE_COUNT: u32 = 1; // statistical samples (loops)
const LARGE_SAMPLE_SIZE: u32 = 1; // iterations per sample

fn generate_chunk(start: u64, count: usize) -> Vec<(Hash, Hash)> {
    // Seed RNG from the starting index for deterministic chunks while avoiding heavy hashing.
    let mut rng = StdRng::seed_from_u64(start);
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let mut key = [0u8; 32];
        let mut value = [0u8; 32];
        rng.fill_bytes(&mut key);
        rng.fill_bytes(&mut value);
        out.push((key, value));
    }
    out
}

fn insert_streaming<T: MerklePatriciaTree>(tree: &mut T, start_index: u64, total: usize) {
    let mut inserted = 0usize;
    while inserted < total {
        let remaining = total - inserted;
        let this_chunk = remaining.min(GEN_CHUNK_SIZE);
        let chunk = generate_chunk(start_index + inserted as u64, this_chunk);
        tree.batch_upsert(black_box(chunk.as_slice()));
        inserted += this_chunk;
    }
}

// Shared base data directories for incremental benchmarks, created once per MPT type.
static SHARED_ROCK_LEAF_BASE: OnceLock<PathBuf> = OnceLock::new();
static SHARED_ROCK_SPARSE_BASE: OnceLock<PathBuf> = OnceLock::new();
static SHARED_ROCKS_PAR_TRANS_BASE: OnceLock<PathBuf> = OnceLock::new();
static SHARED_ROCKS_TRANS_REL_BASE: OnceLock<PathBuf> = OnceLock::new();

// Macro to create a pre-populated base tree for a given type.
macro_rules! create_base_tree {
    ($ty:ty, $base_path:expr) => {{
        if $base_path.exists() {
            std::fs::remove_dir_all(&$base_path).expect("cleanup old base");
        }
        std::fs::create_dir(&$base_path).expect("create base dir");
        let mut tree = <$ty>::new_with_path(&$base_path).expect("create base tree");
        insert_streaming(&mut tree, 0, RECORDS_PER_PHASE);
        tree.flush().ok();
    }};
}

// Helper to copy a directory recursively (simple implementation).
fn copy_dir_all(src: &PathBuf, dst: &PathBuf) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}
// Macros to eliminate repetitive benchmark boilerplate for each MPT variant.
macro_rules! bench_fresh {
    ($fn_name:ident, $ty:ty, $bench_name:literal, $base_lock:expr) => {
        #[divan::bench(
                    name = $bench_name,
                    sample_count = LARGE_SAMPLE_COUNT,
                    sample_size = LARGE_SAMPLE_SIZE,
                    counter = ItemsCount::new(RECORDS_PER_PHASE)
                )]
        fn $fn_name() {
            // Create a fresh tree for the benchmark measurement.
            let temp_dir = TempDir::new().expect("temp dir");
            let bench_path: PathBuf = temp_dir.path().join(unique_dir_name($bench_name));
            std::fs::create_dir(&bench_path).expect("create unique bench dir");
            let mut tree = <$ty>::new_with_path(&bench_path).expect("open unique path");
            insert_streaming(&mut tree, 0, RECORDS_PER_PHASE);
            black_box(tree.get_root_hash());
            tree.flush().ok();

            // After benchmarking, populate the shared base for incremental benchmarks to use.
            // This ensures incremental benchmarks have data available without duplicating work.
            $base_lock.get_or_init(|| {
                let base =
                    std::env::temp_dir().join(format!("jellyfish_bench_base_{}", stringify!($ty)));
                if !base.exists() {
                    copy_dir_all(&bench_path, &base).expect("copy to shared base");
                }
                base
            });
        }
    };
}

macro_rules! bench_incremental_reopen {
    ($fn_name:ident, $ty:ty, $bench_name:literal, $base_lock:expr) => {
        #[divan::bench(
                    name = $bench_name,
                    sample_count = LARGE_SAMPLE_COUNT,
                    sample_size = LARGE_SAMPLE_SIZE,
                    counter = ItemsCount::new(SECOND_LOAD)
                )]
        fn $fn_name(b: divan::Bencher) {
            // Get or create the shared base tree with 5M records.
            let base_path = $base_lock.get_or_init(|| {
                let base = std::env::temp_dir().join(format!("jellyfish_bench_{}", $bench_name));
                create_base_tree!($ty, base);
                base
            });

            b.bench(|| {
                // Copy the base tree to a temporary location for this iteration.
                let temp_dir = TempDir::new().expect("temp dir");
                let bench_path: PathBuf = temp_dir.path().join(unique_dir_name($bench_name));
                copy_dir_all(&base_path, &bench_path).expect("copy base tree");

                let mut tree = <$ty>::new_with_path(&bench_path).expect("reopen");
                insert_streaming(&mut tree, RECORDS_PER_PHASE as u64, SECOND_LOAD);
                black_box(tree.get_root_hash());
            });
        }
    };
}

// macro_rules! bench_load_small_after_large {
//     ($fn_name:ident, $ty:ty, $bench_name:literal) => {
//         #[divan::bench(
//             name = $bench_name,
//             sample_count = LOAD_SAMPLE_COUNT,
//             sample_size = LOAD_SAMPLE_SIZE,
//             counter = ItemsCount::new(LOAD_SMALL)
//         )]
//         fn $fn_name() {
//             // TempDir persists for preload + measurement, then cleans up.
//             let temp_dir = TempDir::new().expect("temp dir");
//             let bench_path: PathBuf = temp_dir.path().join(unique_dir_name($bench_name));
//             std::fs::create_dir(&bench_path).expect("create unique bench dir");
//             {
//                 let mut preload = <$ty>::new_with_path(&bench_path).expect("preload open");
//                 insert_streaming(&mut preload, 0, RECORDS_PER_PHASE);
//                 preload.flush().ok();
//             }
//             let mut tree = <$ty>::new_with_path(&bench_path).expect("reopen");
//             insert_streaming(&mut tree, RECORDS_PER_PHASE as u64, LOAD_SMALL);
//             black_box(tree.get_root_hash());
//         }
//     };
// }

// Generate concrete benchmark functions.
bench_fresh!(
    rock_leaf_fresh_5m,
    RockLeafMPT,
    "rock_leaf_fresh_5m",
    &SHARED_ROCK_LEAF_BASE
);
bench_fresh!(
    rock_sparse_fresh_5m,
    RockSparseMPT,
    "rock_sparse_fresh_5m",
    &SHARED_ROCK_SPARSE_BASE
);
bench_fresh!(
    rocks_par_trans_fresh_5m,
    RocksParTransMPT,
    "rocks_par_trans_fresh_5m",
    &SHARED_ROCKS_PAR_TRANS_BASE
);
bench_fresh!(
    rocks_trans_rel_fresh_5m,
    RocksTransRelMPT,
    "rocks_trans_rel_fresh_5m",
    &SHARED_ROCKS_TRANS_REL_BASE
);

bench_incremental_reopen!(
    rock_leaf_incremental_reopen,
    RockLeafMPT,
    "rock_leaf_incremental_reopen_5m_plus_5m",
    &SHARED_ROCK_LEAF_BASE
);
bench_incremental_reopen!(
    rock_sparse_incremental_reopen,
    RockSparseMPT,
    "rock_sparse_incremental_reopen_5m_plus_5m",
    &SHARED_ROCK_SPARSE_BASE
);
bench_incremental_reopen!(
    rocks_par_trans_incremental_reopen,
    RocksParTransMPT,
    "rocks_par_trans_incremental_reopen_5m_plus_5m",
    &SHARED_ROCKS_PAR_TRANS_BASE
);
bench_incremental_reopen!(
    rocks_trans_rel_incremental_reopen,
    RocksTransRelMPT,
    "rocks_trans_rel_incremental_reopen_5m_plus_5m",
    &SHARED_ROCKS_TRANS_REL_BASE
);

fn main() {
    divan::main();
}

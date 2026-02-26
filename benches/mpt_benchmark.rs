use divan::{self, black_box, counter::ItemsCount};
use jellyfish_rs::Hash;
use jellyfish_rs::mpt::{MerklePatriciaTree, RocksTransRelMPT};
use rand::{RngCore, SeedableRng, rngs::StdRng};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

// Produce a unique suffix for a directory based on bench name, time and pid.
fn unique_dir_name(base: &str) -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let pid = std::process::id() as u128;
    let addr = (&ts as *const u128 as usize) as u128;
    let mix = ts ^ pid ^ addr;
    format!("{base}-{:x}", mix)
}

// Number of records per phase for large inserts.
const RECORDS_PER_PHASE: usize = 5_000_000;
// Chunk size for streaming generation to avoid allocating gigantic vectors.
const GEN_CHUNK_SIZE: usize = 10_000;
// Small loading benchmark size.
const SECOND_LOAD: usize = 1000;

// Expensive benches use tiny sample sizes to avoid hours of runtime.
const LARGE_SAMPLE_COUNT: u32 = 1;
const LARGE_SAMPLE_SIZE: u32 = 1;

fn generate_chunk(start: u64, count: usize) -> Vec<(Hash, Hash)> {
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

// Shared base data directory for incremental benchmarks, created once.
static SHARED_ROCKS_TRANS_REL_BASE: OnceLock<PathBuf> = OnceLock::new();

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

#[divan::bench(
    name = "rocks_trans_rel_fresh_5m",
    sample_count = LARGE_SAMPLE_COUNT,
    sample_size = LARGE_SAMPLE_SIZE,
    counter = ItemsCount::new(RECORDS_PER_PHASE)
)]
fn rocks_trans_rel_fresh_5m() {
    let temp_dir = TempDir::new().expect("temp dir");
    let bench_path: PathBuf = temp_dir
        .path()
        .join(unique_dir_name("rocks_trans_rel_fresh_5m"));
    std::fs::create_dir(&bench_path).expect("create unique bench dir");
    let mut tree = RocksTransRelMPT::new_with_path(&bench_path).expect("open unique path");
    insert_streaming(&mut tree, 0, RECORDS_PER_PHASE);
    black_box(tree.get_root_hash());
    tree.flush().ok();

    // Populate shared base for incremental benchmarks.
    SHARED_ROCKS_TRANS_REL_BASE.get_or_init(|| {
        let base = std::env::temp_dir().join(format!(
            "jellyfish_bench_base_{}",
            stringify!(RocksTransRelMPT)
        ));
        if !base.exists() {
            copy_dir_all(&bench_path, &base).expect("copy to shared base");
        }
        base
    });
}

#[divan::bench(
    name = "rocks_trans_rel_incremental_reopen_5m_plus_1k",
    sample_count = LARGE_SAMPLE_COUNT,
    sample_size = LARGE_SAMPLE_SIZE,
    counter = ItemsCount::new(SECOND_LOAD)
)]
fn rocks_trans_rel_incremental_reopen(b: divan::Bencher) {
    let base_path = SHARED_ROCKS_TRANS_REL_BASE.get_or_init(|| {
        let base = std::env::temp_dir()
            .join("jellyfish_bench_rocks_trans_rel_incremental_reopen_5m_plus_1k");
        create_base_tree!(RocksTransRelMPT, base);
        base
    });

    b.bench(|| {
        let temp_dir = TempDir::new().expect("temp dir");
        let bench_path: PathBuf = temp_dir.path().join(unique_dir_name(
            "rocks_trans_rel_incremental_reopen_5m_plus_1k",
        ));
        copy_dir_all(&base_path, &bench_path).expect("copy base tree");

        let mut tree = RocksTransRelMPT::new_with_path(&bench_path).expect("reopen");
        insert_streaming(&mut tree, RECORDS_PER_PHASE as u64, SECOND_LOAD);
        black_box(tree.get_root_hash());
    });
}

fn main() {
    divan::main();
}

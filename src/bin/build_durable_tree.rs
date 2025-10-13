use jellyfish_rs::mpt::MerklePatriciaTree;
use jellyfish_rs::{DurableBatchMPT, Hash};
use log::info;
use sha2::{Digest, Sha256};
use std::env;
use std::error::Error;
use std::path::Path;
use std::time::{Duration, Instant};

fn main() {
    if let Err(err) = run() {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    init_logging();

    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() < 2 || args.len() > 3 {
        eprintln!(
            "usage: build_durable_tree <tree_size> <batch_size> [db_path]\n\
             tree_size and batch_size must be positive integers"
        );
        std::process::exit(1);
    }

    let tree_size: usize = args[0].parse()?;
    let batch_size: usize = args[1].parse()?;
    if tree_size == 0 {
        return Err("tree_size must be greater than zero".into());
    }
    if batch_size == 0 {
        return Err("batch_size must be greater than zero".into());
    }

    let db_path = args.get(2);
    let mut tree = if let Some(path) = db_path {
        ensure_parent(path)?;
        info!("Writing durable MPT to {}", path);
        DurableBatchMPT::new_with_path(path)?
    } else {
        info!("Using temporary database path for durable MPT");
        DurableBatchMPT::new()
    };

    info!(
        "Building durable MPT with tree_size={} and batch_size={}",
        tree_size, batch_size
    );
    let offset = rand::random::<u32>();
    let mut inserted = 0usize;
    let mut batches = 0usize;
    let mut total_duration = Duration::ZERO;
    while inserted < tree_size {
        let remaining = tree_size - inserted;
        let current_batch = remaining.min(batch_size);
        let entries = generate_batch(offset, inserted, current_batch);
        let batch_start = Instant::now();
        tree.batch_upsert(&entries);
        let batch_duration = batch_start.elapsed();
        total_duration += batch_duration;
        inserted += current_batch;
        batches += 1;
        info!(
            "Inserted {} entries in {:.3} ms (total inserted: {}, batches: {})",
            current_batch,
            batch_duration.as_secs_f64() * 1_000.0,
            inserted,
            batches
        );
    }

    let total_secs = total_duration.as_secs_f64();
    let avg_batch_ms = if batches > 0 {
        (total_secs / batches as f64) * 1_000.0
    } else {
        0.0
    };
    let throughput = if total_secs > 0.0 {
        tree_size as f64 / total_secs
    } else {
        0.0
    };

    info!(
        "Total time: {:.3} s, average per batch: {:.3} ms, throughput: {:.1} entries/s",
        total_secs, avg_batch_ms, throughput
    );

    if let Some(root_hash) = tree.get_root_hash() {
        info!(
            "Finished building tree. Root hash: {}",
            hex::encode(root_hash)
        );
    } else {
        info!("Finished building tree, but root hash is empty (tree has no nodes)");
    }

    Ok(())
}

fn init_logging() {
    use env_logger::{Env, Target};

    let mut builder = env_logger::Builder::from_env(Env::default().default_filter_or("info"));
    builder.target(Target::Stdout);
    builder.init();
}

fn ensure_parent(path_str: &str) -> Result<(), Box<dyn Error>> {
    let path = Path::new(path_str);
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn generate_batch(offset: u32, start: usize, count: usize) -> Vec<(Hash, Hash)> {
    (offset as usize + start..offset as usize + start + count)
        .map(|i| {
            let mut key_hasher = Sha256::new();
            key_hasher.update(b"key");
            key_hasher.update((i as u64).to_le_bytes());
            let key: Hash = key_hasher.finalize().into();

            let mut value_hasher = Sha256::new();
            value_hasher.update(b"value");
            value_hasher.update((i as u64).to_le_bytes());
            let value: Hash = value_hasher.finalize().into();

            (key, value)
        })
        .collect()
}

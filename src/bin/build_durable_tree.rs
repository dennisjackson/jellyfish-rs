use jellyfish_rs::mpt::MerklePatriciaTree;
use jellyfish_rs::{BatchMPT, DurableBatchMPT, Hash};
use log::info;
use sha2::{Digest, Sha256};
use std::env;
use std::error::Error;
use std::path::Path;
use std::time::{Duration, Instant};

const IN_MEMORY_FLAG: &str = "--in-memory";

enum Tree {
    Durable(DurableBatchMPT),
    InMemory(BatchMPT),
}

impl Tree {
    fn batch_upsert(&mut self, entries: &[(Hash, Hash)]) {
        match self {
            Tree::Durable(tree) => tree.batch_upsert(entries),
            Tree::InMemory(tree) => tree.batch_upsert(entries),
        }
    }

    fn get_root_hash(&self) -> Option<Hash> {
        match self {
            Tree::Durable(tree) => tree.get_root_hash(),
            Tree::InMemory(tree) => tree.get_root_hash(),
        }
    }
}

fn print_usage() {
    eprintln!(
        "usage: build_durable_tree [--in-memory] <tree_size> <window_size> <batch_size> [db_path]\n\
         tree_size, window_size, and batch_size must be positive integers\n\
         when --in-memory is used, db_path cannot be specified"
    );
}

fn exit_with_usage(code: i32) -> ! {
    print_usage();
    std::process::exit(code);
}

fn main() {
    if let Err(err) = run() {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    init_logging();

    let mut use_in_memory = false;
    let mut positional_args = Vec::new();

    for arg in env::args().skip(1) {
        match arg.as_str() {
            IN_MEMORY_FLAG => {
                use_in_memory = true;
            }
            "--help" | "-h" => exit_with_usage(0),
            _ if arg.starts_with("--") => {
                eprintln!("Unknown option: {arg}");
                exit_with_usage(1);
            }
            _ => positional_args.push(arg),
        }
    }

    let positional_len = positional_args.len();
    if positional_len < 3 {
        eprintln!("tree_size, window_size, and batch_size are required");
        exit_with_usage(1);
    }
    if use_in_memory && positional_len > 3 {
        eprintln!("db_path is not supported when using --in-memory");
        exit_with_usage(1);
    }
    if !use_in_memory && positional_len > 4 {
        eprintln!("Too many arguments provided");
        exit_with_usage(1);
    }

    let tree_size: usize = positional_args[0].parse()?;
    let window_size: usize = positional_args[1].parse()?;
    let batch_size: usize = positional_args[2].parse()?;
    if tree_size == 0 {
        return Err("tree_size must be greater than zero".into());
    }
    if window_size == 0 {
        return Err("window_size must be greater than zero".into());
    }
    if batch_size == 0 {
        return Err("batch_size must be greater than zero".into());
    }

    let db_path = if use_in_memory {
        None
    } else {
        positional_args.get(3).map(|s| s.as_str())
    };

    let mut tree = if use_in_memory {
        info!("Using in-memory BatchMPT implementation");
        Tree::InMemory(BatchMPT::new())
    } else if let Some(path) = db_path {
        ensure_parent(path)?;
        info!("Writing durable MPT to {}", path);
        Tree::Durable(DurableBatchMPT::new_with_path(path)?)
    } else {
        info!("Using temporary database path for durable MPT");
        Tree::Durable(DurableBatchMPT::new())
    };

    info!(
        "Building {} MPT with tree_size={}, window_size={}, and batch_size={}",
        if use_in_memory {
            "in-memory"
        } else {
            "durable"
        },
        tree_size,
        window_size,
        batch_size
    );
    let offset = rand::random::<u32>();
    let mut total_inserted = 0usize;
    let mut batches = 0usize;
    let mut total_duration = Duration::ZERO;
    while total_inserted < tree_size {
        let start_index = total_inserted;
        let remaining = tree_size - start_index;
        let current_window = remaining.min(window_size);
        let mut entries = generate_entries(offset, start_index, current_window);
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        for chunk in entries.chunks(batch_size) {
            let batch_len = chunk.len();
            let batch_start = Instant::now();
            tree.batch_upsert(chunk);
            let batch_duration = batch_start.elapsed();
            total_duration += batch_duration;
            total_inserted += batch_len;
            batches += 1;
            info!(
                "Inserted {} entries in {:.3} ms (total inserted: {}, batches: {})",
                batch_len,
                batch_duration.as_secs_f64() * 1_000.0,
                total_inserted,
                batches
            );
        }
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

fn generate_entries(offset: u32, start: usize, count: usize) -> Vec<(Hash, Hash)> {
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

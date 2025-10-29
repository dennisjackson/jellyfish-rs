use indicatif::{ProgressBar, ProgressStyle};
use jellyfish_rs::mpt::{MerklePatriciaTree, SledBatchMPT};
use jellyfish_rs::{BatchMPT, Hash};
use log::info;
use std::env;
use std::error::Error;
use std::path::Path;
use std::time::Instant;

const IN_MEMORY_FLAG: &str = "--in-memory";

enum Tree {
    Durable(SledBatchMPT),
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

    fn len(&self) -> usize {
        match self {
            Tree::Durable(x) => x.enumerate_nodes().len(),
            Tree::InMemory(tree) => tree.enumerate_nodes().len(),
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

    let start = Instant::now();
    let mut tree = if use_in_memory {
        info!("Using in-memory BatchMPT implementation");
        Tree::InMemory(BatchMPT::new())
    } else if let Some(path) = db_path {
        ensure_parent(path)?;
        info!("Writing durable MPT to {}", path);
        Tree::Durable(SledBatchMPT::new_with_path(path)?)
    } else {
        info!("Using temporary database path for durable MPT");
        Tree::Durable(SledBatchMPT::new())
    };
    let startup_time = start.elapsed().as_secs_f64();
    let current_tree_size = tree.len();
    info!(
        "Initialized MPT of size {} in {:.3} s. Rate: {} /s",
        current_tree_size,
        startup_time,
        current_tree_size as f64 / startup_time
    );

    info!(
        "Building {} MPT with tree_size={}, insertions={}, window_size={}, and batch_size={}",
        if use_in_memory {
            "in-memory"
        } else {
            "durable"
        },
        human_count(current_tree_size),
        human_count(tree_size),
        human_count(window_size),
        human_count(batch_size)
    );
    let mut total_inserted = 0usize;
    let start = Instant::now();
    let pb = ProgressBar::new(tree_size as u64).with_style(
        ProgressStyle::with_template(
            "{wide_bar} {human_pos} / {human_len} - {percent}% - {per_sec} - {eta}",
        )
        .unwrap(),
    );
    while total_inserted < tree_size {
        let start_index = total_inserted;
        let remaining = tree_size - start_index;
        let current_window = remaining.min(window_size);
        let mut entries = generate_entries(current_window);
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        for chunk in entries.chunks(batch_size) {
            pb.inc(chunk.len() as u64);
            total_inserted += chunk.len();
            tree.batch_upsert(chunk);
        }
    }

    let total_secs = start.elapsed().as_secs_f64();
    let throughput = if total_secs > 0.0 {
        tree_size as f64 / total_secs
    } else {
        0.0
    };

    info!(
        "Total time: {:.3} s, throughput: {:.1} entries/s",
        total_secs, throughput
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

fn human_count(n: usize) -> String {
    // Format counts in SI units: K, M, B, T
    const UNITS: [&str; 5] = ["", "K", "M", "B", "T"];
    let mut value = n as f64;
    let mut unit_idx = 0usize;
    while value >= 1000.0 && unit_idx < UNITS.len() - 1 {
        value /= 1000.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{}", n)
    } else if value < 10.0 {
        format!("{:.2}{}", value, UNITS[unit_idx])
    } else if value < 100.0 {
        format!("{:.1}{}", value, UNITS[unit_idx])
    } else {
        format!("{:.0}{}", value, UNITS[unit_idx])
    }
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

fn generate_entries(count: usize) -> Vec<(Hash, Hash)> {
    let mut vec = Vec::with_capacity(count);
    let mut rng = fastrand::Rng::new();
    for _ in 0..count {
        let mut key = [0u8; 32];
        rng.fill(&mut key);
        let mut value = [0u8; 32];
        rng.fill(&mut value);
        vec.push((key, value));
    }
    vec
}

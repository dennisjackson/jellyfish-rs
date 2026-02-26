use indicatif::{ProgressBar, ProgressStyle};
use jellyfish_rs::mpt::{MerklePatriciaTree, RocksTransRelMPT};
use jellyfish_rs::{BatchMPT, DurableBatchMPT, Hash};
use log::info;
use std::env;
use std::error::Error;
use std::path::Path;
use std::time::Instant;

fn print_usage() {
    eprintln!(
        "usage: build_durable_tree [--backend <rocks|sqlite|memory>] <tree_size> <window_size> <batch_size> [db_path]\n\
         \n\
         Backends:\n\
         \x20 rocks   - RocksDB with frontier optimization (default)\n\
         \x20 sqlite  - SQLite-backed durable tree\n\
         \x20 memory  - In-memory only (db_path not allowed)\n\
         \n\
         All size arguments must be positive integers."
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

    let mut backend = "rocks".to_string();
    let mut positional_args = Vec::new();
    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--backend" | "-b" => {
                backend = args
                    .next()
                    .ok_or("--backend requires a value (rocks, sqlite, memory)")?;
            }
            // Keep old flag working
            "--in-memory" => {
                backend = "memory".to_string();
            }
            "--help" | "-h" => exit_with_usage(0),
            _ if arg.starts_with("--") => {
                eprintln!("Unknown option: {arg}");
                exit_with_usage(1);
            }
            _ => positional_args.push(arg),
        }
    }

    if positional_args.len() < 3 {
        eprintln!("tree_size, window_size, and batch_size are required");
        exit_with_usage(1);
    }
    if backend == "memory" && positional_args.len() > 3 {
        eprintln!("db_path is not supported with the memory backend");
        exit_with_usage(1);
    }
    if positional_args.len() > 4 {
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

    let db_path = positional_args.get(3).map(|s| s.as_str());

    let start = Instant::now();
    let mut tree: Box<dyn MerklePatriciaTree> = match backend.as_str() {
        "memory" => {
            info!("Using in-memory BatchMPT");
            Box::new(BatchMPT::new())
        }
        "sqlite" => {
            if let Some(path) = db_path {
                ensure_parent(path)?;
                info!("Using SQLite-backed DurableBatchMPT at {path}");
                Box::new(DurableBatchMPT::new_with_path(path)?)
            } else {
                info!("Using SQLite-backed DurableBatchMPT (temporary)");
                Box::new(DurableBatchMPT::new())
            }
        }
        "rocks" => {
            if let Some(path) = db_path {
                ensure_parent(path)?;
                info!("Using RocksDB-backed RocksTransRelMPT at {path}");
                Box::new(RocksTransRelMPT::new_with_path(path)?)
            } else {
                info!("Using RocksDB-backed RocksTransRelMPT (temporary)");
                Box::new(RocksTransRelMPT::new())
            }
        }
        other => {
            return Err(format!("Unknown backend: {other}. Use rocks, sqlite, or memory.").into());
        }
    };
    let startup_secs = start.elapsed().as_secs_f64();
    info!("Initialized in {startup_secs:.3} s");

    info!(
        "Building tree: backend={}, insertions={}, window_size={}, batch_size={}",
        backend,
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
        let remaining = tree_size - total_inserted;
        let current_window = remaining.min(window_size);
        let mut entries = generate_entries(current_window);
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        for chunk in entries.chunks(batch_size) {
            pb.inc(chunk.len() as u64);
            total_inserted += chunk.len();
            tree.batch_upsert(chunk);
        }
    }
    pb.finish();

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
            "Root hash: {}",
            root_hash
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
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

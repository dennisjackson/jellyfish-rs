use indicatif::{ProgressBar, ProgressStyle};
use jellyfish_rs::mpt::{MerklePatriciaTree, RocksTransRelMPT};
use jellyfish_rs::{BatchMPT, DurableBatchMPT, Hash};
use log::info;
use std::env;
use std::error::Error;
use std::path::Path;
use std::time::{Duration, Instant};

fn print_usage() {
    eprintln!(
        "usage: build_durable_tree [OPTIONS] <tree_size> <window_size> <batch_size> [db_path]\n\
         \n\
         Options:\n\
         \x20 --backend <rocks|sqlite|memory>  Storage backend (default: rocks)\n\
         \x20 --timeout <seconds>              Stop after this many seconds\n\
         \x20 --in-memory                      Alias for --backend memory\n\
         \n\
         Backends:\n\
         \x20 rocks   - RocksDB with frontier optimization\n\
         \x20 sqlite  - SQLite-backed durable tree\n\
         \x20 memory  - In-memory only (db_path not allowed)\n\
         \n\
         All size arguments must be positive integers.\n\
         With --timeout, tree_size acts as a maximum; insertion stops at whichever limit is hit first."
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
    let mut timeout_secs: Option<f64> = None;
    let mut positional_args = Vec::new();
    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--backend" | "-b" => {
                backend = args
                    .next()
                    .ok_or("--backend requires a value (rocks, sqlite, memory)")?;
            }
            "--timeout" | "-t" => {
                let val: f64 = args
                    .next()
                    .ok_or("--timeout requires a value in seconds")?
                    .parse()?;
                if val <= 0.0 {
                    return Err("--timeout must be positive".into());
                }
                timeout_secs = Some(val);
            }
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
    let deadline = timeout_secs.map(|s| Instant::now() + Duration::from_secs_f64(s));

    let init_start = Instant::now();
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
    let init_secs = init_start.elapsed().as_secs_f64();

    let limit_desc = match timeout_secs {
        Some(s) => format!("insertions={} (or {s}s timeout)", human_count(tree_size)),
        None => format!("insertions={}", human_count(tree_size)),
    };
    info!(
        "Building tree: backend={backend}, {limit_desc}, window_size={}, batch_size={}",
        human_count(window_size),
        human_count(batch_size)
    );

    let mut total_inserted = 0usize;
    let mut window_count = 0usize;
    let mut batch_times: Vec<f64> = Vec::new();
    let stopped_early;

    let insert_start = Instant::now();
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
        window_count += 1;

        for chunk in entries.chunks(batch_size) {
            if let Some(dl) = deadline {
                if Instant::now() >= dl {
                    stopped_early = true;
                    pb.finish();
                    // Jump to stats reporting
                    return print_stats(
                        &backend,
                        init_secs,
                        insert_start,
                        total_inserted,
                        window_count,
                        &batch_times,
                        stopped_early,
                        timeout_secs,
                        &tree,
                    );
                }
            }

            let batch_start = Instant::now();
            tree.batch_upsert(chunk);
            batch_times.push(batch_start.elapsed().as_secs_f64());

            total_inserted += chunk.len();
            pb.inc(chunk.len() as u64);
        }
    }
    pb.finish();
    stopped_early = false;

    print_stats(
        &backend,
        init_secs,
        insert_start,
        total_inserted,
        window_count,
        &batch_times,
        stopped_early,
        timeout_secs,
        &tree,
    )
}

#[allow(clippy::too_many_arguments)]
fn print_stats(
    backend: &str,
    init_secs: f64,
    insert_start: Instant,
    total_inserted: usize,
    window_count: usize,
    batch_times: &[f64],
    stopped_early: bool,
    timeout_secs: Option<f64>,
    tree: &Box<dyn MerklePatriciaTree>,
) -> Result<(), Box<dyn Error>> {
    let insert_secs = insert_start.elapsed().as_secs_f64();
    let throughput = if insert_secs > 0.0 {
        total_inserted as f64 / insert_secs
    } else {
        0.0
    };

    eprintln!();
    eprintln!("=== Benchmark Results ===");
    eprintln!("Backend:            {backend}");
    eprintln!("Init time:          {init_secs:.3} s");
    eprintln!(
        "Inserted:           {} entries{}",
        human_count(total_inserted),
        if stopped_early {
            format!(" (stopped at {:.1}s timeout)", timeout_secs.unwrap_or(0.0))
        } else {
            String::new()
        }
    );
    eprintln!("Insert time:        {insert_secs:.3} s");
    eprintln!("Throughput:         {throughput:.1} entries/s");
    eprintln!("Windows processed:  {window_count}");
    eprintln!("Batches processed:  {}", batch_times.len());

    if !batch_times.is_empty() {
        let mut sorted = batch_times.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let sum: f64 = sorted.iter().sum();
        let mean = sum / sorted.len() as f64;
        let p50 = percentile(&sorted, 50.0);
        let p95 = percentile(&sorted, 95.0);
        let p99 = percentile(&sorted, 99.0);
        let min = sorted[0];
        let max = sorted[sorted.len() - 1];

        eprintln!();
        eprintln!("--- Batch Latency ---");
        eprintln!("  mean:  {}", fmt_duration(mean));
        eprintln!("  p50:   {}", fmt_duration(p50));
        eprintln!("  p95:   {}", fmt_duration(p95));
        eprintln!("  p99:   {}", fmt_duration(p99));
        eprintln!("  min:   {}", fmt_duration(min));
        eprintln!("  max:   {}", fmt_duration(max));
    }

    if let Some(root_hash) = tree.get_root_hash() {
        eprintln!();
        eprintln!(
            "Root hash: {}",
            root_hash
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
    }

    Ok(())
}

fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.len() == 1 {
        return sorted[0];
    }
    let idx = (pct / 100.0) * (sorted.len() - 1) as f64;
    let lo = idx.floor() as usize;
    let hi = idx.ceil() as usize;
    if lo == hi {
        sorted[lo]
    } else {
        let frac = idx - lo as f64;
        sorted[lo] * (1.0 - frac) + sorted[hi] * frac
    }
}

fn fmt_duration(secs: f64) -> String {
    if secs < 0.001 {
        format!("{:.1} us", secs * 1_000_000.0)
    } else if secs < 1.0 {
        format!("{:.2} ms", secs * 1_000.0)
    } else {
        format!("{:.3} s", secs)
    }
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

use jellyfish_rs::{
    CensusSnapshot, Entry, Key, Metric, RocksFrontierConfig, RocksFrontierMPT, Value,
};
use log::info;
use std::env;
use std::error::Error;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, IsTerminal, Read as _, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

const DEFAULT_TIMEOUT_SECS: f64 = 30.0;
/// The recorded protocol; `ab.py`'s defaults match.
const DEFAULT_WINDOW_SIZE: usize = 100_000;
const DEFAULT_BATCH_SIZE: usize = 10_000;

const DEFAULT_MAX_DEPTH: u16 = RocksFrontierConfig::DEFAULT_MAX_DEPTH;

/// Appended inside the database directory, so a database carries its whole history.
/// RocksDB ignores file names it does not recognise.
const LOG_FILE_NAME: &str = "bench-log.jsonl";

/// Parse a flag's value, naming the flag in either failure.
fn flag_value<T>(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<T, Box<dyn Error>>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let raw = args
        .next()
        .ok_or_else(|| format!("{flag} requires a value"))?;
    raw.parse()
        .map_err(|err| format!("{flag}: {raw:?} is not a valid value ({err})").into())
}

fn print_usage() {
    eprintln!(
        "usage: bench [OPTIONS] <db_path>\n\
         \n\
         Inserts into the RocksDB database at <db_path> (created if absent) until the\n\
         timeout expires, or until --max-entries have been inserted.\n\
         \n\
         The log is <db_path>/{LOG_FILE_NAME}: one JSON object a line, appended. A 'run'\n\
         record per run, a 'batch' record per batch, a 'census' record per window.\n\
         \n\
         Options:\n\
         \x20 -t, --timeout <seconds>              Run duration (default: {DEFAULT_TIMEOUT_SECS})\n\
         \x20 -w, --window-size <n>                Entries generated per window (default: {DEFAULT_WINDOW_SIZE})\n\
         \x20 -c, --batch-size <n>                 Entries per batch_upsert call (default: {DEFAULT_BATCH_SIZE})\n\
         \x20 -l, --log-file <path>                Write the log here instead of with the database\n\
         \x20 -n, --max-entries <n>                Stop after inserting n entries (fixed-work A/B)\n\
         \x20 -s, --seed <u64>                     Key-stream seed (default: random; recorded in report and log)\n\
         \x20     --census-from <n>                Reset the write census once n entries are in\n\
         \x20     --max-depth <n>                  Ceiling on the tree-top level held in RAM (default\n\
         \x20                                      {DEFAULT_MAX_DEPTH}, max 28); the depth actually held\n\
         \x20                                      follows the leaf count\n\
         \x20     --frontier-cap <n>               Deepest frontier to advance to and persist (default\n\
         \x20                                      --max-depth minus 2)"
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

    let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
    let mut window_size = DEFAULT_WINDOW_SIZE;
    let mut batch_size = DEFAULT_BATCH_SIZE;
    let mut log_file_path: Option<String> = None;
    let mut max_entries: Option<usize> = None;
    let mut census_from: Option<usize> = None;
    let mut seed = random_seed();
    let mut max_depth: u16 = RocksFrontierConfig::DEFAULT_MAX_DEPTH;
    let mut frontier_cap: Option<u16> = None;
    let mut positional_args = Vec::new();
    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            // Accepted so recorded command lines still run; only rocks exists.
            "--backend" | "-b" => {
                let backend = args.next().ok_or("--backend requires a value")?;
                if backend != "rocks" {
                    return Err(format!("unknown backend: {backend}. Only rocks exists").into());
                }
            }
            "--max-entries" | "-n" => {
                max_entries = Some(flag_value(&mut args, &arg)?);
            }
            "--census-from" => {
                census_from = Some(flag_value(&mut args, &arg)?);
            }
            "--seed" | "-s" => {
                // 'random' is the default, still accepted so recorded command lines run.
                let raw = args.next().ok_or("--seed requires a value")?;
                if raw != "random" {
                    seed = raw
                        .parse()
                        .map_err(|err| format!("--seed: {raw:?} is not a valid value ({err})"))?;
                }
            }
            "--max-depth" => {
                max_depth = flag_value(&mut args, &arg)?;
            }
            "--frontier-cap" => {
                frontier_cap = Some(flag_value(&mut args, &arg)?);
            }
            "--timeout" | "-t" => {
                timeout_secs = flag_value(&mut args, &arg)?;
            }
            "--window-size" | "-w" => {
                window_size = flag_value(&mut args, &arg)?;
            }
            "--batch-size" | "-c" => {
                batch_size = flag_value(&mut args, &arg)?;
            }
            "--log-file" | "-l" => {
                log_file_path = Some(args.next().ok_or("--log-file requires a path")?);
            }
            "--help" | "-h" => exit_with_usage(0),
            _ if arg.starts_with('-') => {
                eprintln!("Unknown option: {arg}");
                exit_with_usage(1);
            }
            _ => positional_args.push(arg),
        }
    }

    if positional_args.len() > 1 {
        eprintln!("Too many arguments provided");
        exit_with_usage(1);
    }
    let Some(db_path) = positional_args.first().map(String::as_str) else {
        eprintln!("A database directory is required");
        exit_with_usage(1);
    };
    if window_size == 0 {
        return Err("--window-size must be greater than zero".into());
    }
    if batch_size == 0 {
        return Err("--batch-size must be greater than zero".into());
    }
    if max_entries == Some(0) {
        return Err("--max-entries must be greater than zero".into());
    }
    if timeout_secs <= 0.0 {
        return Err("--timeout must be positive".into());
    }
    let config = match frontier_cap {
        Some(cap) => RocksFrontierConfig::with_depths(max_depth, cap),
        None => RocksFrontierConfig::with_max_depth(max_depth),
    };
    let frontier_cap = frontier_cap.unwrap_or(max_depth.saturating_sub(2));

    let deadline = Instant::now() + Duration::from_secs_f64(timeout_secs);

    let init_start = Instant::now();
    ensure_parent(db_path)?;
    info!("Opening RocksFrontierMPT at {db_path} with {config:?}");
    let mut tree = RocksFrontierMPT::open(db_path, config)?;
    let init_secs = init_start.elapsed().as_secs_f64();

    info!(
        "Benchmarking: backend=rocks, timeout={timeout_secs}s, window_size={}, batch_size={}",
        human_count(window_size),
        human_count(batch_size)
    );

    // JSON lines; `tools/bench_plot.py` reads them.
    let log_path = log_file_path.unwrap_or_else(|| default_log_path(db_path));
    let mut log_writer = {
        ensure_parent(&log_path)?;
        let mut w = BufWriter::new(open_append(&log_path)?);
        writeln!(
            w,
            "{{\"kind\":\"run\",\"started\":{:.3},\"seed\":{seed},\"window_size\":{window_size},\
             \"batch_size\":{batch_size},\"max_depth\":{max_depth},\
             \"frontier_cap\":{frontier_cap},\"db\":{},\"leaves_at_open\":{},\
             \"frontier_at_open\":{},\"levels_depth_at_open\":{}}}",
            unix_now(),
            json_string(db_path),
            tree.leaf_count(),
            tree.frontier_depth(),
            tree.levels_depth()
        )?;
        info!("Appending the insertion log to {log_path}");
        w
    };

    let mut total_inserted = 0usize;
    let mut window_count = 0usize;
    let mut batch_times: Vec<f64> = Vec::new();
    // The census covers the insert loop only, not the open.
    let mut rng = KeyStream::with_seed(seed);
    tree.census_reset();
    let mut census_base = CensusSnapshot::default();
    let mut census_base_entries = 0usize;
    let mut next_census_at = window_size;

    let insert_start = Instant::now();
    let mut progress = Progress::start();

    'outer: loop {
        if Instant::now() >= deadline {
            break;
        }
        if max_entries.is_some_and(|limit| total_inserted >= limit) {
            break;
        }

        let mut entries = generate_entries(&mut rng, window_size);
        // Sorted windows are the recorded protocol.
        entries.sort_unstable_by_key(|a| a.0);
        window_count += 1;

        for chunk in entries.chunks(batch_size) {
            if Instant::now() >= deadline {
                break 'outer;
            }
            // Land --max-entries exactly: a fixed-work A/B must insert the same keys.
            let chunk = match max_entries {
                Some(limit) if total_inserted + chunk.len() > limit => {
                    &chunk[..limit - total_inserted]
                }
                _ => chunk,
            };
            if chunk.is_empty() {
                break 'outer;
            }

            let batch_start = Instant::now();
            tree.batch_upsert(chunk);
            let batch_secs = batch_start.elapsed().as_secs_f64();
            batch_times.push(batch_secs);

            total_inserted += chunk.len();
            progress.advance(total_inserted);

            if census_from.is_some_and(|from| census_base_entries == 0 && total_inserted >= from) {
                census_base = tree.census_snapshot();
                census_base_entries = total_inserted;
                next_census_at = total_inserted + window_size;
            }

            writeln!(
                log_writer,
                "{{\"kind\":\"batch\",\"timestamp\":{:.3},\"elapsed_secs\":{:.6},\
                 \"total_inserted\":{total_inserted},\"leaf_count\":{},\
                 \"frontier_depth\":{},\"batch_entries\":{},\"batch_secs\":{batch_secs:.6},\
                 \"sorted_runs\":{},\"rss_bytes\":{}}}",
                unix_now(),
                insert_start.elapsed().as_secs_f64(),
                tree.leaf_count(),
                tree.frontier_depth(),
                chunk.len(),
                tree.sorted_runs(),
                proc_status_bytes("VmRSS:").unwrap_or(0)
            )?;

            if total_inserted >= next_census_at {
                write_census_record(
                    &mut log_writer,
                    insert_start.elapsed().as_secs_f64(),
                    total_inserted,
                    &tree,
                    total_inserted - census_base_entries,
                    &tree.census_snapshot().since(&census_base),
                )?;
                next_census_at = total_inserted + window_size;
            }

            if max_entries.is_some_and(|limit| total_inserted >= limit) {
                break 'outer;
            }
        }
    }
    progress.finish();

    let census = tree.census_snapshot().since(&census_base);
    let census_entries = total_inserted - census_base_entries;

    // A final census record naming the whole phase, whatever the window cadence reached.
    if census_entries > 0 {
        write_census_record(
            &mut log_writer,
            insert_start.elapsed().as_secs_f64(),
            total_inserted,
            &tree,
            census_entries,
            &census,
        )?;
    }
    log_writer.flush()?;

    // The stderr report is parsed by tools/ab.py: its format is an API.
    let insert_secs = insert_start.elapsed().as_secs_f64();
    let throughput = if insert_secs > 0.0 {
        total_inserted as f64 / insert_secs
    } else {
        0.0
    };

    eprintln!();
    eprintln!("=== Benchmark Results ===");
    eprintln!("Backend:            rocks");
    eprintln!("Seed:               {seed}");
    eprintln!("Init time:          {init_secs:.3} s");
    eprintln!("Timeout:            {timeout_secs:.1} s");
    eprintln!(
        "Inserted:           {} entries",
        human_count(total_inserted)
    );
    eprintln!("Insert time:        {insert_secs:.3} s");
    eprintln!("Throughput:         {throughput:.1} entries/s");
    eprintln!("Windows processed:  {window_count}");
    eprintln!("Batches processed:  {}", batch_times.len());
    let leaves = tree.leaf_count();
    let depth = tree.frontier_depth();
    let frontier_nodes = 1usize.checked_shl(u32::from(depth)).unwrap_or(usize::MAX);
    eprintln!("Leaves:             {}", human_count(leaves));
    eprintln!(
        "Frontier depth:     {depth} ({} leaves per frontier node)",
        leaves / frontier_nodes.max(1)
    );
    let levels_depth = tree.levels_depth();
    let scanned_positions = 1usize
        .checked_shl(u32::from(levels_depth))
        .unwrap_or(usize::MAX);
    eprintln!(
        "Levels held:        0..={levels_depth} of a {max_depth} ceiling ({} leaves per \
         scanned position)",
        leaves / scanned_positions.max(1)
    );
    eprintln!("Sorted runs:        {}", tree.sorted_runs());
    if let Some(rss) = proc_status_bytes("VmHWM:") {
        eprintln!("Peak RSS:           {:.2} GB", rss as f64 / 1e9);
    }

    if !batch_times.is_empty() {
        let mut sorted = batch_times.to_vec();
        sorted.sort_unstable_by(f64::total_cmp);
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

    print_census(&census, census_entries);

    if let Some(root_hash) = tree.get_root_hash() {
        eprintln!();
        eprintln!(
            "Root hash: {}",
            root_hash
                .as_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
    }

    Ok(())
}

/// A kB field of `/proc/self/status` (`VmRSS:`, `VmHWM:`) in bytes.
fn proc_status_bytes(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with(field))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system clock is before the UNIX epoch")
        .as_secs_f64()
}

/// One `census` record: raw counts and the entries they cover, the same numbers
/// [`print_census`] divides.
fn write_census_record(
    writer: &mut impl Write,
    elapsed_secs: f64,
    total_inserted: usize,
    tree: &RocksFrontierMPT,
    entries: usize,
    census: &CensusSnapshot,
) -> std::io::Result<()> {
    writeln!(
        writer,
        "{{\"kind\":\"census\",\"timestamp\":{:.3},\"elapsed_secs\":{elapsed_secs:.6},\
         \"total_inserted\":{total_inserted},\"leaf_count\":{},\"frontier_depth\":{},\
         \"levels_depth\":{},\"entries\":{entries},\"leaf_puts\":{},\"interior_puts\":{},\
         \"bytes_staged\":{},\"write_batches\":{},\"subtree_loads\":{},\"leaves_read\":{},\
         \"data_blocks_read\":{},\"index_blocks_read\":{},\"seeks\":{}}}",
        unix_now(),
        tree.leaf_count(),
        tree.frontier_depth(),
        tree.levels_depth(),
        census[Metric::LeafPuts],
        census[Metric::InteriorPuts],
        census[Metric::BytesStaged],
        census[Metric::BatchesCommitted],
        census[Metric::SubtreeLoads],
        census[Metric::LeavesReadByLoads],
        census[Metric::DataBlocksRead],
        census[Metric::IndexBlocksRead],
        census[Metric::Seeks],
    )
}

/// Per-insert traffic. `puts/insert` is the write headline, `blocks read/insert` the read
/// one; `leaves read/insert` is structural.
fn print_census(census: &CensusSnapshot, entries: usize) {
    if entries == 0 {
        return;
    }
    let per_insert = |n: u64| n as f64 / entries as f64;

    eprintln!();
    eprintln!("--- Write census ({} entries) ---", human_count(entries));
    eprintln!(
        "  puts/insert:            {:.3}  ({:.3} leaf + {:.3} interior)",
        per_insert(census.total_puts()),
        per_insert(census[Metric::LeafPuts]),
        per_insert(census[Metric::InteriorPuts])
    );
    eprintln!(
        "  bytes staged/insert:    {:.1}",
        per_insert(census[Metric::BytesStaged])
    );
    eprintln!(
        "  write batches/insert:   {:.3}",
        per_insert(census[Metric::BatchesCommitted])
    );
    eprintln!(
        "  subtree loads/insert:   {:.3}",
        per_insert(census[Metric::SubtreeLoads])
    );
    eprintln!(
        "  leaves read/insert:     {:.1}",
        per_insert(census[Metric::LeavesReadByLoads])
    );
    eprintln!(
        "  leaves read/load:       {:.1}",
        census[Metric::LeavesReadByLoads] as f64 / census[Metric::SubtreeLoads].max(1) as f64
    );
    eprintln!(
        "  blocks read/insert:     {:.3}  ({:.3} data + {:.3} index)",
        per_insert(census[Metric::DataBlocksRead] + census[Metric::IndexBlocksRead]),
        per_insert(census[Metric::DataBlocksRead]),
        per_insert(census[Metric::IndexBlocksRead])
    );
    eprintln!(
        "  seeks/insert:           {:.3}",
        per_insert(census[Metric::Seeks])
    );
    eprintln!(
        "  blocks read/load:       {:.1}",
        (census[Metric::DataBlocksRead] + census[Metric::IndexBlocksRead]) as f64
            / census[Metric::SubtreeLoads].max(1) as f64
    );
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

/// `RUST_LOG` names a level (default `info`). Lines go to stdout so the stderr report stays
/// clean for `ab.py`.
struct StdoutLogger;

impl log::Log for StdoutLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            println!(
                "[{:<5} {}] {}",
                record.level(),
                record.target(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

fn init_logging() {
    let level = env::var("RUST_LOG")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(log::LevelFilter::Info);
    log::set_max_level(level);
    log::set_logger(&StdoutLogger).expect("no other logger is installed");
}

/// Redrawn at most four times a second, and only when stderr is a terminal (`ab.py`
/// captures stderr).
struct Progress {
    started: Instant,
    drawn: Instant,
    enabled: bool,
}

impl Progress {
    fn start() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            drawn: now,
            enabled: std::io::stderr().is_terminal(),
        }
    }

    fn advance(&mut self, inserted: usize) {
        if !self.enabled || self.drawn.elapsed() < Duration::from_millis(250) {
            return;
        }
        self.drawn = Instant::now();
        let secs = self.started.elapsed().as_secs_f64();
        eprint!(
            "\r{} entries - {}/s - {secs:.0}s   ",
            human_count(inserted),
            human_count((inserted as f64 / secs) as usize)
        );
    }

    fn finish(&self) {
        if self.enabled {
            eprintln!();
        }
    }
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

fn default_log_path(db_path: &str) -> String {
    Path::new(db_path)
        .join(LOG_FILE_NAME)
        .to_string_lossy()
        .into_owned()
}

/// Open for appending, creating if absent, and terminate a partial last line first so a
/// killed run's truncated record cannot splice into this run's first.
fn open_append(path: &str) -> Result<File, Box<dyn Error>> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    if !ends_with_newline(path)? {
        file.write_all(b"\n")?;
    }
    Ok(file)
}

fn ends_with_newline(path: &str) -> Result<bool, Box<dyn Error>> {
    let mut file = File::open(path)?;
    if file.seek(SeekFrom::End(0))? == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

/// JSON-escape `text`; a path may hold a quote or a backslash.
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
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

/// `count` uniform key/value pairs. The generator is threaded through the run, so a run is a
/// function of the seed and the entry count alone.
fn generate_entries(rng: &mut KeyStream, count: usize) -> Vec<Entry> {
    let mut vec = Vec::with_capacity(count);
    for _ in 0..count {
        let mut key = [0u8; 32];
        rng.fill(&mut key);
        let mut value = [0u8; 32];
        rng.fill(&mut value);
        vec.push((Key(key), Value(value)));
    }
    vec
}

/// wyrand, bit-identical to `fastrand` 2.x, so recorded seeds still name the same keys and
/// root hashes.
struct KeyStream(u64);

impl KeyStream {
    fn with_seed(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        const WY_CONST_0: u64 = 0x2d35_8dcc_aa6c_78a5;
        const WY_CONST_1: u64 = 0x8bb8_4b93_962e_acc9;
        self.0 = self.0.wrapping_add(WY_CONST_0);
        let t = u128::from(self.0) * u128::from(self.0 ^ WY_CONST_1);
        (t as u64) ^ (t >> 64) as u64
    }

    /// Eight bytes per draw in native order, as `fastrand::Rng::fill` did.
    fn fill(&mut self, bytes: &mut [u8; 32]) {
        for chunk in bytes.as_chunks_mut::<8>().0 {
            *chunk = self.next_u64().to_ne_bytes();
        }
    }
}

/// One hash under `RandomState`'s OS-seeded keys.
fn random_seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::hash::RandomState::new().build_hasher().finish()
}

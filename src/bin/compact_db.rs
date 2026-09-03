//! Collapse a database's LSM to one sorted run, in place (DESIGN.md, Performance).

use std::env;
use std::error::Error;
use std::path::Path;
use std::time::Instant;

use jellyfish_rs::mpt::rocks_frontier::storage::RocksStorage;

fn main() -> Result<(), Box<dyn Error>> {
    let path = match env::args().nth(1) {
        Some(path) => path,
        None => {
            eprintln!("usage: compact_db <db_path>");
            std::process::exit(1);
        }
    };
    // `open` would create a missing database.
    if !Path::new(&path).is_dir() {
        return Err(format!("{path} is not a database directory").into());
    }

    let start = Instant::now();
    let storage = RocksStorage::open(&path)?;
    eprintln!(
        "opened {path} in {:.1}s, compacting...",
        start.elapsed().as_secs_f64()
    );

    let compact_start = Instant::now();
    storage.compact_all();
    eprintln!("compacted in {:.1}s", compact_start.elapsed().as_secs_f64());
    Ok(())
}

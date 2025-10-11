use jellyfish_rs::mpt::MerklePatriciaTree;
use jellyfish_rs::{DurableBatchMPT, Hash};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::env;

fn hash_key(key: &str) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hasher.finalize().into()
}

fn hash_value(value: &str) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hasher.finalize().into()
}

fn main() {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();

    // Create a temporary database file
    let db_path = env::temp_dir().join("jellyfish_mpt_dump.db");
    let db_path_str = db_path.to_str().expect("Invalid temp path");

    println!("Creating DurableBatchMPT at: {}", db_path_str);

    // Create a new DurableBatchMPT with a file-based database
    let mut mpt =
        DurableBatchMPT::new_with_path(db_path_str).expect("Failed to create DurableBatchMPT");

    // Insert some sample data
    let items = vec![
        ("apple", "red"),
        ("banana", "yellow"),
        ("grape", "purple"),
        ("lemon", "yellow"),
        ("lime", "green"),
        ("orange", "orange"),
        ("blueberry", "blue"),
        ("strawberry", "red"),
    ];

    let entries: Vec<(Hash, Hash)> = items
        .iter()
        .map(|(k, v)| (hash_key(k), hash_value(v)))
        .collect();

    println!("\nInserting {} entries into MPT...", entries.len());
    mpt.batch_upsert(&entries[1..2]);

    println!("Entries inserted successfully!");
    println!("Cache stats: {:?}", mpt.cache_stats());

    // Now open the database directly and dump all tables
    println!("\n=== SQLite Database Dump ===\n");

    let conn = Connection::open(db_path_str).expect("Failed to open database for dumping");

    // Dump the nodes table
    println!("--- NODES TABLE ---");
    let mut stmt = conn
        .prepare("SELECT prefix_hash, prefix_length, node_type, length(node_data) as data_size FROM nodes ORDER BY prefix_length, prefix_hash")
        .expect("Failed to prepare statement");

    let mut rows = stmt.query([]).expect("Failed to query nodes");
    let mut count = 0;

    while let Some(row) = rows.next().expect("Failed to fetch row") {
        let prefix_hash: Vec<u8> = row.get(0).expect("Failed to get prefix_hash");
        let prefix_length: i64 = row.get(1).expect("Failed to get prefix_length");
        let node_type: String = row.get(2).expect("Failed to get node_type");
        let data_size: i64 = row.get(3).expect("Failed to get data_size");

        // Convert prefix_hash to hex string (first 8 bytes for readability)
        let hex_prefix: String = prefix_hash
            .iter()
            .take(8)
            .map(|b| format!("{:02x}", b))
            .collect::<Vec<_>>()
            .join("");

        println!(
            "Row {}: prefix=0x{}... (len={}), type={}, data_size={} bytes",
            count + 1,
            hex_prefix,
            prefix_length,
            node_type,
            data_size
        );
        count += 1;
    }

    println!("\nTotal nodes: {}", count);

    // Dump the metadata table
    println!("\n--- METADATA TABLE ---");
    let mut stmt = conn
        .prepare("SELECT key, value FROM metadata")
        .expect("Failed to prepare metadata statement");

    let mut rows = stmt.query([]).expect("Failed to query metadata");
    let mut meta_count = 0;

    while let Some(row) = rows.next().expect("Failed to fetch metadata row") {
        let key: String = row.get(0).expect("Failed to get key");
        let value: i64 = row.get(1).expect("Failed to get value");
        println!("  {}: {}", key, value);
        meta_count += 1;
    }

    if meta_count == 0 {
        println!("  (empty)");
    }

    // Dump full SQL schema
    println!("\n--- DATABASE SCHEMA ---");
    let mut stmt = conn
        .prepare(
            "SELECT sql FROM sqlite_master WHERE type='table' OR type='index' ORDER BY type, name",
        )
        .expect("Failed to prepare schema statement");

    let mut rows = stmt.query([]).expect("Failed to query schema");

    while let Some(row) = rows.next().expect("Failed to fetch schema row") {
        let sql: Option<String> = row.get(0).expect("Failed to get sql");
        if let Some(sql) = sql {
            println!("{};", sql);
        }
    }

    println!("\n=== End of Database Dump ===");
    println!("\nDatabase file location: {}", db_path_str);
}

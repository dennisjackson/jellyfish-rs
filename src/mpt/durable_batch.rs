use log::{debug, info, warn};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Result as SqliteResult};
use std::env;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::mpt::MerklePatriciaTree;
use crate::{Hash, Prefix};

use super::{
    Cache, Node,
    cache::{DEFAULT_CACHE_MEMORY_LIMIT_BYTES, ROOT_METADATA_KEY},
};

/// A durable batch-optimized Merkle Patricia Tree implementation backed by SQLite.
/// This implementation performs batch upserts by:
/// 1. Loading necessary nodes from SQLite into a Cache
/// 2. Performing the batch upsert in memory
/// 3. Writing changed nodes back to SQLite
///
/// The tree structure is persisted to disk, allowing for larger-than-memory trees.
pub struct DurableBatchMPT {
    cache: Cache,
    db: Arc<Mutex<Connection>>,
    root: Prefix,
}

impl DurableBatchMPT {
    /// Create a new durable MPT with the given SQLite database path and cache limit.
    pub fn new_with_path_and_cache_limit(
        db_path: &str,
        cache_memory_limit_bytes: usize,
    ) -> SqliteResult<Self> {
        Self::new_with_path_internal(db_path, cache_memory_limit_bytes, false)
    }

    /// Create a new durable MPT with the given SQLite database path.
    pub fn new_with_path(db_path: &str) -> SqliteResult<Self> {
        Self::new_with_path_and_cache_limit(db_path, DEFAULT_CACHE_MEMORY_LIMIT_BYTES)
    }

    /// Open an existing durable MPT, verifying that the database has already been initialized.
    pub fn new_existing_with_path(db_path: &str) -> SqliteResult<Self> {
        Self::new_existing_with_path_and_cache_limit(db_path, DEFAULT_CACHE_MEMORY_LIMIT_BYTES)
    }

    /// Open an existing durable MPT with the given SQLite database path and cache limit.
    pub fn new_existing_with_path_and_cache_limit(
        db_path: &str,
        cache_memory_limit_bytes: usize,
    ) -> SqliteResult<Self> {
        Self::new_with_path_internal(db_path, cache_memory_limit_bytes, true)
    }

    /// Create a new in-memory durable MPT (for testing).
    pub fn new_in_memory() -> SqliteResult<Self> {
        Self::new_with_path(":memory:")
    }

    /// Create a new in-memory durable MPT with a very small cache (tests only).
    #[cfg(test)]
    pub fn new_in_memory_with_small_cache() -> SqliteResult<Self> {
        const TEST_CACHE_LIMIT_BYTES: usize = 10 * 1024;
        Self::new_with_path_and_cache_limit(":memory:", TEST_CACHE_LIMIT_BYTES)
    }

    /// Toggle SQLite durability-related safety settings (fullfsync and synchronous).
    /// Enabling these settings maximizes crash safety at the cost of write throughput.
    /// Disabling them trades some durability for speed, which can be useful when the caller
    /// provides its own durability guarantees or during bulk imports.
    pub fn set_safety_mode(&self, enable: bool) {
        let conn = self.db.lock().unwrap();
        Self::configure_safety_pragmas(&conn, enable).expect("Failed to set safety pragmas");
    }

    fn new_with_path_internal(
        db_path: &str,
        cache_memory_limit_bytes: usize,
        require_existing: bool,
    ) -> SqliteResult<Self> {
        let conn = if require_existing {
            Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?
        } else {
            Connection::open_with_flags(
                db_path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
            )?
        };

        if require_existing {
            Self::validate_existing_database(&conn)?;
        }

        Self::configure_safety_pragmas(&conn, true)?;
        Cache::initialize_database(&conn)?;

        let db = Arc::new(Mutex::new(conn));
        let cache = Cache::new_with_limit(Arc::clone(&db), cache_memory_limit_bytes);
        let root = cache.get_root();

        Ok(Self { cache, db, root })
    }

    fn configure_safety_pragmas(conn: &Connection, enable: bool) -> SqliteResult<()> {
        conn.pragma_update(None, "journal_mode", "WAL")?;

        // Andrew Ayer's advice: rely on PRAGMA fullfsync for durable SQLite WAL writes.
        conn.pragma_update(None, "fullfsync", enable)?;
        if enable {
            // FULL synchronous ensures the WAL is flushed to stable storage on each commit.
            conn.execute("PRAGMA synchronous = FULL", [])?;
        } else {
            // NORMAL is SQLite's WAL default and skips the extra fsync for better throughput.
            conn.execute("PRAGMA synchronous = NORMAL", [])?;
        }
        Ok(())
    }

    fn validate_existing_database(conn: &Connection) -> SqliteResult<()> {
        if !Self::table_exists(conn, "metadata")? {
            return Err(Self::existing_database_error(
                "DurableBatch database is missing the metadata table",
            ));
        }

        if !Self::table_exists(conn, "nodes")? {
            return Err(Self::existing_database_error(
                "DurableBatch database is missing the nodes table",
            ));
        }

        let mut stmt = conn.prepare("SELECT 1 FROM metadata WHERE key = ?1 LIMIT 1")?;
        let has_root_metadata = stmt
            .query_row([ROOT_METADATA_KEY], |_| Ok(()))
            .optional()?
            .is_some();

        if !has_root_metadata {
            return Err(Self::existing_database_error(
                "DurableBatch database is missing the persisted root metadata entry",
            ));
        }

        Ok(())
    }

    fn table_exists(conn: &Connection, table_name: &str) -> SqliteResult<bool> {
        let mut stmt =
            conn.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?1 LIMIT 1")?;

        Ok(stmt
            .query_row([table_name], |_| Ok(()))
            .optional()?
            .is_some())
    }

    fn existing_database_error(message: impl Into<String>) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseCorrupt,
                extended_code: rusqlite::ErrorCode::DatabaseCorrupt as i32,
            },
            Some(message.into()),
        )
    }

    /// Batch upsert with recursive single-pass optimization.
    /// This method traverses the tree only once, partitioning entries at each interior node
    /// and updating hashes on the way back up the recursion.
    pub fn batch_upsert_optimized(&mut self, entries: &[(Hash, Hash)]) {
        if entries.is_empty() {
            return;
        }

        debug!("Batch upserting {} entries", entries.len());

        let entries_vec = super::sorted_unique_entries(entries);

        let batch_nodes = entries_vec.len();
        let limit_bytes = self.cache.cache_memory_limit_bytes();
        if limit_bytes > 0 && batch_nodes > 0 {
            let tree_size = self.cache.tree_size();
            let tree_log = if tree_size > 1 {
                (tree_size as f64).ln()
            } else {
                0.0
            };

            if tree_log > 0.0 {
                let entry_size = self.cache.cache_entry_size_bytes().max(1);
                let limit_entries = std::cmp::max(1, limit_bytes / entry_size);
                let estimated_nodes = (batch_nodes as f64) * tree_log;

                if estimated_nodes > limit_entries as f64 {
                    warn!(
                        "Batch upsert of {} entries may exceed cache capacity (log(tree_size={})≈{:.2}, estimated nodes≈{:.2}, limit≈{} entries, limit_bytes={})",
                        batch_nodes,
                        tree_size,
                        tree_log,
                        estimated_nodes,
                        limit_entries,
                        limit_bytes
                    );
                }
            }
        }

        let new_root =
            super::batch_ops::batch_upsert_recursive(&self.cache, self.root, entries_vec);
        self.root = new_root;
        self.cache.set_root(new_root);
    }

    /// Clear the in-memory cache (useful for testing memory constraints).
    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }
}

impl MerklePatriciaTree for DurableBatchMPT {
    fn new() -> Self {
        // Generate a unique temporary file path using process ID and timestamp
        let temp_dir = env::temp_dir();
        let pid = std::process::id();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_file = temp_dir.join(format!("jellyfish_mpt_{}_{}.db", pid, timestamp));
        let db_path = temp_file.to_str().expect("Invalid temp path");

        Self::new_with_path(db_path).expect("Failed to create temporary database")
    }

    fn upsert(&mut self, key: Hash, value: Hash) {
        self.batch_upsert(&[(key, value)]);
    }

    fn enumerate_nodes(&self) -> Vec<(Prefix, Node)> {
        self.cache
            .enumerate_nodes()
            .expect("Failed to enumerate nodes")
    }

    fn get_root_hash(&self) -> Option<Hash> {
        self.cache.pre_advise(&[self.root]).unwrap();
        self.cache
            .get(&self.root)
            .map(|node| node.value().merkle_hash())
    }

    fn get_leaf_value(&self, key: Hash) -> Option<Hash> {
        let prefix = Prefix::from(key);

        self.cache.pre_advise(&[prefix]).unwrap();

        let node = self.cache.get(&prefix)?;
        match node.value() {
            Node::Leaf(leaf) => Some(leaf.value),
            _ => None,
        }
    }

    fn batch_upsert(&mut self, entries: &[(Hash, Hash)]) {
        if entries.is_empty() {
            return;
        }

        // Sort entries by key so chunk boundaries follow key order
        let sorted_entries = super::sorted_unique_entries(entries);

        // Determine chunk size based on cache memory limit; fall back to whole batch if unlimited
        let limit_bytes = self.cache.cache_memory_limit_bytes();
        let entry_bytes = self.cache.cache_entry_size_bytes().max(1);
        let mut prefix_buffer: Vec<Prefix> = Vec::new();

        // Loop over dynamic chunks, releasing keys, pre-advising, batch_upserting and flushing
        let mut start = 0;
        let total = sorted_entries.len();
        while start < total {
            let remaining = total - start;
            let chunk_capacity = if limit_bytes == 0 {
                remaining
            } else {
                let tree_size = self.cache.tree_size().max(2);
                let log_tree = (tree_size as f64).ln();
                let cost_per_entry = (2.0 * entry_bytes as f64 * log_tree).max(entry_bytes as f64);
                std::cmp::max(1, (limit_bytes as f64 / cost_per_entry) as usize)
            };
            let chunk_capacity = std::cmp::min(chunk_capacity, 1_000);
            let end = std::cmp::min(total, start + chunk_capacity);
            let chunk = &sorted_entries[start..end];

            prefix_buffer.clear();
            prefix_buffer.extend(chunk.iter().map(|(k, _)| Prefix::from(*k)));
            let batch_start = Instant::now();
            self.cache.release_keys(&prefix_buffer);
            self.cache.pre_advise(&prefix_buffer).unwrap();
            self.batch_upsert_optimized(chunk);
            self.cache.flush().unwrap();
            let batch_duration = batch_start.elapsed();
            info!(
                "Upserted batch of {} entries in {:.3} ms ()",
                chunk.len(),
                batch_duration.as_secs_f64() * 1_000.0,
            );
            start = end;
        }
    }
}

#[cfg(test)]
mod tests;

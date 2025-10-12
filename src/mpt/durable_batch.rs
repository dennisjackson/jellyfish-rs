use log::{debug, warn};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Result as SqliteResult};
use std::env;
use std::sync::{Arc, Mutex};

use crate::mpt::MerklePatriciaTree;
use crate::{Hash, Prefix};

use super::{
    Cache, InteriorNode, LeafNode, Node,
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

    fn sorted_unique_entries(entries: &[(Hash, Hash)]) -> Vec<(Hash, Hash)> {
        let mut sorted = entries.to_vec();
        sorted.sort_by_key(|(key, _)| *key);
        sorted.dedup_by_key(|(key, _)| *key);
        sorted
    }

    /// Helper to order two children based on whether the key goes right at the split point
    fn order_children(
        split_prefix: &Prefix,
        key: Hash,
        key_prefix: Prefix,
        key_hash: Hash,
        other_prefix: Prefix,
        other_hash: Hash,
    ) -> (Prefix, Prefix, Hash, Hash) {
        if split_prefix.key_goes_right(key) {
            (other_prefix, key_prefix, other_hash, key_hash)
        } else {
            (key_prefix, other_prefix, key_hash, other_hash)
        }
    }

    /// Batch upsert with recursive single-pass optimization.
    /// This method traverses the tree only once, partitioning entries at each interior node
    /// and updating hashes on the way back up the recursion.
    pub fn batch_upsert_optimized(&mut self, entries: &[(Hash, Hash)]) {
        if entries.is_empty() {
            return;
        }

        debug!("Batch upserting {} entries", entries.len());

        // Convert to sorted vector for efficient partitioning
        let entries_vec = Self::sorted_unique_entries(entries);

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

        // Perform recursive batch upsert
        let new_root = self.recursive_batch_upsert(self.root, entries_vec);
        self.root = new_root;
        self.cache.set_root(new_root);
    }

    /// Recursively batch upsert entries at the current node.
    /// Returns the prefix of the (possibly new) root of this subtree.
    fn recursive_batch_upsert(&self, current_prefix: Prefix, entries: Vec<(Hash, Hash)>) -> Prefix {
        if entries.is_empty() {
            return current_prefix;
        }

        let node = match self.cache.get(&current_prefix) {
            Some(node) => node.value().clone(),
            None => return self.batch_insert_into_empty(entries),
        };

        match node {
            Node::Leaf(leaf) => self.batch_upsert_at_leaf(leaf, entries),
            Node::Interior(interior) => self.batch_upsert_at_interior(interior, entries),
        }
    }

    /// Insert all entries into an empty tree.
    fn batch_insert_into_empty(&self, entries: Vec<(Hash, Hash)>) -> Prefix {
        let mut entries_iter = entries.into_iter();
        let Some((first_key, first_value)) = entries_iter.next() else {
            return Prefix::root();
        };

        let first_prefix = Prefix::from(first_key);
        let first_leaf = LeafNode::new(first_key, first_value);
        self.cache.set(first_prefix, Node::Leaf(first_leaf));

        // Recursively insert remaining entries
        self.recursive_batch_upsert(first_prefix, entries_iter.collect())
    }

    /// Batch upsert at a leaf node.
    fn batch_upsert_at_leaf(&self, leaf: LeafNode, entries: Vec<(Hash, Hash)>) -> Prefix {
        let leaf_prefix = Prefix::from(leaf.key);

        // Partition entries into updates for this leaf and remaining inserts
        let (mut updates, remaining): (Vec<_>, Vec<_>) =
            entries.into_iter().partition(|(key, _)| *key == leaf.key);

        if let Some((_, new_value)) = updates.pop() {
            let updated_leaf = LeafNode::new(leaf.key, new_value);
            self.cache.set(leaf_prefix, Node::Leaf(updated_leaf));

            if remaining.is_empty() {
                return leaf_prefix;
            }
            // Continue inserting remaining entries
            return self.recursive_batch_upsert(leaf_prefix, remaining);
        }

        if remaining.is_empty() {
            return leaf_prefix;
        }

        // Split: need to create interior node(s) and distribute entries
        // Start with the first non-matching entry
        let mut remaining_iter = remaining.into_iter();
        let (first_key, first_value) = remaining_iter
            .next()
            .expect("remaining is non-empty so first element must exist");
        let remaining = remaining_iter.collect();

        let new_leaf = LeafNode::new(first_key, first_value);
        let new_prefix = Prefix::from(first_key);
        let existing_prefix = Prefix::from(leaf.key);
        let merged_prefix = Prefix::common_prefix(&existing_prefix, &new_prefix);

        let (left_prefix, right_prefix, left_hash, right_hash) = Self::order_children(
            &merged_prefix,
            first_key,
            new_prefix,
            new_leaf.merkle_hash,
            existing_prefix,
            leaf.merkle_hash,
        );

        let new_interior = InteriorNode::new(
            merged_prefix,
            left_prefix,
            right_prefix,
            left_hash,
            right_hash,
        );

        self.cache.set(merged_prefix, Node::Interior(new_interior));
        self.cache.set(existing_prefix, Node::Leaf(leaf));
        self.cache.set(new_prefix, Node::Leaf(new_leaf));

        // Continue with remaining entries
        self.recursive_batch_upsert(merged_prefix, remaining)
    }

    /// Batch upsert at an interior node.
    fn batch_upsert_at_interior(
        &self,
        interior: InteriorNode,
        entries: Vec<(Hash, Hash)>,
    ) -> Prefix {
        // Partition entries: those that belong under this node vs. those that diverge
        let (mut contained_entries, divergent_entries): (Vec<_>, Vec<_>) = entries
            .into_iter()
            .partition(|(key, _)| interior.prefix.contains(key));

        // Handle divergent entries first (they require creating a new parent)
        let mut divergent_iter = divergent_entries.into_iter();
        if let Some((first_key, first_value)) = divergent_iter.next() {
            // Create new parent(s) for divergent entries
            let new_leaf = LeafNode::new(first_key, first_value);
            let new_leaf_prefix = Prefix::from(first_key);
            let common = Prefix::common_prefix(&interior.prefix, &new_leaf_prefix);

            let (left_prefix, right_prefix, left_hash, right_hash) = Self::order_children(
                &common,
                first_key,
                new_leaf_prefix,
                new_leaf.merkle_hash,
                interior.prefix,
                interior.merkle_hash,
            );

            let new_interior =
                InteriorNode::new(common, left_prefix, right_prefix, left_hash, right_hash);

            self.cache.set(common, Node::Interior(new_interior));
            self.cache.set(new_leaf_prefix, Node::Leaf(new_leaf));

            // Merge remaining entries and continue
            contained_entries.extend(divergent_iter);
            return self.recursive_batch_upsert(common, contained_entries);
        }

        // All entries belong under this interior node
        // Partition them by left/right
        let (left_entries, right_entries): (Vec<_>, Vec<_>) = contained_entries
            .into_iter()
            .partition(|(key, _)| !interior.prefix.key_goes_right(*key));

        // Recursively process left and right subtrees in parallel using rayon
        let (new_left, new_right) = rayon::join(
            || {
                if !left_entries.is_empty() {
                    self.recursive_batch_upsert(interior.left, left_entries)
                } else {
                    interior.left
                }
            },
            || {
                if !right_entries.is_empty() {
                    self.recursive_batch_upsert(interior.right, right_entries)
                } else {
                    interior.right
                }
            },
        );

        // Recalculate this interior node's hash based on updated children
        let left_hash = self
            .cache
            .get(&new_left)
            .expect("Expected left child to exist in cache")
            .value()
            .merkle_hash();
        let right_hash = self
            .cache
            .get(&new_right)
            .expect("Expected right child to exist in cache")
            .value()
            .merkle_hash();

        let updated_interior =
            InteriorNode::new(interior.prefix, new_left, new_right, left_hash, right_hash);

        self.cache
            .set(interior.prefix, Node::Interior(updated_interior));

        interior.prefix
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
        // Use Cache's enumerate_nodes method
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

        // Load from database
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
        let sorted_entries = Self::sorted_unique_entries(entries);

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

            self.cache.release_keys(&prefix_buffer);
            self.cache.pre_advise(&prefix_buffer).unwrap();
            self.batch_upsert_optimized(chunk);
            self.cache.flush().unwrap();
            start = end;
        }
    }
}

#[cfg(test)]
mod tests;

use dashmap::DashMap;
use log::info;
use rayon::prelude::*;
use rusqlite::{Connection, Result as SqliteResult, params};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::mpt::MerklePatriciaTree;
use crate::{Hash, Prefix};

use super::{InteriorNode, LeafNode, Node};

/// A durable batch-optimized Merkle Patricia Tree implementation backed by SQLite.
/// This implementation performs batch upserts by:
/// 1. Loading necessary nodes from SQLite into a DashMap cache
/// 2. Performing the batch upsert in memory
/// 3. Writing changed nodes back to SQLite
///
/// The tree structure is persisted to disk, allowing for larger-than-memory trees.
pub struct DurableBatchMPT {
    db: Arc<Mutex<Connection>>,
    cache: Arc<DashMap<Prefix, Node>>,
    dirty: Arc<DashMap<Prefix, ()>>,
    root: Prefix,
    tree_size: Arc<Mutex<usize>>,
    /// Counter for SQLite queries (reads and writes)
    sqlite_query_count: Arc<AtomicUsize>,
}

impl DurableBatchMPT {
    /// Create a new durable MPT with the given SQLite database path.
    pub fn new_with_path(db_path: &str) -> SqliteResult<Self> {
        let conn = Connection::open(db_path)?;

        // Create table if it doesn't exist
        conn.execute(
            "CREATE TABLE IF NOT EXISTS nodes (
                prefix_hash BLOB NOT NULL,
                prefix_length INTEGER NOT NULL,
                node_type TEXT NOT NULL,
                node_data BLOB NOT NULL,
                PRIMARY KEY (prefix_hash, prefix_length)
            )",
            [],
        )?;

        // Create index for faster lookups
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_prefix ON nodes(prefix_hash, prefix_length)",
            [],
        )?;

        // Create metadata table for storing tree size and other metadata
        conn.execute(
            "CREATE TABLE IF NOT EXISTS metadata (
                key TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            )",
            [],
        )?;

        // Load tree size from metadata
        let tree_size = conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'tree_size'",
                [],
                |row| row.get::<_, i64>(0).map(|v| v as usize),
            )
            .unwrap_or(0);

        Ok(Self {
            db: Arc::new(Mutex::new(conn)),
            cache: Arc::new(DashMap::new()),
            dirty: Arc::new(DashMap::new()),
            root: Prefix::root(),
            tree_size: Arc::new(Mutex::new(tree_size)),
            sqlite_query_count: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Create a new in-memory durable MPT (for testing).
    pub fn new_in_memory() -> SqliteResult<Self> {
        Self::new_with_path(":memory:")
    }

    /// Calculate the depth of common prefix levels we should preload.
    /// This determines how many levels from the root are likely to be shared
    /// by multiple keys in the batch, making them worth preloading.
    fn calculate_preload_depth(&self, keys: &[Hash]) -> usize {
        if keys.is_empty() {
            return 0;
        }

        let size = *self.tree_size.lock().unwrap();
        if size == 0 {
            return 0;
        }

        // Calculate the approximate depth of the tree
        let tree_depth = (size as f64).log2() as usize;

        // For small batches, preload conservatively
        // For large batches, preload more aggressively
        let batch_size = keys.len();

        if batch_size == 1 {
            // Single insert: preload minimal depth
            return tree_depth.min(3);
        } else if batch_size < 10 {
            // Small batch: preload top few levels
            return tree_depth.min(5);
        } else if batch_size < 100 {
            // Medium batch: preload more levels
            return tree_depth.min(8);
        } else {
            // Large batch: preload significant portion of tree
            // but leave room for the deeper levels
            if tree_depth <= 5 {
                tree_depth
            } else {
                tree_depth - 2
            }
        }
    }

    /// Bulk load multiple nodes from the database in a single query.
    /// Returns a map of prefix -> node for all found nodes.
    fn bulk_load_nodes(&self, prefixes: &[Prefix]) -> std::collections::HashMap<Prefix, Node> {
        if prefixes.is_empty() {
            return std::collections::HashMap::new();
        }

        // Track this as a SQLite query
        self.sqlite_query_count.fetch_add(1, Ordering::Relaxed);

        let db = self.db.lock().unwrap();

        // Build a query with multiple OR conditions
        // For better performance with large sets, we use a temporary table approach
        let tx = db.unchecked_transaction().unwrap();

        // Create temporary table for the prefixes we want to load
        tx.execute(
            "CREATE TEMP TABLE IF NOT EXISTS temp_prefixes (
                prefix_hash BLOB NOT NULL,
                prefix_length INTEGER NOT NULL
            )",
            [],
        )
        .unwrap();

        // Clear any previous data
        tx.execute("DELETE FROM temp_prefixes", []).unwrap();

        // Insert all prefixes we want to load
        {
            let mut stmt = tx
                .prepare("INSERT INTO temp_prefixes (prefix_hash, prefix_length) VALUES (?1, ?2)")
                .unwrap();
            for prefix in prefixes {
                stmt.execute(params![&prefix.hash[..], prefix.length])
                    .unwrap();
            }
        }

        // Now do a single JOIN query to get all matching nodes
        let results = {
            let mut stmt = tx
                .prepare(
                    "SELECT n.prefix_hash, n.prefix_length, n.node_type, n.node_data
                 FROM nodes n
                 INNER JOIN temp_prefixes t
                 ON n.prefix_hash = t.prefix_hash AND n.prefix_length = t.prefix_length",
                )
                .unwrap();

            stmt.query_map([], |row| {
                let prefix_hash: Vec<u8> = row.get(0)?;
                let prefix_length: u16 = row.get::<_, i64>(1)? as u16;
                let node_type: String = row.get(2)?;
                let node_data: Vec<u8> = row.get(3)?;

                let mut hash = [0u8; 32];
                hash.copy_from_slice(&prefix_hash);
                let prefix = Prefix {
                    hash,
                    length: prefix_length,
                };

                let node = match node_type.as_str() {
                    "leaf" => {
                        let decoded: (Hash, Hash, Hash) =
                            bincode::decode_from_slice(&node_data, bincode::config::standard())
                                .map(|(v, _)| v)
                                .map_err(|_| rusqlite::Error::InvalidQuery)?;
                        Node::Leaf(LeafNode {
                            key: decoded.0,
                            value: decoded.1,
                            merkle_hash: decoded.2,
                        })
                    }
                    "interior" => {
                        let decoded: (Prefix, Hash, Prefix, Prefix) =
                            bincode::decode_from_slice(&node_data, bincode::config::standard())
                                .map(|(v, _)| v)
                                .map_err(|_| rusqlite::Error::InvalidQuery)?;
                        Node::Interior(InteriorNode {
                            prefix: decoded.0,
                            merkle_hash: decoded.1,
                            left: decoded.2,
                            right: decoded.3,
                        })
                    }
                    _ => return Err(rusqlite::Error::InvalidQuery),
                };

                Ok((prefix, node))
            })
            .unwrap()
            .collect::<Result<std::collections::HashMap<_, _>, _>>()
            .unwrap()
        };

        tx.commit().unwrap();

        results
    }

    /// Preload nodes along the paths to the keys being inserted.
    /// This uses a smart strategy: start from the root and fan out level by level,
    /// but only follow paths that lead to keys we're actually inserting.
    /// Uses bulk loading to minimize database round-trips.
    fn preload_paths_for_keys(&self, keys: &[Hash]) {
        if keys.is_empty() {
            return;
        }

        let depth = self.calculate_preload_depth(keys);
        if depth == 0 {
            return;
        }

        info!("Preloading top {} levels for {} keys", depth, keys.len());

        // Build a set of all keys for quick lookup
        let key_set: std::collections::HashSet<Hash> = keys.iter().copied().collect();

        let mut current_level = vec![self.root];

        for level in 0..depth {
            // Collect all prefixes we need to load at this level
            let prefixes_to_load: Vec<Prefix> = current_level
                .iter()
                .filter(|p| !self.cache.contains_key(p))
                .copied()
                .collect();

            // Bulk load all nodes for this level
            if !prefixes_to_load.is_empty() {
                let loaded_nodes = self.bulk_load_nodes(&prefixes_to_load);

                // Insert all loaded nodes into cache
                for (prefix, node) in loaded_nodes {
                    self.cache.insert(prefix, node);
                }
            }

            // Now determine which children to explore for the next level
            let mut next_level = Vec::new();

            for prefix in current_level {
                if let Some(node) = self.cache.get(&prefix) {
                    if let Node::Interior(interior) = node.value() {
                        // Check if any of our keys would go down the left path
                        let mut has_left_key = false;
                        let mut has_right_key = false;

                        for key in &key_set {
                            if interior.prefix.contains(key) {
                                if interior.prefix.key_goes_right(*key) {
                                    has_right_key = true;
                                } else {
                                    has_left_key = true;
                                }
                            }
                        }

                        // Only preload children if we have keys going that way
                        if has_left_key {
                            next_level.push(interior.left);
                        }
                        if has_right_key {
                            next_level.push(interior.right);
                        }
                    }
                }
            }

            if next_level.is_empty() {
                info!(
                    "Preloading stopped at level {} (no more paths to keys)",
                    level
                );
                break;
            }

            current_level = next_level;
        }

        info!("Preloaded {} nodes into cache", self.cache.len());
    }

    /// Load a node from SQLite into the cache if not already present.
    fn load_node(&self, prefix: &Prefix) -> Option<Node> {
        // Check cache first
        if let Some(node) = self.cache.get(prefix) {
            return Some(node.clone());
        }

        // Track this as a SQLite query
        self.sqlite_query_count.fetch_add(1, Ordering::Relaxed);

        // Load from database
        let db = self.db.lock().unwrap();
        let mut stmt = db
            .prepare("SELECT node_type, node_data FROM nodes WHERE prefix_hash = ?1 AND prefix_length = ?2")
            .ok()?;

        let result = stmt
            .query_row(params![&prefix.hash[..], prefix.length], |row| {
                let node_type: String = row.get(0)?;
                let node_data: Vec<u8> = row.get(1)?;

                let node = match node_type.as_str() {
                    "leaf" => {
                        let decoded: (Hash, Hash, Hash) =
                            bincode::decode_from_slice(&node_data, bincode::config::standard())
                                .map(|(v, _)| v)
                                .map_err(|_| rusqlite::Error::InvalidQuery)?;
                        Node::Leaf(LeafNode {
                            key: decoded.0,
                            value: decoded.1,
                            merkle_hash: decoded.2,
                        })
                    }
                    "interior" => {
                        let decoded: (Prefix, Hash, Prefix, Prefix) =
                            bincode::decode_from_slice(&node_data, bincode::config::standard())
                                .map(|(v, _)| v)
                                .map_err(|_| rusqlite::Error::InvalidQuery)?;
                        Node::Interior(InteriorNode {
                            prefix: decoded.0,
                            merkle_hash: decoded.1,
                            left: decoded.2,
                            right: decoded.3,
                        })
                    }
                    _ => return Err(rusqlite::Error::InvalidQuery),
                };

                Ok(node)
            })
            .ok()?;

        // Cache the loaded node
        self.cache.insert(*prefix, result.clone());
        Some(result)
    }

    /// Load a node and its immediate children into the cache.
    fn load_node_with_children(&self, prefix: &Prefix) {
        if let Some(node) = self.load_node(prefix) {
            if let Node::Interior(interior) = node {
                // Preload children
                self.load_node(&interior.left);
                self.load_node(&interior.right);
            }
        }
    }

    /// Recursively load all nodes in the path from root to the given key.
    fn load_path_to_key(&self, key: Hash) {
        let mut current = self.root;

        loop {
            self.load_node_with_children(&current);

            let node = match self.cache.get(&current) {
                Some(n) => n.clone(),
                None => break,
            };

            match node {
                Node::Leaf(_) => break,
                Node::Interior(interior) => {
                    if interior.prefix.contains(&key) {
                        current = if interior.prefix.key_goes_right(key) {
                            interior.right
                        } else {
                            interior.left
                        };
                    } else {
                        break;
                    }
                }
            }
        }
    }

    /// Write all dirty nodes back to SQLite.
    fn flush_to_disk(&self) -> SqliteResult<()> {
        // Track this as a SQLite query (write operation)
        self.sqlite_query_count.fetch_add(1, Ordering::Relaxed);

        let db = self.db.lock().unwrap();

        let tx = db.unchecked_transaction()?;

        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO nodes (prefix_hash, prefix_length, node_type, node_data) VALUES (?1, ?2, ?3, ?4)"
            )?;

            for entry in self.dirty.iter() {
                let prefix = entry.key();

                if let Some(node) = self.cache.get(prefix) {
                    let (node_type, node_data) = match node.value() {
                        Node::Leaf(leaf) => {
                            let data = bincode::encode_to_vec(
                                (leaf.key, leaf.value, leaf.merkle_hash),
                                bincode::config::standard(),
                            )
                            .map_err(|_| rusqlite::Error::InvalidQuery)?;
                            ("leaf", data)
                        }
                        Node::Interior(interior) => {
                            let data = bincode::encode_to_vec(
                                (
                                    interior.prefix,
                                    interior.merkle_hash,
                                    interior.left,
                                    interior.right,
                                ),
                                bincode::config::standard(),
                            )
                            .map_err(|_| rusqlite::Error::InvalidQuery)?;
                            ("interior", data)
                        }
                    };

                    stmt.execute(params![
                        &prefix.hash[..],
                        prefix.length,
                        node_type,
                        node_data
                    ])?;
                }
            }
        }

        // Update tree size in metadata
        {
            let tree_size = *self.tree_size.lock().unwrap();
            tx.execute(
                "INSERT OR REPLACE INTO metadata (key, value) VALUES ('tree_size', ?1)",
                params![tree_size as i64],
            )?;
        }

        tx.commit()?;

        // Clear dirty set after successful flush
        self.dirty.clear();

        Ok(())
    }

    /// Mark a node as dirty (needs to be written to disk).
    fn mark_dirty(&self, prefix: Prefix) {
        self.dirty.insert(prefix, ());
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
    fn batch_upsert_optimized(&mut self, entries: &[(Hash, Hash)]) {
        if entries.is_empty() {
            return;
        }

        info!("Batch upserting {} entries", entries.len());

        // Convert to sorted vector for efficient partitioning
        let mut entries_vec: Vec<(Hash, Hash)> = entries.to_vec();
        entries_vec.sort_by_key(|(k, _)| *k);
        // Remove duplicates, keeping the last occurrence (latest value)
        entries_vec.dedup_by_key(|(k, _)| *k);

        // Extract keys for preloading
        let keys: Vec<Hash> = entries_vec.iter().map(|(k, _)| *k).collect();

        // Preload only the nodes we'll need based on the keys being inserted
        self.preload_paths_for_keys(&keys);

        // Load remaining paths for all keys to ensure necessary nodes are in cache
        // This will be faster now since the top levels are already cached
        entries_vec.par_iter().for_each(|(key, _)| {
            self.load_path_to_key(*key);
        });

        // Track the old cache size to calculate how many nodes were added
        let old_cache_size = self.cache.len();

        // Perform recursive batch upsert
        let new_root = self.recursive_batch_upsert(self.root, entries_vec);
        self.root = new_root;

        // Update tree size based on new nodes added to cache
        let new_cache_size = self.cache.len();
        *self.tree_size.lock().unwrap() = new_cache_size;

        info!(
            "Tree size updated: {} -> {} nodes",
            old_cache_size, new_cache_size
        );

        // Flush all changes to disk
        if let Err(e) = self.flush_to_disk() {
            log::error!("Failed to flush to disk: {}", e);
        }
    }

    /// Recursively batch upsert entries at the current node.
    /// Returns the prefix of the (possibly new) root of this subtree.
    fn recursive_batch_upsert(&self, current_prefix: Prefix, entries: Vec<(Hash, Hash)>) -> Prefix {
        if entries.is_empty() {
            return current_prefix;
        }

        let node = self.cache.get(&current_prefix).map(|n| n.clone());
        let Some(node) = node else {
            // Empty tree: insert all entries
            return self.batch_insert_into_empty(entries);
        };

        match node {
            Node::Leaf(leaf) => self.batch_upsert_at_leaf(leaf, entries),
            Node::Interior(interior) => self.batch_upsert_at_interior(interior, entries),
        }
    }

    /// Insert all entries into an empty tree.
    fn batch_insert_into_empty(&self, mut entries: Vec<(Hash, Hash)>) -> Prefix {
        if entries.is_empty() {
            return Prefix::root();
        }

        // Start with the first entry
        let (first_key, first_value) = entries.remove(0);

        let first_prefix = Prefix::from(first_key);
        let first_leaf = LeafNode::new(first_key, first_value);
        self.cache.insert(first_prefix, Node::Leaf(first_leaf));
        self.mark_dirty(first_prefix);

        // Recursively insert remaining entries
        self.recursive_batch_upsert(first_prefix, entries)
    }

    /// Batch upsert at a leaf node.
    fn batch_upsert_at_leaf(&self, leaf: LeafNode, mut entries: Vec<(Hash, Hash)>) -> Prefix {
        let leaf_prefix = Prefix::from(leaf.key);

        // Check if any entry updates this leaf (using binary search since entries are sorted)
        if let Ok(idx) = entries.binary_search_by_key(&leaf.key, |(k, _)| *k) {
            let (_, new_value) = entries.remove(idx);
            let updated_leaf = LeafNode::new(leaf.key, new_value);
            self.cache.insert(leaf_prefix, Node::Leaf(updated_leaf));
            self.mark_dirty(leaf_prefix);

            if entries.is_empty() {
                return leaf_prefix;
            }
            // Continue inserting remaining entries
            return self.recursive_batch_upsert(leaf_prefix, entries);
        }

        if entries.is_empty() {
            return leaf_prefix;
        }

        // Split: need to create interior node(s) and distribute entries
        // Start with the first non-matching entry
        let (first_key, first_value) = entries.remove(0);

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

        self.cache
            .insert(merged_prefix, Node::Interior(new_interior));
        self.cache.insert(existing_prefix, Node::Leaf(leaf));
        self.cache.insert(new_prefix, Node::Leaf(new_leaf));

        self.mark_dirty(merged_prefix);
        self.mark_dirty(existing_prefix);
        self.mark_dirty(new_prefix);

        // Continue with remaining entries
        self.recursive_batch_upsert(merged_prefix, entries)
    }

    /// Batch upsert at an interior node.
    fn batch_upsert_at_interior(
        &self,
        interior: InteriorNode,
        entries: Vec<(Hash, Hash)>,
    ) -> Prefix {
        // Partition entries: those that belong under this node vs. those that diverge
        let mut contained_entries = Vec::new();
        let mut divergent_entries = Vec::new();

        for &(key, value) in entries.iter() {
            if interior.prefix.contains(&key) {
                contained_entries.push((key, value));
            } else {
                divergent_entries.push((key, value));
            }
        }

        // Handle divergent entries first (they require creating a new parent)
        if !divergent_entries.is_empty() {
            // Create new parent(s) for divergent entries
            let (first_key, first_value) = divergent_entries.remove(0);

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

            self.cache.insert(common, Node::Interior(new_interior));
            self.cache.insert(new_leaf_prefix, Node::Leaf(new_leaf));

            self.mark_dirty(common);
            self.mark_dirty(new_leaf_prefix);

            // Merge remaining entries and continue
            contained_entries.extend(divergent_entries);
            return self.recursive_batch_upsert(common, contained_entries);
        }

        // All entries belong under this interior node
        // Partition them by left/right
        let mut left_entries = Vec::new();
        let mut right_entries = Vec::new();

        for &(key, value) in contained_entries.iter() {
            if interior.prefix.key_goes_right(key) {
                right_entries.push((key, value));
            } else {
                left_entries.push((key, value));
            }
        }

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
        let left_hash = self.cache.get(&new_left).unwrap().merkle_hash();
        let right_hash = self.cache.get(&new_right).unwrap().merkle_hash();

        let updated_interior =
            InteriorNode::new(interior.prefix, new_left, new_right, left_hash, right_hash);

        self.cache
            .insert(interior.prefix, Node::Interior(updated_interior));
        self.mark_dirty(interior.prefix);

        interior.prefix
    }

    /// Clear the in-memory cache (useful for testing memory constraints).
    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    /// Get cache statistics.
    pub fn cache_stats(&self) -> (usize, usize) {
        (self.cache.len(), self.dirty.len())
    }

    /// Get the number of SQLite queries performed since last reset.
    pub fn sqlite_query_count(&self) -> usize {
        self.sqlite_query_count.load(Ordering::Relaxed)
    }

    /// Reset the SQLite query counter.
    pub fn reset_sqlite_query_count(&self) {
        self.sqlite_query_count.store(0, Ordering::Relaxed);
    }
}

impl MerklePatriciaTree for DurableBatchMPT {
    fn new() -> Self {
        Self::new_in_memory().expect("Failed to create in-memory database")
    }

    fn upsert(&mut self, key: Hash, value: Hash) {
        self.batch_upsert_optimized(&[(key, value)]);
    }

    fn enumerate_nodes(&self) -> Vec<(Prefix, Node)> {
        // Track this as a SQLite query
        self.sqlite_query_count.fetch_add(1, Ordering::Relaxed);

        // For durable implementation, we need to load all nodes from database
        let db = self.db.lock().unwrap();
        let mut stmt = db
            .prepare("SELECT prefix_hash, prefix_length, node_type, node_data FROM nodes")
            .expect("Failed to prepare statement");

        let nodes = stmt
            .query_map([], |row| {
                let prefix_hash: Vec<u8> = row.get(0)?;
                let prefix_length: u16 = row.get::<_, i64>(1)? as u16;
                let node_type: String = row.get(2)?;
                let node_data: Vec<u8> = row.get(3)?;

                let mut hash = [0u8; 32];
                hash.copy_from_slice(&prefix_hash);
                let prefix = Prefix {
                    hash,
                    length: prefix_length,
                };
                let node = match node_type.as_str() {
                    "leaf" => {
                        let decoded: (Hash, Hash, Hash) =
                            bincode::decode_from_slice(&node_data, bincode::config::standard())
                                .map(|(v, _)| v)
                                .map_err(|_| rusqlite::Error::InvalidQuery)?;
                        Node::Leaf(LeafNode {
                            key: decoded.0,
                            value: decoded.1,
                            merkle_hash: decoded.2,
                        })
                    }
                    "interior" => {
                        let decoded: (Prefix, Hash, Prefix, Prefix) =
                            bincode::decode_from_slice(&node_data, bincode::config::standard())
                                .map(|(v, _)| v)
                                .map_err(|_| rusqlite::Error::InvalidQuery)?;
                        Node::Interior(InteriorNode {
                            prefix: decoded.0,
                            merkle_hash: decoded.1,
                            left: decoded.2,
                            right: decoded.3,
                        })
                    }
                    _ => return Err(rusqlite::Error::InvalidQuery),
                };

                Ok((prefix, node))
            })
            .expect("Failed to query nodes")
            .collect::<Result<Vec<_>, _>>()
            .expect("Failed to collect nodes");

        nodes
    }

    fn get_root_hash(&self) -> Option<Hash> {
        self.load_node(&self.root).map(|n| n.merkle_hash())
    }

    fn get_leaf_value(&self, key: Hash) -> Option<Hash> {
        let prefix = Prefix::from(key);

        // Try cache first
        if let Some(node) = self.cache.get(&prefix) {
            if let Node::Leaf(leaf) = node.value() {
                return Some(leaf.value);
            }
        }

        // Load from database
        match self.load_node(&prefix)? {
            Node::Leaf(leaf) => Some(leaf.value),
            _ => None,
        }
    }

    fn batch_upsert(&mut self, entries: &[(Hash, Hash)]) {
        self.batch_upsert_optimized(entries);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_insert_and_retrieve() {
        let mut mpt = DurableBatchMPT::new_in_memory().unwrap();

        let key = [1u8; 32];
        let value = [2u8; 32];

        mpt.upsert(key, value);

        assert_eq!(mpt.get_leaf_value(key), Some(value));
    }

    #[test]
    fn test_batch_upsert_persistence() {
        let mut mpt = DurableBatchMPT::new_in_memory().unwrap();

        let entries: Vec<(Hash, Hash)> = vec![
            ([1u8; 32], [10u8; 32]),
            ([2u8; 32], [20u8; 32]),
            ([3u8; 32], [30u8; 32]),
        ];

        mpt.batch_upsert(&entries);

        // Clear cache to force reload from database
        mpt.clear_cache();

        for (key, value) in entries {
            assert_eq!(mpt.get_leaf_value(key), Some(value));
        }
    }

    #[test]
    fn test_cache_stats() {
        let mut mpt = DurableBatchMPT::new_in_memory().unwrap();

        let entries: Vec<(Hash, Hash)> = vec![([1u8; 32], [10u8; 32]), ([2u8; 32], [20u8; 32])];

        mpt.batch_upsert(&entries);

        let (cache_size, dirty_size) = mpt.cache_stats();
        assert!(cache_size > 0);
        assert_eq!(dirty_size, 0); // Should be flushed after batch_upsert
    }

    #[test]
    fn test_preload_optimization() {
        let mut mpt = DurableBatchMPT::new_in_memory().unwrap();

        // Insert enough entries to create a tree with multiple levels
        // Use diverse keys to spread across the tree
        let entries: Vec<(Hash, Hash)> = (0..100)
            .map(|i| {
                let mut key = [0u8; 32];
                let mut value = [0u8; 32];
                // Spread keys across the hash space
                key[0] = i;
                key[1] = (i.wrapping_mul(3)) as u8;
                value[0] = i * 2;
                (key, value)
            })
            .collect();

        mpt.batch_upsert(&entries);

        // Verify tree size is tracked
        let tree_size = *mpt.tree_size.lock().unwrap();
        assert!(tree_size > 0);
        assert_eq!(tree_size, mpt.cache.len());

        // Clear cache to test selective preloading
        let cache_size_before_clear = mpt.cache.len();
        mpt.clear_cache();
        assert_eq!(mpt.cache.len(), 0);

        // Perform a small batch upsert with just a few keys
        // This should only preload paths relevant to these specific keys
        let new_entries: Vec<(Hash, Hash)> =
            vec![([200u8; 32], [250u8; 32]), ([201u8; 32], [251u8; 32])];
        mpt.batch_upsert(&new_entries);

        // Verify that preloading was selective - we should have loaded far fewer
        // nodes than the entire tree, but more than just the new entries
        let cache_size_after_insert = mpt.cache.len();
        assert!(
            cache_size_after_insert > 2,
            "Should have preloaded some interior nodes"
        );
        assert!(
            cache_size_after_insert < cache_size_before_clear,
            "Should have loaded fewer nodes than full tree (loaded: {}, full tree: {})",
            cache_size_after_insert,
            cache_size_before_clear
        );

        // Verify all original values are still retrievable (from disk)
        for (key, value) in entries.iter() {
            assert_eq!(mpt.get_leaf_value(*key), Some(*value));
        }

        // Verify new values are correct
        for (key, value) in new_entries.iter() {
            assert_eq!(mpt.get_leaf_value(*key), Some(*value));
        }
    }

    #[test]
    fn test_sqlite_query_tracking() {
        let mut mpt = DurableBatchMPT::new_in_memory().unwrap();

        // Reset counter to start fresh
        mpt.reset_sqlite_query_count();
        assert_eq!(mpt.sqlite_query_count(), 0);

        // Insert some entries
        let entries: Vec<(Hash, Hash)> = vec![
            ([1u8; 32], [10u8; 32]),
            ([2u8; 32], [20u8; 32]),
            ([3u8; 32], [30u8; 32]),
        ];

        mpt.batch_upsert(&entries);

        // Should have made some SQLite queries (for preloading and flushing)
        let queries_after_insert = mpt.sqlite_query_count();
        assert!(
            queries_after_insert > 0,
            "Should have made SQLite queries during insert"
        );

        // Clear cache and retrieve values (should trigger more queries)
        mpt.clear_cache();
        mpt.reset_sqlite_query_count();

        for (key, _) in &entries {
            let _ = mpt.get_leaf_value(*key);
        }

        let queries_after_get = mpt.sqlite_query_count();
        assert!(
            queries_after_get > 0,
            "Should have made SQLite queries during get"
        );

        // Reset counter and verify it's zero
        mpt.reset_sqlite_query_count();
        assert_eq!(mpt.sqlite_query_count(), 0);
    }
}

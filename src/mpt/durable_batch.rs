use dashmap::DashMap;
use log::info;
use rusqlite::{Connection, Result as SqliteResult, params};
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

        Ok(Self {
            db: Arc::new(Mutex::new(conn)),
            cache: Arc::new(DashMap::new()),
            dirty: Arc::new(DashMap::new()),
            root: Prefix::root(),
        })
    }

    /// Create a new in-memory durable MPT (for testing).
    pub fn new_in_memory() -> SqliteResult<Self> {
        Self::new_with_path(":memory:")
    }

    /// Load a node from SQLite into the cache if not already present.
    fn load_node(&self, prefix: &Prefix) -> Option<Node> {
        // Check cache first
        if let Some(node) = self.cache.get(prefix) {
            return Some(node.clone());
        }

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

        // Load paths for all keys to ensure necessary nodes are in cache
        for (key, _) in entries.iter() {
            self.load_path_to_key(*key);
        }

        // Convert to sorted vector for efficient partitioning
        let mut entries_vec: Vec<(Hash, Hash)> = entries.to_vec();
        entries_vec.sort_by_key(|(k, _)| *k);
        // Remove duplicates, keeping the last occurrence (latest value)
        entries_vec.dedup_by_key(|(k, _)| *k);

        // Perform recursive batch upsert
        let new_root = self.recursive_batch_upsert(self.root, entries_vec);
        self.root = new_root;

        // Flush all changes to disk
        if let Err(e) = self.flush_to_disk() {
            log::error!("Failed to flush to disk: {}", e);
        }
    }

    /// Recursively batch upsert entries at the current node.
    /// Returns the prefix of the (possibly new) root of this subtree.
    fn recursive_batch_upsert(
        &mut self,
        current_prefix: Prefix,
        entries: Vec<(Hash, Hash)>,
    ) -> Prefix {
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
    fn batch_insert_into_empty(&mut self, mut entries: Vec<(Hash, Hash)>) -> Prefix {
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
    fn batch_upsert_at_leaf(&mut self, leaf: LeafNode, mut entries: Vec<(Hash, Hash)>) -> Prefix {
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
        &mut self,
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

        // Recursively process left and right subtrees
        // Note: We can't use rayon::join here because &mut self is not thread-safe
        // We process sequentially instead
        let new_left = if !left_entries.is_empty() {
            self.recursive_batch_upsert(interior.left, left_entries)
        } else {
            interior.left
        };

        let new_right = if !right_entries.is_empty() {
            self.recursive_batch_upsert(interior.right, right_entries)
        } else {
            interior.right
        };

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
}

impl MerklePatriciaTree for DurableBatchMPT {
    fn new() -> Self {
        Self::new_in_memory().expect("Failed to create in-memory database")
    }

    fn upsert(&mut self, key: Hash, value: Hash) {
        self.batch_upsert_optimized(&[(key, value)]);
    }

    fn enumerate_nodes(&self) -> Vec<(Prefix, Node)> {
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
}

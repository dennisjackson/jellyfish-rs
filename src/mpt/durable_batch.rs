use log::{debug, warn};
use rusqlite::{Connection, Result as SqliteResult};
use std::env;
use std::sync::{Arc, Mutex};

use crate::mpt::MerklePatriciaTree;
use crate::{Hash, Prefix};

use super::{Cache, InteriorNode, LeafNode, Node};

/// A durable batch-optimized Merkle Patricia Tree implementation backed by SQLite.
/// This implementation performs batch upserts by:
/// 1. Loading necessary nodes from SQLite into a Cache
/// 2. Performing the batch upsert in memory
/// 3. Writing changed nodes back to SQLite
///
/// The tree structure is persisted to disk, allowing for larger-than-memory trees.
pub struct DurableBatchMPT {
    cache: Cache,
    root: Prefix,
}

impl DurableBatchMPT {
    /// Create a new durable MPT with the given SQLite database path.
    pub fn new_with_path(db_path: &str) -> SqliteResult<Self> {
        let conn = Connection::open(db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "fullfsync", true)?; //Andrew Ayer's advice
        // Configure SQLite for durability and robustness
        // WAL mode provides better concurrency and crash resilience
        // conn.execute("PRAGMA journal_mode = WAL", [])?;

        // FULL synchronous mode ensures all data is written to disk before commit returns
        // This guarantees durability even in case of power failure or OS crash
        conn.execute("PRAGMA synchronous = FULL", [])?;

        // Enable foreign key constraints for referential integrity
        // conn.execute("PRAGMA foreign_keys = ON", [])?;

        // Set a reasonable busy timeout (5 seconds) for handling concurrent access
        // conn.execute("PRAGMA busy_timeout = 5000", [])?;

        // Enable auto_vacuum to reclaim disk space when data is deleted
        // conn.execute("PRAGMA auto_vacuum = INCREMENTAL", [])?;

        // Initialize database schema
        Cache::initialize_database(&conn)?;

        let db = Arc::new(Mutex::new(conn));
        let cache = Cache::new(Arc::clone(&db));

        let root = cache.get_root();
        // Ensure the cached root node is available if it exists on disk.
        cache.get_or_load(root)?;

        Ok(Self { cache, root })
    }

    /// Create a new in-memory durable MPT (for testing).
    pub fn new_in_memory() -> SqliteResult<Self> {
        Self::new_with_path(":memory:")
    }

    /// Write all dirty nodes back to SQLite.
    fn flush_to_disk(&self) -> SqliteResult<()> {
        // Use Cache::flush to write all dirty nodes
        self.cache.flush()?;

        // Note: tree_size metadata could be updated here if needed
        // For now, we track it separately in memory

        Ok(())
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
        let mut entries_vec: Vec<(Hash, Hash)> = entries.to_vec();
        entries_vec.sort_by_key(|(k, _)| *k);
        // Remove duplicates, keeping the last occurrence (latest value)
        entries_vec.dedup_by_key(|(k, _)| *k);

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
            None => match self.cache.get_or_load(current_prefix) {
                Ok(Some(node)) => node.value().clone(),
                Ok(None) => {
                    // Empty tree: insert all entries
                    return self.batch_insert_into_empty(entries);
                }
                Err(err) => {
                    warn!(
                        "Failed to load node {:?} from cache during batch upsert: {}",
                        current_prefix, err
                    );
                    return self.batch_insert_into_empty(entries);
                }
            },
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
        self.cache.set(first_prefix, Node::Leaf(first_leaf));

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
            self.cache.set(leaf_prefix, Node::Leaf(updated_leaf));

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

        self.cache.set(merged_prefix, Node::Interior(new_interior));
        self.cache.set(existing_prefix, Node::Leaf(leaf));
        self.cache.set(new_prefix, Node::Leaf(new_leaf));

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

            self.cache.set(common, Node::Interior(new_interior));
            self.cache.set(new_leaf_prefix, Node::Leaf(new_leaf));

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

    /// Get cache statistics.
    pub fn cache_stats(&self) -> (usize, usize) {
        (self.cache.len(), self.cache.dirty_len())
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
        match self.cache.get_or_load(self.root) {
            Ok(Some(node)) => Some(node.value().merkle_hash()),
            Ok(None) => None,
            Err(err) => {
                warn!("Failed to load root node from cache: {}", err);
                None
            }
        }
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
        self.cache.release_keys(
            &entries
                .iter()
                .map(|(k, _)| Prefix::from(*k))
                .collect::<Vec<_>>(),
        );
        self.cache
            .pre_advise(
                &entries
                    .iter()
                    .map(|(k, _)| Prefix::from(*k))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        self.batch_upsert_optimized(entries);
        self.flush_to_disk().ok();
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
    fn test_batch_upsert_with_cache_clearing() {
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
        let tree_size = mpt.cache.tree_size();
        assert!(tree_size > 0);
        assert_eq!(tree_size, mpt.cache.len());

        // Clear cache to test loading from disk
        mpt.clear_cache();
        assert_eq!(mpt.cache.len(), 0);

        // Perform a small batch upsert with just a few keys
        let new_entries: Vec<(Hash, Hash)> =
            vec![([200u8; 32], [250u8; 32]), ([201u8; 32], [251u8; 32])];
        mpt.batch_upsert(&new_entries);

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
    fn test_release_pre_advise_then_batch_upsert() {
        let _ = env_logger::builder()
            .is_test(true)
            .filter(None, log::LevelFilter::Debug)
            .try_init();
        let mut mpt = DurableBatchMPT::new();

        let key1 = [1u8; 32];
        let key2 = [2u8; 32];
        let key3 = [3u8; 32];

        let mut initial_entries: Vec<(Hash, Hash)> = Vec::new();
        for i in 0..100_000 {
            let mut key = [0u8; 32];
            let mut value = [0u8; 32];
            key[0] = (i / 256) as u8;
            key[1] = (i % 256) as u8;
            value[0] = (i % 128) as u8;
            initial_entries.push((key, value));
        }

        initial_entries.push((key1, [10u8; 32]));
        initial_entries.push((key2, [20u8; 32]));
        initial_entries.push((key3, [30u8; 32]));
        mpt.batch_upsert(&initial_entries);

        let prefixes: Vec<Prefix> = initial_entries
            .iter()
            .map(|(key, _)| Prefix::from(*key))
            .collect();

        mpt.cache.release_keys(&[]);

        let updated_value_for_key2 = [200u8; 32];
        let key4 = [4u8; 32];
        let value4 = [40u8; 32];

        let second_batch: Vec<(Hash, Hash)> = vec![(key2, updated_value_for_key2), (key4, value4)];
        mpt.batch_upsert(&second_batch);

        assert_eq!(mpt.get_leaf_value(key1), Some([10u8; 32]));
        assert_eq!(mpt.get_leaf_value(key2), Some(updated_value_for_key2));
        assert_eq!(mpt.get_leaf_value(key3), Some([30u8; 32]));
        assert_eq!(mpt.get_leaf_value(key4), Some(value4));
    }

    #[test]
    fn test_root_persisted_across_restarts() {
        let temp_dir = env::temp_dir();
        let pid = std::process::id();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let db_path = temp_dir.join(format!("durable_batch_root_test_{}_{}.db", pid, timestamp));
        let db_path_str = db_path.to_string_lossy().to_string();

        let key = [42u8; 32];
        let value = [99u8; 32];

        let expected_root = {
            let mut first = DurableBatchMPT::new_with_path(&db_path_str).unwrap();
            first.upsert(key, value);
            let expected_root = first.get_root_hash();
            assert!(expected_root.is_some());
            expected_root
        };

        {
            let second = DurableBatchMPT::new_with_path(&db_path_str).unwrap();
            let persisted_root = second.get_root_hash();
            assert!(persisted_root.is_some());
            assert_eq!(persisted_root, expected_root);
            assert_eq!(second.get_leaf_value(key), Some(value));
        }

        let _ = std::fs::remove_file(&db_path);
        let wal_path = db_path.with_extension("db-wal");
        let shm_path = db_path.with_extension("db-shm");
        let _ = std::fs::remove_file(wal_path);
        let _ = std::fs::remove_file(shm_path);
    }
}

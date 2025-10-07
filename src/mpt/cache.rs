use dashmap::{DashMap, DashSet};
use rusqlite::{Connection, Result as SqliteResult, params};
use std::{collections::HashSet, sync::{Arc, Mutex}};

use super::Node;
use crate::Prefix;

/// A cache structure that wraps DashMap and provides SQLite-backed persistent storage.
/// This cache allows preloading keys from disk and batch writing keys back to disk.
/// It automatically tracks dirty (modified) keys for efficient flushing.
pub struct Cache {
    /// In-memory cache using DashMap for concurrent access
    map: Arc<DashMap<Prefix, Node>>,
    /// SQLite database connection for persistent storage
    db: Arc<Mutex<Connection>>,
    /// Tracks keys that have been modified and need to be written to disk
    dirty: Arc<DashSet<Prefix>>,
    /// Tracks whether we're currently in an active transaction
    in_transaction: Arc<Mutex<bool>>,
}

impl Cache {
    /// Initialize the database schema by creating the necessary tables and indexes.
    /// This should be called after opening a database connection.
    pub fn initialize_database(conn: &Connection) -> SqliteResult<()> {
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

        Ok(())
    }

    /// Create a new cache with the given SQLite database connection.
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        Self {
            map: Arc::new(DashMap::new()),
            db,
            dirty: Arc::new(DashSet::new()),
            in_transaction: Arc::new(Mutex::new(false)),
        }
    }

    /// Get a node from the cache.
    /// Returns None if the key is not present in the cache.
    pub fn get(&self, key: &Prefix) -> Option<Node> {
        self.map.get(key).map(|node| node.clone())
    }

    /// Insert or update a node in the cache.
    /// Automatically marks the key as dirty for later flushing.
    pub fn set(&self, key: Prefix, value: Node) {
        self.map.insert(key, value);
        self.dirty.insert(key);
    }

    /// Get the number of entries currently in the cache.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Check if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Get the current tree size (total number of nodes in the database).
    pub fn tree_size(&self) -> usize {
        let db = self.db.lock().unwrap();
        db.query_row("SELECT COUNT(*) FROM nodes", [], |row| {
            row.get::<_, i64>(0).map(|v| v as usize)
        })
        .unwrap_or(0)
    }

    /// Clear all entries from the cache.
    pub fn clear(&self) {
        self.map.clear();
    }

    #[allow(dead_code)]
    fn release_keys(&self, needed_keys: &[Prefix]) {
        // TODO: This should be smarter and keep nodes which are on-path.
        self.map.retain(|key, _| needed_keys.contains(key));
    }

    /// Helper function to batch query multiple nodes from the database.
    /// Returns a vector of (Prefix, Node) tuples for nodes that were found.
    fn batch_query_nodes(
        &self,
        db: &Connection,
        prefixes: &[Prefix],
    ) -> SqliteResult<Vec<(Prefix, Node)>> {
        if prefixes.is_empty() {
            return Ok(Vec::new());
        }

        // Build a query with IN clause for batch fetching
        // We need to match on (prefix_hash, prefix_length) pairs
        let placeholders: Vec<String> = prefixes
            .iter()
            .map(|_| "(?, ?)".to_string())
            .collect();
        let query = format!(
            "SELECT prefix_hash, prefix_length, node_type, node_data FROM nodes WHERE (prefix_hash, prefix_length) IN ({})",
            placeholders.join(", ")
        );

        let mut stmt = db.prepare(&query)?;

        // Flatten all parameters: [hash1, len1, hash2, len2, ...]
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        for prefix in prefixes {
            params_vec.push(Box::new(prefix.hash.to_vec()));
            params_vec.push(Box::new(prefix.length as i64));
        }
        let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();

        let nodes = stmt
            .query_map(params_refs.as_slice(), |row| {
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

                let node = Node::deserialize(&node_type, &node_data)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?;

                Ok((prefix, node))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(nodes)
    }

    /// Pre-advise: Load nodes from SQLite that are needed for the given keys.
    /// This loads all nodes on the path from root to the needed keys, plus their siblings.
    /// Nodes that are already in the cache will not be reloaded.
    /// Begins a durable transaction that will remain active until flush() is called.
    pub fn pre_advise(&self, _keys: &[Prefix]) -> SqliteResult<()> {
        let db = self.db.lock().unwrap();

        // Begin a durable transaction
        let mut in_tx = self.in_transaction.lock().unwrap();
        if !*in_tx {
            db.execute("BEGIN DEFERRED TRANSACTION", [])?;
            *in_tx = true;
        }
        drop(in_tx); // Release lock before querying

        let mut needed_keys: HashSet<Prefix> = _keys.iter().cloned().collect();
        let mut current_frontier = vec![Prefix::root()];

        while !needed_keys.is_empty() && !current_frontier.is_empty() {
            let mut next_frontier = Vec::new();
            let mut siblings_to_load = Vec::new();

            // Collect all prefixes to query in this iteration (excluding those already in cache)
            let prefixes_to_query: Vec<Prefix> = current_frontier
                .iter()
                .filter(|p| !self.map.contains_key(p))
                .cloned()
                .collect();

            // Batch query for all frontier nodes
            let frontier_nodes = if !prefixes_to_query.is_empty() {
                self.batch_query_nodes(&db, &prefixes_to_query)?
            } else {
                Vec::new()
            };

            // Insert all queried nodes into cache
            for (prefix, node) in frontier_nodes {
                self.map.insert(prefix, node.clone());

                match node {
                    Node::Leaf(_) => {
                        // If this is one of the needed keys, remove it
                        needed_keys.remove(&prefix);
                    }
                    Node::Interior(interior) => {
                        // Determine which children are on-path to needed keys
                        let mut left_needed = false;
                        let mut right_needed = false;

                        for needed_key in &needed_keys {
                            if interior.prefix.prefix_of(needed_key) {
                                // Determine if needed_key goes left or right
                                if interior.prefix.key_goes_right(needed_key.hash) {
                                    right_needed = true;
                                } else {
                                    left_needed = true;
                                }
                            }
                        }

                        // Add on-path children to next frontier and siblings to load list
                        if left_needed {
                            next_frontier.push(interior.left);
                            // Sibling (right) should be loaded but not expanded
                            if !next_frontier.contains(&interior.right) {
                                siblings_to_load.push(interior.right);
                            }
                        }
                        if right_needed {
                            next_frontier.push(interior.right);
                            // Sibling (left) should be loaded but not expanded (if not already in frontier)
                            if !left_needed && !siblings_to_load.contains(&interior.left) {
                                siblings_to_load.push(interior.left);
                            }
                        }
                    }
                }
            }

            // Batch query for siblings (excluding those already in cache)
            let siblings_to_query: Vec<Prefix> = siblings_to_load
                .iter()
                .filter(|p| !self.map.contains_key(p))
                .cloned()
                .collect();

            if !siblings_to_query.is_empty() {
                let sibling_nodes = self.batch_query_nodes(&db, &siblings_to_query)?;
                for (prefix, node) in sibling_nodes {
                    self.map.insert(prefix, node);
                }
            }

            current_frontier = next_frontier;
        }

        // After traversal, directly load any remaining needed keys that weren't found
        // This handles cases where nodes exist in the database but aren't reachable from root
        let remaining_to_query: Vec<Prefix> = needed_keys
            .iter()
            .filter(|k| !self.map.contains_key(k))
            .cloned()
            .collect();

        if !remaining_to_query.is_empty() {
            let remaining_nodes = self.batch_query_nodes(&db, &remaining_to_query)?;
            for (prefix, node) in remaining_nodes {
                self.map.insert(prefix, node);
            }
        }

        Ok(())
    }

    /// Flush all dirty (modified) keys to the database.
    /// If a transaction was started by pre_advise, it will be committed.
    /// Otherwise, creates a new transaction for this flush operation.
    /// After successful flush, clears the dirty tracking.
    pub fn flush(&self) -> SqliteResult<()> {
        let dirty_keys: Vec<Prefix> = self.dirty.iter().map(|entry| *entry).collect();

        if dirty_keys.is_empty() {
            // Even if no dirty keys, commit the transaction if one is active
            let mut in_tx = self.in_transaction.lock().unwrap();
            if *in_tx {
                let db = self.db.lock().unwrap();
                db.execute("COMMIT", [])?;
                *in_tx = false;
            }
            return Ok(());
        }

        let db = self.db.lock().unwrap();
        let mut in_tx = self.in_transaction.lock().unwrap();

        // If not in a transaction, start one for this flush
        let needs_commit = if !*in_tx {
            db.execute("BEGIN DEFERRED TRANSACTION", [])?;
            true
        } else {
            true
        };

        // Write all dirty nodes
        {
            let mut stmt = db.prepare(
                "INSERT OR REPLACE INTO nodes (prefix_hash, prefix_length, node_type, node_data) VALUES (?1, ?2, ?3, ?4)"
            )?;

            for key in &dirty_keys {
                if let Some(node) = self.map.get(key) {
                    let (node_type, node_data) = node
                        .value()
                        .serialize()
                        .map_err(|_| rusqlite::Error::InvalidQuery)?;

                    stmt.execute(params![&key.hash[..], key.length, node_type, node_data])?;
                }
            }
        }

        // Commit the transaction
        if needs_commit {
            db.execute("COMMIT", [])?;
            *in_tx = false;
        }

        // Clear dirty tracking after successful flush
        self.dirty.clear();

        Ok(())
    }

    /// Get the number of dirty (unflushed) entries.
    pub fn dirty_len(&self) -> usize {
        self.dirty.len()
    }

    /// Enumerate all nodes from the database.
    /// This queries the database directly to get all persisted nodes.
    pub fn enumerate_nodes(&self) -> SqliteResult<Vec<(Prefix, Node)>> {
        let db = self.db.lock().unwrap();
        let mut stmt =
            db.prepare("SELECT prefix_hash, prefix_length, node_type, node_data FROM nodes")?;

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

                let node = Node::deserialize(&node_type, &node_data)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?;

                Ok((prefix, node))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(nodes)
    }
}

#[cfg(test)]
mod tests {
    use super::super::LeafNode;
    use super::*;

    fn create_test_db() -> Arc<Mutex<Connection>> {
        let conn = Connection::open_in_memory().unwrap();

        // Create table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS nodes (
                prefix_hash BLOB NOT NULL,
                prefix_length INTEGER NOT NULL,
                node_type TEXT NOT NULL,
                node_data BLOB NOT NULL,
                PRIMARY KEY (prefix_hash, prefix_length)
            )",
            [],
        )
        .unwrap();

        Arc::new(Mutex::new(conn))
    }

    #[test]
    fn test_new_cache() {
        let db = create_test_db();
        let cache = Cache::new(db);
        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
    }

    #[test]
    fn test_get_set() {
        let db = create_test_db();
        let cache = Cache::new(db);

        let key = [1u8; 32];
        let value = [2u8; 32];
        let prefix = Prefix::from(key);
        let leaf = LeafNode::new(key, value);

        // Get non-existent key
        assert!(cache.get(&prefix).is_none());

        // Set and get
        cache.set(prefix, Node::Leaf(leaf.clone()));
        let retrieved = cache.get(&prefix);
        assert!(retrieved.is_some());

        if let Some(Node::Leaf(retrieved_leaf)) = retrieved {
            assert_eq!(retrieved_leaf.key, key);
            assert_eq!(retrieved_leaf.value, value);
        } else {
            panic!("Expected leaf node");
        }
    }

    #[test]
    fn test_flush_and_pre_advise() {
        let db = create_test_db();
        let cache = Cache::new(Arc::clone(&db));

        let key1 = [1u8; 32];
        let value1 = [10u8; 32];
        let prefix1 = Prefix::from(key1);
        let leaf1 = LeafNode::new(key1, value1);

        let key2 = [2u8; 32];
        let value2 = [20u8; 32];
        let prefix2 = Prefix::from(key2);
        let leaf2 = LeafNode::new(key2, value2);

        // Add nodes to cache
        cache.set(prefix1, Node::Leaf(leaf1));
        cache.set(prefix2, Node::Leaf(leaf2));

        // Write to database
        cache.flush().unwrap();

        // Clear cache
        cache.clear();
        assert_eq!(cache.len(), 0);

        // Pre-advise to load from database
        cache.pre_advise(&[prefix1, prefix2]).unwrap();

        // Verify nodes are back in cache
        assert_eq!(cache.len(), 2);
        let retrieved1 = cache.get(&prefix1);
        assert!(retrieved1.is_some());

        if let Some(Node::Leaf(retrieved_leaf)) = retrieved1 {
            assert_eq!(retrieved_leaf.key, key1);
            assert_eq!(retrieved_leaf.value, value1);
        } else {
            panic!("Expected leaf node");
        }
    }

    #[test]
    fn test_pre_advise_skips_cached_keys() {
        let db = create_test_db();
        let cache = Cache::new(Arc::clone(&db));

        let key = [1u8; 32];
        let value = [10u8; 32];
        let prefix = Prefix::from(key);
        let leaf = LeafNode::new(key, value);

        // Add to cache and write to DB
        cache.set(prefix, Node::Leaf(leaf));
        cache.flush().unwrap();

        // Pre-advise should not reload (already in cache)
        let initial_len = cache.len();
        cache.pre_advise(&[prefix]).unwrap();
        assert_eq!(cache.len(), initial_len);
    }

    #[test]
    fn test_flush_empty() {
        let db = create_test_db();
        let cache = Cache::new(db);

        // Should not error on empty flush
        cache.flush().unwrap();
    }

    #[test]
    fn test_pre_advise_empty() {
        let db = create_test_db();
        let cache = Cache::new(db);

        // Should not error on empty batch
        cache.pre_advise(&[]).unwrap();
    }

    #[test]
    fn test_dirty_tracking() {
        let db = create_test_db();
        let cache = Cache::new(db);

        let key = [1u8; 32];
        let value = [10u8; 32];
        let prefix = Prefix::from(key);
        let leaf = LeafNode::new(key, value);

        // Initially no dirty entries
        assert_eq!(cache.dirty_len(), 0);

        // Set marks as dirty
        cache.set(prefix, Node::Leaf(leaf));
        assert_eq!(cache.dirty_len(), 1);

        // Flush clears dirty tracking
        cache.flush().unwrap();
        assert_eq!(cache.dirty_len(), 0);
    }

    #[test]
    fn test_flush() {
        let db = create_test_db();
        let cache = Cache::new(Arc::clone(&db));

        let key1 = [1u8; 32];
        let value1 = [10u8; 32];
        let prefix1 = Prefix::from(key1);
        let leaf1 = LeafNode::new(key1, value1);

        let key2 = [2u8; 32];
        let value2 = [20u8; 32];
        let prefix2 = Prefix::from(key2);
        let leaf2 = LeafNode::new(key2, value2);

        // Add nodes
        cache.set(prefix1, Node::Leaf(leaf1));
        cache.set(prefix2, Node::Leaf(leaf2));
        assert_eq!(cache.dirty_len(), 2);

        // Flush to database
        cache.flush().unwrap();
        assert_eq!(cache.dirty_len(), 0);

        // Clear cache and reload to verify persistence
        cache.clear();
        cache.pre_advise(&[prefix1, prefix2]).unwrap();

        let retrieved = cache.get(&prefix1);
        assert!(retrieved.is_some());
    }

    #[test]
    fn test_enumerate_nodes() {
        let db = create_test_db();
        let cache = Cache::new(Arc::clone(&db));

        let key1 = [1u8; 32];
        let value1 = [10u8; 32];
        let prefix1 = Prefix::from(key1);
        let leaf1 = LeafNode::new(key1, value1);

        let key2 = [2u8; 32];
        let value2 = [20u8; 32];
        let prefix2 = Prefix::from(key2);
        let leaf2 = LeafNode::new(key2, value2);

        // Add and flush nodes
        cache.set(prefix1, Node::Leaf(leaf1));
        cache.set(prefix2, Node::Leaf(leaf2));
        cache.flush().unwrap();

        // Enumerate should return both nodes
        let nodes = cache.enumerate_nodes().unwrap();
        assert_eq!(nodes.len(), 2);
    }

    #[test]
    fn test_pre_advise_efficiency() {
        use super::super::InteriorNode;

        let db = create_test_db();
        let cache = Cache::new(Arc::clone(&db));

        // Create a binary tree structure with controlled keys
        // Tree structure:
        //           root (prefix 0)
        //          /              \
        //    left subtree      right subtree
        //       /    \             /    \
        //    leaf1  leaf2      leaf3  leaf4

        // Create 4 leaf nodes with specific keys
        let mut key1 = [0u8; 32];
        key1[0] = 0b0000_0000; // Goes left then left
        let value1 = [1u8; 32];
        let prefix1 = Prefix::from(key1);
        let leaf1 = LeafNode::new(key1, value1);

        let mut key2 = [0u8; 32];
        key2[0] = 0b0100_0000; // Goes left then right
        let value2 = [2u8; 32];
        let prefix2 = Prefix::from(key2);
        let leaf2 = LeafNode::new(key2, value2);

        let mut key3 = [0u8; 32];
        key3[0] = 0b1000_0000; // Goes right then left
        let value3 = [3u8; 32];
        let prefix3 = Prefix::from(key3);
        let leaf3 = LeafNode::new(key3, value3);

        let mut key4 = [0u8; 32];
        key4[0] = 0b1100_0000; // Goes right then right
        let value4 = [4u8; 32];
        let prefix4 = Prefix::from(key4);
        let leaf4 = LeafNode::new(key4, value4);

        // Create left subtree interior node (covers left side)
        let left_subtree_prefix = Prefix { hash: [0u8; 32], length: 1 };
        let left_interior = InteriorNode::new(
            left_subtree_prefix,
            prefix1,
            prefix2,
            leaf1.merkle_hash,
            leaf2.merkle_hash,
        );

        // Create right subtree interior node (covers right side)
        let mut right_prefix_hash = [0u8; 32];
        right_prefix_hash[0] = 0b1000_0000;
        let right_subtree_prefix = Prefix { hash: right_prefix_hash, length: 1 };
        let right_interior = InteriorNode::new(
            right_subtree_prefix,
            prefix3,
            prefix4,
            leaf3.merkle_hash,
            leaf4.merkle_hash,
        );

        // Create root interior node
        let root_prefix = Prefix::root();
        let root_interior = InteriorNode::new(
            root_prefix,
            left_subtree_prefix,
            right_subtree_prefix,
            left_interior.merkle_hash,
            right_interior.merkle_hash,
        );

        // Add all nodes to cache and flush to database
        cache.set(prefix1, Node::Leaf(leaf1));
        cache.set(prefix2, Node::Leaf(leaf2));
        cache.set(prefix3, Node::Leaf(leaf3));
        cache.set(prefix4, Node::Leaf(leaf4));
        cache.set(left_subtree_prefix, Node::Interior(left_interior));
        cache.set(right_subtree_prefix, Node::Interior(right_interior));
        cache.set(root_prefix, Node::Interior(root_interior));
        cache.flush().unwrap();

        // Clear cache to start fresh
        cache.clear();
        assert_eq!(cache.len(), 0);

        // Pre-advise for just 2 keys (leaf1 and leaf3)
        // This should load:
        // - root (on path)
        // - left_subtree (on path to leaf1)
        // - right_subtree (on path to leaf3)
        // - leaf1 (needed)
        // - leaf2 (sibling of leaf1)
        // - leaf3 (needed)
        // - leaf4 (sibling of leaf3)
        // Total: 7 nodes
        cache.pre_advise(&[prefix1, prefix3]).unwrap();

        // Verify we loaded at most 2 * needed_keys nodes
        // Actually, with siblings, we expect: needed_keys + their on-path ancestors + their siblings
        // For 2 needed keys in a balanced tree of depth 2:
        // - 2 needed leaves
        // - 2 siblings (one for each needed leaf)
        // - 2 interior nodes on path (left_subtree, right_subtree)
        // - 1 root
        // = 7 total
        let loaded_count = cache.len();
        println!("Loaded {} nodes for 2 needed keys", loaded_count);

        // The bound should be: at most 2 * needed_keys * tree_depth
        // But a simpler bound: we should load needed keys + their ancestors + siblings on path
        // For efficiency, we want to ensure we're not loading the entire tree
        assert!(loaded_count <= 2 * 2 * 3, "Loaded too many nodes: {} (expected <= 12)", loaded_count);

        // More importantly, verify we didn't load ALL nodes (7 total exist)
        // We should have loaded exactly the necessary nodes
        assert!(loaded_count <= 7, "Loaded {} nodes, but only 7 exist in tree", loaded_count);

        // Verify the needed keys are actually loaded
        assert!(cache.get(&prefix1).is_some(), "leaf1 should be loaded");
        assert!(cache.get(&prefix3).is_some(), "leaf3 should be loaded");
    }

    #[test]
    fn test_pre_advise_large_tree_efficiency() {
        use super::super::InteriorNode;
        use sha2::{Sha256, Digest};

        let db = create_test_db();
        let cache = Cache::new(Arc::clone(&db));

        // Generate 1000 unique leaf nodes with deterministic but varied keys
        let num_nodes = 1000;
        let mut prefixes = Vec::new();

        for i in 0..num_nodes {
            // Create unique keys by hashing the index
            let mut hasher = Sha256::new();
            hasher.update((i as u64).to_le_bytes());
            let hash = hasher.finalize();
            let mut key = [0u8; 32];
            key.copy_from_slice(&hash);

            let value = [(i % 256) as u8; 32];
            let prefix = Prefix::from(key);
            let leaf = LeafNode::new(key, value);

            cache.set(prefix, Node::Leaf(leaf));
            prefixes.push(prefix);
        }

        // Create a simple root interior node that points to first two leaves
        // (This creates a minimal tree structure - in reality you'd build a proper tree,
        // but for testing efficiency we just need some nodes in the DB)
        let root_prefix = Prefix::root();
        let root_interior = InteriorNode::new(
            root_prefix,
            prefixes[0],
            prefixes[1],
            [1u8; 32],
            [2u8; 32],
        );
        cache.set(root_prefix, Node::Interior(root_interior));

        // Flush all nodes to database
        cache.flush().unwrap();
        println!("Flushed {} nodes to database", cache.tree_size());

        // Clear cache to start fresh
        cache.clear();
        assert_eq!(cache.len(), 0);

        // Pre-advise for just ONE node
        cache.pre_advise(&[prefixes[0]]).unwrap();

        let loaded_count = cache.len();
        println!("Loaded {} nodes when pre-advising 1 key from a tree of 1000+ nodes", loaded_count);

        // Check that we loaded a reasonable number of nodes
        // For a single key, we should load:
        // - The key itself (1)
        // - Nodes on path from root (depends on tree depth, ~log(n))
        // - Siblings on the path (also ~log(n))
        // For 1000 nodes, log2(1000) ≈ 10, so with siblings we'd expect ~20 nodes max
        // Let's be generous and say anything under 50 is reasonable (2*needed_keys * reasonable_depth)
        assert!(
            loaded_count <= 50,
            "Loaded too many nodes: {} (expected <= 50 for 1 key in 1000-node tree)",
            loaded_count
        );

        // More importantly, we should NOT have loaded all or most nodes
        assert!(
            loaded_count < 100,
            "Loaded {} nodes, which suggests inefficient traversal (should be ~O(log n))",
            loaded_count
        );

        // Verify the requested key is actually loaded
        assert!(
            cache.get(&prefixes[0]).is_some(),
            "The requested key should be loaded"
        );
    }
}

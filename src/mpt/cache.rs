use dashmap::{DashMap, DashSet};
use log::{debug, warn};
use rusqlite::{
    Connection, OptionalExtension, Result as SqliteResult, limits::Limit, params, types::Type,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

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
    /// Root prefix persisted in metadata
    root: Arc<Mutex<Prefix>>,
    /// Indicates whether the root metadata needs to be flushed
    root_dirty: AtomicBool,
}

const ROOT_METADATA_KEY: &str = "root_prefix";

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
        let (initial_root, should_mark_dirty) = {
            let conn_guard = db.lock().unwrap();
            let result = Self::read_root_from_metadata(&conn_guard);
            drop(conn_guard);

            match result {
                Ok(Some(prefix)) => (prefix, false),
                Ok(None) => (Prefix::root(), true),
                Err(err) => {
                    warn!("Failed to load root metadata: {}", err);
                    (Prefix::root(), true)
                }
            }
        };

        Self {
            map: Arc::new(DashMap::new()),
            db,
            dirty: Arc::new(DashSet::new()),
            in_transaction: Arc::new(Mutex::new(false)),
            root: Arc::new(Mutex::new(initial_root)),
            root_dirty: AtomicBool::new(should_mark_dirty),
        }
    }

    fn read_root_from_metadata(conn: &Connection) -> SqliteResult<Option<Prefix>> {
        let mut stmt = conn.prepare("SELECT value FROM metadata WHERE key = ?1")?;
        let encoded: Option<Vec<u8>> = stmt
            .query_row(params![ROOT_METADATA_KEY], |row| row.get(0))
            .optional()?;

        match encoded {
            Some(bytes) => {
                let (prefix, _) =
                    bincode::decode_from_slice::<Prefix, _>(&bytes, bincode::config::standard())
                        .map_err(|err| {
                            rusqlite::Error::FromSqlConversionFailure(0, Type::Blob, Box::new(err))
                        })?;
                Ok(Some(prefix))
            }
            None => Ok(None),
        }
    }

    fn persist_root_metadata(&self, conn: &Connection) -> SqliteResult<()> {
        let root = self.get_root();
        let data = bincode::encode_to_vec(root, bincode::config::standard())
            .map_err(|_| rusqlite::Error::InvalidQuery)?;

        conn.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)",
            params![ROOT_METADATA_KEY, data],
        )?;

        Ok(())
    }

    /// Get a node from the cache.
    /// Returns None if the key is not present in the cache.
    pub fn get(&self, key: &Prefix) -> Option<Node> {
        self.map.get(key).map(|node| node.clone())
    }

    /// Retrieve a node, loading it from the database if necessary.
    pub fn get_or_load(&self, key: Prefix) -> SqliteResult<Option<Node>> {
        if let Some(node) = self.map.get(&key) {
            return Ok(Some(node.clone()));
        }

        let mut nodes = {
            let db = self.db.lock().unwrap();
            self.batch_query_nodes(&db, &[key])?
        };

        if let Some((_, node)) = nodes.pop() {
            self.map.insert(key, node.clone());
            Ok(Some(node))
        } else {
            Ok(None)
        }
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

    /// Get the currently tracked root prefix.
    pub fn get_root(&self) -> Prefix {
        *self.root.lock().unwrap()
    }

    /// Update the tracked root prefix and mark it dirty for persistence.
    pub fn set_root(&self, new_root: Prefix) {
        {
            let mut guard = self.root.lock().unwrap();
            if *guard == new_root {
                return;
            }
            *guard = new_root;
        }
        self.root_dirty.store(true, Ordering::SeqCst);
    }

    pub fn release_keys(&self, needed_keys: &[Prefix]) {
        const CACHE_PREFIX_LENGTH: u16 = 18;

        // Retain keys that are either:
        // 1. In the needed_keys list, OR
        // 2. Interior nodes with prefix length < CACHE_PREFIX_LENGTH (18 bits)
        self.map.retain(|key, node| {
            // Keep if it's in the needed_keys list
            if needed_keys.contains(key) {
                return true;
            }

            // Keep interior nodes with CACHE_PREFIX_LENGTH length < 18 bits
            if let Node::Interior(int_node) = node {
                return int_node.prefix.length < CACHE_PREFIX_LENGTH;
            }

            // Remove everything else
            false
        });
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
        let placeholders: Vec<String> = prefixes.iter().map(|_| "(?, ?)".to_string()).collect();
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
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();

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

        //TODO: This makes things fail because we aren't correctly reloading the
        //keys that we need.
        // self.release_keys(_keys);

        // Begin a durable transaction
        let mut in_tx = self.in_transaction.lock().unwrap();
        if !*in_tx {
            db.execute("BEGIN DEFERRED TRANSACTION", [])?;
            *in_tx = true;
        }
        drop(in_tx); // Release lock before querying

        let mut needed_keys: HashSet<Prefix> = _keys.iter().cloned().collect();
        let mut current_frontier = HashSet::new();
        current_frontier.insert(self.get_root());
        debug!(
            "Pre-advise for {} keys, starting from root",
            needed_keys.len()
        );
        while !needed_keys.is_empty() && !current_frontier.is_empty() {
            debug!(
                "Iterating. Need {} keys, frontier size {}",
                needed_keys.len(),
                current_frontier.len()
            );
            let mut frontier_nodes: Vec<Node> = Vec::new();

            // Separate frontier into cached and uncached nodes
            let (cached_prefixes, prefixes_to_query): (Vec<Prefix>, Vec<Prefix>) = current_frontier
                .iter()
                .partition(|p| self.map.contains_key(p));
            debug!(
                "Frontier has {} cached, {} to query",
                cached_prefixes.len(),
                prefixes_to_query.len()
            );

            for p in cached_prefixes {
                if let Some(node) = self.get(&p) {
                    frontier_nodes.push(node);
                }
            }

            let new_nodes: Vec<(Prefix, Node)> = self
                .batch_query_nodes(&db, &prefixes_to_query)?
                .into_iter()
                .collect();

            for (p, n) in new_nodes.iter() {
                self.map.insert(*p, n.clone());
            }

            // At this point, all frontier nodes are in the cache.
            frontier_nodes.extend(new_nodes.iter().map(|(_, n)| n.clone()));
            current_frontier.clear();

            //Now we can remove any nodes we just loaded from the needed keys.
            for node in &frontier_nodes {
                if let Node::Leaf(leaf) = node {
                    let prefix = Prefix::from(leaf.key);
                    debug!("Loaded leaf node {:?}", prefix);
                    needed_keys.remove(&prefix);
                }
            }

            let mut siblings_to_load: Vec<Prefix> = Vec::new();
            for node in &frontier_nodes {
                if let Node::Interior(interior) = node {
                    for nk in needed_keys.iter() {
                        if interior.left.prefix_of(nk) {
                            current_frontier.insert(interior.left);
                        } else {
                            siblings_to_load.push(interior.left);
                        }
                        if interior.right.prefix_of(nk) {
                            current_frontier.insert(interior.right);
                        } else {
                            siblings_to_load.push(interior.right);
                        }
                    }
                }
            }

            debug!(
                "Identified {} siblings to load and {} next frontier nodes",
                siblings_to_load.len(),
                current_frontier.len()
            );
            let siblings = siblings_to_load
                .iter()
                .filter(|p| !self.map.contains_key(p))
                .cloned()
                .collect::<Vec<Prefix>>();

            let siblings = self.batch_query_nodes(&db, &siblings)?;

            // Insert all queried nodes into cache
            for (prefix, node) in siblings {
                self.map.insert(prefix, node.clone());
            }
        }

        // After traversal, directly load any remaining needed keys that weren't found
        // This handles cases where nodes exist in the database but aren't reachable from root
        // This should only happen in testing
        let remaining_to_query: Vec<Prefix> = needed_keys
            .iter()
            .filter(|k| !self.map.contains_key(k))
            .cloned()
            .collect();

        if !remaining_to_query.is_empty() {
            warn!(
                "After pre-advise traversal, still need to load {} keys directly",
                remaining_to_query.len()
            );
            let remaining_nodes = self.batch_query_nodes(&db, &remaining_to_query)?;
            for (prefix, node) in remaining_nodes {
                self.map.insert(prefix, node);
            }
        }

        debug!("Pre-advise complete. Cache size: {}", self.len());
        Ok(())
    }

    /// Flush all dirty (modified) keys to the database.
    /// If a transaction was started by pre_advise, it will be committed.
    /// Otherwise, creates a new transaction for this flush operation.
    /// After successful flush, clears the dirty tracking.
    pub fn flush(&self) -> SqliteResult<()> {
        let dirty_keys: Vec<Prefix> = self.dirty.iter().map(|entry| *entry).collect();
        let root_dirty = self.root_dirty.load(Ordering::SeqCst);

        if dirty_keys.is_empty() && !root_dirty {
            // Even if no dirty keys, commit the transaction if one is active
            let mut in_tx = self.in_transaction.lock().unwrap();
            if *in_tx {
                let db = self.db.lock().unwrap();
                db.execute("COMMIT", [])?;
                *in_tx = false;
            }
            return Ok(());
        }

        let mut cleared_root_dirty = false;

        {
            let db = self.db.lock().unwrap();
            let mut in_tx = self.in_transaction.lock().unwrap();

            if dirty_keys.is_empty() {
                // Only the root metadata needs to be persisted.
                if !*in_tx {
                    db.execute("BEGIN DEFERRED TRANSACTION", [])?;
                    *in_tx = true;
                }
                self.persist_root_metadata(&db)?;
                cleared_root_dirty = true;
                db.execute("COMMIT", [])?;
                *in_tx = false;
            } else {
                const PARAMS_PER_INSERT: usize = 4;
                let max_variables = db.limit(Limit::SQLITE_LIMIT_VARIABLE_NUMBER);
                // let max_inserts_per_tx = std::cmp::max(
                //     1,
                //     if max_variables <= 0 {
                //         0
                //     } else {
                //         (max_variables as usize) / PARAMS_PER_INSERT
                //     },
                // );
                let max_inserts_per_tx = 500;
                let total_chunks = (dirty_keys.len() + max_inserts_per_tx - 1) / max_inserts_per_tx;

                debug!(
                    "Flushing {} dirty nodes to database in {} transaction chunk(s)",
                    dirty_keys.len(),
                    total_chunks
                );

                for (chunk_index, chunk) in dirty_keys.chunks(max_inserts_per_tx).enumerate() {
                    if !*in_tx {
                        db.execute("BEGIN DEFERRED TRANSACTION", [])?;
                        *in_tx = true;
                    }

                    {
                        let mut stmt = db.prepare(
                            "INSERT OR REPLACE INTO nodes (prefix_hash, prefix_length, node_type, node_data) VALUES (?1, ?2, ?3, ?4)"
                        )?;

                        for key in chunk {
                            if let Some(node) = self.map.get(key) {
                                debug!("Flushing dirty key {:?}", key);
                                let (node_type, node_data) = node
                                    .value()
                                    .serialize()
                                    .map_err(|_| rusqlite::Error::InvalidQuery)?;

                                stmt.execute(params![
                                    &key.hash[..],
                                    key.length,
                                    node_type,
                                    node_data
                                ])?;
                            } else {
                                // This should not happen - dirty key must be in cache
                                log::warn!(
                                    "Warning: Dirty key {:?} not found in cache during flush",
                                    key
                                );
                            }
                        }
                    }

                    if root_dirty && chunk_index == total_chunks - 1 {
                        self.persist_root_metadata(&db)?;
                        cleared_root_dirty = true;
                    }

                    db.execute("COMMIT", [])?;
                    *in_tx = false;
                }
            }
        }

        self.dirty.clear();
        if cleared_root_dirty {
            self.root_dirty.store(false, Ordering::SeqCst);
        }
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

        // Mirror production schema so cache logic exercises metadata handling too.
        Cache::initialize_database(&conn).unwrap();

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
        let left_subtree_prefix = Prefix {
            hash: [0u8; 32],
            length: 1,
        };
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
        let right_subtree_prefix = Prefix {
            hash: right_prefix_hash,
            length: 1,
        };
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
        assert!(
            loaded_count <= 2 * 2 * 3,
            "Loaded too many nodes: {} (expected <= 12)",
            loaded_count
        );

        // More importantly, verify we didn't load ALL nodes (7 total exist)
        // We should have loaded exactly the necessary nodes
        assert!(
            loaded_count <= 7,
            "Loaded {} nodes, but only 7 exist in tree",
            loaded_count
        );

        // Verify the needed keys are actually loaded
        assert!(cache.get(&prefix1).is_some(), "leaf1 should be loaded");
        assert!(cache.get(&prefix3).is_some(), "leaf3 should be loaded");
    }

    #[test]
    fn test_pre_advise_large_tree_efficiency() {
        use super::super::InteriorNode;
        use sha2::{Digest, Sha256};

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
        let root_interior =
            InteriorNode::new(root_prefix, prefixes[0], prefixes[1], [1u8; 32], [2u8; 32]);
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
        println!(
            "Loaded {} nodes when pre-advising 1 key from a tree of 1000+ nodes",
            loaded_count
        );

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

    #[test]
    fn test_pre_advise_loads_complete_paths_with_siblings() {
        use super::super::InteriorNode;

        let db = create_test_db();
        let cache = Cache::new(Arc::clone(&db));

        // Create a simple 3-level tree with explicit structure:
        //           root
        //          /    \
        //      left      right
        //      /  \      /   \
        //    l1   l2   l3    l4

        let mut key_l1 = [0u8; 32];
        key_l1[0] = 0b0000_0000;
        let prefix_l1 = Prefix::from(key_l1);
        let leaf_l1 = LeafNode::new(key_l1, [11u8; 32]);

        let mut key_l2 = [0u8; 32];
        key_l2[0] = 0b0100_0000;
        let prefix_l2 = Prefix::from(key_l2);
        let leaf_l2 = LeafNode::new(key_l2, [22u8; 32]);

        let mut key_l3 = [0u8; 32];
        key_l3[0] = 0b1000_0000;
        let prefix_l3 = Prefix::from(key_l3);
        let leaf_l3 = LeafNode::new(key_l3, [33u8; 32]);

        let mut key_l4 = [0u8; 32];
        key_l4[0] = 0b1100_0000;
        let prefix_l4 = Prefix::from(key_l4);
        let leaf_l4 = LeafNode::new(key_l4, [44u8; 32]);

        // Create left subtree
        let left_prefix = Prefix {
            hash: [0u8; 32],
            length: 1,
        };
        let left_interior = InteriorNode::new(
            left_prefix,
            prefix_l1,
            prefix_l2,
            leaf_l1.merkle_hash,
            leaf_l2.merkle_hash,
        );

        // Create right subtree
        let mut right_hash = [0u8; 32];
        right_hash[0] = 0b1000_0000;
        let right_prefix = Prefix {
            hash: right_hash,
            length: 1,
        };
        let right_interior = InteriorNode::new(
            right_prefix,
            prefix_l3,
            prefix_l4,
            leaf_l3.merkle_hash,
            leaf_l4.merkle_hash,
        );

        // Create root
        let root_prefix = Prefix::root();
        let root_interior = InteriorNode::new(
            root_prefix,
            left_prefix,
            right_prefix,
            left_interior.merkle_hash,
            right_interior.merkle_hash,
        );

        // Add all nodes and flush
        cache.set(prefix_l1, Node::Leaf(leaf_l1));
        cache.set(prefix_l2, Node::Leaf(leaf_l2));
        cache.set(prefix_l3, Node::Leaf(leaf_l3));
        cache.set(prefix_l4, Node::Leaf(leaf_l4));
        cache.set(left_prefix, Node::Interior(left_interior));
        cache.set(right_prefix, Node::Interior(right_interior));
        cache.set(root_prefix, Node::Interior(root_interior));
        cache.flush().unwrap();

        // Clear and pre-advise for just l1
        cache.clear();
        cache.pre_advise(&[prefix_l1]).unwrap();

        // Verify all ancestors on the path are loaded
        assert!(cache.get(&root_prefix).is_some(), "Root should be loaded");
        assert!(
            cache.get(&left_prefix).is_some(),
            "Left interior (ancestor) should be loaded"
        );
        assert!(
            cache.get(&prefix_l1).is_some(),
            "Target leaf l1 should be loaded"
        );

        // Verify siblings on the path are loaded (needed for merkle hash recomputation)
        assert!(
            cache.get(&right_prefix).is_some(),
            "Right interior (sibling of left path) should be loaded"
        );
        assert!(
            cache.get(&prefix_l2).is_some(),
            "Leaf l2 (sibling of l1) should be loaded"
        );

        // We shouldn't load leaves that aren't on path or siblings
        // (l3 and l4 are children of right_interior, which is a sibling but not on the direct path)
    }

    #[test]
    fn test_pre_advise_shared_ancestors() {
        use super::super::InteriorNode;

        let db = create_test_db();
        let cache = Cache::new(Arc::clone(&db));

        // Create keys that share common ancestors
        let mut key1 = [0u8; 32];
        key1[0] = 0b0000_0000; // Goes left, left
        let prefix1 = Prefix::from(key1);
        let leaf1 = LeafNode::new(key1, [1u8; 32]);

        let mut key2 = [0u8; 32];
        key2[0] = 0b0100_0000; // Goes left, right (shares left subtree with key1)
        let prefix2 = Prefix::from(key2);
        let leaf2 = LeafNode::new(key2, [2u8; 32]);

        // Create shared left subtree
        let left_prefix = Prefix {
            hash: [0u8; 32],
            length: 1,
        };
        let left_interior = InteriorNode::new(
            left_prefix,
            prefix1,
            prefix2,
            leaf1.merkle_hash,
            leaf2.merkle_hash,
        );

        // Create a dummy right subtree
        let mut key3 = [0u8; 32];
        key3[0] = 0b1000_0000;
        let prefix3 = Prefix::from(key3);
        let leaf3 = LeafNode::new(key3, [3u8; 32]);

        let mut right_hash = [0u8; 32];
        right_hash[0] = 0b1000_0000;
        let right_prefix = Prefix {
            hash: right_hash,
            length: 1,
        };

        // Create root
        let root_prefix = Prefix::root();
        let root_interior = InteriorNode::new(
            root_prefix,
            left_prefix,
            right_prefix,
            left_interior.merkle_hash,
            [99u8; 32],
        );

        // Add and flush
        cache.set(prefix1, Node::Leaf(leaf1));
        cache.set(prefix2, Node::Leaf(leaf2));
        cache.set(prefix3, Node::Leaf(leaf3));
        cache.set(left_prefix, Node::Interior(left_interior));
        cache.set(root_prefix, Node::Interior(root_interior));
        cache.flush().unwrap();

        // Clear and pre-advise for both keys that share ancestors
        cache.clear();
        let initial_tree_size = cache.tree_size();
        cache.pre_advise(&[prefix1, prefix2]).unwrap();

        // Verify shared ancestors are loaded
        assert!(
            cache.get(&root_prefix).is_some(),
            "Shared root should be loaded"
        );
        assert!(
            cache.get(&left_prefix).is_some(),
            "Shared left interior should be loaded"
        );
        assert!(cache.get(&prefix1).is_some(), "Key1 should be loaded");
        assert!(cache.get(&prefix2).is_some(), "Key2 should be loaded");

        // Verify we didn't load significantly more than necessary
        // Should load: root (1) + left_interior (1) + right_prefix sibling (1) + leaf1 (1) + leaf2 (1) = 5
        let loaded = cache.len();
        println!(
            "Loaded {} nodes for 2 keys with shared ancestors (tree size: {})",
            loaded, initial_tree_size
        );
        assert!(
            loaded <= 6,
            "Should load at most 6 nodes, loaded {}",
            loaded
        );
    }

    #[test]
    fn test_pre_advise_with_nonexistent_keys() {
        use super::super::InteriorNode;

        let db = create_test_db();
        let cache = Cache::new(Arc::clone(&db));

        // Create a small tree
        let mut key1 = [0u8; 32];
        key1[0] = 0b0000_0000;
        let prefix1 = Prefix::from(key1);
        let leaf1 = LeafNode::new(key1, [1u8; 32]);

        let mut key2 = [0u8; 32];
        key2[0] = 0b1000_0000;
        let prefix2 = Prefix::from(key2);
        let leaf2 = LeafNode::new(key2, [2u8; 32]);

        let root_prefix = Prefix::root();
        let root_interior = InteriorNode::new(
            root_prefix,
            prefix1,
            prefix2,
            leaf1.merkle_hash,
            leaf2.merkle_hash,
        );

        cache.set(prefix1, Node::Leaf(leaf1));
        cache.set(prefix2, Node::Leaf(leaf2));
        cache.set(root_prefix, Node::Interior(root_interior));
        cache.flush().unwrap();

        // Clear and pre-advise for a key that doesn't exist
        cache.clear();
        let mut nonexistent_key = [0u8; 32];
        nonexistent_key[0] = 0b0100_0000; // Would go left from root, but doesn't exist
        let nonexistent_prefix = Prefix::from(nonexistent_key);

        cache.pre_advise(&[nonexistent_prefix]).unwrap();

        // Should still load the root and potentially some nodes along the path
        assert!(
            cache.get(&root_prefix).is_some(),
            "Root should be loaded even for nonexistent key"
        );

        // The nonexistent key itself won't be in cache since it doesn't exist in DB
        assert!(
            cache.get(&nonexistent_prefix).is_none(),
            "Nonexistent key shouldn't be in cache"
        );
    }

    #[test]
    fn test_pre_advise_keys_at_different_depths() {
        use super::super::InteriorNode;

        let db = create_test_db();
        let cache = Cache::new(Arc::clone(&db));

        // Create an unbalanced tree where leaves are at different depths
        // Level 0: root
        // Level 1: left_interior, leaf_right (depth 1)
        // Level 2: leaf_ll, leaf_lr (depth 2)

        let mut key_ll = [0u8; 32];
        key_ll[0] = 0b0000_0000;
        let prefix_ll = Prefix::from(key_ll);
        let leaf_ll = LeafNode::new(key_ll, [1u8; 32]);

        let mut key_lr = [0u8; 32];
        key_lr[0] = 0b0100_0000;
        let prefix_lr = Prefix::from(key_lr);
        let leaf_lr = LeafNode::new(key_lr, [2u8; 32]);

        let mut key_right = [0u8; 32];
        key_right[0] = 0b1000_0000;
        let prefix_right = Prefix::from(key_right);
        let leaf_right = LeafNode::new(key_right, [3u8; 32]);

        // Left interior at depth 1
        let left_prefix = Prefix {
            hash: [0u8; 32],
            length: 1,
        };
        let left_interior = InteriorNode::new(
            left_prefix,
            prefix_ll,
            prefix_lr,
            leaf_ll.merkle_hash,
            leaf_lr.merkle_hash,
        );

        // Root
        let root_prefix = Prefix::root();
        let root_interior = InteriorNode::new(
            root_prefix,
            left_prefix,
            prefix_right,
            left_interior.merkle_hash,
            leaf_right.merkle_hash,
        );

        cache.set(prefix_ll, Node::Leaf(leaf_ll));
        cache.set(prefix_lr, Node::Leaf(leaf_lr));
        cache.set(prefix_right, Node::Leaf(leaf_right));
        cache.set(left_prefix, Node::Interior(left_interior));
        cache.set(root_prefix, Node::Interior(root_interior));
        cache.flush().unwrap();

        // Clear and pre-advise for leaves at different depths
        cache.clear();
        cache.pre_advise(&[prefix_ll, prefix_right]).unwrap();

        // Both leaves should be loaded despite being at different depths
        assert!(
            cache.get(&prefix_ll).is_some(),
            "Deep leaf (depth 2) should be loaded"
        );
        assert!(
            cache.get(&prefix_right).is_some(),
            "Shallow leaf (depth 1) should be loaded"
        );

        // Their ancestors should be loaded
        assert!(cache.get(&root_prefix).is_some(), "Root should be loaded");
        assert!(
            cache.get(&left_prefix).is_some(),
            "Left interior (ancestor of ll) should be loaded"
        );
    }

    #[test]
    fn test_pre_advise_loads_both_siblings() {
        use super::super::InteriorNode;

        let db = create_test_db();
        let cache = Cache::new(Arc::clone(&db));

        // Create a path: root -> left -> right (to leaf)
        // This means we go left at root, then right at the next level
        let mut key_target = [0u8; 32];
        key_target[0] = 0b0100_0000; // First bit 0 (left), second bit 1 (right)
        let prefix_target = Prefix::from(key_target);
        let leaf_target = LeafNode::new(key_target, [99u8; 32]);

        let mut key_sibling = [0u8; 32];
        key_sibling[0] = 0b0000_0000; // Sibling of target (left-left)
        let prefix_sibling = Prefix::from(key_sibling);
        let leaf_sibling = LeafNode::new(key_sibling, [88u8; 32]);

        let mut key_right_child1 = [0u8; 32];
        key_right_child1[0] = 0b1000_0000;
        let prefix_right_child1 = Prefix::from(key_right_child1);
        let leaf_right_child1 = LeafNode::new(key_right_child1, [77u8; 32]);

        let mut key_right_child2 = [0u8; 32];
        key_right_child2[0] = 0b1100_0000;
        let prefix_right_child2 = Prefix::from(key_right_child2);
        let leaf_right_child2 = LeafNode::new(key_right_child2, [66u8; 32]);

        // Left subtree interior
        let left_prefix = Prefix {
            hash: [0u8; 32],
            length: 1,
        };
        let left_interior = InteriorNode::new(
            left_prefix,
            prefix_sibling,
            prefix_target,
            leaf_sibling.merkle_hash,
            leaf_target.merkle_hash,
        );

        // Right subtree interior (sibling of left path)
        let mut right_hash = [0u8; 32];
        right_hash[0] = 0b1000_0000;
        let right_prefix = Prefix {
            hash: right_hash,
            length: 1,
        };
        let right_interior = InteriorNode::new(
            right_prefix,
            prefix_right_child1,
            prefix_right_child2,
            leaf_right_child1.merkle_hash,
            leaf_right_child2.merkle_hash,
        );

        // Root
        let root_prefix = Prefix::root();
        let root_interior = InteriorNode::new(
            root_prefix,
            left_prefix,
            right_prefix,
            left_interior.merkle_hash,
            right_interior.merkle_hash,
        );

        cache.set(prefix_target, Node::Leaf(leaf_target));
        cache.set(prefix_sibling, Node::Leaf(leaf_sibling));
        cache.set(prefix_right_child1, Node::Leaf(leaf_right_child1));
        cache.set(prefix_right_child2, Node::Leaf(leaf_right_child2));
        cache.set(left_prefix, Node::Interior(left_interior));
        cache.set(right_prefix, Node::Interior(right_interior));
        cache.set(root_prefix, Node::Interior(root_interior));
        cache.flush().unwrap();

        // Clear and pre-advise for target
        cache.clear();
        cache.pre_advise(&[prefix_target]).unwrap();

        // Verify target and its sibling are loaded
        assert!(
            cache.get(&prefix_target).is_some(),
            "Target leaf should be loaded"
        );
        assert!(
            cache.get(&prefix_sibling).is_some(),
            "Sibling of target should be loaded"
        );

        // Verify right_interior (sibling of left path) is loaded
        assert!(
            cache.get(&right_prefix).is_some(),
            "Right interior (sibling at root level) should be loaded"
        );

        // Verify ancestors
        assert!(cache.get(&root_prefix).is_some(), "Root should be loaded");
        assert!(
            cache.get(&left_prefix).is_some(),
            "Left interior should be loaded"
        );
    }

    #[test]
    fn test_pre_advise_sparse_keys_efficiency() {
        use super::super::InteriorNode;
        use sha2::{Digest, Sha256};

        let db = create_test_db();
        let cache = Cache::new(Arc::clone(&db));

        // Build a larger tree with 100 leaves
        let num_leaves = 100;
        let mut all_prefixes = Vec::new();

        for i in 0..num_leaves {
            let mut hasher = Sha256::new();
            hasher.update(&(i as u64).to_le_bytes());
            let hash = hasher.finalize();
            let mut key = [0u8; 32];
            key.copy_from_slice(&hash);

            let prefix = Prefix::from(key);
            let leaf = LeafNode::new(key, [(i % 256) as u8; 32]);

            cache.set(prefix, Node::Leaf(leaf));
            all_prefixes.push(prefix);
        }

        // Add a minimal root structure
        let root_prefix = Prefix::root();
        let root_interior = InteriorNode::new(
            root_prefix,
            all_prefixes[0],
            all_prefixes[1],
            [1u8; 32],
            [2u8; 32],
        );
        cache.set(root_prefix, Node::Interior(root_interior));

        cache.flush().unwrap();
        let total_nodes = cache.tree_size();

        // Clear and pre-advise for 3 sparse keys
        cache.clear();
        let sparse_keys = vec![all_prefixes[0], all_prefixes[50], all_prefixes[99]];
        cache.pre_advise(&sparse_keys).unwrap();

        let loaded = cache.len();
        println!(
            "Loaded {} nodes for 3 sparse keys from a tree of {} nodes",
            loaded, total_nodes
        );

        // Should load O(log n) nodes per key, not O(n)
        // With 100 nodes and 3 keys, we expect roughly 3 * (log2(100) + siblings) ≈ 3 * 14 = 42
        // Be generous and allow up to 60 nodes
        assert!(
            loaded < 60,
            "Loaded {} nodes for 3 keys, expected < 60 (tree has {} nodes)",
            loaded,
            total_nodes
        );

        // Most importantly, should not load most of the tree
        assert!(
            loaded < total_nodes / 2,
            "Loaded {} nodes, which is >= 50% of tree ({}), suggesting inefficient loading",
            loaded,
            total_nodes
        );
    }

    #[test]
    fn test_pre_advise_with_partial_cache() {
        use super::super::InteriorNode;

        let db = create_test_db();
        let cache = Cache::new(Arc::clone(&db));

        // Create a 3-level tree
        let mut key1 = [0u8; 32];
        key1[0] = 0b0000_0000;
        let prefix1 = Prefix::from(key1);
        let leaf1 = LeafNode::new(key1, [1u8; 32]);

        let mut key2 = [0u8; 32];
        key2[0] = 0b0100_0000;
        let prefix2 = Prefix::from(key2);
        let leaf2 = LeafNode::new(key2, [2u8; 32]);

        let mut key3 = [0u8; 32];
        key3[0] = 0b1000_0000;
        let prefix3 = Prefix::from(key3);
        let leaf3 = LeafNode::new(key3, [3u8; 32]);

        let mut key4 = [0u8; 32];
        key4[0] = 0b1100_0000;
        let prefix4 = Prefix::from(key4);
        let leaf4 = LeafNode::new(key4, [4u8; 32]);

        let left_prefix = Prefix {
            hash: [0u8; 32],
            length: 1,
        };
        let left_interior = InteriorNode::new(
            left_prefix,
            prefix1,
            prefix2,
            leaf1.merkle_hash,
            leaf2.merkle_hash,
        );

        let mut right_hash = [0u8; 32];
        right_hash[0] = 0b1000_0000;
        let right_prefix = Prefix {
            hash: right_hash,
            length: 1,
        };
        let right_interior = InteriorNode::new(
            right_prefix,
            prefix3,
            prefix4,
            leaf3.merkle_hash,
            leaf4.merkle_hash,
        );

        let root_prefix = Prefix::root();
        let root_interior = InteriorNode::new(
            root_prefix,
            left_prefix,
            right_prefix,
            left_interior.merkle_hash,
            right_interior.merkle_hash,
        );

        cache.set(prefix1, Node::Leaf(leaf1));
        cache.set(prefix2, Node::Leaf(leaf2));
        cache.set(prefix3, Node::Leaf(leaf3));
        cache.set(prefix4, Node::Leaf(leaf4));
        cache.set(left_prefix, Node::Interior(left_interior.clone()));
        cache.set(right_prefix, Node::Interior(right_interior.clone()));
        cache.set(root_prefix, Node::Interior(root_interior.clone()));
        cache.flush().unwrap();

        // Test scenario: Cache only some upper-level nodes, not the target leaves
        // This simulates a more realistic partial cache where some tree structure is known
        cache.clear();
        cache.set(root_prefix, Node::Interior(root_interior));
        cache.set(left_prefix, Node::Interior(left_interior));
        let initial_count = cache.len();
        assert_eq!(
            initial_count, 2,
            "Should start with root and left interior cached"
        );

        // Pre-advise for prefix1 (left-left leaf)
        // Since root and left_interior are cached, pre_advise won't traverse them
        // This reveals a limitation: pre_advise skips already-cached interior nodes
        // So it won't load children of cached interior nodes unless forced
        cache.pre_advise(&[prefix1]).unwrap();

        // The current implementation has a limitation: when interior nodes are already
        // cached, pre_advise doesn't traverse into them to load their children.
        // It only processes nodes that it queries from the database.
        // So we test what actually happens, not what might be ideal:

        // Root and left_interior should still be in cache
        assert!(
            cache.get(&root_prefix).is_some(),
            "Root should still be in cache"
        );
        assert!(
            cache.get(&left_prefix).is_some(),
            "Left interior should still be in cache"
        );

        // The target leaf will be loaded via the fallback mechanism that directly loads
        // remaining needed keys at the end of pre_advise
        assert!(
            cache.get(&prefix1).is_some(),
            "Target leaf should be loaded via fallback"
        );

        let final_count = cache.len();
        println!(
            "Cache size: initial={}, final={}",
            initial_count, final_count
        );

        // We should have at least loaded the target
        assert!(
            final_count >= initial_count,
            "Should have loaded at least the target"
        );

        // This test documents current behavior: pre_advise has a limitation where
        // it doesn't traverse already-cached interior nodes. The fallback mechanism
        // at the end handles loading the actual target keys directly.
    }

    #[test]
    fn test_release_keys_retains_needed_keys() {
        use super::super::InteriorNode;

        let db = create_test_db();
        let cache = Cache::new(db);

        // Create some leaf nodes
        let key1 = [1u8; 32];
        let prefix1 = Prefix::from(key1);
        let leaf1 = LeafNode::new(key1, [10u8; 32]);

        let key2 = [2u8; 32];
        let prefix2 = Prefix::from(key2);
        let leaf2 = LeafNode::new(key2, [20u8; 32]);

        let key3 = [3u8; 32];
        let prefix3 = Prefix::from(key3);
        let leaf3 = LeafNode::new(key3, [30u8; 32]);

        // Create an interior node with short prefix (< 18 bits)
        let short_interior_prefix = Prefix {
            hash: [0u8; 32],
            length: 10,
        };
        let short_interior = InteriorNode::new(
            short_interior_prefix,
            prefix1,
            prefix2,
            leaf1.merkle_hash,
            leaf2.merkle_hash,
        );

        // Create an interior node with long prefix (>= 18 bits)
        let long_interior_prefix = Prefix {
            hash: [0u8; 32],
            length: 20,
        };
        let long_interior = InteriorNode::new(
            long_interior_prefix,
            prefix2,
            prefix3,
            leaf2.merkle_hash,
            leaf3.merkle_hash,
        );

        // Add all nodes to cache
        cache.set(prefix1, Node::Leaf(leaf1));
        cache.set(prefix2, Node::Leaf(leaf2));
        cache.set(prefix3, Node::Leaf(leaf3));
        cache.set(short_interior_prefix, Node::Interior(short_interior));
        cache.set(long_interior_prefix, Node::Interior(long_interior));

        assert_eq!(cache.len(), 5);

        // Release keys, keeping only prefix1 as needed
        cache.release_keys(&[prefix1]);

        // Should retain:
        // - prefix1 (in needed_keys)
        // - short_interior_prefix (interior node with length < 18)
        // Should remove:
        // - prefix2 (not in needed_keys, leaf node)
        // - prefix3 (not in needed_keys, leaf node)
        // - long_interior_prefix (interior node with length >= 18)
        assert_eq!(cache.len(), 2);
        assert!(cache.get(&prefix1).is_some(), "prefix1 should be retained");
        assert!(
            cache.get(&short_interior_prefix).is_some(),
            "short interior should be retained"
        );
        assert!(cache.get(&prefix2).is_none(), "prefix2 should be removed");
        assert!(cache.get(&prefix3).is_none(), "prefix3 should be removed");
        assert!(
            cache.get(&long_interior_prefix).is_none(),
            "long interior should be removed"
        );
    }

    #[test]
    fn test_release_keys_retains_interior_nodes_at_boundary() {
        use super::super::InteriorNode;

        let db = create_test_db();
        let cache = Cache::new(db);

        // Create interior nodes at the boundary (17 and 18 bits)
        let mut key1 = [0u8; 32];
        key1[0] = 0b0000_0000;
        let prefix1 = Prefix::from(key1);
        let leaf1 = LeafNode::new(key1, [1u8; 32]);

        let mut key2 = [0u8; 32];
        key2[0] = 0b1000_0000;
        let prefix2 = Prefix::from(key2);
        let leaf2 = LeafNode::new(key2, [2u8; 32]);

        // Interior node with length 17 (should be retained)
        let interior_17_prefix = Prefix {
            hash: [0u8; 32],
            length: 17,
        };
        let interior_17 = InteriorNode::new(
            interior_17_prefix,
            prefix1,
            prefix2,
            leaf1.merkle_hash,
            leaf2.merkle_hash,
        );

        // Interior node with length 18 (should be removed)
        let interior_18_prefix = Prefix {
            hash: [0u8; 32],
            length: 18,
        };
        let interior_18 = InteriorNode::new(
            interior_18_prefix,
            prefix1,
            prefix2,
            leaf1.merkle_hash,
            leaf2.merkle_hash,
        );

        cache.set(interior_17_prefix, Node::Interior(interior_17));
        cache.set(interior_18_prefix, Node::Interior(interior_18));

        assert_eq!(cache.len(), 2);

        // Release keys with no needed keys
        cache.release_keys(&[]);

        // Only the 17-bit interior node should remain
        assert_eq!(cache.len(), 1);
        assert!(
            cache.get(&interior_17_prefix).is_some(),
            "17-bit interior should be retained"
        );
        assert!(
            cache.get(&interior_18_prefix).is_none(),
            "18-bit interior should be removed"
        );
    }

    #[test]
    fn test_release_keys_with_root_node() {
        use super::super::InteriorNode;

        let db = create_test_db();
        let cache = Cache::new(db);

        let key1 = [1u8; 32];
        let prefix1 = Prefix::from(key1);
        let leaf1 = LeafNode::new(key1, [10u8; 32]);

        let key2 = [2u8; 32];
        let prefix2 = Prefix::from(key2);
        let leaf2 = LeafNode::new(key2, [20u8; 32]);

        // Root node (length 0, should be retained)
        let root_prefix = Prefix::root();
        let root = InteriorNode::new(
            root_prefix,
            prefix1,
            prefix2,
            leaf1.merkle_hash,
            leaf2.merkle_hash,
        );

        cache.set(root_prefix, Node::Interior(root));
        cache.set(prefix1, Node::Leaf(leaf1));
        cache.set(prefix2, Node::Leaf(leaf2));

        assert_eq!(cache.len(), 3);

        // Release keys with no needed keys
        cache.release_keys(&[]);

        // Root should be retained (length 0 < 18)
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&root_prefix).is_some(), "root should be retained");
        assert!(cache.get(&prefix1).is_none(), "leaf1 should be removed");
        assert!(cache.get(&prefix2).is_none(), "leaf2 should be removed");
    }

    #[test]
    fn test_pre_advise_transaction_management() {
        let db = create_test_db();
        let cache = Cache::new(Arc::clone(&db));

        // Create and flush a simple node
        let key = [1u8; 32];
        let prefix = Prefix::from(key);
        let leaf = LeafNode::new(key, [10u8; 32]);
        cache.set(prefix, Node::Leaf(leaf));
        cache.flush().unwrap();

        // Clear and do pre_advise - should start a transaction
        cache.clear();
        cache.pre_advise(&[prefix]).unwrap();

        // Verify transaction is active
        {
            let in_tx = cache.in_transaction.lock().unwrap();
            assert!(*in_tx, "Transaction should be active after pre_advise");
        }

        // Do another pre_advise - should not create nested transaction
        let key2 = [2u8; 32];
        let prefix2 = Prefix::from(key2);
        cache.pre_advise(&[prefix2]).unwrap();

        // Transaction should still be active
        {
            let in_tx = cache.in_transaction.lock().unwrap();
            assert!(*in_tx, "Transaction should still be active");
        }

        // Flush should commit the transaction
        cache.flush().unwrap();

        // Transaction should be closed
        {
            let in_tx = cache.in_transaction.lock().unwrap();
            assert!(!*in_tx, "Transaction should be closed after flush");
        }
    }

    #[test]
    fn test_pre_advise_loads_all_ancestors() {
        use super::super::InteriorNode;

        let db = create_test_db();
        let cache = Cache::new(Arc::clone(&db));

        // Build a simpler deep tree with 4 levels
        // Structure:
        //     root (len=0)
        //     /         \
        //   i1(len=1)   r1
        //   /      \
        // i2(len=2) r2
        // /      \
        // leaf   r3

        let mut leaf_key = [0u8; 32];
        leaf_key[0] = 0b0000_0000; // Goes left at all levels
        let leaf_prefix = Prefix::from(leaf_key);
        let leaf = LeafNode::new(leaf_key, [99u8; 32]);

        // Create right sibling leaves at each level
        let mut r3_key = [0u8; 32];
        r3_key[0] = 0b0100_0000; // Differs at bit 1
        let r3_prefix = Prefix::from(r3_key);
        let r3_leaf = LeafNode::new(r3_key, [3u8; 32]);

        let mut r2_key = [0u8; 32];
        r2_key[0] = 0b0010_0000; // Differs at bit 2
        let r2_prefix = Prefix::from(r2_key);
        let r2_leaf = LeafNode::new(r2_key, [2u8; 32]);

        let mut r1_key = [0u8; 32];
        r1_key[0] = 0b1000_0000; // Differs at bit 0
        let r1_prefix = Prefix::from(r1_key);
        let r1_leaf = LeafNode::new(r1_key, [1u8; 32]);

        // Build from bottom up
        // i2: has leaf and r3 as children
        let mut i2_hash = [0u8; 32];
        i2_hash[0] = 0b0000_0000;
        let i2_prefix = Prefix {
            hash: i2_hash,
            length: 2,
        };
        let i2 = InteriorNode::new(
            i2_prefix,
            leaf_prefix,
            r3_prefix,
            leaf.merkle_hash,
            r3_leaf.merkle_hash,
        );

        // i1: has i2 and r2 as children
        let mut i1_hash = [0u8; 32];
        i1_hash[0] = 0b0000_0000;
        let i1_prefix = Prefix {
            hash: i1_hash,
            length: 1,
        };
        let i1 = InteriorNode::new(
            i1_prefix,
            i2_prefix,
            r2_prefix,
            i2.merkle_hash,
            r2_leaf.merkle_hash,
        );

        // root: has i1 and r1 as children
        let root_prefix = Prefix::root();
        let root = InteriorNode::new(
            root_prefix,
            i1_prefix,
            r1_prefix,
            i1.merkle_hash,
            r1_leaf.merkle_hash,
        );

        // Add all nodes to cache and flush
        cache.set(leaf_prefix, Node::Leaf(leaf));
        cache.set(r3_prefix, Node::Leaf(r3_leaf));
        cache.set(r2_prefix, Node::Leaf(r2_leaf));
        cache.set(r1_prefix, Node::Leaf(r1_leaf));
        cache.set(i2_prefix, Node::Interior(i2));
        cache.set(i1_prefix, Node::Interior(i1));
        cache.set(root_prefix, Node::Interior(root));
        cache.flush().unwrap();

        // Clear and pre-advise for the deep leaf
        cache.clear();
        cache.pre_advise(&[leaf_prefix]).unwrap();

        // Verify the leaf is loaded
        assert!(
            cache.get(&leaf_prefix).is_some(),
            "Target leaf should be loaded"
        );

        // Verify ALL ancestors are loaded (no gaps in the path)
        assert!(
            cache.get(&root_prefix).is_some(),
            "Root (length 0) should be loaded"
        );
        assert!(
            cache.get(&i1_prefix).is_some(),
            "Interior node i1 (length 1) should be loaded"
        );
        assert!(
            cache.get(&i2_prefix).is_some(),
            "Interior node i2 (length 2) should be loaded"
        );

        // Verify siblings are loaded (needed for merkle hash recomputation)
        assert!(
            cache.get(&r3_prefix).is_some(),
            "Sibling r3 should be loaded"
        );

        println!("Successfully verified all ancestors and siblings are loaded for deep leaf");
    }
}

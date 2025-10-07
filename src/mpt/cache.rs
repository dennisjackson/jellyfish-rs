use dashmap::{DashMap, DashSet};
use rusqlite::{Connection, Result as SqliteResult, params};
use std::sync::{Arc, Mutex};

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

    /// Pre-advise: Load all nodes from SQLite into the cache and begin a durable transaction.
    /// This ignores the keys parameter and loads all nodes from the database.
    /// Nodes that are already in the cache will not be reloaded.
    /// The transaction will remain active until flush() is called.
    pub fn pre_advise(&self, _keys: &[Prefix]) -> SqliteResult<()> {
        let db = self.db.lock().unwrap();

        // Begin a durable transaction
        let mut in_tx = self.in_transaction.lock().unwrap();
        if !*in_tx {
            db.execute("BEGIN DEFERRED TRANSACTION", [])?;
            *in_tx = true;
        }
        drop(in_tx); // Release lock before querying

        let mut stmt =
            db.prepare("SELECT prefix_hash, prefix_length, node_type, node_data FROM nodes")?;

        let nodes = stmt.query_map([], |row| {
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
        })?;

        for result in nodes {
            let (prefix, node) = result?;
            // Skip if already in cache
            if !self.map.contains_key(&prefix) {
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
}

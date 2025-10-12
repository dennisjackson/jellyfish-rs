use dashmap::{DashMap, DashSet, mapref::one::Ref as DashMapRef};
use log::{debug, info, warn};
use rusqlite::{
    Connection, OptionalExtension, Result as SqliteResult,
    limits::Limit,
    params, params_from_iter,
    types::{ToSqlOutput, Type, ValueRef},
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use super::Node;
use crate::{Prefix, prefix::HashExt};

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
    /// Cached tree size tracked to avoid repeated COUNT queries
    tree_size_cache: AtomicUsize,
    /// Indicates whether the cached tree size is currently valid
    tree_size_known: AtomicBool,
    /// Tracks nodes that have been inserted but not yet persisted
    new_nodes: Arc<DashSet<Prefix>>,
    /// Maximum memory budget for cached entries, in bytes
    cache_memory_limit_bytes: usize,
}

const ROOT_METADATA_KEY: &str = "root_prefix";

pub(crate) const DEFAULT_CACHE_MEMORY_LIMIT_BYTES: usize = 1024 * 1024 * 1024; //10 MB

const CACHE_ENTRY_SIZE_BYTES: usize = std::mem::size_of::<Prefix>() + std::mem::size_of::<Node>();

pub type NodeReadGuard<'a> = DashMapRef<'a, Prefix, Node>;

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
        Self::new_with_limit(db, DEFAULT_CACHE_MEMORY_LIMIT_BYTES)
    }

    /// Create a new cache with a specific in-memory limit.
    pub fn new_with_limit(db: Arc<Mutex<Connection>>, cache_memory_limit_bytes: usize) -> Self {
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
            tree_size_cache: AtomicUsize::new(0),
            tree_size_known: AtomicBool::new(false),
            new_nodes: Arc::new(DashSet::new()),
            cache_memory_limit_bytes,
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
    pub fn get(&self, key: &Prefix) -> Option<NodeReadGuard<'_>> {
        self.map.get(key)
    }

    /// Retrieve a node, loading it from the database if necessary.
    pub fn get_or_load(&self, key: Prefix) -> SqliteResult<Option<NodeReadGuard<'_>>> {
        if let Some(node) = self.map.get(&key) {
            return Ok(Some(node));
        }

        let mut nodes = {
            let db = self.db.lock().unwrap();
            self.batch_query_nodes(&db, &[key])?
        };

        if let Some((_, node)) = nodes.pop() {
            self.map.insert(key, node);
            Ok(self.map.get(&key))
        } else {
            Ok(None)
        }
    }

    /// Insert or update a node in the cache.
    /// Automatically marks the key as dirty for later flushing.
    pub fn set(&self, key: Prefix, value: Node) {
        let previous = self.map.insert(key, value);
        self.dirty.insert(key);

        if previous.is_none() {
            self.new_nodes.insert(key);
            if self.tree_size_known.load(Ordering::SeqCst) {
                self.tree_size_cache.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    /// Get the number of entries currently in the cache.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Check if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn cache_memory_limit_bytes(&self) -> usize {
        self.cache_memory_limit_bytes
    }

    pub fn cache_entry_size_bytes(&self) -> usize {
        CACHE_ENTRY_SIZE_BYTES
    }

    fn update_tree_size_cache(&self, size: usize) {
        self.tree_size_cache.store(size, Ordering::SeqCst);
        self.tree_size_known.store(true, Ordering::SeqCst);
    }

    fn refresh_tree_size_with_conn(&self, conn: &Connection) -> SqliteResult<usize> {
        let base = Self::query_tree_size(conn)?;
        let unflushed = self.new_nodes.len();
        let total = base.saturating_add(unflushed);
        self.update_tree_size_cache(total);
        Ok(total)
    }

    fn query_tree_size(conn: &Connection) -> SqliteResult<usize> {
        conn.query_row("SELECT COUNT(*) FROM nodes", [], |row| {
            row.get::<_, i64>(0).map(|v| v as usize)
        })
    }

    /// Get the current tree size (total number of nodes in the database).
    pub fn tree_size(&self) -> usize {
        if self.tree_size_known.load(Ordering::SeqCst) {
            return self.tree_size_cache.load(Ordering::SeqCst);
        }

        let result = {
            let db = self.db.lock().unwrap();
            self.refresh_tree_size_with_conn(&db)
        };

        match result {
            Ok(size) => size,
            Err(err) => {
                warn!("Failed to load tree size from database: {}", err);
                0
            }
        }
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
        if CACHE_ENTRY_SIZE_BYTES == 0 || self.cache_memory_limit_bytes == 0 {
            return;
        }

        let mut current_usage = self.map.len().saturating_mul(CACHE_ENTRY_SIZE_BYTES);
        if current_usage <= self.cache_memory_limit_bytes {
            return;
        }

        let mut protected: HashSet<Prefix> =
            HashSet::with_capacity(needed_keys.len().saturating_mul(8) + 16);
        protected.insert(self.get_root());

        for key in needed_keys.iter().copied() {
            let mut current = key;
            loop {
                protected.insert(current);
                if current.length == 0 {
                    break;
                }
                let parent_length = current.length - 1;
                let parent_hash = current.hash.zero_bits_from(parent_length);
                current = Prefix {
                    hash: parent_hash,
                    length: parent_length,
                };
            }
        }

        let mut candidates: Vec<(Prefix, u16)> = Vec::new();
        {
            for entry in self.map.iter() {
                let key = *entry.key();
                if protected.contains(&key) {
                    continue;
                }
                candidates.push((key, key.length));
            }
        }

        if candidates.is_empty() {
            return;
        }

        candidates.sort_by(|a, b| match b.1.cmp(&a.1) {
            std::cmp::Ordering::Equal => b.0.cmp(&a.0),
            other => other,
        });

        let mut ints = 0;
        let mut leaves = 0;
        for (key, _) in candidates {
            if current_usage <= self.cache_memory_limit_bytes {
                break;
            }
            if let Some(n) = self.map.remove(&key) {
                debug!("Released key {:?} from cache", key.short_hex());
                current_usage = current_usage.saturating_sub(CACHE_ENTRY_SIZE_BYTES);
                if let Node::Interior(_) = n.1 {
                    ints += 1;
                } else {
                    leaves += 1;
                }
            }
        }

        if current_usage > self.cache_memory_limit_bytes {
            warn!(
                "release_keys: cache remains above limit (usage={} bytes, limit={} bytes)",
                current_usage, self.cache_memory_limit_bytes
            );
        }
        info!("Released {} internal nodes and {} leaf nodes", ints, leaves);
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

        const PARAMS_PER_PREFIX: usize = 2;
        let max_variables = db.limit(Limit::SQLITE_LIMIT_VARIABLE_NUMBER);
        let chunk_size = if max_variables <= 0 {
            1
        } else {
            std::cmp::max(1, (max_variables as usize) / PARAMS_PER_PREFIX)
        };

        let mut results: Vec<(Prefix, Node)> = Vec::with_capacity(prefixes.len());
        let mut query_buffer = String::new();
        let mut param_buffer = Vec::with_capacity(chunk_size * PARAMS_PER_PREFIX);

        for chunk in prefixes.chunks(chunk_size) {
            build_batch_query(&mut query_buffer, chunk.len());
            param_buffer.clear();
            for prefix in chunk {
                param_buffer.push(BatchQueryParam::Hash(prefix.hash.as_ref()));
                param_buffer.push(BatchQueryParam::Length(prefix.length as i64));
            }

            let mut stmt = db.prepare_cached(query_buffer.as_str())?;

            let nodes = stmt
                .query_map(params_from_iter(param_buffer.iter().copied()), |row| {
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

            results.extend(nodes);
        }

        Ok(results)
    }

    /// Pre-advise: Load nodes from SQLite that are needed for the given keys.
    /// This loads all nodes on the path from root to the needed keys, plus their siblings.
    /// Nodes that are already in the cache will not be reloaded.
    /// Begins a durable transaction that will remain active until flush() is called.
    pub fn pre_advise(&self, _keys: &[Prefix]) -> SqliteResult<()> {
        let db = self.db.lock().unwrap();
        let mut queried_nodes = 0;

        // Begin a durable transaction
        let mut in_tx = self.in_transaction.lock().unwrap();
        if !*in_tx {
            db.execute("BEGIN DEFERRED TRANSACTION", [])?;
            *in_tx = true;
        }
        drop(in_tx); // Release lock before querying

        //Currently pretty inefficient. O(n^2) so we limit the max we process at once
        for (chunk_idx, keys_chunk) in _keys.chunks(100).enumerate() {
            let mut needed_keys: HashSet<Prefix> = keys_chunk.iter().cloned().collect();
            if needed_keys.is_empty() {
                continue;
            }

            let mut current_frontier = HashSet::new();
            current_frontier.insert(self.get_root());
            debug!(
                "Pre-advise chunk {} for {} keys, starting from root",
                chunk_idx + 1,
                needed_keys.len()
            );
            for x in needed_keys.iter() {
                debug!("  Needed key: {:?}", x.short_hex());
            }
            while !needed_keys.is_empty() && !current_frontier.is_empty() {
                debug!(
                    "Iterating chunk {}. Need {} keys, frontier size {}",
                    chunk_idx + 1,
                    needed_keys.len(),
                    current_frontier.len()
                );
                for x in &current_frontier {
                    debug!("  Frontier node: {:?}", x.short_hex());
                }
                let mut frontier_nodes: Vec<Node> = Vec::new();

                // Separate frontier into cached and uncached nodes
                let (cached_prefixes, prefixes_to_query): (Vec<Prefix>, Vec<Prefix>) =
                    current_frontier
                        .iter()
                        .partition(|p| self.map.contains_key(p));
                debug!(
                    "Frontier has {} cached, {} to query",
                    cached_prefixes.len(),
                    prefixes_to_query.len()
                );

                for p in cached_prefixes {
                    if let Some(node) = self.get(&p) {
                        frontier_nodes.push(node.value().clone());
                    }
                }

                let new_nodes: Vec<(Prefix, Node)> = self
                    .batch_query_nodes(&db, &prefixes_to_query)?
                    .into_iter()
                    .collect();
                queried_nodes += new_nodes.len();
                for (p, n) in new_nodes.iter() {
                    self.map.insert(*p, n.clone());
                }

                // At this point, all frontier nodes are in the cache.
                frontier_nodes.extend(new_nodes.iter().map(|(_, n)| n.clone()));
                current_frontier.clear();
                for x in frontier_nodes.iter() {
                    debug!("  Loaded frontier node: {}", x);
                }
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
                            } else if !self.map.contains_key(&interior.left) {
                                siblings_to_load.push(interior.left);
                            }
                            if interior.right.prefix_of(nk) {
                                current_frontier.insert(interior.right);
                            } else if !self.map.contains_key(&interior.right) {
                                siblings_to_load.push(interior.right);
                            }
                        }
                    }
                }

                debug!(
                    "Identified {} siblings to load and {} next frontier nodes for {} needed keys",
                    siblings_to_load.len(),
                    current_frontier.len(),
                    needed_keys.len()
                );

                let siblings = self.batch_query_nodes(&db, &siblings_to_load)?;
                queried_nodes += siblings.len();
                // Insert all queried nodes into cache
                for (prefix, node) in siblings {
                    self.map.insert(prefix, node);
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
                let remaining_nodes = self.batch_query_nodes(&db, &remaining_to_query)?;
                queried_nodes += remaining_nodes.len();
                for (prefix, node) in remaining_nodes {
                    self.map.insert(prefix, node);
                }
            }
        }

        info!(
            "Pre-advise complete. Loaded nodes: {}, Cache size: {}",
            queried_nodes,
            self.len()
        );
        Ok(())
    }

    /// Flush all dirty (modified) keys to the database.
    /// If a transaction was started by pre_advise, it will be committed.
    /// Otherwise, creates a new transaction for this flush operation.
    /// After successful flush, clears the dirty tracking.
    pub fn flush(&self) -> SqliteResult<()> {
        let dirty_keys: Vec<Prefix> = self.dirty.iter().map(|entry| *entry).collect();
        let root_dirty = self.root_dirty.load(Ordering::SeqCst);
        info!("Flushing {} dirty nodes", dirty_keys.len());
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
                if !*in_tx {
                    db.execute("BEGIN DEFERRED TRANSACTION", [])?;
                    *in_tx = true;
                }

                debug!(
                    "Flushing {} dirty nodes to database in a single transaction",
                    dirty_keys.len()
                );

                let mut newly_persisted: Vec<Prefix> = Vec::new();

                {
                    let mut stmt = db.prepare_cached(
                        "INSERT OR REPLACE INTO nodes (prefix_hash, prefix_length, node_type, node_data) VALUES (?1, ?2, ?3, ?4)"
                    )?;

                    for key in &dirty_keys {
                        if let Some(node) = self.map.get(key) {
                            debug!("Flushing dirty key {:?}", key.short_hex());
                            let (node_type, node_data) = node
                                .value()
                                .serialize()
                                .map_err(|_| rusqlite::Error::InvalidQuery)?;

                            stmt.execute(params![&key.hash[..], key.length, node_type, node_data])?;
                            if self.new_nodes.contains(key) {
                                newly_persisted.push(*key);
                            }
                        } else {
                            log::warn!(
                                "Warning: Dirty key {:?} not found in cache during flush",
                                key
                            );
                        }
                    }
                }

                if root_dirty {
                    self.persist_root_metadata(&db)?;
                    cleared_root_dirty = true;
                }

                db.execute("COMMIT", [])?;
                *in_tx = false;
                for key in newly_persisted {
                    self.new_nodes.remove(&key);
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

#[derive(Clone, Copy)]
enum BatchQueryParam<'a> {
    Hash(&'a [u8]),
    Length(i64),
}

impl<'a> rusqlite::ToSql for BatchQueryParam<'a> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self {
            BatchQueryParam::Hash(bytes) => ToSqlOutput::Borrowed(ValueRef::Blob(bytes)),
            BatchQueryParam::Length(len) => ToSqlOutput::Borrowed(ValueRef::Integer(*len)),
        })
    }
}

fn build_batch_query(buffer: &mut String, pair_count: usize) {
    debug_assert!(pair_count > 0);
    const SELECT_PREFIX: &str = "SELECT prefix_hash, prefix_length, node_type, node_data FROM nodes \
         WHERE (prefix_hash, prefix_length) IN (";
    const PAIR_PLACEHOLDER: &str = "(?, ?)";

    buffer.clear();
    buffer.reserve(
        SELECT_PREFIX.len()
            + pair_count.saturating_mul(PAIR_PLACEHOLDER.len())
            + pair_count.saturating_sub(1)
            + 1,
    );
    buffer.push_str(SELECT_PREFIX);
    for index in 0..pair_count {
        if index > 0 {
            buffer.push(',');
        }
        buffer.push_str(PAIR_PLACEHOLDER);
    }
    buffer.push(')');
}

#[cfg(test)]
mod tests;

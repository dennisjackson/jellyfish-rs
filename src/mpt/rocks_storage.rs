use std::{fmt, io};
use std::path::Path;

use rocksdb::{
    DBIteratorWithThreadMode, IteratorMode, OptimisticTransactionDB, Options, Transaction,
};

use crate::Prefix;

use super::sled_storage::{
    decode_node, decode_prefix, encode_node, encode_prefix, prefix_key, ROOT_KEY,
};
use super::Node;

pub type RocksResult<T> = Result<T, RocksStorageError>;

#[derive(Debug)]
pub enum RocksStorageError {
    Db(rocksdb::Error),
    Codec(String),
    Io(io::Error),
    TransactionClosed,
}

impl fmt::Display for RocksStorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RocksStorageError::Db(err) => write!(f, "rocksdb error: {}", err),
            RocksStorageError::Codec(err) => write!(f, "serialization error: {}", err),
            RocksStorageError::Io(err) => write!(f, "io error: {}", err),
            RocksStorageError::TransactionClosed => write!(f, "transaction already finished"),
        }
    }
}

impl std::error::Error for RocksStorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RocksStorageError::Db(err) => Some(err),
            RocksStorageError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<rocksdb::Error> for RocksStorageError {
    fn from(err: rocksdb::Error) -> Self {
        RocksStorageError::Db(err)
    }
}

impl From<String> for RocksStorageError {
    fn from(err: String) -> Self {
        RocksStorageError::Codec(err)
    }
}

impl From<io::Error> for RocksStorageError {
    fn from(err: io::Error) -> Self {
        RocksStorageError::Io(err)
    }
}

pub struct RocksStorage {
    db: OptimisticTransactionDB,
}

impl RocksStorage {
    pub fn open(path: impl AsRef<Path>) -> RocksResult<Self> {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.set_max_open_files(512);
        // Enable prefix bloom filter for efficient prefix scans
        // The prefix extractor extracts the first 2 bytes (the length field)
        options.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(2));
        let db = OptimisticTransactionDB::open(&options, path)?;
        Ok(Self { db })
    }

    pub fn start_transaction(&self) -> RocksTransaction<'_> {
        RocksTransaction {
            tx: Some(self.db.transaction()),
        }
    }

    pub fn approximate_entry_count(&self) -> usize {
        self.db
            .property_int_value("rocksdb.estimate-num-keys")
            .ok()
            .flatten()
            .unwrap_or(0) as usize
    }

    pub fn enumerate_nodes(&self) -> RocksResult<Vec<(Prefix, Node)>> {
        self.iter_nodes().collect()
    }

    pub fn iter_nodes(&self) -> RocksNodeIter<'_> {
        RocksNodeIter {
            iter: self.db.iterator(IteratorMode::Start),
        }
    }

    /// Get all leaf nodes (length 256) that match the specified prefix.
    /// This uses RocksDB's prefix iterator for efficient querying.
    ///
    /// Takes an arbitrary prefix (e.g., length 4 with specific bits) and finds
    /// all leaf nodes (length 256) whose hashes start with that prefix.
    ///
    /// For example, if prefix has length 4 with bits "1010", this will return
    /// all leaf nodes whose 256-bit hashes start with "1010".
    pub fn get_leaf_nodes_by_prefix(&self, prefix: &Prefix) -> RocksResult<Vec<(Prefix, Node)>> {
        let mut results = Vec::new();

        // Construct the key prefix: <length=256 (2 bytes)> || <hash prefix>
        // We want to match all keys that start with length 256 and the given hash prefix
        let mut key_prefix = Vec::with_capacity(2 + 32);
        // Add length field for leaf nodes (256)
        key_prefix.extend_from_slice(&256u16.to_be_bytes());
        // Add the hash prefix (only the relevant bytes based on prefix.length)
        let num_bytes = ((prefix.length + 7) / 8) as usize; // Round up to nearest byte
        key_prefix.extend_from_slice(&prefix.hash[..num_bytes]);

        // Use RocksDB's prefix iterator with the constructed key
        let iter = self.db.prefix_iterator(&key_prefix);

        for entry in iter {
            match entry {
                Ok((key, value)) => {
                    // Decode the prefix from the key
                    let node_prefix = match decode_prefix(key.as_ref()) {
                        Ok(prefix) => prefix,
                        Err(err) => return Err(err.into()),
                    };

                    // Double-check that this is a leaf node (length 256) and matches our prefix
                    if node_prefix.length == 256 && prefix.prefix_of(&node_prefix) {
                        // Decode the node
                        let node = decode_node(value.as_ref())?;
                        results.push((node_prefix, node));
                    }
                }
                Err(err) => return Err(err.into()),
            }
        }

        Ok(results)
    }

    /// Get all nodes at a specific prefix length.
    /// This is efficient because keys are encoded as <length> || <hash>,
    /// so we can use RocksDB's prefix iterator on the length bytes.
    pub fn get_nodes_by_prefix_length(&self, length: u16) -> RocksResult<Vec<(Prefix, Node)>> {
        let mut results = Vec::new();

        // Create the prefix key for this length (just the 2 length bytes)
        let length_prefix = length.to_be_bytes();

        // Use prefix iterator - this is optimized by RocksDB with bloom filters
        let iter = self.db.prefix_iterator(&length_prefix);

        for entry in iter {
            match entry {

                Ok((key, value)) => {
                    // Decode the prefix from the key
                    let prefix = match decode_prefix(key.as_ref()) {
                        Ok(prefix) => prefix,
                        Err(err) => return Err(err.into()),
                    };


                    // Decode the node
                    let node = decode_node(value.as_ref())?;
                    results.push((prefix, node));
                }
                Err(err) => return Err(err.into()),
            }
        }

        Ok(results)
    }

    pub fn flush(&self) -> RocksResult<()> {
        self.db.flush()?;
        Ok(())
    }
}

pub struct RocksTransaction<'a> {
    tx: Option<Transaction<'a, OptimisticTransactionDB>>,
}

impl<'a> RocksTransaction<'a> {
    fn inner(&self) -> RocksResult<&Transaction<'a, OptimisticTransactionDB>> {
        self.tx.as_ref().ok_or(RocksStorageError::TransactionClosed)
    }

    pub fn load_root(&self) -> RocksResult<Prefix> {
        let tx = self.inner()?;
        match tx.get(ROOT_KEY)? {
            Some(raw) => decode_prefix(raw.as_ref()).map_err(Into::into),
            None => {
                let root = Prefix::root();
                tx.put(ROOT_KEY, encode_prefix(root))?;
                Ok(root)
            }
        }
    }

    pub fn batch_read_nodes(&self, prefixes: &[Prefix]) -> RocksResult<Vec<Option<Node>>> {
        if prefixes.is_empty() {
            return Ok(Vec::new());
        }

        let keys: Vec<Vec<u8>> = prefixes.iter().map(prefix_key).collect();
        let tx = self.inner()?;
        let mut decoded = Vec::with_capacity(prefixes.len());
        for entry in tx.multi_get(keys) {
            match entry {
                Ok(Some(raw)) => decoded.push(Some(decode_node(raw.as_ref())?)),
                Ok(None) => decoded.push(None),
                Err(err) => return Err(err.into()),
            }
        }
        Ok(decoded)
    }

    pub fn batch_write_nodes(&self, entries: &[(Prefix, Node)]) -> RocksResult<()> {
        let tx = self.inner()?;
        for (prefix, node) in entries.iter() {
            let key = prefix_key(prefix);
            let value = encode_node(node);
            tx.put(key, value)?;
        }
        Ok(())
    }

    pub fn set_root(&self, root: Prefix) -> RocksResult<()> {
        self.inner()?.put(ROOT_KEY, encode_prefix(root))?;
        Ok(())
    }

    pub fn commit(mut self) -> RocksResult<()> {
        if let Some(tx) = self.tx.take() {
            tx.commit()?;
        }
        Ok(())
    }

    pub fn rollback(mut self) -> RocksResult<()> {
        if let Some(tx) = self.tx.take() {
            tx.rollback()?;
        }
        Ok(())
    }
}

impl<'a> Drop for RocksTransaction<'a> {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.rollback();
        }
    }
}

pub struct RocksNodeIter<'a> {
    iter: DBIteratorWithThreadMode<'a, OptimisticTransactionDB>,
}

impl<'a> Iterator for RocksNodeIter<'a> {
    type Item = RocksResult<(Prefix, Node)>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(entry) = self.iter.next() {
            match entry {
                Ok((key, value)) => {
                    if key.as_ref() == ROOT_KEY {
                        continue;
                    }
                    let prefix = match decode_prefix(key.as_ref()) {
                        Ok(prefix) => prefix,
                        Err(err) => return Some(Err(err.into())),
                    };
                    let node = match decode_node(value.as_ref()) {
                        Ok(node) => node,
                        Err(err) => return Some(Err(err.into())),
                    };
                    return Some(Ok((prefix, node)));
                }
                Err(err) => return Some(Err(err.into())),
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpt::LeafNode;
    use tempfile::TempDir;

    fn build_leaf(key_byte: u8, value_byte: u8) -> (Prefix, Node) {
        let mut key = [0u8; 32];
        key[0] = key_byte;
        let mut value = [0u8; 32];
        value[0] = value_byte;
        let leaf = LeafNode::new(key, value);
        (Prefix::from(key), Node::Leaf(leaf))
    }

    #[test]
    fn transaction_flow_round_trip() {
        let dir = TempDir::new().expect("temp dir");
        let storage = RocksStorage::open(dir.path()).expect("open rocksdb");
        let tx = storage.start_transaction();

        let root = tx.load_root().expect("load root");
        assert_eq!(root, Prefix::root());

        let entries = vec![build_leaf(1, 11), build_leaf(2, 22)];
        tx.batch_write_nodes(&entries).expect("write batch");
        tx.set_root(entries[0].0).expect("set root");
        tx.commit().expect("commit");

        let tx = storage.start_transaction();
        let fetched = tx
            .batch_read_nodes(&[entries[0].0, entries[1].0])
            .expect("batch read");
        assert!(matches!(fetched[0], Some(Node::Leaf(_))));
        assert!(matches!(fetched[1], Some(Node::Leaf(_))));
        tx.rollback().expect("rollback");
    }

    #[test]
    fn test_get_nodes_by_prefix_empty_tree() {
        let dir = TempDir::new().expect("temp dir");
        let storage = RocksStorage::open(dir.path()).expect("open rocksdb");

        // Query on empty tree should return empty
        let root = Prefix::root();
        let results = storage.get_leaf_nodes_by_prefix(&root).expect("query by prefix");
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_get_nodes_by_prefix_root() {
        let dir = TempDir::new().expect("temp dir");
        let storage = RocksStorage::open(dir.path()).expect("open rocksdb");

        // Insert some nodes
        let entries = vec![
            build_leaf(0x00, 1),
            build_leaf(0x80, 2),
            build_leaf(0xFF, 3),
        ];

        let tx = storage.start_transaction();
        tx.batch_write_nodes(&entries).expect("write batch");
        tx.commit().expect("commit");

        // Root prefix (length 0) should match all leaf nodes
        let root = Prefix::root();
        let results = storage.get_leaf_nodes_by_prefix(&root).expect("query by prefix");
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_get_nodes_by_prefix_specific_prefix() {
        let dir = TempDir::new().expect("temp dir");
        let storage = RocksStorage::open(dir.path()).expect("open rocksdb");

        // Create nodes with different first bits
        // 0x00 = 00000000... (first bit is 0)
        // 0x80 = 10000000... (first bit is 1)
        // 0xFF = 11111111... (first bit is 1)
        let entries = vec![
            build_leaf(0x00, 1),
            build_leaf(0x80, 2),
            build_leaf(0xFF, 3),
        ];

        let tx = storage.start_transaction();
        tx.batch_write_nodes(&entries).expect("write batch");
        tx.commit().expect("commit");

        // Create a prefix with length 1 and first bit = 1
        let mut hash = [0u8; 32];
        hash[0] = 0x80; // First bit is 1
        let prefix = Prefix { hash, length: 1 };

        // Should match the two leaf nodes starting with 1 (0x80 and 0xFF)
        let results = storage.get_leaf_nodes_by_prefix(&prefix).expect("query by prefix");
        assert_eq!(results.len(), 2);

        // Verify the results
        let keys: Vec<u8> = results.iter().map(|(p, _)| p.hash[0]).collect();
        assert!(keys.contains(&0x80));
        assert!(keys.contains(&0xFF));
        assert!(!keys.contains(&0x00));
    }

    #[test]
    fn test_get_nodes_by_prefix_no_matches() {
        let dir = TempDir::new().expect("temp dir");
        let storage = RocksStorage::open(dir.path()).expect("open rocksdb");

        // Insert nodes with first bit = 0
        let entries = vec![
            build_leaf(0x00, 1),
            build_leaf(0x01, 2),
        ];

        let tx = storage.start_transaction();
        tx.batch_write_nodes(&entries).expect("write batch");
        tx.commit().expect("commit");

        // Query for prefix with first bit = 1
        let mut hash = [0u8; 32];
        hash[0] = 0x80;
        let prefix = Prefix { hash, length: 1 };

        let results = storage.get_leaf_nodes_by_prefix(&prefix).expect("query by prefix");
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_get_nodes_by_prefix_exact_match() {
        let dir = TempDir::new().expect("temp dir");
        let storage = RocksStorage::open(dir.path()).expect("open rocksdb");

        // Insert some leaf nodes
        let entries = vec![
            build_leaf(0x00, 1),
            build_leaf(0x80, 2),
            build_leaf(0xFF, 3),
        ];

        let tx = storage.start_transaction();
        tx.batch_write_nodes(&entries).expect("write batch");
        tx.commit().expect("commit");

        // Query for exact full-length prefix (256 bits)
        let mut hash = [0u8; 32];
        hash[0] = 0x80;
        let prefix = Prefix::from(hash);

        // Should match exactly one node with this exact hash
        let results = storage.get_leaf_nodes_by_prefix(&prefix).expect("query by prefix");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.hash, hash);
    }

    #[test]
    fn test_get_nodes_by_prefix_partial_byte() {
        let dir = TempDir::new().expect("temp dir");
        let storage = RocksStorage::open(dir.path()).expect("open rocksdb");

        // Insert nodes with different bit patterns
        // 0xF0 = 11110000
        // 0xF8 = 11111000
        // 0xE0 = 11100000
        let entries = vec![
            build_leaf(0xF0, 1),
            build_leaf(0xF8, 2),
            build_leaf(0xE0, 3),
        ];

        let tx = storage.start_transaction();
        tx.batch_write_nodes(&entries).expect("write batch");
        tx.commit().expect("commit");

        // Query for prefix with first 4 bits = 1111
        let mut hash = [0u8; 32];
        hash[0] = 0xF0;
        let prefix = Prefix { hash, length: 4 };

        // Should match the two nodes starting with 1111 (0xF0 and 0xF8)
        let results = storage.get_leaf_nodes_by_prefix(&prefix).expect("query by prefix");
        assert_eq!(results.len(), 2);

        let keys: Vec<u8> = results.iter().map(|(p, _)| p.hash[0]).collect();
        assert!(keys.contains(&0xF0));
        assert!(keys.contains(&0xF8));
        assert!(!keys.contains(&0xE0));
    }

    #[test]
    fn test_get_nodes_by_prefix_length_empty_tree() {
        let dir = TempDir::new().expect("temp dir");
        let storage = RocksStorage::open(dir.path()).expect("open rocksdb");

        let results = storage.get_nodes_by_prefix_length(256).expect("query by length");
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_get_nodes_by_prefix_length_all_same_length() {
        let dir = TempDir::new().expect("temp dir");
        let storage = RocksStorage::open(dir.path()).expect("open rocksdb");

        // All leaf nodes have full-length prefixes (256 bits)
        let entries = vec![
            build_leaf(0x00, 1),
            build_leaf(0x01, 2),
            build_leaf(0xFF, 3),
        ];

        let tx = storage.start_transaction();
        tx.batch_write_nodes(&entries).expect("write batch");
        tx.commit().expect("commit");

        // Query for length 256
        let results = storage.get_nodes_by_prefix_length(256).expect("query by length");
        assert_eq!(results.len(), 3);

        // All results should have length 256
        for (prefix, _) in &results {
            assert_eq!(prefix.length, 256);
        }
    }

    #[test]
    fn test_get_nodes_by_prefix_length_mixed_lengths() {
        let dir = TempDir::new().expect("temp dir");
        let storage = RocksStorage::open(dir.path()).expect("open rocksdb");

        // Create nodes with different prefix lengths
        let mut hash1 = [0u8; 32];
        hash1[0] = 0x00;
        let prefix1 = Prefix { hash: hash1, length: 4 };

        let mut hash2 = [0u8; 32];
        hash2[0] = 0x10;
        let prefix2 = Prefix { hash: hash2, length: 4 };

        let mut hash3 = [0u8; 32];
        hash3[0] = 0x80;
        let prefix3 = Prefix { hash: hash3, length: 8 };

        // Full length nodes
        let (prefix4, node4) = build_leaf(0xFF, 1);

        let entries = vec![
            (prefix1, Node::Leaf(LeafNode::new([0u8; 32], [1u8; 32]))),
            (prefix2, Node::Leaf(LeafNode::new([1u8; 32], [2u8; 32]))),
            (prefix3, Node::Leaf(LeafNode::new([2u8; 32], [3u8; 32]))),
            (prefix4, node4),
        ];

        let tx = storage.start_transaction();
        tx.batch_write_nodes(&entries).expect("write batch");
        tx.commit().expect("commit");

        // Query for length 4
        let results_4 = storage.get_nodes_by_prefix_length(4).expect("query by length 4");
        assert_eq!(results_4.len(), 2);
        for (prefix, _) in &results_4 {
            assert_eq!(prefix.length, 4);
        }

        // Query for length 8
        let results_8 = storage.get_nodes_by_prefix_length(8).expect("query by length 8");
        assert_eq!(results_8.len(), 1);
        assert_eq!(results_8[0].0.length, 8);

        // Query for length 256
        let results_256 = storage.get_nodes_by_prefix_length(256).expect("query by length 256");
        assert_eq!(results_256.len(), 1);
        assert_eq!(results_256[0].0.length, 256);

        // Query for length that doesn't exist
        let results_16 = storage.get_nodes_by_prefix_length(16).expect("query by length 16");
        assert_eq!(results_16.len(), 0);
    }

    #[test]
    fn test_get_nodes_by_prefix_length_ordering() {
        let dir = TempDir::new().expect("temp dir");
        let storage = RocksStorage::open(dir.path()).expect("open rocksdb");

        // Create multiple nodes with the same length but different hashes
        let mut entries = Vec::new();
        for i in 0..10u8 {
            let mut hash = [0u8; 32];
            hash[0] = i * 20; // Spread them out
            let prefix = Prefix { hash, length: 16 };
            let node = Node::Leaf(LeafNode::new(hash, [i; 32]));
            entries.push((prefix, node));
        }

        let tx = storage.start_transaction();
        tx.batch_write_nodes(&entries).expect("write batch");
        tx.commit().expect("commit");

        // Query for length 16
        let results = storage.get_nodes_by_prefix_length(16).expect("query by length 16");
        assert_eq!(results.len(), 10);

        // Verify all have length 16
        for (prefix, _) in &results {
            assert_eq!(prefix.length, 16);
        }

        // Results should be ordered by hash (since keys are <length> || <hash>)
        for i in 1..results.len() {
            assert!(results[i-1].0.hash <= results[i].0.hash);
        }
    }

    #[test]
    fn test_get_nodes_by_prefix_combined_with_length() {
        let dir = TempDir::new().expect("temp dir");
        let storage = RocksStorage::open(dir.path()).expect("open rocksdb");

        // Create nodes with various lengths and prefixes
        let mut entries = Vec::new();

        // Length 4, starting with 0000
        let mut hash1 = [0u8; 32];
        hash1[0] = 0x00;
        entries.push((Prefix { hash: hash1, length: 4 }, Node::Leaf(LeafNode::new(hash1, [1u8; 32]))));

        // Length 4, starting with 1111
        let mut hash2 = [0u8; 32];
        hash2[0] = 0xF0;
        entries.push((Prefix { hash: hash2, length: 4 }, Node::Leaf(LeafNode::new(hash2, [2u8; 32]))));

        // Length 8, starting with 0000
        let mut hash3 = [0u8; 32];
        hash3[0] = 0x00;
        entries.push((Prefix { hash: hash3, length: 8 }, Node::Leaf(LeafNode::new(hash3, [3u8; 32]))));

        let tx = storage.start_transaction();
        tx.batch_write_nodes(&entries).expect("write batch");
        tx.commit().expect("commit");

        // Get all length 4 nodes
        let length_4 = storage.get_nodes_by_prefix_length(4).expect("query by length");
        assert_eq!(length_4.len(), 2);

        // Among length 4 nodes, filter by prefix
        let mut prefix_filter = [0u8; 32];
        prefix_filter[0] = 0x00;
        let filter_prefix = Prefix { hash: prefix_filter, length: 1 };

        let matching: Vec<_> = length_4
            .iter()
            .filter(|(p, _)| filter_prefix.prefix_of(p))
            .collect();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].0.hash[0], 0x00);
    }
}


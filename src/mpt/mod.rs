use std::fmt;

use sha2::Digest;

use crate::{Hash, Prefix, prefix::HashExt};

mod simple;
pub use simple::SimpleMPT;

mod batch;
pub use batch::BatchMPT;

mod durable_batch;
pub use durable_batch::DurableBatchMPT;

mod rocks_storage;
pub use rocks_storage::RocksStorage;

mod rock_trans_rel;
pub use rock_trans_rel::RocksTransRelMPT;

mod cache;
pub use cache::Cache;

#[derive(Clone, Debug)]
pub struct LeafNode {
    pub value: Hash,
    pub merkle_hash: Hash,
}

impl LeafNode {
    pub fn new(key: Hash, value: Hash) -> Self {
        Self {
            value,
            merkle_hash: Self::calculate_hash(key, value),
        }
    }

    pub fn calculate_hash(key: Hash, value: Hash) -> Hash {
        let mut hasher = sha2::Sha256::new();
        hasher.update(b"leaf");
        hasher.update(key);
        hasher.update(value);
        hasher.finalize().into()
    }
}

impl fmt::Display for LeafNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Leaf(value={}, hash={})",
            self.value.short_hex(),
            self.merkle_hash.short_hex()
        )
    }
}

#[derive(Clone, Debug)]
pub struct InteriorNode {
    pub merkle_hash: Hash,
    pub left: Prefix,
    pub right: Prefix,
}

impl InteriorNode {
    pub fn new(
        prefix: Prefix,
        left: Prefix,
        right: Prefix,
        left_hash: Hash,
        right_hash: Hash,
    ) -> Self {
        Self {
            left,
            right,
            merkle_hash: Self::calculate_hash(prefix, left_hash, right_hash),
        }
    }

    pub fn calculate_hash(prefix: Prefix, left_hash: Hash, right_hash: Hash) -> Hash {
        let mut hasher = sha2::Sha256::new();
        hasher.update(b"interior");
        hasher.update(prefix.hash);
        hasher.update(prefix.length.to_be_bytes());
        hasher.update(left_hash);
        hasher.update(right_hash);
        hasher.finalize().into()
    }
}

impl fmt::Display for InteriorNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Interior(left={}, right={}, hash={})",
            self.left.short_hex(),
            self.right.short_hex(),
            self.merkle_hash.short_hex()
        )
    }
}

#[derive(Clone, Debug)]
pub enum Node {
    Leaf(LeafNode),
    Interior(InteriorNode),
}

impl Node {
    pub fn merkle_hash(&self) -> Hash {
        match self {
            Node::Leaf(leaf) => leaf.merkle_hash,
            Node::Interior(interior) => interior.merkle_hash,
        }
    }

    /// Serialize a node to its database representation.
    pub fn serialize(&self) -> Result<(&'static str, Vec<u8>), String> {
        match self {
            Node::Leaf(leaf) => {
                let data = bincode::encode_to_vec(
                    (leaf.value, leaf.merkle_hash),
                    bincode::config::standard(),
                )
                .map_err(|e| format!("Failed to encode leaf: {}", e))?;
                Ok(("leaf", data))
            }
            Node::Interior(interior) => {
                let data = bincode::encode_to_vec(
                    (interior.merkle_hash, interior.left, interior.right),
                    bincode::config::standard(),
                )
                .map_err(|e| format!("Failed to encode interior: {}", e))?;
                Ok(("interior", data))
            }
        }
    }

    /// Deserialize a node from its database representation.
    pub fn deserialize(node_type: &str, node_data: &[u8]) -> Result<Self, String> {
        match node_type {
            "leaf" => {
                let decoded: (Hash, Hash) =
                    bincode::decode_from_slice(node_data, bincode::config::standard())
                        .map(|(v, _)| v)
                        .map_err(|e| format!("Failed to decode leaf: {}", e))?;
                Ok(Node::Leaf(LeafNode {
                    value: decoded.0,
                    merkle_hash: decoded.1,
                }))
            }
            "interior" => {
                let decoded: (Hash, Prefix, Prefix) =
                    bincode::decode_from_slice(node_data, bincode::config::standard())
                        .map(|(v, _)| v)
                        .map_err(|e| format!("Failed to decode interior: {}", e))?;
                Ok(Node::Interior(InteriorNode {
                    merkle_hash: decoded.0,
                    left: decoded.1,
                    right: decoded.2,
                }))
            }
            _ => Err(format!("Unknown node type: {}", node_type)),
        }
    }
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Node::Leaf(leaf) => write!(f, "{leaf}"),
            Node::Interior(interior) => write!(f, "{interior}"),
        }
    }
}

/// Order two children based on whether `key` goes right at the split point.
/// Returns `(left_prefix, right_prefix, left_hash, right_hash)`.
pub fn order_children(
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

/// Sort entries by key and deduplicate, keeping the last value for each key.
pub fn sorted_unique_entries(entries: &[(Hash, Hash)]) -> Vec<(Hash, Hash)> {
    let mut sorted = entries.to_vec();
    sorted.sort_by_key(|(k, _)| *k);
    sorted.dedup_by_key(|(k, _)| *k);
    sorted
}

pub trait MerklePatriciaTree {
    /// Construct a new in-memory (or temporary) instance.
    fn new() -> Self;
    /// Construct an instance backed by a persistent path (e.g. RocksDB directory).
    /// Implementations should create the store if missing.
    fn new_with_path<P: AsRef<std::path::Path>>(_path: P) -> rocks_storage::RocksResult<Self>
    where
        Self: Sized,
    {
        // Default fallback: construct an in-memory instance. Implementations
        // that support persistence should override this.
        Ok(Self::new())
    }
    /// Core batch insertion primitive each implementation must provide.
    /// Implementations should accept a slice of entries already de-duplicated and
    /// (ideally) sorted by key for best performance, but callers are not required
    /// to sort before calling.
    fn batch_upsert(&mut self, entries: &[(Hash, Hash)]);
    /// Enumerate all nodes currently materialized in memory. Implementations
    /// may choose to lazy-load before returning.
    fn enumerate_nodes(&self) -> Vec<(Prefix, Node)>;
    /// Root merkle hash if the tree is non-empty.
    fn get_root_hash(&self) -> Option<Hash>;
    /// Lookup leaf value by full key hash.
    fn get_leaf_value(&self, key: Hash) -> Option<Hash>;

    /// Convenience single-key upsert implemented in terms of batch_upsert.
    #[inline]
    fn upsert(&mut self, key: Hash, value: Hash) {
        self.batch_upsert(&[(key, value)]);
    }

    /// Generic iterator-based batch convenience. This avoids forcing callers
    /// to allocate a Vec if they already have an iterator. Default collects
    /// into a small Vec then delegates to required slice method.
    #[inline]
    fn batch_upsert_iter<I>(&mut self, entries: I)
    where
        I: IntoIterator<Item = (Hash, Hash)>,
    {
        let vec: Vec<(Hash, Hash)> = entries.into_iter().collect();
        if vec.is_empty() {
            return;
        }
        self.batch_upsert(&vec);
    }
}

#[cfg(test)]
mod tests;

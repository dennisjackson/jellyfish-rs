use sha2::Digest;

use crate::{Hash, Prefix};

mod simple;
pub use simple::SimpleMPT;

mod batch;
pub use batch::BatchMPT;

mod durable_batch;
pub use durable_batch::DurableBatchMPT;

mod cache;
pub use cache::Cache;

#[derive(Clone)]
pub struct LeafNode {
    pub key: Hash,
    pub value: Hash,
    pub merkle_hash: Hash,
}

impl LeafNode {
    pub fn new(key: Hash, value: Hash) -> Self {
        Self {
            key,
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

#[derive(Clone)]
pub struct InteriorNode {
    pub prefix: Prefix,
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
            prefix,
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

#[derive(Clone)]
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
                    (leaf.key, leaf.value, leaf.merkle_hash),
                    bincode::config::standard(),
                )
                .map_err(|e| format!("Failed to encode leaf: {}", e))?;
                Ok(("leaf", data))
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
                .map_err(|e| format!("Failed to encode interior: {}", e))?;
                Ok(("interior", data))
            }
        }
    }

    /// Deserialize a node from its database representation.
    pub fn deserialize(node_type: &str, node_data: &[u8]) -> Result<Self, String> {
        match node_type {
            "leaf" => {
                let decoded: (Hash, Hash, Hash) =
                    bincode::decode_from_slice(node_data, bincode::config::standard())
                        .map(|(v, _)| v)
                        .map_err(|e| format!("Failed to decode leaf: {}", e))?;
                Ok(Node::Leaf(LeafNode {
                    key: decoded.0,
                    value: decoded.1,
                    merkle_hash: decoded.2,
                }))
            }
            "interior" => {
                let decoded: (Prefix, Hash, Prefix, Prefix) =
                    bincode::decode_from_slice(node_data, bincode::config::standard())
                        .map(|(v, _)| v)
                        .map_err(|e| format!("Failed to decode interior: {}", e))?;
                Ok(Node::Interior(InteriorNode {
                    prefix: decoded.0,
                    merkle_hash: decoded.1,
                    left: decoded.2,
                    right: decoded.3,
                }))
            }
            _ => Err(format!("Unknown node type: {}", node_type)),
        }
    }
}

pub trait MerklePatriciaTree {
    fn new() -> Self;
    fn upsert(&mut self, key: Hash, value: Hash);
    fn batch_upsert(&mut self, entries: &[(Hash, Hash)]);
    fn enumerate_nodes(&self) -> Vec<(Prefix, Node)>;
    fn get_root_hash(&self) -> Option<Hash>;
    fn get_leaf_value(&self, key: Hash) -> Option<Hash>;
}

#[cfg(test)]
mod tests;

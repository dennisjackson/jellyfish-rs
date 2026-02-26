use dashmap::DashMap;
use log::info;
use std::sync::Arc;

use crate::mpt::MerklePatriciaTree;
use crate::{Hash, Prefix};

use super::{Node, NodeStore};

impl NodeStore for Arc<DashMap<Prefix, Node>> {
    fn get_node(&self, prefix: &Prefix) -> Option<Node> {
        self.get(prefix).map(|n| n.clone())
    }
    fn set_node(&self, prefix: Prefix, node: Node) {
        self.insert(prefix, node);
    }
}

/// A batch-optimized Merkle Patricia Tree implementation.
/// This implementation efficiently performs batch upserts by deferring
/// hash recalculations until after all insertions are complete.
/// Uses a concurrent hashmap (DashMap) for thread-safe parallel operations.
pub struct BatchMPT {
    pub(crate) store: Arc<DashMap<Prefix, Node>>,
    pub(crate) root: Prefix,
}

impl Default for BatchMPT {
    fn default() -> Self {
        Self::new()
    }
}

impl BatchMPT {
    pub fn new() -> Self {
        Self {
            store: Arc::new(DashMap::new()),
            root: Prefix::root(),
        }
    }

    fn batch_upsert_optimized(&mut self, entries: &[(Hash, Hash)]) {
        if entries.is_empty() {
            return;
        }

        info!("Batch upserting {} entries", entries.len());

        let entries_vec = super::sorted_unique_entries(entries);
        self.root = super::batch_ops::batch_upsert_recursive(&self.store, self.root, entries_vec);
    }
}

impl MerklePatriciaTree for BatchMPT {
    fn new() -> Self {
        Self::new()
    }

    fn upsert(&mut self, key: Hash, value: Hash) {
        self.batch_upsert_optimized(&[(key, value)]);
    }

    fn enumerate_nodes(&self) -> Vec<(Prefix, Node)> {
        self.store
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect()
    }

    fn get_root_hash(&self) -> Option<Hash> {
        self.store.get(&self.root).map(|n| n.merkle_hash())
    }

    fn get_leaf_value(&self, key: Hash) -> Option<Hash> {
        let prefix = Prefix::from(key);
        match self.store.get(&prefix).as_deref() {
            Some(Node::Leaf(leaf)) => Some(leaf.value),
            _ => None,
        }
    }

    fn batch_upsert(&mut self, entries: &[(Hash, Hash)]) {
        self.batch_upsert_optimized(entries);
    }
}

use dashmap::DashMap;
use log::info;
use std::sync::Arc;

use crate::mpt::MerklePatriciaTree;
use crate::{Hash, Prefix};

use super::{InteriorNode, LeafNode, Node};

/// A batch-optimized Merkle Patricia Tree implementation.
/// This implementation efficiently performs batch upserts by deferring
/// hash recalculations until after all insertions are complete.
/// Uses a concurrent hashmap (DashMap) for thread-safe parallel operations.
pub struct BatchMPT {
    pub store: Arc<DashMap<Prefix, Node>>,
    pub root: Prefix,
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

    /// Batch upsert with recursive single-pass optimization.
    /// This method traverses the tree only once, partitioning entries at each interior node
    /// and updating hashes on the way back up the recursion.
    fn batch_upsert_optimized(&mut self, entries: &[(Hash, Hash)]) {
        if entries.is_empty() {
            return;
        }

        info!("Batch upserting {} entries", entries.len());

        // Convert to sorted, deduplicated vector for efficient partitioning
        let entries_vec = super::sorted_unique_entries(entries);

        // Perform recursive batch upsert
        let new_root = Self::recursive_batch_upsert(&self.store, self.root, entries_vec);
        self.root = new_root;
    }

    /// Recursively batch upsert entries at the current node.
    /// Returns the prefix of the (possibly new) root of this subtree.
    fn recursive_batch_upsert(
        store: &Arc<DashMap<Prefix, Node>>,
        current_prefix: Prefix,
        entries: Vec<(Hash, Hash)>,
    ) -> Prefix {
        if entries.is_empty() {
            return current_prefix;
        }

        let node = store.get(&current_prefix).map(|n| n.clone());
        let Some(node) = node else {
            // Empty tree: insert all entries
            return Self::batch_insert_into_empty(store, entries);
        };

        match node {
            Node::Leaf(leaf) => Self::batch_upsert_at_leaf(store, current_prefix, leaf, entries),
            Node::Interior(interior) => {
                Self::batch_upsert_at_interior(store, current_prefix, interior, entries)
            }
        }
    }

    /// Insert all entries into an empty tree.
    fn batch_insert_into_empty(
        store: &Arc<DashMap<Prefix, Node>>,
        mut entries: Vec<(Hash, Hash)>,
    ) -> Prefix {
        if entries.is_empty() {
            return Prefix::root();
        }

        // Start with the first entry
        let (first_key, first_value) = entries.remove(0);

        let first_prefix = Prefix::from(first_key);
        let first_leaf = LeafNode::new(first_key, first_value);
        store.insert(first_prefix, Node::Leaf(first_leaf));

        // Recursively insert remaining entries
        Self::recursive_batch_upsert(store, first_prefix, entries)
    }

    /// Batch upsert at a leaf node.
    fn batch_upsert_at_leaf(
        store: &Arc<DashMap<Prefix, Node>>,
        leaf_prefix: Prefix,
        leaf: LeafNode,
        mut entries: Vec<(Hash, Hash)>,
    ) -> Prefix {
        // Check if any entry updates this leaf (using binary search since entries are sorted)
        if let Ok(idx) = entries.binary_search_by_key(&leaf_prefix.hash, |(k, _)| *k) {
            let (_, new_value) = entries.remove(idx);
            let updated_leaf = LeafNode::new(leaf_prefix.hash, new_value);
            store.insert(leaf_prefix, Node::Leaf(updated_leaf));

            if entries.is_empty() {
                return leaf_prefix;
            }
            // Continue inserting remaining entries
            return Self::recursive_batch_upsert(store, leaf_prefix, entries);
        }

        if entries.is_empty() {
            return leaf_prefix;
        }

        // Split: need to create interior node(s) and distribute entries
        // Start with the first non-matching entry
        let (first_key, first_value) = entries.remove(0);

        let new_leaf = LeafNode::new(first_key, first_value);
        let new_prefix = Prefix::from(first_key);
        let existing_prefix = leaf_prefix;
        let merged_prefix = Prefix::common_prefix(&existing_prefix, &new_prefix);

        let (left_prefix, right_prefix, left_hash, right_hash) = super::order_children(
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

        store.insert(merged_prefix, Node::Interior(new_interior));
        store.insert(existing_prefix, Node::Leaf(leaf));
        store.insert(new_prefix, Node::Leaf(new_leaf));

        // Continue with remaining entries
        Self::recursive_batch_upsert(store, merged_prefix, entries)
    }

    /// Batch upsert at an interior node.
    fn batch_upsert_at_interior(
        store: &Arc<DashMap<Prefix, Node>>,
        interior_prefix: Prefix,
        interior: InteriorNode,
        entries: Vec<(Hash, Hash)>,
    ) -> Prefix {
        // Partition entries: those that belong under this node vs. those that diverge
        let mut contained_entries = Vec::new();
        let mut divergent_entries = Vec::new();

        for &(key, value) in entries.iter() {
            if interior_prefix.contains(&key) {
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
            let common = Prefix::common_prefix(&interior_prefix, &new_leaf_prefix);

            let (left_prefix, right_prefix, left_hash, right_hash) = super::order_children(
                &common,
                first_key,
                new_leaf_prefix,
                new_leaf.merkle_hash,
                interior_prefix,
                interior.merkle_hash,
            );

            let new_interior =
                InteriorNode::new(common, left_prefix, right_prefix, left_hash, right_hash);

            store.insert(common, Node::Interior(new_interior));
            store.insert(new_leaf_prefix, Node::Leaf(new_leaf));

            // Merge remaining entries and continue
            contained_entries.extend(divergent_entries);
            return Self::recursive_batch_upsert(store, common, contained_entries);
        }

        // All entries belong under this interior node
        // Partition them by left/right
        let mut left_entries = Vec::new();
        let mut right_entries = Vec::new();

        for &(key, value) in contained_entries.iter() {
            if interior_prefix.key_goes_right(key) {
                right_entries.push((key, value));
            } else {
                left_entries.push((key, value));
            }
        }

        // Recursively process left and right subtrees in parallel
        let store_clone = Arc::clone(store);
        let (new_left, new_right) = rayon::join(
            || {
                if !left_entries.is_empty() {
                    Self::recursive_batch_upsert(store, interior.left, left_entries)
                } else {
                    interior.left
                }
            },
            || {
                if !right_entries.is_empty() {
                    Self::recursive_batch_upsert(&store_clone, interior.right, right_entries)
                } else {
                    interior.right
                }
            },
        );

        // Recalculate this interior node's hash based on updated children
        let left_hash = store.get(&new_left).unwrap().merkle_hash();
        let right_hash = store.get(&new_right).unwrap().merkle_hash();

        let updated_interior =
            InteriorNode::new(interior_prefix, new_left, new_right, left_hash, right_hash);

        store.insert(interior_prefix, Node::Interior(updated_interior));
        interior_prefix
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

use log::{debug, info};
use std::collections::HashMap;

use crate::mpt::MerklePatriciaTree;
use crate::prefix::HashExt;
use crate::{Hash, Prefix};

use super::{InteriorNode, LeafNode, Node};

/// A batch-optimized Merkle Patricia Tree implementation.
/// This implementation efficiently performs batch upserts by deferring
/// hash recalculations until after all insertions are complete.
pub struct BatchMPT {
    pub store: HashMap<Prefix, Node>,
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
            store: HashMap::new(),
            root: Prefix::root(),
        }
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
    fn batch_upsert_optimized(&mut self, entries: &[(Hash, Hash)]) {
        if entries.is_empty() {
            return;
        }

        info!("Batch upserting {} entries", entries.len());

        // Convert to HashMap for efficient lookups and updates
        let mut entries_map: HashMap<Hash, Hash> = entries.iter().copied().collect();

        // Perform recursive batch upsert
        let new_root = self.recursive_batch_upsert(self.root, &mut entries_map);
        self.root = new_root;
    }

    /// Recursively batch upsert entries at the current node.
    /// Returns the prefix of the (possibly new) root of this subtree.
    fn recursive_batch_upsert(
        &mut self,
        current_prefix: Prefix,
        entries: &mut HashMap<Hash, Hash>,
    ) -> Prefix {
        if entries.is_empty() {
            return current_prefix;
        }

        let Some(node) = self.store.get(&current_prefix).cloned() else {
            // Empty tree: insert all entries
            return self.batch_insert_into_empty(entries);
        };

        match node {
            Node::Leaf(leaf) => self.batch_upsert_at_leaf(leaf, entries),
            Node::Interior(interior) => self.batch_upsert_at_interior(interior, entries),
        }
    }

    /// Insert all entries into an empty tree.
    fn batch_insert_into_empty(&mut self, entries: &mut HashMap<Hash, Hash>) -> Prefix {
        if entries.is_empty() {
            return Prefix::root();
        }

        // Start with the first entry
        let (&first_key, &first_value) = entries.iter().next().unwrap();
        entries.remove(&first_key);

        let first_prefix = Prefix::from(first_key);
        let first_leaf = LeafNode::new(first_key, first_value);
        self.store.insert(first_prefix, Node::Leaf(first_leaf));

        // Recursively insert remaining entries
        self.recursive_batch_upsert(first_prefix, entries)
    }

    /// Batch upsert at a leaf node.
    fn batch_upsert_at_leaf(
        &mut self,
        leaf: LeafNode,
        entries: &mut HashMap<Hash, Hash>,
    ) -> Prefix {
        let leaf_prefix = Prefix::from(leaf.key);

        // Check if any entry updates this leaf
        if let Some(&new_value) = entries.get(&leaf.key) {
            entries.remove(&leaf.key);
            let updated_leaf = LeafNode::new(leaf.key, new_value);
            self.store.insert(leaf_prefix, Node::Leaf(updated_leaf));

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
        let (&first_key, &first_value) = entries.iter().next().unwrap();
        entries.remove(&first_key);

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

        self.store
            .insert(merged_prefix, Node::Interior(new_interior));
        self.store.insert(existing_prefix, Node::Leaf(leaf));
        self.store.insert(new_prefix, Node::Leaf(new_leaf));

        // Continue with remaining entries
        self.recursive_batch_upsert(merged_prefix, entries)
    }

    /// Batch upsert at an interior node.
    fn batch_upsert_at_interior(
        &mut self,
        interior: InteriorNode,
        entries: &mut HashMap<Hash, Hash>,
    ) -> Prefix {
        // Partition entries: those that belong under this node vs. those that diverge
        let mut contained_entries = HashMap::new();
        let mut divergent_entries = HashMap::new();

        for (&key, &value) in entries.iter() {
            if interior.prefix.contains(&key) {
                contained_entries.insert(key, value);
            } else {
                divergent_entries.insert(key, value);
            }
        }

        // Handle divergent entries first (they require creating a new parent)
        if !divergent_entries.is_empty() {
            // Create new parent(s) for divergent entries
            let (&first_key, &first_value) = divergent_entries.iter().next().unwrap();
            divergent_entries.remove(&first_key);

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

            self.store.insert(common, Node::Interior(new_interior));
            self.store.insert(new_leaf_prefix, Node::Leaf(new_leaf));

            // Merge remaining entries and continue
            contained_entries.extend(divergent_entries);
            *entries = contained_entries;
            return self.recursive_batch_upsert(common, entries);
        }

        // All entries belong under this interior node
        // Partition them by left/right
        let mut left_entries = HashMap::new();
        let mut right_entries = HashMap::new();

        for (&key, &value) in contained_entries.iter() {
            if interior.prefix.key_goes_right(key) {
                right_entries.insert(key, value);
            } else {
                left_entries.insert(key, value);
            }
        }

        // Recursively process left and right subtrees
        let new_left = if !left_entries.is_empty() {
            self.recursive_batch_upsert(interior.left, &mut left_entries)
        } else {
            interior.left
        };

        let new_right = if !right_entries.is_empty() {
            self.recursive_batch_upsert(interior.right, &mut right_entries)
        } else {
            interior.right
        };

        // Recalculate this interior node's hash based on updated children
        let left_hash = self.store.get(&new_left).unwrap().merkle_hash();
        let right_hash = self.store.get(&new_right).unwrap().merkle_hash();

        let updated_interior =
            InteriorNode::new(interior.prefix, new_left, new_right, left_hash, right_hash);

        self.store
            .insert(interior.prefix, Node::Interior(updated_interior));
        entries.clear();
        interior.prefix
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
        self.store.iter().map(|(k, v)| (*k, v.clone())).collect()
    }

    fn get_root_hash(&self) -> Option<Hash> {
        self.store.get(&self.root).map(|n| n.merkle_hash())
    }

    fn get_leaf_value(&self, key: Hash) -> Option<Hash> {
        let prefix = Prefix::from(key);
        match self.store.get(&prefix) {
            Some(Node::Leaf(leaf)) => Some(leaf.value),
            _ => None,
        }
    }

    fn batch_upsert(&mut self, entries: &[(Hash, Hash)]) {
        self.batch_upsert_optimized(entries);
    }
}

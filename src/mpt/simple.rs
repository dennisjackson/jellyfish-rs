use log::{debug, info};
use std::collections::HashMap;

use crate::mpt::MerklePatriciaTree;
use crate::prefix::HashExt;
use crate::{Hash, Prefix};

use super::{InteriorNode, LeafNode, Node};

pub struct SimpleMPT {
    pub store: HashMap<Prefix, Node>,
    pub root: Prefix,
}

impl Default for SimpleMPT {
    fn default() -> Self {
        Self::new()
    }
}

impl SimpleMPT {
    pub fn new() -> Self {
        Self {
            store: HashMap::new(),
            root: Prefix::root(),
        }
    }

    pub fn upsert(&mut self, key: Hash, value: Hash) {
        info!(
            "Upserting key: {}, value: {}",
            key.short_hex(),
            value.short_hex()
        );
        let new_root = self.recursive_upsert(self.root, key, value);
        self.root = new_root;
    }

    fn recursive_upsert(&mut self, current_prefix: Prefix, key: Hash, value: Hash) -> Prefix {
        debug!("Current node prefix {:?}", current_prefix.short_hex());

        let key_prefix = Prefix::from(key);
        let Some(node) = self.store.get(&current_prefix).cloned() else {
            // Empty tree: insert new leaf node
            debug!("Tree is empty, inserting a new leaf");
            let new_leaf = LeafNode::new(key, value);
            self.insert_node(key_prefix, Node::Leaf(new_leaf));
            return key_prefix;
        };

        match node {
            Node::Leaf(leaf) => self.base_leaf_upsert(current_prefix, leaf, key, value),
            Node::Interior(interior) => {
                self.recursive_interior_upsert(current_prefix, interior, key, value)
            }
        }
    }

    fn base_leaf_upsert(
        &mut self,
        leaf_prefix: Prefix,
        leaf: LeafNode,
        key: Hash,
        value: Hash,
    ) -> Prefix {
        debug!("At leaf node with key {}", leaf_prefix.hash.short_hex());

        if leaf_prefix.hash == key {
            // Update existing leaf in place
            debug!("At leaf node with matching key, updating in place");
            let updated_leaf = LeafNode::new(key, value);
            self.insert_node(leaf_prefix, Node::Leaf(updated_leaf));
            return leaf_prefix;
        }

        // Split: create new interior node with both leaves as children
        debug!("At leaf node with different key, splitting");
        let new_leaf = LeafNode::new(key, value);
        let existing_prefix = leaf_prefix;
        let new_prefix = Prefix::from(key);
        let merged_prefix = Prefix::common_prefix(&existing_prefix, &new_prefix);

        let (left_prefix, right_prefix, left_hash, right_hash) = super::order_children(
            &merged_prefix,
            key,
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

        self.insert_node(merged_prefix, Node::Interior(new_interior));
        self.insert_node(existing_prefix, Node::Leaf(leaf));
        self.insert_node(new_prefix, Node::Leaf(new_leaf));

        merged_prefix
    }

    fn recursive_interior_upsert(
        &mut self,
        interior_prefix: Prefix,
        interior: InteriorNode,
        key: Hash,
        value: Hash,
    ) -> Prefix {
        debug!(
            "At interior node prefix {} left: {} right: {}",
            interior_prefix.short_hex(),
            interior.left.short_hex(),
            interior.right.short_hex()
        );

        if !interior_prefix.contains(&key) {
            // Key diverges before interior's prefix ends: create new parent
            debug!("Key diverges from interior prefix, creating new parent");
            let new_leaf = LeafNode::new(key, value);
            let new_leaf_prefix = Prefix::from(key);
            let common = Prefix::common_prefix(&interior_prefix, &new_leaf_prefix);

            let (left_prefix, right_prefix, left_hash, right_hash) = super::order_children(
                &common,
                key,
                new_leaf_prefix,
                new_leaf.merkle_hash,
                interior_prefix,
                interior.merkle_hash,
            );

            let new_interior =
                InteriorNode::new(common, left_prefix, right_prefix, left_hash, right_hash);

            self.insert_node(common, Node::Interior(new_interior));
            self.insert_node(new_leaf_prefix, Node::Leaf(new_leaf));
            return common;
        }

        // Key belongs under this interior: descend to appropriate child
        let goes_right = interior_prefix.key_goes_right(key);
        let (new_left, new_right) = if goes_right {
            debug!("At interior node, descending right");
            (
                interior.left,
                self.recursive_upsert(interior.right, key, value),
            )
        } else {
            debug!("At interior node, descending left");
            (
                self.recursive_upsert(interior.left, key, value),
                interior.right,
            )
        };

        let left_hash = self.store.get(&new_left).unwrap().merkle_hash();
        let right_hash = self.store.get(&new_right).unwrap().merkle_hash();

        let updated_interior =
            InteriorNode::new(interior_prefix, new_left, new_right, left_hash, right_hash);

        self.insert_node(interior_prefix, Node::Interior(updated_interior));
        interior_prefix
    }

    fn insert_node(&mut self, prefix: Prefix, node: Node) {
        match &node {
            Node::Leaf(leaf) => {
                debug!(
                    "  Inserting Leaf at {}: value={}",
                    prefix.short_hex(),
                    leaf.value.short_hex(),
                );
            }
            Node::Interior(interior) => {
                debug!(
                    "  Inserting Interior at {}: left={} right={}",
                    prefix.short_hex(),
                    interior.left.short_hex(),
                    interior.right.short_hex(),
                );
            }
        }
        self.store.insert(prefix, node);
    }
}

impl MerklePatriciaTree for SimpleMPT {
    fn new() -> Self {
        Self::new()
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
        for (key, value) in entries {
            self.upsert(*key, *value);
        }
    }
}

//! The oracle: single-key, in-memory, materialises every node. Deliberately a different
//! algorithm from the production tree so the two cannot share a bug.

use std::collections::HashMap;

use crate::{Digest, Entry, Key, Prefix, Value, hash};

#[derive(Clone, Debug)]
pub struct LeafNode {
    pub value: Value,
    pub merkle_hash: Digest,
}

impl LeafNode {
    pub fn new(key: Key, value: Value) -> Self {
        Self {
            value,
            merkle_hash: hash::leaf(key, value),
        }
    }
}

/// An interior node: its hash and the compressed prefixes of its children.
#[derive(Clone, Debug)]
pub struct InteriorNode {
    pub merkle_hash: Digest,
    pub left: Prefix,
    pub right: Prefix,
}

impl InteriorNode {
    pub fn new(
        prefix: Prefix,
        left: Prefix,
        right: Prefix,
        left_hash: Digest,
        right_hash: Digest,
    ) -> Self {
        Self {
            left,
            right,
            merkle_hash: hash::interior(prefix, left_hash, right_hash),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Node {
    Leaf(LeafNode),
    Interior(InteriorNode),
}

impl Node {
    pub fn merkle_hash(&self) -> Digest {
        match self {
            Node::Leaf(leaf) => leaf.merkle_hash,
            Node::Interior(interior) => interior.merkle_hash,
        }
    }
}

#[derive(Default)]
pub struct SimpleMPT {
    store: HashMap<Prefix, Node>,
    root: Option<Prefix>,
}

impl SimpleMPT {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn nodes(&self) -> Vec<(Prefix, Node)> {
        self.store.iter().map(|(k, v)| (*k, v.clone())).collect()
    }

    pub fn batch_upsert(&mut self, entries: &[Entry]) {
        for (key, value) in entries {
            self.upsert(*key, *value);
        }
    }

    pub fn get_root_hash(&self) -> Option<Digest> {
        self.store.get(&self.root?).map(|n| n.merkle_hash())
    }

    pub fn get_leaf_value(&self, key: Key) -> Option<Value> {
        match self.store.get(&Prefix::from(key)) {
            Some(Node::Leaf(leaf)) => Some(leaf.value),
            _ => None,
        }
    }

    pub fn upsert(&mut self, key: Key, value: Value) {
        let root = self.root.unwrap_or_else(Prefix::root);
        self.root = Some(self.recursive_upsert(root, key, value));
    }

    fn recursive_upsert(&mut self, current_prefix: Prefix, key: Key, value: Value) -> Prefix {
        let key_prefix = Prefix::from(key);
        let Some(node) = self.store.get(&current_prefix).cloned() else {
            self.store
                .insert(key_prefix, Node::Leaf(LeafNode::new(key, value)));
            return key_prefix;
        };

        match node {
            Node::Leaf(_) if current_prefix.key() == key => {
                self.store
                    .insert(current_prefix, Node::Leaf(LeafNode::new(key, value)));
                current_prefix
            }
            Node::Leaf(leaf) => self.split(current_prefix, leaf.merkle_hash, key, value),
            Node::Interior(interior) => {
                self.recursive_interior_upsert(current_prefix, interior, key, value)
            }
        }
    }

    /// Push the node at `sibling_prefix` under a new interior at its common prefix with a
    /// new leaf for `key`; return the interior's prefix. The sibling stays where it is.
    fn split(
        &mut self,
        sibling_prefix: Prefix,
        sibling_hash: Digest,
        key: Key,
        value: Value,
    ) -> Prefix {
        let new_leaf = LeafNode::new(key, value);
        let new_prefix = Prefix::from(key);
        let split = Prefix::common_prefix(&sibling_prefix, &new_prefix);

        let (left, right, left_hash, right_hash) = if split.key_goes_right(key) {
            (
                sibling_prefix,
                new_prefix,
                sibling_hash,
                new_leaf.merkle_hash,
            )
        } else {
            (
                new_prefix,
                sibling_prefix,
                new_leaf.merkle_hash,
                sibling_hash,
            )
        };

        self.store.insert(
            split,
            Node::Interior(InteriorNode::new(split, left, right, left_hash, right_hash)),
        );
        self.store.insert(new_prefix, Node::Leaf(new_leaf));
        split
    }

    fn recursive_interior_upsert(
        &mut self,
        interior_prefix: Prefix,
        interior: InteriorNode,
        key: Key,
        value: Value,
    ) -> Prefix {
        if !interior_prefix.contains(&key) {
            return self.split(interior_prefix, interior.merkle_hash, key, value);
        }

        let goes_right = interior_prefix.key_goes_right(key);
        let (new_left, new_right) = if goes_right {
            (
                interior.left,
                self.recursive_upsert(interior.right, key, value),
            )
        } else {
            (
                self.recursive_upsert(interior.left, key, value),
                interior.right,
            )
        };

        let left_hash = self
            .store
            .get(&new_left)
            .expect("left child missing after upsert")
            .merkle_hash();
        let right_hash = self
            .store
            .get(&new_right)
            .expect("right child missing after upsert")
            .merkle_hash();

        let updated_interior =
            InteriorNode::new(interior_prefix, new_left, new_right, left_hash, right_hash);

        self.store
            .insert(interior_prefix, Node::Interior(updated_interior));
        interior_prefix
    }
}

// So we want an MPT
// It exposes upsert
// It exposes get_proof

// We're going to do it all in memory
// We're going to do it one operation at time

use log::{debug, info};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

mod prefix;
pub use prefix::{Hash, Prefix};

#[derive(Clone)]
pub struct LeafNode {
    pub key: Hash,
    pub value: Hash,
    pub merkle_hash: Hash,
}

impl LeafNode {
    pub fn new(key: Hash, value: Hash) -> Self {
        LeafNode {
            key,
            value,
            merkle_hash: Self::calculate_hash(key, value),
        }
    }

    pub fn calculate_hash(key: Hash, value: Hash) -> Hash {
        let mut hasher = sha2::Sha256::new();
        hasher.update(b"leaf");
        hasher.update(&key);
        hasher.update(&value);
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
        InteriorNode {
            prefix,
            merkle_hash: Self::calculate_hash(prefix, left_hash, right_hash),
            left,
            right,
        }
    }

    pub fn calculate_hash(prefix: Prefix, left_hash: Hash, right_hash: Hash) -> Hash {
        let mut hasher = sha2::Sha256::new();
        hasher.update(b"interior");
        hasher.update(&prefix.hash);
        hasher.update(&prefix.length.to_be_bytes());
        hasher.update(&left_hash);
        hasher.update(&right_hash);
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
}

pub struct SimpleMPT {
    pub store: HashMap<Prefix, Node>,
    pub root: Prefix,
}

fn short_hex_hash(hash: &Hash) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(8);
    for b in hash.iter().take(4) {
        // 4 bytes = 8 hex chars
        write!(&mut s, "{:02x}", b).unwrap();
    }
    s
}

fn short_hex(hash: &Hash) -> String {
    short_hex_hash(hash)
}

fn short_hex_prefix(prefix: &Prefix) -> String {
    if prefix.length == 256 {
        return short_hex(&prefix.hash);
    } else if prefix.length == 0 {
        return "empty".into();
    }
    let mut prefix_bits = Vec::new();
    for i in 0..prefix.length {
        let bit = (prefix.hash[(i / 8) as usize] >> (7 - (i % 8))) & 1;
        prefix_bits.push(bit);
    }
    prefix_bits.iter().map(|b| if *b > 0 { '1' } else { '0' }).collect()
}

impl SimpleMPT {
    pub fn new() -> Self {
        SimpleMPT {
            store: HashMap::new(),
            root : Prefix::root(),
        }
    }

    pub fn upsert(&mut self, key: Hash, value: Hash) {
        info!(
            "Upserting key: {}, value: {}",
            short_hex(&key),
            short_hex(&value)
        );
        let new_root = self.recursive_upsert(self.root, key, value);
        self.root = new_root;
    }

    fn recursive_upsert(&mut self, current_prefix: Prefix, key: Hash, value: Hash) -> Prefix {
        let mut updates = Vec::new();
        let finished_prefix: Prefix;
        let current_node = self.store.get(&current_prefix).cloned(); // Clone the node, ending the borrow
        debug!("Current node prefix {:?}", short_hex_prefix(&current_prefix));
        if let Some(node) = current_node {
            match node {
                Node::Leaf(leaf) => {
                    debug!("At leaf node with key {}", short_hex(&leaf.key));
                    if leaf.key == key {
                        debug!("At leaf node with matching key, updating in place");
                        let updated_leaf = LeafNode::new(key, value);
                        updates.push((current_prefix, Node::Leaf(updated_leaf)));
                        finished_prefix = current_prefix;
                    } else {
                        debug!("At leaf node with different key, splitting");
                        let new_leaf = LeafNode::new(key, value);
                        let left = if leaf.key < key { &leaf } else { &new_leaf };
                        let right = if leaf.key < key { &new_leaf } else { &leaf };
                        let lp = Prefix::from_hash(left.key);
                        let rp = Prefix::from_hash(right.key);
                        let merged_prefix = Prefix::common_prefix(&lp, &rp);
                        let new_interior = InteriorNode::new(
                            merged_prefix,
                            lp,
                            rp,
                            left.merkle_hash,
                            right.merkle_hash,
                        );
                        updates.push((merged_prefix, Node::Interior(new_interior)));
                        updates.push((lp, Node::Leaf(left.clone())));
                        updates.push((rp, Node::Leaf(right.clone())));
                        finished_prefix = merged_prefix;
                    }
                }
                Node::Interior(interior) => {
                    debug!("At interior node prefix {} left: {} right: {}", short_hex_prefix(&interior.prefix), short_hex_prefix(&interior.left), short_hex_prefix(&interior.right));
                    if key < interior.prefix.hash {
                        debug!("At interior node, descending left");
                        let new_left = self.recursive_upsert(interior.left, key, value);
                        updates.push((
                            interior.prefix,
                            Node::Interior(InteriorNode::new(
                                interior.prefix,
                                new_left,
                                interior.right,
                                self.store.get(&new_left).unwrap().merkle_hash(),
                                self.store.get(&interior.right).unwrap().merkle_hash(),
                            )),
                        ));
                        finished_prefix = interior.prefix;
                    } else if key >= interior.prefix.hash {
                        debug!("At interior node, descending right");
                        let new_right = self.recursive_upsert(interior.right, key, value);
                        updates.push((
                            interior.prefix,
                            Node::Interior(InteriorNode::new(
                                interior.prefix,
                                interior.left,
                                new_right,
                                self.store.get(&interior.left).unwrap().merkle_hash(),
                                self.store.get(&new_right).unwrap().merkle_hash(),
                            )),
                        ));
                        finished_prefix = interior.prefix;
                    } else {
                        debug!("At interior node, but neither child is a prefix, inserting");
                        let new_leaf = LeafNode::new(key, value);
                        let nlp = Prefix::from_hash(new_leaf.key);
                        let (left, lmh) = if interior.prefix < nlp {
                            (interior.prefix, interior.merkle_hash)
                        } else {
                            (nlp, new_leaf.merkle_hash)
                        };
                        let (right, rmh) = if interior.prefix >= nlp {
                            (interior.prefix, interior.merkle_hash)
                        } else {
                            (nlp, new_leaf.merkle_hash)
                        };
                        let new_interior = InteriorNode::new(
                            Prefix::common_prefix(&left, &right),
                            left,
                            right,
                            lmh,
                            rmh,
                        );
                        let nip = new_interior.prefix.clone();
                        updates.push((new_interior.prefix, Node::Interior(new_interior)));
                        updates.push((nlp, Node::Leaf(new_leaf)));
                        finished_prefix = nip;
                    }
                }
            }
        } else {
            // If no node exists at this prefix, insert a new leaf node
            // Only happens if the tree is empty.
            debug!("Tree is empty, inserting a new leaf");
            let new_leaf = LeafNode::new(key, value);
            updates.push((Prefix::from_hash(key), Node::Leaf(new_leaf)));
            finished_prefix = Prefix::from_hash(key);
        }
        debug!("Inserting/updating {} nodes", updates.len());
        for (prefix, node) in updates {
            match node {
                Node::Leaf(ref leaf) => {
                    debug!(
                        "  Node: Prefix: {} Leaf key:{} value:{}",
                        short_hex_prefix(&prefix),
                        short_hex(&leaf.key),
                        short_hex(&leaf.value),
                        // short_hex(&leaf.merkle_hash)
                    );
                }
                Node::Interior(ref interior) => {
                    debug!(
                        "  Node: Interior prefix:{} left:{} right:{}",
                        short_hex_prefix(&interior.prefix),
                        short_hex_prefix(&interior.left),
                        short_hex_prefix(&interior.right),
                        // short_hex(&interior.merkle_hash)
                    );
                }
            }
            self.store.insert(prefix, node);
        }
        return finished_prefix;
    }
}

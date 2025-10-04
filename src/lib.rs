// So we want an MPT
// It exposes upsert
// It exposes get_proof

// We're going to do it all in memory
// We're going to do it one operation at time

use log::{debug, info};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub type Hash = [u8; 32];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Prefix {
    pub hash: Hash,
    pub length: u16,
}

impl Prefix {
    pub fn from_hash(hash: Hash) -> Self {
        Prefix { hash, length: 256 }
    }

    pub fn root() -> Self {
        Prefix {
            hash: [0; 32],
            length: 0,
        }
    }

    pub fn prefix_of(&self, other: &Prefix) -> bool {
        if self.length > other.length {
            return false;
        }
        for i in 0..self.length {
            let bit_self = (self.hash[(i / 8) as usize] >> (7 - (i % 8))) & 1;
            let bit_other = (other.hash[(i / 8) as usize] >> (7 - (i % 8))) & 1;
            if bit_self != bit_other {
                return false;
            }
        }
        true
    }

    pub fn common_prefix(a: &Prefix, b: &Prefix) -> Prefix {
        let min_length = a.length.min(b.length);
        let mut common_bits = 0u16;
        for i in 0..min_length {
            let bit_a = (a.hash[(i / 8) as usize] >> (7 - (i % 8))) & 1 != 0;
            let bit_b = (b.hash[(i / 8) as usize] >> (7 - (i % 8))) & 1 != 0;
            if bit_a != bit_b {
                break;
            }
            common_bits += 1;
        }
        Prefix {
            hash: a.hash,
            length: common_bits,
        }
    }
}

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

//Upsert
//Starting at the root
//If we're a prefix of left, go left. Ditto for right.
//If we don't have a child in direction, then it's a simple insert.
//If we're NOT a child of the child. Then we need to create a new Interior node and insert it here.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_common_prefix_identical_full_length() {
        // Two identical hashes should have a common prefix of 256 bits
        let hash = [0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0,
                    0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88,
                    0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00,
                    0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let prefix_a = Prefix::from_hash(hash);
        let prefix_b = Prefix::from_hash(hash);

        let result = Prefix::common_prefix(&prefix_a, &prefix_b);

        assert_eq!(result.length, 256);
        assert_eq!(result.hash, hash);
    }

    #[test]
    fn test_common_prefix_completely_different() {
        // Hashes that differ in the first bit should have 0 common bits
        let hash_a = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let hash_b = [0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

        let prefix_a = Prefix::from_hash(hash_a);
        let prefix_b = Prefix::from_hash(hash_b);

        let result = Prefix::common_prefix(&prefix_a, &prefix_b);

        assert_eq!(result.length, 0);
    }

    #[test]
    fn test_common_prefix_first_byte_differs() {
        // Hashes that match for 7 bits, then differ
        let hash_a = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
                      0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
                      0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
                      0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        let hash_b = [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
                      0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
                      0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
                      0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

        let prefix_a = Prefix::from_hash(hash_a);
        let prefix_b = Prefix::from_hash(hash_b);

        let result = Prefix::common_prefix(&prefix_a, &prefix_b);

        // 0x00 ^ 0x01 = 0x01 which has 7 leading zeros
        assert_eq!(result.length, 7);
        assert_eq!(result.hash[0], 0x00); // Should copy from either prefix
    }

    #[test]
    fn test_common_prefix_full_bytes_match() {
        // First two bytes match completely, third byte differs
        let hash_a = [0xAB, 0xCD, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let hash_b = [0xAB, 0xCD, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

        let prefix_a = Prefix::from_hash(hash_a);
        let prefix_b = Prefix::from_hash(hash_b);

        let result = Prefix::common_prefix(&prefix_a, &prefix_b);

        // First 16 bits match, then they differ
        assert_eq!(result.length, 16);
        assert_eq!(result.hash[0], 0xAB);
        assert_eq!(result.hash[1], 0xCD);
    }

    #[test]
    fn test_common_prefix_partial_byte_match() {
        // Match in the middle of a byte
        let hash_a = [0xF0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let hash_b = [0xF8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

        let prefix_a = Prefix::from_hash(hash_a);
        let prefix_b = Prefix::from_hash(hash_b);

        let result = Prefix::common_prefix(&prefix_a, &prefix_b);

        // 0xF0 ^ 0xF8 = 0x08 which has 4 leading zeros
        assert_eq!(result.length, 4);
    }

    #[test]
    fn test_common_prefix_with_shorter_prefixes() {
        // Test with prefixes that are not full 256-bit hashes
        let hash_a = [0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let hash_b = [0xF0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

        let prefix_a = Prefix { hash: hash_a, length: 4 };  // Only 4 bits long
        let prefix_b = Prefix { hash: hash_b, length: 8 };  // 8 bits long

        let result = Prefix::common_prefix(&prefix_a, &prefix_b);

        // Should only compare up to min_length (4 bits)
        // Both have 0xF in the top nibble, so common prefix is 4 bits
        assert_eq!(result.length, 4);
    }

    #[test]
    fn test_common_prefix_one_byte_all_bits_set() {
        let hash_a = [0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let hash_b = [0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

        let prefix_a = Prefix::from_hash(hash_a);
        let prefix_b = Prefix::from_hash(hash_b);

        let result = Prefix::common_prefix(&prefix_a, &prefix_b);

        // First byte matches completely
        assert_eq!(result.length, 8);
        assert_eq!(result.hash[0], 0xFF);
    }

    #[test]
    fn test_common_prefix_root_prefix() {
        // Test with the root prefix (length 0)
        let root = Prefix::root();
        let hash = [0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89,
                    0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89,
                    0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89,
                    0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89];
        let prefix = Prefix::from_hash(hash);

        let result = Prefix::common_prefix(&root, &prefix);

        // Common prefix with a length-0 prefix should be 0
        assert_eq!(result.length, 0);
    }

    #[test]
    fn test_common_prefix_symmetric() {
        // common_prefix should be symmetric in length, and the actual common bits should match
        let hash_a = [0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0,
                      0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88,
                      0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00,
                      0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let hash_b = [0x12, 0x34, 0x50, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

        let prefix_a = Prefix::from_hash(hash_a);
        let prefix_b = Prefix::from_hash(hash_b);

        let result_ab = Prefix::common_prefix(&prefix_a, &prefix_b);
        let result_ba = Prefix::common_prefix(&prefix_b, &prefix_a);

        assert_eq!(result_ab.length, result_ba.length);

        // The common bits (up to the common length) should be identical
        // We need to check bit-by-bit for the common prefix
        let common_len = result_ab.length;
        let full_bytes = (common_len / 8) as usize;

        // Check full bytes
        for i in 0..full_bytes {
            assert_eq!(result_ab.hash[i], result_ba.hash[i],
                "Byte {} differs: {:02x} vs {:02x}", i, result_ab.hash[i], result_ba.hash[i]);
        }

        // Check remaining bits if any
        if common_len % 8 != 0 {
            let rem_bits = common_len % 8;
            let mask = 0xFF << (8 - rem_bits);
            assert_eq!(result_ab.hash[full_bytes] & mask, result_ba.hash[full_bytes] & mask,
                "Partial byte {} differs", full_bytes);
        }
    }
}

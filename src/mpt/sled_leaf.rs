use dashmap::DashMap;
use rayon::join;
use sled::{Config, Db};
use std::path::Path;

use crate::mpt::MerklePatriciaTree;
use crate::{Hash, Prefix};

use super::{InteriorNode, LeafNode, Node};

const ROOT_KEY: &[u8] = b"__mpt_root__";
const COUNT_KEY: &[u8] = b"__mpt_count__";

pub struct SledLeafMPT {
    db: Db,
    store: DashMap<Prefix, Node>,
    root: Prefix,
    count: std::sync::atomic::AtomicUsize,
}

impl SledLeafMPT {
    fn get_default_config() -> Config {
        Config::new()
            .flush_every_ms(Some(5000))
            .mode(sled::Mode::HighThroughput)
            .use_compression(false)
            .print_profile_on_drop(true)
            .cache_capacity(1024 * 1024 * 1024 * 8) // 8 GB
    }

    pub fn new_with_path(path: impl AsRef<Path>) -> sled::Result<Self> {
        let db = SledLeafMPT::get_default_config().path(path).open()?;
        Self::from_db(db)
    }

    pub fn new_temporary() -> sled::Result<Self> {
        let db = SledLeafMPT::get_default_config().temporary(true).open()?;
        Self::from_db(db)
    }

    fn insert_node_with_db(&self, prefix: Prefix, node: Node) {
        let is_new_leaf = matches!(node, Node::Leaf(_)) && !self.store.contains_key(&prefix);
        self.store.insert(prefix, node.clone());
        self.db
            .insert(prefix_key(&prefix), encode_node(&node))
            .expect("DB insert failed");
        if is_new_leaf {
            let new_count = self
                .count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            self.db
                .insert(COUNT_KEY, &new_count.to_be_bytes())
                .expect("Failed to update count");
        }
    }

    fn insert_node_memory_only(&self, prefix: Prefix, node: Node) {
        self.store.insert(prefix, node);
    }

    fn from_db(db: Db) -> sled::Result<Self> {
        let root = match db.get(ROOT_KEY)? {
            Some(raw) => decode_prefix(raw.as_ref()).map_err(sled::Error::Unsupported)?,
            None => {
                let root = Prefix::root();
                db.insert(ROOT_KEY, encode_prefix(root))?;
                db.flush()?;
                root
            }
        };

        let count = match db.get(COUNT_KEY)? {
            Some(raw) => {
                if raw.len() == 8 {
                    usize::from_be_bytes(raw.as_ref().try_into().unwrap())
                } else {
                    0
                }
            }
            None => {
                db.insert(COUNT_KEY, &0_usize.to_be_bytes())?;
                0
            }
        };

        let iter_db = db.clone();
        let estimated_entries = count.max(1);
        let mut instance = Self {
            db,
            store: DashMap::with_capacity(estimated_entries.saturating_mul(2)),
            root,
            count: std::sync::atomic::AtomicUsize::new(count),
        };

        const RECOVERY_CHUNK: usize = 1024 * 1024;
        let mut leaf_entries: Vec<(Hash, Hash)> = Vec::with_capacity(RECOVERY_CHUNK);
        for entry in iter_db.iter() {
            let (key, value) = entry?;
            if key.as_ref() == ROOT_KEY || key.as_ref() == COUNT_KEY {
                continue;
            }
            let prefix = decode_prefix(key.as_ref()).map_err(sled::Error::Unsupported)?;
            let node = decode_node(value.as_ref()).map_err(sled::Error::Unsupported)?;
            if let Node::Leaf(leaf) = node {
                leaf_entries.push((prefix.hash, leaf.value));
                if leaf_entries.len() == RECOVERY_CHUNK {
                    assert!(leaf_entries.is_sorted());
                    instance.batch_upsert_memory_only(&leaf_entries);
                    leaf_entries.clear();
                }
            }
        }

        if !leaf_entries.is_empty() {
            instance.batch_upsert_memory_only(&leaf_entries);
        }

        Ok(instance)
    }

    fn batch_upsert_optimized(&mut self, entries: &[(Hash, Hash)]) {
        if entries.is_empty() {
            return;
        }

        let mut entries_vec: Vec<(Hash, Hash)> = entries.to_vec();
        entries_vec.sort_unstable_by_key(|(k, _)| *k);
        entries_vec.dedup_by_key(|(k, _)| *k);

        let new_root = Self::recursive_batch_upsert(self, self.root, entries_vec, false);
        self.root = new_root;
        self.db
            .insert(ROOT_KEY, encode_prefix(self.root))
            .expect("Failed to update root in DB");
    }

    fn batch_upsert_memory_only(&mut self, entries: &[(Hash, Hash)]) {
        if entries.is_empty() {
            return;
        }

        let mut entries_vec: Vec<(Hash, Hash)> = entries.to_vec();
        entries_vec.sort_unstable_by_key(|(k, _)| *k);
        entries_vec.dedup_by_key(|(k, _)| *k);

        let new_root = Self::recursive_batch_upsert(self, self.root, entries_vec, true);
        self.root = new_root;
    }

    fn recursive_batch_upsert(
        &self,
        current_prefix: Prefix,
        entries: Vec<(Hash, Hash)>,
        memory_only: bool,
    ) -> Prefix {
        if entries.is_empty() {
            return current_prefix;
        }

        let node = self
            .store
            .get(&current_prefix)
            .map(|guard| guard.value().clone());
        let Some(node) = node else {
            return Self::batch_insert_into_empty(self, entries, memory_only);
        };

        match node {
            Node::Leaf(leaf) => {
                Self::batch_upsert_at_leaf(self, current_prefix, leaf, entries, memory_only)
            }
            Node::Interior(interior) => {
                Self::batch_upsert_at_interior(self, current_prefix, interior, entries, memory_only)
            }
        }
    }

    fn batch_insert_into_empty(&self, mut entries: Vec<(Hash, Hash)>, memory_only: bool) -> Prefix {
        if entries.is_empty() {
            return Prefix::root();
        }

        let (first_key, first_value) = entries.remove(0);
        let first_prefix = Prefix::from(first_key);
        let first_leaf = LeafNode::new(first_key, first_value);
        if memory_only {
            self.insert_node_memory_only(first_prefix, Node::Leaf(first_leaf));
        } else {
            self.insert_node_with_db(first_prefix, Node::Leaf(first_leaf));
        }
        Self::recursive_batch_upsert(self, first_prefix, entries, memory_only)
    }

    fn batch_upsert_at_leaf(
        &self,
        leaf_prefix: Prefix,
        leaf: LeafNode,
        mut entries: Vec<(Hash, Hash)>,
        memory_only: bool,
    ) -> Prefix {
        if let Ok(idx) = entries.binary_search_by_key(&leaf_prefix.hash, |(k, _)| *k) {
            let (_, new_value) = entries.remove(idx);
            let updated_leaf = LeafNode::new(leaf_prefix.hash, new_value);
            if memory_only {
                self.insert_node_memory_only(leaf_prefix, Node::Leaf(updated_leaf));
            } else {
                self.insert_node_with_db(leaf_prefix, Node::Leaf(updated_leaf));
            }
            if entries.is_empty() {
                return leaf_prefix;
            }
            return Self::recursive_batch_upsert(self, leaf_prefix, entries, memory_only);
        }

        if entries.is_empty() {
            return leaf_prefix;
        }

        let (first_key, first_value) = entries.remove(0);

        let new_leaf = LeafNode::new(first_key, first_value);
        let new_prefix = Prefix::from(first_key);
        let existing_prefix = leaf_prefix;
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

        if memory_only {
            self.insert_node_memory_only(merged_prefix, Node::Interior(new_interior));
            self.insert_node_memory_only(existing_prefix, Node::Leaf(leaf));
            self.insert_node_memory_only(new_prefix, Node::Leaf(new_leaf));
        } else {
            self.insert_node_memory_only(merged_prefix, Node::Interior(new_interior));
            self.insert_node_with_db(existing_prefix, Node::Leaf(leaf));
            self.insert_node_with_db(new_prefix, Node::Leaf(new_leaf));
        }

        Self::recursive_batch_upsert(self, merged_prefix, entries, memory_only)
    }

    fn batch_upsert_at_interior(
        &self,
        interior_prefix: Prefix,
        interior: InteriorNode,
        entries: Vec<(Hash, Hash)>,
        memory_only: bool,
    ) -> Prefix {
        let (mut contained_entries, mut divergent_entries) : (Vec<(Hash, Hash)>, Vec<(Hash, Hash)>) = entries
            .into_iter()
            .partition(|(k, v)| interior_prefix.contains(&k));

        if !divergent_entries.is_empty() {
            let (first_key, first_value) = divergent_entries.remove(0);

            let new_leaf = LeafNode::new(first_key, first_value);
            let new_leaf_prefix = Prefix::from(first_key);
            let common = Prefix::common_prefix(&interior_prefix, &new_leaf_prefix);

            let (left_prefix, right_prefix, left_hash, right_hash) = Self::order_children(
                &common,
                first_key,
                new_leaf_prefix,
                new_leaf.merkle_hash,
                interior_prefix,
                interior.merkle_hash,
            );

            let new_interior =
                InteriorNode::new(common, left_prefix, right_prefix, left_hash, right_hash);

            if memory_only {
                self.insert_node_memory_only(common, Node::Interior(new_interior));
                self.insert_node_memory_only(new_leaf_prefix, Node::Leaf(new_leaf));
            } else {
                self.insert_node_memory_only(common, Node::Interior(new_interior));
                self.insert_node_with_db(new_leaf_prefix, Node::Leaf(new_leaf));
            }

            contained_entries.extend(divergent_entries);
            return Self::recursive_batch_upsert(self, common, contained_entries, memory_only);
        }
        let (right_entries, left_entries) : (Vec<(Hash, Hash)>, Vec<(Hash, Hash)>) = contained_entries
            .into_iter()
            .partition(|(k, v)| interior_prefix.key_goes_right(*k));

        let count = left_entries.len() + right_entries.len();
        let l_work = || {
            if !left_entries.is_empty() {
                Self::recursive_batch_upsert(self, interior.left, left_entries, memory_only)
            } else {
                interior.left
            }
        };
        let r_work = || {
            if !right_entries.is_empty() {
                Self::recursive_batch_upsert(self, interior.right, right_entries, memory_only)
            } else {
                interior.right
            }
        };

        let (new_left, new_right) = if count > 1024 {
            join(l_work, r_work)
        } else {
            (l_work(), r_work())
        };

        let left_hash = self
            .store
            .get(&new_left)
            .expect("Left child missing after batch upsert")
            .merkle_hash();
        let right_hash = self
            .store
            .get(&new_right)
            .expect("Right child missing after batch upsert")
            .merkle_hash();

        let updated_interior =
            InteriorNode::new(interior_prefix, new_left, new_right, left_hash, right_hash);

        self.insert_node_memory_only(interior_prefix, Node::Interior(updated_interior));
        interior_prefix
    }

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

    pub fn flush(&mut self) -> sled::Result<()> {
        self.db.flush()?;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.count.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl MerklePatriciaTree for SledLeafMPT {
    fn new() -> Self {
        Self::new_temporary().expect("Failed to create temporary sled database")
    }

    fn upsert(&mut self, key: Hash, value: Hash) {
        self.batch_upsert(&[(key, value)]);
    }

    fn batch_upsert(&mut self, entries: &[(Hash, Hash)]) {
        self.batch_upsert_optimized(entries);
    }

    fn enumerate_nodes(&self) -> Vec<(Prefix, Node)> {
        self.store
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect()
    }

    fn get_root_hash(&self) -> Option<Hash> {
        self.store
            .get(&self.root)
            .map(|node| node.value().merkle_hash())
    }

    fn get_leaf_value(&self, key: Hash) -> Option<Hash> {
        let prefix = Prefix::from(key);
        match self.store.get(&prefix) {
            Some(node) => match node.value() {
                Node::Leaf(leaf) => Some(leaf.value),
                _ => None,
            },
            None => None,
        }
    }
}

fn prefix_key(prefix: &Prefix) -> Vec<u8> {
    let mut key = Vec::with_capacity(34);
    let mut temp = prefix.hash;
    temp.reverse();
    key.extend_from_slice(&temp);
    key.extend_from_slice(&prefix.length.to_be_bytes());
    key
}

fn encode_node(node: &Node) -> Vec<u8> {
    let (node_type, data) = node.serialize().expect("Failed to serialize node");
    let mut encoded = Vec::with_capacity(1 + data.len());
    let discriminator = match node_type {
        "leaf" => 0u8,
        "interior" => 1u8,
        _ => panic!("Unexpected node type {}", node_type),
    };
    encoded.push(discriminator);
    encoded.extend_from_slice(&data);
    encoded
}

fn decode_node(bytes: &[u8]) -> Result<Node, String> {
    if bytes.is_empty() {
        return Err("Node bytes are empty".into());
    }
    let (tag, data) = bytes.split_first().unwrap();
    let node_type = match tag {
        0 => "leaf",
        1 => "interior",
        other => return Err(format!("Unknown node tag {}", other)),
    };
    Node::deserialize(node_type, data)
}

fn encode_prefix(mut prefix: Prefix) -> Vec<u8> {
    let mut buf = Vec::with_capacity(34);
    prefix.hash.reverse();
    buf.extend_from_slice(&prefix.hash);
    buf.extend_from_slice(&prefix.length.to_be_bytes());
    buf
}

fn decode_prefix(bytes: &[u8]) -> Result<Prefix, String> {
    if bytes.len() != 34 {
        return Err(format!("Prefix bytes must be 34 long, got {}", bytes.len()));
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&bytes[..32]);
    let length = u16::from_be_bytes([bytes[32], bytes[33]]);
    Ok(Prefix { hash, length })
}

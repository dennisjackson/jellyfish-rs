use dashmap::{DashMap, DashSet};
use log::info;
use rayon::join;
use sled::{Batch, Config, Db};
use std::path::Path;

use crate::mpt::MerklePatriciaTree;
use crate::{Hash, Prefix};

use super::{InteriorNode, LeafNode, Node};

const ROOT_KEY: &[u8] = b"__mpt_root__";

pub struct SledBatchMPT {
    db: Db,
    store: DashMap<Prefix, Node>,
    dirty: DashSet<Prefix>,
    old_dirty: DashSet<Prefix>,
    root: Prefix,
    root_dirty: bool,
}

impl SledBatchMPT {
    fn get_default_config() -> Config {
        Config::new()
            .flush_every_ms(Some(5000))
            .mode(sled::Mode::HighThroughput)
        .use_compression(false)
        .print_profile_on_drop(true)
    }

    pub fn new_with_path(path: impl AsRef<Path>) -> sled::Result<Self> {
        let db = SledBatchMPT::get_default_config().path(path).open()?;
        Self::from_db(db)
    }

    pub fn new_temporary() -> sled::Result<Self> {
        let db = SledBatchMPT::get_default_config().temporary(true).open()?;
        Self::from_db(db)
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

        let store = DashMap::new();
        for entry in db.iter() {
            let (key, value) = entry?;
            if key.as_ref() == ROOT_KEY {
                continue;
            }
            let prefix = decode_prefix(key.as_ref()).map_err(sled::Error::Unsupported)?;
            let node = decode_node(value.as_ref()).map_err(sled::Error::Unsupported)?;
            store.insert(prefix, node);
        }

        Ok(Self {
            db,
            store,
            dirty: DashSet::new(),
            old_dirty: DashSet::new(),
            root,
            root_dirty: false,
        })
    }

    fn batch_upsert_optimized(&mut self, entries: &[(Hash, Hash)]) {
        if entries.is_empty() {
            return;
        }

        let mut entries_vec: Vec<(Hash, Hash)> = entries.to_vec();
        entries_vec.sort_unstable_by_key(|(k, _)| *k);
        entries_vec.dedup_by_key(|(k, _)| *k);

        let new_root =
            Self::recursive_batch_upsert(&self.store, &self.dirty, self.root, entries_vec);
        let root_changed = new_root != self.root;
        self.root = new_root;
        if root_changed {
            self.root_dirty = true;
        }

        self.persist_dirty()
            .expect("Failed to persist sled batch updates");
    }

    fn recursive_batch_upsert(
        store: &DashMap<Prefix, Node>,
        dirty: &DashSet<Prefix>,
        current_prefix: Prefix,
        entries: Vec<(Hash, Hash)>,
    ) -> Prefix {
        if entries.is_empty() {
            return current_prefix;
        }

        let node = store
            .get(&current_prefix)
            .map(|guard| guard.value().clone());
        let Some(node) = node else {
            return Self::batch_insert_into_empty(store, dirty, entries);
        };

        match node {
            Node::Leaf(leaf) => Self::batch_upsert_at_leaf(store, dirty, leaf, entries),
            Node::Interior(interior) => {
                Self::batch_upsert_at_interior(store, dirty, interior, entries)
            }
        }
    }

    fn batch_insert_into_empty(
        store: &DashMap<Prefix, Node>,
        dirty: &DashSet<Prefix>,
        mut entries: Vec<(Hash, Hash)>,
    ) -> Prefix {
        if entries.is_empty() {
            return Prefix::root();
        }

        let (first_key, first_value) = entries.remove(0);
        let first_prefix = Prefix::from(first_key);
        let first_leaf = LeafNode::new(first_key, first_value);
        set_node(store, dirty, first_prefix, Node::Leaf(first_leaf));

        Self::recursive_batch_upsert(store, dirty, first_prefix, entries)
    }

    fn batch_upsert_at_leaf(
        store: &DashMap<Prefix, Node>,
        dirty: &DashSet<Prefix>,
        leaf: LeafNode,
        mut entries: Vec<(Hash, Hash)>,
    ) -> Prefix {
        let leaf_prefix = Prefix::from(leaf.key);

        if let Ok(idx) = entries.binary_search_by_key(&leaf.key, |(k, _)| *k) {
            let (_, new_value) = entries.remove(idx);
            let updated_leaf = LeafNode::new(leaf.key, new_value);
            set_node(store, dirty, leaf_prefix, Node::Leaf(updated_leaf));

            if entries.is_empty() {
                return leaf_prefix;
            }
            return Self::recursive_batch_upsert(store, dirty, leaf_prefix, entries);
        }

        if entries.is_empty() {
            return leaf_prefix;
        }

        let (first_key, first_value) = entries.remove(0);

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

        set_node(store, dirty, merged_prefix, Node::Interior(new_interior));
        set_node(store, dirty, existing_prefix, Node::Leaf(leaf));
        set_node(store, dirty, new_prefix, Node::Leaf(new_leaf));

        Self::recursive_batch_upsert(store, dirty, merged_prefix, entries)
    }

    fn batch_upsert_at_interior(
        store: &DashMap<Prefix, Node>,
        dirty: &DashSet<Prefix>,
        interior: InteriorNode,
        entries: Vec<(Hash, Hash)>,
    ) -> Prefix {
        let mut contained_entries = Vec::new();
        let mut divergent_entries = Vec::new();

        for &(key, value) in entries.iter() {
            if interior.prefix.contains(&key) {
                contained_entries.push((key, value));
            } else {
                divergent_entries.push((key, value));
            }
        }

        if !divergent_entries.is_empty() {
            let (first_key, first_value) = divergent_entries.remove(0);

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

            set_node(store, dirty, common, Node::Interior(new_interior));
            set_node(store, dirty, new_leaf_prefix, Node::Leaf(new_leaf));

            contained_entries.extend(divergent_entries);
            return Self::recursive_batch_upsert(store, dirty, common, contained_entries);
        }

        let mut left_entries = Vec::new();
        let mut right_entries = Vec::new();

        for &(key, value) in contained_entries.iter() {
            if interior.prefix.key_goes_right(key) {
                right_entries.push((key, value));
            } else {
                left_entries.push((key, value));
            }
        }

        let (new_left, new_right) = join(
            || {
                if !left_entries.is_empty() {
                    Self::recursive_batch_upsert(store, dirty, interior.left, left_entries)
                } else {
                    interior.left
                }
            },
            || {
                if !right_entries.is_empty() {
                    Self::recursive_batch_upsert(store, dirty, interior.right, right_entries)
                } else {
                    interior.right
                }
            },
        );

        let left_hash = store
            .get(&new_left)
            .expect("Left child missing after batch upsert")
            .merkle_hash();
        let right_hash = store
            .get(&new_right)
            .expect("Right child missing after batch upsert")
            .merkle_hash();

        let updated_interior =
            InteriorNode::new(interior.prefix, new_left, new_right, left_hash, right_hash);

        set_node(
            store,
            dirty,
            interior.prefix,
            Node::Interior(updated_interior),
        );
        interior.prefix
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

    fn persist_dirty(&mut self) -> sled::Result<()> {
        if self.dirty.is_empty() && !self.root_dirty {
            return Ok(());
        }

        std::mem::swap(&mut self.dirty, &mut self.old_dirty);

        let mut wrote_nodes = false;
        let mut batch = Batch::default();
        for prefix in self.dirty.iter() {
            if let Some(node) = self.store.get(&prefix) {
                batch.insert(prefix_key(&prefix), encode_node(node.value()));
                wrote_nodes = true;
            }
        }
        self.dirty.clear();

        if wrote_nodes || self.root_dirty {
            batch.insert(ROOT_KEY, encode_prefix(self.root));
            self.root_dirty = false;
            self.db.apply_batch(batch)?;
            // self.db.flush()?;
        }

        Ok(())
    }
}

impl MerklePatriciaTree for SledBatchMPT {
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

fn set_node(store: &DashMap<Prefix, Node>, dirty: &DashSet<Prefix>, prefix: Prefix, node: Node) {
    store.insert(prefix, node);
    dirty.insert(prefix);
}

fn prefix_key(prefix: &Prefix) -> Vec<u8> {
    let mut key = Vec::with_capacity(34);
    key.extend_from_slice(&prefix.hash);
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

fn encode_prefix(prefix: Prefix) -> Vec<u8> {
    let mut buf = Vec::with_capacity(34);
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

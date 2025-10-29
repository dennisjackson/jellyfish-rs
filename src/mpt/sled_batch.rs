use dashmap::{DashMap, DashSet};
use log::debug;
use rayon::join;
use std::path::Path;
use std::sync::OnceLock;

use crate::mpt::MerklePatriciaTree;
use crate::{Hash, Prefix};

use super::sled_storage::SledStorage;
use super::{InteriorNode, LeafNode, Node};

pub struct SledBatchMPT {
    storage: SledStorage,
    store: DashMap<Prefix, Node>,
    dirty: DashSet<Prefix>,
    // old_dirty: DashSet<Prefix>,
    root: Prefix,
    root_dirty: bool,
}

fn get_thread_pool() -> &'static rayon::ThreadPool {
    static THREAD_POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    THREAD_POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .expect("Failed to create thread pool")
    })
}

impl SledBatchMPT {
    pub fn new_with_path(path: impl AsRef<Path>) -> sled::Result<Self> {
        let storage = SledStorage::new_with_path(path)?;
        Self::from_storage(storage)
    }

    pub fn new_temporary() -> sled::Result<Self> {
        let storage = SledStorage::new_temporary()?;
        Self::from_storage(storage)
    }

    fn from_storage(storage: SledStorage) -> sled::Result<Self> {
        let root = storage.load_root()?;

        let store = DashMap::new();
        for entry in storage.iter_nodes() {
            let (prefix, node) = entry?;
            store.insert(prefix, node);
        }

        Ok(Self {
            storage,
            store,
            dirty: DashSet::new(),
            // old_dirty: DashSet::new(),
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

        let new_root = get_thread_pool().install(|| {
            Self::recursive_batch_upsert(&self.store, &self.dirty, self.root, entries_vec)
        });
        let root_changed = new_root != self.root;
        self.root = new_root;
        if root_changed {
            self.root_dirty = true;
        }
        debug!("self.dirty.len() = {}", self.dirty.len());
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
            Node::Leaf(leaf) => {
                Self::batch_upsert_at_leaf(store, dirty, current_prefix, leaf, entries)
            }
            Node::Interior(interior) => {
                Self::batch_upsert_at_interior(store, dirty, current_prefix, interior, entries)
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
        leaf_prefix: Prefix,
        leaf: LeafNode,
        mut entries: Vec<(Hash, Hash)>,
    ) -> Prefix {
        if let Ok(idx) = entries.binary_search_by_key(&leaf_prefix.hash, |(k, _)| *k) {
            let (_, new_value) = entries.remove(idx);
            let updated_leaf = LeafNode::new(leaf_prefix.hash, new_value);
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

        set_node(store, dirty, merged_prefix, Node::Interior(new_interior));
        set_node(store, dirty, existing_prefix, Node::Leaf(leaf));
        set_node(store, dirty, new_prefix, Node::Leaf(new_leaf));

        Self::recursive_batch_upsert(store, dirty, merged_prefix, entries)
    }

    fn batch_upsert_at_interior(
        store: &DashMap<Prefix, Node>,
        dirty: &DashSet<Prefix>,
        interior_prefix: Prefix,
        interior: InteriorNode,
        entries: Vec<(Hash, Hash)>,
    ) -> Prefix {
        let mut contained_entries = Vec::new();
        let mut divergent_entries = Vec::new();

        for &(key, value) in entries.iter() {
            if interior_prefix.contains(&key) {
                contained_entries.push((key, value));
            } else {
                divergent_entries.push((key, value));
            }
        }

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

            set_node(store, dirty, common, Node::Interior(new_interior));
            set_node(store, dirty, new_leaf_prefix, Node::Leaf(new_leaf));

            contained_entries.extend(divergent_entries);
            return Self::recursive_batch_upsert(store, dirty, common, contained_entries);
        }

        let mut left_entries = Vec::new();
        let mut right_entries = Vec::new();

        for &(key, value) in contained_entries.iter() {
            if interior_prefix.key_goes_right(key) {
                right_entries.push((key, value));
            } else {
                left_entries.push((key, value));
            }
        }

        let count = left_entries.len() + right_entries.len();
        let lf = || {
            if !left_entries.is_empty() {
                Self::recursive_batch_upsert(store, dirty, interior.left, left_entries)
            } else {
                interior.left
            }
        };
        let rf = || {
            if !right_entries.is_empty() {
                Self::recursive_batch_upsert(store, dirty, interior.right, right_entries)
            } else {
                interior.right
            }
        };

        let (new_left, new_right) = if count > 128 {
            join(lf, rf)
        } else {
            (lf(), rf())
        };

        let left_hash = store
            .get(&new_left)
            .expect("Left child missing after batch upsert")
            .merkle_hash();
        let right_hash = store
            .get(&new_right)
            .expect("Right child missing after batch upsert")
            .merkle_hash();

        let updated_interior =
            InteriorNode::new(interior_prefix, new_left, new_right, left_hash, right_hash);

        set_node(
            store,
            dirty,
            interior_prefix,
            Node::Interior(updated_interior),
        );
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

    fn persist_dirty(&mut self) -> sled::Result<()> {
        if self.dirty.is_empty() && !self.root_dirty {
            debug!("No dirty nodes to persist");
            return Ok(());
        }

        // std::mem::swap(&mut self.dirty, &mut self.old_dirty);

        let mut batch = self.storage.start_batch();
        for prefix in self.dirty.iter() {
            let prefix = *prefix;
            let node = self.store.get(&prefix).unwrap();
            batch.insert_node(prefix, node.value())?;
        }
        debug!("Persisted {} dirty nodes", self.dirty.len());
        if self.root_dirty {
            batch.set_root(self.root);
        }
        batch.commit()?;

        self.dirty.clear();
        // self.old_dirty.clear();
        self.root_dirty = false;

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
    // debug!("Inserting node {:?} at prefix {:?}", node, prefix);
    store.insert(prefix, node);
    dirty.insert(prefix);
}

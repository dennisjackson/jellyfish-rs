use async_recursion::async_recursion;
use std::path::Path;
use std::sync::OnceLock;
use tokio::runtime::{Builder, Runtime};

use crate::mpt::MerklePatriciaTree;
use crate::{Hash, Prefix};

use super::sled_storage::SledStorage;
use super::{InteriorNode, LeafNode, Node};

fn mpt_runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(12)
            .thread_name("mpt-worker")
            .build()
            .expect("Failed to build MPT async runtime")
    })
}

#[derive(Clone)]
pub struct SledAllMPT {
    storage: SledStorage,
    root: Prefix,
}

impl SledAllMPT {
    pub fn new_with_path(path: impl AsRef<Path>) -> sled::Result<Self> {
        let storage = SledStorage::new_with_path(path)?;
        Self::from_storage(storage)
    }

    pub fn new_temporary() -> sled::Result<Self> {
        let storage = SledStorage::new_temporary()?;
        Self::from_storage(storage)
    }

    fn insert_node_with_db(&self, prefix: Prefix, node: Node) {
        self.storage
            .put_node(prefix, &node)
            .expect("DB insert failed");
    }

    fn get_node(&self, prefix: &Prefix) -> Option<Node> {
        self.storage.fetch_node(prefix).expect("DB get failed")
    }

    fn from_storage(storage: SledStorage) -> sled::Result<Self> {
        let root = storage.load_root()?;
        Ok(Self { storage, root })
    }

    fn batch_upsert_optimized(&mut self, entries: &[(Hash, Hash)]) {
        if entries.is_empty() {
            return;
        }

        let mut entries_vec: Vec<(Hash, Hash)> = entries.to_vec();
        entries_vec.sort_unstable_by_key(|(k, _)| *k);
        entries_vec.dedup_by_key(|(k, _)| *k);

        let root = self.root;
        let new_root = mpt_runtime()
            .block_on(async { Self::recursive_batch_upsert(self, root, entries_vec).await });
        self.root = new_root;
        self.storage
            .persist_root(self.root)
            .expect("Failed to update root in DB");
    }

    #[async_recursion]
    async fn recursive_batch_upsert(
        &self,
        current_prefix: Prefix,
        entries: Vec<(Hash, Hash)>,
    ) -> Prefix {
        if entries.is_empty() {
            return current_prefix;
        }

        let node = self.get_node(&current_prefix);
        let Some(node) = node else {
            return Self::batch_insert_into_empty(self, entries).await;
        };

        match node {
            Node::Leaf(leaf) => {
                Self::batch_upsert_at_leaf(self, current_prefix, leaf, entries).await
            }
            Node::Interior(interior) => {
                Self::batch_upsert_at_interior(self, current_prefix, interior, entries).await
            }
        }
    }

    async fn batch_insert_into_empty(&self, mut entries: Vec<(Hash, Hash)>) -> Prefix {
        if entries.is_empty() {
            return Prefix::root();
        }

        let (first_key, first_value) = entries.remove(0);
        let first_prefix = Prefix::from(first_key);
        let first_leaf = LeafNode::new(first_key, first_value);
        self.insert_node_with_db(first_prefix, Node::Leaf(first_leaf));
        Self::recursive_batch_upsert(self, first_prefix, entries).await
    }

    async fn batch_upsert_at_leaf(
        &self,
        leaf_prefix: Prefix,
        leaf: LeafNode,
        mut entries: Vec<(Hash, Hash)>,
    ) -> Prefix {
        if let Ok(idx) = entries.binary_search_by_key(&leaf_prefix.hash, |(k, _)| *k) {
            let (_, new_value) = entries.remove(idx);
            let updated_leaf = LeafNode::new(leaf_prefix.hash, new_value);
            self.insert_node_with_db(leaf_prefix, Node::Leaf(updated_leaf));
            if entries.is_empty() {
                return leaf_prefix;
            }
            return Self::recursive_batch_upsert(self, leaf_prefix, entries).await;
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

        self.insert_node_with_db(merged_prefix, Node::Interior(new_interior));
        self.insert_node_with_db(existing_prefix, Node::Leaf(leaf));
        self.insert_node_with_db(new_prefix, Node::Leaf(new_leaf));

        Self::recursive_batch_upsert(self, merged_prefix, entries).await
    }

    async fn batch_upsert_at_interior(
        &self,
        interior_prefix: Prefix,
        interior: InteriorNode,
        entries: Vec<(Hash, Hash)>,
    ) -> Prefix {
        let (mut contained_entries, mut divergent_entries): (Vec<(Hash, Hash)>, Vec<(Hash, Hash)>) =
            entries
                .into_iter()
                .partition(|(k, _)| interior_prefix.contains(k));

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

            self.insert_node_with_db(common, Node::Interior(new_interior));
            self.insert_node_with_db(new_leaf_prefix, Node::Leaf(new_leaf));

            contained_entries.extend(divergent_entries);
            return Self::recursive_batch_upsert(self, common, contained_entries).await;
        }
        let (right_entries, left_entries): (Vec<(Hash, Hash)>, Vec<(Hash, Hash)>) =
            contained_entries
                .into_iter()
                .partition(|(k, _)| interior_prefix.key_goes_right(*k));

        let (new_left, new_right) =
            Self::process_children(self, &interior, left_entries, right_entries).await;

        let left_hash = self
            .get_node(&new_left)
            .expect("Left child missing after batch upsert")
            .merkle_hash();
        let right_hash = self
            .get_node(&new_right)
            .expect("Right child missing after batch upsert")
            .merkle_hash();

        let updated_interior =
            InteriorNode::new(interior_prefix, new_left, new_right, left_hash, right_hash);

        self.insert_node_with_db(interior_prefix, Node::Interior(updated_interior));
        interior_prefix
    }

    async fn process_children(
        &self,
        interior: &InteriorNode,
        left_entries: Vec<(Hash, Hash)>,
        right_entries: Vec<(Hash, Hash)>,
    ) -> (Prefix, Prefix) {
        let left_is_empty = left_entries.is_empty();
        let right_is_empty = right_entries.is_empty();
        let count = left_entries.len() + right_entries.len();

        if count > 128 {
            let left_prefix = interior.left;
            let right_prefix = interior.right;

            let left_handle = if !left_is_empty {
                let entries = left_entries;
                let worker = self.clone();
                Some(tokio::spawn(async move {
                    worker.recursive_batch_upsert(left_prefix, entries).await
                }))
            } else {
                None
            };

            let right_handle = if !right_is_empty {
                let entries = right_entries;
                let worker = self.clone();
                Some(tokio::spawn(async move {
                    worker.recursive_batch_upsert(right_prefix, entries).await
                }))
            } else {
                None
            };

            let new_left = match left_handle {
                Some(handle) => handle
                    .await
                    .expect("Left branch task panicked during async upsert"),
                None => left_prefix,
            };
            let new_right = match right_handle {
                Some(handle) => handle
                    .await
                    .expect("Right branch task panicked during async upsert"),
                None => right_prefix,
            };

            (new_left, new_right)
        } else {
            let new_left = if !left_is_empty {
                Self::recursive_batch_upsert(self, interior.left, left_entries).await
            } else {
                interior.left
            };
            let new_right = if !right_is_empty {
                Self::recursive_batch_upsert(self, interior.right, right_entries).await
            } else {
                interior.right
            };
            (new_left, new_right)
        }
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
        self.storage.persist_leaf_count()?;
        self.storage.flush()
    }

    pub fn len(&self) -> usize {
        self.storage.leaf_count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl MerklePatriciaTree for SledAllMPT {
    fn new() -> Self {
        Self::new_temporary().expect("Failed to create temporary sled database")
    }

    fn upsert(&mut self, key: Hash, value: Hash) {
        self.batch_upsert(&[(key, value)]);
    }

    fn batch_upsert(&mut self, entries: &[(Hash, Hash)]) {
        self.batch_upsert_optimized(entries);
        self.flush().expect("Failed to flush DB after batch upsert");
    }

    fn enumerate_nodes(&self) -> Vec<(Prefix, Node)> {
        self.storage
            .enumerate_nodes()
            .expect("Failed to enumerate nodes from DB")
    }

    fn get_root_hash(&self) -> Option<Hash> {
        self.get_node(&self.root).map(|node| node.merkle_hash())
    }

    fn get_leaf_value(&self, key: Hash) -> Option<Hash> {
        let prefix = Prefix::from(key);
        match self.get_node(&prefix) {
            Some(Node::Leaf(leaf)) => Some(leaf.value),
            Some(Node::Interior(_)) => panic!("Expected leaf node"),
            None => None,
        }
    }
}

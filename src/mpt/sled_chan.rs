use async_recursion::async_recursion;
use dashmap::DashMap;
use sled::{Config, Db};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use tokio::runtime::{Builder, Runtime};

use crate::mpt::MerklePatriciaTree;
use crate::{Hash, Prefix};

use super::{InteriorNode, LeafNode, Node};

const ROOT_KEY: &[u8] = b"__mpt_root__";
const WORKER_CHANNEL_CAPACITY: usize = 400_000;

enum WorkerCommand {
    Insert { prefix: Prefix, node: Node },
    SetRoot { root: Prefix },
    Commit { respond_to: mpsc::Sender<sled::Result<()>> },
    Shutdown,
}

struct WorkerHandle {
    sender: SyncSender<WorkerCommand>,
    join: Mutex<Option<thread::JoinHandle<()>>>,
}

impl WorkerHandle {
    fn spawn(db: Db) -> Arc<Self> {
        let (sender, receiver) = mpsc::sync_channel(WORKER_CHANNEL_CAPACITY);
        let join = thread::spawn(move || worker_loop(db, receiver));
        Arc::new(Self {
            sender,
            join: Mutex::new(Some(join)),
        })
    }

    fn send(&self, command: WorkerCommand) {
        self.sender
            .send(command)
            .expect("sled worker thread terminated unexpectedly");
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        // Best-effort shutdown; if the worker is already gone we still attempt to join.
        let _ = self.sender.send(WorkerCommand::Shutdown);
        if let Ok(mut guard) = self.join.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
}

fn worker_loop(db: Db, receiver: Receiver<WorkerCommand>) {
    let mut batch = sled::Batch::default();
    for command in receiver {
        match command {
            WorkerCommand::Insert { prefix, node } => {
                batch.insert(prefix_key(&prefix), encode_node(&node));
            }
            WorkerCommand::SetRoot { root } => {
                batch.insert(ROOT_KEY, encode_prefix(root));
            }
            WorkerCommand::Commit { respond_to } => {
                let result = db.apply_batch(batch);
                batch = sled::Batch::default();
                let _ = respond_to.send(result);
            }
            WorkerCommand::Shutdown => {
                let _ = db.flush();
                break;
            }
        }
    }
}

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
pub struct SledChanMPT {
    db: Db,
    root: Prefix,
    worker: Arc<WorkerHandle>,
    cache: Arc<DashMap<Prefix, Node>>,
}

impl SledChanMPT {
    fn get_default_config() -> Config {
        Config::new()
            .flush_every_ms(Some(5000))
            .mode(sled::Mode::HighThroughput)
            .use_compression(false)
            .print_profile_on_drop(true)
            .cache_capacity(1024 * 1024 * 1024 * 10) // 8 GB
    }

    pub fn new_with_path(path: impl AsRef<Path>) -> sled::Result<Self> {
        let db = SledChanMPT::get_default_config().path(path).open()?;
        Self::from_db(db)
    }

    pub fn new_temporary() -> sled::Result<Self> {
        let db = SledChanMPT::get_default_config().temporary(true).open()?;
        Self::from_db(db)
    }

    fn queue_node(&self, prefix: Prefix, node: Node) {
        self.cache.insert(prefix.clone(), node.clone());
        self.worker
            .send(WorkerCommand::Insert { prefix, node });
    }

    fn queue_root_update(&self, root: Prefix) {
        self.worker.send(WorkerCommand::SetRoot { root });
    }

    fn get_node(&self, prefix: &Prefix) -> Option<Node> {
        if let Some(entry) = self.cache.get(prefix) {
            return Some(entry.value().clone());
        }
        self.db
            .get(prefix_key(prefix))
            .expect("DB get failed")
            .map(|raw| decode_node(raw.as_ref()).expect("Failed to decode node from DB"))
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
        let worker = WorkerHandle::spawn(db.clone());
        let cache = Arc::new(DashMap::new());
        let instance = Self {
            db,
            root,
            worker,
            cache,
        };

        Ok(instance)
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
        self.queue_root_update(self.root);
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
        self.queue_node(first_prefix, Node::Leaf(first_leaf));
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
            self.queue_node(leaf_prefix, Node::Leaf(updated_leaf));
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

        self.queue_node(merged_prefix, Node::Interior(new_interior));
        self.queue_node(existing_prefix, Node::Leaf(leaf));
        self.queue_node(new_prefix, Node::Leaf(new_leaf));

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

            self.queue_node(common, Node::Interior(new_interior));
            self.queue_node(new_leaf_prefix, Node::Leaf(new_leaf));

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

        self.queue_node(interior_prefix, Node::Interior(updated_interior));
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
        let (respond_to, wait_for) = mpsc::channel();
        self.worker
            .send(WorkerCommand::Commit { respond_to });
        let result = wait_for
            .recv()
            .expect("Commit acknowledgment channel closed unexpectedly");
        if result.is_ok() {
            self.cache.clear();
        }
        result
    }

    pub fn len(&self) -> usize {
        0 //todo
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl MerklePatriciaTree for SledChanMPT {
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
        let mut nodes = Vec::new();
        for result in self.db.iter() {
            let (raw_key, raw_value) = result.expect("DB iteration failed");
            if raw_key.as_ref() == ROOT_KEY {
                continue;
            }
            let prefix = decode_prefix(raw_key.as_ref()).expect("Failed to decode prefix from DB");
            let node = decode_node(raw_value.as_ref()).expect("Failed to decode node from DB");
            nodes.push((prefix, node));
        }
        nodes
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
    hash.reverse();
    let length = u16::from_be_bytes([bytes[32], bytes[33]]);
    Ok(Prefix { hash, length })
}

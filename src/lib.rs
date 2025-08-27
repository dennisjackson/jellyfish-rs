use core::panic::PanicMessage;
use std::collections::{HashMap, HashSet};
use serde::{Deserialize, Serialize};

const HASH_LENGTH : usize = 32;

trait Storage {
    fn begin_transaction(&mut self);
    fn commit_transaction(&mut self);
    // Probably want to adjust this to handle the various types, rather than just raw values.
    fn serving_read_batch(&self, keys: &[MPTKey]) -> Vec<MPTNode>;
    fn witness_read_batch(&self, keys: &[MPTKey]) -> Vec<MPTNode>;
    fn update_read_batch(&self, keys: &[MPTKey]) -> Vec<MPTNode>;
    fn write_batch(&mut self, node_changes: &[MPTNode]);
    fn get_epoch(&self) -> u64;
    fn increment_epoch(&mut self);
    fn get_tree_size(&self) -> u64;
}

struct InMemoryStorage {
    serving_kv_store: HashMap<MPTKey, MPTNode>,
    witness_kv_store: HashMap<MPTKey, MPTNode>,
    update_kv_store: HashMap<MPTKey, MPTNode>,
    epoch: u64,
}

impl InMemoryStorage {
    fn new() -> Self {
        InMemoryStorage {
            serving_kv_store: HashMap::new(),
            witness_kv_store: HashMap::new(),
            update_kv_store: HashMap::new(),
            epoch: 0,
        }
    }
}

impl Storage for InMemoryStorage {
    fn begin_transaction(&mut self) {
        // No-op for in-memory storage
    }

    fn commit_transaction(&mut self) {
        // No-op for in-memory storage
    }

    fn serving_read_batch(&self, keys: &[MPTKey]) -> Vec<MPTNode> {
        keys.iter()
            .filter_map(|key| self.serving_kv_store.get(key).map(|&value| MPTNode { key: *key, value }))
            .collect()
    }

    fn witness_read_batch(&self, keys: &[MPTKey]) -> Vec<MPTNode> {
        keys.iter()
            .filter_map(|key| self.witness_kv_store.get(key).map(|&value| MPTNode { key: *key, value }))
            .collect()
    }

    fn update_read_batch(&self, keys: &[MPTKey]) -> Vec<MPTNode> {
        keys.iter()
            .filter_map(|key| self.update_kv_store.get(key).map(|&value| MPTNode { key: *key, value }))
            .collect()
    }

    fn write_batch(&mut self, changes: &[(MPTKey, Value)]) {
        for &(key, value) in changes {
            self.kv_store.insert(key, value);
        }
    }

    fn get_epoch(&self) -> u64 {
        self.epoch
    }

    fn increment_epoch(&mut self) {
        self.epoch += 1;
        self.serving_kv_store = self.witness_kv_store;
        self.witness_kev_store = self.update_kv_store.clone();
    }

    fn get_tree_size(&self) -> u64 {
        self.kv_store.len() as u64
    }
}

enum Action {
    Add,
    Remove,
}

type MPTKey = Hash;


struct MPTLeafNode {
   hashchain_node: HashchainNode,
   merkle_tree: MerkleTree,
}

struct MPTInteriorNode {
    left: Hash,
    right: Hash
}

enum MPTNode {
    Leaf(MPTLeafNode),
    Interior(MPTInteriorNode),
}

type Value = Vec<u8>;
type Change = (Action, MPTKey, Value); // Probably want to transmit the hash as well - for now we recalculate it. Also want to have a oneshot channel for signalling the outcome.
type Hash = [u8; HASH_LENGTH];


#[derive(Serialize, Deserialize, Debug)]
struct HashchainNode {
    value: Hash,
    next: Hash,
}

impl HashchainNode {
    fn empty(value: Hash) -> Self {
        HashchainNode { value, next : [0; HASH_LENGTH] }
    }

    fn serialize(&self) -> Vec<u8> {
        bincode::serde::encode_to_vec(self, bincode::config::standard()).expect("Failed to serialize HashchainNode")
    }

    fn deserialize(data: &[u8]) -> Result<Self, bincode::error::DecodeError> {
        bincode::serde::decode_from_slice(data, bincode::config::standard()).map(|(node, _)| node)
    }

    fn self_hash(&self) -> Hash {
        sha256::hash(&self.serialize())
    }

    fn extend(&self, value: Hash) -> Self {
        HashchainNode {
            value,
            next: self.self_hash(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct MerkleTree {
    leaves: Vec<Hash>,
}

impl MerkleTree {
    fn new() -> Self {
        MerkleTree {
            leaves: Vec::new(),
        }
    }

    fn add_leaf(&mut self, leaf: Hash) {
        self.leaves.push(leaf);
    }

    fn remove_leaf(&mut self, leaf: Hash) {
        self.leaves.retain(|&x| x != leaf);
    }

    fn root(&self) -> Hash {
        // Simplified root calculation for demonstration purposes
        if self.leaves.is_empty() {
            [0; HASH_LENGTH]
        } else {
            todo!("Implement actual Merkle root calculation")
        }
    }

    fn serialize(&self) -> Vec<u8> {
        bincode::serde::encode_to_vec(self, bincode::config::standard()).expect("Failed to serialize MerkleTree")
    }

    fn deserialize(data: &[u8]) -> Result<Self, bincode::error::DecodeError> {
        bincode::serde::decode_from_slice(data, bincode::config::standard()).map(|(tree, _)| tree)
    }
}

struct JellyfishWriter<S: Storage> {
    storage: S,
    send_channel : std::sync::mpsc::Sender<Change>,
    receive_channel: std::sync::mpsc::Receiver<Change>,
}

impl<S: Storage> JellyfishWriter<S> {
    fn new(storage: S) -> Self {
        let (send_channel, receive_channel) = std::sync::mpsc::channel();
        JellyfishTree {
            storage,
            send_channel,
            receive_channel,

        }
    }

    fn get_queue(&self) -> std::sync::mpsc::Sender<Change> {
        self.send_channel.clone()
    }

    fn update(&mut self) {
        // Drain the queue up to tolerable limit
        // Begin transaction
        // Batch read all necessary nodes
        // Calculate the insertions, level by level,
        // Batch Write
        // Commit transaction
    }
}

fn load_nodes_for_update<S: Storage>(storage: &S, keys: &[MPTKey]) -> HashMap<MPTKey, Value> {
    // This where we ought to do some optimistic loading of the full layers we need, plus a follow up to make sure we have the necessary leaves.
    let mut needed_keys = HashSet::with_capacity(keys.len()*HASH_LENGTH); // This is the maximum we might need.
    // Load the prefix trie
    for key in keys {
        for i in 0..(key.len()) {
            needed_keys.insert(key[0..i].to_vec());
        }
    let mut initial_read = storage.update_read_batch(needed_keys.iter().collect()).into_iter().collect();
    initial_read
}

fn calculate_nodes_for_update(existing_nodes: HashMap<MPTKey, Value>, changes: &[Change]) -> HashMap<MPTKey, Value> {
    // Given reads, calculating insertions:
    let mut new_values = HashMap::new();
    // * We store the full value, indexed by its sha256 hash
    for (change, key, value) in changes {
        match change {
            Action::Add => {
                let hash = sha256::hash(value);
                new_values.insert(hash, value.clone());
            }
            Action::Remove => {}
        }
    }
    let mpt_keys_to_recalculate = Vec::new();
    for (change, key, value) in changes {
        if let Some(value) = existing_nodes.get(key) {
            let node = HashchainNode::deserialize(value)
                .expect("Failed to deserialize HashchainNode");
            let mut merkle_tree = MerkleTree::deserialize(existing_nodes.get(&node.value).expect("Can't find merkle tree for hashchain head!"))
                .expect("Failed to deserialize MerkleTree");
            match change {
                Action::Add => merkle_tree.push_leaf(sha256::hash(value)),
                Action::Remove => {
                    merkle_tree.remove_leaf(sha256::hash(value));
                }
            }
            // The hashchain node points to the root of the merkle tree.
            let new_hashchain_node = node.extend(sha256::hash(merkle_tree.root()));
            // The leaf of the MPT points to the hash chain node.
            new_values.insert(key.clone(), new_hashchain_node.serialize());
            // We store the merkle tree by its root hash.
            new_values.insert(merkle_tree.root().to_vec(), merkle_tree.serialize());
            mpt_keys_to_recalculate.push(key.clone());
        } else {
            if change == Action::Remove {
                continue; // No entry to remove
            }
            let mt = MerkleTree::new();
            mt.add_leaf(sha256::hash(value));
            let new_hashchain_node = HashchainNode::empty(mt.root());
            new_values.insert(key.clone(), new_hashchain_node.serialize());
            new_values.insert(mt.root().to_vec(), mt.serialize());
            mpt_keys_to_recalculate.push(key.clone());
        }
    }
    // Now we need to go up the MPT and calculate accordingly.
    // TODO
    new_values
}

struct JellyfishReader<S: Storage> {
    storage: S,
}

impl<S: Storage> JellyfishReader<S> {
    fn new(storage: S) -> Self {
        JellyfishReader {
            storage,
        }
    }

    fn get_latest(&self, key: MPTKey) -> Vec<Value> {
        // TODO: Implement get_latest
        Vec::new()
    }

    fn get_history(&self, key: MPTKey, timestamp: u64) -> Vec<Value> {
        // TODO: Implement get_history
        Vec::new()
    }
}
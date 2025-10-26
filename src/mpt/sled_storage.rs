use std::convert::TryFrom;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use sled::{Batch, Config, Error};

use crate::Prefix;

use super::Node;

const DEFAULT_CACHE_CAPACITY: u64 = 10 * 1024 * 1024 * 1024;

pub const ROOT_KEY: &[u8] = b"__mpt_root__";
pub const LEAF_COUNT_KEY: &[u8] = b"__mpt_leaf_count__";

pub fn prefix_key(prefix: &Prefix) -> Vec<u8> {
    let mut key = Vec::with_capacity(34);
    key.extend_from_slice(&prefix.hash);
    key.extend_from_slice(&prefix.length.to_be_bytes());
    key
}

pub fn encode_prefix(prefix: Prefix) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(34);
    encode_prefix_into(prefix, &mut buffer).to_vec()
}

pub fn encode_prefix_into<'a>(prefix: Prefix, buffer: &'a mut Vec<u8>) -> &'a [u8] {
    buffer.clear();
    buffer.extend_from_slice(&prefix.hash);
    buffer.extend_from_slice(&prefix.length.to_be_bytes());
    buffer.as_slice()
}

pub fn decode_prefix(bytes: &[u8]) -> Result<Prefix, String> {
    if bytes.len() != 34 {
        return Err(format!("Prefix bytes must be 34 long, got {}", bytes.len()));
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&bytes[..32]);
    let length = u16::from_be_bytes([bytes[32], bytes[33]]);
    Ok(Prefix { hash, length })
}

pub fn encode_node(node: &Node) -> Vec<u8> {
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

pub fn decode_node(bytes: &[u8]) -> Result<Node, String> {
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

fn decode_leaf_count(bytes: &[u8]) -> Option<usize> {
    if bytes.len() != 8 {
        return None;
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(bytes);
    Some(u64::from_be_bytes(buf) as usize)
}

/// Shared helpers for interacting with sled-backed storage.
#[derive(Clone)]
pub struct SledStorage {
    db: sled::Db,
    leaf_count: Arc<AtomicUsize>,
}

impl SledStorage {
    fn storage_config() -> Config {
        Config::new()
            .flush_every_ms(Some(5000))
            .mode(sled::Mode::HighThroughput)
            .use_compression(false)
            .print_profile_on_drop(true)
            .cache_capacity(DEFAULT_CACHE_CAPACITY)
    }

    fn open_config(config: Config) -> sled::Result<Self> {
        let db = config.open()?;
        Ok(Self::new(db))
    }

    pub fn new_with_path(path: impl AsRef<Path>) -> sled::Result<Self> {
        let config = Self::storage_config().path(path);
        Self::open_config(config)
    }

    pub fn new_temporary() -> sled::Result<Self> {
        let config = Self::storage_config().temporary(true);
        Self::open_config(config)
    }

    pub fn new(db: sled::Db) -> Self {
        let initial_count = db
            .get(LEAF_COUNT_KEY)
            .expect("Failed to read persisted leaf count")
            .and_then(|raw| decode_leaf_count(raw.as_ref()))
            .unwrap_or(0);
        Self {
            db,
            leaf_count: Arc::new(AtomicUsize::new(initial_count)),
        }
    }

    pub fn start_batch(&self) -> SledWriteBatch {
        SledWriteBatch {
            storage: self.clone(),
            batch: Batch::default(),
            leaf_delta: 0,
            root_update: None,
        }
    }

    pub fn load_root(&self) -> sled::Result<Prefix> {
        match self.db.get(ROOT_KEY)? {
            Some(raw) => decode_prefix(raw.as_ref()).map_err(Error::Unsupported),
            None => {
                let root = Prefix::root();
                self.db.insert(ROOT_KEY, encode_prefix(root))?;
                self.db.flush()?;
                Ok(root)
            }
        }
    }

    pub fn persist_root(&self, root: Prefix) -> sled::Result<()> {
        self.db.insert(ROOT_KEY, encode_prefix(root))?;
        Ok(())
    }

    pub fn fetch_node(&self, prefix: &Prefix) -> sled::Result<Option<Node>> {
        let key = prefix_key(prefix);
        match self.db.get(key)? {
            Some(raw) => decode_node(raw.as_ref())
                .map(Some)
                .map_err(Error::Unsupported),
            None => Ok(None),
        }
    }

    pub fn put_node(&self, prefix: Prefix, node: &Node) -> sled::Result<()> {
        let key = prefix_key(&prefix);
        let encoded = encode_node(node);
        let previous = self.db.insert(key, encoded)?;

        let new_leaf = matches!(node, Node::Leaf(_)) && previous.is_none();

        if new_leaf {
            self.adjust_leaf_count(1);
        }

        Ok(())
    }

    pub fn iter_nodes(&self) -> SledNodeIter {
        SledNodeIter {
            iter: self.db.iter(),
        }
    }

    pub fn enumerate_nodes(&self) -> sled::Result<Vec<(Prefix, Node)>> {
        self.iter_nodes().collect::<sled::Result<Vec<_>>>()
    }

    pub fn leaf_count(&self) -> usize {
        self.leaf_count.load(Ordering::Relaxed)
    }

    fn adjust_leaf_count(&self, delta: isize) {
        if delta > 0 {
            self.leaf_count.fetch_add(delta as usize, Ordering::Relaxed);
        } else if delta < 0 {
            self.leaf_count
                .fetch_sub(delta.unsigned_abs(), Ordering::Relaxed);
        }
    }

    pub fn persist_leaf_count(&self) -> sled::Result<()> {
        let current = self.leaf_count();
        self.set_leaf_count(current)
    }

    pub fn flush(&self) -> sled::Result<()> {
        let _ = self.db.flush()?;
        Ok(())
    }

    fn set_leaf_count(&self, count: usize) -> sled::Result<()> {
        let encoded = u64::try_from(count)
            .expect("Leaf count exceeds u64 range")
            .to_be_bytes();
        self.db.insert(LEAF_COUNT_KEY, &encoded)?;
        Ok(())
    }
}

pub struct SledWriteBatch {
    storage: SledStorage,
    batch: Batch,
    leaf_delta: isize,
    root_update: Option<Prefix>,
}

impl SledWriteBatch {
    pub fn insert_node(&mut self, prefix: Prefix, node: &Node) -> sled::Result<()> {
        let key = prefix_key(&prefix);
        let is_leaf = matches!(node, Node::Leaf(_));
        self.leaf_delta += if is_leaf { 1 } else { 0 };
        let value = encode_node(node);
        self.batch.insert(key, value);
        Ok(())
    }

    pub fn set_root(&mut self, root: Prefix) {
        self.root_update = Some(root);
    }

    pub fn commit(self) -> sled::Result<()> {
        self.commit_with_flush(false)
    }

    pub fn commit_and_flush(self) -> sled::Result<()> {
        self.commit_with_flush(true)
    }

    fn commit_with_flush(mut self, flush: bool) -> sled::Result<()> {
        if let Some(root) = self.root_update {
            self.batch.insert(ROOT_KEY, encode_prefix(root));
        }
        self.storage.db.apply_batch(self.batch)?;
        if self.leaf_delta != 0 {
            self.storage.adjust_leaf_count(self.leaf_delta);
            self.storage.persist_leaf_count()?;
        }
        if flush {
            self.storage.db.flush()?;
        }
        Ok(())
    }
}

pub struct SledNodeIter {
    iter: sled::Iter,
}

impl Iterator for SledNodeIter {
    type Item = sled::Result<(Prefix, Node)>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(entry) = self.iter.next() {
            match entry {
                Ok((key, value)) => {
                    let key_ref = key.as_ref();
                    if key_ref == ROOT_KEY || key_ref == LEAF_COUNT_KEY {
                        continue;
                    }
                    let prefix = match decode_prefix(key_ref) {
                        Ok(prefix) => prefix,
                        Err(err) => return Some(Err(Error::Unsupported(err))),
                    };
                    let node = match decode_node(value.as_ref()) {
                        Ok(node) => node,
                        Err(err) => return Some(Err(Error::Unsupported(err))),
                    };
                    return Some(Ok((prefix, node)));
                }
                Err(err) => return Some(Err(err)),
            }
        }
        None
    }
}

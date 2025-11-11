use std::{fmt, io};
use std::path::Path;

use rocksdb::{
    DBIteratorWithThreadMode, IteratorMode, OptimisticTransactionDB, Options, Transaction,
};

use crate::Prefix;

use super::sled_storage::{
    decode_node, decode_prefix, encode_node, encode_prefix, prefix_key, ROOT_KEY,
};
use super::Node;

pub type RocksResult<T> = Result<T, RocksStorageError>;

#[derive(Debug)]
pub enum RocksStorageError {
    Db(rocksdb::Error),
    Codec(String),
    Io(io::Error),
    TransactionClosed,
}

impl fmt::Display for RocksStorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RocksStorageError::Db(err) => write!(f, "rocksdb error: {}", err),
            RocksStorageError::Codec(err) => write!(f, "serialization error: {}", err),
            RocksStorageError::Io(err) => write!(f, "io error: {}", err),
            RocksStorageError::TransactionClosed => write!(f, "transaction already finished"),
        }
    }
}

impl std::error::Error for RocksStorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RocksStorageError::Db(err) => Some(err),
            RocksStorageError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<rocksdb::Error> for RocksStorageError {
    fn from(err: rocksdb::Error) -> Self {
        RocksStorageError::Db(err)
    }
}

impl From<String> for RocksStorageError {
    fn from(err: String) -> Self {
        RocksStorageError::Codec(err)
    }
}

impl From<io::Error> for RocksStorageError {
    fn from(err: io::Error) -> Self {
        RocksStorageError::Io(err)
    }
}

pub struct RocksStorage {
    db: OptimisticTransactionDB,
}

impl RocksStorage {
    pub fn open(path: impl AsRef<Path>) -> RocksResult<Self> {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.set_max_open_files(512);
        let db = OptimisticTransactionDB::open(&options, path)?;
        Ok(Self { db })
    }

    pub fn start_transaction(&self) -> RocksTransaction<'_> {
        RocksTransaction {
            tx: Some(self.db.transaction()),
        }
    }

    pub fn approximate_entry_count(&self) -> usize {
        self.db
            .property_int_value("rocksdb.estimate-num-keys")
            .ok()
            .flatten()
            .unwrap_or(0) as usize
    }

    pub fn enumerate_nodes(&self) -> RocksResult<Vec<(Prefix, Node)>> {
        self.iter_nodes().collect()
    }

    pub fn iter_nodes(&self) -> RocksNodeIter<'_> {
        RocksNodeIter {
            iter: self.db.iterator(IteratorMode::Start),
        }
    }

    pub fn flush(&self) -> RocksResult<()> {
        self.db.flush()?;
        Ok(())
    }
}

pub struct RocksTransaction<'a> {
    tx: Option<Transaction<'a, OptimisticTransactionDB>>,
}

impl<'a> RocksTransaction<'a> {
    fn inner(&self) -> RocksResult<&Transaction<'a, OptimisticTransactionDB>> {
        self.tx.as_ref().ok_or(RocksStorageError::TransactionClosed)
    }

    pub fn load_root(&self) -> RocksResult<Prefix> {
        let tx = self.inner()?;
        match tx.get(ROOT_KEY)? {
            Some(raw) => decode_prefix(raw.as_ref()).map_err(Into::into),
            None => {
                let root = Prefix::root();
                tx.put(ROOT_KEY, encode_prefix(root))?;
                Ok(root)
            }
        }
    }

    pub fn batch_read_nodes(&self, prefixes: &[Prefix]) -> RocksResult<Vec<Option<Node>>> {
        if prefixes.is_empty() {
            return Ok(Vec::new());
        }

        let keys: Vec<Vec<u8>> = prefixes.iter().map(prefix_key).collect();
        let tx = self.inner()?;
        let mut decoded = Vec::with_capacity(prefixes.len());
        for entry in tx.multi_get(keys) {
            match entry {
                Ok(Some(raw)) => decoded.push(Some(decode_node(raw.as_ref())?)),
                Ok(None) => decoded.push(None),
                Err(err) => return Err(err.into()),
            }
        }
        Ok(decoded)
    }

    pub fn batch_write_nodes(&self, entries: &[(Prefix, Node)]) -> RocksResult<()> {
        let tx = self.inner()?;
        for (prefix, node) in entries.iter() {
            let key = prefix_key(prefix);
            let value = encode_node(node);
            tx.put(key, value)?;
        }
        Ok(())
    }

    pub fn set_root(&self, root: Prefix) -> RocksResult<()> {
        self.inner()?.put(ROOT_KEY, encode_prefix(root))?;
        Ok(())
    }

    pub fn commit(mut self) -> RocksResult<()> {
        if let Some(tx) = self.tx.take() {
            tx.commit()?;
        }
        Ok(())
    }

    pub fn rollback(mut self) -> RocksResult<()> {
        if let Some(tx) = self.tx.take() {
            tx.rollback()?;
        }
        Ok(())
    }
}

impl<'a> Drop for RocksTransaction<'a> {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.rollback();
        }
    }
}

pub struct RocksNodeIter<'a> {
    iter: DBIteratorWithThreadMode<'a, OptimisticTransactionDB>,
}

impl<'a> Iterator for RocksNodeIter<'a> {
    type Item = RocksResult<(Prefix, Node)>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(entry) = self.iter.next() {
            match entry {
                Ok((key, value)) => {
                    if key.as_ref() == ROOT_KEY {
                        continue;
                    }
                    let prefix = match decode_prefix(key.as_ref()) {
                        Ok(prefix) => prefix,
                        Err(err) => return Some(Err(err.into())),
                    };
                    let node = match decode_node(value.as_ref()) {
                        Ok(node) => node,
                        Err(err) => return Some(Err(err.into())),
                    };
                    return Some(Ok((prefix, node)));
                }
                Err(err) => return Some(Err(err.into())),
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpt::LeafNode;
    use tempfile::TempDir;

    fn build_leaf(key_byte: u8, value_byte: u8) -> (Prefix, Node) {
        let mut key = [0u8; 32];
        key[0] = key_byte;
        let mut value = [0u8; 32];
        value[0] = value_byte;
        let leaf = LeafNode::new(key, value);
        (Prefix::from(key), Node::Leaf(leaf))
    }

    #[test]
    fn transaction_flow_round_trip() {
        let dir = TempDir::new().expect("temp dir");
        let storage = RocksStorage::open(dir.path()).expect("open rocksdb");
        let tx = storage.start_transaction();

        let root = tx.load_root().expect("load root");
        assert_eq!(root, Prefix::root());

        let entries = vec![build_leaf(1, 11), build_leaf(2, 22)];
        tx.batch_write_nodes(&entries).expect("write batch");
        tx.set_root(entries[0].0).expect("set root");
        tx.commit().expect("commit");

        let tx = storage.start_transaction();
        let fetched = tx
            .batch_read_nodes(&[entries[0].0, entries[1].0])
            .expect("batch read");
        assert!(matches!(fetched[0], Some(Node::Leaf(_))));
        assert!(matches!(fetched[1], Some(Node::Leaf(_))));
        tx.rollback().expect("rollback");
    }
}

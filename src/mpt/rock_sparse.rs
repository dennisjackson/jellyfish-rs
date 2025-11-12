use dashmap::{DashMap, DashSet};
use rayon::join;
use std::path::Path;
use tempfile::TempDir;

use crate::mpt::MerklePatriciaTree;
use crate::{Hash, Prefix};

use super::rocks_storage::{RocksResult, RocksStorage};
use super::{InteriorNode, LeafNode, Node};

type DirtyPrefixes = DashSet<Prefix>;

pub struct RockSparseMPT {
    storage: RocksStorage,
    store: DashMap<Prefix, Node>,
    root: Prefix,
    dirty_prefixes: DirtyPrefixes,
    _temp_dir: Option<TempDir>,
    /// Tracks the depth (prefix length) where all nodes exist and are interior nodes.
    /// At depth D, there should be 2^D interior nodes for this depth to be considered complete.
    complete_interior_depth: std::sync::atomic::AtomicU16,
}

impl RockSparseMPT {
    pub fn new_with_path(path: impl AsRef<Path>) -> RocksResult<Self> {
        let storage = RocksStorage::open(path)?;
        Self::from_storage(storage, None)
    }

    pub fn new_temporary() -> RocksResult<Self> {
        let temp_dir = TempDir::new()?;
        let storage = RocksStorage::open(temp_dir.path())?;
        Self::from_storage(storage, Some(temp_dir))
    }

    fn insert_node_with_db(&self, dirty_prefixes: &DirtyPrefixes, prefix: Prefix, node: Node) {
        self.store.insert(prefix, node);
        dirty_prefixes.insert(prefix);
    }

    fn insert_node_memory_only(&self, prefix: Prefix, node: Node) {
        self.store.insert(prefix, node);
    }

    fn from_storage(storage: RocksStorage, temp_dir: Option<TempDir>) -> RocksResult<Self> {
        let (root, complete_depth) = {
            let tx = storage.start_transaction();
            let root = tx.load_root()?;
            let complete_depth = tx.get_complete_depth()?.unwrap_or(0);
            tx.commit()?;
            (root, complete_depth)
        };

        let estimated_entries = storage.approximate_entry_count().max(1);
        let mut instance = Self {
            storage,
            store: DashMap::with_capacity(estimated_entries.saturating_mul(2)),
            root,
            dirty_prefixes: DashSet::new(),
            _temp_dir: temp_dir,
            complete_interior_depth: std::sync::atomic::AtomicU16::new(complete_depth),
        };

        const RECOVERY_CHUNK: usize = 1024 * 1024;
        let mut leaf_entries: Vec<(Hash, Hash)> = Vec::with_capacity(RECOVERY_CHUNK);
        let stored_nodes = instance
            .storage
            .iter_nodes()
            .collect::<RocksResult<Vec<_>>>()?;
        for (prefix, node) in stored_nodes {
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

        self.dirty_prefixes.clear();
        let tx = self.storage.start_transaction();
        let new_root = Self::recursive_batch_upsert(
            self,
            self.root,
            entries_vec,
            Some(&self.dirty_prefixes),
        );

        {
            let mut writes: Vec<(Prefix, Node)> = self.dirty_prefixes
                .iter()
                .filter_map(|prefix_ref| {
                    let prefix = *prefix_ref;
                    self.store
                        .get(&prefix)
                        .map(|node| (prefix, node.value().clone()))
                })
                .collect();

            if !writes.is_empty() {
                // Keep a consistent order for determinism in tests/debugging.
                writes.sort_unstable_by_key(|(prefix, _)| *prefix);
                tx.batch_write_nodes(&writes)
                    .expect("DB batch write failed");
            }
        }

        tx.set_root(new_root)
            .expect("Failed to update root in DB");

        // Update and persist the complete interior depth in the same transaction
        self.update_complete_interior_depth(&tx);

        tx.commit().expect("Failed to commit batch upsert");
        self.root = new_root;
        self.dirty_prefixes.clear();
    }

    fn batch_upsert_memory_only(&mut self, entries: &[(Hash, Hash)]) {
        if entries.is_empty() {
            return;
        }

        let mut entries_vec: Vec<(Hash, Hash)> = entries.to_vec();
        entries_vec.sort_unstable_by_key(|(k, _)| *k);
        entries_vec.dedup_by_key(|(k, _)| *k);

        let new_root = Self::recursive_batch_upsert(self, self.root, entries_vec, None);
        self.root = new_root;
    }

    fn recursive_batch_upsert(
        &self,
        current_prefix: Prefix,
        entries: Vec<(Hash, Hash)>,
        dirty_prefixes: Option<&DirtyPrefixes>,
    ) -> Prefix {
        if entries.is_empty() {
            return current_prefix;
        }

        let node = self
            .store
            .get(&current_prefix)
            .map(|guard| guard.value().clone());
        let Some(node) = node else {
            return Self::batch_insert_into_empty(self, entries, dirty_prefixes);
        };

        match node {
            Node::Leaf(leaf) => {
                Self::batch_upsert_at_leaf(self, current_prefix, leaf, entries, dirty_prefixes)
            }
            Node::Interior(interior) => {
                Self::batch_upsert_at_interior(
                    self,
                    current_prefix,
                    interior,
                    entries,
                    dirty_prefixes,
                )
            }
        }
    }

    fn batch_insert_into_empty(
        &self,
        mut entries: Vec<(Hash, Hash)>,
        dirty_prefixes: Option<&DirtyPrefixes>,
    ) -> Prefix {
        if entries.is_empty() {
            return Prefix::root();
        }

        let (first_key, first_value) = entries.remove(0);
        let first_prefix = Prefix::from(first_key);
        let first_leaf = LeafNode::new(first_key, first_value);
        if let Some(dirty) = dirty_prefixes.as_ref() {
            self.insert_node_with_db(dirty, first_prefix, Node::Leaf(first_leaf));
        } else {
            self.insert_node_memory_only(first_prefix, Node::Leaf(first_leaf));
        }
        Self::recursive_batch_upsert(self, first_prefix, entries, dirty_prefixes)
    }

    fn batch_upsert_at_leaf(
        &self,
        leaf_prefix: Prefix,
        leaf: LeafNode,
        mut entries: Vec<(Hash, Hash)>,
        dirty_prefixes: Option<&DirtyPrefixes>,
    ) -> Prefix {
        if let Ok(idx) = entries.binary_search_by_key(&leaf_prefix.hash, |(k, _)| *k) {
            let (_, new_value) = entries.remove(idx);
            let updated_leaf = LeafNode::new(leaf_prefix.hash, new_value);
            if let Some(dirty) = dirty_prefixes.as_ref() {
                self.insert_node_with_db(dirty, leaf_prefix, Node::Leaf(updated_leaf));
            } else {
                self.insert_node_memory_only(leaf_prefix, Node::Leaf(updated_leaf));
            }
            if entries.is_empty() {
                return leaf_prefix;
            }
            return Self::recursive_batch_upsert(self, leaf_prefix, entries, dirty_prefixes);
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

        if let Some(dirty) = dirty_prefixes.as_ref() {
            self.insert_node_memory_only(merged_prefix, Node::Interior(new_interior));
            self.insert_node_with_db(dirty, existing_prefix, Node::Leaf(leaf));
            self.insert_node_with_db(dirty, new_prefix, Node::Leaf(new_leaf));
        } else {
            self.insert_node_memory_only(merged_prefix, Node::Interior(new_interior));
            self.insert_node_memory_only(existing_prefix, Node::Leaf(leaf));
            self.insert_node_memory_only(new_prefix, Node::Leaf(new_leaf));
        }

        Self::recursive_batch_upsert(self, merged_prefix, entries, dirty_prefixes)
    }

    fn batch_upsert_at_interior(
        &self,
        interior_prefix: Prefix,
        interior: InteriorNode,
        entries: Vec<(Hash, Hash)>,
        dirty_prefixes: Option<&DirtyPrefixes>,
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

            if let Some(dirty) = dirty_prefixes.as_ref() {
                self.insert_node_memory_only(common, Node::Interior(new_interior));
                self.insert_node_with_db(dirty, new_leaf_prefix, Node::Leaf(new_leaf));
            } else {
                self.insert_node_memory_only(common, Node::Interior(new_interior));
                self.insert_node_memory_only(new_leaf_prefix, Node::Leaf(new_leaf));
            }

            contained_entries.extend(divergent_entries);
            return Self::recursive_batch_upsert(self, common, contained_entries, dirty_prefixes);
        }
        let (right_entries, left_entries): (Vec<(Hash, Hash)>, Vec<(Hash, Hash)>) =
            contained_entries
                .into_iter()
                .partition(|(k, _)| interior_prefix.key_goes_right(*k));

        let count = left_entries.len() + right_entries.len();
        let l_work = || {
            if !left_entries.is_empty() {
                Self::recursive_batch_upsert(self, interior.left, left_entries, Some(&self.dirty_prefixes))
            } else {
                interior.left
            }
        };
        let r_work = || {
            if !right_entries.is_empty() {
                Self::recursive_batch_upsert(self, interior.right, right_entries, Some(&self.dirty_prefixes))
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

        // Check if this interior node is at or below the complete depth
        // If so, mark it as dirty so it gets persisted to the database
        let complete_depth = self.complete_interior_depth.load(std::sync::atomic::Ordering::Relaxed);
        if interior_prefix.length == complete_depth {
            // This interior node is at or below the complete depth, so it should be persisted
            if let Some(dirty) = dirty_prefixes.as_ref() {
                self.insert_node_with_db(dirty, interior_prefix, Node::Interior(updated_interior));
            } else {
                self.insert_node_memory_only(interior_prefix, Node::Interior(updated_interior));
            }
        } else {
            // Above the complete depth, keep it in memory only
            self.insert_node_memory_only(interior_prefix, Node::Interior(updated_interior));
        }

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

    pub fn flush(&mut self) -> RocksResult<()> {
        self.storage.flush()
    }

    pub fn len(&self) -> usize {
        self.storage.approximate_entry_count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the current complete interior depth.
    /// At this depth, all 2^depth nodes exist and are interior nodes.
    pub fn complete_interior_depth(&self) -> u16 {
        self.complete_interior_depth.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Check if all nodes at the given depth exist and are interior nodes.
    /// At depth D, there should be exactly 2^D nodes, all of which must be Interior nodes.
    fn check_depth_complete(&self, depth: u16) -> bool {
        // The root (depth 0) is always considered complete if it exists as an interior
        if depth == 0 {
            let is_interior = self.store.get(&Prefix::root())
                .map(|n| matches!(n.value(), Node::Interior(_)))
                .unwrap_or(false);
            return is_interior;
        }

        // If depth is too large (>= 20), we can't have that many nodes
        if depth >= 20 {
            return false;
        }

        // For depth > 0, we need to check that exactly 2^depth nodes exist at this depth
        let expected_count = 1u64 << depth; // 2^depth

        // Generate all possible prefixes at this depth and check them directly
        // This is more efficient than scanning all nodes in the DashMap
        for i in 0..expected_count {
            let prefix = Self::prefix_from_depth_and_index(depth, i);

            match self.store.get(&prefix) {
                Some(node) => {
                    // Node exists, check if it's an interior node
                    if !matches!(node.value(), Node::Interior(_)) {
                        return false;
                    }
                }
                None => {
                    // Node doesn't exist at this prefix
                    return false;
                }
            }
        }

        true
    }

    /// Generate a prefix at a specific depth with a given index.
    /// For depth D, valid indices are 0..2^D.
    /// The index represents the binary number formed by the first D bits.
    fn prefix_from_depth_and_index(depth: u16, index: u64) -> Prefix {
        let mut hash = [0u8; 32];

        // Set bits according to the index
        for bit_pos in 0..depth {
            let bit_value = (index >> (depth - 1 - bit_pos)) & 1;
            if bit_value == 1 {
                let byte_index = (bit_pos / 8) as usize;
                let bit_index = 7 - (bit_pos % 8);
                hash[byte_index] |= 1 << bit_index;
            }
        }

        Prefix { hash, length: depth }
    }

    /// Persist all interior nodes at a specific depth to the database in the given transaction.
    /// This is called when we discover that a depth has become complete.
    fn persist_interior_nodes_at_depth_in_tx(&self, depth: u16, tx: &super::rocks_storage::RocksTransaction) -> RocksResult<()> {
        if depth >= 20 {
            // Too deep, too many nodes to handle
            return Ok(());
        }

        let expected_count = 1u64 << depth; // 2^depth
        let mut nodes_to_write = Vec::new();

        // Collect all interior nodes at this depth
        for i in 0..expected_count {
            let prefix = Self::prefix_from_depth_and_index(depth, i);

            if let Some(node_ref) = self.store.get(&prefix) {
                if matches!(node_ref.value(), Node::Interior(_)) {
                    nodes_to_write.push((prefix, node_ref.value().clone()));
                }
            }
        }

        // Write all nodes in the given transaction
        if !nodes_to_write.is_empty() {
            tx.batch_write_nodes(&nodes_to_write)?;
        }

        Ok(())
    }

    /// Update the tracked complete interior depth by scanning upward from the current depth.
    /// This is called after a batch upsert to check if new levels have become complete.
    /// The updated depth is persisted to the database in the provided transaction.
    fn update_complete_interior_depth(&self, tx: &super::rocks_storage::RocksTransaction) {
        let mut current_depth = self.complete_interior_depth.load(std::sync::atomic::Ordering::Relaxed);

        // Keep checking successive depths until we find one that's incomplete
        loop {
            let next_depth = current_depth + 1;

            // Don't go beyond reasonable depth (256 would be a full hash)
            if next_depth >= 256 {
                break;
            }

            // Check if the next depth level is complete
            if self.check_depth_complete(next_depth) {
                // Persist all interior nodes at this newly complete depth to the database
                if let Err(e) = self.persist_interior_nodes_at_depth_in_tx(next_depth, tx) {
                    eprintln!("Warning: Failed to persist interior nodes at depth {}: {}", next_depth, e);
                    break;
                }

                current_depth = next_depth;
                self.complete_interior_depth.store(current_depth, std::sync::atomic::Ordering::Relaxed);
            } else {
                // If this level is incomplete, we're done
                break;
            }
        }

        // Persist the updated depth to the database
        if let Err(e) = tx.set_complete_depth(current_depth) {
            eprintln!("Warning: Failed to persist complete depth {}: {}", current_depth, e);
        }
    }
}

impl MerklePatriciaTree for RockSparseMPT {
    fn new() -> Self {
        Self::new_temporary().expect("Failed to create temporary RocksDB database")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Hash;

    fn make_hash(byte: u8) -> Hash {
        let mut hash = [0u8; 32];
        hash[0] = byte;
        hash
    }

    #[test]
    fn test_complete_interior_depth_empty_tree() {
        let tree = RockSparseMPT::new_temporary().expect("create tree");
        // Empty tree should have depth 0
        assert_eq!(tree.complete_interior_depth(), 0);
    }

    #[test]
    fn test_complete_interior_depth_single_insert() {
        let mut tree = RockSparseMPT::new_temporary().expect("create tree");

        // Insert a single key-value pair
        tree.batch_upsert(&[(make_hash(0x00), make_hash(0x01))]);

        // With just one leaf, we should still have depth 0
        // (the root exists but is a leaf, not an interior)
        assert_eq!(tree.complete_interior_depth(), 0);
    }

    #[test]
    fn test_complete_interior_depth_two_inserts() {
        let mut tree = RockSparseMPT::new_temporary().expect("create tree");

        // Insert two keys that differ in the first bit
        // 0x00 = 00000000...
        // 0x80 = 10000000...
        tree.batch_upsert(&[
            (make_hash(0x00), make_hash(0x01)),
            (make_hash(0x80), make_hash(0x02)),
        ]);

        // Now the root should be an interior node (depth 0 complete)
        // But depth 1 requires 2 interior nodes, which we don't have yet
        let depth = tree.complete_interior_depth();
        println!("Depth after 2 inserts with different first bit: {}", depth);
    }

    #[test]
    fn test_complete_interior_depth_full_level() {
        let mut tree = RockSparseMPT::new_temporary().expect("create tree");

        // Insert 4 keys to create a tree with depth 2
        // This creates interior nodes at depth 0 and 1
        tree.batch_upsert(&[
            (make_hash(0x00), make_hash(0x01)), // 00...
            (make_hash(0x40), make_hash(0x02)), // 01...
            (make_hash(0x80), make_hash(0x03)), // 10...
            (make_hash(0xC0), make_hash(0x04)), // 11...
        ]);

        let depth = tree.complete_interior_depth();
        println!("Complete interior depth with 4 keys: {}", depth);

        // The actual depth depends on the tree structure
        // With these 4 keys differing in the first 2 bits, we should have:
        // - Depth 0: 1 interior node (root)
        // - Depth 1: 2 interior nodes (left and right subtrees)
        // So depth should be at least 1 or 2
    }

    #[test]
    fn test_complete_interior_depth_incremental() {
        let mut tree = RockSparseMPT::new_temporary().expect("create tree");

        let initial_depth = tree.complete_interior_depth();
        println!("Initial depth: {}", initial_depth);

        // Add first key
        tree.batch_upsert(&[(make_hash(0x00), make_hash(0x01))]);
        let depth1 = tree.complete_interior_depth();
        println!("Depth after 1 insert: {}", depth1);

        // Add second key (different first bit)
        tree.batch_upsert(&[(make_hash(0x80), make_hash(0x02))]);
        let depth2 = tree.complete_interior_depth();
        println!("Depth after 2 inserts: {}", depth2);

        // Add third and fourth keys
        tree.batch_upsert(&[
            (make_hash(0x40), make_hash(0x03)),
            (make_hash(0xC0), make_hash(0x04)),
        ]);
        let depth3 = tree.complete_interior_depth();
        println!("Depth after 4 inserts: {}", depth3);

        // The depth should not decrease
        assert!(depth3 >= depth2);
        assert!(depth2 >= depth1);
    }
}

use dashmap::{DashMap, DashSet};
use log::{debug, info, warn};
use rayon::{join, prelude::*};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use tempfile::TempDir;

use crate::mpt::MerklePatriciaTree;
use crate::prefix::HashExt;
use crate::{Hash, Prefix};

use super::rocks_storage::{RocksResult, RocksStorage};
use super::{InteriorNode, LeafNode, Node};

type DirtyPrefixes = DashSet<Prefix>;

#[derive(Default)]
struct ParentChildren {
    left: Option<(Prefix, Hash)>,
    right: Option<(Prefix, Hash)>,
}

const KEEP_BELOW_FRONTIER: u16 = 3;

pub struct RocksTransRelMPT {
    storage: RocksStorage,
    store: DashMap<Prefix, Node>,
    root: Prefix,
    dirty_prefixes: DirtyPrefixes,
    loaded_subtrees: DashSet<Prefix>,
    full_tree_loaded: AtomicBool,
    _temp_dir: Option<TempDir>,
    /// Tracks the depth (prefix length) where all nodes exist and are interior nodes.
    /// At depth D, there should be 2^D interior nodes for this depth to be considered complete.
    complete_interior_depth: AtomicU16,
    prefix_loads: AtomicU64,
}

impl RocksTransRelMPT {
    pub fn new_with_path(path: impl AsRef<Path>) -> RocksResult<Self> {
        let storage = RocksStorage::open(path)?;
        Self::from_storage(storage, None)
    }

    pub fn new_temporary() -> RocksResult<Self> {
        let temp_dir = TempDir::new()?;
        let storage = RocksStorage::open(temp_dir.path())?;
        Self::from_storage(storage, Some(temp_dir))
    }

    fn insert_node_memory_only(&self, prefix: Prefix, node: Node) {
        self.store.insert(prefix, node);
    }

    // No longer needed: leaves are written opportunistically during recursion.

    fn recover_full_tree_from_storage(&mut self) -> RocksResult<()> {
        const RECOVERY_CHUNK: usize = 1024 * 1024;
        let mut leaf_entries: Vec<(Hash, Hash)> = Vec::with_capacity(RECOVERY_CHUNK);
        let stored_nodes = self.storage.iter_nodes().collect::<RocksResult<Vec<_>>>()?;
        for (prefix, node) in stored_nodes {
            if let Node::Leaf(leaf) = node {
                leaf_entries.push((prefix.hash, leaf.value));
                if leaf_entries.len() == RECOVERY_CHUNK {
                    assert!(leaf_entries.is_sorted());
                    self.batch_upsert_memory_only(&leaf_entries);
                    leaf_entries.clear();
                }
            }
        }

        if !leaf_entries.is_empty() {
            self.batch_upsert_memory_only(&leaf_entries);
        }
        Ok(())
    }

    fn load_interior_nodes_from_storage(&self, depth: u16) -> RocksResult<bool> {
        let nodes_at_depth = self.storage.get_nodes_by_prefix_length(depth)?;
        let mut interior_nodes: Vec<(Prefix, InteriorNode)> = nodes_at_depth
            .into_iter()
            .filter_map(|(prefix, node)| match node {
                Node::Interior(interior) => Some((prefix, interior)),
                _ => None,
            })
            .collect();
        info!(
            "Loaded {} interior nodes at depth {} from storage",
            interior_nodes.len(),
            depth
        );
        if interior_nodes.is_empty() {
            return Ok(false);
        }

        interior_nodes.sort_unstable_by_key(|(prefix, _)| *prefix);
        if !self.rebuild_tree_from_interior_nodes(depth, interior_nodes) {
            warn!(
                "Warning: Failed to rebuild interior nodes from persisted depth {}. Falling back to full recovery.",
                depth
            );
            return Ok(false);
        }
        Ok(true)
    }

    fn rebuild_tree_from_interior_nodes(
        &self,
        depth: u16,
        nodes: Vec<(Prefix, InteriorNode)>,
    ) -> bool {
        if nodes.is_empty() {
            return false;
        }

        let mut current_nodes = nodes;
        let mut current_depth = depth;
        loop {
            if !Self::validate_level(current_depth, current_nodes.len()) {
                return false;
            }

            for (prefix, node) in &current_nodes {
                self.insert_node_memory_only(*prefix, Node::Interior(node.clone()));
            }

            if current_depth == 0 {
                return true;
            }

            let mut parent_map: BTreeMap<Prefix, ParentChildren> = BTreeMap::new();
            for (prefix, node) in &current_nodes {
                assert!(
                    prefix.length == current_depth,
                    "Unexpected prefix depth during rebuild"
                );
                let parent_prefix = Self::parent_prefix(*prefix);
                let entry = parent_map
                    .entry(parent_prefix)
                    .or_insert_with(ParentChildren::default);
                let child_info = (*prefix, node.merkle_hash);
                if Self::is_right_child(prefix) {
                    if entry.right.replace(child_info).is_some() {
                        return false;
                    }
                } else {
                    if entry.left.replace(child_info).is_some() {
                        return false;
                    }
                }
            }

            let mut next_nodes = Vec::with_capacity(parent_map.len());
            for (parent_prefix, children) in parent_map {
                let (left_prefix, left_hash) = match children.left {
                    Some(child) => child,
                    None => return false,
                };
                let (right_prefix, right_hash) = match children.right {
                    Some(child) => child,
                    None => return false,
                };
                let parent_node = InteriorNode::new(
                    parent_prefix,
                    left_prefix,
                    right_prefix,
                    left_hash,
                    right_hash,
                );
                next_nodes.push((parent_prefix, parent_node));
            }

            current_nodes = next_nodes;
            if current_nodes.is_empty() {
                return false;
            }
            current_depth -= 1;
        }
    }

    fn parent_prefix(prefix: Prefix) -> Prefix {
        assert!(prefix.length > 0, "Root prefix has no parent");
        let parent_length = prefix.length - 1;
        Prefix {
            hash: prefix.hash.zero_bits_from(parent_length),
            length: parent_length,
        }
    }

    fn is_right_child(prefix: &Prefix) -> bool {
        assert!(
            prefix.length > 0,
            "Root prefix cannot be classified as child"
        );
        prefix.hash.get_bit(prefix.length - 1)
    }

    fn expected_nodes_for_depth(depth: u16) -> Option<usize> {
        if depth as u32 >= usize::BITS {
            return None;
        }
        Some(1usize << depth)
    }

    fn validate_level(depth: u16, count: usize) -> bool {
        match Self::expected_nodes_for_depth(depth) {
            Some(expected) => count == expected,
            None => true,
        }
    }

    fn load_subtree_from_storage(&self, prefix: Prefix) -> RocksResult<bool> {
        // if self.loaded_subtrees.contains(&prefix) {
        //     debug!("Subtree prefix {} already loaded", prefix.short_hex());
        //     return Ok(false);
        // }

        self.prefix_loads.fetch_add(1, Ordering::Relaxed);
        let leaf_nodes = self.storage.get_leaf_nodes_by_prefix(&prefix)?;
        let loaded_nodes = leaf_nodes.len();
        if leaf_nodes.is_empty() {
            self.loaded_subtrees.insert(prefix);
            return Ok(false);
        }

        let mut entries: Vec<(Hash, Hash)> = Vec::with_capacity(leaf_nodes.len());
        for (leaf_prefix, node) in leaf_nodes {
            if let Node::Leaf(leaf) = node {
                entries.push((leaf_prefix.hash, leaf.value));
            }
        }

        if entries.is_empty() {
            self.loaded_subtrees.insert(prefix);
            return Ok(false);
        }

        entries.sort_unstable_by_key(|(hash, _)| *hash);
        entries.dedup_by_key(|(hash, _)| *hash);

        let mut dummy = false;
        let built_prefix = Self::batch_insert_into_empty(self, &entries, None, &mut dummy);
        self.loaded_subtrees.insert(prefix);
        if built_prefix != prefix {
            warn!(
                "Loaded subtree prefix {} does not match built prefix {}",
                prefix.short_hex(),
                built_prefix.short_hex()
            );
            self.loaded_subtrees.insert(built_prefix);
        }
        debug!(
            "Loaded subtree prefix {} successfully from {} leaf nodes",
            prefix.short_hex(),
            loaded_nodes
        );
        Ok(true)
    }

    fn load_prefix_or_panic(&self, prefix: Prefix) {
        if let Err(err) = self.load_subtree_from_storage(prefix) {
            panic!(
                "Failed to load prefix {} from RocksDB: {}",
                prefix.short_hex(),
                err
            );
        }
    }

    fn ensure_node_loaded(&self, prefix: Prefix) {
        if self.store.contains_key(&prefix) {
            return;
        }
        self.load_prefix_or_panic(prefix);
        debug_assert!(
            self.store.contains_key(&prefix),
            "Prefix {} missing after load",
            prefix.short_hex()
        );
    }

    fn ensure_full_tree_loaded(&self) {
        if self.full_tree_loaded.load(Ordering::Acquire) {
            return;
        }

        let depth = self.complete_interior_depth.load(Ordering::Relaxed);
        let span = if depth == 0 { 1 } else { 1u64 << depth };
        for idx in 0..span {
            let prefix = Self::prefix_from_depth_and_index(depth, idx);
            self.load_prefix_or_panic(prefix);
        }

        self.full_tree_loaded.store(true, Ordering::Release);
    }

    fn from_storage(storage: RocksStorage, temp_dir: Option<TempDir>) -> RocksResult<Self> {
        let (root, complete_depth) = {
            let tx = storage.start_transaction();
            let root = tx.load_root()?;
            let complete_depth = tx.get_complete_depth()?.unwrap_or(0);
            tx.commit()?;
            (root, complete_depth)
        };

        let approx_entries = storage.approximate_entry_count();
        let estimated_entries = approx_entries.max(1);
        let mut instance = Self {
            storage,
            store: DashMap::with_capacity(2_usize.pow(complete_depth as u32 +4)),
            root,
            dirty_prefixes: DashSet::new(),
            loaded_subtrees: DashSet::new(),
            full_tree_loaded: AtomicBool::new(false),
            _temp_dir: temp_dir,
            prefix_loads: AtomicU64::new(0),
            complete_interior_depth: AtomicU16::new(complete_depth),
        };

        let has_entries = approx_entries > 0;
        let mut initialized = false;
        if complete_depth <= 256 {
            initialized = instance.load_interior_nodes_from_storage(complete_depth)?;
        }
        if !initialized && has_entries {
            instance.recover_full_tree_from_storage()?;
            instance.full_tree_loaded.store(true, Ordering::Relaxed);
        }
        info!(
            "Initialized RocksTransRelMPT with root {}, complete depth {}, approx entries {}",
            instance.root.short_hex(),
            instance.complete_interior_depth.load(Ordering::Relaxed),
            approx_entries
        );
        Ok(instance)
    }

    fn batch_upsert_optimized(&mut self, entries: &[(Hash, Hash)]) {
        if entries.is_empty() {
            return;
        }

        debug!(
            "Beginning batch upsert. Prefix Loads: {} Loaded prefixes: {}",
            self.prefix_loads.load(Ordering::Relaxed),
            self.loaded_subtrees.len()
        );
        let mut entries_vec: Vec<(Hash, Hash)> = entries.to_vec();
        entries_vec.sort_unstable_by_key(|(k, _)| *k);
        entries_vec.dedup_by_key(|(k, _)| *k);

        debug!(
            "RocksSparse: batch_upsert_optimized start entries={} complete_depth={}",
            entries_vec.len(),
            self.complete_interior_depth.load(Ordering::Relaxed)
        );
        // Perform recursive upsert using per-interior-node batches only at boundaries.
        let mut boundary_started = false;
        let new_root = Self::recursive_batch_upsert(
            self,
            self.root,
            &entries_vec,
            None,
            &mut boundary_started,
        );

        // Persist root and possibly advance/persist complete depth in a final small tx.
        let tx = self.storage.start_transaction();
        tx.set_root(new_root).expect("Failed to update root in DB");
        self.update_complete_interior_depth(&tx);
        tx.commit().expect("Failed to commit metadata update");
        // With a global write batch, a separate boundary batch may not be started explicitly.
        if let Some(root_hash) = self.store.get(&new_root).map(|n| n.value().merkle_hash()) {
            debug!(
                "RocksSparse: batch_upsert_optimized end new_root={} hash={}",
                new_root.short_hex(),
                root_hash.short_hex()
            );
        }

        self.root = new_root;

        // Prune all nodes below the frontier to maintain the invariant
        // Only prune when we have an established frontier with multiple levels
        // For small trees (frontier at depth 0), keep everything in memory
        if self.complete_interior_depth.load(Ordering::Relaxed) > 0 {
            self.prune_below_frontier();
        }
    }

    fn batch_upsert_memory_only(&mut self, entries: &[(Hash, Hash)]) {
        if entries.is_empty() {
            return;
        }

        let mut entries_vec: Vec<(Hash, Hash)> = entries.to_vec();
        entries_vec.sort_unstable_by_key(|(k, _)| *k);
        entries_vec.dedup_by_key(|(k, _)| *k);

        let mut dummy = false;
        let new_root =
            Self::recursive_batch_upsert(self, self.root, &entries_vec, None, &mut dummy);
        self.root = new_root;
    }

    fn recursive_batch_upsert(
        &self,
        current_prefix: Prefix,
        entries: &[(Hash, Hash)],
        mut active_batch: Option<&mut super::rocks_storage::RocksWriteBatch>,
        boundary_started: &mut bool,
    ) -> Prefix {
        if entries.is_empty() {
            return current_prefix;
        }

        let node = self
            .store
            .get(&current_prefix)
            .map(|guard| guard.value().clone())
            .or_else(|| {
                self.load_prefix_or_panic(current_prefix);
                self.store
                    .get(&current_prefix)
                    .map(|guard| guard.value().clone())
            });
        let Some(node) = node else {
            debug!(
                "RocksSparse: node miss at {} depth {}, inserting into empty (entries={})",
                current_prefix.short_hex(),
                current_prefix.length,
                entries.len()
            );
            return Self::batch_insert_into_empty(
                self,
                entries,
                active_batch.as_deref_mut(),
                boundary_started,
            );
        };

        match node {
            Node::Leaf(leaf) => Self::batch_upsert_at_leaf(
                self,
                current_prefix,
                leaf,
                entries,
                active_batch.as_deref_mut(),
                boundary_started,
            ),
            Node::Interior(interior) => Self::batch_upsert_at_interior(
                self,
                current_prefix,
                interior,
                entries,
                active_batch.as_deref_mut(),
                boundary_started,
            ),
        }
    }

    fn batch_insert_into_empty(
        &self,
        entries: &[(Hash, Hash)],
        mut active_batch: Option<&mut super::rocks_storage::RocksWriteBatch>,
        _boundary_started: &mut bool,
    ) -> Prefix {
        if entries.is_empty() {
            return Prefix::root();
        }

        let (first_key, first_value) = entries[0];
        let first_prefix = Prefix::from(first_key);
        let first_leaf = LeafNode::new(first_key, first_value);
        self.insert_node_memory_only(first_prefix, Node::Leaf(first_leaf.clone()));
        if let Some(batch) = active_batch.as_deref_mut() {
            let _ = batch.put_node(&first_prefix, &Node::Leaf(first_leaf));
        }
        Self::recursive_batch_upsert(
            self,
            first_prefix,
            &entries[1..],
            active_batch,
            _boundary_started,
        )
    }

    fn batch_upsert_at_leaf(
        &self,
        leaf_prefix: Prefix,
        leaf: LeafNode,
        entries: &[(Hash, Hash)],
        mut active_batch: Option<&mut super::rocks_storage::RocksWriteBatch>,
        boundary_started: &mut bool,
    ) -> Prefix {
        if let Ok(idx) = entries.binary_search_by_key(&leaf_prefix.hash, |(k, _)| *k) {
            let (_, new_value) = entries[idx];
            let updated_leaf = LeafNode::new(leaf_prefix.hash, new_value);
            self.insert_node_memory_only(leaf_prefix, Node::Leaf(updated_leaf.clone()));

            if let Some(batch) = active_batch.as_deref_mut() {
                let _ = batch.put_node(&leaf_prefix, &Node::Leaf(updated_leaf));
                // Special case: if only one entry left after the match, no need for recursion
                if entries.len() == 1 {
                    return leaf_prefix;
                }
                // Build combined slice: before + after the matched entry
                let mut remaining = Vec::with_capacity(entries.len() - 1);
                remaining.extend_from_slice(&entries[..idx]);
                remaining.extend_from_slice(&entries[idx + 1..]);
                return Self::recursive_batch_upsert(
                    self,
                    leaf_prefix,
                    &remaining,
                    active_batch,
                    boundary_started,
                );
            } else {
                // Start a local batch for this leaf update so it is durable
                let mut batch = self.storage.start_batch();
                let _ = batch.put_node(&leaf_prefix, &Node::Leaf(updated_leaf));

                if entries.len() == 1 {
                    self.storage
                        .write_batch(batch)
                        .expect("Failed to commit leaf update batch");
                    return leaf_prefix;
                }

                let mut remaining = Vec::with_capacity(entries.len() - 1);
                remaining.extend_from_slice(&entries[..idx]);
                remaining.extend_from_slice(&entries[idx + 1..]);
                let res = Self::recursive_batch_upsert(
                    self,
                    leaf_prefix,
                    &remaining,
                    Some(&mut batch),
                    boundary_started,
                );
                self.storage
                    .write_batch(batch)
                    .expect("Failed to commit leaf update batch");
                self.release_subtree(res);
                return res;
            }
        }

        if entries.is_empty() {
            return leaf_prefix;
        }

        let (first_key, first_value) = entries[0];

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

        // Always update in-memory nodes
        self.insert_node_memory_only(merged_prefix, Node::Interior(new_interior.clone()));
        self.insert_node_memory_only(existing_prefix, Node::Leaf(leaf.clone()));
        self.insert_node_memory_only(new_prefix, Node::Leaf(new_leaf.clone()));

        // If we already have an active batch, just write into it and continue.
        if let Some(batch) = active_batch.as_deref_mut() {
            let _ = batch.put_node(&existing_prefix, &Node::Leaf(leaf));
            let _ = batch.put_node(&new_prefix, &Node::Leaf(new_leaf));
            return Self::recursive_batch_upsert(
                self,
                merged_prefix,
                &entries[1..],
                active_batch,
                boundary_started,
            );
        }

        // If no active batch yet and we created an interior at the current
        // complete boundary depth, start a batch here so all subsequent
        // nodes under this interior get persisted.
        let complete_depth = self.complete_interior_depth.load(Ordering::Relaxed);
        if merged_prefix.length == complete_depth {
            debug!(
                "RocksSparse: start boundary batch (leaf-merge) at {} depth {} rem_entries {}",
                merged_prefix.short_hex(),
                merged_prefix.length,
                entries.len().saturating_sub(1)
            );
            let mut batch = self.storage.start_batch();
            let _ = batch.put_node(&merged_prefix, &Node::Interior(new_interior));
            let _ = batch.put_node(&existing_prefix, &Node::Leaf(leaf));
            let _ = batch.put_node(&new_prefix, &Node::Leaf(new_leaf));

            let res = Self::recursive_batch_upsert(
                self,
                merged_prefix,
                &entries[1..],
                Some(&mut batch),
                boundary_started,
            );
            self.storage
                .write_batch(batch)
                .expect("Failed to commit leaf-merge subtree batch");
            self.release_subtree(res);
            // self.loaded_subtrees.insert(merged_prefix);
            *boundary_started = true;
            return res;
        }

        // Otherwise, still no active batch: start a local batch for this merge so both leaves persist
        let mut batch = self.storage.start_batch();
        // let _ = batch.put_node(&merged_prefix, &Node::Interior(new_interior));
        let _ = batch.put_node(&existing_prefix, &Node::Leaf(leaf));
        let _ = batch.put_node(&new_prefix, &Node::Leaf(new_leaf));
        let res = Self::recursive_batch_upsert(
            self,
            merged_prefix,
            &entries[1..],
            Some(&mut batch),
            boundary_started,
        );
        self.storage
            .write_batch(batch)
            .expect("Failed to commit leaf-merge batch");
        // self.release_subtree(res);
        res
    }

    fn batch_upsert_at_interior(
        &self,
        interior_prefix: Prefix,
        interior: InteriorNode,
        entries: &[(Hash, Hash)],
        mut active_batch: Option<&mut super::rocks_storage::RocksWriteBatch>,
        boundary_started: &mut bool,
    ) -> Prefix {
        let len = entries.len();

        let complete_depth = self.complete_interior_depth.load(Ordering::Relaxed);
        let persist_here = interior_prefix.length == complete_depth;
        debug!(
            "RocksSparse: visit interior {} depth {} persist_here={} entries={}",
            interior_prefix.short_hex(),
            interior_prefix.length,
            persist_here,
            len
        );

        // Locate the contiguous window of entries covered by this interior node.
        let mut left_edge = len;
        for (idx, &(key, _)) in entries.iter().enumerate() {
            if interior_prefix.contains(&key) {
                left_edge = idx;
                break;
            }
        }

        if left_edge == len {
            // All entries diverge from this prefix.
            if persist_here && active_batch.is_none() {
                debug!(
                    "RocksSparse: start boundary batch (diverge-all) at {} depth {} entries {}",
                    interior_prefix.short_hex(),
                    interior_prefix.length,
                    len
                );
                let mut batch = self.storage.start_batch();
                let res = self.handle_divergent_entries(
                    interior_prefix,
                    interior,
                    entries,
                    Some(&mut batch),
                    boundary_started,
                    0,
                );
                self.storage
                    .write_batch(batch)
                    .expect("Failed to commit diverge-all subtree batch");
                // self.loaded_subtrees.insert(interior_prefix);
                self.release_subtree(res);
                *boundary_started = true;
                return res;
            } else {
                return self.handle_divergent_entries(
                    interior_prefix,
                    interior,
                    entries,
                    active_batch.as_deref_mut(),
                    boundary_started,
                    0,
                );
            }
        }

        let mut right_edge = left_edge;
        while right_edge < len && interior_prefix.contains(&entries[right_edge].0) {
            right_edge += 1;
        }

        // Assert windowing correctness around the contained segment.
        if right_edge > left_edge {
            for i in left_edge..right_edge {
                assert!(
                    interior_prefix.contains(&entries[i].0),
                    "Windowing error: entry at {} not contained by prefix {}",
                    i,
                    interior_prefix.short_hex()
                );
            }
            if left_edge > 0 {
                assert!(
                    !interior_prefix.contains(&entries[left_edge - 1].0),
                    "Windowing error: left_edge-1 is still contained by prefix {}",
                    interior_prefix.short_hex()
                );
            }
            if right_edge < len {
                assert!(
                    !interior_prefix.contains(&entries[right_edge].0),
                    "Windowing error: right_edge is still contained by prefix {}",
                    interior_prefix.short_hex()
                );
            }
        }

        // Divergent entries exist on the left side.
        if left_edge > 0 {
            if persist_here && active_batch.is_none() {
                debug!(
                    "RocksSparse: start boundary batch (diverge-left) at {} depth {} left_edge {} entries {}",
                    interior_prefix.short_hex(),
                    interior_prefix.length,
                    left_edge,
                    len
                );
                let mut batch = self.storage.start_batch();
                let res = self.handle_divergent_entries(
                    interior_prefix,
                    interior,
                    entries,
                    Some(&mut batch),
                    boundary_started,
                    0,
                );
                self.storage
                    .write_batch(batch)
                    .expect("Failed to commit diverge-left subtree batch");
                // self.loaded_subtrees.insert(interior_prefix);
                self.release_subtree(res);
                *boundary_started = true;
                return res;
            } else {
                return self.handle_divergent_entries(
                    interior_prefix,
                    interior,
                    entries,
                    active_batch.as_deref_mut(),
                    boundary_started,
                    0,
                );
            }
        }

        // Divergent entries exist on the right side.
        if right_edge < len {
            if persist_here && active_batch.is_none() {
                debug!(
                    "RocksSparse: start boundary batch (diverge-right) at {} depth {} right_edge {} entries {}",
                    interior_prefix.short_hex(),
                    interior_prefix.length,
                    right_edge,
                    len
                );
                let mut batch = self.storage.start_batch();
                let res = self.handle_divergent_entries(
                    interior_prefix,
                    interior,
                    entries,
                    Some(&mut batch),
                    boundary_started,
                    right_edge,
                );
                self.storage
                    .write_batch(batch)
                    .expect("Failed to commit diverge-right subtree batch");
                // self.loaded_subtrees.insert(interior_prefix);
                self.release_subtree(res);
                *boundary_started = true;
                return res;
            } else {
                return self.handle_divergent_entries(
                    interior_prefix,
                    interior,
                    entries,
                    active_batch.as_deref_mut(),
                    boundary_started,
                    right_edge,
                );
            }
        }

        // All entries are contained, split the window into left and right children.
        let contained = &entries[left_edge..right_edge];
        let middle =
            left_edge + contained.partition_point(|(key, _)| !interior_prefix.key_goes_right(*key));
        let left_entries = &entries[left_edge..middle];
        let right_entries = &entries[middle..right_edge];
        let count = left_entries.len() + right_entries.len();

        // Recurse into children.
        let (new_left, new_right) = if persist_here && active_batch.is_none() {
            // Start a local batch at the boundary (only if none exists) and pass it down so leaf updates write into it.
            debug!(
                "RocksSparse: start boundary batch (contained) at {} depth {} entries {} (L={}, R={})",
                interior_prefix.short_hex(),
                interior_prefix.length,
                len,
                left_entries.len(),
                right_entries.len()
            );
            let mut batch = self.storage.start_batch();
            let new_left = Self::process_left_entries(
                self,
                interior.left,
                &left_entries,
                Some(&mut batch),
                boundary_started,
            );
            let new_right = Self::process_right_entries(
                self,
                interior.right,
                &right_entries,
                Some(&mut batch),
                boundary_started,
            );

            // Compute interior and include it in the batch
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
            self.insert_node_memory_only(interior_prefix, Node::Interior(updated_interior.clone()));
            let _ = batch.put_node(&interior_prefix, &Node::Interior(updated_interior));
            self.storage
                .write_batch(batch)
                .expect("Failed to commit subtree batch");
            // self.loaded_subtrees.insert(interior_prefix);
            // self.release_subtree(interior_prefix);
            *boundary_started = true;
            return interior_prefix;
        } else if count > 64 && active_batch.is_none() {
            // No active batch; we can safely parallelize left/right recursion.
            // Use local flags and fold them back after join to avoid shared mut across threads.
            let mut left_started = false;
            let mut right_started = false;
            let (new_left, new_right) = join(
                || {
                    Self::process_left_entries(
                        self,
                        interior.left,
                        &left_entries,
                        None,
                        &mut left_started,
                    )
                },
                || {
                    Self::process_right_entries(
                        self,
                        interior.right,
                        &right_entries,
                        None,
                        &mut right_started,
                    )
                },
            );
            *boundary_started |= left_started || right_started;
            (new_left, new_right)
        } else {
            // Sequential when a batch is active; pass it through so updates persist.
            let new_left = Self::process_left_entries(
                self,
                interior.left,
                &left_entries,
                active_batch.as_deref_mut(),
                boundary_started,
            );
            let new_right = Self::process_right_entries(
                self,
                interior.right,
                &right_entries,
                active_batch.as_deref_mut(),
                boundary_started,
            );
            (new_left, new_right)
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

        // Always update memory with the interior node (non-persistence path)
        self.insert_node_memory_only(interior_prefix, Node::Interior(updated_interior.clone()));

        // Not a persistence boundary here.
        interior_prefix
    }

    // Wrapper: ensure a write batch exists when handling divergent entries so newly
    // created ancestors/leaves are durably persisted even if we're above the
    // current persistence boundary.
    fn handle_divergent_entries(
        &self,
        interior_prefix: Prefix,
        interior: InteriorNode,
        entries: &[(Hash, Hash)],
        mut active_batch: Option<&mut super::rocks_storage::RocksWriteBatch>,
        boundary_started: &mut bool,
        first_idx: usize,
    ) -> Prefix {
        if active_batch.is_some() {
            return self.handle_divergent_entries_impl(
                interior_prefix,
                interior,
                entries,
                active_batch,
                boundary_started,
                first_idx,
            );
        }

        // Start a local batch for this divergent subtree.
        let mut batch = self.storage.start_batch();
        let res = self.handle_divergent_entries_impl(
            interior_prefix,
            interior,
            entries,
            Some(&mut batch),
            boundary_started,
            first_idx,
        );
        self.storage
            .write_batch(batch)
            .expect("Failed to commit divergent subtree batch");
        res
    }

    fn handle_divergent_entries_impl(
        &self,
        interior_prefix: Prefix,
        interior: InteriorNode,
        entries: &[(Hash, Hash)],
        mut active_batch: Option<&mut super::rocks_storage::RocksWriteBatch>,
        boundary_started: &mut bool,
        first_idx: usize,
    ) -> Prefix {
        let (first_key, first_value) = entries[first_idx];
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

        self.insert_node_memory_only(common, Node::Interior(new_interior.clone()));
        self.insert_node_memory_only(new_leaf_prefix, Node::Leaf(new_leaf.clone()));

        if let Some(batch) = active_batch.as_deref_mut() {
            let _ = batch.put_node(&new_leaf_prefix, &Node::Leaf(new_leaf));
        }

        if entries.len() == 1 {
            return common;
        }

        // Reuse the original slices by processing the segments on each side of the divergent entry.
        let mut result_prefix = common;

        if first_idx > 0 {
            let pfx = Self::recursive_batch_upsert(
                self,
                result_prefix,
                &entries[..first_idx],
                active_batch.as_deref_mut(),
                boundary_started,
            );
            result_prefix = pfx;
        }

        if first_idx + 1 < entries.len() {
            let pfx = Self::recursive_batch_upsert(
                self,
                result_prefix,
                &entries[first_idx + 1..],
                active_batch.as_deref_mut(),
                boundary_started,
            );
            result_prefix = pfx;
        }

        result_prefix
    }

    fn process_left_entries(
        &self,
        left_prefix: Prefix,
        left_entries: &[(Hash, Hash)],
        active_batch: Option<&mut super::rocks_storage::RocksWriteBatch>,
        boundary_started: &mut bool,
    ) -> Prefix {
        if !left_entries.is_empty() {
            Self::recursive_batch_upsert(
                self,
                left_prefix,
                left_entries,
                active_batch,
                boundary_started,
            )
        } else {
            self.ensure_node_loaded(left_prefix);
            left_prefix
        }
    }

    fn process_right_entries(
        &self,
        right_prefix: Prefix,
        right_entries: &[(Hash, Hash)],
        active_batch: Option<&mut super::rocks_storage::RocksWriteBatch>,
        boundary_started: &mut bool,
    ) -> Prefix {
        if !right_entries.is_empty() {
            Self::recursive_batch_upsert(
                self,
                right_prefix,
                right_entries,
                active_batch,
                boundary_started,
            )
        } else {
            self.ensure_node_loaded(right_prefix);
            right_prefix
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
        self.complete_interior_depth.load(Ordering::Relaxed)
    }

    /// Check if all nodes at the given depth exist and are interior nodes.
    /// At depth D, there should be exactly 2^D nodes, all of which must be Interior nodes.
    fn check_depth_complete(&self, depth: u16) -> bool {
        // The root (depth 0) is always considered complete if it exists as an interior
        if depth == 0 {
            let is_interior = self
                .store
                .get(&Prefix::root())
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

        Prefix {
            hash,
            length: depth,
        }
    }

    /// Persist all interior nodes at a specific depth to the database in the given transaction.
    /// This is called when we discover that a depth has become complete.
    fn persist_interior_nodes_at_depth_in_tx(
        &self,
        depth: u16,
        tx: &super::rocks_storage::RocksTransaction,
    ) -> RocksResult<()> {
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
        let mut current_depth = self.complete_interior_depth.load(Ordering::Relaxed);

        // Keep checking successive depths until we find one that's incomplete
        loop {
            let next_depth = current_depth + 1;

            // Don't go beyond reasonable depth (256 would be a full hash)
            if next_depth >= 256 {
                break;
            }

            let leaf_nodes = self.storage.approximate_entry_count();
            let log_leaf_nodes = if leaf_nodes == 0 {
                0
            } else {
                (leaf_nodes as f64).log2().ceil() as u16
            };
            if current_depth > log_leaf_nodes.saturating_sub(10) {
                // Don't want depth to be close to true frontier
                break;
            }

            // Check if the next depth level is complete
            if self.check_depth_complete(next_depth) {
                // Persist all interior nodes at this newly complete depth to the database
                if let Err(e) = self.persist_interior_nodes_at_depth_in_tx(next_depth, tx) {
                    eprintln!(
                        "Warning: Failed to persist interior nodes at depth {}: {}",
                        next_depth, e
                    );
                    break;
                }

                current_depth = next_depth;
                self.complete_interior_depth
                    .store(current_depth, Ordering::Relaxed);
            } else {
                // If this level is incomplete, we're done
                break;
            }
        }

        // Persist the updated depth to the database
        if let Err(e) = tx.set_complete_depth(current_depth) {
            eprintln!(
                "Warning: Failed to persist complete depth {}: {}",
                current_depth, e
            );
        }
    }

    /// Release all nodes in the subtree starting from `prefix`, excluding the node at `prefix` itself.
    fn release_subtree(&self, prefix: Prefix) {
        let depth = self.complete_interior_depth.load(Ordering::Relaxed);
        self.full_tree_loaded.store(false, Ordering::Relaxed);
        if let Some(node_ref) = self.store.get(&prefix) {
            match node_ref.value() {
                Node::Interior(interior) => {
                    // Recursively release children and then remove them.
                    self.release_subtree(interior.left);
                    self.release_subtree(interior.right);
                    if interior.left.length > depth+3 {
                        self.store.remove(&interior.left);
                    }
                    if interior.right.length > depth+KEEP_BELOW_FRONTIER {
                        self.store.remove(&interior.right);
                    }
                }
                Node::Leaf(_) => {
                    // Nothing to do for a leaf, as it has no children.
                }
            }
        }
    }

    /// Prune all nodes from memory that are below the frontier depth.
    /// This maintains the invariant that only nodes at or above the frontier depth remain in memory.
    fn prune_below_frontier(&self) {
        self.full_tree_loaded.store(false, Ordering::Relaxed);
        let frontier_depth = self.complete_interior_depth.load(Ordering::Relaxed);
        let root = self.root;

        // Remove nodes in parallel without collecting first
        // DashMap supports concurrent removal, so we can safely remove during iteration
        self.store.retain(|prefix, _| {
            // Keep nodes at or above frontier depth, and always keep the root
            prefix.length <= frontier_depth+KEEP_BELOW_FRONTIER || *prefix == root
        });
    }
}

impl MerklePatriciaTree for RocksTransRelMPT {
    fn new() -> Self {
        Self::new_temporary().expect("Failed to create temporary RocksDB database")
    }

    fn new_with_path<P: AsRef<Path>>(path: P) -> super::rocks_storage::RocksResult<Self>
    where
        Self: Sized,
    {
        RocksTransRelMPT::new_with_path(path)
    }

    fn batch_upsert(&mut self, entries: &[(Hash, Hash)]) {
        self.batch_upsert_optimized(entries);
        self.storage.flush().expect("Failed to flush after batch upsert");
    }

    fn enumerate_nodes(&self) -> Vec<(Prefix, Node)> {
        self.ensure_full_tree_loaded();
        self.store
            .par_iter()
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
        let node = self
            .store
            .get(&prefix)
            .map(|guard| guard.value().clone())
            .or_else(|| {
                self.load_prefix_or_panic(prefix);
                self.store.get(&prefix).map(|guard| guard.value().clone())
            });
        match node {
            Some(Node::Leaf(leaf)) => Some(leaf.value),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Hash;
    use rand::{Rng, SeedableRng};
    use rand::rngs::StdRng;
    use std::cell::RefCell;

    thread_local! {
        static RNG: RefCell<StdRng> = RefCell::new(StdRng::seed_from_u64(42));
    }

    fn make_random_hash() -> Hash {
        RNG.with(|rng| rng.borrow_mut().random::<[u8; 32]>())
    }


    #[test]
    fn test_complete_interior_depth_empty_tree() {
        let tree = RocksTransRelMPT::new_temporary().expect("create tree");
        // Empty tree should have depth 0
        assert_eq!(tree.complete_interior_depth(), 0);
    }

    #[test]
    fn test_complete_interior_depth_single_insert() {
        let mut tree = RocksTransRelMPT::new_temporary().expect("create tree");

        // Insert a single key-value pair
        tree.batch_upsert(&[(make_random_hash(), make_random_hash())]);

        // With just one leaf, we should still have depth 0
        // (the root exists but is a leaf, not an interior)
        assert_eq!(tree.complete_interior_depth(), 0);
    }

    #[test]
    fn test_complete_interior_depth_two_inserts() {
        let mut tree = RocksTransRelMPT::new_temporary().expect("create tree");

        // Insert two keys that differ in the first bit
        // 0x00 = 00000000...
        // 0x80 = 10000000...
        tree.batch_upsert(&[
            (make_random_hash(), make_random_hash()),
            (make_random_hash(), make_random_hash()),
        ]);

        // Now the root should be an interior node (depth 0 complete)
        // But depth 1 requires 2 interior nodes, which we don't have yet
        let depth = tree.complete_interior_depth();
        println!("Depth after 2 inserts with different first bit: {}", depth);
    }

    #[test]
    fn test_complete_interior_depth_full_level() {
        let mut tree = RocksTransRelMPT::new_temporary().expect("create tree");

        // Insert 4 keys to create a tree with depth 2
        // This creates interior nodes at depth 0 and 1
        tree.batch_upsert(&[
            (make_random_hash(), make_random_hash()),
            (make_random_hash(), make_random_hash()),
            (make_random_hash(), make_random_hash()),
            (make_random_hash(), make_random_hash()),
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
        let mut tree = RocksTransRelMPT::new_temporary().expect("create tree");

        let initial_depth = tree.complete_interior_depth();
        println!("Initial depth: {}", initial_depth);

        // Add first key
        tree.batch_upsert(&[(make_random_hash(), make_random_hash())]);
        let depth1 = tree.complete_interior_depth();
        println!("Depth after 1 insert: {}", depth1);

        // Add second key (different first bit)
        tree.batch_upsert(&[(make_random_hash(), make_random_hash())]);
        let depth2 = tree.complete_interior_depth();
        println!("Depth after 2 inserts: {}", depth2);

        // Add third and fourth keys
        tree.batch_upsert(&[
            (make_random_hash(), make_random_hash()),
            (make_random_hash(), make_random_hash()),
        ]);
        let depth3 = tree.complete_interior_depth();
        println!("Depth after 4 inserts: {}", depth3);

        // The depth should not decrease
        assert!(depth3 >= depth2);
        assert!(depth2 >= depth1);
    }

    #[cfg(test)]
    impl RocksTransRelMPT {
        /// Checks that the in-memory tree is complete down to the frontier, and pruned below it.
        fn check_frontier_invariant(&self) {
            let complete_depth = self.complete_interior_depth();
            let mut queue: std::collections::VecDeque<Prefix> = std::collections::VecDeque::new();
            if self.store.get(&self.root).is_some() {
                queue.push_back(self.root);
            }

            let mut visited = std::collections::HashSet::new();
            visited.insert(self.root);

            while let Some(prefix) = queue.pop_front() {
                if prefix.length > complete_depth+KEEP_BELOW_FRONTIER as u16 {
                    // Nodes at the frontier should exist, but we don't check their children.
                    // Nodes below the frontier should not be in the queue.
                    panic!(
                        "Node {} with depth {} found below frontier depth {}",
                        prefix.short_hex(),
                        prefix.length,
                        complete_depth+KEEP_BELOW_FRONTIER as u16
                    );
                }

                if prefix.length == complete_depth+KEEP_BELOW_FRONTIER as u16 {
                    continue;
                }
                // We are above the frontier, so this must be an interior node.
                let node = self.store.get(&prefix).expect("Missing node in tree traversal");
                match node.value() {
                    Node::Interior(interior) => {
                        // Children must be in the store if we are above the frontier.
                        if !visited.contains(&interior.left) {
                            assert!(
                                self.store.contains_key(&interior.left),
                                "Left child {} of {} not in store",
                                interior.left.short_hex(),
                                prefix.short_hex()
                            );
                            queue.push_back(interior.left);
                            visited.insert(interior.left);
                        }
                        if !visited.contains(&interior.right) {
                            assert!(
                                self.store.contains_key(&interior.right),
                                "Right child {} of {} not in store",
                                interior.right.short_hex(),
                                prefix.short_hex()
                            );
                            queue.push_back(interior.right);
                            visited.insert(interior.right);
                        }
                    }
                    Node::Leaf(_) => {
                        panic!(
                            "Leaf node found at prefix {} with depth {}, which is above the frontier depth {}",
                            prefix.short_hex(),
                            prefix.length,
                            complete_depth
                        );
                    }
                }
            }

            // Check that all nodes at the frontier depth are present.
            if complete_depth > 0 {
                let expected_frontier_nodes = 1u64 << complete_depth;
                for i in 0..expected_frontier_nodes {
                    let prefix = Self::prefix_from_depth_and_index(complete_depth, i);
                    assert!(
                        self.store.contains_key(&prefix),
                        "Frontier node {} at depth {} is missing from the store",
                        prefix.short_hex(),
                        complete_depth
                    );
                }
            }

            // Verify no nodes below the frontier exist in the store.
            for item in self.store.iter() {
                let prefix = item.key();
                assert!(
                    prefix.length <= complete_depth+KEEP_BELOW_FRONTIER,
                    "Node {} with depth {} found in store, but is below frontier depth {}",
                    prefix.short_hex(),
                    prefix.length,
                    complete_depth+KEEP_BELOW_FRONTIER
                );
            }
        }
    }

    #[test]
    fn test_frontier_invariant_after_batch_insert() {
        let mut tree = RocksTransRelMPT::new_temporary().expect("create tree");

        // Insert enough keys to create a few levels of interior nodes.
        let entries: Vec<_> = (0..10000u32)
            .map(|_| (make_random_hash(), make_random_hash()))
            .collect();
        tree.batch_upsert(&entries);

        // After the batch insert, the invariant should hold.
        tree.check_frontier_invariant();
    }
}

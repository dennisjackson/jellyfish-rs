//! The RocksDB-backed tree (DESIGN.md §2–§5).
//!
//! Disk holds leaves plus one persisted interior level, the frontier F. [`Levels`] holds
//! every hash for depths `0..=F+1` and caches deeper ones. A batch is one positional descent
//! that stages nothing; what it writes is staged afterwards, then any newly complete level,
//! then the metadata.

use log::{debug, info};
use rayon::{join, prelude::*};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::census::{CensusSnapshot, Metric};
use crate::prefix::Side;
use crate::{Digest, Entry, Key, Prefix, Value, hash};

mod levels;
// `pub`: `compact-db` and tests/full_recovery_memory.rs use the storage layer directly.
pub mod storage;
#[cfg(test)]
mod tests;

use levels::{Levels, Position, Slot, levels_bytes};
use storage::{LEAVES_PER_BLOCK, RocksResult, RocksStorage, RocksStorageError, RocksWriteBatch};

/// A ceiling on the tree top held in memory and the deepest frontier to persist. Both are
/// limits, not targets: the depth actually held is [`Self::deepest_level`] (DESIGN.md §8).
#[derive(Clone, Copy, Debug)]
pub struct RocksFrontierConfig {
    max_depth: u16,
    frontier_cap: u16,
    /// [`LEAVES_PER_BLOCK`]; 1 in tests, so a few hundred leaves exercise every level rule.
    leaves_per_block: u64,
}

impl Default for RocksFrontierConfig {
    fn default() -> Self {
        Self::with_max_depth(Self::DEFAULT_MAX_DEPTH)
    }
}

impl RocksFrontierConfig {
    /// Production ceiling: 4.4 GB of tree top, frontier at up to 24.
    pub const DEFAULT_MAX_DEPTH: u16 = 26;

    /// Levels `0..=max_depth`, frontier capped at `max_depth - 2`. Panics outside
    /// `2..=MAX_DEPTH`.
    pub fn with_max_depth(max_depth: u16) -> Self {
        Self::with_depths(max_depth, max_depth.saturating_sub(2))
    }

    /// Panics unless `max_depth` is in `2..=MAX_DEPTH` and `frontier_cap < max_depth` (the
    /// frontier's children must be held).
    pub fn with_depths(max_depth: u16, frontier_cap: u16) -> Self {
        assert!(
            (2..=MAX_DEPTH).contains(&max_depth),
            "max_depth must be in 2..={MAX_DEPTH}, got {max_depth}: {MAX_DEPTH} levels already \
             hold {:.1} GB",
            levels_bytes(MAX_DEPTH) as f64 / 1e9
        );
        assert!(
            frontier_cap < max_depth,
            "a frontier at {frontier_cap} needs its children at {} held, against a \
             max_depth of {max_depth}",
            frontier_cap + 1
        );
        Self {
            max_depth,
            frontier_cap,
            leaves_per_block: LEAVES_PER_BLOCK,
        }
    }

    fn frontier_cap(&self) -> u16 {
        self.frontier_cap
    }

    /// Deepest level to hold: the frontier's children, plus the gate level while the frontier
    /// can still advance, or the block floor if deeper, capped at `max_depth`. Monotone in
    /// both arguments: the levels never shrink.
    fn deepest_level(&self, frontier_depth: u16, leaves: u64) -> u16 {
        let gate = if frontier_depth < self.frontier_cap {
            2
        } else {
            1
        };
        (frontier_depth + gate).max(block_floor(leaves, self.leaves_per_block).min(self.max_depth))
    }

    #[cfg(test)]
    pub fn test_config() -> Self {
        Self {
            leaves_per_block: 1,
            ..Self::with_depths(6, 4)
        }
    }
}

/// Shallowest depth at which a positional subtree's leaves fit one data block; past it a
/// deeper level buys nothing (DESIGN.md §2). Zero for a tree of at most one block.
fn block_floor(leaves: u64, leaves_per_block: u64) -> u16 {
    let blocks = leaves.div_ceil(leaves_per_block.max(1));
    blocks
        .checked_sub(1)
        .map_or(0, |below| 64 - below.leading_zeros()) as u16
}

/// Entries above which `upsert` splits across `rayon::join`. Swept 2..=64 (REVIEW.md §15.2).
const JOIN_THRESHOLD: usize = 8;

/// Deepest level any build holds: 17.7 GB of tree top. Refused at configuration and at open.
const MAX_DEPTH: u16 = 28;

/// Frontier rows per `WriteBatch` when a level is persisted (~9 MB encoded).
const PERSIST_CHUNK_NODES: u64 = 1 << 16;

/// Split a sorted batch at `bit`: keys with the bit clear, then the rest.
fn split_at_bit(entries: &[Entry], bit: u16) -> (&[Entry], &[Entry]) {
    let middle = entries.partition_point(|(key, _)| !key.get_bit(bit));
    entries.split_at(middle)
}

pub struct RocksFrontierMPT {
    storage: RocksStorage,
    /// F: every position at F holds an interior node. Written only by `advance_frontier`.
    frontier: u16,
    /// Exact for this process; a lower bound across a crash (DESIGN.md §4). `Relaxed`: read
    /// only after the joins that incremented it have returned.
    leaf_count: AtomicU64,
    levels: Levels,
    config: RocksFrontierConfig,
    /// Set for the duration of a batch. A panic mid-batch leaves the tree top and count
    /// ahead of disk, so every later call refuses; drop and reopen (DESIGN.md §4).
    poisoned: bool,
    /// Debug check that the descent enters each position at most once per batch
    /// (DESIGN.md §3).
    #[cfg(debug_assertions)]
    visited: std::sync::Mutex<std::collections::HashSet<Position>>,
}

impl RocksFrontierMPT {
    /// Open or create. Anything on disk outside the format is refused, not guessed at, and
    /// left as found (DESIGN.md §6, §9).
    pub fn open(path: impl AsRef<Path>, config: RocksFrontierConfig) -> RocksResult<Self> {
        let storage = RocksStorage::open(path)?;
        let (leaves, frontier) = match storage.read_metadata()? {
            Some((_, frontier)) if frontier > MAX_DEPTH - 2 => {
                return Err(RocksStorageError::corrupt(format!(
                    "the metadata names frontier depth {frontier}, whose tree top would \
                     need {} levels against the {MAX_DEPTH} this build can hold, so the \
                     metadata is corrupt or from a larger build. Refusing to open",
                    u32::from(frontier) + 3
                )));
            }
            Some(metadata) => metadata,
            // No batch ever committed: small by construction, so count exactly.
            None => (storage.count_leaves_by_prefix(&Prefix::root())? as u64, 0),
        };
        // Interior rows without metadata naming them: a stripped database, not a small tree.
        if frontier == 0 && storage.has_interior_rows()? {
            return Err(RocksStorageError::corrupt(
                "the metadata records no frontier, but interior rows exist at depths \
                 1..=255: the metadata was lost from a database with a frontier, or an \
                 older build's advance was torn. Refusing to open; restore the metadata or \
                 rebuild the database",
            ));
        }

        let tree = Self {
            storage,
            leaf_count: AtomicU64::new(leaves),
            frontier,
            levels: Levels::new(config.deepest_level(frontier, leaves)),
            config,
            poisoned: false,
            #[cfg(debug_assertions)]
            visited: Default::default(),
        };
        if frontier >= 1 {
            tree.load_frontier_level(frontier)?;
        }
        if log::log_enabled!(log::Level::Info) {
            let frontier_nodes = 1u64.checked_shl(u32::from(frontier)).unwrap_or(u64::MAX);
            info!(
                "Initialized RocksFrontierMPT with complete depth {frontier}, leaves {leaves}, \
                 leaves per frontier {}",
                leaves / frontier_nodes
            );
        }
        Ok(tree)
    }

    /// Fill depth `F + 1` from the persisted rows at `depth`, in parallel ranges, then derive
    /// every level above. The decoder rejects non-positional rows and RocksDB yields distinct
    /// keys in order, so the one check left is the row count.
    fn load_frontier_level(&self, depth: u16) -> RocksResult<()> {
        debug_assert!(depth >= 1, "a frontier level to rebuild from sits below 0");
        let expected = 1u64 << depth;
        // Ranges sized by stride so none starts past the level (DESIGN.md §9).
        let per_range = expected.div_ceil(rayon::current_num_threads() as u64 * 4);
        let ranges = expected.div_ceil(per_range);

        let rows = (0..ranges)
            .into_par_iter()
            .map(|range| -> RocksResult<u64> {
                let start = Position::new(depth, range * per_range).prefix();
                let end = ((range + 1) * per_range).min(expected);
                let end = (end < expected).then(|| Position::new(depth, end).prefix());
                let mut rows = 0u64;
                self.storage.for_each_frontier_row(
                    depth,
                    &start,
                    end.as_ref(),
                    |prefix, left, right| {
                        let position = Position::of(&prefix);
                        rows += 1;
                        self.levels
                            .set(position.child(Side::Left), Slot::Hash(left));
                        self.levels
                            .set(position.child(Side::Right), Slot::Hash(right));
                        Ok(())
                    },
                )?;
                Ok(rows)
            })
            .try_reduce(|| 0, |a, b| Ok(a + b))?;

        if rows != expected {
            return Err(RocksStorageError::corrupt(format!(
                "the metadata names frontier level {depth}, but the level on disk holds \
                 {rows} of the {expected} rows a complete level has. Refusing to guess; \
                 restore the level or rebuild the database"
            )));
        }
        info!("Loaded frontier level {depth} ({rows} rows) from storage");

        for depth in (0..=depth).rev() {
            (0..(1u64 << depth)).into_par_iter().for_each(|index| {
                let position = Position::new(depth, index);
                let hash = hash::interior(
                    position.prefix(),
                    self.levels.hash(position.child(Side::Left)),
                    self.levels.hash(position.child(Side::Right)),
                );
                self.levels.set(position, Slot::Hash(hash));
            });
        }
        Ok(())
    }

    /// Apply a batch, persist any frontier advance and the metadata, flush the WAL. Durable
    /// per call, not atomic; the commit order is the crash story (DESIGN.md §4). Panics on a
    /// write failure and poisons the tree.
    pub fn batch_upsert(&mut self, entries: &[Entry]) {
        if entries.is_empty() {
            return;
        }
        self.refuse_if_poisoned();
        self.poisoned = true;

        #[cfg(debug_assertions)]
        self.visited.lock().unwrap().clear();

        let entries = super::sorted_unique_entries(entries);

        debug!(
            "apply_batch start entries={} complete_depth={}",
            entries.len(),
            self.frontier
        );
        // Size the tree top for the leaves this batch can add.
        let leaves_after = self.leaf_count.load(Ordering::Relaxed) + entries.len() as u64;
        self.levels
            .ensure_depth(self.config.deepest_level(self.frontier, leaves_after));
        self.upsert(Position::ROOT, &entries);

        let storage = &self.storage;
        self.stage_batches(&entries)
            .into_par_iter()
            .for_each(|batch| {
                storage
                    .write_batch(batch)
                    .expect("Failed to commit frontier subtree batch");
            });

        let frontier_depth = self
            .advance_frontier()
            .expect("Failed to persist a newly complete frontier level");

        self.storage
            .commit_metadata(self.leaf_count.load(Ordering::Relaxed), frontier_depth)
            .expect("Failed to commit metadata update");
        self.storage
            .flush()
            .expect("Failed to flush after batch upsert");
        self.poisoned = false;
    }

    /// Everything the batch writes, in about thread-count write batches: every entry once as
    /// a leaf record, one frontier row per touched subtree from the hashes at `F + 1`. Only
    /// after [`Self::upsert`] has returned. Cuts are advanced to subtree boundaries so a
    /// subtree's leaves and its row share a batch — a crash-safety invariant, since the
    /// batches commit independently (DESIGN.md §3–§4).
    fn stage_batches(&self, entries: &[Entry]) -> Vec<RocksWriteBatch> {
        let depth = self.frontier;
        debug_assert!(!entries.is_empty(), "caller guarantees entries");
        let index_of = |key: &Key| Position::index_of(key, depth);

        let stride = entries
            .len()
            .div_ceil(rayon::current_num_threads().max(1))
            .max(1);
        let mut bounds = vec![0usize];
        let mut at = stride;
        while at < entries.len() {
            let index = index_of(&entries[at - 1].0);
            while at < entries.len() && index_of(&entries[at].0) == index {
                at += 1;
            }
            if at < entries.len() {
                bounds.push(at);
            }
            at += stride;
        }
        bounds.push(entries.len());

        bounds
            .par_windows(2)
            .map(|window| {
                let range = &entries[window[0]..window[1]];
                let mut batch = RocksWriteBatch::default();
                for group in range.chunk_by(|(a, _), (b, _)| index_of(a) == index_of(b)) {
                    for (key, value) in group {
                        batch.put_leaf(key, value);
                    }
                    if depth >= 1 {
                        let position = Position::new(depth, index_of(&group[0].0));
                        batch.put_frontier_node(
                            &position.prefix(),
                            self.levels.hash(position.child(Side::Left)),
                            self.levels.hash(position.child(Side::Right)),
                        );
                    }
                }
                batch
            })
            .collect()
    }

    /// Apply `entries` (non-empty, sorted, unique, under `position`) to the subtree there,
    /// record its hash and return it. Parallel above the frontier; at the first position
    /// whose children are unknown the subtree is merged from disk. Children are addressed by
    /// position: a compressed child prefix covers the same leaf range, so the scan is the
    /// same (DESIGN.md §3).
    fn upsert(&self, position: Position, entries: &[Entry]) -> Digest {
        debug_assert!(!entries.is_empty(), "caller guarantees entries");
        debug_assert!(
            entries
                .iter()
                .all(|(key, _)| position.prefix().contains(key)),
            "entries outside {position:?}"
        );
        self.note_visit(position);
        let Some((left, right)) = self.levels.children(position) else {
            let merged = self.merge_with_disk(position.prefix(), entries);
            return self.build_subtree(position, &merged);
        };

        let (left_entries, right_entries) = split_at_bit(entries, position.depth);
        let left_child = position.child(Side::Left);
        let right_child = position.child(Side::Right);
        let parallel = position.depth < self.frontier && entries.len() > JOIN_THRESHOLD;
        let (left, right) = if parallel {
            join(
                || self.upsert_child(left_child, left_entries, left),
                || self.upsert_child(right_child, right_entries, right),
            )
        } else {
            (
                self.upsert_child(left_child, left_entries, left),
                self.upsert_child(right_child, right_entries, right),
            )
        };

        let hash = Self::combine_children(position, left, right);
        self.levels.set(position, Slot::Hash(hash));
        hash
    }

    /// Untouched: the stored slot. Touched and hashed: recurse. Touched and proven empty:
    /// build from the batch alone, no scan — sound because positions are visited once per
    /// batch and there is no delete path.
    fn upsert_child(&self, position: Position, entries: &[Entry], stored: Slot) -> Slot {
        if entries.is_empty() {
            return stored;
        }
        if let Slot::Hash(_) = stored {
            return Slot::Hash(self.upsert(position, entries));
        }
        debug_assert!(
            self.storage
                .get_leaf_entries_by_prefix(&position.prefix())
                .map(|leaves| leaves.is_empty())
                .unwrap_or(false),
            "the levels claimed {position:?} empty, but disk disagrees"
        );
        self.leaf_count
            .fetch_add(entries.len() as u64, Ordering::Relaxed);
        Slot::Hash(self.build_subtree(position, entries))
    }

    /// `entries` merged over the leaves on disk under `prefix`, the batch winning on equal
    /// keys; keys not on disk are counted as new leaves.
    fn merge_with_disk(&self, prefix: Prefix, entries: &[Entry]) -> Vec<Entry> {
        let loaded = self
            .storage
            .get_leaf_entries_by_prefix(&prefix)
            .unwrap_or_else(|err| Self::panic_load_failed(&prefix, &err));
        if !loaded.is_empty() {
            let census = self.storage.census();
            census[Metric::SubtreeLoads].bump();
            census[Metric::LeavesReadByLoads].add(loaded.len() as u64);
        }

        let mut merged: Vec<Entry> = Vec::with_capacity(loaded.len() + entries.len());
        let mut new_leaves = 0u64;
        let mut on_disk = loaded.into_iter().peekable();
        for &entry in entries {
            while let Some(&below) = on_disk.peek()
                && below.0 < entry.0
            {
                merged.push(below);
                on_disk.next();
            }
            if on_disk.next_if(|same| same.0 == entry.0).is_none() {
                new_leaves += 1;
            }
            merged.push(entry);
        }
        merged.extend(on_disk);

        self.leaf_count.fetch_add(new_leaves, Ordering::Relaxed);
        merged
    }

    /// Hash of the compressed root over `entries` (non-empty, sorted, unique, under
    /// `position`), recording every covered position. Bit-identical to
    /// [`Self::subtree_hash`]: a two-sided split at `depth` is a compressed root there, a
    /// one-sided split a pass-through. Below the deepest level it hands over to the
    /// compressed recursion, which jumps pass-through runs longer than a `u64` index.
    fn build_subtree(&self, position: Position, entries: &[Entry]) -> Digest {
        debug_assert!(!entries.is_empty());
        let hash = if position.depth >= self.levels.deepest() {
            Self::subtree_hash(entries)
        } else {
            let (left_entries, right_entries) = split_at_bit(entries, position.depth);
            let left = self.build_child(position.child(Side::Left), left_entries);
            let right = self.build_child(position.child(Side::Right), right_entries);
            Self::combine_children(position, left, right)
        };
        self.levels.set(position, Slot::Hash(hash));
        hash
    }

    fn build_child(&self, position: Position, entries: &[Entry]) -> Slot {
        if entries.is_empty() {
            self.levels.set(position, Slot::Empty);
            Slot::Empty
        } else {
            Slot::Hash(self.build_subtree(position, entries))
        }
    }

    /// Both children: an interior at this depth. One: its hash passes through.
    fn combine_children(position: Position, left: Slot, right: Slot) -> Digest {
        match (left, right) {
            (Slot::Hash(left), Slot::Hash(right)) => hash::interior(position.prefix(), left, right),
            (Slot::Hash(hash), Slot::Empty) | (Slot::Empty, Slot::Hash(hash)) => hash,
            (Slot::Empty, Slot::Empty) => {
                unreachable!("entries is non-empty, so one child must be too")
            }
        }
    }

    /// Merkle hash of the compressed subtree over `entries` (non-empty, sorted, unique),
    /// materialising nothing: the common prefix of the first and last key is the slice's,
    /// and the bit after it splits the slice into two non-empty halves.
    fn subtree_hash(entries: &[Entry]) -> Digest {
        let (first_key, first_value) = entries[0];
        if entries.len() == 1 {
            return hash::leaf(first_key, first_value);
        }
        let last_key = entries[entries.len() - 1].0;
        let prefix = Prefix::common_prefix(&Prefix::from(first_key), &Prefix::from(last_key));
        let middle = entries.partition_point(|(key, _)| !prefix.key_goes_right(*key));
        debug_assert!(middle > 0 && middle < entries.len());
        hash::interior(
            prefix,
            Self::subtree_hash(&entries[..middle]),
            Self::subtree_hash(&entries[middle..]),
        )
    }

    /// Root of a frontierless tree untouched in this process: one leaf scan through
    /// [`Self::build_subtree`], which records it.
    fn small_tree_root_hash(&self) -> Option<Digest> {
        let entries = self
            .storage
            .get_leaf_entries_by_prefix(&Prefix::root())
            .unwrap_or_else(|err| Self::panic_load_failed(&Prefix::root(), &err));
        if entries.is_empty() {
            return None;
        }
        Some(self.build_subtree(Position::ROOT, &entries))
    }

    /// `depth` is within the cap and every position at `depth + 1` holds a hash.
    fn depth_is_complete(&self, depth: u16) -> bool {
        debug_assert!(depth >= 1, "completeness is asked below the root");
        if depth > self.config.frontier_cap() {
            return false;
        }
        self.levels.all_hashed(depth + 1)
    }

    /// Persist the level at `depth` as frontier rows, `chunk` per `WriteBatch`. The last
    /// chunk carries the metadata naming `depth`, so a level is complete on disk exactly when
    /// the metadata names it (DESIGN.md §4). Returns the batch count.
    fn persist_level(&self, depth: u16, chunk: u64) -> RocksResult<usize> {
        debug_assert!(chunk > 0, "persist chunk size must be non-zero");
        let mut batch = RocksWriteBatch::default();
        let mut staged = 0u64;
        let mut batches = 0usize;

        for index in 0..1u64 << depth {
            let position = Position::new(depth, index);
            if let Some((Slot::Hash(left), Slot::Hash(right))) = self.levels.children(position) {
                if staged == chunk {
                    self.storage.write_batch(std::mem::take(&mut batch))?;
                    staged = 0;
                    batches += 1;
                }
                batch.put_frontier_node(&position.prefix(), left, right);
                staged += 1;
            }
        }
        debug_assert!(staged > 0, "a complete level has rows");
        self.storage.write_batch_with_metadata(
            batch,
            self.leaf_count.load(Ordering::Relaxed),
            depth,
        )?;
        Ok(batches + 1)
    }

    /// Advance while the next depth is complete, persisting each level; nothing moves in
    /// memory. Returns the depth to record.
    fn advance_frontier(&mut self) -> RocksResult<u16> {
        loop {
            let next_depth = self.frontier + 1;
            if !self.depth_is_complete(next_depth) {
                break;
            }

            info!(
                "Advancing frontier depth to {} with nodes {}",
                next_depth,
                1u64 << next_depth
            );
            self.persist_level(next_depth, PERSIST_CHUNK_NODES)?;
            self.frontier = next_depth;
        }

        self.levels.ensure_depth(self.deepest_level());
        Ok(self.frontier)
    }

    /// `None` for an empty tree.
    pub fn get_root_hash(&self) -> Option<Digest> {
        self.refuse_if_poisoned();
        match self.levels.get(Position::ROOT) {
            Some(Slot::Hash(hash)) => Some(hash),
            _ => self.small_tree_root_hash(),
        }
    }

    pub fn get_leaf_value(&self, key: Key) -> Option<Value> {
        self.refuse_if_poisoned();
        match self.storage.get_leaf_value(&key) {
            Ok(value) => value,
            Err(err) => Self::panic_load_failed(&Prefix::from(key), &err),
        }
    }

    /// O(1). Exact for this process's batches; a lower bound across a crash (DESIGN.md §4).
    pub fn leaf_count(&self) -> usize {
        self.refuse_if_poisoned();
        usize::try_from(self.leaf_count.load(Ordering::Relaxed)).unwrap_or(usize::MAX)
    }

    pub fn frontier_depth(&self) -> u16 {
        self.refuse_if_poisoned();
        self.frontier
    }

    /// Deepest level actually held; derived from the tree, not the configured ceiling.
    pub fn levels_depth(&self) -> u16 {
        self.refuse_if_poisoned();
        self.levels.deepest()
    }

    /// Traffic so far in the current census phase, RocksDB's tickers included.
    pub fn census_snapshot(&self) -> CensusSnapshot {
        self.storage.census_snapshot()
    }

    pub fn census_reset(&self) {
        self.storage.census_reset();
    }

    /// Sorted runs in the LSM: the multiplier on every leaf-scan seek.
    pub fn sorted_runs(&self) -> u64 {
        self.storage.sorted_runs()
    }

    #[cfg(debug_assertions)]
    fn note_visit(&self, position: Position) {
        assert!(
            self.visited.lock().unwrap().insert(position),
            "{position:?} entered twice in one batch: the positional descent must reach each \
             position at most once, or a scan can miss a leaf this batch has staged"
        );
    }

    #[cfg(not(debug_assertions))]
    #[inline(always)]
    fn note_visit(&self, _position: Position) {}

    fn refuse_if_poisoned(&self) {
        assert!(
            !self.poisoned,
            "a previous batch_upsert panicked part-way: the in-memory tree top and leaf \
             count no longer describe the database. Drop this value and open the database \
             again"
        );
    }

    fn deepest_level(&self) -> u16 {
        self.config
            .deepest_level(self.frontier, self.leaf_count.load(Ordering::Relaxed))
    }

    /// A read failure mid-descent has no recovery path.
    fn panic_load_failed(prefix: &Prefix, err: &dyn std::fmt::Display) -> ! {
        panic!(
            "Failed to load prefix {} from RocksDB: {}",
            prefix.short_hex(),
            err
        )
    }
}

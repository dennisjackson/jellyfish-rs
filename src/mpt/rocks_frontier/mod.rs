//! The RocksDB-backed tree (DESIGN.md, HASHCHAINS.md).
//!
//! Disk holds leaf rows (a Merkle hash and a pointer into history each), every version of
//! every leaf, plus one persisted interior level, the frontier F. [`Levels`] holds every hash
//! for depths `0..=F+1` and caches deeper ones. A batch is one positional descent that stages
//! nothing to disk; the rows it derives are collected in a sink and written afterwards, then
//! any newly complete level, then the metadata.

use log::{debug, info};
use rayon::{join, prelude::*};
use std::ops::RangeBounds;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::census::{CensusSnapshot, Metric};
use crate::prefix::Side;
use crate::{Digest, Entry, Key, Prefix, Record, Value, hash};

mod levels;
// `pub`: `compact-db` and tests/full_recovery_memory.rs use the storage layer directly.
pub mod storage;
#[cfg(test)]
mod tests;

use levels::{Levels, Position, Slot, levels_bytes};
use storage::{
    LEAF_PREFIX_BITS, LEAVES_PER_BLOCK, LeafRow, RocksResult, RocksStorage, RocksStorageError,
    RocksWriteBatch, Version,
};

/// A ceiling on the tree top held in memory and the deepest frontier to persist. Both are
/// limits, not targets: the depth actually held is [`Self::deepest_level`] (DESIGN.md, Representation).
#[derive(Clone, Copy, Debug)]
pub struct RocksFrontierConfig {
    max_depth: u16,
    frontier_cap: u16,
    /// [`LEAVES_PER_BLOCK`]; 1 in tests, so a few hundred leaves exercise every level rule.
    leaves_per_block: u64,
    /// Levels held whatever the tree's size, so that every subtree scan starts at least this
    /// deep: at [`LEAF_PREFIX_BITS`] the scan's prefix is the bloom filters' prefix and each
    /// sorted run without a leaf under it is skipped (HASHCHAINS.md, Database Schema). Zero
    /// in tests, where the levels must follow the tree.
    scan_floor: u16,
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
            scan_floor: LEAF_PREFIX_BITS.min(max_depth),
        }
    }

    /// Hold at least `depth` levels from the start (see `scan_floor`). Zero lets the levels
    /// follow the tree alone, which is what the memory tests measure. Panics past
    /// `max_depth`.
    pub fn with_scan_floor(mut self, depth: u16) -> Self {
        assert!(
            depth <= self.max_depth,
            "a scan floor at {depth} exceeds the max_depth of {}",
            self.max_depth
        );
        self.scan_floor = depth;
        self
    }

    fn frontier_cap(&self) -> u16 {
        self.frontier_cap
    }

    /// Deepest level to hold: the frontier's children, plus the gate level while the frontier
    /// can still advance, or the block floor if deeper, capped at `max_depth`; never above
    /// the scan floor. Monotone in both arguments: the levels never shrink.
    fn deepest_level(&self, frontier_depth: u16, leaves: u64) -> u16 {
        let gate = if frontier_depth < self.frontier_cap {
            2
        } else {
            1
        };
        (frontier_depth + gate)
            .max(block_floor(leaves, self.leaves_per_block).min(self.max_depth))
            .max(self.scan_floor)
    }

    #[cfg(test)]
    pub fn test_config() -> Self {
        Self {
            leaves_per_block: 1,
            ..Self::with_depths(6, 4).with_scan_floor(0)
        }
    }
}

/// Shallowest depth at which a positional subtree's leaves fit one data block; past it a
/// deeper level buys nothing (DESIGN.md, Representation). Zero for a tree of at most one block.
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

/// Anything the descent partitions by key: the batch's entries and the leaf rows it derives.
trait Keyed {
    fn key(&self) -> Key;
}

impl Keyed for Entry {
    fn key(&self) -> Key {
        self.0
    }
}

impl Keyed for LeafRow {
    fn key(&self) -> Key {
        self.key
    }
}

/// Split a sorted slice at `bit`: keys with the bit clear, then the rest.
fn split_at_bit<T: Keyed>(items: &[T], bit: u16) -> (&[T], &[T]) {
    let middle = items.partition_point(|item| !item.key().get_bit(bit));
    items.split_at(middle)
}

/// Extend `key`'s chain by one link per entry of `run` (one key, slice order), from the leaf
/// on disk if any, at consecutive sequence numbers from `seqs`; every version is pushed to
/// `versions`. Returns the new leaf row. Nothing is read: the old leaf row holds the hash
/// the first new link needs.
fn extend_chain(
    key: Key,
    head: Option<LeafRow>,
    run: &[Entry],
    seqs: &AtomicU64,
    versions: &mut Vec<Version>,
) -> LeafRow {
    debug_assert!(run.iter().all(|(k, _)| *k == key), "a run holds one key");
    let first_seq = seqs.fetch_add(run.len() as u64, Ordering::Relaxed);
    let mut head = head;
    for (&(_, value), seq) in run.iter().zip(first_seq..) {
        let next = match head {
            None => Version::first(key, value, seq),
            Some(previous) => previous.next(value, seq),
        };
        versions.push(next);
        head = Some(next.leaf_row());
    }
    head.expect("a run is non-empty")
}

pub struct RocksFrontierMPT {
    storage: RocksStorage,
    /// F: every position at F holds an interior node. Written only by `advance_frontier`.
    frontier: u16,
    /// Exact for this process; a lower bound across a crash (DESIGN.md, Safety and Correctness). `Relaxed`: read
    /// only after the joins that incremented it have returned.
    leaf_count: AtomicU64,
    /// The next history sequence number. Recovered at open from the last row on disk, so it
    /// needs no metadata; rows a torn batch committed are simply continued from.
    next_seq: AtomicU64,
    levels: Levels,
    config: RocksFrontierConfig,
    /// Every version the current batch derived, one group per merged or freshly built
    /// subtree, each in key order; groups cover disjoint key ranges. Drained by
    /// [`Self::stage_batches`] (HASHCHAINS.md, Updating the Representation).
    staged: Mutex<Vec<Vec<Version>>>,
    /// Set for the duration of a batch. A panic mid-batch leaves the tree top and count
    /// ahead of disk, so every later call refuses; drop and reopen (DESIGN.md, Safety and Correctness).
    poisoned: bool,
    /// Debug check that the descent enters each position at most once per batch
    /// (DESIGN.md, Safety and Correctness).
    #[cfg(debug_assertions)]
    visited: std::sync::Mutex<std::collections::HashSet<Position>>,
}

impl RocksFrontierMPT {
    /// Open or create. Anything on disk outside the format is refused, not guessed at, and
    /// left as found (DESIGN.md, Safety and Correctness).
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
        let next_seq = storage.last_history_seq()? + 1;

        let tree = Self {
            storage,
            leaf_count: AtomicU64::new(leaves),
            next_seq: AtomicU64::new(next_seq),
            frontier,
            levels: Levels::new(config.deepest_level(frontier, leaves)),
            config,
            staged: Mutex::new(Vec::new()),
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
                 leaves per frontier {}, {} history rows",
                leaves / frontier_nodes,
                next_seq - 1
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
        // Ranges sized by stride so none starts past the level.
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

    /// Apply a batch, persist any frontier advance and the metadata, flush the WAL. Entries
    /// are applied in slice order: each occurrence of a key adds one record to its chain, so
    /// a key that appears k times gains k versions and its leaf ends at the last
    /// (HASHCHAINS.md). Durable per call, not atomic; the commit order is the crash story
    /// (DESIGN.md, Safety and Correctness). Panics on a write failure and poisons the tree.
    pub fn batch_upsert(&mut self, entries: &[Entry]) {
        if entries.is_empty() {
            return;
        }
        self.refuse_if_poisoned();
        self.poisoned = true;

        #[cfg(debug_assertions)]
        self.visited.lock().unwrap().clear();
        debug_assert!(
            self.staged.lock().unwrap().is_empty(),
            "the previous batch left rows unstaged"
        );

        let entries = super::sorted_entries(entries);

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
        self.stage_batches().into_par_iter().for_each(|batch| {
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

    /// Everything the batch writes, in about thread-count write batches: every version the
    /// descent derived as a history row, the last version of each key as its leaf row, one
    /// frontier row per touched subtree from the hashes at `F + 1`. Only after
    /// [`Self::upsert`] has returned, which filled the sink. Cuts are advanced to subtree
    /// boundaries so a subtree's leaves, its history rows and its frontier row share a
    /// batch — a crash-safety invariant, since the batches commit independently (DESIGN.md,
    /// Updating the Representation).
    fn stage_batches(&self) -> Vec<RocksWriteBatch> {
        let depth = self.frontier;
        let index_of = |key: &Key| Position::index_of(key, depth);

        let mut groups = std::mem::take(&mut *self.staged.lock().unwrap());
        // Groups are disjoint key ranges, each in key order: sorting them by first key
        // makes the concatenation sorted by (key, version).
        groups.sort_unstable_by_key(|group| group.first().map(|version| version.key));
        let rows: Vec<Version> = groups.into_iter().flatten().collect();
        debug_assert!(
            rows.windows(2)
                .all(|pair| (pair[0].key, pair[0].version) < (pair[1].key, pair[1].version)),
            "the staged rows are not in (key, version) order"
        );
        debug_assert!(!rows.is_empty(), "a non-empty batch derives rows");

        let stride = rows
            .len()
            .div_ceil(rayon::current_num_threads().max(1))
            .max(1);
        let mut bounds = vec![0usize];
        let mut at = stride;
        while at < rows.len() {
            let index = index_of(&rows[at - 1].key);
            while at < rows.len() && index_of(&rows[at].key) == index {
                at += 1;
            }
            if at < rows.len() {
                bounds.push(at);
            }
            at += stride;
        }
        bounds.push(rows.len());

        bounds
            .par_windows(2)
            .map(|window| {
                let range = &rows[window[0]..window[1]];
                let mut batch = RocksWriteBatch::default();
                for subtree in range.chunk_by(|a, b| index_of(&a.key) == index_of(&b.key)) {
                    for chain in subtree.chunk_by(|a, b| a.key == b.key) {
                        for version in chain {
                            batch.put_history(version);
                        }
                        batch.put_leaf(&chain[chain.len() - 1].leaf_row());
                    }
                    if depth >= 1 {
                        let position = Position::new(depth, index_of(&subtree[0].key));
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

    /// Apply `entries` (non-empty, sorted, under `position`) to the subtree there, record its
    /// hash and return it. Parallel above the frontier; at the first position whose children
    /// are unknown the subtree is merged from disk, which is where the batch's runs of one
    /// key are folded onto the chain on disk. Children are addressed by position: a
    /// compressed child prefix covers the same leaf range, so the scan is the same (DESIGN.md,
    /// Updating the Representation).
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
                .get_leaf_rows_by_prefix(&position.prefix())
                .map(|leaves| leaves.is_empty())
                .unwrap_or(false),
            "the levels claimed {position:?} empty, but disk disagrees"
        );
        let leaves = self.fresh_leaves(entries);
        Slot::Hash(self.build_subtree(position, &leaves))
    }

    /// The batch's runs under a proven-empty position, each chained from genesis; every key
    /// is a new leaf.
    fn fresh_leaves(&self, entries: &[Entry]) -> Vec<LeafRow> {
        let mut versions = Vec::with_capacity(entries.len());
        let leaves: Vec<LeafRow> = entries
            .chunk_by(|a, b| a.0 == b.0)
            .map(|run| extend_chain(run[0].0, None, run, &self.next_seq, &mut versions))
            .collect();
        self.leaf_count
            .fetch_add(leaves.len() as u64, Ordering::Relaxed);
        self.staged.lock().unwrap().push(versions);
        leaves
    }

    /// `entries` merged over the leaves on disk under `prefix`: each run of one key extends
    /// the chain found on disk, or starts one; keys not on disk are counted as new leaves.
    /// Every version derived goes to the sink; the returned rows are the subtree's current
    /// leaves, unique and sorted.
    fn merge_with_disk(&self, prefix: Prefix, entries: &[Entry]) -> Vec<LeafRow> {
        let loaded = self
            .storage
            .get_leaf_rows_by_prefix(&prefix)
            .unwrap_or_else(|err| Self::panic_load_failed(&prefix, &err));
        if !loaded.is_empty() {
            let census = self.storage.census();
            census[Metric::SubtreeLoads].bump();
            census[Metric::LeavesReadByLoads].add(loaded.len() as u64);
        }

        let mut merged: Vec<LeafRow> = Vec::with_capacity(loaded.len() + entries.len());
        let mut versions: Vec<Version> = Vec::with_capacity(entries.len());
        let mut new_leaves = 0u64;
        let mut on_disk = loaded.into_iter().peekable();
        for run in entries.chunk_by(|a, b| a.0 == b.0) {
            let key = run[0].0;
            while let Some(&below) = on_disk.peek()
                && below.key < key
            {
                merged.push(below);
                on_disk.next();
            }
            let head = on_disk.next_if(|same| same.key == key);
            if head.is_none() {
                new_leaves += 1;
            }
            merged.push(extend_chain(key, head, run, &self.next_seq, &mut versions));
        }
        merged.extend(on_disk);

        self.leaf_count.fetch_add(new_leaves, Ordering::Relaxed);
        self.staged.lock().unwrap().push(versions);
        merged
    }

    /// Hash of the compressed root over `leaves` (non-empty, sorted, unique, under
    /// `position`), recording every covered position. Bit-identical to
    /// [`Self::subtree_hash`]: a two-sided split at `depth` is a compressed root there, a
    /// one-sided split a pass-through. Below the deepest level it hands over to the
    /// compressed recursion, which jumps pass-through runs longer than a `u64` index.
    fn build_subtree(&self, position: Position, leaves: &[LeafRow]) -> Digest {
        debug_assert!(!leaves.is_empty());
        let hash = if position.depth >= self.levels.deepest() {
            Self::subtree_hash(leaves)
        } else {
            let (left_leaves, right_leaves) = split_at_bit(leaves, position.depth);
            let left = self.build_child(position.child(Side::Left), left_leaves);
            let right = self.build_child(position.child(Side::Right), right_leaves);
            Self::combine_children(position, left, right)
        };
        self.levels.set(position, Slot::Hash(hash));
        hash
    }

    fn build_child(&self, position: Position, leaves: &[LeafRow]) -> Slot {
        if leaves.is_empty() {
            self.levels.set(position, Slot::Empty);
            Slot::Empty
        } else {
            Slot::Hash(self.build_subtree(position, leaves))
        }
    }

    /// Both children: an interior at this depth. One: its hash passes through.
    fn combine_children(position: Position, left: Slot, right: Slot) -> Digest {
        match (left, right) {
            (Slot::Hash(left), Slot::Hash(right)) => hash::interior(position.prefix(), left, right),
            (Slot::Hash(hash), Slot::Empty) | (Slot::Empty, Slot::Hash(hash)) => hash,
            (Slot::Empty, Slot::Empty) => {
                unreachable!("leaves is non-empty, so one child must be too")
            }
        }
    }

    /// Merkle hash of the compressed subtree over `leaves` (non-empty, sorted, unique),
    /// materialising nothing: the common prefix of the first and last key is the slice's,
    /// and the bit after it splits the slice into two non-empty halves. Leaf hashes are
    /// stored, so no leaf is rehashed here.
    fn subtree_hash(leaves: &[LeafRow]) -> Digest {
        if leaves.len() == 1 {
            return leaves[0].hash;
        }
        let first_key = leaves[0].key;
        let last_key = leaves[leaves.len() - 1].key;
        let prefix = Prefix::common_prefix(&Prefix::from(first_key), &Prefix::from(last_key));
        let middle = leaves.partition_point(|leaf| !prefix.key_goes_right(leaf.key));
        debug_assert!(middle > 0 && middle < leaves.len());
        hash::interior(
            prefix,
            Self::subtree_hash(&leaves[..middle]),
            Self::subtree_hash(&leaves[middle..]),
        )
    }

    /// Root of a frontierless tree untouched in this process: one leaf scan through
    /// [`Self::build_subtree`], which records it.
    fn small_tree_root_hash(&self) -> Option<Digest> {
        let leaves = self
            .storage
            .get_leaf_rows_by_prefix(&Prefix::root())
            .unwrap_or_else(|err| Self::panic_load_failed(&Prefix::root(), &err));
        if leaves.is_empty() {
            return None;
        }
        Some(self.build_subtree(Position::ROOT, &leaves))
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
    /// the metadata names it (DESIGN.md, Safety and Correctness). Returns the batch count.
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

    /// The current value under `key`; `None` if the key was never written. Two point reads:
    /// the leaf row, then the history row it names.
    pub fn get_leaf_value(&self, key: Key) -> Option<Value> {
        self.get_record(key).map(|(_, record)| record.value)
    }

    /// The current record under `key` and its version (the number of records before it);
    /// `None` if the key was never written.
    pub fn get_record(&self, key: Key) -> Option<(u64, Record)> {
        self.refuse_if_poisoned();
        match self.storage.get_record(&key) {
            Ok(record) => record,
            Err(err) => Self::panic_load_failed(&Prefix::from(key), &err),
        }
    }

    /// Every record ever written under `key`, oldest first, ending with the current one;
    /// empty if the key was never written. [`Record::verify_chain`] holds over the result.
    /// One point read per version, walking the chain backwards from the leaf.
    pub fn get_history(&self, key: Key) -> Vec<Record> {
        self.get_history_range(key, ..)
    }

    /// Versions `range` of `key`, oldest first. The walk starts at the newest version, so a
    /// range near the head costs its length; one near the genesis costs the whole chain.
    pub fn get_history_range(&self, key: Key, range: impl RangeBounds<u64>) -> Vec<Record> {
        self.refuse_if_poisoned();
        match self.storage.get_history(&key, range) {
            Ok(records) => records,
            Err(err) => Self::panic_load_failed(&Prefix::from(key), &err),
        }
    }

    /// O(1). Exact for this process's batches; a lower bound across a crash (DESIGN.md, Safety and Correctness).
    pub fn leaf_count(&self) -> usize {
        self.refuse_if_poisoned();
        usize::try_from(self.leaf_count.load(Ordering::Relaxed)).unwrap_or(usize::MAX)
    }

    /// History rows written so far, across every open of this database. O(1).
    pub fn version_count(&self) -> u64 {
        self.refuse_if_poisoned();
        self.next_seq.load(Ordering::Relaxed) - 1
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

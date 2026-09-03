//! The RocksDB tree against the oracle. Trees use [`RocksFrontierConfig::test_config`] unless
//! a test says otherwise; a [`Model`] carries the oracle and the contents, and after every
//! batch the tree must report the model's root, count and values and pass the levels'
//! invariants.

use super::levels::Slot;
use super::storage::{FRONTIER_RECORD_LEN, LEAF_RECORD_LEN, NODE_KEY_LEN};
use super::*;
use crate::census::Metric;
use crate::mpt::SimpleMPT;
use crate::prefix::Side;
use crate::testing::{TestRng, fresh_dir};
use crate::{Digest, Entry, Key, Value, hash};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

pub(super) fn open_test(path: impl AsRef<Path>) -> RocksResult<RocksFrontierMPT> {
    RocksFrontierMPT::open(path, RocksFrontierConfig::test_config())
}

fn fresh_tree() -> RocksFrontierMPT {
    open_test(fresh_dir()).expect("create tree")
}

thread_local! {
    static RNG: RefCell<TestRng> = const { RefCell::new(TestRng::seed(42)) };
}

fn random_bytes() -> [u8; 32] {
    RNG.with(|rng| rng.borrow_mut().bytes())
}

fn random_value() -> Value {
    Value(random_bytes())
}

pub(super) fn random_entries(n: usize) -> Vec<Entry> {
    (0..n)
        .map(|_| (Key(random_bytes()), random_value()))
        .collect()
}

/// A key whose first byte is `byte`, zero past it.
fn key(byte: u8) -> Key {
    let mut key = [0u8; 32];
    key[0] = byte;
    Key(key)
}

/// The oracle plus the contents it was built from.
#[derive(Default)]
pub(super) struct Model {
    pub(super) oracle: SimpleMPT,
    pub(super) contents: BTreeMap<Key, Value>,
}

impl Model {
    /// Apply `batch` to tree and model, then check. A new key must read as absent first.
    pub(super) fn apply(&mut self, tree: &mut RocksFrontierMPT, batch: &[Entry]) {
        for (key, _) in batch {
            if !self.contents.contains_key(key) {
                assert_eq!(tree.get_leaf_value(*key), None, "unwritten key present");
            }
        }
        tree.batch_upsert(batch);
        self.oracle.batch_upsert(batch);
        self.contents.extend(batch.iter().copied());
        self.check(tree);
    }

    pub(super) fn check(&self, tree: &RocksFrontierMPT) {
        assert_eq!(tree.get_root_hash(), self.oracle.get_root_hash(), "root");
        assert_eq!(tree.leaf_count(), self.contents.len(), "leaf count");
        for (key, value) in &self.contents {
            assert_eq!(
                tree.get_leaf_value(*key),
                Some(*value),
                "value under {key:?}"
            );
        }
        tree.check_invariants();
    }

    /// `n` entries: up to half updates of existing keys, the rest new.
    pub(super) fn mixed_batch(&self, n: usize) -> Vec<Entry> {
        let mut batch: Vec<Entry> = self
            .contents
            .keys()
            .take(n / 2)
            .map(|key| (*key, random_value()))
            .collect();
        batch.extend(random_entries(n - batch.len()));
        batch
    }
}

impl RocksFrontierMPT {
    /// The levels cover the configured range (a batch may size them one deeper); every
    /// position at or above the frontier's children is hashed; every hash is what its
    /// children imply; the per-depth hashed counts agree with a census of the slots and
    /// `depth_is_complete` with the counts.
    pub(super) fn check_invariants(&self) {
        let frontier = self.frontier_depth();
        let deepest = self.deepest_level();
        assert!(
            (deepest..=deepest + 1).contains(&self.levels.deepest()),
            "the levels reach {} against a configured {deepest}",
            self.levels.deepest()
        );
        let mut hashed = vec![0u64; usize::from(self.levels.deepest()) + 2];
        for (position, slot) in self.levels.slots() {
            let Slot::Hash(hash) = slot else { continue };
            hashed[usize::from(position.depth)] += 1;
            let left = self.levels.get(position.child(Side::Left));
            let right = self.levels.get(position.child(Side::Right));
            let implied = match (left, right) {
                (Some(Slot::Hash(l)), Some(Slot::Hash(r))) => {
                    hash::interior(position.prefix(), l, r)
                }
                (Some(Slot::Hash(h)), Some(Slot::Empty))
                | (Some(Slot::Empty), Some(Slot::Hash(h))) => h,
                (Some(Slot::Empty), Some(Slot::Empty)) => {
                    panic!("{position:?} hashed over no leaves")
                }
                // The bottom of the covered range, or a level grown since.
                (None, None) => continue,
                _ => panic!("{position:?} knows exactly one of its children"),
            };
            assert_eq!(hash, implied, "{position:?} disagrees with its children");
        }
        for depth in 0..hashed.len() as u16 {
            let count = hashed[usize::from(depth)];
            assert_eq!(
                self.levels.hashed_count(depth),
                count,
                "hashed count at {depth}"
            );
            if frontier > 0 && depth <= frontier + 1 {
                assert_eq!(
                    count,
                    1 << depth,
                    "a gap at or above the frontier's children"
                );
            }
            if depth >= 1 {
                let complete = depth <= self.config.frontier_cap()
                    && hashed.get(usize::from(depth) + 1) == Some(&(1 << (depth + 1)));
                assert_eq!(
                    self.depth_is_complete(depth),
                    complete,
                    "completeness at {depth}"
                );
            }
        }
    }

    /// All `2^F` rows exist at the frontier and none at the four depths below it.
    pub(super) fn check_persisted_rows(&self) {
        let frontier = self.frontier_depth();
        for depth in frontier.max(1)..=frontier + 4 {
            let mut rows = 0u64;
            let start = Position::new(depth, 0).prefix();
            self.storage
                .for_each_frontier_row(depth, &start, None, |_, _, _| {
                    rows += 1;
                    Ok(())
                })
                .expect("scan the level");
            let expected = if depth == frontier { 1 << depth } else { 0 };
            assert_eq!(rows, expected, "interior rows at depth {depth}");
        }
    }
}

/// Growth through the advance, a cold reopen, a cold single-key update, a cold mixed batch
/// and a second reopen, checked against the model throughout; the census counts exactly what
/// a batch stages, and the sampled metrics are re-baselined by a reset.
#[test]
fn a_tree_matches_the_oracle_through_growth_advances_reopens_and_cold_batches() {
    let path = fresh_dir().join("tree.db");
    let mut model = Model::default();
    let mut tree = open_test(&path).expect("create tree");
    assert_eq!((tree.frontier_depth(), tree.leaf_count()), (0, 0));
    model.check(&tree);
    model.apply(&mut tree, &[]);
    assert_eq!(
        tree.get_root_hash(),
        None,
        "an empty batch leaves the tree empty"
    );
    model.apply(&mut tree, &random_entries(1));
    assert_eq!(tree.frontier_depth(), 0, "one leaf completes no level");

    for _ in 0..6 {
        let batch = model.mixed_batch(300);
        model.apply(&mut tree, &batch);
    }
    let frontier = tree.frontier_depth();
    assert!(
        frontier >= 1,
        "the frontier must advance for this to mean anything"
    );
    assert!(
        tree.levels.slots().any(|(p, _)| p.depth > frontier + 1),
        "the levels below the frontier's children must be populated"
    );

    // Each new key once, plus exactly one row per touched frontier subtree.
    let fresh = random_entries(200);
    let touched: HashSet<u64> = fresh
        .iter()
        .map(|(k, _)| Position::index_of(k, frontier))
        .collect();
    let touched = touched.len() as u64;
    tree.census_reset();
    model.apply(&mut tree, &fresh);
    assert_eq!(
        tree.frontier_depth(),
        frontier,
        "the counted batch must not advance"
    );
    let census = tree.census_snapshot();
    assert_eq!(census[Metric::LeafPuts], 200);
    assert_eq!(census[Metric::InteriorPuts], touched);
    assert_eq!(
        census[Metric::BytesStaged],
        200 * (NODE_KEY_LEN + LEAF_RECORD_LEN) as u64
            + touched * (NODE_KEY_LEN + FRONTIER_RECORD_LEN) as u64
    );
    assert!(census[Metric::BatchesCommitted] >= 1);
    tree.check_persisted_rows();

    drop(tree);
    let mut tree = open_test(&path).expect("reopen tree");
    model.check(&tree);
    assert!(
        tree.sorted_runs() >= 1,
        "the reopen flushed the recovered rows"
    );
    let (&first, _) = model.contents.iter().next().expect("contents");
    model.apply(&mut tree, &[(first, random_value())]);

    tree.census_reset();
    let batch = model.mixed_batch(600);
    model.apply(&mut tree, &batch);
    let census = tree.census_snapshot();
    assert!(
        census[Metric::SubtreeLoads] >= 1,
        "a cold batch merges from disk"
    );
    assert!(census[Metric::LeavesReadByLoads] >= census[Metric::SubtreeLoads]);
    assert!(
        census[Metric::Seeks] >= census[Metric::SubtreeLoads],
        "a scan seeks at least once: {} seeks against {} loads",
        census[Metric::Seeks],
        census[Metric::SubtreeLoads]
    );
    assert!(
        tree.census_snapshot()[Metric::Seeks] >= census[Metric::Seeks],
        "a second snapshot of one phase cannot go backwards"
    );
    tree.census_reset();
    assert_eq!(
        tree.census_snapshot()[Metric::Seeks],
        0,
        "a reset re-baselines the tickers"
    );
    tree.check_persisted_rows();

    drop(tree);
    model.check(&open_test(&path).expect("reopen tree"));
}

/// Every batch shape survives a reopen: singles into empty and non-empty trees, whole small
/// trees in one batch, shapes crossing the advance. The structured keys enumerate positions
/// so tiny trees complete levels at the production configuration.
#[test]
fn every_batch_shape_survives_a_reopen() {
    let mut shapes: Vec<(Vec<Entry>, usize, RocksFrontierConfig)> = Vec::new();
    for (total, batch) in [
        (1, 1),
        (2, 1),
        (2, 2),
        (3, 1),
        (3, 3),
        (4, 4),
        (5, 2),
        (5, 5),
        (17, 4),
        (64, 64),
        (250, 25),
        (1000, 100),
        (200, 1),
    ] {
        shapes.push((
            random_entries(total),
            batch,
            RocksFrontierConfig::test_config(),
        ));
    }
    let subsets: [&[u8]; 5] = [
        &[0, 1, 2, 3, 4, 5, 6, 7],
        &[0, 1, 2, 3],
        &[0, 2, 4, 6],
        &[0, 7],
        &[0, 1, 4],
    ];
    for positions in subsets {
        let entries: Vec<Entry> = positions
            .iter()
            .map(|&p| (key(p << 5), random_value()))
            .collect();
        for batch in [1, entries.len()] {
            shapes.push((entries.clone(), batch, RocksFrontierConfig::default()));
        }
    }

    for (entries, batch, config) in shapes {
        let path = fresh_dir().join("shape.db");
        let mut model = Model::default();
        let mut tree = RocksFrontierMPT::open(&path, config).expect("create tree");
        for chunk in entries.chunks(batch) {
            model.apply(&mut tree, chunk);
        }
        drop(tree);
        let tree = RocksFrontierMPT::open(&path, config).expect("reopen tree");
        model.check(&tree);
        tree.check_persisted_rows();
    }
}

/// Reopened between every one of fifty small batches, across the advance.
#[test]
fn a_tree_reopened_between_every_batch_matches_the_oracle() {
    let path = fresh_dir().join("reopen.db");
    let mut model = Model::default();
    let mut rng = TestRng::seed(1337);
    for _ in 0..50 {
        let batch = model.mixed_batch(1 + rng.below(5));
        let mut tree = open_test(&path).expect("reopen tree");
        model.check(&tree);
        model.apply(&mut tree, &batch);
    }
    assert!(open_test(&path).expect("final open").frontier_depth() >= 1);
}

/// A 9-level budget lets the frontier advance several times; the hashed counts must track
/// the slots across it (via the model's check), and a known position stays known.
#[test]
fn the_levels_stay_counted_and_never_forget_across_batches_advances_and_a_reopen() {
    let config = RocksFrontierConfig::with_max_depth(9);
    let path = fresh_dir().join("counts.db");
    let mut model = Model::default();
    let mut rng = TestRng::seed(0xC0117);
    let mut tree = RocksFrontierMPT::open(&path, config).expect("create tree");
    model.check(&tree);

    let mut known: HashSet<Position> = HashSet::new();
    for round in 0..50 {
        if round == 40 {
            drop(tree);
            tree = RocksFrontierMPT::open(&path, config).expect("reopen tree");
            model.check(&tree);
            known.clear();
        }
        let batch = model.mixed_batch(1 + rng.below(48));
        model.apply(&mut tree, &batch);
        let now: HashSet<Position> = tree.levels.slots().map(|(p, _)| p).collect();
        assert!(
            now.is_superset(&known),
            "round {round} forgot a known position"
        );
        known = now;
    }
    assert!(
        tree.frontier_depth() >= 3,
        "the frontier must advance several times"
    );
}

/// Every refusal in DESIGN.md §6, by name, plus a whole deep level as the positive control.
#[test]
fn the_open_refuses_everything_outside_the_format_and_loads_a_whole_level() {
    let row = |depth, index| {
        (
            Position::new(depth, index).prefix(),
            Digest([1; 32]),
            Digest([2; 32]),
        )
    };
    let plant = |name: &str, f: &dyn Fn(&RocksStorage)| -> String {
        let path = fresh_dir().join(name);
        let storage = RocksStorage::open(&path).expect("open storage");
        f(&storage);
        storage.flush().expect("flush wal");
        drop(storage);
        match open_test(&path) {
            Err(err) => err.to_string(),
            Ok(_) => panic!("{name} must be refused"),
        }
    };

    let err = plant("missing_level", &|s| s.commit_metadata(0, 14).unwrap());
    assert!(err.contains("frontier level 14"), "{err}");

    let err = plant("too_deep", &|s| {
        s.commit_metadata(0, MAX_DEPTH - 1).unwrap()
    });
    assert!(
        err.contains(&format!("frontier depth {}", MAX_DEPTH - 1)),
        "{err}"
    );

    let err = plant("no_count", &|s| {
        s.write_frontier_rows(&[row(1, 0), row(1, 1)]).unwrap();
        s.plant_frontier_depth(1).unwrap();
    });
    assert!(err.contains("leaf count"), "{err}");

    let err = plant("stripped", &|s| {
        s.write_frontier_rows(&[row(1, 0)]).unwrap()
    });
    assert!(err.contains("interior rows"), "{err}");

    let err = plant("dirty_row", &|s| {
        s.write_frontier_rows(&[row(1, 0), row(1, 1)]).unwrap();
        let mut dirty = RocksWriteBatch::default();
        dirty.put_raw(
            [&1u16.to_be_bytes()[..], &key(0x20).0[..]].concat(),
            [1u8; FRONTIER_RECORD_LEN].to_vec(),
        );
        s.write_batch(dirty).unwrap();
        s.commit_metadata(0, 1).unwrap();
    });
    assert!(err.contains("past its length"), "{err}");

    // A v3 leaf record: the open counts it without decoding; the first read refuses it.
    let path = fresh_dir().join("legacy.db");
    {
        let storage = RocksStorage::open(&path).expect("open storage");
        let mut batch = RocksWriteBatch::default();
        batch.put_raw(
            [&256u16.to_be_bytes()[..], &[7u8; 32][..]].concat(),
            [&[2u8][..], &[0x5Au8; 32][..]].concat(),
        );
        storage.write_batch(batch).expect("plant record");
        storage.flush().expect("flush wal");
    }
    let tree = open_test(&path).expect("the open decodes no leaf");
    let panic = catch_unwind(AssertUnwindSafe(|| tree.get_root_hash())).expect_err("refuse");
    let message = panic.downcast_ref::<String>().cloned().expect("message");
    assert!(message.contains("got 33"), "{message}");

    let path = fresh_dir().join("deep.db");
    {
        let storage = RocksStorage::open(&path).expect("open storage");
        let level: Vec<_> = (0..1u64 << 15).map(|index| row(15, index)).collect();
        storage
            .write_frontier_rows(&level)
            .expect("write the level");
        storage.commit_metadata(0, 15).expect("commit metadata");
        storage.flush().expect("flush wal");
    }
    let tree = open_test(&path).expect("a whole level opens");
    assert_eq!((tree.frontier_depth(), tree.leaf_count()), (15, 0));
    assert!(
        tree.get_root_hash().is_some(),
        "the root is derived from the level"
    );
    tree.check_invariants();
}

/// Leaf records and nothing else open to the tree they describe: exact count, lazy root
/// rebuild that writes nothing, and the next batch builds on both.
#[test]
fn a_leaves_only_database_opens_to_the_tree_its_leaves_describe() {
    for count in [0, 1, 5, 60, 200] {
        let path = fresh_dir().join("leaves_only.db");
        let mut model = Model::default();
        let entries = random_entries(count);
        {
            let storage = RocksStorage::open(&path).expect("open storage");
            storage.write_leaves(&entries).expect("write leaves");
            storage.flush().expect("flush wal");
        }
        model.oracle.batch_upsert(&entries);
        model.contents.extend(entries.iter().copied());

        let mut tree = open_test(&path).expect("recover tree");
        let sequence = tree.storage.latest_sequence_number();
        model.check(&tree);
        assert_eq!(
            tree.storage.latest_sequence_number(),
            sequence,
            "the root rebuild wrote"
        );
        if count > 0 {
            let root = tree.get_root_hash();
            assert_eq!(
                tree.levels.get(Position::ROOT),
                root.map(Slot::Hash),
                "root recorded"
            );
        }

        model.apply(&mut tree, &random_entries(1));
        drop(tree);
        model.check(&open_test(&path).expect("reopen tree"));
    }
}

/// The frontier settles at the deepest complete level within the cap. One level per batch:
/// the tree top holds exactly one spare level below the frontier's children.
#[test]
fn the_frontier_advances_to_the_deepest_complete_level_within_the_cap() {
    // Top five bits enumerate 0..32: depths 0..=4 complete, depth 5 one leaf per position.
    let entries: Vec<Entry> = (0..32u8).map(|i| (key(i << 3), Value([i; 32]))).collect();
    for (config, expected) in [
        (RocksFrontierConfig::default(), 4),
        (RocksFrontierConfig::test_config(), 4),
        (RocksFrontierConfig::with_max_depth(5), 3),
    ] {
        let mut tree = RocksFrontierMPT::open(fresh_dir(), config).expect("create tree");
        let mut model = Model::default();
        // Re-applying the same entries moves no leaf, so the frontier settles honestly.
        for _ in 0..8 {
            model.apply(&mut tree, &entries);
        }
        assert_eq!(tree.frontier_depth(), expected, "{config:?}");
    }
}

/// `deepest_level` is the larger of the block floor and what the frontier needs, capped at
/// the budget; the constructors enforce the knob constraints.
#[test]
fn the_tree_top_follows_the_block_floor_up_to_the_budget() {
    // One leaf a block: the floor is `ceil(log2(leaves))`.
    let config = RocksFrontierConfig::test_config();
    for (frontier, leaves, expected) in [
        (0, 0, 2),
        (0, 1, 2),
        (0, 4, 2),
        (0, 5, 3),
        (0, 64, 6),
        (0, u64::MAX, 6),
        // Children always held, plus the gate level while below the cap: 4 (the cap) -> 5.
        (3, 0, 5),
        (4, 0, 5),
        (4, u64::MAX, 6),
        (10, 0, 11),
    ] {
        assert_eq!(
            config.deepest_level(frontier, leaves),
            expected,
            "({frontier}, {leaves})"
        );
    }

    for (leaves, expected) in [
        (0, 0),
        (LEAVES_PER_BLOCK, 0),
        (LEAVES_PER_BLOCK + 1, 1),
        (1_000_000_000, 24),
        (4_000_000_000, 26),
        (16_000_000_000, 28),
    ] {
        assert_eq!(
            block_floor(leaves, LEAVES_PER_BLOCK),
            expected,
            "{leaves} leaves"
        );
    }

    // At a billion leaves the production ceiling does not bind; a higher ceiling buys the
    // gate level and nothing more.
    let production = RocksFrontierConfig::with_max_depth(26);
    assert_eq!(production.deepest_level(24, 1_000_000_000), 25);
    assert_eq!(
        RocksFrontierConfig::with_max_depth(28).deepest_level(24, 1_000_000_000),
        26
    );

    for max_depth in [22, 26] {
        let config = RocksFrontierConfig::with_max_depth(max_depth);
        assert_eq!(config.frontier_cap(), max_depth - 2);
        for frontier in [0, max_depth - 3, max_depth - 2] {
            assert_eq!(
                config.deepest_level(frontier, u64::MAX),
                max_depth,
                "frontier {frontier}"
            );
        }
    }
    for max_depth in [1, MAX_DEPTH + 1] {
        assert!(catch_unwind(|| RocksFrontierConfig::with_max_depth(max_depth)).is_err());
    }
    assert!(catch_unwind(|| RocksFrontierConfig::with_depths(9, 9)).is_err());
    assert_eq!(RocksFrontierConfig::with_depths(9, 8).frontier_cap(), 8);
    assert_eq!(RocksFrontierConfig::with_depths(28, 4).frontier_cap(), 4);
    assert_eq!(
        levels::levels_bytes(0),
        33,
        "one position, a hash and a state byte"
    );
    assert_eq!(levels::levels_bytes(2), 7 * 33);
    assert_eq!(Levels::new(5).deepest(), 5);
}

#[test]
fn positions_and_positional_prefixes_name_each_other() {
    for depth in [0u16, 1, 7, 8, 9, 63] {
        for index in [0, 1, (1u64 << depth) - 1]
            .into_iter()
            .filter(|&i| i < 1 << depth)
        {
            let position = Position::new(depth, index);
            let prefix = position.prefix();
            assert_eq!((prefix.length(), Position::of(&prefix)), (depth, position));
            for (k, _) in random_entries(4) {
                assert_eq!(prefix.contains(&k), Position::index_of(&k, depth) == index);
            }
        }
    }
}

/// The positional builder hashes bit-identically to the compressed one at every handover
/// depth. The caterpillar (key `i` differs from key 0 in bit `i` alone) is 128 levels of
/// pass-through, where an unbounded positional index would overflow.
#[test]
fn the_positional_builder_matches_the_compressed_builder() {
    let mut cases: Vec<Vec<Entry>> = [1, 2, 3, 17, 65, 200].map(random_entries).into();
    let mut caterpillar = vec![(Key::ZERO, random_value())];
    for bit in 0..128 {
        let mut k = [0u8; 32];
        k[bit / 8] |= 1 << (7 - bit % 8);
        caterpillar.push((Key(k), random_value()));
    }
    cases.push(caterpillar);

    let mut tree = fresh_tree();
    for entries in &cases {
        let sorted = crate::mpt::sorted_unique_entries(entries);
        let expected = RocksFrontierMPT::subtree_hash(&sorted);
        // `build_subtree` only writes positions, so a stale one cannot change the hash.
        for deepest in [2, 3, 8, 12] {
            tree.levels.ensure_depth(deepest);
            let built = tree.build_subtree(Position::ROOT, &sorted);
            assert_eq!(
                built,
                expected,
                "{} leaves, levels to {deepest}",
                sorted.len()
            );
        }
    }
}

/// Chunk sizes hitting several boundaries, an exact multiple and a partial tail; the
/// pass-through position at the level is skipped.
#[test]
fn a_level_is_persisted_in_chunks_without_its_pass_throughs() {
    const DEPTH: u16 = 4;
    let pass_through = Position::new(DEPTH, 5);
    let written = (1usize << DEPTH) - 1;
    for chunk in [1u64, 3, 8, 16, 64] {
        let mut tree = fresh_tree();
        tree.levels.ensure_depth(DEPTH + 1);
        for index in 0..1u64 << DEPTH {
            let position = Position::new(DEPTH, index);
            tree.levels
                .set(position.child(Side::Left), Slot::Hash(Digest([1; 32])));
            let right = if position == pass_through {
                Slot::Empty
            } else {
                Slot::Hash(Digest([2; 32]))
            };
            tree.levels.set(position.child(Side::Right), right);
        }

        let batches = tree.persist_level(DEPTH, chunk).expect("persist level");
        assert_eq!(
            batches,
            written.div_ceil(chunk as usize),
            "chunk {chunk}: batches"
        );
        assert_eq!(
            tree.storage.read_metadata().expect("metadata"),
            Some((0, DEPTH)),
            "chunk {chunk}: the last chunk names the level"
        );

        let mut stored = Vec::new();
        let start = Position::new(DEPTH, 0).prefix();
        tree.storage
            .for_each_frontier_row(DEPTH, &start, None, |prefix, _, _| {
                stored.push(prefix);
                Ok(())
            })
            .expect("read the level back");
        assert_eq!(
            stored.len(),
            written,
            "chunk {chunk}: every full position once"
        );
        assert!(
            !stored.contains(&pass_through.prefix()),
            "chunk {chunk}: pass-through"
        );
    }
}

/// The merge stages only the batch's own leaves. A rewrite would be byte-identical, so the
/// pin is the sequence number: one leaf plus two metadata puts.
#[test]
fn a_merge_does_not_rewrite_the_leaves_it_read() {
    let mut tree = fresh_tree();
    let mut model = Model::default();
    let mut low = [0u8; 32];
    low[31] = 1;
    let mut arriving = [0u8; 32];
    arriving[31] = 3; // parts from `low` deep below the root, so the merge reads both leaves
    model.apply(
        &mut tree,
        &[(Key(low), random_value()), (key(0x80), random_value())],
    );
    assert_eq!(tree.frontier_depth(), 0, "two leaves complete no level");

    let sequence = tree.storage.latest_sequence_number();
    model.apply(&mut tree, &[(Key(arriving), random_value())]);
    assert_eq!(
        tree.storage.latest_sequence_number() - sequence,
        3,
        "writes beyond the leaf"
    );
}

/// Half the staged batches dropped on the floor (DESIGN.md §4): the reopened root must be
/// the oracle's root over the leaves that reached disk, since a subtree's row and its
/// leaves share a batch.
#[test]
fn a_torn_commit_leaves_the_rows_agreeing_with_the_leaves() {
    let path = fresh_dir().join("torn.db");
    let mut model = Model::default();
    let mut tree = open_test(&path).expect("create tree");
    for _ in 0..3 {
        let batch = model.mixed_batch(300);
        model.apply(&mut tree, &batch);
    }
    assert!(
        tree.frontier_depth() >= 1,
        "rows exist only with a frontier"
    );

    // The descent and the staging by hand, as `batch_upsert` runs them.
    let entries = crate::mpt::sorted_unique_entries(&model.mixed_batch(300));
    #[cfg(debug_assertions)]
    tree.visited.lock().unwrap().clear();
    tree.upsert(Position::ROOT, &entries);
    let batches = tree.stage_batches(&entries);
    assert!(batches.len() > 1, "one batch cannot tear");
    for (i, batch) in batches.into_iter().enumerate() {
        if i % 2 == 0 {
            tree.storage.write_batch(batch).expect("commit half");
        }
    }
    tree.storage.flush().expect("flush wal");
    drop(tree);

    let tree = open_test(&path).expect("reopen the torn database");
    let on_disk = tree
        .storage
        .get_leaf_entries_by_prefix(&Prefix::root())
        .expect("scan leaves");
    assert!(
        on_disk.len() > model.contents.len(),
        "the committed half added leaves"
    );
    let mut oracle = SimpleMPT::new();
    oracle.batch_upsert(&on_disk);
    assert_eq!(
        tree.get_root_hash(),
        oracle.get_root_hash(),
        "a frontier row names leaves that were never committed"
    );
    assert_eq!(
        tree.leaf_count(),
        model.contents.len(),
        "the count is what the last metadata commit knew: a lower bound, by contract"
    );
    tree.check_invariants();
}

/// A leaf count reset to zero on disk must not stall the frontier: the count only sizes the
/// cache, and the tree top always holds the levels the gate reads.
#[test]
fn a_wrong_leaf_count_does_not_stall_the_frontier() {
    let config = RocksFrontierConfig::with_max_depth(9);
    let path = fresh_dir().join("count.db");
    let mut model = Model::default();
    let mut tree = RocksFrontierMPT::open(&path, config).expect("create tree");
    for _ in 0..2 {
        model.apply(&mut tree, &random_entries(300));
    }
    let frontier = tree.frontier_depth();
    assert!(
        (1..config.frontier_cap()).contains(&frontier),
        "the frontier must have room to advance, got {frontier}"
    );
    tree.storage
        .commit_metadata(0, frontier)
        .expect("reset the count");
    tree.storage.flush().expect("flush wal");
    drop(tree);

    let mut tree = RocksFrontierMPT::open(&path, config).expect("reopen tree");
    assert_eq!((tree.leaf_count(), tree.frontier_depth()), (0, frontier));
    let mut added = 0;
    for batch in 0..30 {
        if tree.frontier_depth() == config.frontier_cap() {
            break;
        }
        let entries = random_entries(300);
        added += entries.len();
        tree.batch_upsert(&entries);
        model.oracle.batch_upsert(&entries);
        model.contents.extend(entries.iter().copied());
        assert_eq!(
            tree.leaf_count(),
            added,
            "exact for what this process added"
        );
        assert_eq!(
            tree.get_root_hash(),
            model.oracle.get_root_hash(),
            "root after batch {batch}"
        );
        tree.check_invariants();
    }
    assert_eq!(
        tree.frontier_depth(),
        config.frontier_cap(),
        "the frontier must reach the cap within 30 batches"
    );
    tree.check_persisted_rows();
}

/// The level scan's parallel ranges are sized by stride (DESIGN.md §9); frontier 6 lies in
/// the window each of these pool sizes once double-counted.
#[test]
fn a_frontier_level_loads_under_any_thread_count() {
    let config = RocksFrontierConfig::with_max_depth(9);
    let path = fresh_dir().join("pools.db");
    // Top seven bits enumerate 0..128: depth 6 complete, depth 7 leaves.
    let entries: Vec<Entry> = (0..128u8).map(|i| (key(i << 1), Value([i; 32]))).collect();
    let mut model = Model::default();
    let mut tree = RocksFrontierMPT::open(&path, config).expect("create tree");
    for _ in 0..8 {
        model.apply(&mut tree, &entries);
    }
    assert_eq!(tree.frontier_depth(), 6);
    drop(tree);

    for threads in [3, 5, 6, 7, 12] {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("build pool");
        let tree = pool
            .install(|| RocksFrontierMPT::open(&path, config))
            .unwrap_or_else(|err| panic!("{threads} threads: {err}"));
        model.check(&tree);
    }
}

/// A crash inside an advance (DESIGN.md §4): the durable prefix names a level it holds
/// whole, orphan rows of the next level are ignored, and the next batch finishes the advance.
#[test]
fn a_crash_inside_an_advance_leaves_an_openable_database() {
    let config = RocksFrontierConfig::with_max_depth(9);
    let path = fresh_dir().join("advance.db");
    let mut model = Model::default();
    let mut tree = RocksFrontierMPT::open(&path, config).expect("create tree");
    model.apply(&mut tree, &random_entries(3));
    assert_eq!(tree.frontier_depth(), 0, "three leaves complete no level");

    let entries = crate::mpt::sorted_unique_entries(&random_entries(300));
    // Driving the descent by hand, so size the tree top as `batch_upsert` would.
    let leaves_after = tree.leaf_count() as u64 + entries.len() as u64;
    let deepest = tree.config.deepest_level(tree.frontier, leaves_after);
    tree.levels.ensure_depth(deepest);
    #[cfg(debug_assertions)]
    tree.visited.lock().unwrap().clear();
    tree.upsert(Position::ROOT, &entries);
    for batch in tree.stage_batches(&entries) {
        tree.storage.write_batch(batch).expect("commit leaves");
    }
    assert!(
        tree.depth_is_complete(2),
        "the victim must complete more than one level"
    );
    tree.persist_level(1, PERSIST_CHUNK_NODES)
        .expect("persist level 1");
    let orphan = Position::new(2, 0);
    let mut row = RocksWriteBatch::default();
    row.put_frontier_node(
        &orphan.prefix(),
        tree.levels.hash(orphan.child(Side::Left)),
        tree.levels.hash(orphan.child(Side::Right)),
    );
    tree.storage
        .write_batch(row)
        .expect("commit one row of level 2");
    tree.storage.flush().expect("flush wal");
    drop(tree);

    model.oracle.batch_upsert(&entries);
    model.contents.extend(entries.iter().copied());
    let mut tree = RocksFrontierMPT::open(&path, config).expect("reopen after the torn advance");
    assert_eq!(tree.frontier_depth(), 1);
    model.check(&tree);
    model.apply(&mut tree, &random_entries(300));
    assert!(tree.frontier_depth() >= 2);
    tree.check_persisted_rows();
    drop(tree);
    model.check(&RocksFrontierMPT::open(&path, config).expect("reopen tree"));
}

/// Several tasks setting one slot count it once.
#[test]
fn a_slot_set_by_several_threads_is_counted_once() {
    const DEPTH: u16 = 16;
    let levels = Levels::new(DEPTH);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                for index in 0..1u64 << DEPTH {
                    levels.set(Position::new(DEPTH, index), Slot::Hash(Digest([1; 32])));
                }
            });
        }
    });
    assert_eq!(levels.hashed_count(DEPTH), 1 << DEPTH);
}

/// A batch that panics part-way poisons the tree (DESIGN.md §4); the reopened database is
/// untouched by the failed batch.
#[test]
fn a_tree_whose_batch_panicked_refuses_further_use() {
    let path = fresh_dir().join("poison.db");
    let mut model = Model::default();
    let mut tree = open_test(&path).expect("create tree");
    model.apply(&mut tree, &random_entries(5));
    // A corrupt record beside an existing leaf, so the merge that rewrites it reads it.
    let (&existing, _) = model.contents.iter().next().expect("contents");
    let mut beside = existing.0;
    beside[31] ^= 1;
    let corrupt_key = [&256u16.to_be_bytes()[..], &beside[..]].concat();
    let mut plant = RocksWriteBatch::default();
    plant.put_raw(corrupt_key.clone(), [&[2u8][..], &[0x5A; 32][..]].concat());
    tree.storage
        .write_batch(plant)
        .expect("plant a corrupt record");

    let message = |panic: Box<dyn std::any::Any + Send>| -> String {
        panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
            .expect("message")
    };
    let overwrite = [(existing, random_value())];
    let panic = catch_unwind(AssertUnwindSafe(|| tree.batch_upsert(&overwrite)))
        .expect_err("the merge refuses the record");
    assert!(message(panic).contains("got 33"));
    let panic = catch_unwind(AssertUnwindSafe(|| tree.batch_upsert(&random_entries(1))))
        .expect_err("a poisoned tree refuses a batch");
    assert!(message(panic).contains("panicked part-way"));
    assert!(catch_unwind(AssertUnwindSafe(|| tree.leaf_count())).is_err());

    tree.storage
        .delete_raw(&corrupt_key)
        .expect("clear the record");
    drop(tree);
    model.check(&open_test(&path).expect("reopen tree"));
}

//! Parity: both implementations through one test trait against a map model. The root is a
//! function of the contents alone, so every step's root must equal a fresh oracle's over
//! those contents, whatever order, batching or repetition produced them.

use super::*;
use crate::testing::fresh_dir;
use crate::{Digest, Key, Prefix, Value, hash};
use simple::Node;
use std::collections::{BTreeMap, BTreeSet};

trait TestTree {
    fn build() -> Self;
    fn batch_upsert(&mut self, entries: &[Entry]);
    fn get_root_hash(&self) -> Option<Digest>;
    fn get_leaf_value(&self, key: Key) -> Option<Value>;
    fn leaf_count(&self) -> usize;
}

impl TestTree for SimpleMPT {
    fn build() -> Self {
        Self::new()
    }

    fn batch_upsert(&mut self, entries: &[Entry]) {
        SimpleMPT::batch_upsert(self, entries);
    }

    fn get_root_hash(&self) -> Option<Digest> {
        SimpleMPT::get_root_hash(self)
    }

    fn get_leaf_value(&self, key: Key) -> Option<Value> {
        SimpleMPT::get_leaf_value(self, key)
    }

    fn leaf_count(&self) -> usize {
        self.nodes()
            .iter()
            .filter(|(_, node)| matches!(node, Node::Leaf(_)))
            .count()
    }
}

impl TestTree for RocksFrontierMPT {
    fn build() -> Self {
        RocksFrontierMPT::open(fresh_dir(), RocksFrontierConfig::test_config()).expect("open")
    }

    fn batch_upsert(&mut self, entries: &[Entry]) {
        RocksFrontierMPT::batch_upsert(self, entries);
    }

    fn get_root_hash(&self) -> Option<Digest> {
        RocksFrontierMPT::get_root_hash(self)
    }

    fn get_leaf_value(&self, key: Key) -> Option<Value> {
        RocksFrontierMPT::get_leaf_value(self, key)
    }

    fn leaf_count(&self) -> usize {
        RocksFrontierMPT::leaf_count(self)
    }
}

fn key(byte: u8) -> Key {
    let mut key = [0u8; 32];
    key[0] = byte;
    Key(key)
}

fn value(byte: u8) -> Value {
    let mut value = [0u8; 32];
    value[0] = byte;
    Value(value)
}

fn entries(bytes: impl IntoIterator<Item = u8>) -> Vec<Entry> {
    bytes
        .into_iter()
        .map(|b| (key(b), value(b + 100)))
        .collect()
}

fn one_at_a_time(entries: &[Entry]) -> Vec<Vec<Entry>> {
    entries.iter().map(|entry| vec![*entry]).collect()
}

type Contents = BTreeMap<Key, Value>;

fn canonical_root(contents: &Contents) -> Option<Digest> {
    let mut oracle = SimpleMPT::new();
    oracle.batch_upsert(&contents.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>());
    oracle.get_root_hash()
}

/// Apply `batches` to a fresh `T`, checking against the model after each; return the
/// `(contents, root)` pairs seen.
fn run<T: TestTree>(batches: &[Vec<Entry>]) -> Vec<(Contents, Option<Digest>)> {
    let keys: BTreeSet<Key> = batches.iter().flatten().map(|(k, _)| *k).collect();
    let mut tree = T::build();
    let mut model = Contents::new();
    let mut seen = Vec::new();
    for batch in batches {
        tree.batch_upsert(batch);
        model.extend(batch.iter().copied());
        let root = tree.get_root_hash();
        assert_eq!(root, canonical_root(&model), "root after {batch:?}");
        assert_eq!(tree.leaf_count(), model.len(), "leaf count after {batch:?}");
        for k in &keys {
            assert_eq!(
                tree.get_leaf_value(*k),
                model.get(k).copied(),
                "value after {batch:?}"
            );
        }
        seen.push((model.clone(), root));
    }
    seen
}

#[test]
fn every_scenario_reports_the_root_count_and_values_of_its_contents() {
    let ten = entries(0..10);
    let shuffled = entries([7, 2, 9, 0, 4, 1, 8, 3, 6, 5]);
    let repeats = vec![
        (key(1), value(101)),
        (key(2), value(102)),
        (key(1), value(201)),
        (key(3), value(103)),
        (key(1), value(111)),
        (key(2), value(222)),
    ];
    let scenarios: Vec<Vec<Vec<Entry>>> = vec![
        vec![vec![]],
        vec![vec![], entries([1]), vec![]],
        vec![
            entries([1]),
            vec![(key(1), value(2))],
            vec![(key(1), value(3))],
        ],
        vec![ten.clone()],
        one_at_a_time(&ten),
        vec![shuffled.clone()],
        one_at_a_time(&shuffled),
        vec![entries((0..10).rev())],
        vec![repeats.clone()],
        one_at_a_time(&repeats),
        vec![
            entries([1, 2]),
            entries([3, 4]),
            vec![(key(1), value(201)), (key(5), value(105))],
        ],
        vec![entries(0..50)],
    ];

    let mut roots: BTreeMap<Contents, Option<Digest>> = BTreeMap::new();
    for batches in &scenarios {
        let seen = run::<SimpleMPT>(batches);
        assert_eq!(seen, run::<RocksFrontierMPT>(batches));
        for (contents, root) in seen {
            assert_eq!(
                root.is_none(),
                contents.is_empty(),
                "an empty tree has no root"
            );
            roots.insert(contents, root);
        }
        for batch in batches {
            let deduped: Contents = batch.iter().copied().collect();
            assert_eq!(
                sorted_unique_entries(batch),
                deduped.into_iter().collect::<Vec<_>>()
            );
        }
    }
    let distinct: BTreeSet<_> = roots.values().collect();
    assert_eq!(
        distinct.len(),
        roots.len(),
        "two different contents share a root"
    );
}

/// The oracle's hash computed by hand for one shape.
#[test]
fn the_oracle_hashes_the_compressed_trie() {
    let (a, b, c) = (
        (key(0x00), value(1)),
        (key(0x40), value(2)),
        (key(0x80), value(3)),
    );
    let mut oracle = SimpleMPT::new();
    oracle.batch_upsert(&[c, a, b]);
    // `a` and `b` share bit 0 and part at bit 1; `c` parts from both at bit 0.
    let left = hash::interior(
        Prefix::new(Key::ZERO, 1),
        hash::leaf(a.0, a.1),
        hash::leaf(b.0, b.1),
    );
    let root = hash::interior(Prefix::root(), left, hash::leaf(c.0, c.1));
    assert_eq!(oracle.get_root_hash(), Some(root));
    assert_eq!(oracle.nodes().len(), 5, "three leaves and two interiors");
}

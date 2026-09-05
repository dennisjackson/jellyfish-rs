//! Parity: both implementations through one test trait against a history model. The root is
//! a function of each key's sequence of values alone (HASHCHAINS.md), so every step's root
//! must equal a fresh oracle's over those sequences, whatever order or batching produced them,
//! and every stored chain must verify.

use super::*;
use crate::testing::fresh_dir;
use crate::{Digest, Key, Prefix, Record, Value, hash};
use simple::Node;
use std::collections::{BTreeMap, BTreeSet};

trait TestTree {
    fn build() -> Self;
    fn batch_upsert(&mut self, entries: &[Entry]);
    fn get_root_hash(&self) -> Option<Digest>;
    fn get_leaf_value(&self, key: Key) -> Option<Value>;
    fn get_record(&self, key: Key) -> Option<(u64, Record)>;
    fn get_history(&self, key: Key) -> Vec<Record>;
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

    fn get_record(&self, key: Key) -> Option<(u64, Record)> {
        SimpleMPT::get_record(self, key)
    }

    fn get_history(&self, key: Key) -> Vec<Record> {
        SimpleMPT::get_history(self, key)
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

    fn get_record(&self, key: Key) -> Option<(u64, Record)> {
        RocksFrontierMPT::get_record(self, key)
    }

    fn get_history(&self, key: Key) -> Vec<Record> {
        RocksFrontierMPT::get_history(self, key)
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

/// Every value written under each key, in order.
type Histories = BTreeMap<Key, Vec<Value>>;

fn extend(histories: &mut Histories, batch: &[Entry]) {
    for (key, value) in batch {
        histories.entry(*key).or_default().push(*value);
    }
}

/// The chain `values` produce under `key`, computed by hand.
fn chain(key: Key, values: &[Value]) -> Vec<Record> {
    let mut records: Vec<Record> = Vec::new();
    for &value in values {
        let record = match records.last() {
            None => Record::first(value),
            Some(previous) => previous.next(key, value),
        };
        records.push(record);
    }
    records
}

/// A fresh oracle fed each key's sequence, key by key: the root depends on the sequences
/// alone, not on how they were interleaved.
fn canonical_root(histories: &Histories) -> Option<Digest> {
    let mut oracle = SimpleMPT::new();
    for (key, values) in histories {
        oracle.batch_upsert(&values.iter().map(|v| (*key, *v)).collect::<Vec<_>>());
    }
    oracle.get_root_hash()
}

/// Apply `batches` to a fresh `T`, checking against the model after each; return the
/// `(histories, root)` pairs seen.
fn run<T: TestTree>(batches: &[Vec<Entry>]) -> Vec<(Histories, Option<Digest>)> {
    let keys: BTreeSet<Key> = batches.iter().flatten().map(|(k, _)| *k).collect();
    let mut tree = T::build();
    let mut model = Histories::new();
    let mut seen = Vec::new();
    for batch in batches {
        tree.batch_upsert(batch);
        extend(&mut model, batch);
        let root = tree.get_root_hash();
        assert_eq!(root, canonical_root(&model), "root after {batch:?}");
        assert_eq!(tree.leaf_count(), model.len(), "leaf count after {batch:?}");
        for k in &keys {
            let expected = model.get(k).map(|values| chain(*k, values));
            assert_eq!(
                tree.get_leaf_value(*k),
                model.get(k).map(|values| values[values.len() - 1]),
                "value after {batch:?}"
            );
            assert_eq!(
                tree.get_record(*k),
                expected
                    .as_ref()
                    .map(|records| (records.len() as u64 - 1, records[records.len() - 1])),
                "record after {batch:?}"
            );
            let history = tree.get_history(*k);
            assert_eq!(
                history,
                expected.unwrap_or_default(),
                "history after {batch:?}"
            );
            assert_eq!(
                Record::verify_chain(*k, &history),
                !history.is_empty(),
                "chain after {batch:?}"
            );
        }
        seen.push((model.clone(), root));
    }
    seen
}

#[test]
fn every_scenario_reports_the_root_count_values_and_histories_of_its_contents() {
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
        // The same value twice in one batch and across two: distinct links each time.
        vec![
            vec![(key(4), value(4)), (key(4), value(4))],
            vec![(key(4), value(4))],
        ],
        vec![
            entries([1, 2]),
            entries([3, 4]),
            vec![(key(1), value(201)), (key(5), value(105))],
        ],
        vec![entries(0..50)],
    ];

    let mut roots: BTreeMap<Histories, Option<Digest>> = BTreeMap::new();
    for batches in &scenarios {
        let seen = run::<SimpleMPT>(batches);
        assert_eq!(seen, run::<RocksFrontierMPT>(batches));
        for (histories, root) in seen {
            assert_eq!(
                root.is_none(),
                histories.is_empty(),
                "an empty tree has no root"
            );
            roots.insert(histories, root);
        }
        for batch in batches {
            let sorted = sorted_entries(batch);
            assert!(
                sorted.windows(2).all(|pair| pair[0].0 <= pair[1].0),
                "sorted"
            );
            let mut per_key = Histories::new();
            extend(&mut per_key, &sorted);
            let mut expected = Histories::new();
            extend(&mut expected, batch);
            assert_eq!(
                per_key, expected,
                "stable: each key's values keep their order"
            );
        }
    }
    let distinct: BTreeSet<_> = roots.values().collect();
    assert_eq!(
        distinct.len(),
        roots.len(),
        "two different histories share a root"
    );
}

/// The oracle's hash computed by hand for one shape, with one key overwritten so the chain
/// enters the root.
#[test]
fn the_oracle_hashes_the_compressed_trie_over_chained_records() {
    let (a, b, c) = (
        (key(0x00), value(1)),
        (key(0x40), value(2)),
        (key(0x80), value(3)),
    );
    let mut oracle = SimpleMPT::new();
    oracle.batch_upsert(&[c, a, b, (b.0, value(9))]);
    // `a` and `b` share bit 0 and part at bit 1; `c` parts from both at bit 0.
    let first = Record::first(b.1);
    let second = Record {
        value: value(9),
        link: hash::leaf(b.0, &first),
    };
    assert_eq!(second, first.next(b.0, value(9)));
    let left = hash::interior(
        Prefix::new(Key::ZERO, 1),
        hash::leaf(a.0, &Record::first(a.1)),
        hash::leaf(b.0, &second),
    );
    let root = hash::interior(Prefix::root(), left, hash::leaf(c.0, &Record::first(c.1)));
    assert_eq!(oracle.get_root_hash(), Some(root));
    assert_eq!(oracle.nodes().len(), 5, "three leaves and two interiors");
    assert_eq!(oracle.get_history(b.0), [first, second]);
    assert_eq!(oracle.get_record(b.0), Some((1, second)));
}

/// `verify_chain` accepts exactly the chains `next` builds.
#[test]
fn a_chain_verifies_only_when_every_link_names_its_predecessor() {
    let k = key(7);
    let good = chain(k, &[value(1), value(2), value(1)]);
    assert!(Record::verify_chain(k, &good));
    assert!(Record::verify_chain(k, &good[..1]));
    assert!(!Record::verify_chain(k, &[]), "an empty chain is no chain");
    assert!(
        !Record::verify_chain(key(8), &good),
        "the links bind the key"
    );
    assert!(
        !Record::verify_chain(k, &good[1..]),
        "a chain starts at genesis"
    );
    let mut reordered = good.clone();
    reordered.swap(1, 2);
    assert!(!Record::verify_chain(k, &reordered));
    let mut tampered = good.clone();
    tampered[1].value = value(3);
    assert!(
        !Record::verify_chain(k, &tampered),
        "a changed value breaks the next link"
    );
    let mut skipped = good.clone();
    skipped.remove(1);
    assert!(!Record::verify_chain(k, &skipped));
    assert_eq!(good[0].link, Record::GENESIS_LINK);
    assert_eq!(good[1].link, good[0].leaf_hash(k));
}

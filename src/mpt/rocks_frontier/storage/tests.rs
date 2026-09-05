use super::*;
use crate::Entry;
use crate::testing::{TestRng, fresh_dir};

// Test-only storage surface: observe or plant state no production caller needs.
impl RocksStorage {
    /// The raw leaf record under `key`, undecoded.
    pub(crate) fn get_raw_leaf(&self, key: &Key) -> RocksResult<Option<Vec<u8>>> {
        Ok(self
            .db
            .get_pinned(leaf_key(key))?
            .map(|raw| raw.as_ref().to_vec()))
    }

    /// The raw history record at `seq`, undecoded.
    pub(crate) fn get_raw_history(&self, seq: u64) -> RocksResult<Option<Vec<u8>>> {
        Ok(self
            .db
            .get_pinned_cf(self.history(), history_key(seq))?
            .map(|raw| raw.as_ref().to_vec()))
    }

    /// The raw frontier record under `prefix`, undecoded.
    pub(crate) fn get_raw_frontier(&self, prefix: &Prefix) -> RocksResult<Option<Vec<u8>>> {
        Ok(self
            .db
            .get_pinned_cf(self.frontier(), prefix_key(prefix))?
            .map(|raw| raw.as_ref().to_vec()))
    }

    /// Every write bumps it, so a delta of zero proves a path wrote nothing.
    pub(crate) fn latest_sequence_number(&self) -> u64 {
        self.db.latest_sequence_number()
    }

    /// `COMPLETE_DEPTH_KEY` alone: torn metadata no production path can write.
    pub(crate) fn plant_frontier_depth(&self, depth: u16) -> RocksResult<()> {
        self.db
            .put_cf(self.frontier(), COMPLETE_DEPTH_KEY, depth.to_be_bytes())?;
        Ok(())
    }

    /// Count and depth among the leaves, without a format version: what a v4 build
    /// committed.
    pub(crate) fn plant_v4_metadata(&self, leaf_count: u64, depth: u16) -> RocksResult<()> {
        let mut batch = WriteBatch::default();
        batch.put(LEAF_COUNT_KEY, leaf_count.to_be_bytes());
        batch.put(COMPLETE_DEPTH_KEY, depth.to_be_bytes());
        self.db.write(batch)?;
        Ok(())
    }

    /// A format version among the leaves: where v5 and v6 kept it.
    pub(crate) fn plant_legacy_format_version(&self, version: u16) -> RocksResult<()> {
        self.db.put(FORMAT_KEY, version.to_be_bytes())?;
        Ok(())
    }

    /// The format version where this build keeps it.
    pub(crate) fn plant_format_version(&self, version: u16) -> RocksResult<()> {
        self.db
            .put_cf(self.frontier(), FORMAT_KEY, version.to_be_bytes())?;
        Ok(())
    }

    /// Raw bytes under a raw key in the leaf column family, bypassing the codec.
    pub(crate) fn plant_raw(&self, key: &[u8], value: &[u8]) -> RocksResult<()> {
        self.db.put(key, value)?;
        Ok(())
    }

    /// Raw bytes under the leaf key of `key`, bypassing the record codec.
    pub(crate) fn plant_raw_leaf(&self, key: &Key, value: &[u8]) -> RocksResult<()> {
        self.db.put(leaf_key(key), value)?;
        Ok(())
    }

    /// Raw bytes under a raw key in the frontier column family, bypassing the codec.
    pub(crate) fn plant_raw_frontier(&self, key: &[u8], value: &[u8]) -> RocksResult<()> {
        self.db.put_cf(self.frontier(), key, value)?;
        Ok(())
    }

    pub(crate) fn delete_raw_leaf(&self, key: &Key) -> RocksResult<()> {
        self.db.delete(leaf_key(key))?;
        Ok(())
    }

    pub(crate) fn delete_history_row(&self, seq: u64) -> RocksResult<()> {
        self.db.delete_cf(self.history(), history_key(seq))?;
        Ok(())
    }

    /// First versions directly, bypassing the trie: a leaf row and its history row each, at
    /// the next sequence numbers.
    pub(crate) fn write_leaves(&self, entries: &[Entry]) -> RocksResult<()> {
        let mut batch = RocksWriteBatch::default();
        let first_seq = self.last_history_seq()? + 1;
        for (&(key, value), seq) in entries.iter().zip(first_seq..) {
            let version = Version::first(key, value, seq);
            batch.put_history(&version);
            batch.put_leaf(&version.leaf_row());
        }
        self.write_batch(batch)
    }

    pub(crate) fn write_frontier_rows(&self, rows: &[(Prefix, Digest, Digest)]) -> RocksResult<()> {
        let mut batch = RocksWriteBatch::default();
        for (prefix, left, right) in rows {
            batch.put_frontier_node(prefix, *left, *right);
        }
        self.write_batch(batch)
    }
}

fn open() -> RocksStorage {
    RocksStorage::open(fresh_dir()).expect("open rocksdb")
}

/// A key whose leading bytes are `bytes`, zero past them.
fn key(bytes: &[u8]) -> Key {
    let mut out = [0u8; 32];
    out[..bytes.len()].copy_from_slice(bytes);
    Key(out)
}

/// Leaves at the given first bytes, each valued by its first byte.
fn leaves(first_bytes: impl IntoIterator<Item = u8>) -> Vec<Entry> {
    first_bytes
        .into_iter()
        .map(|b| (key(&[b]), Value([b; 32])))
        .collect()
}

fn positional(bytes: &[u8], length: u16) -> Prefix {
    Prefix::new(key(bytes).zero_bits_from(length), length)
}

fn frontier_row(bytes: &[u8], length: u16) -> (Prefix, Digest, Digest) {
    (positional(bytes, length), Digest([1; 32]), Digest([2; 32]))
}

/// Format v3: a tag byte and the value, 33 bytes.
fn v3_leaf_record() -> Vec<u8> {
    [&[2u8][..], &[0x5A; 32][..]].concat()
}

/// Format v3: a tag byte and two hashes, 65 bytes.
fn v3_frontier_record() -> Vec<u8> {
    [&[3u8][..], &[0x77; 64][..]].concat()
}

/// Format v4: the bare value.
fn v4_leaf_record() -> Vec<u8> {
    vec![0x5A; 32]
}

/// Format v5: `value || link || version`.
fn v5_leaf_record() -> Vec<u8> {
    vec![0x5A; 72]
}

/// Format v6: `leaf_hash || head_seq || version_u64`.
fn v6_leaf_record() -> Vec<u8> {
    vec![0x5A; 48]
}

fn frontier_rows_at(storage: &RocksStorage, depth: u16) -> RocksResult<Vec<Prefix>> {
    let mut rows = Vec::new();
    storage.for_each_frontier_row(depth, &positional(&[], depth), None, |prefix, _, _| {
        rows.push(prefix);
        Ok(())
    })?;
    Ok(rows)
}

/// Write `values` under `key` as one chain, from the next sequence number; returns its
/// versions.
fn write_chain(storage: &RocksStorage, key: Key, values: &[Value]) -> Vec<Version> {
    let mut batch = RocksWriteBatch::default();
    let first_seq = storage.last_history_seq().expect("seq") + 1;
    let mut versions = Vec::new();
    let mut head: Option<LeafRow> = None;
    for (&value, seq) in values.iter().zip(first_seq..) {
        let version = match head {
            None => Version::first(key, value, seq),
            Some(leaf) => leaf.next(value, seq),
        };
        batch.put_history(&version);
        head = Some(version.leaf_row());
        versions.push(version);
    }
    batch.put_leaf(&head.expect("values"));
    storage.write_batch(batch).expect("write");
    versions
}

/// The leaf key spreads the fourth byte so the first 26 bits are a whole number of bytes:
/// it round-trips, keeps the keys' order, and its decoder refuses every other shape.
#[test]
fn leaf_keys_split_the_fourth_byte_and_keep_their_order() {
    let mut rng = TestRng::seed(7);
    let mut keys: Vec<Key> = (0..2000).map(|_| Key(rng.bytes())).collect();
    keys.extend([
        Key::ZERO,
        Key([0xFF; 32]),
        key(&[0, 0, 0, 0x3F]),
        key(&[0, 0, 0, 0x40]),
    ]);
    keys.sort();
    let encoded: Vec<[u8; LEAF_KEY_LEN]> = keys.iter().map(leaf_key).collect();
    assert!(
        encoded.windows(2).all(|pair| pair[0] < pair[1]),
        "encoding preserves order"
    );
    for (k, e) in keys.iter().zip(&encoded) {
        assert_eq!(&e[..3], &k.0[..3]);
        assert_eq!(e[3] & 0x3F, 0, "the high half holds two bits");
        assert_eq!(e[4] & 0xC0, 0, "the low half holds six");
        assert_eq!(e[3] | e[4], k.0[3]);
        assert_eq!(&e[5..], &k.0[4..]);
        assert_eq!(decode_leaf_key(e).expect("decode"), *k);
        // The bloom prefix is exactly the key's first 26 bits.
        let prefix = Prefix::new(k.zero_bits_from(LEAF_PREFIX_BITS), LEAF_PREFIX_BITS);
        assert_eq!(
            &leaf_key(&prefix.key())[..LEAF_PREFIX_LEN],
            &e[..LEAF_PREFIX_LEN]
        );
        let mut past = k.0;
        past[3] ^= 0x20; // bit 26
        assert_eq!(
            &leaf_key(&Key(past))[..LEAF_PREFIX_LEN],
            &e[..LEAF_PREFIX_LEN]
        );
        past[3] ^= 0x60; // bit 25 too
        assert_ne!(
            &leaf_key(&Key(past))[..LEAF_PREFIX_LEN],
            &e[..LEAF_PREFIX_LEN]
        );
    }
    let mut dirty = leaf_key(&key(&[1, 2, 3, 0xC3]));
    dirty[3] |= 0x01;
    let err = decode_leaf_key(&dirty)
        .expect_err("dirty high half")
        .to_string();
    assert!(err.contains("split fourth byte"), "{err}");
    let mut dirty = leaf_key(&key(&[1, 2, 3, 0xC3]));
    dirty[4] |= 0x80;
    assert!(decode_leaf_key(&dirty).is_err(), "dirty low half");
    for len in [0, 32, 34] {
        let err = decode_leaf_key(&vec![0; len])
            .expect_err("length")
            .to_string();
        assert!(err.contains(&format!("{len}-byte key")), "{err}");
    }
    let (start, end) = leaf_scan_range(&Prefix::root());
    assert_eq!((start, end), ([0; LEAF_KEY_LEN], None));
    let (start, end) = leaf_scan_range(&positional(&[0x80], 1));
    assert_eq!((start, end), (leaf_key(&key(&[0x80])), None));
    let (start, end) = leaf_scan_range(&positional(&[0x40], 2));
    assert_eq!(
        (start, end),
        (leaf_key(&key(&[0x40])), Some(leaf_key(&key(&[0x80]))))
    );
}

/// No metadata until a batch commits (distinct from a zero count); the triple round-trips;
/// a partial triple is refused; metadata among the leaves names the legacy format.
#[test]
fn metadata_is_absent_until_committed_and_round_trips() {
    let storage = open();
    assert_eq!(storage.read_metadata().expect("metadata"), None);
    for (count, depth) in [(0, 0), (2, 3), (u64::MAX, u16::MAX)] {
        storage.commit_metadata(count, depth).expect("commit");
        assert_eq!(
            storage.read_metadata().expect("metadata"),
            Some((count, depth))
        );
    }
    for other in [4, FORMAT_VERSION - 1, FORMAT_VERSION + 1] {
        storage.plant_format_version(other).expect("plant format");
        let err = storage
            .read_metadata()
            .expect_err("wrong format")
            .to_string();
        assert!(err.contains(&format!("format v{other}")), "{err}");
    }
    assert!(
        storage.get_raw_leaf(&Key::ZERO).expect("get").is_none()
            && storage
                .count_leaves_by_prefix(&Prefix::root())
                .expect("count")
                == 0,
        "no metadata key sits among the leaves"
    );

    let torn = open();
    torn.plant_frontier_depth(4).expect("plant depth");
    let err = torn.read_metadata().expect_err("torn").to_string();
    assert!(err.contains("only some are present"), "{err}");

    let v4 = open();
    v4.plant_v4_metadata(7, 4).expect("plant v4 metadata");
    let err = v4.read_metadata().expect_err("v4").to_string();
    assert!(
        err.contains("format v4") && err.contains("7f87873"),
        "{err}"
    );

    for legacy in [5, 6] {
        let old = open();
        old.plant_v4_metadata(7, 4).expect("plant metadata");
        old.plant_legacy_format_version(legacy)
            .expect("plant format");
        let err = old.read_metadata().expect_err("legacy").to_string();
        assert!(
            err.contains(&format!("format v{legacy}")) && err.contains("among the leaves"),
            "{err}"
        );
    }
}

/// A key of another shape in the leaf column family is refused, not skipped.
#[test]
fn a_foreign_key_inside_the_leaf_range_is_refused() {
    let storage = open();
    storage
        .write_leaves(&leaves([0x10, 0x20]))
        .expect("write leaves");
    storage
        .plant_raw(&[0x18u8, 0, 0], &[0; LEAF_RECORD_LEN])
        .expect("plant a 3-byte key");
    let err = storage
        .get_leaf_rows_by_prefix(&Prefix::root())
        .expect_err("scan")
        .to_string();
    assert!(err.contains("3-byte key"), "{err}");
    assert!(storage.count_leaves_by_prefix(&Prefix::root()).is_err());
    assert_eq!(
        storage.get_leaf_value(&key(&[0x20])).expect("point get"),
        Some(Value([0x20; 32]))
    );
}

/// Leaf scans see exactly the leaves under a prefix: the root and all-ones prefixes (no
/// successor), leaves at both range boundaries, a full-length prefix, and prefixes around
/// the bloom prefix length, where the seek consults the filters instead of scanning in total
/// order.
#[test]
fn leaf_scans_see_exactly_the_leaves_under_a_prefix() {
    let storage = open();
    storage.commit_metadata(0, 7).expect("commit metadata");
    let mut entries = leaves((0..64u8).map(|i| i.wrapping_mul(4)));
    entries.extend([
        (key(&[0xDF; 32]), Value([1; 32])), // last key before "1110"
        (key(&[0xEF; 32]), Value([2; 32])), // last key under "1110"
        (Key([0xFF; 32]), Value([3; 32])),  // the largest key there is
        (key(&[0x40, 0x00, 0x00, 0x20]), Value([4; 32])), // shares 26 bits with 0x40
        (key(&[0x40, 0x00, 0x00, 0x40]), Value([5; 32])), // parts from 0x40 at bit 25
        (key(&[0x40, 0x00, 0x00, 0x80]), Value([6; 32])), // parts from 0x40 at bit 24
        (key(&[0x40, 0x00, 0x01]), Value([7; 32])), // parts from 0x40 at bit 23
    ]);
    entries.sort();
    // Half the leaves in a table file, half in the memtable: both hold filters.
    let (flushed, resident) = entries.split_at(entries.len() / 2);
    storage.write_leaves(flushed).expect("write leaves");
    storage.db.flush().expect("flush memtable");
    storage.write_leaves(resident).expect("write leaves");

    let mut prefixes = vec![Prefix::root(), Prefix::from(entries[5].0)];
    for length in [1, 2, 3, 4, 8, 23, 24, 25, 26, 27, 28, 31, 32, 40] {
        for byte in [0x00, 0x40, 0x80, 0xC0, 0xE0, 0xF0, 0xFF] {
            prefixes.push(positional(&[byte], length));
        }
    }
    for length in [24, 25, 26, 27, 28] {
        for tail in [0x20, 0x40, 0x80] {
            prefixes.push(positional(&[0x40, 0x00, 0x00, tail], length));
        }
    }
    prefixes.push(positional(&[0x40, 0x00, 0x01], 24));
    for prefix in prefixes {
        let expected: Vec<Entry> = entries
            .iter()
            .copied()
            .filter(|(k, _)| prefix.contains(k))
            .collect();
        let scanned = storage
            .get_leaf_rows_by_prefix(&prefix)
            .expect("entry scan");
        assert_eq!(
            scanned.iter().map(|leaf| leaf.key).collect::<Vec<_>>(),
            expected.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            "scan at {prefix:?}"
        );
        for (leaf, (k, v)) in scanned.iter().zip(&expected) {
            assert_eq!(leaf.version, 0);
            assert_eq!(
                leaf.hash,
                Record::first(*v).leaf_hash(*k),
                "hash at {prefix:?}"
            );
        }
        let count = storage.count_leaves_by_prefix(&prefix).expect("count scan");
        assert_eq!(count, expected.len(), "count at {prefix:?}");
    }
    for (k, v) in &entries {
        assert_eq!(storage.get_leaf_value(k).expect("point get"), Some(*v));
        assert_eq!(
            storage.get_record(k).expect("point get"),
            Some((0, Record::first(*v)))
        );
        let leaf = storage
            .get_leaf_row(k)
            .expect("point get")
            .expect("present");
        assert_eq!(
            storage.get_history_row(leaf.head).expect("row"),
            Some((Record::first(*v), NO_SEQ))
        );
    }
    assert_eq!(
        storage.get_leaf_value(&key(&[0xDE])).expect("point get"),
        None
    );
    assert_eq!(storage.get_record(&key(&[0xDE])).expect("point get"), None);
}

/// History is one chain per key walked backwards from the leaf: every version in order,
/// ranges cut both ends, neighbouring keys never leak in, sequence numbers continue across
/// a reopen, and a chain that ends early or points at a missing row is refused.
#[test]
fn history_walks_follow_prev_pointers_within_their_bounds() {
    let dir = fresh_dir();
    let storage = RocksStorage::open(&dir).expect("open rocksdb");
    assert_eq!(storage.last_history_seq().expect("seq"), NO_SEQ);
    let subjects = [Key([0x7F; 32]), Key([0xFF; 32]), Key::ZERO];
    let values: Vec<Value> = (0..5u8).map(|i| Value([i; 32])).collect();
    let mut chains = Vec::new();
    for subject in subjects {
        chains.push(write_chain(&storage, subject, &values));
    }
    let neighbour = write_chain(&storage, key(&[0x7F, 0x7F, 0x7F]), &values[..3]);
    assert_eq!(storage.last_history_seq().expect("seq"), 18);
    assert_eq!(neighbour[2].seq, 18);

    for (subject, chain) in subjects.iter().zip(&chains) {
        let expected: Vec<Record> = chain.iter().map(|v| v.record).collect();
        assert!(Record::verify_chain(*subject, &expected), "hand chain");
        let all = storage.get_history(subject, ..).expect("walk");
        assert_eq!(all, expected, "{subject:?}");
        assert_eq!(
            storage.get_record(subject).expect("get"),
            Some((4, expected[4]))
        );
        assert_eq!(
            storage.get_leaf_row(subject).expect("get"),
            Some(chain[4].leaf_row())
        );
        assert_eq!(storage.get_history(subject, 1..3).expect("walk"), all[1..3]);
        assert_eq!(storage.get_history(subject, 3..).expect("walk"), all[3..]);
        assert_eq!(storage.get_history(subject, ..=1).expect("walk"), all[..2]);
        assert_eq!(storage.get_history(subject, 4..=9).expect("walk"), all[4..]);
        assert_eq!(storage.get_history(subject, 5..).expect("walk"), []);
        assert_eq!(storage.get_history(subject, 2..2).expect("walk"), []);
        assert_eq!(storage.get_history(subject, ..0).expect("walk"), []);
    }
    assert_eq!(
        storage
            .get_history(&key(&[0x7F, 0x7F, 0x7F]), ..)
            .expect("walk")
            .len(),
        3
    );
    assert_eq!(storage.get_history(&key(&[0x01]), ..).expect("walk"), []);

    // A reopen continues the sequence after the last row on disk.
    storage.flush().expect("flush");
    drop(storage);
    let storage = RocksStorage::open(&dir).expect("reopen rocksdb");
    assert_eq!(storage.last_history_seq().expect("seq"), 18);
    let more = write_chain(&storage, key(&[0x01]), &values[..2]);
    assert_eq!((more[0].seq, more[1].seq), (19, 20));
    assert_eq!(
        storage.get_history(&key(&[0x01]), ..).expect("walk").len(),
        2
    );

    // Version 2 of the first subject removed: the walk refuses at the gap from any start
    // above it, and still serves the versions above the gap.
    let victim = subjects[0];
    storage
        .delete_history_row(chains[0][2].seq)
        .expect("delete");
    let err = storage
        .get_history(&victim, ..)
        .expect_err("gap")
        .to_string();
    assert!(
        err.contains("version 2") && err.contains("missing"),
        "{err}"
    );
    assert!(storage.get_history(&victim, 1..).is_err());
    assert_eq!(
        storage
            .get_history(&victim, 3..)
            .expect("above the gap")
            .len(),
        2
    );
    assert_eq!(
        storage
            .get_record(&victim)
            .expect("head intact")
            .map(|(v, _)| v),
        Some(4)
    );

    // A head row whose record does not hash to the leaf is refused.
    let tampered = write_chain(&storage, key(&[0x02]), &values[..1]);
    storage
        .db
        .put_cf(
            storage.history(),
            history_key(tampered[0].seq),
            encode_history_record(&Record::first(Value([9; 32])), NO_SEQ),
        )
        .expect("tamper");
    let err = storage
        .get_record(&key(&[0x02]))
        .expect_err("tampered")
        .to_string();
    assert!(err.contains("does not hash"), "{err}");

    // A chain whose version 0 names a predecessor, and one that runs out early.
    let odd = write_chain(&storage, key(&[0x03]), &values[..2]);
    storage
        .db
        .put_cf(
            storage.history(),
            history_key(odd[0].seq),
            encode_history_record(&odd[0].record, 7),
        )
        .expect("plant predecessor");
    let err = storage
        .get_history(&key(&[0x03]), ..)
        .expect_err("odd")
        .to_string();
    assert!(
        err.contains("version 0") && err.contains("predecessor"),
        "{err}"
    );
    storage
        .db
        .put_cf(
            storage.history(),
            history_key(odd[1].seq),
            encode_history_record(&odd[1].record, NO_SEQ),
        )
        .expect("cut the chain");
    let err = storage
        .get_history(&key(&[0x03]), ..)
        .expect_err("short")
        .to_string();
    assert!(err.contains("ends at version 1"), "{err}");
}

/// The level scan sees one length, in positional order, within its bounds; the metadata
/// beside the rows in its column family is outside every level.
#[test]
fn frontier_row_scans_see_one_length_within_their_bounds() {
    let storage = open();
    storage.commit_metadata(0, 4).expect("commit metadata");
    storage
        .write_frontier_rows(&[
            frontier_row(&[0x00], 4),
            frontier_row(&[0x40], 4),
            frontier_row(&[0x80], 4),
            frontier_row(&[0x80], 8),
            frontier_row(&[0xFF; 32], 255),
        ])
        .expect("write frontier rows");
    storage
        .write_leaves(&leaves([0x00, 0xFF]))
        .expect("write leaves");

    let at_4 = [
        positional(&[0x00], 4),
        positional(&[0x40], 4),
        positional(&[0x80], 4),
    ];
    assert_eq!(frontier_rows_at(&storage, 4).expect("4"), at_4);
    assert_eq!(
        frontier_rows_at(&storage, 8).expect("8"),
        [positional(&[0x80], 8)]
    );
    assert_eq!(frontier_rows_at(&storage, 5).expect("5"), []);
    assert_eq!(frontier_rows_at(&storage, 255).expect("255").len(), 1);

    let mut bounded = Vec::new();
    storage
        .for_each_frontier_row(4, &at_4[1], Some(&at_4[2]), |prefix, _, _| {
            bounded.push(prefix);
            Ok(())
        })
        .expect("bounded scan");
    assert_eq!(bounded, [at_4[1]]);
}

/// Records written by the production writers are exactly their pinned sizes, laid out as
/// HASHCHAINS.md says, in their column families, and decode back.
#[test]
fn records_are_bare_and_round_trip_through_their_writers() {
    let storage = open();
    let (k, v) = (key(&[0x77, 0x66, 0x55, 0xC3]), Value([0x33; 32]));
    let (left, right) = (Digest([0xAA; 32]), Digest([0xBB; 32]));
    let version = Version {
        key: k,
        seq: 0x0102030405060708,
        prev: 0x1112131415161718,
        version: 0x21222324,
        record: Record {
            value: v,
            link: Digest([0x44; 32]),
        },
    };
    let leaf = version.leaf_row();
    let mut batch = RocksWriteBatch::default();
    batch.put_leaf(&leaf);
    batch.put_history(&version);
    batch.put_frontier_node(&positional(&[], 3), left, right);
    storage.write_batch(batch).expect("write batch");

    let stored = storage.get_raw_leaf(&k).expect("raw get").expect("present");
    assert_eq!(stored.len(), LEAF_RECORD_LEN);
    assert_eq!(&stored[..32], version.record.leaf_hash(k).as_bytes());
    assert_eq!(&stored[32..40], &version.seq.to_be_bytes());
    assert_eq!(&stored[40..], &(version.version as u32).to_be_bytes());
    assert_eq!(leaf_from_record(k, &stored).expect("decode"), leaf);
    assert_eq!(
        leaf_key(&k).to_vec(),
        [&[0x77u8, 0x66, 0x55, 0xC0, 0x03][..], &[0u8; 28][..]].concat()
    );
    assert!(
        storage
            .get_raw_frontier(&Prefix::from(k))
            .expect("raw get")
            .is_none(),
        "a leaf is not a frontier row"
    );

    let history = storage
        .get_raw_history(version.seq)
        .expect("raw get")
        .expect("present");
    assert_eq!(history.len(), HISTORY_RECORD_LEN);
    assert_eq!(&history[..32], v.as_bytes());
    assert_eq!(&history[32..64], version.record.link.as_bytes());
    assert_eq!(&history[64..], &version.prev.to_be_bytes());
    assert_eq!(
        history_record(&history).expect("decode"),
        (version.record, version.prev)
    );
    assert_eq!(history_key(version.seq), version.seq.to_be_bytes());
    assert_eq!(
        decode_history_key(&history_key(version.seq)).expect("decode"),
        version.seq
    );
    assert!(decode_history_key(&k.0).is_err(), "a 32-byte key");

    let row = storage
        .get_raw_frontier(&positional(&[], 3))
        .expect("raw get")
        .expect("present");
    assert_eq!(row.len(), FRONTIER_RECORD_LEN);
    assert_eq!(
        (&row[..32], &row[32..]),
        (&left.as_bytes()[..], &right.as_bytes()[..])
    );
    assert_eq!(frontier_child_hashes(&row).expect("decode"), (left, right));

    // The next version of a leaf takes its link from the stored hash and points back at it.
    let next = leaf.next(Value([0x55; 32]), 99);
    assert_eq!(next.record.link, leaf.hash);
    assert_eq!(next.record, version.record.next(k, Value([0x55; 32])));
    assert_eq!(
        (next.prev, next.version, next.seq),
        (leaf.head, leaf.version + 1, 99)
    );
    assert_eq!(next.leaf_row().head, 99);

    // A version past 32 bits cannot be encoded.
    let mut absurd = leaf;
    absurd.version = u64::from(u32::MAX) + 1;
    assert!(std::panic::catch_unwind(|| encode_leaf(&absurd)).is_err());
}

/// Decoders accept exactly their own record length; v3 to v6 records are refused with the
/// format named; a leaf pointing at no history row, a node key past length 256 or with a
/// dirty tail are refused.
#[test]
fn decoders_refuse_every_other_shape_by_name() {
    let leaf = encode_leaf(&Version::first(key(&[1]), Value([3; 32]), 1).leaf_row());
    let row = encode_frontier_node(Digest([1; 32]), Digest([2; 32]));
    assert!(
        leaf_from_record(key(&[1]), &row).is_err(),
        "a frontier row is not a leaf"
    );
    assert!(
        frontier_child_hashes(&leaf).is_err(),
        "a leaf is not a frontier row"
    );
    assert!(
        history_record(&leaf).is_err(),
        "a leaf is not a history row"
    );
    let mut unlinked = leaf.clone();
    unlinked[32..40].copy_from_slice(&NO_SEQ.to_be_bytes());
    let err = leaf_from_record(key(&[1]), &unlinked)
        .expect_err("no head")
        .to_string();
    assert!(err.contains("no history row"), "{err}");

    type Decoder = fn(&[u8]) -> RocksResult<()>;
    let decoders: [Decoder; 3] = [
        |bytes| leaf_from_record(Key::ZERO, bytes).map(drop),
        |bytes| frontier_child_hashes(bytes).map(drop),
        |bytes| history_record(bytes).map(drop),
    ];
    for decode in decoders {
        for payload in [0, 1, 20, 33, 43, 45, 47, 49, 65, 71, 73, 100] {
            assert!(decode(&vec![0; payload]).is_err(), "{payload} bytes");
        }
        for legacy in [v3_leaf_record(), v3_frontier_record()] {
            let err = decode(&legacy).unwrap_err().to_string();
            assert!(
                err.contains("format v3") && err.contains("acff956"),
                "{err}"
            );
        }
        let err = decode(&v4_leaf_record()).unwrap_err().to_string();
        assert!(
            err.contains("format v4") && err.contains("7f87873"),
            "{err}"
        );
    }
    for (legacy, name) in [
        (v5_leaf_record(), "format v5"),
        (v6_leaf_record(), "format v6"),
    ] {
        let err = leaf_from_record(Key::ZERO, &legacy)
            .unwrap_err()
            .to_string();
        assert!(err.contains(name), "{err}");
    }

    let mut node_key = vec![0u8; NODE_KEY_LEN];
    node_key[..2].copy_from_slice(&257u16.to_be_bytes());
    let err = decode_prefix(&node_key).expect_err("length 257");
    assert!(
        matches!(err, RocksStorageError::Corrupt(ref m) if m.contains("257")),
        "{err}"
    );
    node_key[..2].copy_from_slice(&256u16.to_be_bytes());
    assert_eq!(decode_prefix(&node_key).expect("length 256").length(), 256);
    assert!(decode_prefix(&node_key[1..]).is_err(), "a short key");
    node_key[..2].copy_from_slice(&1u16.to_be_bytes());
    node_key[2] = 0x20; // bit 2 set at length 1
    let err = decode_prefix(&node_key)
        .expect_err("dirty tail")
        .to_string();
    assert!(err.contains("past its length"), "{err}");
    node_key[2] = 0x80;
    assert_eq!(
        decode_prefix(&node_key).expect("clean"),
        positional(&[0x80], 1)
    );

    assert!(std::error::Error::source(&RocksStorageError::corrupt("row")).is_none());
}

/// A legacy record is refused through every path that decodes a value; counting never
/// decodes.
#[test]
fn legacy_records_are_refused_through_every_read_path() {
    for legacy in [
        v3_leaf_record(),
        v4_leaf_record(),
        v5_leaf_record(),
        v6_leaf_record(),
    ] {
        let storage = open();
        let leaf_key_ = Key([1; 32]);
        storage
            .plant_raw_leaf(&leaf_key_, &legacy)
            .expect("plant record");
        storage
            .plant_raw_frontier(&prefix_key(&positional(&[0x80], 1)), &v3_frontier_record())
            .expect("plant record");

        assert!(storage.get_leaf_value(&leaf_key_).is_err(), "point get");
        assert!(
            storage.get_leaf_rows_by_prefix(&Prefix::root()).is_err(),
            "leaf scan"
        );
        assert!(frontier_rows_at(&storage, 1).is_err(), "level scan");
        assert_eq!(
            storage
                .count_leaves_by_prefix(&Prefix::root())
                .expect("count"),
            1
        );
    }
}

/// Rows at lengths 1..=255 in the frontier column family trip it; leaves, metadata and a
/// depth-0 root row do not.
#[test]
fn has_interior_rows_sees_exactly_the_interior_lengths() {
    let storage = open();
    assert!(!storage.has_interior_rows().expect("empty"));
    storage
        .write_leaves(&leaves([0x01, 0xF0]))
        .expect("write leaves");
    storage.commit_metadata(2, 0).expect("commit metadata");
    storage
        .plant_raw_frontier(
            &prefix_key(&Prefix::root()),
            &encode_frontier_node(Digest([1; 32]), Digest([2; 32])),
        )
        .expect("plant depth-0 row");
    assert!(
        !storage
            .has_interior_rows()
            .expect("leaves, metadata and a root row")
    );
    storage
        .write_frontier_rows(&[frontier_row(&[], 255)])
        .expect("write frontier row");
    assert!(storage.has_interior_rows().expect("interior row present"));
}

/// Regression test for the `PhysicalCoreID()` miscompile (DESIGN.md §8): concurrent writes
/// contend on a memtable arena shard, which calls it. Every batch spans all three column
/// families.
#[test]
fn concurrent_batch_writes_do_not_corrupt_the_memtable() {
    const THREADS: u8 = 16;
    const BATCHES_PER_THREAD: u8 = 32;
    const LEAVES_PER_BATCH: u8 = 64;
    let storage = open();
    let seqs = AtomicU64::new(1);
    std::thread::scope(|scope| {
        for thread in 0..THREADS {
            let (storage, seqs) = (&storage, &seqs);
            scope.spawn(move || {
                for batch_id in 0..BATCHES_PER_THREAD {
                    let mut batch = RocksWriteBatch::default();
                    for leaf in 0..LEAVES_PER_BATCH {
                        let seq = seqs.fetch_add(1, Ordering::Relaxed);
                        let version =
                            Version::first(key(&[thread, batch_id, leaf]), Value([0; 32]), seq);
                        batch.put_leaf(&version.leaf_row());
                        batch.put_history(&version);
                    }
                    batch.put_frontier_node(
                        &positional(&[thread, batch_id], 16),
                        Digest([1; 32]),
                        Digest([2; 32]),
                    );
                    storage.write_batch(batch).expect("write batch");
                }
            });
        }
    });
    let written = storage
        .count_leaves_by_prefix(&Prefix::root())
        .expect("count");
    assert_eq!(
        written,
        THREADS as usize * BATCHES_PER_THREAD as usize * LEAVES_PER_BATCH as usize
    );
    assert_eq!(storage.last_history_seq().expect("seq") as usize, written);
    assert_eq!(
        storage
            .get_history(&key(&[3, 4, 5]), ..)
            .expect("history")
            .len(),
        1
    );
    assert_eq!(frontier_rows_at(&storage, 16).expect("rows").len(), 512);
}

/// `sorted_runs` counts L0 files and populated levels and `compact_all` collapses them; a
/// plain `DB` leaves `max_write_buffer_size_to_maintain` at 0; the LOW pool does not exceed
/// `max_background_jobs`; the column families carry the options HASHCHAINS.md (Cost) argues
/// for.
#[test]
fn the_measured_rocksdb_options_are_in_force() {
    let dir = fresh_dir();
    let entries = leaves(0..=255);
    let mut storage = RocksStorage::open(&dir).expect("open rocksdb");
    assert_eq!(storage.sorted_runs(), 0, "nothing has reached an SST");
    for half in entries.chunks(128) {
        // Each reopen flushes the recovered memtable to its own L0 file.
        storage.write_leaves(half).expect("write leaves");
        storage.flush().expect("flush wal");
        drop(storage);
        storage = RocksStorage::open(&dir).expect("reopen rocksdb");
    }
    assert_eq!(
        storage.sorted_runs(),
        2,
        "one L0 file per recovered memtable"
    );
    let scanned = storage
        .get_leaf_rows_by_prefix(&Prefix::root())
        .expect("scan");
    assert_eq!(
        scanned.iter().map(|l| l.key).collect::<Vec<_>>(),
        entries.iter().map(|(k, _)| *k).collect::<Vec<_>>()
    );
    storage.compact_all();
    assert_eq!(
        storage.sorted_runs(),
        1,
        "one bottommost run after compaction"
    );
    for (k, v) in &entries {
        assert_eq!(
            storage.get_history(k, ..).expect("history"),
            [Record::first(*v)],
            "history survives the reopens and the compaction"
        );
    }

    let options_dump = std::fs::read_dir(&dir)
        .expect("read db dir")
        .filter_map(Result::ok)
        .find(|entry| entry.file_name().to_string_lossy().starts_with("OPTIONS-"))
        .expect("RocksDB writes an OPTIONS-* dump at open");
    let text = std::fs::read_to_string(options_dump.path()).expect("read options dump");
    let recorded = |section: &str, option: &str| {
        section
            .lines()
            .find_map(|line| line.trim().strip_prefix(option)?.strip_prefix('='))
            .unwrap_or_else(|| panic!("the dump records {option}"))
            .trim()
            .to_owned()
    };
    assert_eq!(
        recorded(&text, "max_write_buffer_size_to_maintain"),
        "0",
        "memtable history is retained for conflict checks"
    );
    assert_eq!(
        recorded(&text, "use_direct_io_for_flush_and_compaction"),
        "true"
    );
    // One [CFOptions "<name>"] section per column family, default first.
    let (leaves_section, rest) = text
        .split_once(&format!("[CFOptions \"{FRONTIER_CF}\"]"))
        .expect("the frontier column family is open");
    let (frontier_section, history_section) = rest
        .split_once(&format!("[CFOptions \"{HISTORY_CF}\"]"))
        .expect("the history column family is open");
    assert_eq!(
        recorded(leaves_section, "prefix_extractor"),
        format!("rocksdb.FixedPrefix.{LEAF_PREFIX_LEN}")
    );
    assert_eq!(recorded(frontier_section, "prefix_extractor"), "nullptr");
    assert_eq!(recorded(history_section, "prefix_extractor"), "nullptr");
    assert_eq!(
        recorded(leaves_section, "write_buffer_size"),
        LEAF_WRITE_BUFFER_SIZE.to_string()
    );
    assert_eq!(
        recorded(leaves_section, "max_write_buffer_number"),
        LEAF_WRITE_BUFFERS.to_string()
    );
    assert_eq!(
        recorded(leaves_section, "max_bytes_for_level_base"),
        LEAF_LEVEL_BASE.to_string()
    );
    assert_eq!(
        recorded(frontier_section, "write_buffer_size"),
        WRITE_BUFFER_SIZE.to_string()
    );
    assert_eq!(
        recorded(history_section, "write_buffer_size"),
        WRITE_BUFFER_SIZE.to_string()
    );
    for section in [leaves_section, frontier_section, history_section] {
        assert_eq!(
            recorded(section, "compaction_style"),
            "kCompactionStyleLevel"
        );
        assert_eq!(
            recorded(section, "level_compaction_dynamic_level_bytes"),
            "true"
        );
    }
    assert!(
        recorded(leaves_section, "filter_policy").contains("bloomfilter"),
        "leaves carry bloom filters"
    );
    assert_eq!(recorded(leaves_section, "whole_key_filtering"), "false");
    assert_eq!(recorded(frontier_section, "filter_policy"), "nullptr");

    if cfg!(target_os = "linux") {
        let low = std::fs::read_dir("/proc/self/task")
            .expect("read /proc/self/task")
            .filter_map(Result::ok)
            .filter(|task| {
                std::fs::read_to_string(task.path().join("comm"))
                    .is_ok_and(|n| n.trim() == "rocksdb:low")
            })
            .count();
        assert!(
            low <= 8,
            "LOW pool has {low} threads but max_background_jobs is 8"
        );
    }
}

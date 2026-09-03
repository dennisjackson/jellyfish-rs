use super::*;
use crate::testing::fresh_dir;

// Test-only storage surface: observe or plant state no production caller needs.
impl RocksStorage {
    /// The raw stored value, undecoded.
    pub(crate) fn get_raw(&self, prefix: &Prefix) -> RocksResult<Option<Vec<u8>>> {
        Ok(self
            .db
            .get_pinned(prefix_key(prefix))?
            .map(|raw| raw.as_ref().to_vec()))
    }

    /// Every write bumps it, so a delta of zero proves a path wrote nothing.
    pub(crate) fn latest_sequence_number(&self) -> u64 {
        self.db.latest_sequence_number()
    }

    /// `COMPLETE_DEPTH_KEY` alone: torn metadata no production path can write.
    pub(crate) fn plant_frontier_depth(&self, depth: u16) -> RocksResult<()> {
        self.db.put(COMPLETE_DEPTH_KEY, depth.to_be_bytes())?;
        Ok(())
    }

    pub(crate) fn delete_raw(&self, key: &[u8]) -> RocksResult<()> {
        self.db.delete(key)?;
        Ok(())
    }

    /// Leaves directly, bypassing the trie and the census.
    pub(crate) fn write_leaves(&self, entries: &[Entry]) -> RocksResult<()> {
        let mut batch = WriteBatch::default();
        for (key, value) in entries {
            batch.put(leaf_key(key), value.as_bytes());
        }
        self.db.write(batch)?;
        Ok(())
    }

    pub(crate) fn write_frontier_rows(&self, rows: &[(Prefix, Digest, Digest)]) -> RocksResult<()> {
        let mut batch = RocksWriteBatch::default();
        for (prefix, left, right) in rows {
            batch.put_frontier_node(prefix, *left, *right);
        }
        self.write_batch(batch)
    }
}

impl RocksWriteBatch {
    /// Raw bytes under a raw key, bypassing the codec.
    pub(crate) fn put_raw(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.batch.put(key, value);
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

fn frontier_rows_at(storage: &RocksStorage, depth: u16) -> RocksResult<Vec<Prefix>> {
    let mut rows = Vec::new();
    storage.for_each_frontier_row(depth, &positional(&[], depth), None, |prefix, _, _| {
        rows.push(prefix);
        Ok(())
    })?;
    Ok(rows)
}

/// No metadata until a batch commits (distinct from a zero count); the pair round-trips;
/// one without the other is refused.
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

    let torn = open();
    torn.plant_frontier_depth(4).expect("plant depth");
    let err = torn.read_metadata().expect_err("torn").to_string();
    assert!(err.contains("only one is present"), "{err}");
}

/// A key of another length inside the leaf range is refused, not skipped.
#[test]
fn a_foreign_key_inside_the_leaf_range_is_refused() {
    let storage = open();
    storage
        .write_leaves(&leaves([0x10, 0x20]))
        .expect("write leaves");
    let mut foreign = RocksWriteBatch::default();
    foreign.put_raw(
        [&256u16.to_be_bytes()[..], &[0x18u8][..]].concat(),
        vec![0; 32],
    );
    storage.write_batch(foreign).expect("plant a 3-byte key");
    assert!(storage.get_leaf_entries_by_prefix(&Prefix::root()).is_err());
    assert!(storage.count_leaves_by_prefix(&Prefix::root()).is_err());
    assert_eq!(
        storage.get_leaf_value(&key(&[0x20])).expect("point get"),
        Some(Value([0x20; 32]))
    );
}

/// Leaf scans see exactly the leaves under a prefix: the root and all-ones prefixes (no
/// successor; must stop before the metadata keys), leaves at both range boundaries, and a
/// full-length prefix.
#[test]
fn leaf_scans_see_exactly_the_leaves_under_a_prefix() {
    let storage = open();
    storage.commit_metadata(0, 7).expect("commit metadata");
    let mut entries = leaves((0..64u8).map(|i| i.wrapping_mul(4)));
    entries.extend([
        (key(&[0xDF; 32]), Value([1; 32])), // last key before "1110"
        (key(&[0xEF; 32]), Value([2; 32])), // last key under "1110"
        (Key([0xFF; 32]), Value([3; 32])),  // the largest key there is
    ]);
    entries.sort();
    storage.write_leaves(&entries).expect("write leaves");

    let mut prefixes = vec![Prefix::root(), Prefix::from(entries[5].0)];
    for length in [1, 2, 3, 4, 8] {
        for byte in [0x00, 0x40, 0x80, 0xC0, 0xE0, 0xF0, 0xFF] {
            prefixes.push(positional(&[byte], length));
        }
    }
    for prefix in prefixes {
        let expected: Vec<Entry> = entries
            .iter()
            .copied()
            .filter(|(k, _)| prefix.contains(k))
            .collect();
        let scanned = storage
            .get_leaf_entries_by_prefix(&prefix)
            .expect("entry scan");
        assert_eq!(scanned, expected, "scan at {prefix:?}");
        let count = storage.count_leaves_by_prefix(&prefix).expect("count scan");
        assert_eq!(count, expected.len(), "count at {prefix:?}");
    }
    for (k, v) in &entries {
        assert_eq!(storage.get_leaf_value(k).expect("point get"), Some(*v));
    }
    assert_eq!(
        storage.get_leaf_value(&key(&[0xDE])).expect("point get"),
        None
    );
}

/// The level scan sees one length, in positional order, within its bounds.
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

    let mut bounded = Vec::new();
    storage
        .for_each_frontier_row(4, &at_4[1], Some(&at_4[2]), |prefix, _, _| {
            bounded.push(prefix);
            Ok(())
        })
        .expect("bounded scan");
    assert_eq!(bounded, [at_4[1]]);
}

/// Records written by the production writers are exactly their pinned sizes and decode back.
#[test]
fn records_are_bare_and_round_trip_through_their_writers() {
    let storage = open();
    let (k, v) = (key(&[0x77]), Value([0x33; 32]));
    let (left, right) = (Digest([0xAA; 32]), Digest([0xBB; 32]));
    let mut batch = RocksWriteBatch::default();
    batch.put_leaf(&k, &v);
    batch.put_frontier_node(&positional(&[], 3), left, right);
    storage.write_batch(batch).expect("write batch");
    let raw = |prefix| storage.get_raw(&prefix).expect("raw get").expect("present");

    let leaf = raw(Prefix::from(k));
    assert_eq!(leaf.len(), LEAF_RECORD_LEN);
    assert_eq!(&leaf[..], v.as_bytes());
    assert_eq!(leaf_value_from_record(&leaf).expect("decode"), v);

    let row = raw(positional(&[], 3));
    assert_eq!(row.len(), FRONTIER_RECORD_LEN);
    assert_eq!(
        (&row[..32], &row[32..]),
        (&left.as_bytes()[..], &right.as_bytes()[..])
    );
    assert_eq!(frontier_child_hashes(&row).expect("decode"), (left, right));
}

/// Decoders accept exactly their own record length; v3 records are refused with the escape
/// hatch named; a node key past length 256 or with a dirty tail is refused.
#[test]
fn decoders_refuse_every_other_shape_by_name() {
    let leaf = Value([3; 32]);
    let row = encode_frontier_node(Digest([1; 32]), Digest([2; 32]));
    assert!(
        leaf_value_from_record(&row).is_err(),
        "a frontier row is not a leaf"
    );
    assert!(
        frontier_child_hashes(leaf.as_bytes()).is_err(),
        "a leaf is not a frontier row"
    );

    type Decoder = fn(&[u8]) -> RocksResult<()>;
    let decoders: [Decoder; 2] = [
        |bytes| leaf_value_from_record(bytes).map(drop),
        |bytes| frontier_child_hashes(bytes).map(drop),
    ];
    for decode in decoders {
        for payload in [0, 1, 20, 33, 65, 100] {
            assert!(decode(&vec![0; payload]).is_err(), "{payload} bytes");
        }
        for legacy in [v3_leaf_record(), v3_frontier_record()] {
            let err = decode(&legacy).unwrap_err().to_string();
            assert!(
                err.contains("format v3") && err.contains("acff956"),
                "{err}"
            );
        }
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

/// A v3 record is refused through every path that decodes a value; counting never decodes.
#[test]
fn legacy_records_are_refused_through_every_read_path() {
    let storage = open();
    let leaf_key_ = Key([1; 32]);
    let mut batch = RocksWriteBatch::default();
    batch.put_raw(leaf_key(&leaf_key_), v3_leaf_record());
    batch.put_raw(prefix_key(&positional(&[0x80], 1)), v3_frontier_record());
    storage.write_batch(batch).expect("plant records");

    assert!(storage.get_leaf_value(&leaf_key_).is_err(), "point get");
    assert!(
        storage.get_leaf_entries_by_prefix(&Prefix::root()).is_err(),
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

/// Rows at lengths 1..=255 trip it; leaves and a depth-0 root row do not.
#[test]
fn has_interior_rows_sees_exactly_the_interior_lengths() {
    let storage = open();
    assert!(!storage.has_interior_rows().expect("empty"));
    storage
        .write_leaves(&leaves([0x01, 0xF0]))
        .expect("write leaves");
    let mut batch = RocksWriteBatch::default();
    batch.put_raw(
        prefix_key(&Prefix::root()),
        encode_frontier_node(Digest([1; 32]), Digest([2; 32])),
    );
    storage.write_batch(batch).expect("plant depth-0 row");
    assert!(!storage.has_interior_rows().expect("leaves and a root row"));
    storage
        .write_frontier_rows(&[frontier_row(&[], 255)])
        .expect("write frontier row");
    assert!(storage.has_interior_rows().expect("interior row present"));
}

/// Regression test for the `PhysicalCoreID()` miscompile (DESIGN.md §8): concurrent writes
/// contend on a memtable arena shard, which calls it.
#[test]
fn concurrent_batch_writes_do_not_corrupt_the_memtable() {
    const THREADS: u8 = 16;
    const BATCHES_PER_THREAD: u8 = 32;
    const LEAVES_PER_BATCH: u8 = 64;
    let storage = open();
    std::thread::scope(|scope| {
        for thread in 0..THREADS {
            let storage = &storage;
            scope.spawn(move || {
                for batch_id in 0..BATCHES_PER_THREAD {
                    let mut batch = RocksWriteBatch::default();
                    for leaf in 0..LEAVES_PER_BATCH {
                        batch.put_leaf(&key(&[thread, batch_id, leaf]), &Value([0; 32]));
                    }
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
}

/// `sorted_runs` counts L0 files and populated levels and `compact_all` collapses them; a
/// plain `DB` leaves `max_write_buffer_size_to_maintain` at 0; the LOW pool does not exceed
/// `max_background_jobs`.
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
        .get_leaf_entries_by_prefix(&Prefix::root())
        .expect("scan");
    assert_eq!(scanned, entries);
    storage.compact_all();
    assert_eq!(
        storage.sorted_runs(),
        1,
        "one bottommost run after compaction"
    );

    let options_dump = std::fs::read_dir(&dir)
        .expect("read db dir")
        .filter_map(Result::ok)
        .find(|entry| entry.file_name().to_string_lossy().starts_with("OPTIONS-"))
        .expect("RocksDB writes an OPTIONS-* dump at open");
    let text = std::fs::read_to_string(options_dump.path()).expect("read options dump");
    let recorded = |option: &str| {
        text.lines()
            .find_map(|line| line.trim().strip_prefix(option)?.strip_prefix('='))
            .unwrap_or_else(|| panic!("the dump records {option}"))
            .trim()
            .to_owned()
    };
    assert_eq!(
        recorded("max_write_buffer_size_to_maintain"),
        "0",
        "memtable history is retained for conflict checks"
    );
    assert_eq!(recorded("write_buffer_size"), WRITE_BUFFER_SIZE.to_string());

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

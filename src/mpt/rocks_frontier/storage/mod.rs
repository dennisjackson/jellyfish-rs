//! The RocksDB layer: three column families ([`codec`]), bounded scans, the write batch.
//! Options are hardcoded; the sweeps behind each are in docs/old/REVIEW.md and
//! docs/HASHCHAINS.md (Cost).

use std::fmt;
use std::ops::{Bound, RangeBounds};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use rocksdb::statistics::{StatsLevel, Ticker};
use rocksdb::{
    BlockBasedOptions, Cache, ColumnFamily, ColumnFamilyDescriptor, DB, Direction, IteratorMode,
    Options, ReadOptions, SliceTransform, WriteBatch,
};

use crate::census::{Census, CensusSnapshot, Metric};
use crate::{Digest, Key, Prefix, Record, Value};

mod codec;
pub(crate) use codec::*;

pub type RocksResult<T> = Result<T, RocksStorageError>;

/// A leaf as the node column family holds it: the Merkle hash of the key's current record,
/// the history row that record sits in, and how many records preceded it. The subtree
/// rebuild needs the hash alone; an overwrite needs all three and never reads the record
/// (HASHCHAINS.md, Database Schema).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeafRow {
    pub key: Key,
    pub hash: Digest,
    /// Sequence number of the history row holding the current record.
    pub head: u64,
    /// Records written under the key before the current one; the first write is 0.
    pub version: u64,
}

/// One version of a key: a history row and the key it belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    pub key: Key,
    /// This row's sequence number.
    pub seq: u64,
    /// The row it replaced, or [`NO_SEQ`] for a key's first record.
    pub prev: u64,
    pub version: u64,
    pub record: Record,
}

impl Version {
    /// A key's first version, at row `seq`.
    pub fn first(key: Key, value: Value, seq: u64) -> Self {
        Self {
            key,
            seq,
            prev: NO_SEQ,
            version: 0,
            record: Record::first(value),
        }
    }

    /// The leaf row that names this version as current.
    pub fn leaf_row(&self) -> LeafRow {
        LeafRow {
            key: self.key,
            hash: self.record.leaf_hash(self.key),
            head: self.seq,
            version: self.version,
        }
    }
}

impl LeafRow {
    /// The version that replaces this leaf's record with `value`, at row `seq`. The new link
    /// is the stored hash, so nothing is read.
    pub fn next(&self, value: Value, seq: u64) -> Version {
        Version {
            key: self.key,
            seq,
            prev: self.head,
            version: self.version + 1,
            record: Record {
                value,
                link: self.hash,
            },
        }
    }
}

#[derive(Debug)]
pub enum RocksStorageError {
    Db(rocksdb::Error),
    /// On-disk state this build refuses to interpret.
    Corrupt(String),
}

impl RocksStorageError {
    pub fn corrupt(message: impl Into<String>) -> Self {
        RocksStorageError::Corrupt(message.into())
    }
}

impl fmt::Display for RocksStorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RocksStorageError::Db(err) => write!(f, "rocksdb error: {err}"),
            RocksStorageError::Corrupt(err) => write!(f, "corrupt database: {err}"),
        }
    }
}

impl std::error::Error for RocksStorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RocksStorageError::Db(err) => Some(err),
            RocksStorageError::Corrupt(_) => None,
        }
    }
}

impl From<rocksdb::Error> for RocksStorageError {
    fn from(err: rocksdb::Error) -> Self {
        RocksStorageError::Db(err)
    }
}

pub struct RocksStorage {
    db: DB,
    /// Kept for `get_ticker_count`; the statistics object hangs off the options.
    options: Options,
    census: Census,
    /// Ticker readings when the census phase opened; tickers count from open.
    ticker_base: [AtomicU64; TICKER_METRICS.len()],
}

/// RocksDB's default of -1 hit EMFILE (BENCHMARK-BASELINE.md §1.5).
const MAX_OPEN_FILES: i32 = 4096;
/// The default; 256 MB within noise (REVIEW.md §5.7).
const WRITE_BUFFER_SIZE: usize = 64 * 1024 * 1024;
/// Larger bought nothing but RSS at either scale (REVIEW.md §7).
const BLOCK_CACHE_SIZE: usize = 1024 * 1024 * 1024;
const MAX_BACKGROUND_JOBS: i32 = 8;
const MAX_SUBCOMPACTIONS: u32 = 4;

/// The default (REVIEW.md §3.3.6).
const BLOCK_SIZE: usize = 4096;

/// Bits per prefix in the leaf column family's bloom filters. Filters are over the 5-byte
/// key prefix, not whole keys: a subtree scan asks each sorted run whether it holds any leaf
/// under the prefix before reading a data block from it (HASHCHAINS.md, Cost).
const LEAF_BLOOM_BITS: f64 = 10.0;

/// The history column family's own cache: its flushes and its rare reads must not evict
/// leaf blocks.
const HISTORY_BLOCK_CACHE_SIZE: usize = 64 * 1024 * 1024;

/// Leaf rows per data block: the granularity every scan is charged at (DESIGN.md, Representation).
pub(crate) const LEAVES_PER_BLOCK: u64 = (BLOCK_SIZE / (LEAF_KEY_LEN + LEAF_RECORD_LEN)) as u64;

/// The leaf column family's LSM shape. With dynamic level sizing the number of populated
/// levels is about log10(data / base): at 44 GB of leaves a 256 MB base gave four (L3–L6),
/// each a bloom probe and often a block per scan and each a rewrite per byte compacted; a
/// 2 GB base gives two. L0 is kept at about the base size so the L0→base compaction stays
/// cheap: bigger memtables, and enough of them that a 512 MB flush cannot stall writes
/// (HASHCHAINS.md, Cost).
const LEAF_LEVEL_BASE: u64 = 2 * 1024 * 1024 * 1024;
const LEAF_WRITE_BUFFER_SIZE: usize = 512 * 1024 * 1024;
const LEAF_WRITE_BUFFERS: i32 = 4;

/// Sampled metrics and the tickers each sums.
const TICKER_METRICS: [(Metric, &[Ticker]); 3] = [
    (Metric::DataBlocksRead, &[Ticker::BlockCacheDataMiss]),
    (
        Metric::IndexBlocksRead,
        &[Ticker::BlockCacheIndexMiss, Ticker::BlockCacheFilterMiss],
    ),
    (Metric::Seeks, &[Ticker::NumberDbSeek]),
];

impl RocksStorage {
    pub fn open(path: impl AsRef<Path>) -> RocksResult<Self> {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        options.set_max_open_files(MAX_OPEN_FILES);
        options.set_write_buffer_size(WRITE_BUFFER_SIZE);
        options.set_enable_pipelined_write(true);
        options.set_allow_concurrent_memtable_write(true);
        options.set_manual_wal_flush(true);
        options.set_max_subcompactions(MAX_SUBCOMPACTIONS);
        options.set_max_background_jobs(MAX_BACKGROUND_JOBS);
        // Past the RAM size the page cache's hit rate on leaf blocks decides throughput.
        // Flushes and compactions write through direct I/O so their output cannot evict the
        // blocks the scans want (HASHCHAINS.md, Cost).
        options.set_use_direct_io_for_flush_and_compaction(true);
        // The cheapest level that still counts the tickers the census samples.
        options.enable_statistics();
        options.set_statistics_level(StatsLevel::ExceptHistogramOrTimers);

        // A block cache, not a row cache: the hot reads are iterators. Shared by the leaves
        // and the frontier rows.
        let cache = Cache::new_lru_cache(BLOCK_CACHE_SIZE);
        let mut block_options = BlockBasedOptions::default();
        block_options.set_block_cache(&cache);
        options.set_block_based_table_factory(&block_options);

        let column_families = [
            ColumnFamilyDescriptor::new(
                rocksdb::DEFAULT_COLUMN_FAMILY_NAME,
                leaf_options(&options, &cache),
            ),
            ColumnFamilyDescriptor::new(FRONTIER_CF, options.clone()),
            ColumnFamilyDescriptor::new(HISTORY_CF, history_options(&options)),
        ];
        let db = DB::open_cf_descriptors(&options, path, column_families)?;
        let storage = Self {
            db,
            options,
            census: Census::new(),
            ticker_base: Default::default(),
        };
        storage.census_reset();
        Ok(storage)
    }

    fn history(&self) -> &ColumnFamily {
        self.db
            .cf_handle(HISTORY_CF)
            .expect("the history column family is opened with the database")
    }

    fn frontier(&self) -> &ColumnFamily {
        self.db
            .cf_handle(FRONTIER_CF)
            .expect("the frontier column family is opened with the database")
    }

    /// The counters bumped directly. Sampled metrics are stale here; use
    /// [`Self::census_snapshot`].
    pub(crate) fn census(&self) -> &Census {
        &self.census
    }

    /// The census with the sampled metrics refreshed against this phase's baseline.
    pub fn census_snapshot(&self) -> CensusSnapshot {
        for (slot, (metric, tickers)) in TICKER_METRICS.iter().enumerate() {
            let base = self.ticker_base[slot].load(Ordering::Relaxed);
            self.census[*metric].set(self.ticker_total(tickers).saturating_sub(base));
        }
        self.census.snapshot()
    }

    /// Zero the counters and re-baseline the tickers.
    pub fn census_reset(&self) {
        self.census.reset();
        for (slot, (_, tickers)) in TICKER_METRICS.iter().enumerate() {
            self.ticker_base[slot].store(self.ticker_total(tickers), Ordering::Relaxed);
        }
    }

    fn ticker_total(&self, tickers: &[Ticker]) -> u64 {
        tickers
            .iter()
            .map(|&ticker| self.options.get_ticker_count(ticker))
            .sum()
    }

    /// Collapse every column family to one sorted run; blocks until done (DESIGN.md, Performance).
    pub fn compact_all(&self) {
        self.db.compact_range::<&[u8], &[u8]>(None, None);
        self.db
            .compact_range_cf::<&[u8], &[u8]>(self.frontier(), None, None);
        self.db
            .compact_range_cf::<&[u8], &[u8]>(self.history(), None, None);
    }

    /// Leaf count, frontier depth and format version in one `WriteBatch`. Not counted as a
    /// node batch.
    pub fn commit_metadata(&self, leaf_count: u64, frontier_depth: u16) -> RocksResult<()> {
        let mut batch = WriteBatch::default();
        put_metadata(&mut batch, self.frontier(), leaf_count, frontier_depth);
        self.db.write(batch)?;
        Ok(())
    }

    /// `batch` plus the metadata, atomically.
    pub fn write_batch_with_metadata(
        &self,
        mut batch: RocksWriteBatch,
        leaf_count: u64,
        frontier_depth: u16,
    ) -> RocksResult<()> {
        put_metadata(
            &mut batch.batch,
            self.frontier(),
            leaf_count,
            frontier_depth,
        );
        self.write_batch(batch)
    }

    /// `(leaf_count, frontier_depth)`, or `None` if no batch ever committed. The three
    /// metadata records commit together, so some without the others is refused, and a
    /// count and depth without a format version is a v4 database. Formats up to v6 kept the
    /// metadata in the leaf column family; finding it there names the format.
    pub fn read_metadata(&self) -> RocksResult<Option<(u64, u16)>> {
        fn fixed<const N: usize>(
            raw: Option<rocksdb::DBPinnableSlice<'_>>,
            what: &str,
        ) -> RocksResult<Option<[u8; N]>> {
            raw.map(|raw| {
                raw.as_ref().try_into().map_err(|_| {
                    RocksStorageError::corrupt(format!(
                        "{what} must be {N} bytes, got {}",
                        raw.len()
                    ))
                })
            })
            .transpose()
        }
        if let Some(legacy) = self.db.get_pinned(FORMAT_KEY)? {
            let format = fixed::<2>(Some(legacy), "the format version")?.expect("present");
            return Err(RocksStorageError::corrupt(format!(
                "the database is format v{}, whose metadata sits among the leaves; this \
                 build reads format v{FORMAT_VERSION}. Rebuild the database",
                u16::from_be_bytes(format)
            )));
        }
        if self.db.get_pinned(LEAF_COUNT_KEY)?.is_some() {
            return Err(RocksStorageError::corrupt(format!(
                "the database is format v4 (bare 32-byte leaf values, metadata among the \
                 leaves); this build reads format v{FORMAT_VERSION}. Open it with a build at \
                 commit 7f87873, or rebuild"
            )));
        }
        let frontier = self.frontier();
        let count = fixed::<8>(
            self.db.get_pinned_cf(frontier, LEAF_COUNT_KEY)?,
            "the leaf count",
        )?;
        let depth = fixed::<2>(
            self.db.get_pinned_cf(frontier, COMPLETE_DEPTH_KEY)?,
            "the frontier depth",
        )?;
        let format = fixed::<2>(
            self.db.get_pinned_cf(frontier, FORMAT_KEY)?,
            "the format version",
        )?;
        match (count, depth, format) {
            (None, None, None) => Ok(None),
            (Some(count), Some(depth), Some(format)) => {
                let format = u16::from_be_bytes(format);
                if format != FORMAT_VERSION {
                    return Err(RocksStorageError::corrupt(format!(
                        "the database is format v{format}; this build reads format \
                         v{FORMAT_VERSION}. Rebuild the database"
                    )));
                }
                Ok(Some((u64::from_be_bytes(count), u16::from_be_bytes(depth))))
            }
            (Some(_), Some(_), None) => Err(RocksStorageError::corrupt(format!(
                "the metadata names no format version, so the database is format v4 (bare \
                 32-byte leaf values); this build reads format v{FORMAT_VERSION}, whose leaves \
                 carry a hash chain. Open it with a build at commit 7f87873, or rebuild"
            ))),
            _ => Err(RocksStorageError::corrupt(
                "the leaf count, the frontier depth and the format version commit together, \
                 but only some are present: the metadata is torn. Rebuild the database",
            )),
        }
    }

    /// The highest history sequence number on disk, or [`NO_SEQ`] if none: one seek to the
    /// end of the history column family. Rows are keyed by sequence, so this is where the
    /// next batch continues from, whatever the metadata says.
    pub fn last_history_seq(&self) -> RocksResult<u64> {
        let mut iter = self.db.iterator_cf(self.history(), IteratorMode::End);
        match iter.next() {
            Some(Ok((key, _))) => decode_history_key(key.as_ref()),
            Some(Err(err)) => Err(err.into()),
            None => Ok(NO_SEQ),
        }
    }

    /// L0 files plus one per populated deeper level, in the leaf column family.
    pub fn sorted_runs(&self) -> u64 {
        // RocksDB's default `num_levels`.
        const NUM_LEVELS: usize = 7;
        let files_at = |level: usize| {
            self.db
                .property_int_value(&format!("rocksdb.num-files-at-level{level}"))
                .ok()
                .flatten()
                .unwrap_or(0)
        };
        (1..NUM_LEVELS)
            .map(|l| u64::from(files_at(l) > 0))
            .sum::<u64>()
            + files_at(0)
    }

    /// Every leaf row under `prefix`, in key order.
    pub fn get_leaf_rows_by_prefix(&self, prefix: &Prefix) -> RocksResult<Vec<LeafRow>> {
        let mut results = Vec::new();
        self.for_each_leaf_record(prefix, |key, record| {
            results.push(leaf_from_record(key, record)?);
            Ok(())
        })?;
        Ok(results)
    }

    /// Any frontier row at a length in `1..=255`; one bounded seek. Length 0 is excluded: old
    /// builds persisted a root row.
    pub fn has_interior_rows(&self) -> RocksResult<bool> {
        let start_key = length_bound(1);
        let mut read_opts = ReadOptions::default();
        read_opts.set_iterate_upper_bound(length_bound(256).to_vec());
        let mode = IteratorMode::From(&start_key, Direction::Forward);
        match self
            .db
            .iterator_cf_opt(self.frontier(), read_opts, mode)
            .next()
        {
            Some(Ok(_)) => Ok(true),
            Some(Err(err)) => Err(err.into()),
            None => Ok(false),
        }
    }

    /// Counts without decoding a value.
    pub fn count_leaves_by_prefix(&self, prefix: &Prefix) -> RocksResult<usize> {
        let mut count = 0usize;
        self.for_each_leaf_record(prefix, |_, _| {
            count += 1;
            Ok(())
        })?;
        Ok(count)
    }

    /// The leaf scan. The upper bound is the successor's leaf key, or none for the root and
    /// all-ones prefixes, since the column family holds nothing but leaves; so every key
    /// returned is a leaf key under `prefix`. A key of another shape is corrupt: skipping it
    /// would silently hash a prefix of the leaves. Under a prefix of at least
    /// [`LEAF_PREFIX_BITS`] bits every key in range shares the bloom prefix, so the seek may
    /// consult the filters; shallower scans seek in total order.
    fn for_each_leaf_record<F>(&self, prefix: &Prefix, mut f: F) -> RocksResult<()>
    where
        F: FnMut(Key, &[u8]) -> RocksResult<()>,
    {
        let (start_key, end_key) = leaf_scan_range(prefix);

        let mut read_opts = ReadOptions::default();
        if let Some(end_key) = end_key {
            read_opts.set_iterate_upper_bound(end_key.to_vec());
        }
        read_opts.set_total_order_seek(prefix.length() < LEAF_PREFIX_BITS);

        let mode = IteratorMode::From(start_key.as_slice(), Direction::Forward);

        for entry in self.db.iterator_opt(mode, read_opts) {
            let (key, value) = entry?;
            let leaf = decode_leaf_key(key.as_ref())?;
            debug_assert!(
                prefix.contains(&leaf),
                "the bound admits only keys under the scanned prefix"
            );
            f(leaf, value.as_ref())?;
        }

        Ok(())
    }

    /// Frontier rows at `depth` from `start` (inclusive) to `end` (exclusive; the level's end
    /// if `None`), streamed in positional order.
    pub fn for_each_frontier_row<F>(
        &self,
        depth: u16,
        start: &Prefix,
        end: Option<&Prefix>,
        mut f: F,
    ) -> RocksResult<()>
    where
        F: FnMut(Prefix, Digest, Digest) -> RocksResult<()>,
    {
        debug_assert_eq!(
            start.length(),
            depth,
            "the start bound must sit at the level"
        );
        let start_key = prefix_key(start);
        let mut read_opts = ReadOptions::default();
        read_opts.set_iterate_upper_bound(match end {
            Some(end) => {
                debug_assert_eq!(end.length(), depth, "the end bound must sit at the level");
                prefix_key(end)
            }
            None => length_bound(depth + 1).to_vec(),
        });
        let mode = IteratorMode::From(&start_key, Direction::Forward);

        for entry in self.db.iterator_cf_opt(self.frontier(), read_opts, mode) {
            let (key, value) = entry?;
            debug_assert_eq!(key.len(), NODE_KEY_LEN, "the bound admits only node keys");
            let prefix = decode_prefix(key.as_ref())?;
            debug_assert_eq!(prefix.length(), depth, "the bound admits only this length");
            let (left_hash, right_hash) = frontier_child_hashes(value.as_ref())?;
            f(prefix, left_hash, right_hash)?;
        }
        Ok(())
    }

    /// The leaf row under `key`.
    pub fn get_leaf_row(&self, key: &Key) -> RocksResult<Option<LeafRow>> {
        match self.db.get_pinned(leaf_key(key))? {
            Some(raw) => Ok(Some(leaf_from_record(*key, raw.as_ref())?)),
            None => Ok(None),
        }
    }

    /// `(record, prev_seq)` at history row `seq`.
    pub fn get_history_row(&self, seq: u64) -> RocksResult<Option<(Record, u64)>> {
        match self.db.get_pinned_cf(self.history(), history_key(seq))? {
            Some(raw) => Ok(Some(history_record(raw.as_ref())?)),
            None => Ok(None),
        }
    }

    /// The current record under `key` and its version: the leaf row, then its head history
    /// row, which must hash to what the leaf stores.
    pub fn get_record(&self, key: &Key) -> RocksResult<Option<(u64, Record)>> {
        let Some(leaf) = self.get_leaf_row(key)? else {
            return Ok(None);
        };
        let (record, _) = self.follow(&leaf, leaf.head, leaf.version)?;
        if record.leaf_hash(*key) != leaf.hash {
            return Err(RocksStorageError::corrupt(format!(
                "the leaf under {} does not hash to the record its history row {} holds",
                key.short_hex(),
                leaf.head
            )));
        }
        Ok(Some((leaf.version, record)))
    }

    pub fn get_leaf_value(&self, key: &Key) -> RocksResult<Option<Value>> {
        Ok(self.get_record(key)?.map(|(_, record)| record.value))
    }

    /// The records at versions `range` of `key`, oldest first: the chain walked backwards
    /// from the leaf's head row along `prev_seq`, which must reach version 0 exactly when it
    /// runs out of rows.
    pub fn get_history(&self, key: &Key, range: impl RangeBounds<u64>) -> RocksResult<Vec<Record>> {
        let first = match range.start_bound() {
            Bound::Included(&first) => first,
            Bound::Excluded(&before) => before.saturating_add(1),
            Bound::Unbounded => 0,
        };
        let last = match range.end_bound() {
            Bound::Included(&last) => Some(last),
            Bound::Excluded(&0) => return Ok(Vec::new()),
            Bound::Excluded(&end) => Some(end - 1),
            Bound::Unbounded => None,
        };
        if last.is_some_and(|last| last < first) {
            return Ok(Vec::new());
        }
        let Some(leaf) = self.get_leaf_row(key)? else {
            return Ok(Vec::new());
        };

        let mut records = Vec::new();
        let (mut seq, mut version) = (leaf.head, leaf.version);
        loop {
            let (record, prev) = self.follow(&leaf, seq, version)?;
            if version >= first && last.is_none_or(|last| version <= last) {
                records.push(record);
            }
            if version == 0 {
                if prev != NO_SEQ {
                    return Err(RocksStorageError::corrupt(format!(
                        "version 0 of {} at row {seq} names a predecessor {prev}",
                        key.short_hex()
                    )));
                }
                break;
            }
            if version <= first {
                break;
            }
            if prev == NO_SEQ {
                return Err(RocksStorageError::corrupt(format!(
                    "the chain of {} ends at version {version} (row {seq}) instead of 0",
                    key.short_hex()
                )));
            }
            seq = prev;
            version -= 1;
        }
        records.reverse();
        Ok(records)
    }

    /// History row `seq`, which `leaf`'s chain names as version `version`.
    fn follow(&self, leaf: &LeafRow, seq: u64, version: u64) -> RocksResult<(Record, u64)> {
        self.get_history_row(seq)?.ok_or_else(|| {
            RocksStorageError::corrupt(format!(
                "version {version} of {} should sit at history row {seq}, which is missing",
                leaf.key.short_hex()
            ))
        })
    }

    pub fn flush(&self) -> RocksResult<()> {
        self.db.flush_wal(true)?;
        Ok(())
    }

    pub fn write_batch(&self, batch: RocksWriteBatch) -> RocksResult<()> {
        // Tallies land at commit, not per record (REVIEW.md §19).
        self.census[Metric::BytesStaged].add(batch.bytes_staged);
        self.census[Metric::LeafPuts].add(batch.leaf_puts);
        self.census[Metric::InteriorPuts].add(batch.frontier.len() as u64);
        self.census[Metric::HistoryPuts].add(batch.history.len() as u64);
        self.census[Metric::BatchesCommitted].bump();
        let RocksWriteBatch {
            mut batch,
            frontier,
            history,
            ..
        } = batch;
        // Rows for the other column families need handles only the store has.
        let cf = self.frontier();
        for (prefix, left, right) in &frontier {
            batch.put_cf(cf, prefix_key(prefix), encode_frontier_node(*left, *right));
        }
        let cf = self.history();
        for version in &history {
            batch.put_cf(
                cf,
                history_key(version.seq),
                encode_history_record(&version.record, version.prev),
            );
        }
        self.db.write(batch)?;
        Ok(())
    }
}

/// The leaf column family: the store's options plus prefix bloom filters over the first
/// [`LEAF_PREFIX_LEN`] key bytes. Whole-key filters were measured to do nothing (DESIGN.md,
/// Performance), since scans cannot use them; a prefix filter lets a scan skip a sorted run
/// that holds no leaf under its prefix, which is otherwise one data block read per run.
fn leaf_options(base: &Options, cache: &Cache) -> Options {
    let mut options = base.clone();
    options.set_prefix_extractor(SliceTransform::create_fixed_prefix(LEAF_PREFIX_LEN));
    options.set_max_bytes_for_level_base(LEAF_LEVEL_BASE);
    options.set_write_buffer_size(LEAF_WRITE_BUFFER_SIZE);
    options.set_max_write_buffer_number(LEAF_WRITE_BUFFERS);
    let mut block_options = BlockBasedOptions::default();
    block_options.set_block_cache(cache);
    block_options.set_bloom_filter(LEAF_BLOOM_BITS, false);
    block_options.set_whole_key_filtering(false);
    options.set_block_based_table_factory(&block_options);
    options
}

/// The history column family's options. Rows are keyed by write order, so each flush is
/// disjoint from everything on disk and leveled compaction moves files down without
/// rewriting them: write amplification near one, and no compaction competing with the
/// leaves. (Keyed by `key ‖ version`, as in v5, history was rewritten ten times over under
/// leveled compaction and filled the disk with in-flight merges under universal; HASHCHAINS.md,
/// Cost.) It gets its own small block cache so its reads and flushes cannot evict leaf blocks.
fn history_options(base: &Options) -> Options {
    let mut options = base.clone();
    let mut block_options = BlockBasedOptions::default();
    block_options.set_block_cache(&Cache::new_lru_cache(HISTORY_BLOCK_CACHE_SIZE));
    options.set_block_based_table_factory(&block_options);
    options
}

fn put_metadata(batch: &mut WriteBatch, cf: &ColumnFamily, leaf_count: u64, frontier_depth: u16) {
    batch.put_cf(cf, LEAF_COUNT_KEY, leaf_count.to_be_bytes());
    batch.put_cf(cf, COMPLETE_DEPTH_KEY, frontier_depth.to_be_bytes());
    batch.put_cf(cf, FORMAT_KEY, FORMAT_VERSION.to_be_bytes());
}

#[derive(Default)]
pub struct RocksWriteBatch {
    batch: WriteBatch,
    /// Rows for the frontier column family, appended to `batch` at commit.
    frontier: Vec<(Prefix, Digest, Digest)>,
    /// Rows for the history column family, appended to `batch` at commit.
    history: Vec<Version>,
    /// Census tallies, flushed by [`RocksStorage::write_batch`].
    bytes_staged: u64,
    leaf_puts: u64,
}

impl RocksWriteBatch {
    /// The current leaf row under `leaf.key`.
    pub fn put_leaf(&mut self, leaf: &LeafRow) {
        let db_key = leaf_key(&leaf.key);
        self.bytes_staged += (db_key.len() + LEAF_RECORD_LEN) as u64;
        self.leaf_puts += 1;
        self.batch.put(db_key, encode_leaf(leaf));
    }

    /// One version of a key, at its sequence number in the history column family.
    pub fn put_history(&mut self, version: &Version) {
        debug_assert_ne!(version.seq, NO_SEQ, "sequence numbers start at 1");
        self.bytes_staged += (HISTORY_KEY_LEN + HISTORY_RECORD_LEN) as u64;
        self.history.push(*version);
    }

    pub fn put_frontier_node(&mut self, prefix: &Prefix, left_hash: Digest, right_hash: Digest) {
        self.bytes_staged += (NODE_KEY_LEN + FRONTIER_RECORD_LEN) as u64;
        self.frontier.push((*prefix, left_hash, right_hash));
    }
}

#[cfg(test)]
mod tests;

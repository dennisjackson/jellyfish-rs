//! The RocksDB layer: one key space ([`codec`]), bounded scans, the write batch. Options are
//! hardcoded; the sweeps behind each are in DESIGN.md §7.

use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use rocksdb::statistics::{StatsLevel, Ticker};
use rocksdb::{
    BlockBasedOptions, Cache, DB, Direction, IteratorMode, Options, ReadOptions, WriteBatch,
};

use crate::census::{Census, CensusSnapshot, Metric};
use crate::{Digest, Entry, Key, Prefix, Value};

mod codec;
pub(crate) use codec::*;

pub type RocksResult<T> = Result<T, RocksStorageError>;

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

/// Leaf rows per data block: the granularity every scan is charged at (DESIGN.md §2).
pub(crate) const LEAVES_PER_BLOCK: u64 = (BLOCK_SIZE / (NODE_KEY_LEN + LEAF_RECORD_LEN)) as u64;

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
        options.set_max_open_files(MAX_OPEN_FILES);
        options.set_write_buffer_size(WRITE_BUFFER_SIZE);
        options.set_enable_pipelined_write(true);
        // A block cache, not a row cache: the hot reads are iterators.
        let mut block_options = BlockBasedOptions::default();
        block_options.set_block_cache(&Cache::new_lru_cache(BLOCK_CACHE_SIZE));
        options.set_block_based_table_factory(&block_options);
        options.set_allow_concurrent_memtable_write(true);
        options.set_manual_wal_flush(true);
        options.set_max_subcompactions(MAX_SUBCOMPACTIONS);
        options.set_max_background_jobs(MAX_BACKGROUND_JOBS);
        // The cheapest level that still counts the tickers the census samples.
        options.enable_statistics();
        options.set_statistics_level(StatsLevel::ExceptHistogramOrTimers);
        let db = DB::open(&options, path)?;
        let storage = Self {
            db,
            options,
            census: Census::new(),
            ticker_base: Default::default(),
        };
        storage.census_reset();
        Ok(storage)
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

    /// Collapse the LSM to one sorted run; blocks until done (DESIGN.md §10).
    pub fn compact_all(&self) {
        self.db.compact_range::<&[u8], &[u8]>(None, None);
    }

    /// Leaf count and frontier depth in one `WriteBatch`. Not counted as a node batch.
    pub fn commit_metadata(&self, leaf_count: u64, frontier_depth: u16) -> RocksResult<()> {
        let mut batch = WriteBatch::default();
        put_metadata(&mut batch, leaf_count, frontier_depth);
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
        put_metadata(&mut batch.batch, leaf_count, frontier_depth);
        self.write_batch(batch)
    }

    /// `(leaf_count, frontier_depth)`, or `None` if no batch ever committed. The two commit
    /// together, so one without the other is refused.
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
        let count = fixed::<8>(self.db.get_pinned(LEAF_COUNT_KEY)?, "the leaf count")?;
        let depth = fixed::<2>(
            self.db.get_pinned(COMPLETE_DEPTH_KEY)?,
            "the frontier depth",
        )?;
        match (count, depth) {
            (None, None) => Ok(None),
            (Some(count), Some(depth)) => {
                Ok(Some((u64::from_be_bytes(count), u16::from_be_bytes(depth))))
            }
            _ => Err(RocksStorageError::corrupt(
                "the leaf count and the frontier depth commit together, but only one is \
                 present: the metadata is torn. Rebuild the database",
            )),
        }
    }

    /// L0 files plus one per populated deeper level.
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

    pub fn get_leaf_entries_by_prefix(&self, prefix: &Prefix) -> RocksResult<Vec<Entry>> {
        let mut results = Vec::new();
        self.for_each_leaf_record(prefix, |key, value| {
            results.push((key, leaf_value_from_record(value)?));
            Ok(())
        })?;
        Ok(results)
    }

    /// Any row at a length in `1..=255`; one bounded seek. Length 0 is excluded: old builds
    /// persisted a root row.
    pub fn has_interior_rows(&self) -> RocksResult<bool> {
        let start_key = length_bound(1);
        let mut read_opts = ReadOptions::default();
        read_opts.set_iterate_upper_bound(length_bound(256).to_vec());
        let mode = IteratorMode::From(&start_key, Direction::Forward);
        match self.db.iterator_opt(mode, read_opts).next() {
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

    /// The leaf scan. The upper bound is `256 ‖ successor` (or `257`), so every key returned
    /// is a leaf key under `prefix` and bytes `2..34` are the leaf's key. A key of another
    /// length in the range is corrupt: skipping it would silently hash a prefix of the leaves.
    fn for_each_leaf_record<F>(&self, prefix: &Prefix, mut f: F) -> RocksResult<()>
    where
        F: FnMut(Key, &[u8]) -> RocksResult<()>,
    {
        let (start_key, end_key) = leaf_scan_range(prefix);

        let mut read_opts = ReadOptions::default();
        read_opts.set_iterate_upper_bound(end_key);

        let mode = IteratorMode::From(start_key.as_slice(), Direction::Forward);

        for entry in self.db.iterator_opt(mode, read_opts) {
            let (key, value) = entry?;
            if key.len() != NODE_KEY_LEN {
                return Err(RocksStorageError::corrupt(format!(
                    "a {}-byte key sits inside the leaf range; node keys are {NODE_KEY_LEN} \
                     bytes and nothing this tree writes puts another key there",
                    key.len()
                )));
            }
            debug_assert_eq!(
                &key[..2],
                256u16.to_be_bytes(),
                "the bound admits only leaf keys"
            );
            let leaf = Key(key[2..].try_into().expect("34-byte key"));
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

        for entry in self.db.iterator_opt(mode, read_opts) {
            let (key, value) = entry?;
            debug_assert_eq!(key.len(), NODE_KEY_LEN, "the bound admits only node keys");
            let prefix = decode_prefix(key.as_ref())?;
            debug_assert_eq!(prefix.length(), depth, "the bound admits only this length");
            let (left_hash, right_hash) = frontier_child_hashes(value.as_ref())?;
            f(prefix, left_hash, right_hash)?;
        }
        Ok(())
    }

    pub fn get_leaf_value(&self, key: &Key) -> RocksResult<Option<Value>> {
        match self.db.get_pinned(leaf_key(key))? {
            Some(raw) => Ok(Some(leaf_value_from_record(raw.as_ref())?)),
            None => Ok(None),
        }
    }

    pub fn flush(&self) -> RocksResult<()> {
        self.db.flush_wal(true)?;
        Ok(())
    }

    pub fn write_batch(&self, batch: RocksWriteBatch) -> RocksResult<()> {
        // Tallies land at commit, not per record (REVIEW.md §19).
        self.census[Metric::BytesStaged].add(batch.bytes_staged);
        self.census[Metric::LeafPuts].add(batch.leaf_puts);
        self.census[Metric::InteriorPuts].add(batch.interior_puts);
        self.census[Metric::BatchesCommitted].bump();
        self.db.write(batch.batch)?;
        Ok(())
    }
}

fn put_metadata(batch: &mut WriteBatch, leaf_count: u64, frontier_depth: u16) {
    batch.put(LEAF_COUNT_KEY, leaf_count.to_be_bytes());
    batch.put(COMPLETE_DEPTH_KEY, frontier_depth.to_be_bytes());
}

#[derive(Default)]
pub struct RocksWriteBatch {
    batch: WriteBatch,
    /// Census tallies, flushed by [`RocksStorage::write_batch`].
    bytes_staged: u64,
    leaf_puts: u64,
    interior_puts: u64,
}

impl RocksWriteBatch {
    pub fn put_leaf(&mut self, key: &Key, value: &Value) {
        let db_key = leaf_key(key);
        self.bytes_staged += (db_key.len() + LEAF_RECORD_LEN) as u64;
        self.leaf_puts += 1;
        self.batch.put(db_key, value.as_bytes());
    }

    pub fn put_frontier_node(&mut self, prefix: &Prefix, left_hash: Digest, right_hash: Digest) {
        let key = prefix_key(prefix);
        let value = encode_frontier_node(left_hash, right_hash);
        self.bytes_staged += (key.len() + value.len()) as u64;
        self.interior_puts += 1;
        self.batch.put(key, value);
    }
}

#[cfg(test)]
mod tests;

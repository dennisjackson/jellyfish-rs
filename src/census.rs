use std::ops::Index;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Counter(AtomicU64);

impl Counter {
    const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    #[inline(always)]
    pub fn bump(&self) {
        self.add(1);
    }

    #[inline(always)]
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    /// For the sampled metrics: the counter caches the last sample.
    pub fn set(&self, n: u64) {
        self.0.store(n, Ordering::Relaxed);
    }

    fn zero(&self) {
        self.0.store(0, Ordering::Relaxed);
    }
}

/// What the census counts (DESIGN.md §12). The last three are sampled from RocksDB's tickers
/// at snapshot time, so a phase must be opened through `RocksStorage::census_reset`, which
/// re-baselines them; [`Census::reset`] alone is not enough.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metric {
    /// Leaf records staged.
    LeafPuts,
    /// Frontier rows staged.
    InteriorPuts,
    /// Key + value bytes staged.
    BytesStaged,
    /// `db.write` calls for node batches.
    BatchesCommitted,
    /// Subtree scans that found a leaf.
    SubtreeLoads,
    /// Leaves those scans read. Structural: falls as the tree top deepens long after the
    /// block cost has stopped falling; not the read headline.
    LeavesReadByLoads,
    /// Data blocks fetched from an SST (block-cache data miss). The read headline: a scan
    /// costs about one block per sorted run whatever range it covers (DESIGN.md §2).
    DataBlocksRead,
    /// Index and filter blocks fetched from an SST.
    IndexBlocksRead,
    /// Iterator seeks, about one per sorted run per scan.
    Seeks,
}

impl Metric {
    pub const ALL: [Metric; 9] = [
        Metric::LeafPuts,
        Metric::InteriorPuts,
        Metric::BytesStaged,
        Metric::BatchesCommitted,
        Metric::SubtreeLoads,
        Metric::LeavesReadByLoads,
        Metric::DataBlocksRead,
        Metric::IndexBlocksRead,
        Metric::Seeks,
    ];

    const COUNT: usize = Metric::ALL.len();

    const fn slot(self) -> usize {
        self as usize
    }
}

/// Counters for one storage instance.
pub struct Census {
    counters: [Counter; Metric::COUNT],
}

impl Census {
    pub const fn new() -> Self {
        Self {
            counters: [const { Counter::new() }; Metric::COUNT],
        }
    }

    pub fn snapshot(&self) -> CensusSnapshot {
        let mut values = [0u64; Metric::COUNT];
        for metric in Metric::ALL {
            values[metric.slot()] = self[metric].get();
        }
        CensusSnapshot { values }
    }

    pub fn reset(&self) {
        for counter in &self.counters {
            counter.zero();
        }
    }
}

impl Default for Census {
    fn default() -> Self {
        Self::new()
    }
}

impl Index<Metric> for Census {
    type Output = Counter;

    fn index(&self, metric: Metric) -> &Counter {
        &self.counters[metric.slot()]
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CensusSnapshot {
    values: [u64; Metric::COUNT],
}

impl CensusSnapshot {
    /// Field-wise saturating `self - earlier`.
    pub fn since(&self, earlier: &CensusSnapshot) -> CensusSnapshot {
        let mut values = [0u64; Metric::COUNT];
        for metric in Metric::ALL {
            values[metric.slot()] = self[metric].saturating_sub(earlier[metric]);
        }
        CensusSnapshot { values }
    }

    /// Leaf plus frontier puts: "puts per insert".
    pub fn total_puts(&self) -> u64 {
        self[Metric::LeafPuts] + self[Metric::InteriorPuts]
    }
}

impl Index<Metric> for CensusSnapshot {
    type Output = u64;

    fn index(&self, metric: Metric) -> &u64 {
        &self.values[metric.slot()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slots match `Metric::ALL` order, a snapshot diff reports one phase, a reset clears all.
    #[test]
    fn counters_are_distinct_and_snapshots_diff_and_reset() {
        let census = Census::new();
        for (position, metric) in Metric::ALL.into_iter().enumerate() {
            assert_eq!(metric.slot(), position, "{metric:?} is out of order");
            census[metric].add(position as u64 + 1);
        }
        let first = census.snapshot();
        for (position, metric) in Metric::ALL.into_iter().enumerate() {
            assert_eq!(first[metric], position as u64 + 1);
        }
        assert_eq!(first.total_puts(), 1 + 2, "leaf puts plus interior puts");

        census[Metric::LeafPuts].bump();
        census[Metric::LeavesReadByLoads].add(40);
        let phase = census.snapshot().since(&first);
        assert_eq!(
            phase[Metric::LeafPuts],
            1,
            "the diff excludes the first phase"
        );
        assert_eq!(phase[Metric::LeavesReadByLoads], 40);
        assert_eq!(phase[Metric::InteriorPuts], 0);
        assert_eq!(
            first.since(&census.snapshot()),
            CensusSnapshot::default(),
            "saturating"
        );

        census.reset();
        assert_eq!(census.snapshot(), CensusSnapshot::default());
    }
}

use crate::Entry;

/// Test-only root-hash oracle.
#[cfg(test)]
mod simple;
#[cfg(test)]
pub(crate) use simple::SimpleMPT;

// `pub`: `compact-db` and tests/full_recovery_memory.rs use the storage layer directly.
pub mod rocks_frontier;
pub use rocks_frontier::{RocksFrontierConfig, RocksFrontierMPT};

/// Sort by key and deduplicate, last value wins.
pub(crate) fn sorted_unique_entries(entries: &[Entry]) -> Vec<Entry> {
    let mut sorted = entries.to_vec();
    sorted.sort_by_key(|(k, _)| *k);
    sorted.dedup_by(|later, kept| {
        let same_key = later.0 == kept.0;
        if same_key {
            kept.1 = later.1;
        }
        same_key
    });
    sorted
}

#[cfg(test)]
mod tests;

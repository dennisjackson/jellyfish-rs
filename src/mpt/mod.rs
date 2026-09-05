use crate::Entry;

/// Test-only root-hash oracle.
#[cfg(test)]
mod simple;
#[cfg(test)]
pub(crate) use simple::SimpleMPT;

// `pub`: `compact-db` and tests/full_recovery_memory.rs use the storage layer directly.
pub mod rocks_frontier;
pub use rocks_frontier::{RocksFrontierConfig, RocksFrontierMPT};

/// Sort by key, keeping every occurrence of a key in slice order: each one is a link of the
/// key's chain (HASHCHAINS.md). `sort_by_key` is stable.
pub(crate) fn sorted_entries(entries: &[Entry]) -> Vec<Entry> {
    let mut sorted = entries.to_vec();
    sorted.sort_by_key(|(k, _)| *k);
    sorted
}

#[cfg(test)]
mod tests;

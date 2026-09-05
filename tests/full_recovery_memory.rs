//! The open reads exactly the frontier level (or counts the leaves), decodes nothing else,
//! and sizes the tree top by the tree, not the budget. Own binary: the counting allocator
//! must not perturb the unit tests.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use jellyfish_rs::mpt::rocks_frontier::storage::{RocksStorage, RocksWriteBatch, Version};
use jellyfish_rs::{Digest, Entry, Key, Prefix, RocksFrontierConfig, RocksFrontierMPT, Value};

mod common;

/// Offset so frees of memory allocated before a measurement cannot underflow.
const BASE: usize = 1 << 40;

thread_local! {
    static LIVE: Cell<usize> = const { Cell::new(BASE) };
    static PEAK: Cell<usize> = const { Cell::new(BASE) };
}

fn account(delta: isize) {
    let _ = LIVE.try_with(|live| {
        let now = live.get().wrapping_add_signed(delta);
        live.set(now);
        let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
    });
}

struct Probe;

unsafe impl GlobalAlloc for Probe {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            account(layout.size() as isize);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        account(-(layout.size() as isize));
        unsafe { System.dealloc(ptr, layout) };
    }
}

#[global_allocator]
static PROBE: Probe = Probe;

/// `f`'s result and the peak growth of this thread's live allocations while it ran.
fn peak_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    LIVE.with(|live| live.set(BASE));
    PEAK.with(|peak| peak.set(BASE));
    let out = f();
    (out, PEAK.with(|peak| peak.get()) - BASE)
}

struct Lcg(u64);

impl Lcg {
    fn next_bytes(&mut self) -> [u8; 32] {
        let mut out = [0u8; 32];
        for chunk in out.chunks_mut(8) {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            chunk.copy_from_slice(&self.0.to_be_bytes());
        }
        out
    }
}

/// Decoding the 200K planted rows below would be ~27 MB; a bounded open is kilobytes. The
/// trees below open without the scan floor, whose 26 levels are a 4.4 GB reservation by
/// policy (lazily resident) rather than a read of anything.
const PEAK_LIMIT: usize = 8 * 1024 * 1024;

#[test]
fn opening_reads_only_the_frontier_level() {
    // A small tree, then 200K frontier-shaped rows at a depth nothing reads.
    let path = common::fresh_dir().join("recovery.db");
    let mut rng = Lcg(0x5eed);
    let entries: Vec<Entry> = (0..60)
        .map(|_| (Key(rng.next_bytes()), Value(rng.next_bytes())))
        .collect();
    let root = {
        let mut tree =
            RocksFrontierMPT::open(&path, RocksFrontierConfig::default().with_scan_floor(0))
                .expect("create tree");
        tree.batch_upsert(&entries);
        assert!(
            tree.frontier_depth() >= 1,
            "the open must take the level-scan path"
        );
        tree.get_root_hash()
    };
    {
        let storage = RocksStorage::open(&path).expect("open storage");
        let mut junk = RocksWriteBatch::default();
        for _ in 0..200_000 {
            let prefix = Prefix::new(Key(rng.next_bytes()).zero_bits_from(100), 100);
            junk.put_frontier_node(&prefix, Digest(rng.next_bytes()), Digest(rng.next_bytes()));
        }
        storage.write_batch(junk).expect("plant junk rows");
        storage.flush().expect("flush wal");
    }

    let (tree, peak) = peak_during(|| {
        RocksFrontierMPT::open(&path, RocksFrontierConfig::default().with_scan_floor(0))
            .expect("reopen tree")
    });
    assert_eq!(
        tree.get_root_hash(),
        root,
        "the reopen rebuilt a different tree"
    );
    assert!(
        peak < PEAK_LIMIT,
        "the open peak-allocated {peak} bytes: it decoded rows it never reads"
    );
}

#[test]
fn opening_a_frontierless_database_allocates_next_to_nothing() {
    // One leaf, no metadata: no buffer may be reserved up front.
    let path = common::fresh_dir().join("one_leaf.db");
    let mut rng = Lcg(0xc0ffee);
    let (key, value) = (Key(rng.next_bytes()), Value(rng.next_bytes()));
    {
        let storage = RocksStorage::open(&path).expect("open storage");
        let mut batch = RocksWriteBatch::default();
        let version = Version::first(key, value, 1);
        batch.put_leaf(&version.leaf_row());
        batch.put_history(&version);
        storage.write_batch(batch).expect("write leaf");
        storage.flush().expect("flush wal");
    }

    let (tree, peak) = peak_during(|| {
        RocksFrontierMPT::open(&path, RocksFrontierConfig::default().with_scan_floor(0))
            .expect("recover tree")
    });
    assert_eq!(tree.get_leaf_value(key), Some(value));
    assert_eq!(tree.get_history(key), [jellyfish_rs::Record::first(value)]);
    assert!(
        peak < PEAK_LIMIT,
        "the open peak-allocated {peak} bytes: a buffer is reserved up front"
    );
}

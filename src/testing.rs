//! Test support from std alone: a scratch directory and a seeded generator.

use std::cell::Cell;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::{fs, thread};

/// A fresh, empty `target/test-dbs/<test name>/<n>`. Wiped on creation, left afterwards so a
/// failing test's database can be inspected.
pub(crate) fn fresh_dir() -> PathBuf {
    thread_local! {
        static CALLS: Cell<u32> = const { Cell::new(0) };
    }
    let test = thread::current()
        .name()
        .unwrap_or("unnamed")
        .replace("::", "/");
    let call = CALLS.with(|calls| calls.replace(calls.get() + 1));
    let path = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/target/test-dbs"))
        .join(test)
        .join(call.to_string());
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("create test directory");
    path
}

/// `DefaultHasher` in counter mode. The stream may change with the toolchain; tests compare
/// against the oracle, never a recorded value.
pub(crate) struct TestRng {
    seed: u64,
    drawn: u64,
}

impl TestRng {
    pub(crate) const fn seed(seed: u64) -> Self {
        Self { seed, drawn: 0 }
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        let mut hasher = DefaultHasher::new();
        (self.seed, self.drawn).hash(&mut hasher);
        self.drawn += 1;
        hasher.finish()
    }

    pub(crate) fn bytes(&mut self) -> [u8; 32] {
        let mut out = [0u8; 32];
        for chunk in out.as_chunks_mut::<8>().0 {
            *chunk = self.next_u64().to_le_bytes();
        }
        out
    }

    /// Uniform over `0..n`.
    pub(crate) fn below(&mut self, n: usize) -> usize {
        assert!(n > 0, "an empty range has nothing to draw");
        ((u128::from(self.next_u64()) * n as u128) >> 64) as usize
    }
}

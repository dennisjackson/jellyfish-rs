//! The tree top: one dense array per depth, addressed by `(depth, index)` (DESIGN.md, Representation).
//!
//! Above the frontier the trie is perfect, so position determines prefix and only the hash
//! is stored. Below it a positional prefix covers the same leaf range as the compressed
//! prefix beneath it, so the hash under a position is still well defined; a state byte adds
//! unknown / proven empty.
//!
//! Sibling subtrees own disjoint index ranges and the descent below the frontier is
//! sequential, so no two workers touch one slot and `rayon::join` orders cross-thread reads:
//! hash words are `Relaxed`. The state byte is swapped `Release` after the hash and read
//! `Acquire`; the swap hands the old state to one writer, so the per-depth hashed count moves
//! once per transition even when two tasks set the same slot.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use crate::prefix::Side;
use crate::{Digest, Key, Prefix};

/// A 32-byte hash as four big-endian `AtomicU64` words.
#[derive(Default)]
struct AtomicHash([AtomicU64; 4]);

impl AtomicHash {
    fn load(&self) -> Digest {
        let mut hash = [0u8; 32];
        for (word, slot) in self.0.iter().enumerate() {
            hash[word * 8..(word + 1) * 8]
                .copy_from_slice(&slot.load(Ordering::Relaxed).to_be_bytes());
        }
        Digest(hash)
    }

    fn store(&self, hash: Digest) {
        for (word, slot) in self.0.iter().enumerate() {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&hash.as_bytes()[word * 8..(word + 1) * 8]);
            slot.store(u64::from_be_bytes(bytes), Ordering::Relaxed);
        }
    }
}

/// What is known about the leaves under a position. `Empty` is *proven* emptiness —
/// `upsert_child` builds from the batch alone on it — so it must stay distinct from unknown
/// ([`Levels::get`] returns `None`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Slot {
    Empty,
    /// Hash of the compressed subtree under the position; below the frontier its root may
    /// sit deeper and the hash passes through.
    Hash(Digest),
}

pub(super) struct Levels {
    /// `levels[d]` holds `2^d` positions.
    levels: Vec<Level>,
}

struct Level {
    hashes: Box<[AtomicHash]>,
    /// Written `Release` after the hash, read `Acquire`. Zero (unknown) as allocated.
    states: Box<[AtomicU8]>,
    /// Positions in state `Hash`: the frontier-advance gate is one load.
    hashed: AtomicU64,
}

const STATE_UNKNOWN: u8 = 0;
const STATE_EMPTY: u8 = 1;
const STATE_HASH: u8 = 2;

/// Address space reserved for depths `0..=deepest`.
pub(super) fn levels_bytes(deepest: u16) -> u64 {
    let positions = (1u64 << (u32::from(deepest) + 1)) - 1;
    positions * (std::mem::size_of::<AtomicHash>() + std::mem::size_of::<AtomicU8>()) as u64
}

impl Levels {
    pub(super) fn new(deepest: u16) -> Self {
        let mut levels = Self { levels: Vec::new() };
        levels.ensure_depth(deepest);
        levels
    }

    pub(super) fn deepest(&self) -> u16 {
        self.levels.len() as u16 - 1
    }

    /// Never shrinks.
    pub(super) fn ensure_depth(&mut self, deepest: u16) {
        while self.levels.len() <= usize::from(deepest) {
            let depth = self.levels.len();
            self.levels.push(Level {
                hashes: zeroed_slice(1 << depth),
                states: zeroed_slice(1 << depth),
                hashed: AtomicU64::new(0),
            });
        }
    }

    fn level(&self, depth: u16) -> Option<&Level> {
        self.levels.get(usize::from(depth))
    }

    /// `None` if nothing has been recorded since open, or the depth is not covered.
    pub(super) fn get(&self, position: Position) -> Option<Slot> {
        let level = self.level(position.depth)?;
        let index = position.index as usize;
        match level.states[index].load(Ordering::Acquire) {
            STATE_UNKNOWN => None,
            STATE_EMPTY => Some(Slot::Empty),
            STATE_HASH => Some(Slot::Hash(level.hashes[index].load())),
            state => unreachable!("state byte {state} at {position:?}"),
        }
    }

    /// The hash at a position that must hold one.
    pub(super) fn hash(&self, position: Position) -> Digest {
        match self.get(position) {
            Some(Slot::Hash(hash)) => hash,
            other => panic!("{position:?} holds {other:?}, but a hash was required"),
        }
    }

    /// Both children, if both are known.
    pub(super) fn children(&self, position: Position) -> Option<(Slot, Slot)> {
        Some((
            self.get(position.child(Side::Left))?,
            self.get(position.child(Side::Right))?,
        ))
    }

    /// A write below the covered depth is dropped: unknown costs a scan, never a wrong hash.
    pub(super) fn set(&self, position: Position, slot: Slot) {
        let Some(level) = self.level(position.depth) else {
            return;
        };
        let index = position.index as usize;
        let state = match slot {
            Slot::Empty => STATE_EMPTY,
            Slot::Hash(hash) => {
                level.hashes[index].store(hash);
                STATE_HASH
            }
        };
        let was_hashed = level.states[index].swap(state, Ordering::Release) == STATE_HASH;

        match (was_hashed, state == STATE_HASH) {
            (false, true) => level.hashed.fetch_add(1, Ordering::Relaxed),
            (true, false) => level.hashed.fetch_sub(1, Ordering::Relaxed),
            _ => 0,
        };
    }

    /// Every position at `depth` holds a hash, i.e. depth `depth - 1` is complete.
    pub(super) fn all_hashed(&self, depth: u16) -> bool {
        self.hashed_count(depth) == 1u64 << depth
    }

    pub(super) fn hashed_count(&self, depth: u16) -> u64 {
        self.level(depth)
            .map_or(0, |level| level.hashed.load(Ordering::Relaxed))
    }

    #[cfg(test)]
    pub(super) fn slots(&self) -> impl Iterator<Item = (Position, Slot)> + '_ {
        (0..=self.deepest()).flat_map(move |depth| {
            (0..1u64 << depth).filter_map(move |index| {
                let position = Position::new(depth, index);
                self.get(position).map(|slot| (position, slot))
            })
        })
    }
}

/// All-default slice without `unsafe`. Out of line so the release build folds it into the
/// allocator's zeroed path and untouched pages stay non-resident.
#[inline(never)]
fn zeroed_slice<T: Default>(len: usize) -> Box<[T]> {
    let mut slice = Vec::with_capacity(len);
    slice.resize_with(len, T::default);
    slice.into_boxed_slice()
}

/// A node's address: `depth`, and `index` = the first `depth` bits of every key beneath it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Position {
    pub(super) depth: u16,
    pub(super) index: u64,
}

impl Position {
    pub(super) const ROOT: Position = Position::new(0, 0);

    pub(super) const fn new(depth: u16, index: u64) -> Self {
        Self { depth, index }
    }

    pub(super) const fn child(self, side: Side) -> Self {
        let index = match side {
            Side::Left => self.index * 2,
            Side::Right => self.index * 2 + 1,
        };
        Self::new(self.depth + 1, index)
    }

    /// The index at `depth` of the position above `key`. `depth <= 64`; the tree top never
    /// passes 28.
    pub(super) fn index_of(key: &Key, depth: u16) -> u64 {
        debug_assert!(depth <= 64, "depth {depth} has no positional index");
        let head: [u8; 8] = key.as_bytes()[..8].try_into().expect("keys hold 32 bytes");
        u64::from_be_bytes(head).unbounded_shr(64 - u32::from(depth))
    }

    pub(super) fn prefix(self) -> Prefix {
        debug_assert!(
            self.depth <= 64,
            "depth {} has no positional prefix",
            self.depth
        );
        // An index past the level would shift out and alias a low position.
        debug_assert!(
            self.depth >= 64 || self.index < 1u64 << self.depth,
            "index {} lies past the {} positions at depth {}",
            self.index,
            1u64 << self.depth,
            self.depth
        );
        let mut key = [0u8; 32];
        let head = self.index.unbounded_shl(64 - u32::from(self.depth));
        key[..8].copy_from_slice(&head.to_be_bytes());
        Prefix::new(Key(key), self.depth)
    }

    /// Inverse of [`Self::prefix`].
    pub(super) fn of(prefix: &Prefix) -> Self {
        Self::new(
            prefix.length(),
            Self::index_of(&prefix.key(), prefix.length()),
        )
    }
}

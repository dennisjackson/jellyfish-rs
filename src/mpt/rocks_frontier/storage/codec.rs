//! On-disk format v6 (DESIGN.md, Representation; HASHCHAINS.md, Database Schema). The only
//! module that knows the key layouts: callers ask for a key or a range. Records are untagged;
//! the column family and, for nodes, the key's length say the kind.

use super::{LeafRow, RocksResult, RocksStorageError};
use crate::{Digest, Key, Prefix, Record, Value};

/// The metadata keys start with `_` (0x5F), above `length_bound(257)`: outside every scan.
pub(super) const COMPLETE_DEPTH_KEY: &[u8] = b"__mpt_complete_depth__";
pub(super) const LEAF_COUNT_KEY: &[u8] = b"__mpt_leaf_count__";
/// Absent from v4 databases; 5 named the format whose leaves carried their records.
pub(super) const FORMAT_KEY: &[u8] = b"__mpt_format__";
pub(crate) const FORMAT_VERSION: u16 = 6;

/// Every version of every key, keyed by the order it was written (HASHCHAINS.md).
pub(super) const HISTORY_CF: &str = "history";
/// The persisted frontier level, apart from the leaves it would otherwise be compacted with.
pub(super) const FRONTIER_CF: &str = "frontier";

/// `length_be_u16 || key`.
pub(crate) const NODE_KEY_LEN: usize = 34;

/// Leaf keys share their first `LEAF_PREFIX_LEN` bytes (the length and 24 bits of key) with
/// every leaf under any prefix of at least [`LEAF_PREFIX_BITS`] bits, so a scan under such a
/// prefix can ask each sorted run's prefix bloom filter before reading a block from it.
pub(crate) const LEAF_PREFIX_LEN: usize = 5;
pub(crate) const LEAF_PREFIX_BITS: u16 = ((LEAF_PREFIX_LEN - 2) * 8) as u16;

/// A leaf record is `leaf_hash || head_seq || version`: the Merkle hash of the key's current
/// record, the history row holding that record, and how many records preceded it.
pub(crate) const LEAF_RECORD_LEN: usize = 48;

/// A frontier record is `left_hash || right_hash`.
pub(crate) const FRONTIER_RECORD_LEN: usize = 64;

/// A history key is the row's sequence number, big-endian, so rows land in write order.
pub(crate) const HISTORY_KEY_LEN: usize = 8;

/// A history record is `value || link || prev_seq`: a [`Record`] and the row it replaced.
pub(crate) const HISTORY_RECORD_LEN: usize = 72;

/// The `prev_seq` of a key's first record. Sequence numbers start at 1.
pub(crate) const NO_SEQ: u64 = 0;

/// The two-byte key every node key at `length` sorts at or after; `length + 1` is the
/// exclusive bound of a level, `257` of all node keys.
pub(super) fn length_bound(length: u16) -> [u8; 2] {
    length.to_be_bytes()
}

pub(super) fn prefix_key(prefix: &Prefix) -> Vec<u8> {
    let mut key = Vec::with_capacity(NODE_KEY_LEN);
    key.extend_from_slice(&prefix.length().to_be_bytes());
    key.extend_from_slice(prefix.key().as_bytes());
    key
}

pub(super) fn leaf_key(key: &Key) -> Vec<u8> {
    prefix_key(&Prefix::from(*key))
}

/// `[start, end)` over every leaf under `prefix`. The root and all-ones prefixes have no
/// successor and run to the end of the node keys.
pub(super) fn leaf_scan_range(prefix: &Prefix) -> (Vec<u8>, Vec<u8>) {
    let start = leaf_key(&prefix.key());
    let end = match prefix.successor() {
        Some(key) => leaf_key(&key),
        None => length_bound(257).to_vec(),
    };
    (start, end)
}

pub(super) fn history_key(seq: u64) -> [u8; HISTORY_KEY_LEN] {
    seq.to_be_bytes()
}

pub(super) fn decode_history_key(bytes: &[u8]) -> RocksResult<u64> {
    let key: [u8; HISTORY_KEY_LEN] = bytes.try_into().map_err(|_| {
        RocksStorageError::corrupt(format!(
            "a history key must be {HISTORY_KEY_LEN} bytes, got {}",
            bytes.len()
        ))
    })?;
    Ok(u64::from_be_bytes(key))
}

/// The one node key decoder, so the one place `Prefix`'s invariants are established for disk
/// data.
pub(super) fn decode_prefix(bytes: &[u8]) -> RocksResult<Prefix> {
    if bytes.len() != NODE_KEY_LEN {
        return Err(RocksStorageError::corrupt(format!(
            "Prefix bytes must be {NODE_KEY_LEN} long, got {}",
            bytes.len()
        )));
    }
    let length = u16::from_be_bytes([bytes[0], bytes[1]]);
    if length > 256 {
        return Err(RocksStorageError::corrupt(format!(
            "node key names prefix length {length}, above the 256 bits a key holds"
        )));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes[2..NODE_KEY_LEN]);
    let key = Key(key);
    if key != key.zero_bits_from(length) {
        return Err(RocksStorageError::corrupt(format!(
            "node key at length {length} carries a set bit past its length ({})",
            key.short_hex()
        )));
    }
    Ok(Prefix::new(key, length))
}

fn record<const N: usize>(bytes: &[u8], what: &str) -> RocksResult<[u8; N]> {
    bytes.try_into().map_err(|_| {
        let legacy = match bytes.len() {
            32 => {
                " A 32-byte leaf record is format v4: open it with a build at commit 7f87873, \
                 or rebuild."
            }
            33 | 65 => {
                " A tagged 33- or 65-byte record is format v3: open it with a build at commit \
                 acff956, or rebuild."
            }
            72 if N == LEAF_RECORD_LEN => {
                " A 72-byte leaf record is format v5, which kept the record in the leaf; \
                 rebuild."
            }
            _ => "",
        };
        RocksStorageError::corrupt(format!(
            "a {what} record must be {N} bytes, got {}.{legacy}",
            bytes.len()
        ))
    })
}

fn digest(bytes: &[u8]) -> Digest {
    Digest(bytes.try_into().expect("32 bytes"))
}

fn u64_at(bytes: &[u8]) -> u64 {
    u64::from_be_bytes(bytes.try_into().expect("8 bytes"))
}

pub(super) fn encode_frontier_node(left_hash: Digest, right_hash: Digest) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(FRONTIER_RECORD_LEN);
    encoded.extend_from_slice(left_hash.as_bytes());
    encoded.extend_from_slice(right_hash.as_bytes());
    encoded
}

pub(super) fn frontier_child_hashes(bytes: &[u8]) -> RocksResult<(Digest, Digest)> {
    let record = record::<FRONTIER_RECORD_LEN>(bytes, "frontier")?;
    let (left, right) = record.split_at(32);
    Ok((digest(left), digest(right)))
}

pub(super) fn encode_leaf(leaf: &LeafRow) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(LEAF_RECORD_LEN);
    encoded.extend_from_slice(leaf.hash.as_bytes());
    encoded.extend_from_slice(&leaf.head.to_be_bytes());
    encoded.extend_from_slice(&leaf.version.to_be_bytes());
    encoded
}

/// The leaf row under `key` from its record.
pub(super) fn leaf_from_record(key: Key, bytes: &[u8]) -> RocksResult<LeafRow> {
    let record = record::<LEAF_RECORD_LEN>(bytes, "leaf")?;
    let head = u64_at(&record[32..40]);
    if head == NO_SEQ {
        return Err(RocksStorageError::corrupt(format!(
            "the leaf under {} points at no history row",
            key.short_hex()
        )));
    }
    Ok(LeafRow {
        key,
        hash: digest(&record[..32]),
        head,
        version: u64_at(&record[40..48]),
    })
}

pub(super) fn encode_history_record(record: &Record, prev: u64) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(HISTORY_RECORD_LEN);
    encoded.extend_from_slice(record.value.as_bytes());
    encoded.extend_from_slice(record.link.as_bytes());
    encoded.extend_from_slice(&prev.to_be_bytes());
    encoded
}

/// `(record, prev_seq)` of a history row.
pub(super) fn history_record(bytes: &[u8]) -> RocksResult<(Record, u64)> {
    let record = record::<HISTORY_RECORD_LEN>(bytes, "history")?;
    Ok((
        Record {
            value: Value(record[..32].try_into().expect("32 bytes")),
            link: digest(&record[32..64]),
        },
        u64_at(&record[64..72]),
    ))
}

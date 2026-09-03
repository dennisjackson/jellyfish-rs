//! On-disk format v4 (DESIGN.md, Representation). The only module that knows the key layout: callers ask
//! for a key or a range. Records are untagged; the key's length says the kind.

use super::{RocksResult, RocksStorageError};
use crate::{Digest, Key, Prefix, Value};

/// Both metadata keys start with `_` (0x5F), above `length_bound(257)`: outside every scan.
pub(super) const COMPLETE_DEPTH_KEY: &[u8] = b"__mpt_complete_depth__";
pub(super) const LEAF_COUNT_KEY: &[u8] = b"__mpt_leaf_count__";

/// `length_be_u16 || key`.
pub(crate) const NODE_KEY_LEN: usize = 34;

/// A leaf record is its value.
pub(crate) const LEAF_RECORD_LEN: usize = 32;

/// A frontier record is `left_hash || right_hash`.
pub(crate) const FRONTIER_RECORD_LEN: usize = 64;

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

/// The one key decoder, so the one place `Prefix`'s invariants are established for disk data.
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
        RocksStorageError::corrupt(format!(
            "a {what} record must be {N} bytes, got {}. A record one byte longer is format v3: \
             open it with a build at commit acff956, or rebuild",
            bytes.len()
        ))
    })
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
    Ok((
        Digest(left.try_into().expect("32 bytes")),
        Digest(right.try_into().expect("32 bytes")),
    ))
}

pub(super) fn leaf_value_from_record(bytes: &[u8]) -> RocksResult<Value> {
    record::<LEAF_RECORD_LEN>(bytes, "leaf").map(Value)
}

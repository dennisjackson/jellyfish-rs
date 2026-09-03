use sha2::Digest as _;

use crate::{Digest, Key, Prefix, Value};

/// `H("leaf" || key || value)`.
pub fn leaf(key: Key, value: Value) -> Digest {
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"leaf");
    hasher.update(key);
    hasher.update(value);
    Digest(hasher.finalize().into())
}

/// `H("interior" || prefix || length || left || right)`.
pub fn interior(prefix: Prefix, left: Digest, right: Digest) -> Digest {
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"interior");
    hasher.update(prefix.key());
    hasher.update(prefix.length().to_be_bytes());
    hasher.update(left);
    hasher.update(right);
    Digest(hasher.finalize().into())
}

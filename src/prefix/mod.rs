use std::fmt;

/// 32 bytes by value, ordered lexicographically (trie order for a key).
macro_rules! bytes32 {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[repr(transparent)]
        pub struct $name(pub [u8; 32]);

        impl $name {
            #[inline]
            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }

            /// The first four bytes as hex.
            pub fn short_hex(&self) -> String {
                use std::fmt::Write;
                let mut s = String::with_capacity(8);
                for b in self.0.iter().take(4) {
                    write!(&mut s, "{b:02x}").expect("writing to a String cannot fail");
                }
                s
            }
        }

        impl From<[u8; 32]> for $name {
            fn from(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }
        }

        impl AsRef<[u8]> for $name {
            fn as_ref(&self) -> &[u8] {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.short_hex())
            }
        }
    };
}

bytes32! {
    /// A leaf's key: its path through the trie, MSB first.
    Key
}

bytes32! {
    /// A leaf's value; opaque.
    Value
}

bytes32! {
    /// A merkle hash.
    Digest
}

impl Key {
    pub const ZERO: Self = Self([0u8; 32]);

    /// Bit `position`, MSB first. Panics at or past 256.
    pub fn get_bit(&self, position: u16) -> bool {
        let byte_index = (position / 8) as usize;
        let bit_index = 7 - (position % 8);
        (self.0[byte_index] >> bit_index) & 1 != 0
    }

    /// Zero every bit from `start_bit` on.
    pub fn zero_bits_from(&self, start_bit: u16) -> Key {
        let mut result = self.0;
        if start_bit >= 256 {
            return Key(result);
        }
        let byte = (start_bit / 8) as usize;
        let keep = start_bit % 8;
        result[byte] &= !(0xFFu8 >> keep);
        result[byte + 1..].fill(0);
        Key(result)
    }
}

pub type Entry = (Key, Value);

/// Which child: 0 = left, 1 = right.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// A node's name: the first `length` bits of `key`, MSB first. Leaves have `length == 256`.
/// Invariants: `length <= 256` and every bit at or past `length` is zero, since `Eq`/`Ord`/
/// `Hash` and the interior hash read all 32 bytes (DESIGN.md §1). Debug-checked in
/// [`Prefix::new`]; enforced for disk rows by the storage decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Prefix {
    key: Key,
    length: u16,
}

impl From<Key> for Prefix {
    fn from(key: Key) -> Self {
        Prefix { key, length: 256 }
    }
}

impl Prefix {
    #[inline]
    pub fn new(key: Key, length: u16) -> Self {
        debug_assert!(length <= 256, "prefix length {length} exceeds 256");
        debug_assert_eq!(
            key,
            key.zero_bits_from(length),
            "bits at or past the prefix length must be zero"
        );
        Prefix { key, length }
    }

    #[inline]
    pub const fn key(&self) -> Key {
        self.key
    }

    #[inline]
    pub const fn length(&self) -> u16 {
        self.length
    }

    pub fn root() -> Self {
        Prefix {
            key: Key::ZERO,
            length: 0,
        }
    }

    /// The smallest key after every key under this prefix; `None` for the root or an
    /// all-ones prefix.
    pub fn successor(&self) -> Option<Key> {
        if self.length == 0 {
            return None;
        }
        let mut key = self.key.0;
        let mut bit = self.length as i32 - 1;
        while bit >= 0 {
            let byte_index = (bit / 8) as usize;
            let bit_index = 7 - (bit % 8);
            let mask = 1 << bit_index;
            if key[byte_index] & mask == 0 {
                key[byte_index] |= mask;
                return Some(Key(key));
            } else {
                key[byte_index] &= !mask;
                bit -= 1;
            }
        }
        None
    }

    pub fn short_hex(&self) -> String {
        match self.length {
            256 => self.key.short_hex(),
            0 => "empty".into(),
            length => (0..length)
                .map(|bit| if self.key.get_bit(bit) { '1' } else { '0' })
                .collect(),
        }
    }

    fn common_leading_bits(a: &Key, b: &Key) -> u16 {
        let (a_words, _) = a.0.as_chunks::<8>();
        let (b_words, _) = b.0.as_chunks::<8>();
        for (i, (x, y)) in a_words.iter().zip(b_words).enumerate() {
            let diff = u64::from_be_bytes(*x) ^ u64::from_be_bytes(*y);
            if diff != 0 {
                return (i * 64) as u16 + diff.leading_zeros() as u16;
            }
        }
        256
    }

    pub fn contains(&self, key: &Key) -> bool {
        Self::common_leading_bits(&self.key, key) >= self.length
    }

    pub fn common_prefix(a: &Prefix, b: &Prefix) -> Prefix {
        let min_length = a.length.min(b.length);
        let common_bits = Self::common_leading_bits(&a.key, &b.key).min(min_length);
        Prefix {
            key: a.key.zero_bits_from(common_bits),
            length: common_bits,
        }
    }

    /// `key`'s bit at this prefix's length.
    pub fn key_goes_right(&self, key: Key) -> bool {
        key.get_bit(self.length)
    }
}

#[cfg(test)]
mod tests;

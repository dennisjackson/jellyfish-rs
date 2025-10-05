pub type Hash = [u8; 32];

pub trait HashExt {
    fn short_hex(&self) -> String;
}

impl HashExt for Hash {
    fn short_hex(&self) -> String {
        use std::fmt::Write;
        let mut s = String::with_capacity(8);
        for b in self.iter().take(4) {
            // 4 bytes = 8 hex chars
            write!(&mut s, "{b:02x}").unwrap();
        }
        s
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Prefix {
    pub hash: Hash,
    pub length: u16,
}

impl From<Hash> for Prefix {
    fn from(hash: Hash) -> Self {
        Prefix { hash, length: 256 }
    }
}

impl Prefix {
    pub fn root() -> Self {
        Prefix {
            hash: [0; 32],
            length: 0,
        }
    }

    pub fn short_hex(&self) -> String {
        if self.length == 256 {
            return self.hash.short_hex();
        } else if self.length == 0 {
            return "empty".into();
        }
        let mut prefix_bits = Vec::new();
        for i in 0..self.length {
            let bit = (self.hash[(i / 8) as usize] >> (7 - (i % 8))) & 1;
            prefix_bits.push(bit);
        }
        prefix_bits
            .iter()
            .map(|b| if *b > 0 { '1' } else { '0' })
            .collect()
    }

    pub fn prefix_of(&self, other: &Prefix) -> bool {
        if self.length > other.length {
            return false;
        }
        for i in 0..self.length {
            let bit_self = (self.hash[(i / 8) as usize] >> (7 - (i % 8))) & 1;
            let bit_other = (other.hash[(i / 8) as usize] >> (7 - (i % 8))) & 1;
            if bit_self != bit_other {
                return false;
            }
        }
        true
    }

    pub fn contains(&self, key: &Hash) -> bool {
        for i in 0..self.length {
            let bit_prefix = (self.hash[(i / 8) as usize] >> (7 - (i % 8))) & 1;
            let bit_key = (key[(i / 8) as usize] >> (7 - (i % 8))) & 1;
            if bit_prefix != bit_key {
                return false;
            }
        }
        true
    }

    pub fn common_prefix(a: &Prefix, b: &Prefix) -> Prefix {
        let min_length = a.length.min(b.length);
        let mut common_bits = 0u16;
        for i in 0..min_length {
            let bit_a = (a.hash[(i / 8) as usize] >> (7 - (i % 8))) & 1 != 0;
            let bit_b = (b.hash[(i / 8) as usize] >> (7 - (i % 8))) & 1 != 0;
            if bit_a != bit_b {
                break;
            }
            common_bits += 1;
        }
        let mut common_hash = a.hash;
        // Zero out bits beyond common_bits
        for i in common_bits..256 {
            common_hash[(i / 8) as usize] &= !(1 << (7 - (i % 8)));
        }
        Prefix {
            hash: common_hash,
            length: common_bits,
        }
    }

    /// Get the bit at the given position in the hash (0 = leftmost bit)
    pub fn get_bit(&self, position: u16) -> bool {
        if position >= 256 {
            return false;
        }
        let byte_index = (position / 8) as usize;
        let bit_index = 7 - (position % 8);
        (self.hash[byte_index] >> bit_index) & 1 != 0
    }

    /// Determine if a key should go left (false) or right (true) at this prefix
    /// by looking at the bit at position self.length
    pub fn key_goes_right(&self, key: Hash) -> bool {
        if self.length >= 256 {
            return false;
        }
        let byte_index = (self.length / 8) as usize;
        let bit_index = 7 - (self.length % 8);
        (key[byte_index] >> bit_index) & 1 != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_common_prefix_identical_full_length() {
        // Two identical hashes should have a common prefix of 256 bits
        let hash = [
            0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
            0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x01, 0x02, 0x03, 0x04,
            0x05, 0x06, 0x07, 0x08,
        ];
        let prefix_a = Prefix::from(hash);
        let prefix_b = Prefix::from(hash);

        let result = Prefix::common_prefix(&prefix_a, &prefix_b);

        assert_eq!(result.length, 256);
        assert_eq!(result.hash, hash);
    }

    #[test]
    fn test_common_prefix_completely_different() {
        // Hashes that differ in the first bit should have 0 common bits
        let hash_a = [
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let hash_b = [
            0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        let prefix_a = Prefix::from(hash_a);
        let prefix_b = Prefix::from(hash_b);

        let result = Prefix::common_prefix(&prefix_a, &prefix_b);

        assert_eq!(result.length, 0);
    }

    #[test]
    fn test_common_prefix_first_byte_differs() {
        // Hashes that match for 7 bits, then differ
        let hash_a = [
            0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFF,
        ];
        let hash_b = [
            0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFF,
        ];

        let prefix_a = Prefix::from(hash_a);
        let prefix_b = Prefix::from(hash_b);

        let result = Prefix::common_prefix(&prefix_a, &prefix_b);

        // 0x00 ^ 0x01 = 0x01 which has 7 leading zeros
        assert_eq!(result.length, 7);
        assert_eq!(result.hash[0], 0x00); // Should copy from either prefix
    }

    #[test]
    fn test_common_prefix_full_bytes_match() {
        // First two bytes match completely, third byte differs
        let hash_a = [
            0xAB, 0xCD, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let hash_b = [
            0xAB, 0xCD, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        let prefix_a = Prefix::from(hash_a);
        let prefix_b = Prefix::from(hash_b);

        let result = Prefix::common_prefix(&prefix_a, &prefix_b);

        // First 16 bits match, then they differ
        assert_eq!(result.length, 16);
        assert_eq!(result.hash[0], 0xAB);
        assert_eq!(result.hash[1], 0xCD);
    }

    #[test]
    fn test_common_prefix_partial_byte_match() {
        // Match in the middle of a byte
        let hash_a = [
            0xF0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let hash_b = [
            0xF8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        let prefix_a = Prefix::from(hash_a);
        let prefix_b = Prefix::from(hash_b);

        let result = Prefix::common_prefix(&prefix_a, &prefix_b);

        // 0xF0 ^ 0xF8 = 0x08 which has 4 leading zeros
        assert_eq!(result.length, 4);
    }

    #[test]
    fn test_common_prefix_with_shorter_prefixes() {
        // Test with prefixes that are not full 256-bit hashes
        let hash_a = [
            0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let hash_b = [
            0xF0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        let prefix_a = Prefix {
            hash: hash_a,
            length: 4,
        }; // Only 4 bits long
        let prefix_b = Prefix {
            hash: hash_b,
            length: 8,
        }; // 8 bits long

        let result = Prefix::common_prefix(&prefix_a, &prefix_b);

        // Should only compare up to min_length (4 bits)
        // Both have 0xF in the top nibble, so common prefix is 4 bits
        assert_eq!(result.length, 4);
    }

    #[test]
    fn test_common_prefix_one_byte_all_bits_set() {
        let hash_a = [
            0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let hash_b = [
            0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        let prefix_a = Prefix::from(hash_a);
        let prefix_b = Prefix::from(hash_b);

        let result = Prefix::common_prefix(&prefix_a, &prefix_b);

        // First byte matches completely
        assert_eq!(result.length, 8);
        assert_eq!(result.hash[0], 0xFF);
    }

    #[test]
    fn test_common_prefix_root_prefix() {
        // Test with the root prefix (length 0)
        let root = Prefix::root();
        let hash = [
            0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45,
            0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01,
            0x23, 0x45, 0x67, 0x89,
        ];
        let prefix = Prefix::from(hash);

        let result = Prefix::common_prefix(&root, &prefix);

        // Common prefix with a length-0 prefix should be 0
        assert_eq!(result.length, 0);
    }

    #[test]
    fn test_common_prefix_symmetric() {
        // common_prefix should be symmetric in length, and the actual common bits should match
        let hash_a = [
            0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
            0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x01, 0x02, 0x03, 0x04,
            0x05, 0x06, 0x07, 0x08,
        ];
        let hash_b = [
            0x12, 0x34, 0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        let prefix_a = Prefix::from(hash_a);
        let prefix_b = Prefix::from(hash_b);

        let result_ab = Prefix::common_prefix(&prefix_a, &prefix_b);
        let result_ba = Prefix::common_prefix(&prefix_b, &prefix_a);

        assert_eq!(result_ab.length, result_ba.length);

        // The common bits (up to the common length) should be identical
        // We need to check bit-by-bit for the common prefix
        let common_len = result_ab.length;
        let full_bytes = (common_len / 8) as usize;

        // Check full bytes
        for i in 0..full_bytes {
            assert_eq!(
                result_ab.hash[i], result_ba.hash[i],
                "Byte {} differs: {:02x} vs {:02x}",
                i, result_ab.hash[i], result_ba.hash[i]
            );
        }

        // Check remaining bits if any
        if common_len % 8 != 0 {
            let rem_bits = common_len % 8;
            let mask = 0xFF << (8 - rem_bits);
            assert_eq!(
                result_ab.hash[full_bytes] & mask,
                result_ba.hash[full_bytes] & mask,
                "Partial byte {} differs",
                full_bytes
            );
        }
    }
}

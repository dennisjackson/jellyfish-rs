pub type Hash = [u8; 32];

pub trait HashExt {
    fn short_hex(&self) -> String;
    fn get_bit(&self, position: u16) -> bool;
    fn zero_bits_from(&self, start_bit: u16) -> Hash;
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

    /// Get the bit at the given position in the hash (0 = leftmost bit)
    fn get_bit(&self, position: u16) -> bool {
        if position >= 256 {
            return false;
        }
        let byte_index = (position / 8) as usize;
        let bit_index = 7 - (position % 8);
        (self[byte_index] >> bit_index) & 1 != 0
    }

    /// Zero out all bits from start_bit onwards (0 = leftmost bit)
    fn zero_bits_from(&self, start_bit: u16) -> Hash {
        let mut result = *self;
        for i in start_bit..256 {
            result[(i / 8) as usize] &= !(1 << (7 - (i % 8)));
        }
        result
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Prefix {
    pub hash: Hash,
    pub length: u16,
}

// Implement bincode Encode and Decode for Prefix
impl bincode::Encode for Prefix {
    fn encode<E: bincode::enc::Encoder>(
        &self,
        encoder: &mut E,
    ) -> Result<(), bincode::error::EncodeError> {
        bincode::Encode::encode(&self.hash, encoder)?;
        bincode::Encode::encode(&self.length, encoder)?;
        Ok(())
    }
}

impl<Context> bincode::Decode<Context> for Prefix {
    fn decode<D: bincode::de::Decoder>(
        decoder: &mut D,
    ) -> Result<Self, bincode::error::DecodeError> {
        let hash = bincode::Decode::decode(decoder)?;
        let length = bincode::Decode::decode(decoder)?;
        Ok(Prefix { hash, length })
    }
}

impl<'de, Context> bincode::BorrowDecode<'de, Context> for Prefix {
    fn borrow_decode<D: bincode::de::BorrowDecoder<'de>>(
        decoder: &mut D,
    ) -> Result<Self, bincode::error::DecodeError> {
        let hash = bincode::BorrowDecode::borrow_decode(decoder)?;
        let length = bincode::BorrowDecode::borrow_decode(decoder)?;
        Ok(Prefix { hash, length })
    }
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
            let bit = self.hash.get_bit(i);
            prefix_bits.push(bit);
        }
        prefix_bits
            .iter()
            .map(|b| if *b { '1' } else { '0' })
            .collect()
    }

    pub fn prefix_of(&self, other: &Prefix) -> bool {
        if self.length > other.length {
            return false;
        }
        for i in 0..self.length {
            if self.hash.get_bit(i) != other.hash.get_bit(i) {
                return false;
            }
        }
        true
    }

    pub fn contains(&self, key: &Hash) -> bool {
        for i in 0..self.length {
            if self.hash.get_bit(i) != key.get_bit(i) {
                return false;
            }
        }
        true
    }

    pub fn common_prefix(a: &Prefix, b: &Prefix) -> Prefix {
        let min_length = a.length.min(b.length);
        let mut common_bits = 0u16;
        for i in 0..min_length {
            if a.hash.get_bit(i) != b.hash.get_bit(i) {
                break;
            }
            common_bits += 1;
        }
        let common_hash = a.hash.zero_bits_from(common_bits);
        Prefix {
            hash: common_hash,
            length: common_bits,
        }
    }

    /// Get the bit at the given position in the hash (0 = leftmost bit)
    /// Panics if position >= self.length
    pub fn get_bit(&self, position: u16) -> bool {
        assert!(
            position < self.length,
            "Position {} is beyond prefix length {}",
            position,
            self.length
        );
        self.hash.get_bit(position)
    }

    /// Determine if a key should go left (false) or right (true) at this prefix
    /// by looking at the bit at position self.length
    pub fn key_goes_right(&self, key: Hash) -> bool {
        if self.length >= 256 {
            return false;
        }
        key.get_bit(self.length)
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
        if !common_len.is_multiple_of(8) {
            let rem_bits = common_len % 8;
            let mask = 0xFF << (8 - rem_bits);
            assert_eq!(
                result_ab.hash[full_bytes] & mask,
                result_ba.hash[full_bytes] & mask,
                "Partial byte {full_bytes} differs"
            );
        }
    }

    #[test]
    fn test_prefix_of_root_is_prefix_of_everything() {
        // Root prefix (length 0) should be a prefix of any other prefix
        let root = Prefix::root();
        let hash = [
            0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45,
            0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01,
            0x23, 0x45, 0x67, 0x89,
        ];
        let prefix = Prefix::from(hash);

        assert!(root.prefix_of(&prefix));
        assert!(root.prefix_of(&root));
    }

    #[test]
    fn test_prefix_of_identical_prefixes() {
        // A prefix should be a prefix of itself
        let hash = [
            0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
            0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x01, 0x02, 0x03, 0x04,
            0x05, 0x06, 0x07, 0x08,
        ];
        let prefix = Prefix::from(hash);

        assert!(prefix.prefix_of(&prefix));
    }

    #[test]
    fn test_prefix_of_shorter_to_longer() {
        // A shorter prefix should be a prefix of a longer one with matching bits
        let hash = [
            0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        let short_prefix = Prefix { hash, length: 4 };
        let long_prefix = Prefix::from(hash);

        assert!(short_prefix.prefix_of(&long_prefix));
    }

    #[test]
    fn test_prefix_of_longer_to_shorter_fails() {
        // A longer prefix cannot be a prefix of a shorter one
        let hash = [
            0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        let short_prefix = Prefix { hash, length: 4 };
        let long_prefix = Prefix::from(hash);

        assert!(!long_prefix.prefix_of(&short_prefix));
    }

    #[test]
    fn test_prefix_of_different_bits() {
        // Prefixes with different bits should not be prefixes of each other
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

        let prefix_a = Prefix {
            hash: hash_a,
            length: 8,
        };
        let prefix_b = Prefix {
            hash: hash_b,
            length: 8,
        };

        assert!(!prefix_a.prefix_of(&prefix_b));
        assert!(!prefix_b.prefix_of(&prefix_a));
    }

    #[test]
    fn test_prefix_of_partial_match() {
        // Test when prefixes match up to a point but then diverge
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

        // First 4 bits match (1111), then diverge
        let prefix_a = Prefix {
            hash: hash_a,
            length: 4,
        };
        let prefix_b = Prefix {
            hash: hash_b,
            length: 8,
        };

        assert!(prefix_a.prefix_of(&prefix_b)); // First 4 bits of b match a
        assert!(!prefix_b.prefix_of(&prefix_a)); // b is longer and has different 5th bit
    }

    #[test]
    fn test_get_bit_first_and_last() {
        // Test getting the first bit (leftmost) and last bit (rightmost)
        let hash = [
            0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x01,
        ];
        let prefix = Prefix::from(hash);

        assert!(prefix.get_bit(0)); // First bit is 1
        assert!(!prefix.get_bit(1)); // Second bit is 0
        assert!(prefix.get_bit(255)); // Last bit is 1
        assert!(!prefix.get_bit(254)); // Second to last bit is 0
    }

    #[test]
    fn test_get_bit_all_ones() {
        // Test with all bits set to 1
        let hash = [0xFF; 32];
        let prefix = Prefix::from(hash);

        for i in 0..256 {
            assert!(prefix.get_bit(i), "Bit {i} should be 1");
        }
    }

    #[test]
    fn test_get_bit_all_zeros() {
        // Test with all bits set to 0
        let hash = [0x00; 32];
        let prefix = Prefix::from(hash);

        for i in 0..256 {
            assert!(!prefix.get_bit(i), "Bit {i} should be 0");
        }
    }

    #[test]
    fn test_get_bit_alternating_pattern() {
        // Test with alternating bit pattern: 0xAA = 10101010
        let hash = [0xAA; 32];
        let prefix = Prefix::from(hash);

        for i in 0..256 {
            if i % 2 == 0 {
                assert!(prefix.get_bit(i), "Bit {i} should be 1");
            } else {
                assert!(!prefix.get_bit(i), "Bit {i} should be 0");
            }
        }
    }

    #[test]
    fn test_get_bit_byte_boundaries() {
        // Test bits at byte boundaries
        let hash = [
            0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let prefix = Prefix::from(hash);

        // 0x01 = 00000001, so bit 7 is 1
        assert!(prefix.get_bit(7));
        // 0x02 = 00000010, so bit 14 is 1
        assert!(prefix.get_bit(14));
        // 0x04 = 00000100, so bit 21 is 1
        assert!(prefix.get_bit(21));
        // 0x08 = 00001000, so bit 28 is 1
        assert!(prefix.get_bit(28));
        // 0x10 = 00010000, so bit 35 is 1
        assert!(prefix.get_bit(35));
        // 0x20 = 00100000, so bit 42 is 1
        assert!(prefix.get_bit(42));
        // 0x40 = 01000000, so bit 49 is 1
        assert!(prefix.get_bit(49));
        // 0x80 = 10000000, so bit 56 is 1
        assert!(prefix.get_bit(56));
    }

    #[test]
    #[should_panic(expected = "Position 256 is beyond prefix length 256")]
    fn test_get_bit_out_of_range() {
        // Test that getting a bit at or beyond the prefix length panics
        let hash = [0xFF; 32];
        let prefix = Prefix::from(hash);

        prefix.get_bit(256); // This should panic
    }

    #[test]
    #[should_panic(expected = "is beyond prefix length")]
    fn test_get_bit_beyond_short_prefix() {
        // Test that getting a bit beyond a short prefix's length panics
        let hash = [0xFF; 32];
        let prefix = Prefix { hash, length: 10 };

        prefix.get_bit(10); // This should panic (length is 10, so valid bits are 0-9)
    }

    #[test]
    fn test_get_bit_specific_positions() {
        // Test specific bit positions with known values
        // 0xAB = 10101011, 0xCD = 11001101
        let hash = [
            0xAB, 0xCD, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let prefix = Prefix::from(hash);

        // First byte: 0xAB = 10101011
        assert!(prefix.get_bit(0)); // 1
        assert!(!prefix.get_bit(1)); // 0
        assert!(prefix.get_bit(2)); // 1
        assert!(!prefix.get_bit(3)); // 0
        assert!(prefix.get_bit(4)); // 1
        assert!(!prefix.get_bit(5)); // 0
        assert!(prefix.get_bit(6)); // 1
        assert!(prefix.get_bit(7)); // 1

        // Second byte: 0xCD = 11001101
        assert!(prefix.get_bit(8)); // 1
        assert!(prefix.get_bit(9)); // 1
        assert!(!prefix.get_bit(10)); // 0
        assert!(!prefix.get_bit(11)); // 0
        assert!(prefix.get_bit(12)); // 1
        assert!(prefix.get_bit(13)); // 1
        assert!(!prefix.get_bit(14)); // 0
        assert!(prefix.get_bit(15)); // 1
    }

    #[test]
    fn test_short_hex_root_prefix() {
        // Root prefix (length 0) should return "empty"
        let root = Prefix::root();
        assert_eq!(root.short_hex(), "empty");
    }

    #[test]
    fn test_short_hex_full_length_prefix() {
        // Full 256-bit prefix should return first 4 bytes as hex
        let hash = [
            0xAB, 0xCD, 0xEF, 0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x11, 0x22, 0x33,
            0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x01,
            0x02, 0x03, 0x04, 0x05,
        ];
        let prefix = Prefix::from(hash);
        assert_eq!(prefix.short_hex(), "abcdef12");
    }

    #[test]
    fn test_short_hex_partial_prefix_single_bit() {
        // Single bit prefix should return "0" or "1"
        let hash_zero = [0x00; 32];
        let prefix_zero = Prefix {
            hash: hash_zero,
            length: 1,
        };
        assert_eq!(prefix_zero.short_hex(), "0");

        let hash_one = [
            0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0,
        ];
        let prefix_one = Prefix {
            hash: hash_one,
            length: 1,
        };
        assert_eq!(prefix_one.short_hex(), "1");
    }

    #[test]
    fn test_short_hex_partial_prefix_multiple_bits() {
        // Test a prefix with 4 bits (0xF = 1111)
        let hash = [
            0xF0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let prefix = Prefix { hash, length: 4 };
        assert_eq!(prefix.short_hex(), "1111");
    }

    #[test]
    fn test_short_hex_partial_prefix_byte_boundary() {
        // Test a prefix that's exactly 8 bits (1 byte)
        let hash = [
            0xAB, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let prefix = Prefix { hash, length: 8 };
        // 0xAB = 10101011
        assert_eq!(prefix.short_hex(), "10101011");
    }

    #[test]
    fn test_short_hex_no_collision_different_full_hashes() {
        // Different full-length hashes should have different short_hex values
        let hash_a = [
            0x12, 0x34, 0x56, 0x78, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let hash_b = [
            0xAB, 0xCD, 0xEF, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        let prefix_a = Prefix::from(hash_a);
        let prefix_b = Prefix::from(hash_b);

        assert_ne!(prefix_a.short_hex(), prefix_b.short_hex());
        assert_eq!(prefix_a.short_hex(), "12345678");
        assert_eq!(prefix_b.short_hex(), "abcdef01");
    }

    #[test]
    fn test_short_hex_no_collision_different_partial_prefixes() {
        // Partial prefixes with different bit patterns should not collide
        let hash_a = [
            0xF0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let hash_b = [
            0x0F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        let prefix_a = Prefix {
            hash: hash_a,
            length: 4,
        };
        let prefix_b = Prefix {
            hash: hash_b,
            length: 4,
        };

        assert_ne!(prefix_a.short_hex(), prefix_b.short_hex());
        assert_eq!(prefix_a.short_hex(), "1111"); // 0xF0 = 11110000
        assert_eq!(prefix_b.short_hex(), "0000"); // 0x0F = 00001111
    }

    #[test]
    fn test_short_hex_collision_only_when_same_first_4_bytes() {
        // Full-length prefixes can only collide if first 4 bytes are identical
        let hash_a = [
            0x12, 0x34, 0x56, 0x78, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let hash_b = [
            0x12, 0x34, 0x56, 0x78, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        let prefix_a = Prefix::from(hash_a);
        let prefix_b = Prefix::from(hash_b);

        // These should have the same short_hex because first 4 bytes match
        assert_eq!(prefix_a.short_hex(), prefix_b.short_hex());
        assert_eq!(prefix_a.short_hex(), "12345678");
    }

    #[test]
    fn test_short_hex_different_lengths_same_bits() {
        // Prefixes of different lengths but with same leading bits should differ
        let hash = [
            0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        let prefix_4 = Prefix { hash, length: 4 };
        let prefix_8 = Prefix { hash, length: 8 };
        let prefix_256 = Prefix::from(hash);

        assert_ne!(prefix_4.short_hex(), prefix_8.short_hex());
        assert_ne!(prefix_8.short_hex(), prefix_256.short_hex());
        assert_ne!(prefix_4.short_hex(), prefix_256.short_hex());

        assert_eq!(prefix_4.short_hex(), "1111");
        assert_eq!(prefix_8.short_hex(), "11111111");
        assert_eq!(prefix_256.short_hex(), "ff000000");
    }

    #[test]
    fn test_short_hex_various_lengths() {
        // Test various prefix lengths to ensure consistent behavior
        let hash = [
            0xAA, 0x55, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        // 0xAA = 10101010, 0x55 = 01010101
        let prefix_2 = Prefix { hash, length: 2 };
        assert_eq!(prefix_2.short_hex(), "10");

        let prefix_4 = Prefix { hash, length: 4 };
        assert_eq!(prefix_4.short_hex(), "1010");

        let prefix_8 = Prefix { hash, length: 8 };
        assert_eq!(prefix_8.short_hex(), "10101010");

        let prefix_12 = Prefix { hash, length: 12 };
        assert_eq!(prefix_12.short_hex(), "101010100101");

        let prefix_16 = Prefix { hash, length: 16 };
        assert_eq!(prefix_16.short_hex(), "1010101001010101");
    }

    #[test]
    fn test_key_goes_right_root_prefix() {
        // At root prefix (length 0), check the first bit of the key
        let root = Prefix::root();

        // Key with first bit 0 (0x00...)
        let key_left = [0x00; 32];
        assert!(!root.key_goes_right(key_left));

        // Key with first bit 1 (0x80...)
        let key_right = [
            0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        assert!(root.key_goes_right(key_right));
    }

    #[test]
    fn test_key_goes_right_at_byte_boundaries() {
        // Test at various byte boundaries

        // At bit position 7 (end of first byte)
        let prefix_7 = Prefix {
            hash: [0x00; 32],
            length: 7,
        };
        let key_with_bit_7_set = [
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let key_with_bit_7_clear = [0x00; 32];
        assert!(prefix_7.key_goes_right(key_with_bit_7_set));
        assert!(!prefix_7.key_goes_right(key_with_bit_7_clear));

        // At bit position 8 (start of second byte)
        let prefix_8 = Prefix {
            hash: [0x00; 32],
            length: 8,
        };
        let key_with_bit_8_set = [
            0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        assert!(prefix_8.key_goes_right(key_with_bit_8_set));
        assert!(!prefix_8.key_goes_right(key_with_bit_7_clear));
    }

    #[test]
    fn test_key_goes_right_mid_byte() {
        // Test in the middle of a byte (e.g., bit position 4)
        let prefix_4 = Prefix {
            hash: [
                0xF0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00,
            ],
            length: 4,
        };

        // Key with bit 4 set to 1 (0xF8 = 11111000)
        let key_right = [
            0xF8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        // Key with bit 4 set to 0 (0xF0 = 11110000)
        let key_left = [
            0xF0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        assert!(prefix_4.key_goes_right(key_right));
        assert!(!prefix_4.key_goes_right(key_left));
    }

    #[test]
    fn test_key_goes_right_various_positions() {
        // Test several different bit positions
        for position in [0u16, 1, 7, 8, 15, 16, 23, 24, 100, 200, 255] {
            let prefix = Prefix {
                hash: [0x00; 32],
                length: position,
            };

            // Create key with bit at position set
            let mut key_with_bit = [0x00; 32];
            let byte_idx = (position / 8) as usize;
            let bit_idx = 7 - (position % 8);
            key_with_bit[byte_idx] = 1 << bit_idx;

            // Create key without bit at position set
            let key_without_bit = [0x00; 32];

            assert!(
                prefix.key_goes_right(key_with_bit),
                "Failed at position {position}: expected right"
            );
            assert!(
                !prefix.key_goes_right(key_without_bit),
                "Failed at position {position}: expected left"
            );
        }
    }

    #[test]
    fn test_key_goes_right_full_length_prefix() {
        // A prefix with length 256 should always return false
        let hash = [0xAB; 32];
        let prefix = Prefix::from(hash);

        let key_any_1 = [0xFF; 32];
        let key_any_2 = [0x00; 32];
        let key_any_3 = [0xAB; 32];

        assert!(!prefix.key_goes_right(key_any_1));
        assert!(!prefix.key_goes_right(key_any_2));
        assert!(!prefix.key_goes_right(key_any_3));
    }

    #[test]
    fn test_key_goes_right_ignores_earlier_bits() {
        // The decision should only depend on the bit at position self.length,
        // not on earlier bits
        let prefix = Prefix {
            hash: [0x00; 32],
            length: 16,
        };

        // Two keys that differ in earlier bits but have same bit at position 16
        let key_a = [
            0xFF, 0xFF, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let key_b = [
            0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        // Both should go right because bit 16 is set in both
        assert!(prefix.key_goes_right(key_a));
        assert!(prefix.key_goes_right(key_b));
    }

    #[test]
    fn test_key_goes_right_ignores_later_bits() {
        // The decision should not depend on bits after position self.length
        let prefix = Prefix {
            hash: [0x00; 32],
            length: 16,
        };

        // Two keys that differ in later bits but have same bit at position 16
        let key_a = [
            0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFF,
        ];
        let key_b = [
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];

        // Both should go left because bit 16 is clear in both
        assert!(!prefix.key_goes_right(key_a));
        assert!(!prefix.key_goes_right(key_b));
    }

    #[test]
    fn test_key_goes_right_consistency_with_get_bit() {
        // key_goes_right should be consistent with get_bit on the key
        let positions = [0u16, 1, 4, 7, 8, 15, 16, 31, 32, 63, 64, 127, 128, 200, 255];

        for &position in &positions {
            let prefix = Prefix {
                hash: [0x00; 32],
                length: position,
            };

            // Create various keys
            let keys = [[0xFF; 32], [0x00; 32], [0xAA; 32], [0x55; 32]];

            for key in &keys {
                let key_prefix = Prefix::from(*key);
                let expected = key_prefix.get_bit(position);
                let actual = prefix.key_goes_right(*key);

                assert_eq!(
                    actual, expected,
                    "Mismatch at position {position}: key_goes_right={actual}, get_bit={expected}"
                );
            }
        }
    }

    #[test]
    fn test_key_goes_right_last_bit() {
        // Test at the last valid position (255)
        let prefix_255 = Prefix {
            hash: [0x00; 32],
            length: 255,
        };

        // Key with last bit set
        let key_last_bit_set = [
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x01,
        ];

        // Key with last bit clear
        let key_last_bit_clear = [0x00; 32];

        assert!(prefix_255.key_goes_right(key_last_bit_set));
        assert!(!prefix_255.key_goes_right(key_last_bit_clear));
    }
}

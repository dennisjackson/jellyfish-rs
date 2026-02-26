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

    /// Return the parent prefix (one bit shorter), or `None` for the root prefix.
    pub fn parent(&self) -> Option<Prefix> {
        if self.length == 0 {
            return None;
        }
        let parent_length = self.length - 1;
        Some(Prefix {
            hash: self.hash.zero_bits_from(parent_length),
            length: parent_length,
        })
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
        let full_bytes = (self.length / 8) as usize;
        if self.hash[..full_bytes] != other.hash[..full_bytes] {
            return false;
        }
        let remaining_bits = (self.length % 8) as u8;
        if remaining_bits == 0 {
            return true;
        }
        let mask = 0xFF << (8 - remaining_bits);
        (self.hash[full_bytes] & mask) == (other.hash[full_bytes] & mask)
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
mod tests;

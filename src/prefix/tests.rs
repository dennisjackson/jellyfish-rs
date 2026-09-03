//! Every bit-twiddling function against a bit-at-a-time reference, over keys whose first
//! disagreement sits at each of the 257 positions and lengths at every word/byte boundary.

use super::*;
use crate::testing::TestRng;

const LENGTHS: [u16; 14] = [0, 1, 7, 8, 9, 63, 64, 65, 127, 128, 191, 192, 255, 256];

/// A key whose leading bytes are `bytes`, zero past them.
fn key(bytes: &[u8]) -> Key {
    let mut out = [0u8; 32];
    out[..bytes.len()].copy_from_slice(bytes);
    Key(out)
}

/// Tail left as given, so a test can prove only the first `length` bits are read.
fn prefix(key: Key, length: u16) -> Prefix {
    Prefix { key, length }
}

fn bit(key: &Key, i: u16) -> bool {
    (key.0[(i / 8) as usize] >> (7 - i % 8)) & 1 == 1
}

/// `key` with bit `i` flipped; `i == 256` leaves it untouched.
fn flipped(key: &Key, i: u16) -> Key {
    let mut out = key.0;
    if i < 256 {
        out[(i / 8) as usize] ^= 1 << (7 - i % 8);
    }
    Key(out)
}

fn keys(count: usize) -> Vec<Key> {
    let mut rng = TestRng::seed(0x9E37);
    let mut out = vec![
        Key([0x00; 32]),
        Key([0xFF; 32]),
        Key([0xAA; 32]),
        Key([0x55; 32]),
    ];
    out.extend((0..count).map(|_| Key(rng.bytes())));
    out
}

#[test]
fn get_bit_reads_msb_first() {
    for k in keys(4) {
        for i in 0..256 {
            assert_eq!(k.get_bit(i), bit(&k, i), "bit {i} of {k:?}");
        }
    }
}

#[test]
fn zero_bits_from_matches_the_bitwise_reference() {
    fn reference(key: &Key, start: u16) -> Key {
        let mut out = key.0;
        for i in start..256 {
            out[(i / 8) as usize] &= !(1 << (7 - i % 8));
        }
        Key(out)
    }
    for k in keys(64) {
        for start in (0..=257).chain([1000, u16::MAX]) {
            assert_eq!(
                k.zero_bits_from(start),
                reference(&k, start),
                "start {start}"
            );
        }
    }
}

#[test]
fn contains_matches_the_bitwise_reference() {
    fn agree_on(a: &Key, b: &Key, bits: u16) -> bool {
        (0..bits).all(|i| bit(a, i) == bit(b, i))
    }
    for k in keys(4) {
        for shared in 0..=256 {
            let other = flipped(&k, shared);
            let lengths = LENGTHS.into_iter().chain([
                shared.saturating_sub(1),
                shared,
                (shared + 1).min(256),
            ]);
            for length in lengths {
                assert_eq!(
                    prefix(other, length).contains(&k),
                    agree_on(&other, &k, length),
                    "contains: shared={shared} length={length}"
                );
            }
        }
    }
}

#[test]
fn common_prefix_matches_the_bitwise_reference() {
    fn reference(a: &Prefix, b: &Prefix) -> Prefix {
        let common = (0..a.length.min(b.length))
            .take_while(|&i| bit(&a.key, i) == bit(&b.key, i))
            .count() as u16;
        let mut out = a.key.0;
        for i in common..256 {
            out[(i / 8) as usize] &= !(1 << (7 - i % 8));
        }
        Prefix {
            key: Key(out),
            length: common,
        }
    }
    let lengths = [0, 1, 8, 63, 64, 65, 128, 255, 256];
    for k in keys(3) {
        for shared in 0..=256 {
            // Invert the tail past the disagreement so a surviving bit is observable.
            let mut other = flipped(&k, shared).0;
            for byte in other.iter_mut().skip((shared / 8) as usize + 1) {
                *byte = !*byte;
            }
            for a_len in lengths {
                for b_len in lengths {
                    let (a, b) = (prefix(k, a_len), prefix(Key(other), b_len));
                    assert_eq!(
                        Prefix::common_prefix(&a, &b),
                        reference(&a, &b),
                        "shared={shared} a_len={a_len} b_len={b_len}"
                    );
                }
            }
        }
    }
    assert_eq!(Prefix::root(), prefix(Key::ZERO, 0));
}

#[test]
fn key_goes_right_reads_the_bit_at_the_length() {
    for k in keys(2) {
        for length in LENGTHS.into_iter().filter(|&l| l < 256) {
            for own in [Key::ZERO, Key([0xFF; 32])] {
                assert_eq!(
                    prefix(own, length).key_goes_right(k),
                    bit(&k, length),
                    "length {length}"
                );
            }
        }
    }
}

#[test]
fn successor_is_the_first_key_past_the_prefix() {
    fn reference(p: &Prefix) -> Option<Key> {
        if p.length == 0 {
            return None;
        }
        let mut bytes = p.key.0;
        for i in p.length..256 {
            bytes[(i / 8) as usize] |= 1 << (7 - i % 8);
        }
        for byte in bytes.iter_mut().rev() {
            let (sum, carry) = byte.overflowing_add(1);
            *byte = sum;
            if !carry {
                return Some(Key(bytes));
            }
        }
        None
    }
    for k in keys(8) {
        for length in LENGTHS {
            let p = Prefix::new(k.zero_bits_from(length), length);
            assert_eq!(p.successor(), reference(&p), "{p:?}");
        }
    }
}

#[test]
fn short_hex_names_a_prefix_by_its_length() {
    let k = key(&[0xAB, 0xCD, 0xEF, 0x12, 0x34]);
    assert_eq!(Prefix::root().short_hex(), "empty");
    assert_eq!(
        Prefix::from(k).short_hex(),
        "abcdef12",
        "a leaf: its first four bytes"
    );
    assert_eq!(prefix(k, 1).short_hex(), "1");
    assert_eq!(prefix(k, 8).short_hex(), "10101011");
    assert_eq!(
        prefix(k, 12).short_hex(),
        "101010111100",
        "an interior: its bits"
    );
    assert_eq!(format!("{k:?}"), "Key(abcdef12)");
    assert_eq!(format!("{:?}", Digest(k.0)), "Digest(abcdef12)");
}

// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! DES encryption of one block (FIPS 46-3), for VNC's password sign-in
//! alone (see vnc.rs).
//!
//! VNC signs in by encrypting the server's challenge with DES, keyed by
//! the password. DES has long been too weak to keep anything secret, and
//! the lab uses it only because AMT's remote screen asks for it, on the
//! lab network alone, with a password made afresh for each run. The
//! tables are the standard's, with bit 1 the most significant.

/// The initial permutation.
const IP: [u8; 64] = [
    58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6,
    64, 56, 48, 40, 32, 24, 16, 8, 57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3, 61,
    53, 45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
];

/// The final permutation, the initial one's inverse.
const FP: [u8; 64] = [
    40, 8, 48, 16, 56, 24, 64, 32, 39, 7, 47, 15, 55, 23, 63, 31, 38, 6, 46, 14, 54, 22, 62, 30,
    37, 5, 45, 13, 53, 21, 61, 29, 36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19, 59, 27,
    34, 2, 42, 10, 50, 18, 58, 26, 33, 1, 41, 9, 49, 17, 57, 25,
];

/// The expansion of a half block's 32 bits to 48.
const E: [u8; 48] = [
    32, 1, 2, 3, 4, 5, 4, 5, 6, 7, 8, 9, 8, 9, 10, 11, 12, 13, 12, 13, 14, 15, 16, 17, 16, 17, 18,
    19, 20, 21, 20, 21, 22, 23, 24, 25, 24, 25, 26, 27, 28, 29, 28, 29, 30, 31, 32, 1,
];

/// The permutation of the S-boxes' output.
const P: [u8; 32] = [
    16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10, 2, 8, 24, 14, 32, 27, 3, 9, 19,
    13, 30, 6, 22, 11, 4, 25,
];

/// Permuted choice 1: the key's 56 bits, without its parity bits.
const PC1: [u8; 56] = [
    57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59, 51, 43, 35, 27, 19, 11, 3, 60,
    52, 44, 36, 63, 55, 47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37, 29,
    21, 13, 5, 28, 20, 12, 4,
];

/// Permuted choice 2: each round's 48-bit key.
const PC2: [u8; 48] = [
    14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10, 23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2, 41, 52,
    31, 37, 47, 55, 30, 40, 51, 45, 33, 48, 44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
];

/// How far each round turns the key's halves.
const SHIFTS: [u32; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];

/// The eight S-boxes, each 4 rows of 16.
const S: [[u8; 64]; 8] = [
    [
        14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12,
        11, 9, 5, 3, 8, 4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2, 4, 9,
        1, 7, 5, 11, 3, 14, 10, 0, 6, 13,
    ],
    [
        15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1,
        10, 6, 9, 11, 5, 0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13, 8, 10, 1, 3, 15,
        4, 2, 11, 6, 7, 12, 0, 5, 14, 9,
    ],
    [
        10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5,
        14, 12, 11, 15, 1, 13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7, 1, 10, 13, 0, 6,
        9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12,
    ],
    [
        7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2,
        12, 1, 10, 14, 9, 10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6, 10, 1,
        13, 8, 9, 4, 5, 11, 12, 7, 2, 14,
    ],
    [
        2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15,
        10, 3, 9, 8, 6, 4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7, 1, 14,
        2, 13, 6, 15, 0, 9, 10, 4, 5, 3,
    ],
    [
        12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13,
        14, 0, 11, 3, 8, 9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6, 4, 3, 2, 12, 9, 5,
        15, 10, 11, 14, 1, 7, 6, 0, 8, 13,
    ],
    [
        4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5,
        12, 2, 15, 8, 6, 1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8, 1, 4,
        10, 7, 9, 5, 0, 15, 14, 2, 3, 12,
    ],
    [
        13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6,
        11, 0, 14, 9, 2, 7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7, 4, 10,
        8, 13, 15, 12, 9, 0, 3, 5, 6, 11,
    ],
];

/// Picks bits of `input`, which is `width` bits wide, in the order
/// `table` gives (bit 1 the most significant).
fn permute(input: u64, width: u32, table: &[u8]) -> u64 {
    table.iter().fold(0, |out, &bit| (out << 1) | ((input >> (width - u32::from(bit))) & 1))
}

/// The sixteen round keys of `key`.
fn round_keys(key: [u8; 8]) -> [u64; 16] {
    let chosen = permute(u64::from_be_bytes(key), 64, &PC1);
    let (mut c, mut d) = ((chosen >> 28) & 0x0fff_ffff, chosen & 0x0fff_ffff);
    let turn = |half: u64, by: u32| ((half << by) | (half >> (28 - by))) & 0x0fff_ffff;
    let mut keys = [0; 16];
    for (round, key) in keys.iter_mut().enumerate() {
        c = turn(c, SHIFTS[round]);
        d = turn(d, SHIFTS[round]);
        *key = permute((c << 28) | d, 56, &PC2);
    }
    keys
}

/// The round function: the half block expanded, keyed, through the
/// S-boxes, and permuted.
fn feistel(half: u32, key: u64) -> u32 {
    let mixed = permute(u64::from(half), 32, &E) ^ key;
    let mut out = 0u64;
    for (number, sbox) in S.iter().enumerate() {
        let six = (mixed >> (42 - 6 * number)) & 0x3f;
        // The outer bits pick the row, the inner four the column.
        let row = ((six & 0x20) >> 4) | (six & 1);
        let column = (six >> 1) & 0xf;
        out = (out << 4) | u64::from(sbox[(row * 16 + column) as usize]);
    }
    permute(out, 32, &P) as u32
}

/// Encrypts one 8-byte block with `key` (whose parity bits are ignored).
pub fn encrypt(key: [u8; 8], block: [u8; 8]) -> [u8; 8] {
    let keys = round_keys(key);
    let state = permute(u64::from_be_bytes(block), 64, &IP);
    let (mut left, mut right) = ((state >> 32) as u32, state as u32);
    for key in keys {
        (left, right) = (right, left ^ feistel(right, key));
    }
    // The last round's halves are swapped back.
    permute((u64::from(right) << 32) | u64::from(left), 64, &FP).to_be_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypts_the_standard_example() {
        // The worked example most DES write-ups follow.
        let key = [0x13, 0x34, 0x57, 0x79, 0x9b, 0xbc, 0xdf, 0xf1];
        let block = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        assert_eq!(encrypt(key, block), [0x85, 0xe8, 0x13, 0x54, 0x0f, 0x0a, 0xb4, 0x05]);
    }

    #[test]
    fn matches_the_published_test_vectors() {
        // From NIST SP 800-17's variable-plaintext and variable-key
        // tables, with the key of all parity bits set.
        let ones = [0x01; 8];
        assert_eq!(
            encrypt(ones, [0x80, 0, 0, 0, 0, 0, 0, 0]),
            [0x95, 0xf8, 0xa5, 0xe5, 0xdd, 0x31, 0xd9, 0x00]
        );
        assert_eq!(
            encrypt([0x80, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01], [0; 8]),
            [0x95, 0xa8, 0xd7, 0x28, 0x13, 0xda, 0xa9, 0x4d]
        );
    }
}

// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The two hashes real machines need: MD5 for Intel AMT's HTTP digest
//! sign-in, which allows no other, and SHA-256 for checking that a
//! machine's disk holds the image under test once the writer is done.
//!
//! Both are written out here from their specifications (RFC 1321 and
//! FIPS 180-4) rather than taken from a crate, as the lab keeps to three
//! dependencies. MD5 is broken as a hash; HTTP digest uses it only to
//! prove the password without sending it, on the lab's own network.

/// Feeds bytes into a hash in pieces of any size.
pub trait Hasher {
    fn update(&mut self, bytes: &[u8]);
}

/// Shared by both: whole 64-byte blocks go to `compress`, the rest waits.
#[derive(Clone)]
struct Blocks {
    pending: [u8; 64],
    filled: usize,
    /// Bytes fed so far.
    length: u64,
}

impl Blocks {
    fn new() -> Blocks {
        Blocks { pending: [0; 64], filled: 0, length: 0 }
    }

    fn update(&mut self, mut bytes: &[u8], mut compress: impl FnMut(&[u8; 64])) {
        self.length += bytes.len() as u64;
        if self.filled > 0 {
            let take = (64 - self.filled).min(bytes.len());
            self.pending[self.filled..self.filled + take].copy_from_slice(&bytes[..take]);
            self.filled += take;
            bytes = &bytes[take..];
            if self.filled < 64 {
                return;
            }
            compress(&self.pending);
            self.filled = 0;
        }
        let mut chunks = bytes.chunks_exact(64);
        for chunk in &mut chunks {
            compress(chunk.try_into().expect("a 64-byte chunk"));
        }
        let rest = chunks.remainder();
        self.pending[..rest.len()].copy_from_slice(rest);
        self.filled = rest.len();
    }

    /// Pads the last block with the length in bits, little- or
    /// big-endian, and compresses what is left.
    fn finish(mut self, big_endian: bool, mut compress: impl FnMut(&[u8; 64])) {
        let bits = self.length.wrapping_mul(8);
        let mut padding = vec![0x80u8];
        let after = (self.filled + 1) % 64;
        let zeros = if after <= 56 { 56 - after } else { 120 - after };
        padding.resize(1 + zeros, 0);
        padding.extend_from_slice(&if big_endian {
            bits.to_be_bytes()
        } else {
            bits.to_le_bytes()
        });
        let length = self.length;
        self.update(&padding, &mut compress);
        self.length = length;
        debug_assert_eq!(self.filled, 0);
    }
}

/// MD5 (RFC 1321).
#[derive(Clone)]
pub struct Md5 {
    state: [u32; 4],
    blocks: Blocks,
}

const MD5_SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// floor(abs(sin(i + 1)) * 2^32), as RFC 1321 lists them.
const MD5_SINES: [u32; 64] = [
    0xd76a_a478,
    0xe8c7_b756,
    0x2420_70db,
    0xc1bd_ceee,
    0xf57c_0faf,
    0x4787_c62a,
    0xa830_4613,
    0xfd46_9501,
    0x6980_98d8,
    0x8b44_f7af,
    0xffff_5bb1,
    0x895c_d7be,
    0x6b90_1122,
    0xfd98_7193,
    0xa679_438e,
    0x49b4_0821,
    0xf61e_2562,
    0xc040_b340,
    0x265e_5a51,
    0xe9b6_c7aa,
    0xd62f_105d,
    0x0244_1453,
    0xd8a1_e681,
    0xe7d3_fbc8,
    0x21e1_cde6,
    0xc337_07d6,
    0xf4d5_0d87,
    0x455a_14ed,
    0xa9e3_e905,
    0xfcef_a3f8,
    0x676f_02d9,
    0x8d2a_4c8a,
    0xfffa_3942,
    0x8771_f681,
    0x6d9d_6122,
    0xfde5_380c,
    0xa4be_ea44,
    0x4bde_cfa9,
    0xf6bb_4b60,
    0xbebf_bc70,
    0x289b_7ec6,
    0xeaa1_27fa,
    0xd4ef_3085,
    0x0488_1d05,
    0xd9d4_d039,
    0xe6db_99e5,
    0x1fa2_7cf8,
    0xc4ac_5665,
    0xf429_2244,
    0x432a_ff97,
    0xab94_23a7,
    0xfc93_a039,
    0x655b_59c3,
    0x8f0c_cc92,
    0xffef_f47d,
    0x8584_5dd1,
    0x6fa8_7e4f,
    0xfe2c_e6e0,
    0xa301_4314,
    0x4e08_11a1,
    0xf753_7e82,
    0xbd3a_f235,
    0x2ad7_d2bb,
    0xeb86_d391,
];

impl Md5 {
    pub fn new() -> Md5 {
        Md5 { state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476], blocks: Blocks::new() }
    }

    fn compress(state: &mut [u32; 4], block: &[u8; 64]) {
        let words: Vec<u32> = block
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes(word.try_into().expect("4 bytes")))
            .collect();
        let [mut a, mut b, mut c, mut d] = *state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let sum = a.wrapping_add(f).wrapping_add(MD5_SINES[i]).wrapping_add(words[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(sum.rotate_left(MD5_SHIFTS[i]));
        }
        for (word, add) in state.iter_mut().zip([a, b, c, d]) {
            *word = word.wrapping_add(add);
        }
    }

    pub fn finish(mut self) -> [u8; 16] {
        let state = &mut self.state;
        self.blocks.finish(false, |block| Md5::compress(state, block));
        let mut out = [0; 16];
        for (bytes, word) in out.chunks_exact_mut(4).zip(self.state) {
            bytes.copy_from_slice(&word.to_le_bytes());
        }
        out
    }
}

impl Default for Md5 {
    fn default() -> Self {
        Md5::new()
    }
}

impl Hasher for Md5 {
    fn update(&mut self, bytes: &[u8]) {
        let state = &mut self.state;
        self.blocks.update(bytes, |block| Md5::compress(state, block));
    }
}

/// SHA-256 (FIPS 180-4).
#[derive(Clone)]
pub struct Sha256 {
    state: [u32; 8],
    blocks: Blocks,
}

/// The first 32 bits of the fractional parts of the cube roots of the
/// first 64 primes.
const SHA256_ROUNDS: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

impl Sha256 {
    pub fn new() -> Sha256 {
        Sha256 {
            state: [
                0x6a09_e667,
                0xbb67_ae85,
                0x3c6e_f372,
                0xa54f_f53a,
                0x510e_527f,
                0x9b05_688c,
                0x1f83_d9ab,
                0x5be0_cd19,
            ],
            blocks: Blocks::new(),
        }
    }

    fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (word, bytes) in w.iter_mut().zip(block.chunks_exact(4)) {
            *word = u32::from_be_bytes(bytes.try_into().expect("4 bytes"));
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(SHA256_ROUNDS[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (word, add) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *word = word.wrapping_add(add);
        }
    }

    pub fn finish(mut self) -> [u8; 32] {
        let state = &mut self.state;
        self.blocks.finish(true, |block| Sha256::compress(state, block));
        let mut out = [0; 32];
        for (bytes, word) in out.chunks_exact_mut(4).zip(self.state) {
            bytes.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Sha256::new()
    }
}

impl Hasher for Sha256 {
    fn update(&mut self, bytes: &[u8]) {
        let state = &mut self.state;
        self.blocks.update(bytes, |block| Sha256::compress(state, block));
    }
}

/// `bytes` as lower-case hexadecimal.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The MD5 of `bytes`, in hexadecimal.
pub fn md5_hex(bytes: &[u8]) -> String {
    let mut md5 = Md5::new();
    md5.update(bytes);
    hex(&md5.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_rfc_1321() {
        for (text, hash) in [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            ("abcdefghijklmnopqrstuvwxyz", "c3fcd3d76192e4007dfb496cca67e13b"),
            (
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ] {
            assert_eq!(md5_hex(text.as_bytes()), hash, "{text}");
        }
    }

    #[test]
    fn sha256_matches_fips_180() {
        for (text, hash) in [
            ("", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
            ("abc", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
            (
                "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
        ] {
            let mut sha = Sha256::new();
            sha.update(text.as_bytes());
            assert_eq!(hex(&sha.finish()), hash, "{text}");
        }
    }

    #[test]
    fn pieces_of_any_size_hash_as_one() {
        let bytes: Vec<u8> = (0..1000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut whole = Sha256::new();
        whole.update(&bytes);
        let whole = whole.finish();
        for piece in [1, 3, 63, 64, 65, 200] {
            let mut pieces = Sha256::new();
            for chunk in bytes.chunks(piece) {
                pieces.update(chunk);
            }
            assert_eq!(pieces.finish(), whole, "pieces of {piece}");
        }
        // A million a's, FIPS 180's long message.
        let mut sha = Sha256::new();
        for _ in 0..1000 {
            sha.update(&[b'a'; 1000]);
        }
        assert_eq!(
            hex(&sha.finish()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }
}

// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! PNG files for screenshots of real machines, written without a library.
//!
//! QEMU writes its own screenshots; a real machine's screen comes in as
//! pixels over VNC (see vnc.rs), and this writes them as a PNG: 8-bit RGB
//! (ISO/IEC 15948), each row filtered by its left neighbour, so that a
//! run of one colour becomes a run of zeros, then deflated (RFC 1951)
//! with the fixed Huffman codes and greedy matching, inside zlib
//! (RFC 1950). Screens are mostly runs of a few colours, which this
//! packs to a small part of their size.

use crate::screen::Image;

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

/// The Sub filter: each byte less the byte one pixel to its left.
const SUB: u8 = 1;

/// Writes `image` as a PNG file's bytes.
pub fn encode(image: &Image) -> Vec<u8> {
    let (width, height) = (image.width, image.height);
    let mut rows = Vec::with_capacity(height * (1 + width * 3));
    if width > 0 {
        for row in image.pixels().chunks(width) {
            rows.push(SUB);
            let mut left = [0u8; 3];
            for pixel in row {
                for (byte, before) in pixel.iter().zip(left) {
                    rows.push(byte.wrapping_sub(before));
                }
                left = *pixel;
            }
        }
    }
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&(width as u32).to_be_bytes());
    header.extend_from_slice(&(height as u32).to_be_bytes());
    // 8 bits a sample, RGB, deflate, adaptive filters, not interlaced.
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    let mut png = SIGNATURE.to_vec();
    chunk(&mut png, *b"IHDR", &header);
    chunk(&mut png, *b"IDAT", &zlib(&rows));
    chunk(&mut png, *b"IEND", &[]);
    png
}

/// Appends one chunk: its length, type, data and CRC.
fn chunk(png: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend_from_slice(&kind);
    png.extend_from_slice(data);
    let crc = crc32(&png[start..]);
    png.extend_from_slice(&crc.to_be_bytes());
}

/// The CRC-32 table (the polynomial PNG and zlib's gzip share).
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 == 1 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
};

fn crc32(bytes: &[u8]) -> u32 {
    !bytes
        .iter()
        .fold(!0u32, |crc, &byte| CRC_TABLE[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8))
}

/// zlib's Adler-32 of `bytes`.
fn adler32(bytes: &[u8]) -> u32 {
    const BASE: u32 = 65_521;
    let (mut a, mut b) = (1u32, 0u32);
    // The largest run that cannot overflow before the modulo.
    for run in bytes.chunks(5552) {
        for &byte in run {
            a += u32::from(byte);
            b += a;
        }
        a %= BASE;
        b %= BASE;
    }
    (b << 16) | a
}

/// `data` as a zlib stream: its header (deflate, a 32 KiB window, the
/// fastest level), one deflate block, and the Adler-32.
fn zlib(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    out.extend(deflate(data));
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// Bits written least significant first, as deflate packs them.
struct Bits {
    out: Vec<u8>,
    pending: u64,
    count: u32,
}

impl Bits {
    /// Writes the low `count` bits of `value`, the lowest first.
    fn put(&mut self, value: u32, count: u32) {
        self.pending |= u64::from(value) << self.count;
        self.count += count;
        while self.count >= 8 {
            self.out.push(self.pending as u8);
            self.pending >>= 8;
            self.count -= 8;
        }
    }

    /// Writes a Huffman code, which deflate packs most significant bit
    /// first.
    fn code(&mut self, code: u32, length: u32) {
        self.put(code.reverse_bits() >> (32 - length), length);
    }

    /// The fixed code of literal or length `symbol` (RFC 1951, 3.2.6).
    fn symbol(&mut self, symbol: u32) {
        match symbol {
            0..=143 => self.code(0x30 + symbol, 8),
            144..=255 => self.code(0x190 + symbol - 144, 9),
            256..=279 => self.code(symbol - 256, 7),
            _ => self.code(0xc0 + symbol - 280, 8),
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.count > 0 {
            self.out.push(self.pending as u8);
        }
        self.out
    }
}

const LENGTH_BASE: [u32; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u32; 29] =
    [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DISTANCE_BASE: [u32; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DISTANCE_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// How far back a match may reach, and how long one may be.
const WINDOW: usize = 32_768;
const LONGEST: usize = 258;
const SHORTEST: usize = 3;
/// How many earlier places with the same three bytes are tried.
const TRIES: usize = 16;
const HASH_BITS: u32 = 15;
const NONE: usize = usize::MAX;

/// Where three bytes at `at` go in the hash table.
fn hash(data: &[u8], at: usize) -> usize {
    let three = u32::from(data[at]) << 16 | u32::from(data[at + 1]) << 8 | u32::from(data[at + 2]);
    (three.wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize
}

/// `data` as one final deflate block with the fixed Huffman codes.
fn deflate(data: &[u8]) -> Vec<u8> {
    let mut bits = Bits { out: Vec::with_capacity(data.len() / 4), pending: 0, count: 0 };
    // BFINAL, then BTYPE 01: fixed codes.
    bits.put(1, 1);
    bits.put(1, 2);
    // The last place each hash of three bytes was seen, and for each
    // place in the window the place before it with the same hash.
    let mut head = vec![NONE; 1 << HASH_BITS];
    let mut before = vec![NONE; WINDOW];
    let insert = |at: usize, head: &mut [usize], before: &mut [usize]| {
        if at + SHORTEST <= data.len() {
            let key = hash(data, at);
            before[at % WINDOW] = head[key];
            head[key] = at;
        }
    };
    let mut at = 0;
    while at < data.len() {
        let (mut best, mut distance) = (0, 0);
        if at + SHORTEST <= data.len() {
            let longest = LONGEST.min(data.len() - at);
            let mut candidate = head[hash(data, at)];
            let mut tries = TRIES;
            while candidate != NONE && at - candidate <= WINDOW && tries > 0 {
                let length =
                    (0..longest).take_while(|&n| data[candidate + n] == data[at + n]).count();
                if length > best {
                    (best, distance) = (length, at - candidate);
                    if length == longest {
                        break;
                    }
                }
                // A place overwritten in the ring is newer, not older.
                let next = before[candidate % WINDOW];
                if next == NONE || next >= candidate {
                    break;
                }
                candidate = next;
                tries -= 1;
            }
        }
        if best >= SHORTEST {
            let code = LENGTH_BASE.iter().rposition(|&base| base as usize <= best).unwrap_or(0);
            bits.symbol(257 + code as u32);
            bits.put(best as u32 - LENGTH_BASE[code], LENGTH_EXTRA[code]);
            let far =
                DISTANCE_BASE.iter().rposition(|&base| base as usize <= distance).unwrap_or(0);
            bits.code(far as u32, 5);
            bits.put(distance as u32 - DISTANCE_BASE[far], DISTANCE_EXTRA[far]);
            for place in at..at + best {
                insert(place, &mut head, &mut before);
            }
            at += best;
        } else {
            bits.symbol(u32::from(data[at]));
            insert(at, &mut head, &mut before);
            at += 1;
        }
    }
    bits.symbol(256);
    bits.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads back a zlib stream of fixed-code deflate blocks, as this
    /// module writes them.
    fn inflate(zlib: &[u8]) -> Vec<u8> {
        assert_eq!((u32::from(zlib[0]) << 8 | u32::from(zlib[1])) % 31, 0, "zlib header check");
        let body = &zlib[2..zlib.len() - 4];
        let (mut at, mut out) = (0usize, Vec::new());
        let mut bit = || {
            let value = (body[at / 8] >> (at % 8)) & 1;
            at += 1;
            u32::from(value)
        };
        let bits = |count: u32, bit: &mut dyn FnMut() -> u32| {
            (0..count).fold(0, |value, n| value | bit() << n)
        };
        let code = |count: u32, bit: &mut dyn FnMut() -> u32| {
            (0..count).fold(0, |value, _| value << 1 | bit())
        };
        let last = bits(1, &mut bit);
        assert_eq!(last, 1);
        assert_eq!(bits(2, &mut bit), 1, "fixed codes");
        loop {
            let mut symbol = code(7, &mut bit);
            if symbol <= 0x17 {
                symbol += 256;
            } else {
                symbol = symbol << 1 | bit();
                if (0x30..=0xbf).contains(&symbol) {
                    symbol -= 0x30;
                } else if (0xc0..=0xc7).contains(&symbol) {
                    symbol = symbol - 0xc0 + 280;
                } else {
                    symbol = (symbol << 1 | bit()) - 0x190 + 144;
                }
            }
            match symbol {
                0..=255 => out.push(symbol as u8),
                256 => break,
                _ => {
                    let index = (symbol - 257) as usize;
                    let length = LENGTH_BASE[index] + bits(LENGTH_EXTRA[index], &mut bit);
                    let far = code(5, &mut bit) as usize;
                    let distance = DISTANCE_BASE[far] + bits(DISTANCE_EXTRA[far], &mut bit);
                    for _ in 0..length {
                        out.push(out[out.len() - distance as usize]);
                    }
                }
            }
        }
        assert_eq!(zlib[zlib.len() - 4..], adler32(&out).to_be_bytes());
        out
    }

    /// The chunks of a PNG file, each checked against its CRC.
    fn chunks(png: &[u8]) -> Vec<(String, Vec<u8>)> {
        assert_eq!(png[..8], SIGNATURE);
        let mut at = 8;
        let mut found = Vec::new();
        while at < png.len() {
            let length = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
            let kind_and_data = &png[at + 4..at + 8 + length];
            let crc =
                u32::from_be_bytes(png[at + 8 + length..at + 12 + length].try_into().unwrap());
            assert_eq!(crc32(kind_and_data), crc);
            let kind = String::from_utf8(kind_and_data[..4].to_vec()).unwrap();
            found.push((kind, kind_and_data[4..].to_vec()));
            at += 12 + length;
        }
        found
    }

    /// Undoes the Sub filter.
    fn unfilter(rows: &[u8], width: usize) -> Vec<[u8; 3]> {
        let mut pixels = Vec::new();
        for row in rows.chunks(1 + width * 3) {
            assert_eq!(row[0], SUB);
            let mut left = [0u8; 3];
            for bytes in row[1..].chunks(3) {
                let pixel = [
                    bytes[0].wrapping_add(left[0]),
                    bytes[1].wrapping_add(left[1]),
                    bytes[2].wrapping_add(left[2]),
                ];
                pixels.push(pixel);
                left = pixel;
            }
        }
        pixels
    }

    #[test]
    fn checksums_match_the_standards() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn a_screen_reads_back_as_it_was() {
        // A plain background with a box and a ramp: runs and noise both.
        let (width, height) = (200, 120);
        let pixels: Vec<[u8; 3]> = (0..width * height)
            .map(|n| {
                let (x, y) = (n % width, n / width);
                if (40..80).contains(&x) && (30..60).contains(&y) {
                    [0xff, 0xc0, 0x00]
                } else if y > 100 {
                    [(x * 7) as u8, (y * 13) as u8, (x * y) as u8]
                } else {
                    [0x1e, 0x1e, 0x2e]
                }
            })
            .collect();
        let image = Image::new(width, height, pixels.clone()).unwrap();
        let png = encode(&image);
        let found = chunks(&png);
        let kinds: Vec<&str> = found.iter().map(|(kind, _)| kind.as_str()).collect();
        assert_eq!(kinds, ["IHDR", "IDAT", "IEND"]);
        assert_eq!(found[0].1, [0, 0, 0, 200, 0, 0, 0, 120, 8, 2, 0, 0, 0]);
        let rows = inflate(&found[1].1);
        assert_eq!(unfilter(&rows, width), pixels);
        // Mostly runs, so much smaller than the 72,000 bytes of pixels.
        assert!(png.len() < 12_000, "{} bytes", png.len());
    }

    #[test]
    fn long_runs_and_far_matches_read_back() {
        let mut data = vec![7u8; 70_000];
        data.extend((0..40_000).map(|n| (n % 251) as u8));
        data.extend_from_within(1000..3000);
        assert_eq!(inflate(&zlib(&data)), data);
        assert_eq!(inflate(&zlib(&[])), Vec::<u8>::new());
        assert_eq!(inflate(&zlib(b"ab")), b"ab");
    }
}

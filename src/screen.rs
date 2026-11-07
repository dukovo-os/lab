// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Reading the text on a machine's screen, without OCR.
//!
//! Dukovo draws its console (the kernel's, then consoled's) in a bitmap
//! font of its own, cell by cell, and says on the serial console where the
//! cells are:
//!
//! ```text
//! consoled: console of 100x30 cells with font 12x24 at (40, 40)
//! ```
//!
//! So a screenshot cut into those cells reads back exactly: each cell's
//! lit pixels (those not in the cell's background colour) are looked up in
//! a table of the font's glyphs. This is what Dukovo's own boot tests do
//! (`tools/xtask/src/screen.rs` in its repository); the lab reads the same
//! fonts, which `ob lab` passes with `--fonts` (PSF files, as Linux's
//! console fonts are).
//!
//! A cell that matches no glyph reads as `?`: a cursor over a letter, a
//! picture, or a font the folder lacks.

use std::collections::HashMap;
use std::path::Path;

/// Where a console's cells are on the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub cols: usize,
    pub rows: usize,
    /// The font's glyph width and height, in pixels.
    pub font: (usize, usize),
    /// Where the top-left cell starts, in pixels.
    pub origin: (usize, usize),
}

impl Geometry {
    /// The last console the log describes, as the kernel and consoled
    /// write it: `console of <cols>x<rows> cells with font <w>x<h> at
    /// (<x>, <y>)`. A line still being written is left out.
    pub fn from_log(log: &str) -> Option<Geometry> {
        let whole = log.rfind('\n').map_or("", |end| &log[..end]);
        whole.lines().rev().find_map(Geometry::from_line)
    }

    fn from_line(line: &str) -> Option<Geometry> {
        let (_, rest) = line.split_once("console of ")?;
        let (grid, rest) = rest.split_once(" cells with font ")?;
        let (font, rest) = rest.split_once(" at (")?;
        let (x, rest) = rest.split_once(", ")?;
        let y = rest.split_once(')')?.0;
        let pair = |text: &str| -> Option<(usize, usize)> {
            let (a, b) = text.split_once('x')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        };
        let (cols, rows) = pair(grid)?;
        Some(Geometry {
            cols,
            rows,
            font: pair(font)?,
            origin: (x.trim().parse().ok()?, y.trim().parse().ok()?),
        })
    }
}

/// A screenshot, as QEMU's `screendump` writes one by default: a binary
/// PPM (P6) with 8-bit samples.
pub struct Image {
    pub width: usize,
    pub height: usize,
    /// RGB, row by row.
    pixels: Vec<[u8; 3]>,
}

impl Image {
    pub fn parse_ppm(bytes: &[u8]) -> Result<Image, String> {
        // Four fields (magic, width, height, the largest sample), split by
        // white space and comments, then one white space byte, then the
        // samples.
        let mut fields = Vec::new();
        let mut at = 0;
        while fields.len() < 4 {
            while at < bytes.len() && (bytes[at].is_ascii_whitespace() || bytes[at] == b'#') {
                if bytes[at] == b'#' {
                    while at < bytes.len() && bytes[at] != b'\n' {
                        at += 1;
                    }
                } else {
                    at += 1;
                }
            }
            let start = at;
            while at < bytes.len() && !bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            if start == at {
                return Err("the screenshot's PPM header is cut short".into());
            }
            fields.push(String::from_utf8_lossy(&bytes[start..at]).into_owned());
        }
        at += 1;
        if fields[0] != "P6" || fields[3] != "255" {
            return Err(format!(
                "the screenshot is not an 8-bit binary PPM ({} {})",
                fields[0], fields[3]
            ));
        }
        let number = |text: &str| {
            text.parse::<usize>()
                .map_err(|_| format!("the screenshot's size {text:?} is no number"))
        };
        let (width, height) = (number(&fields[1])?, number(&fields[2])?);
        let length = width.checked_mul(height).and_then(|n| n.checked_mul(3));
        let data = length
            .and_then(|length| bytes.get(at..at + length))
            .ok_or("the screenshot's pixels are cut short")?;
        let pixels = data.chunks_exact(3).map(|rgb| [rgb[0], rgb[1], rgb[2]]).collect();
        Ok(Image { width, height, pixels })
    }

    fn pixel(&self, x: usize, y: usize) -> [u8; 3] {
        self.pixels[y * self.width + x]
    }
}

/// One bitmap font: each glyph's lit pixels, row by row (bit `width - 1`
/// is the leftmost), and the characters it draws.
#[derive(Clone, Debug)]
pub struct Font {
    pub width: usize,
    pub height: usize,
    glyphs: HashMap<Vec<u32>, char>,
}

impl Font {
    /// Reads a PSF 2 font (Linux's console fonts and Dukovo's), or a PSF 1
    /// one, with the characters its Unicode table gives each glyph, or
    /// glyph N as character N when it has none.
    pub fn parse_psf(bytes: &[u8]) -> Result<Font, String> {
        let (width, height, row_bytes, glyph_bytes, count, glyphs_at, table) =
            if bytes.starts_with(&[0x72, 0xb5, 0x4a, 0x86]) {
                let word = |i: usize| -> Result<usize, String> {
                    let raw = bytes.get(4 * i..4 * i + 4).ok_or("cut short")?;
                    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize)
                };
                let (header, flags, count, glyph_bytes, height, width) =
                    (word(2)?, word(3)?, word(4)?, word(5)?, word(6)?, word(7)?);
                let row_bytes = width.div_ceil(8);
                (width, height, row_bytes, glyph_bytes, count, header, flags & 1 != 0)
            } else if bytes.starts_with(&[0x36, 0x04]) {
                let mode = *bytes.get(2).ok_or("cut short")?;
                let height = usize::from(*bytes.get(3).ok_or("cut short")?);
                let count = if mode & 0x01 != 0 { 512 } else { 256 };
                (8, height, 1, height, count, 4, mode & 0x06 != 0)
            } else {
                return Err("not a PSF font".into());
            };
        // Rows are kept in a u32, a bit a pixel.
        if width == 0
            || width > 32
            || height == 0
            || count == 0
            || glyph_bytes != row_bytes * height
        {
            return Err(format!("a PSF font of {width}x{height} the lab cannot read"));
        }
        let end = count
            .checked_mul(glyph_bytes)
            .and_then(|size| size.checked_add(glyphs_at))
            .filter(|end| *end <= bytes.len())
            .ok_or("its glyphs are cut short")?;
        let bitmap = |index: usize| -> Vec<u32> {
            let glyph = &bytes[glyphs_at + index * glyph_bytes..][..glyph_bytes];
            glyph
                .chunks_exact(row_bytes)
                .map(|row| {
                    let wide = row.iter().fold(0u64, |bits, byte| (bits << 8) | u64::from(*byte));
                    // Padding bits on the right of the last byte are not pixels.
                    (wide >> (row_bytes * 8 - width)) as u32
                })
                .collect()
        };
        // Each glyph's characters: from the table, or its own index.
        let mut characters: Vec<(char, usize)> = Vec::new();
        if table {
            let is_psf2 = bytes.starts_with(&[0x72, 0xb5, 0x4a, 0x86]);
            characters = unicode_table(&bytes[end..], count, is_psf2);
        } else {
            for index in 0..count {
                if let Some(c) = char::from_u32(index as u32) {
                    characters.push((c, index));
                }
            }
        }
        // Several characters may share a glyph (a space and a no-break
        // space, Latin O and Greek Omicron): the lowest wins, so text reads
        // back as ASCII where it can.
        characters.sort_by_key(|&(c, _)| std::cmp::Reverse(c));
        let mut glyphs = HashMap::new();
        for (c, index) in characters {
            glyphs.insert(bitmap(index), c);
        }
        Ok(Font { width, height, glyphs })
    }

    /// Reads every PSF font (`*.psf`, `*.psfu`, and either gzipped is not
    /// read) in `dir`.
    pub fn read_dir(dir: &Path) -> Result<Vec<Font>, String> {
        let entries = std::fs::read_dir(dir)
            .map_err(|err| format!("reading the console fonts in {}: {err}", dir.display()))?;
        let mut fonts = Vec::new();
        let mut paths: Vec<_> = entries.filter_map(|entry| entry.ok().map(|e| e.path())).collect();
        paths.sort();
        for path in paths {
            let psf = path.extension().is_some_and(|ext| ext == "psf" || ext == "psfu");
            if !psf {
                continue;
            }
            let bytes = std::fs::read(&path)
                .map_err(|err| format!("reading the font {}: {err}", path.display()))?;
            fonts.push(
                Font::parse_psf(&bytes)
                    .map_err(|err| format!("the font {}: {err}", path.display()))?,
            );
        }
        if fonts.is_empty() {
            return Err(format!("{} holds no PSF fonts (*.psf or *.psfu)", dir.display()));
        }
        Ok(fonts)
    }
}

/// The characters of each glyph from a PSF font's Unicode table: in PSF 2,
/// each glyph's UTF-8 characters end with 0xFF, and 0xFE starts sequences
/// (several characters drawn as one glyph), which are left out; in PSF 1,
/// the same with 16-bit characters, 0xFFFF and 0xFFFE.
fn unicode_table(table: &[u8], count: usize, psf2: bool) -> Vec<(char, usize)> {
    let mut characters = Vec::new();
    let mut at = 0;
    for index in 0..count {
        let mut in_sequence = false;
        if psf2 {
            while at < table.len() && table[at] != 0xff {
                if table[at] == 0xfe {
                    in_sequence = true;
                    at += 1;
                    continue;
                }
                // One UTF-8 character: its length from its first byte.
                let length = match table[at] {
                    0x00..=0x7f => 1,
                    0xc0..=0xdf => 2,
                    0xe0..=0xef => 3,
                    _ => 4,
                };
                let end = (at + length).min(table.len());
                if !in_sequence
                    && let Ok(text) = std::str::from_utf8(&table[at..end])
                    && let Some(c) = text.chars().next()
                {
                    characters.push((c, index));
                }
                at = end;
            }
            at += 1;
        } else {
            while at + 1 < table.len() {
                let unit = u16::from_le_bytes([table[at], table[at + 1]]);
                at += 2;
                match unit {
                    0xffff => break,
                    0xfffe => in_sequence = true,
                    _ if !in_sequence => {
                        if let Some(c) = char::from_u32(u32::from(unit)) {
                            characters.push((c, index));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    characters
}

/// Reads the console's text from a screenshot: a string a row, with blanks
/// at the end left off.
pub fn read_text(image: &Image, geometry: Geometry, fonts: &[Font]) -> Result<Vec<String>, String> {
    let (width, height) = geometry.font;
    let font = fonts
        .iter()
        .find(|font| font.width == width && font.height == height)
        .ok_or(format!("the console fonts hold none of {width}x{height}, the console's"))?;
    let (x0, y0) = geometry.origin;
    if x0 + geometry.cols * width > image.width || y0 + geometry.rows * height > image.height {
        return Err(format!(
            "the console ({}x{} cells of {width}x{height} at {x0},{y0}) does not fit the {}x{} screen",
            geometry.cols, geometry.rows, image.width, image.height
        ));
    }
    let mut rows = Vec::with_capacity(geometry.rows);
    for row in 0..geometry.rows {
        let mut text = String::with_capacity(geometry.cols);
        for col in 0..geometry.cols {
            let bits = cell_bits(image, x0 + col * width, y0 + row * height, width, height);
            text.push(*font.glyphs.get(&bits).unwrap_or(&'?'));
        }
        rows.push(text.trim_end().to_string());
    }
    Ok(rows)
}

/// The pixels of one cell not in its background, which is the colour most
/// of its pixels have: a glyph lights far fewer than half.
fn cell_bits(image: &Image, x0: usize, y0: usize, width: usize, height: usize) -> Vec<u32> {
    let mut counts: HashMap<[u8; 3], usize> = HashMap::new();
    for y in y0..y0 + height {
        for x in x0..x0 + width {
            *counts.entry(image.pixel(x, y)).or_default() += 1;
        }
    }
    let background = counts.into_iter().max_by_key(|&(_, n)| n).map(|(c, _)| c).unwrap_or_default();
    (y0..y0 + height)
        .map(|y| {
            (0..width)
                .fold(0u32, |bits, x| (bits << 1) | u32::from(image.pixel(x0 + x, y) != background))
        })
        .collect()
}

/// Whether the screen shows every one of `texts`, each within one row.
pub fn shows(screen: &[String], texts: &[String]) -> bool {
    texts.iter().all(|text| screen.iter().any(|row| row.contains(text.as_str())))
}

/// The screen's rows as one text, for a step's note and the bundle, with
/// the blank rows at the end left off.
pub fn as_text(screen: &[String]) -> String {
    let used = screen.iter().rposition(|row| !row.is_empty()).map_or(0, |last| last + 1);
    let mut text = screen[..used].join("\n");
    text.push('\n');
    text
}

/// The QEMU key combinations that type `text` on a US keyboard, a key a
/// character: `\n` is Enter, `\t` Tab. Characters that keyboard has no key
/// for are refused, naming the first.
pub fn key_combos(text: &str) -> Result<Vec<String>, String> {
    // Keys that type a character unshifted, and the shifted characters on
    // the same keys.
    const PLAIN: [(char, &str); 14] = [
        (' ', "spc"),
        ('\n', "ret"),
        ('\t', "tab"),
        ('-', "minus"),
        ('=', "equal"),
        ('[', "bracket_left"),
        (']', "bracket_right"),
        (';', "semicolon"),
        ('\'', "apostrophe"),
        ('`', "grave_accent"),
        ('\\', "backslash"),
        (',', "comma"),
        ('.', "dot"),
        ('/', "slash"),
    ];
    const SHIFTED: [(char, &str); 21] = [
        ('!', "1"),
        ('@', "2"),
        ('#', "3"),
        ('$', "4"),
        ('%', "5"),
        ('^', "6"),
        ('&', "7"),
        ('*', "8"),
        ('(', "9"),
        (')', "0"),
        ('_', "minus"),
        ('+', "equal"),
        ('{', "bracket_left"),
        ('}', "bracket_right"),
        (':', "semicolon"),
        ('"', "apostrophe"),
        ('~', "grave_accent"),
        ('|', "backslash"),
        ('<', "comma"),
        ('>', "dot"),
        ('?', "slash"),
    ];
    text.chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                Ok(c.to_string())
            } else if c.is_ascii_uppercase() {
                Ok(format!("shift-{}", c.to_ascii_lowercase()))
            } else if let Some((_, key)) = PLAIN.iter().find(|(plain, _)| *plain == c) {
                Ok((*key).to_string())
            } else if let Some((_, key)) = SHIFTED.iter().find(|(shifted, _)| *shifted == c) {
                Ok(format!("shift-{key}"))
            } else {
                Err(format!("a US keyboard has no key for {c:?}"))
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A PSF 2 font of 4x6 glyphs for tests, with a Unicode table: glyph 0
    /// is blank (space and no-break space), and each other glyph draws its
    /// character's code in binary, four bits a row, so each is different.
    pub(crate) fn test_font(chars: &str) -> Vec<u8> {
        let all: Vec<char> = std::iter::once(' ').chain(chars.chars()).collect();
        let mut bytes = Vec::new();
        for word in [0x864a_b572u32, 0, 32, 1, all.len() as u32, 6, 6, 4] {
            bytes.extend(word.to_le_bytes());
        }
        for (index, c) in all.iter().enumerate() {
            let code = *c as u32;
            for row in 0..6u32 {
                // A 4-pixel row in the top nibble of its byte.
                let bits = if index == 0 { 0 } else { (code >> (row * 4)) & 0xf };
                bytes.push((bits as u8) << 4);
            }
        }
        for (index, c) in all.iter().enumerate() {
            let mut buf = [0; 4];
            bytes.extend(c.encode_utf8(&mut buf).as_bytes());
            if index == 0 {
                bytes.extend("\u{a0}".as_bytes());
            }
            bytes.push(0xff);
        }
        bytes
    }

    /// Draws `rows` in `font` at `origin`, on a background, as a PPM.
    pub(crate) fn draw(
        font: &[u8],
        rows: &[&str],
        size: (usize, usize),
        origin: (usize, usize),
    ) -> Vec<u8> {
        let parsed = Font::parse_psf(font).unwrap();
        let by_char: HashMap<char, Vec<u32>> =
            parsed.glyphs.iter().map(|(bits, c)| (*c, bits.clone())).collect();
        let (width, height) = size;
        let mut pixels = vec![[20u8, 30, 60]; width * height];
        for (row, line) in rows.iter().enumerate() {
            for (col, c) in line.chars().enumerate() {
                let bits = &by_char[&c];
                for (y, row_bits) in bits.iter().enumerate() {
                    for x in 0..parsed.width {
                        if row_bits >> (parsed.width - 1 - x) & 1 == 1 {
                            let px = origin.0 + col * parsed.width + x;
                            let py = origin.1 + row * parsed.height + y;
                            pixels[py * width + px] = [240, 230, 200];
                        }
                    }
                }
            }
        }
        let mut out = format!("P6\n# lab test\n{width} {height}\n255\n").into_bytes();
        out.extend(pixels.iter().flatten());
        out
    }

    #[test]
    fn finds_the_last_console_in_the_log() {
        let log = "[    0.000000] Framebuffer: console of 160x50 cells with font 8x16 at (0, 0)\n\
                   [    4.1] spark[1]: consoled: console of 100x30 cells with font 12x24 at (40, 40)\n\
                   consoled: console of 1x1 cells with font 8x16 at (0,";
        assert_eq!(
            Geometry::from_log(log),
            Some(Geometry { cols: 100, rows: 30, font: (12, 24), origin: (40, 40) })
        );
        assert_eq!(Geometry::from_log("no console here\n"), None);
    }

    #[test]
    fn reads_back_text_drawn_in_the_font() {
        let font = test_font("ABCabc:$#?√");
        let fonts = vec![Font::parse_psf(&font).unwrap()];
        let ppm = draw(&font, &["Abc: $ ab", "", "√ CAB#"], (60, 40), (3, 5));
        let image = Image::parse_ppm(&ppm).unwrap();
        let geometry = Geometry { cols: 12, rows: 4, font: (4, 6), origin: (3, 5) };
        let screen = read_text(&image, geometry, &fonts).unwrap();
        assert_eq!(screen, ["Abc: $ ab", "", "√ CAB#", ""]);
        assert_eq!(as_text(&screen), "Abc: $ ab\n\n√ CAB#\n");
        assert!(shows(&screen, &["CAB".into(), "Abc:".into()]));
        assert!(!shows(&screen, &["CAB".into(), "abc".into()]));

        // A font the folder lacks, and a console that does not fit, say so.
        let other = Geometry { font: (8, 16), ..geometry };
        assert!(read_text(&image, other, &fonts).unwrap_err().contains("8x16"));
        let wide = Geometry { cols: 40, ..geometry };
        assert!(read_text(&image, wide, &fonts).unwrap_err().contains("does not fit"));
    }

    #[test]
    fn reads_the_fonts_of_a_folder() {
        let dir = crate::testing::TempDir::new("fonts");
        assert!(Font::read_dir(dir.path()).unwrap_err().contains("no PSF fonts"));
        std::fs::write(dir.path().join("small.psfu"), test_font("ab")).unwrap();
        dir.write("README", "not a font");
        let fonts = Font::read_dir(dir.path()).unwrap();
        assert_eq!((fonts.len(), fonts[0].width, fonts[0].height), (1, 4, 6));
        dir.write("broken.psf", "PSF?");
        assert!(Font::read_dir(dir.path()).unwrap_err().contains("broken.psf"));
    }

    #[test]
    fn types_text_on_a_us_keyboard() {
        assert_eq!(key_combos("aZ 1!\n").unwrap(), ["a", "shift-z", "spc", "1", "shift-1", "ret"]);
        assert_eq!(key_combos("a/b?").unwrap(), ["a", "slash", "b", "shift-slash"]);
        assert_eq!(key_combos("é").unwrap_err(), "a US keyboard has no key for 'é'");
    }
}

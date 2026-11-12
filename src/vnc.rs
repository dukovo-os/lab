// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A VNC client: just enough of RFB (RFC 6143) to see a real machine's
//! screen and use its keyboard and mouse through Intel AMT's remote
//! screen (KVM), which AMT serves on the standard VNC port, 5900.
//!
//! - **Signing in** is VNC's: the server's challenge encrypted with DES
//!   under the password (des.rs). AMT names its protocol version 4.0 and
//!   speaks 3.8, so any version from 3.8 up is spoken as 3.8.
//! - **The screen** comes as raw pixels, in the server's own pixel format
//!   when it is true colour (every server takes its own), and is read
//!   into an [`Image`]. A server that changes the screen's size says so
//!   with the DesktopSize pseudo-encoding, and the client asks again.
//! - **Keys** are X keysyms, as RFB has them: [`keysyms`] turns QEMU's
//!   key names, which the lab's tests use, into them, US layout.
//! - **The pointer** is absolute, in the screen's pixels.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use crate::des;
use crate::screen::Image;

/// AMT's port for a standard VNC client.
pub const PORT: u16 = 5900;

/// How long one message may take to arrive whole: a raw screen is a few
/// megabytes, and AMT is no fast server.
const MESSAGE_TIME: Duration = Duration::from_secs(60);

/// How long one connection attempt may take.
const CONNECT_TIME: Duration = Duration::from_secs(5);

/// Encodings: raw pixels, and the pseudo-encoding that says the screen
/// has a new size.
const RAW: i32 = 0;
const DESKTOP_SIZE: i32 = -223;

/// Security types: none, and VNC's password sign-in.
const NO_SIGN_IN: u8 = 1;
const PASSWORD: u8 = 2;

/// The longest cut text or reason a server may send.
const TEXT_LIMIT: u32 = 1 << 20;

/// How the server sends each pixel (RFC 6143, 7.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelFormat {
    pub bits: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_colour: bool,
    /// Red, green and blue's largest values, and where they sit.
    pub max: [u16; 3],
    pub shift: [u8; 3],
}

impl PixelFormat {
    /// 32 bits a pixel, 8 a colour, blue lowest: what this client asks
    /// for when the server's own format is not true colour.
    pub const RGB32: PixelFormat = PixelFormat {
        bits: 32,
        depth: 24,
        big_endian: false,
        true_colour: true,
        max: [255, 255, 255],
        shift: [16, 8, 0],
    };

    fn parse(bytes: &[u8]) -> PixelFormat {
        let max = |at: usize| u16::from_be_bytes([bytes[at], bytes[at + 1]]);
        PixelFormat {
            bits: bytes[0],
            depth: bytes[1],
            big_endian: bytes[2] != 0,
            true_colour: bytes[3] != 0,
            max: [max(4), max(6), max(8)],
            shift: [bytes[10], bytes[11], bytes[12]],
        }
    }

    fn bytes(self) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        bytes[0] = self.bits;
        bytes[1] = self.depth;
        bytes[2] = u8::from(self.big_endian);
        bytes[3] = u8::from(self.true_colour);
        for (n, max) in self.max.iter().enumerate() {
            bytes[4 + 2 * n..6 + 2 * n].copy_from_slice(&max.to_be_bytes());
        }
        bytes[10..13].copy_from_slice(&self.shift);
        bytes
    }

    /// Whether this client can read pixels in this format.
    fn readable(self) -> bool {
        self.true_colour
            && matches!(self.bits, 8 | 16 | 32)
            && self.max.iter().all(|&max| max > 0)
            && self.shift.iter().all(|&shift| u32::from(shift) < u32::from(self.bits))
    }

    fn bytes_per_pixel(self) -> usize {
        usize::from(self.bits / 8)
    }

    /// The colour of one pixel's bytes.
    pub fn rgb(self, raw: &[u8]) -> [u8; 3] {
        let value = if self.big_endian {
            raw.iter().fold(0u32, |value, &byte| value << 8 | u32::from(byte))
        } else {
            raw.iter().rev().fold(0u32, |value, &byte| value << 8 | u32::from(byte))
        };
        let mut rgb = [0u8; 3];
        for (n, colour) in rgb.iter_mut().enumerate() {
            let max = u32::from(self.max[n]);
            let level = (value >> self.shift[n]) & max;
            *colour = ((level * 255 + max / 2) / max) as u8;
        }
        rgb
    }

    /// One pixel's bytes for a colour, as a server sends it.
    #[cfg(test)]
    pub fn encode(self, rgb: [u8; 3]) -> Vec<u8> {
        let mut value = 0u32;
        for (n, colour) in rgb.iter().enumerate() {
            let max = u32::from(self.max[n]);
            value |= ((u32::from(*colour) * max + 127) / 255) << self.shift[n];
        }
        let bytes = value.to_be_bytes();
        let mine = &bytes[4 - self.bytes_per_pixel()..];
        if self.big_endian { mine.to_vec() } else { mine.iter().rev().copied().collect() }
    }
}

/// The DES response to a VNC challenge: the password, cut or padded to
/// eight bytes, each byte's bits reversed, is the key for each half.
pub fn password_response(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (slot, byte) in key.iter_mut().zip(password.bytes()) {
        *slot = byte.reverse_bits();
    }
    let mut response = [0u8; 16];
    for (half, out) in challenge.chunks_exact(8).zip(response.chunks_exact_mut(8)) {
        let block: [u8; 8] = half.try_into().unwrap_or_default();
        out.copy_from_slice(&des::encrypt(key, block));
    }
    response
}

/// Why a connection attempt failed: worth another try (the port is not
/// open yet, or AMT is still closing the last session), or not.
enum Failed {
    Again(String),
    Stop(String),
}

/// The screen as it comes in: its pixels, and which have come.
struct Canvas {
    width: usize,
    height: usize,
    pixels: Vec<[u8; 3]>,
    came: Vec<bool>,
    missing: usize,
}

impl Canvas {
    fn new(width: u16, height: u16) -> Canvas {
        let (width, height) = (usize::from(width), usize::from(height));
        Canvas {
            width,
            height,
            pixels: vec![[0; 3]; width * height],
            came: vec![false; width * height],
            missing: width * height,
        }
    }
}

/// What one server message was.
enum Message {
    /// A FramebufferUpdate, and whether it gave the screen a new size.
    Update {
        resized: bool,
    },
    Other,
}

/// A session with a VNC server.
pub struct Vnc {
    stream: TcpStream,
    width: u16,
    height: u16,
    format: PixelFormat,
    /// The server's name for its screen.
    pub name: String,
}

impl Vnc {
    /// Signs in to the server at `address` with `password`, trying again
    /// until `deadline` while the port is closed or the server turns the
    /// session away; a wrong password fails at once.
    pub fn connect(address: &str, password: &str, deadline: Instant) -> Result<Vnc, String> {
        loop {
            match Vnc::try_connect(address, password) {
                Ok(vnc) => return Ok(vnc),
                Err(Failed::Stop(why)) => return Err(why),
                Err(Failed::Again(why)) if Instant::now() >= deadline => return Err(why),
                Err(Failed::Again(_)) => std::thread::sleep(Duration::from_millis(500)),
            }
        }
    }

    fn try_connect(address: &str, password: &str) -> Result<Vnc, Failed> {
        let again =
            |err: std::io::Error| Failed::Again(format!("the remote screen at {address}: {err}"));
        let target = address
            .to_socket_addrs()
            .map_err(|err| Failed::Stop(format!("the remote screen's address {address}: {err}")))?
            .next()
            .ok_or_else(|| {
                Failed::Stop(format!("the remote screen's address {address} is no address"))
            })?;
        let stream = TcpStream::connect_timeout(&target, CONNECT_TIME).map_err(again)?;
        stream.set_read_timeout(Some(MESSAGE_TIME)).map_err(again)?;
        stream.set_write_timeout(Some(MESSAGE_TIME)).map_err(again)?;
        stream.set_nodelay(true).map_err(again)?;
        let mut vnc =
            Vnc { stream, width: 0, height: 0, format: PixelFormat::RGB32, name: String::new() };
        vnc.sign_in(password).map_err(|failed| match failed {
            Failed::Again(why) => Failed::Again(format!("the remote screen at {address}: {why}")),
            Failed::Stop(why) => Failed::Stop(format!("the remote screen at {address}: {why}")),
        })?;
        Ok(vnc)
    }

    /// The handshake, sign-in and initialisation (RFC 6143, 7.1 to 7.3).
    fn sign_in(&mut self, password: &str) -> Result<(), Failed> {
        let again = Failed::Again;
        let version = self.read(12).map_err(again)?;
        let minor = parse_version(&version).ok_or_else(|| {
            Failed::Stop(format!(
                "it does not speak RFB: it began {:?}",
                String::from_utf8_lossy(&version)
            ))
        })?;
        self.send(format!("RFB 003.{minor:03}\n").as_bytes()).map_err(again)?;
        let kind = if minor == 3 {
            // 3.3: the server picks.
            let kind = self.read_u32().map_err(again)?;
            if kind == 0 {
                return Err(Failed::Again(format!(
                    "it turned the session away: {}",
                    self.reason()?
                )));
            }
            u8::try_from(kind).unwrap_or(0)
        } else {
            let count = self.read(1).map_err(again)?[0];
            if count == 0 {
                return Err(Failed::Again(format!(
                    "it turned the session away: {}",
                    self.reason()?
                )));
            }
            let kinds = self.read(usize::from(count)).map_err(again)?;
            let kind = [PASSWORD, NO_SIGN_IN]
                .into_iter()
                .find(|kind| kinds.contains(kind))
                .ok_or_else(|| {
                    Failed::Stop(format!(
                        "it offers no sign-in this client knows (security types {kinds:?})"
                    ))
                })?;
            self.send(&[kind]).map_err(again)?;
            kind
        };
        match kind {
            PASSWORD => {
                let challenge: [u8; 16] =
                    self.read(16).map_err(again)?.try_into().unwrap_or_default();
                self.send(&password_response(password, &challenge)).map_err(again)?;
            }
            NO_SIGN_IN => {}
            other => {
                return Err(Failed::Stop(format!(
                    "it asks for security type {other}, which this client does not know"
                )));
            }
        }
        if kind == PASSWORD || minor == 8 {
            let result = self.read_u32().map_err(again)?;
            if result != 0 {
                let reason =
                    if minor == 8 { format!(" ({})", self.reason()?) } else { String::new() };
                return Err(Failed::Stop(format!("it did not take the password{reason}")));
            }
        }
        // Shared: AMT has one session at a time whatever this says.
        self.send(&[1]).map_err(again)?;
        let init = self.read(24).map_err(again)?;
        self.width = u16::from_be_bytes([init[0], init[1]]);
        self.height = u16::from_be_bytes([init[2], init[3]]);
        let format = PixelFormat::parse(&init[4..20]);
        let name_length = u32::from_be_bytes([init[20], init[21], init[22], init[23]]);
        if name_length > TEXT_LIMIT {
            return Err(Failed::Stop(format!("its name is {name_length} bytes long")));
        }
        self.name =
            String::from_utf8_lossy(&self.read(name_length as usize).map_err(again)?).into_owned();
        self.format = if format.readable() {
            format
        } else {
            let mut message = vec![0, 0, 0, 0];
            message.extend_from_slice(&PixelFormat::RGB32.bytes());
            self.send(&message).map_err(again)?;
            PixelFormat::RGB32
        };
        let mut encodings = vec![2, 0, 0, 2];
        for encoding in [RAW, DESKTOP_SIZE] {
            encodings.extend_from_slice(&encoding.to_be_bytes());
        }
        self.send(&encodings).map_err(again)
    }

    /// Reads a reason string: a length, then the text.
    fn reason(&mut self) -> Result<String, Failed> {
        let length = self.read_u32().map_err(Failed::Again)?;
        if length > TEXT_LIMIT {
            return Err(Failed::Stop(format!("its reason is {length} bytes long")));
        }
        let text = self.read(length as usize).map_err(Failed::Again)?;
        Ok(String::from_utf8_lossy(&text).into_owned())
    }

    fn read(&mut self, length: usize) -> Result<Vec<u8>, String> {
        let mut bytes = vec![0u8; length];
        self.stream.read_exact(&mut bytes).map_err(|err| match err.kind() {
            ErrorKind::UnexpectedEof => "it closed the session".to_string(),
            ErrorKind::WouldBlock | ErrorKind::TimedOut => {
                format!("it sent nothing for {} seconds", MESSAGE_TIME.as_secs())
            }
            _ => err.to_string(),
        })?;
        Ok(bytes)
    }

    fn read_u32(&mut self) -> Result<u32, String> {
        let bytes = self.read(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.stream.write_all(bytes).map_err(|err| format!("sending to the remote screen: {err}"))
    }

    /// The screen's size in pixels, as the server said last.
    pub fn size(&self) -> (u32, u32) {
        (u32::from(self.width), u32::from(self.height))
    }

    /// Presses (`down`) or lets go of the key with X keysym `keysym`.
    pub fn key(&mut self, keysym: u32, down: bool) -> Result<(), String> {
        let mut message = vec![4, u8::from(down), 0, 0];
        message.extend_from_slice(&keysym.to_be_bytes());
        self.send(&message)
    }

    /// Puts the pointer at (`x`, `y`) with `buttons` held: bit 0 the left
    /// button, 1 the middle, 2 the right.
    pub fn pointer(&mut self, buttons: u8, x: u16, y: u16) -> Result<(), String> {
        let mut message = vec![5, buttons];
        message.extend_from_slice(&x.to_be_bytes());
        message.extend_from_slice(&y.to_be_bytes());
        self.send(&message)
    }

    /// Asks for the whole screen, and reads it as it comes, by `deadline`.
    pub fn capture(&mut self, deadline: Instant) -> Result<Image, String> {
        let mut canvas = Canvas::new(self.width, self.height);
        self.request()?;
        loop {
            if Instant::now() >= deadline {
                return Err(format!(
                    "the remote screen sent {} of its {} pixels in time",
                    canvas.pixels.len() - canvas.missing,
                    canvas.pixels.len()
                ));
            }
            if let Message::Update { resized } = self.read_message(&mut canvas)? {
                if resized {
                    self.request()?;
                } else if canvas.missing == 0 {
                    return Image::new(canvas.width, canvas.height, canvas.pixels);
                }
            }
        }
    }

    /// A FramebufferUpdateRequest for the whole screen, not incremental.
    fn request(&mut self) -> Result<(), String> {
        let mut message = vec![3, 0, 0, 0, 0, 0];
        message.extend_from_slice(&self.width.to_be_bytes());
        message.extend_from_slice(&self.height.to_be_bytes());
        self.send(&message)
    }

    /// Reads one message from the server, drawing what it updates on
    /// `canvas` (a new one when the screen's size changes).
    fn read_message(&mut self, canvas: &mut Canvas) -> Result<Message, String> {
        let kind = self.read(1)?[0];
        match kind {
            0 => {
                let count = self.read(3).map(|head| u16::from_be_bytes([head[1], head[2]]))?;
                let mut resized = false;
                for _ in 0..count {
                    let head = self.read(12)?;
                    let field =
                        |at: usize| usize::from(u16::from_be_bytes([head[at], head[at + 1]]));
                    let (x, y, width, height) = (field(0), field(2), field(4), field(6));
                    let encoding = i32::from_be_bytes([head[8], head[9], head[10], head[11]]);
                    match encoding {
                        RAW => self.read_raw(canvas, x, y, width, height)?,
                        DESKTOP_SIZE => {
                            self.width = width as u16;
                            self.height = height as u16;
                            *canvas = Canvas::new(self.width, self.height);
                            resized = true;
                        }
                        other => {
                            return Err(format!(
                                "the remote screen sent encoding {other}, which was not asked for"
                            ));
                        }
                    }
                }
                Ok(Message::Update { resized })
            }
            // SetColourMapEntries: true colour has no use for them.
            1 => {
                let head = self.read(5)?;
                let count = u16::from_be_bytes([head[3], head[4]]);
                self.read(usize::from(count) * 6).map(|_| Message::Other)
            }
            // Bell.
            2 => Ok(Message::Other),
            // ServerCutText.
            3 => {
                self.read(3)?;
                let length = self.read_u32()?;
                if length > TEXT_LIMIT {
                    return Err(format!("the remote screen sent {length} bytes of cut text"));
                }
                self.read(length as usize).map(|_| Message::Other)
            }
            other => Err(format!("the remote screen sent message type {other}, which RFB has not")),
        }
    }

    /// Reads a raw rectangle onto `canvas`.
    fn read_raw(
        &mut self,
        canvas: &mut Canvas,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
    ) -> Result<(), String> {
        if x + width > canvas.width || y + height > canvas.height {
            return Err(format!(
                "the remote screen sent {width}x{height} pixels at ({x}, {y}), off its {}x{} screen",
                canvas.width, canvas.height
            ));
        }
        let size = self.format.bytes_per_pixel();
        let bytes = self.read(width * height * size)?;
        for (row, line) in bytes.chunks_exact(width.max(1) * size).enumerate() {
            for (column, raw) in line.chunks_exact(size).enumerate() {
                let at = (y + row) * canvas.width + x + column;
                canvas.pixels[at] = self.format.rgb(raw);
                if !canvas.came[at] {
                    canvas.came[at] = true;
                    canvas.missing -= 1;
                }
            }
        }
        Ok(())
    }

    /// Whether the session is still open: reads whatever the server sent
    /// unasked (a bell, cut text), and is false once it has closed.
    pub fn is_open(&mut self) -> bool {
        loop {
            if self.stream.set_nonblocking(true).is_err() {
                return false;
            }
            let peeked = self.stream.peek(&mut [0u8; 1]);
            if self.stream.set_nonblocking(false).is_err() {
                return false;
            }
            match peeked {
                Ok(0) => return false,
                Ok(_) => {
                    let mut scratch = Canvas::new(self.width, self.height);
                    if self.read_message(&mut scratch).is_err() {
                        return false;
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => return true,
                Err(_) => return false,
            }
        }
    }
}

/// The minor version this client speaks to a server that sent `version`:
/// 8 from 3.8 up (AMT says 4.0), 7 for 3.7, else 3.
fn parse_version(version: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(version).ok()?;
    let numbers = text.strip_prefix("RFB ")?.strip_suffix('\n')?;
    let (major, minor) = numbers.split_once('.')?;
    let (major, minor): (u32, u32) = (major.parse().ok()?, minor.parse().ok()?);
    match (major, minor) {
        (3, 7) => Some(7),
        (3, 8..) | (4.., _) => Some(8),
        (3, _) => Some(3),
        _ => None,
    }
}

/// Keys that type a character, by QEMU's name: the character alone, and
/// with shift held.
const PRINTING: [(&str, char, char); 12] = [
    ("spc", ' ', ' '),
    ("minus", '-', '_'),
    ("equal", '=', '+'),
    ("bracket_left", '[', '{'),
    ("bracket_right", ']', '}'),
    ("semicolon", ';', ':'),
    ("apostrophe", '\'', '"'),
    ("grave_accent", '`', '~'),
    ("backslash", '\\', '|'),
    ("comma", ',', '<'),
    ("dot", '.', '>'),
    ("slash", '/', '?'),
];

/// The other keys, by QEMU's name, with their X keysyms.
const OTHER_KEYS: [(&str, u32); 35] = [
    ("ret", 0xff0d),
    ("tab", 0xff09),
    ("backspace", 0xff08),
    ("esc", 0xff1b),
    ("delete", 0xffff),
    ("insert", 0xff63),
    ("home", 0xff50),
    ("end", 0xff57),
    ("pgup", 0xff55),
    ("pgdn", 0xff56),
    ("left", 0xff51),
    ("up", 0xff52),
    ("right", 0xff53),
    ("down", 0xff54),
    ("shift", 0xffe1),
    ("shift_r", 0xffe2),
    ("ctrl", 0xffe3),
    ("ctrl_r", 0xffe4),
    ("alt", 0xffe9),
    ("alt_r", 0xffea),
    ("meta_l", 0xffeb),
    ("meta_r", 0xffec),
    ("caps_lock", 0xffe5),
    ("num_lock", 0xff7f),
    ("scroll_lock", 0xff14),
    ("print", 0xff61),
    ("sysrq", 0xff15),
    ("pause", 0xff13),
    ("menu", 0xff67),
    ("kp_enter", 0xff8d),
    ("kp_add", 0xffab),
    ("kp_subtract", 0xffad),
    ("kp_multiply", 0xffaa),
    ("kp_divide", 0xffaf),
    ("kp_decimal", 0xffae),
];

/// The characters shift turns the digits 0 to 9 into, US layout.
const SHIFTED_DIGITS: [char; 10] = [')', '!', '@', '#', '$', '%', '^', '&', '*', '('];

/// The X keysym of the key QEMU calls `name`. A key that types a
/// character gives the character it types, with shift held or not, as an
/// X client sends it.
fn keysym(name: &str, shift: bool) -> Option<u32> {
    let pick = |plain: char, shifted: char| Some(u32::from(if shift { shifted } else { plain }));
    if let [byte] = name.as_bytes() {
        let c = char::from(*byte);
        if c.is_ascii_lowercase() {
            return pick(c, c.to_ascii_uppercase());
        }
        if let Some(digit) = c.to_digit(10) {
            return pick(c, SHIFTED_DIGITS[digit as usize]);
        }
    }
    if let Some(&(_, plain, shifted)) = PRINTING.iter().find(|(key, ..)| *key == name) {
        return pick(plain, shifted);
    }
    if let Some(&(_, keysym)) = OTHER_KEYS.iter().find(|(key, _)| *key == name) {
        return Some(keysym);
    }
    let number = |prefix: &str| name.strip_prefix(prefix).and_then(|n| n.parse::<u32>().ok());
    match (number("f"), number("kp_")) {
        (Some(n @ 1..=12), _) => Some(0xffbe + n - 1),
        (_, Some(n @ 0..=9)) => Some(0xffb0 + n),
        _ => None,
    }
}

/// The keysyms of a key combination such as `ctrl-alt-delete`, in the
/// order they are pressed.
pub fn keysyms(combo: &str) -> Result<Vec<u32>, String> {
    let keys: Vec<&str> = combo.split('-').collect();
    let shift = keys.iter().any(|key| matches!(*key, "shift" | "shift_r"));
    keys.iter()
        .map(|key| {
            keysym(key, shift).ok_or_else(|| {
                format!("the remote screen has no key named \"{key}\" (in \"{combo}\")")
            })
        })
        .collect()
}

/// A pretend VNC server for tests, as AMT's remote screen behaves: one
/// screen, a password, and the keys and pointer it is sent.
#[cfg(test)]
pub mod fake {
    use std::io::{Read, Write};
    use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use super::{DESKTOP_SIZE, PixelFormat, RAW, password_response};

    /// What the pretend machine was sent.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Event {
        Key(u32, bool),
        Pointer(u8, u16, u16),
    }

    /// Where the password comes from: none while the port is closed.
    pub type Password = Box<dyn Fn() -> Option<String> + Send>;

    pub struct State {
        pub version: &'static [u8; 12],
        pub format: PixelFormat,
        pub width: u16,
        pub height: u16,
        pub pixels: Vec<[u8; 3]>,
        /// A new screen, given at the next request.
        pub resize: Option<(u16, u16, Vec<[u8; 3]>)>,
        pub events: Vec<Event>,
        pub sessions: u32,
        /// Sessions open now.
        pub live: u32,
        password: Password,
        open: Vec<TcpStream>,
    }

    pub struct FakeVnc {
        pub address: SocketAddr,
        pub state: Arc<Mutex<State>>,
        done: Arc<AtomicBool>,
    }

    /// A test screen: a colour for each corner's quarter, and a stripe.
    pub fn pattern(width: u16, height: u16) -> Vec<[u8; 3]> {
        let (width, height) = (usize::from(width), usize::from(height));
        (0..width * height)
            .map(|n| {
                let (x, y) = (n % width, n / width);
                match (x * 2 / width, y * 2 / height, x % 5 == 0) {
                    (_, _, true) => [0xff, 0xff, 0xff],
                    (0, 0, _) => [0xff, 0, 0],
                    (1, 0, _) => [0, 0xff, 0],
                    (0, _, _) => [0, 0, 0xff],
                    _ => [0, 0, 0],
                }
            })
            .collect()
    }

    impl FakeVnc {
        pub fn start(password: Password) -> FakeVnc {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let state = Arc::new(Mutex::new(State {
                version: b"RFB 004.000\n",
                format: PixelFormat::RGB32,
                width: 40,
                height: 30,
                pixels: pattern(40, 30),
                resize: None,
                events: Vec::new(),
                sessions: 0,
                live: 0,
                password,
                open: Vec::new(),
            }));
            let done = Arc::new(AtomicBool::new(false));
            {
                let (state, done) = (Arc::clone(&state), Arc::clone(&done));
                std::thread::spawn(move || {
                    while !done.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                let state = Arc::clone(&state);
                                std::thread::spawn(move || serve(stream, &state));
                            }
                            Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
                        }
                    }
                });
            }
            FakeVnc { address, state, done }
        }

        pub fn events(&self) -> Vec<Event> {
            self.state.lock().unwrap().events.clone()
        }

        /// Closes every session, as AMT does when it ends one.
        pub fn close_sessions(&self) {
            for stream in self.state.lock().unwrap().open.drain(..) {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    }

    impl Drop for FakeVnc {
        fn drop(&mut self) {
            self.done.store(true, Ordering::Relaxed);
            self.close_sessions();
        }
    }

    fn serve(mut stream: TcpStream, state: &Arc<Mutex<State>>) {
        let _ = stream.set_nonblocking(false);
        let (version, password) = {
            let state = state.lock().unwrap();
            (state.version, (state.password)())
        };
        // A closed port: the session ends before it starts.
        let Some(password) = password else { return };
        let mut counted = false;
        let mut session = || -> std::io::Result<()> {
            stream.write_all(version)?;
            let mut reply = [0u8; 12];
            stream.read_exact(&mut reply)?;
            stream.write_all(&[1, 2])?;
            let mut kind = [0u8; 1];
            stream.read_exact(&mut kind)?;
            let challenge: [u8; 16] = std::array::from_fn(|n| (n as u8).wrapping_mul(37) ^ 0x5a);
            stream.write_all(&challenge)?;
            let mut response = [0u8; 16];
            stream.read_exact(&mut response)?;
            if kind[0] != 2 || response != password_response(&password, &challenge) {
                let reason = b"Authentication failed";
                stream.write_all(&1u32.to_be_bytes())?;
                stream.write_all(&(reason.len() as u32).to_be_bytes())?;
                return stream.write_all(reason);
            }
            stream.write_all(&0u32.to_be_bytes())?;
            stream.read_exact(&mut [0u8; 1])?;
            let mut format = {
                let mut state = state.lock().unwrap();
                state.sessions += 1;
                state.live += 1;
                counted = true;
                state.open.push(stream.try_clone()?);
                let mut init = Vec::new();
                init.extend_from_slice(&state.width.to_be_bytes());
                init.extend_from_slice(&state.height.to_be_bytes());
                init.extend_from_slice(&state.format.bytes());
                let name = b"Intel(r) AMT KVM";
                init.extend_from_slice(&(name.len() as u32).to_be_bytes());
                init.extend_from_slice(name);
                stream.write_all(&init)?;
                state.format
            };
            let mut encodings = Vec::new();
            loop {
                let mut kind = [0u8; 1];
                stream.read_exact(&mut kind)?;
                match kind[0] {
                    0 => {
                        let mut message = [0u8; 19];
                        stream.read_exact(&mut message)?;
                        format = PixelFormat::parse(&message[3..]);
                    }
                    2 => {
                        let mut head = [0u8; 3];
                        stream.read_exact(&mut head)?;
                        let count = usize::from(u16::from_be_bytes([head[1], head[2]]));
                        let mut list = vec![0u8; count * 4];
                        stream.read_exact(&mut list)?;
                        encodings = list
                            .chunks_exact(4)
                            .map(|n| i32::from_be_bytes([n[0], n[1], n[2], n[3]]))
                            .collect();
                    }
                    3 => {
                        stream.read_exact(&mut [0u8; 9])?;
                        update(&mut stream, state, format, &encodings)?;
                    }
                    4 => {
                        let mut message = [0u8; 7];
                        stream.read_exact(&mut message)?;
                        let key =
                            u32::from_be_bytes([message[3], message[4], message[5], message[6]]);
                        state.lock().unwrap().events.push(Event::Key(key, message[0] == 1));
                    }
                    5 => {
                        let mut message = [0u8; 5];
                        stream.read_exact(&mut message)?;
                        let x = u16::from_be_bytes([message[1], message[2]]);
                        let y = u16::from_be_bytes([message[3], message[4]]);
                        state.lock().unwrap().events.push(Event::Pointer(message[0], x, y));
                    }
                    _ => return Ok(()),
                }
            }
        };
        let _ = session();
        if counted {
            state.lock().unwrap().live -= 1;
        }
    }

    /// Answers a request: a new size when one is waiting, else a bell and
    /// the whole screen in two updates, top half then bottom.
    fn update(
        stream: &mut TcpStream,
        state: &Arc<Mutex<State>>,
        format: PixelFormat,
        encodings: &[i32],
    ) -> std::io::Result<()> {
        let mut state = state.lock().unwrap();
        if encodings.contains(&DESKTOP_SIZE)
            && let Some((width, height, pixels)) = state.resize.take()
        {
            (state.width, state.height, state.pixels) = (width, height, pixels);
            let mut message = vec![0, 0, 0, 1, 0, 0, 0, 0];
            message.extend_from_slice(&width.to_be_bytes());
            message.extend_from_slice(&height.to_be_bytes());
            message.extend_from_slice(&DESKTOP_SIZE.to_be_bytes());
            return stream.write_all(&message);
        }
        stream.write_all(&[2])?;
        let (width, height) = (state.width, state.height);
        let half = height / 2;
        for (top, rows) in [(0, half), (half, height - half)] {
            let mut message = vec![0, 0, 0, 1];
            for field in [0, top, width, rows] {
                message.extend_from_slice(&field.to_be_bytes());
            }
            message.extend_from_slice(&RAW.to_be_bytes());
            let start = usize::from(top) * usize::from(width);
            let end = start + usize::from(rows) * usize::from(width);
            for pixel in &state.pixels[start..end] {
                message.extend(format.encode(*pixel));
            }
            stream.write_all(&message)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{Event, FakeVnc, pattern};
    use super::*;

    const SECRET: &str = "Lk7!pQ2#";

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    fn fake() -> FakeVnc {
        FakeVnc::start(Box::new(|| Some(SECRET.into())))
    }

    #[test]
    fn signs_in_as_vnc_does() {
        // Worked out with OpenSSL's DES, the key the password's bytes
        // with their bits reversed.
        let challenge: [u8; 16] = std::array::from_fn(|n| n as u8);
        assert_eq!(
            password_response(SECRET, &challenge),
            [
                0x7f, 0x9f, 0x1a, 0x61, 0x8a, 0x7e, 0x29, 0x34, 0x48, 0xe4, 0x33, 0x33, 0x10, 0x60,
                0x7d, 0x99
            ]
        );
    }

    #[test]
    fn speaks_3_8_to_amts_4_0() {
        assert_eq!(parse_version(b"RFB 004.000\n"), Some(8));
        assert_eq!(parse_version(b"RFB 003.008\n"), Some(8));
        assert_eq!(parse_version(b"RFB 003.889\n"), Some(8));
        assert_eq!(parse_version(b"RFB 003.007\n"), Some(7));
        assert_eq!(parse_version(b"RFB 003.003\n"), Some(3));
        assert_eq!(parse_version(b"HTTP/1.1 200"), None);
    }

    #[test]
    fn sees_the_screen_and_sends_keys_and_the_pointer() {
        let fake = fake();
        let mut vnc = Vnc::connect(&fake.address.to_string(), SECRET, soon()).unwrap();
        assert_eq!(vnc.size(), (40, 30));
        assert_eq!(vnc.name, "Intel(r) AMT KVM");
        let image = vnc.capture(soon()).unwrap();
        assert_eq!((image.width, image.height), (40, 30));
        assert_eq!(image.pixels(), pattern(40, 30));
        vnc.key(0xffe1, true).unwrap();
        vnc.key(0x41, true).unwrap();
        vnc.pointer(1, 39, 29).unwrap();
        assert!(vnc.is_open());
        // The server reads messages in order: once it has answered this,
        // it has the keys and the pointer.
        let image = vnc.capture(soon()).unwrap();
        assert_eq!(image.pixels(), pattern(40, 30));
        assert_eq!(
            fake.events(),
            [Event::Key(0xffe1, true), Event::Key(0x41, true), Event::Pointer(1, 39, 29)]
        );
    }

    #[test]
    fn follows_a_new_size_in_another_format() {
        let fake = fake();
        {
            let mut state = fake.state.lock().unwrap();
            state.version = b"RFB 003.008\n";
            // RGB565, big-endian: the colours used survive it whole.
            state.format = PixelFormat {
                bits: 16,
                depth: 16,
                big_endian: true,
                true_colour: true,
                max: [31, 63, 31],
                shift: [11, 5, 0],
            };
            state.resize = Some((64, 48, pattern(64, 48)));
        }
        let mut vnc = Vnc::connect(&fake.address.to_string(), SECRET, soon()).unwrap();
        let image = vnc.capture(soon()).unwrap();
        assert_eq!((image.width, image.height), (64, 48));
        assert_eq!(image.pixels(), pattern(64, 48));
        assert_eq!(vnc.size(), (64, 48));
    }

    #[test]
    fn a_format_it_cannot_read_is_asked_to_change() {
        let fake = fake();
        fake.state.lock().unwrap().format.true_colour = false;
        let mut vnc = Vnc::connect(&fake.address.to_string(), SECRET, soon()).unwrap();
        assert_eq!(vnc.capture(soon()).unwrap().pixels(), pattern(40, 30));
    }

    #[test]
    fn a_wrong_password_fails_at_once() {
        let fake = fake();
        let started = Instant::now();
        let Err(err) = Vnc::connect(&fake.address.to_string(), "wrong", soon()) else {
            panic!("signed in with the wrong password")
        };
        assert!(err.contains("did not take the password (Authentication failed)"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn waits_for_the_port_to_open() {
        let open = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fake = {
            let open = std::sync::Arc::clone(&open);
            FakeVnc::start(Box::new(move || {
                open.load(std::sync::atomic::Ordering::Relaxed).then(|| SECRET.into())
            }))
        };
        let address = fake.address.to_string();
        let late = Instant::now() + Duration::from_millis(600);
        let Err(err) = Vnc::connect(&address, SECRET, late) else {
            panic!("signed in to a closed port")
        };
        assert!(err.contains("closed the session"), "{err}");
        open.store(true, std::sync::atomic::Ordering::Relaxed);
        Vnc::connect(&address, SECRET, soon()).unwrap();
    }

    #[test]
    fn notices_a_closed_session() {
        let fake = fake();
        let mut vnc = Vnc::connect(&fake.address.to_string(), SECRET, soon()).unwrap();
        assert!(vnc.is_open());
        fake.close_sessions();
        let deadline = soon();
        while vnc.is_open() {
            assert!(Instant::now() < deadline, "the closed session still looks open");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn keys_are_x_keysyms() {
        assert_eq!(keysyms("a").unwrap(), [0x61]);
        assert_eq!(keysyms("shift-a").unwrap(), [0xffe1, 0x41]);
        assert_eq!(keysyms("shift-1").unwrap(), [0xffe1, 0x21]);
        assert_eq!(keysyms("shift-0").unwrap(), [0xffe1, 0x29]);
        assert_eq!(keysyms("shift-slash").unwrap(), [0xffe1, 0x3f]);
        assert_eq!(keysyms("ctrl-alt-delete").unwrap(), [0xffe3, 0xffe9, 0xffff]);
        assert_eq!(keysyms("spc").unwrap(), [0x20]);
        assert_eq!(keysyms("f1").unwrap(), [0xffbe]);
        assert_eq!(keysyms("f12").unwrap(), [0xffc9]);
        assert_eq!(keysyms("kp_7").unwrap(), [0xffb7]);
        assert_eq!(keysyms("backspace").unwrap(), [0xff08]);
        let err = keysyms("ctrl-f13").unwrap_err();
        assert!(err.contains("no key named \"f13\""), "{err}");
        // Every key the lab's typing makes has a keysym.
        let typed = crate::screen::key_combos(
            "The quick brown fox: ~!@#$%^&*()_+{}|\"<>? `-=[]\\;',./ 0123456789\t\n",
        )
        .unwrap();
        for combo in typed {
            keysyms(&combo).unwrap();
        }
    }
}

// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A WCH CH9329 cable: keys and an absolute mouse for a real machine,
//! sent as serial frames that the chip turns into USB HID reports.
//!
//! The cable's CH340 end is a serial port on the lab PC; its CH9329 end is
//! a USB keyboard and mouse plugged into the machine. The plug driver
//! (driver/plug.rs) sends the `keys` and `mouse` controls through
//! [`Ch9329`], and names keys as QEMU does, which [`keys`] turns into HID
//! usages, the way [`crate::vnc::keysyms`] turns them into X keysyms for
//! AMT's remote screen.
//!
//! A frame is `57 AB`, the chip's address, a command, the length of its
//! data, the data, and the low byte of the sum of all before it; the chip
//! answers each command with a frame of its own. Every command is
//! answered before the next is sent, so a reply that is late or wrong
//! names the command it was for. When keys do nothing, `machines check`
//! (GET_INFO) says whether the chip answers and whether the machine's USB
//! host has set it up; the cable ships at 9600 baud, which is what
//! `plug.hid_baud` has to say unless the chip was reconfigured.

use std::fs::File;
use std::io::{ErrorKind, Read as _, Write as _};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::serial;

/// The largest X or Y of the absolute mouse: the whole screen is 0 to it.
pub const ABSOLUTE_MAX: u16 = 4095;

/// How long the chip has to answer a command.
const REPLY_TIME: Duration = Duration::from_millis(500);

/// How often a port with nothing to read is looked at again.
const POLL_GAP: Duration = Duration::from_millis(2);

/// Every frame starts with these two bytes.
const HEAD: [u8; 2] = [0x57, 0xAB];

/// The bytes of a frame besides its data: the head, the address, the
/// command, the length and the sum.
const FRAME_OVERHEAD: usize = 6;

/// The chip's address as it ships.
const ADDRESS: u8 = 0x00;

/// Asks for the chip's version and whether the machine has set it up
/// (GET_INFO).
const GET_INFO: u8 = 0x01;

/// Sends a keyboard report (SEND_KB_GENERAL_DATA).
const SEND_KEYBOARD: u8 = 0x02;

/// Sends an absolute mouse report (SEND_MS_ABS_DATA).
const SEND_POINTER: u8 = 0x04;

/// Added to a command to make the command of its normal reply.
const NORMAL_REPLY: u8 = 0x80;

/// Added to a command to make the command of its error reply.
const ERROR_REPLY: u8 = 0xC0;

/// The report ID of the absolute mouse.
const POINTER_REPORT: u8 = 0x02;

/// The keys a keyboard report holds besides modifiers.
const REPORT_KEYS: usize = 6;

/// A key of a combination: a modifier, a bit of the report's first byte,
/// or any other key, by its usage on the Keyboard/Keypad page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// A modifier, as the bit it sets in the report's first byte.
    Modifier(u8),
    /// A key by its usage.
    Usage(u8),
}

/// The modifiers, by QEMU's name, with their bit in a report.
const MODIFIERS: [(&str, u8); 8] = [
    ("ctrl", 0x01),
    ("shift", 0x02),
    ("alt", 0x04),
    ("meta_l", 0x08),
    ("ctrl_r", 0x10),
    ("shift_r", 0x20),
    ("alt_r", 0x40),
    ("meta_r", 0x80),
];

/// The other keys that have a name of their own, by QEMU's name, with
/// their usage (*HID Usage Tables*, chapter 10). Linux and Dukovo take
/// both `print` and `sysrq` as KEY_SYSRQ, which is usage 0x46.
const NAMED_KEYS: [(&str, u8); 39] = [
    ("ret", 0x28),
    ("esc", 0x29),
    ("backspace", 0x2A),
    ("tab", 0x2B),
    ("spc", 0x2C),
    ("minus", 0x2D),
    ("equal", 0x2E),
    ("bracket_left", 0x2F),
    ("bracket_right", 0x30),
    ("backslash", 0x31),
    ("semicolon", 0x33),
    ("apostrophe", 0x34),
    ("grave_accent", 0x35),
    ("comma", 0x36),
    ("dot", 0x37),
    ("slash", 0x38),
    ("caps_lock", 0x39),
    ("print", 0x46),
    ("sysrq", 0x46),
    ("scroll_lock", 0x47),
    ("pause", 0x48),
    ("insert", 0x49),
    ("home", 0x4A),
    ("pgup", 0x4B),
    ("delete", 0x4C),
    ("end", 0x4D),
    ("pgdn", 0x4E),
    ("right", 0x4F),
    ("left", 0x50),
    ("down", 0x51),
    ("up", 0x52),
    ("num_lock", 0x53),
    ("kp_divide", 0x54),
    ("kp_multiply", 0x55),
    ("kp_subtract", 0x56),
    ("kp_add", 0x57),
    ("kp_enter", 0x58),
    ("kp_decimal", 0x63),
    ("menu", 0x65),
];

/// The key QEMU calls `name`.
fn key(name: &str) -> Option<Key> {
    if let Some(&(_, bit)) = MODIFIERS.iter().find(|(modifier, _)| *modifier == name) {
        return Some(Key::Modifier(bit));
    }
    if let [byte] = name.as_bytes() {
        match byte {
            b'a'..=b'z' => return Some(Key::Usage(byte - b'a' + 0x04)),
            b'1'..=b'9' => return Some(Key::Usage(byte - b'1' + 0x1E)),
            b'0' => return Some(Key::Usage(0x27)),
            _ => {}
        }
    }
    if let Some(&(_, usage)) = NAMED_KEYS.iter().find(|(key, _)| *key == name) {
        return Some(Key::Usage(usage));
    }
    let number = |prefix: &str| name.strip_prefix(prefix).and_then(|n| n.parse::<u8>().ok());
    match (number("f"), number("kp_")) {
        (Some(n @ 1..=12), _) => Some(Key::Usage(0x3A + n - 1)),
        (_, Some(0)) => Some(Key::Usage(0x62)),
        (_, Some(n @ 1..=9)) => Some(Key::Usage(0x59 + n - 1)),
        _ => None,
    }
}

/// The keys of a combination named as QEMU names keys
/// (`ctrl-alt-delete`, `shift-a`, `f2`, `kp_7`), in the order they go down.
/// The same names [`crate::vnc::keysyms`] takes: every name it knows maps
/// here.
///
/// # Errors
///
/// Fails naming the first key that has no usage.
pub fn keys(combo: &str) -> Result<Vec<Key>, String> {
    combo
        .split('-')
        .map(|name| {
            key(name).ok_or_else(|| {
                format!("the CH9329 cable has no key named \"{name}\" (in \"{combo}\")")
            })
        })
        .collect()
}

/// The absolute X or Y of pixel `at` of a side `size` pixels long:
/// `at * 4095 / (size - 1)`, rounded to the nearest; 0 when `size` is 1.
/// A pixel past the side is the far edge.
pub fn absolute(at: u32, size: u32) -> u16 {
    let Some(last) = size.checked_sub(1).filter(|&last| last > 0) else {
        return 0;
    };
    let at = u64::from(at.min(last));
    let (last, max) = (u64::from(last), u64::from(ABSOLUTE_MAX));
    // `at` is at most `last`, so the rounded share is at most 4095.
    u16::try_from((at * max * 2 + last) / (last * 2)).unwrap_or(ABSOLUTE_MAX)
}

/// What GET_INFO says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// The chip's version: 0x30 is V1.0.
    pub version: u8,
    /// Whether the machine's USB host has set the chip up.
    pub enumerated: bool,
}

/// A CH9329 cable, through its serial port.
pub struct Ch9329 {
    port: File,
    /// What messages call the cable.
    name: String,
}

impl Ch9329 {
    /// Opens `path` at `baud`, raw, for reading and writing.
    ///
    /// # Errors
    ///
    /// Fails when the port cannot be opened or set up.
    pub fn open(path: &Path, baud: u32) -> Result<Ch9329, String> {
        let port = serial::open_port(path, baud, true)?;
        Ok(Ch9329 { port, name: path.display().to_string() })
    }

    /// A cable already open as `port`, named `name` in messages: tests hand
    /// it one end of a socket pair.
    #[cfg(test)]
    pub fn over(port: File, name: &str) -> Ch9329 {
        Ch9329 { port, name: name.to_owned() }
    }

    /// Asks the chip for its version and whether the machine has set it up.
    ///
    /// # Errors
    ///
    /// Fails when the chip does not answer or answers wrongly.
    pub fn info(&mut self) -> Result<Info, String> {
        let reply = self.command(GET_INFO, &[], "the info request")?;
        let [version, enumerated, _lights, _reserved @ ..] = reply.as_slice() else {
            return Err(format!(
                "the CH9329 cable at {} answered the info request with {} bytes, not 8: is it a \
                 CH9329 at the speed plug.hid_baud says?",
                self.name,
                reply.len()
            ));
        };
        Ok(Info { version: *version, enumerated: *enumerated == 1 })
    }

    /// Presses `combo`: each key goes down in order, a report each; held
    /// for `hold`; then each comes up in reverse order, a report each.
    ///
    /// # Errors
    ///
    /// Fails before anything is sent when more than six keys are not
    /// modifiers (a report holds six), and when the chip does not answer or
    /// turns a report down. A press that fails partway sends one report
    /// with every key up, so no key is left held on the machine.
    pub fn press(&mut self, combo: &[Key], hold: Duration) -> Result<(), String> {
        let plain = combo.iter().filter(|key| matches!(key, Key::Usage(_))).count();
        if plain > REPORT_KEYS {
            return Err(format!(
                "a keyboard report holds {REPORT_KEYS} keys at once and this combination has \
                 {plain} that are not modifiers: press fewer keys together"
            ));
        }
        let pressed = self.press_and_release(combo, hold);
        if pressed.is_err() {
            // The press has failed already, and its error says why; a
            // release the chip does not take either changes nothing.
            let _ = self.keyboard(0, &[]);
        }
        pressed
    }

    /// The reports of [`Ch9329::press`], without its checks.
    fn press_and_release(&mut self, combo: &[Key], hold: Duration) -> Result<(), String> {
        let mut modifiers = 0u8;
        let mut usages = Vec::with_capacity(REPORT_KEYS);
        for &key in combo {
            match key {
                Key::Modifier(bit) => modifiers |= bit,
                Key::Usage(usage) => usages.push(usage),
            }
            self.keyboard(modifiers, &usages)?;
        }
        std::thread::sleep(hold);
        for &key in combo.iter().rev() {
            match key {
                Key::Modifier(bit) => modifiers &= !bit,
                Key::Usage(usage) => {
                    if let Some(at) = usages.iter().rposition(|&down| down == usage) {
                        usages.remove(at);
                    }
                }
            }
            self.keyboard(modifiers, &usages)?;
        }
        Ok(())
    }

    /// Sends one absolute mouse report: `buttons` (bit 0 left, 1 right, 2
    /// middle) with the pointer at `x`, `y`, each 0 to [`ABSOLUTE_MAX`].
    ///
    /// # Errors
    ///
    /// Fails when the chip does not answer or turns the report down.
    pub fn pointer(&mut self, buttons: u8, x: u16, y: u16) -> Result<(), String> {
        let [x_low, x_high] = x.to_le_bytes();
        let [y_low, y_high] = y.to_le_bytes();
        let report = [POINTER_REPORT, buttons, x_low, x_high, y_low, y_high, 0];
        self.command(SEND_POINTER, &report, "the mouse report").map(drop)
    }

    /// Sends one keyboard report: `modifiers` and up to six `usages` down.
    fn keyboard(&mut self, modifiers: u8, usages: &[u8]) -> Result<(), String> {
        let mut report = [0u8; 8];
        report[0] = modifiers;
        report[2..2 + usages.len()].copy_from_slice(usages);
        self.command(SEND_KEYBOARD, &report, "the key report").map(drop)
    }

    /// Sends `cmd` with `data` and returns the data of its reply. `what`
    /// names the command in messages.
    fn command(&mut self, cmd: u8, data: &[u8], what: &str) -> Result<Vec<u8>, String> {
        self.drain()?;
        self.write_frame(&frame(cmd, data))?;
        let reply = self.read_frame()?;
        let (body, sum_byte) = reply.split_at(reply.len() - 1);
        if sum(body) != sum_byte[0] {
            return Err(format!(
                "the CH9329 cable at {} answered {what} with a frame whose checksum is wrong: \
                 the line may be noisy, or plug.hid_baud is not the cable's speed",
                self.name
            ));
        }
        let reply_cmd = reply[3];
        let failed = reply_cmd == cmd | ERROR_REPLY;
        if reply_cmd != cmd | NORMAL_REPLY && !failed {
            return Err(format!(
                "the CH9329 cable at {} answered {what} with the reply to command 0x{:02X}",
                self.name,
                reply_cmd & !ERROR_REPLY
            ));
        }
        let reply_data = &reply[FRAME_OVERHEAD - 1..reply.len() - 1];
        // GET_INFO's normal reply is the info itself; every other reply
        // is a status byte.
        if failed || cmd != GET_INFO {
            let Some(&status) = reply_data.first() else {
                return Err(format!(
                    "the CH9329 cable at {} answered {what} with no status",
                    self.name
                ));
            };
            if status != 0x00 {
                return Err(format!(
                    "the CH9329 cable at {} turned down {what}: {} (0x{status:02X})",
                    self.name,
                    status_meaning(status)
                ));
            }
        }
        Ok(reply_data.to_vec())
    }

    /// Reads and drops what the port holds, without waiting: bytes left by
    /// an earlier command's late reply would be taken for this one's.
    fn drain(&mut self) -> Result<(), String> {
        let mut chunk = [0u8; 64];
        loop {
            match self.port.read(&mut chunk) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(err) if err.kind() == ErrorKind::WouldBlock => return Ok(()),
                Err(err) if err.kind() == ErrorKind::Interrupted => {}
                Err(err) => return Err(self.io_failure("reading", &err)),
            }
        }
    }

    /// Writes all of `bytes`; the port is non-blocking, so a full buffer
    /// is waited out for as long as the chip has to answer.
    fn write_frame(&mut self, bytes: &[u8]) -> Result<(), String> {
        let deadline = Instant::now() + REPLY_TIME;
        let mut sent = 0;
        while sent < bytes.len() {
            match self.port.write(&bytes[sent..]) {
                Ok(written) => sent += written,
                Err(err) if err.kind() == ErrorKind::Interrupted => {}
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(self.silence());
                    }
                    std::thread::sleep(POLL_GAP);
                }
                Err(err) => return Err(self.io_failure("writing to", &err)),
            }
        }
        Ok(())
    }

    /// Reads the next whole frame, skipping bytes before its head.
    fn read_frame(&mut self) -> Result<Vec<u8>, String> {
        let deadline = Instant::now() + REPLY_TIME;
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 64];
        loop {
            if let Some(frame) = take_frame(&mut buffer) {
                return Ok(frame);
            }
            if Instant::now() >= deadline {
                return Err(self.silence());
            }
            match self.port.read(&mut chunk) {
                Ok(read @ 1..) => buffer.extend_from_slice(&chunk[..read]),
                // Nothing yet, or the end of what a plain file holds so far.
                Ok(0) => std::thread::sleep(POLL_GAP),
                Err(err) if err.kind() == ErrorKind::WouldBlock => std::thread::sleep(POLL_GAP),
                Err(err) if err.kind() == ErrorKind::Interrupted => {}
                Err(err) => return Err(self.io_failure("reading", &err)),
            }
        }
    }

    /// The error for a chip that does not answer in time.
    fn silence(&self) -> String {
        format!(
            "the CH9329 cable at {} did not answer within 0.5 s: is the machine on (the chip may \
             run on the machine's USB power), and is plug.hid_baud the cable's speed (9600 as it \
             comes)?",
            self.name
        )
    }

    /// The error for a port that fails while `doing` something to it.
    fn io_failure(&self, doing: &str, err: &std::io::Error) -> String {
        format!("{doing} the CH9329 cable at {}: {err} (is the cable still plugged in?)", self.name)
    }
}

/// The low byte of the sum of `bytes`, which ends every frame.
fn sum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |total, byte| total.wrapping_add(*byte))
}

/// A frame for command `cmd` carrying `data`, at the chip's address.
fn frame(cmd: u8, data: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(FRAME_OVERHEAD + data.len());
    bytes.extend_from_slice(&HEAD);
    bytes.push(ADDRESS);
    bytes.push(cmd);
    // No command carries more than 8 bytes.
    bytes.push(data.len() as u8);
    bytes.extend_from_slice(data);
    bytes.push(sum(&bytes));
    bytes
}

/// Removes and returns the first whole frame in `buffer`, dropping the
/// bytes before its head. Gives `None` and keeps what might be the start
/// of a frame while more is still to come.
fn take_frame(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let Some(start) = buffer.windows(2).position(|pair| pair == HEAD) else {
        // A lone 0x57 may be the first half of a head.
        let keep = usize::from(buffer.last() == Some(&HEAD[0]));
        buffer.drain(..buffer.len() - keep);
        return None;
    };
    buffer.drain(..start);
    let length = *buffer.get(4)?;
    let total = FRAME_OVERHEAD + usize::from(length);
    if buffer.len() < total {
        return None;
    }
    Some(buffer.drain(..total).collect())
}

/// What a status byte of a reply means.
fn status_meaning(status: u8) -> &'static str {
    match status {
        0xE1 => "timeout",
        0xE2 => "bad head",
        0xE3 => "bad command",
        0xE4 => "bad checksum",
        0xE5 => "bad parameter",
        0xE6 => "operation failed",
        _ => "a status the datasheet does not list",
    }
}

/// A pretend CH9329 for tests: it parses frames, answers them and records
/// what it was sent.
#[cfg(test)]
pub mod fake {
    use std::fs::File;
    use std::io::{ErrorKind, Read as _, Write as _};
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use super::{
        ERROR_REPLY, FRAME_OVERHEAD, GET_INFO, HEAD, NORMAL_REPLY, POINTER_REPORT, SEND_KEYBOARD,
        SEND_POINTER, frame, sum,
    };

    /// The status of a frame whose head is not `57 AB`.
    const BAD_HEAD: u8 = 0xE2;
    /// The status of a command the pretend chip does not have.
    const BAD_COMMAND: u8 = 0xE3;
    /// The status of a frame whose sum is wrong.
    const BAD_SUM: u8 = 0xE4;
    /// The status of a frame whose data is not the command's.
    const BAD_PARAMETER: u8 = 0xE5;
    /// The command was understood and could not be carried out.
    const FAILED: u8 = 0xE6;

    /// How long a frame may stay cut short before the pretend chip drops
    /// what it has, so the next frame starts clean.
    const FRAME_GAP: Duration = Duration::from_millis(50);

    /// What the pretend machine was sent.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Event {
        /// A GET_INFO.
        Info,
        /// A keyboard report: the modifier byte and the six key usages.
        Keyboard {
            /// The report's first byte.
            modifiers: u8,
            /// The six usages, 0 where none.
            keys: [u8; 6],
        },
        /// An absolute mouse report.
        Pointer {
            /// The buttons down.
            buttons: u8,
            /// X, 0 to 4095.
            x: u16,
            /// Y, 0 to 4095.
            y: u16,
        },
    }

    /// The pretend chip's state, shared with the test.
    pub struct State {
        /// Whether the chip has power; without it every byte is dropped
        /// and nothing is answered.
        pub powered: bool,
        /// What GET_INFO says about the machine's USB host.
        pub enumerated: bool,
        /// What GET_INFO says the version is.
        pub version: u8,
        /// Each command that was understood, in order.
        pub events: Vec<Event>,
        /// The one keyboard report, counted from 0, that the chip turns
        /// down with "operation failed".
        pub turn_down: Option<usize>,
        /// How many keyboard reports have arrived.
        keyboard_reports: usize,
    }

    /// A pretend CH9329, serving until dropped.
    pub struct FakeCh9329 {
        /// The chip's state.
        pub state: Arc<Mutex<State>>,
        done: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl FakeCh9329 {
        /// A powered chip that the machine has set up, and the driver's end
        /// of its cable.
        pub fn start() -> (FakeCh9329, File) {
            let (ours, theirs) = UnixStream::pair().unwrap();
            ours.set_nonblocking(true).unwrap();
            theirs.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
            let state = Arc::new(Mutex::new(State {
                powered: true,
                enumerated: true,
                version: 0x30,
                events: Vec::new(),
                turn_down: None,
                keyboard_reports: 0,
            }));
            let done = Arc::new(AtomicBool::new(false));
            let thread = {
                let (state, done) = (Arc::clone(&state), Arc::clone(&done));
                std::thread::spawn(move || serve(theirs, &state, &done))
            };
            let chip = FakeCh9329 { state, done, thread: Some(thread) };
            (chip, File::from(OwnedFd::from(ours)))
        }

        /// What the chip was sent, in order.
        pub fn events(&self) -> Vec<Event> {
            self.state.lock().unwrap_or_else(PoisonError::into_inner).events.clone()
        }
    }

    impl Drop for FakeCh9329 {
        fn drop(&mut self) {
            self.done.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// Reads the cable's bytes and answers each whole frame until `done`
    /// or until the driver's end closes.
    fn serve(mut stream: UnixStream, state: &Mutex<State>, done: &AtomicBool) {
        let mut pending: Vec<u8> = Vec::new();
        let mut last_byte = Instant::now();
        let mut chunk = [0u8; 64];
        while !done.load(Ordering::Relaxed) {
            match stream.read(&mut chunk) {
                Ok(0) => return,
                Ok(read) => {
                    pending.extend_from_slice(&chunk[..read]);
                    last_byte = Instant::now();
                }
                Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    if last_byte.elapsed() > FRAME_GAP {
                        pending.clear();
                    }
                }
                Err(_) => return,
            }
            let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
            if !state.powered {
                pending.clear();
                continue;
            }
            while let Some(reply) = next_reply(&mut pending, &mut state) {
                if stream.write_all(&reply).is_err() {
                    return;
                }
            }
        }
    }

    /// The reply to the first frame in `pending`, which it removes; `None`
    /// while the frame is not whole yet.
    fn next_reply(pending: &mut Vec<u8>, state: &mut State) -> Option<Vec<u8>> {
        let first = *pending.first()?;
        if first != HEAD[0] || pending.get(1).is_some_and(|&second| second != HEAD[1]) {
            // The command byte is unknown here, so the reply gives 0.
            pending.clear();
            return Some(error_reply(0, BAD_HEAD));
        }
        let total = FRAME_OVERHEAD + usize::from(*pending.get(4)?);
        if pending.len() < total {
            return None;
        }
        let received: Vec<u8> = pending.drain(..total).collect();
        let cmd = received[3];
        if sum(&received[..total - 1]) != received[total - 1] {
            return Some(error_reply(cmd, BAD_SUM));
        }
        let data = &received[FRAME_OVERHEAD - 1..total - 1];
        Some(match (cmd, data) {
            (GET_INFO, []) => {
                state.events.push(Event::Info);
                let info = [state.version, u8::from(state.enumerated), 0, 0, 0, 0, 0, 0];
                frame(GET_INFO | NORMAL_REPLY, &info)
            }
            (SEND_KEYBOARD, &[modifiers, _, k0, k1, k2, k3, k4, k5]) => {
                let number = state.keyboard_reports;
                state.keyboard_reports += 1;
                if state.turn_down == Some(number) {
                    return Some(error_reply(cmd, FAILED));
                }
                let keys = [k0, k1, k2, k3, k4, k5];
                state.events.push(Event::Keyboard { modifiers, keys });
                frame(SEND_KEYBOARD | NORMAL_REPLY, &[0])
            }
            (SEND_POINTER, &[POINTER_REPORT, buttons, x_low, x_high, y_low, y_high, _]) => {
                let (x, y) =
                    (u16::from_le_bytes([x_low, x_high]), u16::from_le_bytes([y_low, y_high]));
                state.events.push(Event::Pointer { buttons, x, y });
                frame(SEND_POINTER | NORMAL_REPLY, &[0])
            }
            (GET_INFO | SEND_KEYBOARD | SEND_POINTER, _) => error_reply(cmd, BAD_PARAMETER),
            _ => error_reply(cmd, BAD_COMMAND),
        })
    }

    /// The error reply to command `cmd`, with `status`.
    fn error_reply(cmd: u8, status: u8) -> Vec<u8> {
        frame(cmd | ERROR_REPLY, &[status])
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::thread::JoinHandle;

    use super::fake::{Event, FakeCh9329};
    use super::*;

    /// A cable whose other end answers its first command with `parts`:
    /// each is written, then waited out for the time beside it.
    fn cable_answering(parts: Vec<(Vec<u8>, u64)>) -> (Ch9329, JoinHandle<()>) {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        let thread = std::thread::spawn(move || {
            // The command arrives in one write.
            assert!(theirs.read(&mut [0u8; 64]).unwrap() > 0);
            for (bytes, then_wait) in parts {
                theirs.write_all(&bytes).unwrap();
                std::thread::sleep(Duration::from_millis(then_wait));
            }
        });
        (Ch9329::over(File::from(OwnedFd::from(ours)), "the test cable"), thread)
    }

    /// What `port` gives within a moment of `bytes` being written to it.
    fn answer_to(port: &mut File, bytes: &[u8]) -> Vec<u8> {
        port.write_all(bytes).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let mut got = [0u8; 64];
        match port.read(&mut got) {
            Ok(read) => got[..read].to_vec(),
            Err(_) => Vec::new(),
        }
    }

    #[test]
    fn frames_are_the_datasheets() {
        let key_a = frame(0x02, &[0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(
            key_a,
            [0x57, 0xAB, 0x00, 0x02, 0x08, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10]
        );
        let all_up = frame(0x02, &[0; 8]);
        assert_eq!(
            all_up,
            [0x57, 0xAB, 0x00, 0x02, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0C]
        );
        assert_eq!(frame(0x82, &[0x00]), [0x57, 0xAB, 0x00, 0x82, 0x01, 0x00, 0x85]);
    }

    #[test]
    fn a_whole_reply_is_taken_from_the_bytes_read() {
        let mut buffer = vec![0x57, 0xAB, 0x00, 0x82, 0x01, 0x00, 0x85, 0x57];
        assert_eq!(take_frame(&mut buffer).unwrap(), [0x57, 0xAB, 0x00, 0x82, 0x01, 0x00, 0x85]);
        // The 0x57 after it may start the next frame.
        assert_eq!(buffer, [0x57]);
        assert_eq!(take_frame(&mut buffer), None);
    }

    #[test]
    fn a_reply_split_across_reads_is_read() {
        let reply = frame(0x84, &[0x00]);
        let stray = vec![0x00, 0x57, 0x12, 0x57];
        let (mut cable, thread) =
            cable_answering(vec![(stray, 5), (reply[..3].to_vec(), 30), (reply[3..].to_vec(), 0)]);
        cable.pointer(0, 10, 20).unwrap();
        thread.join().unwrap();
    }

    #[test]
    fn a_bad_reply_sum_is_an_error() {
        let mut reply = frame(0x84, &[0x00]);
        *reply.last_mut().unwrap() ^= 0x01;
        let (mut cable, thread) = cable_answering(vec![(reply, 0)]);
        let err = cable.pointer(0, 10, 20).unwrap_err();
        assert!(err.contains("the test cable") && err.contains("checksum is wrong"), "{err}");
        thread.join().unwrap();
    }

    #[test]
    fn a_reply_to_another_command_is_an_error() {
        let (mut cable, thread) = cable_answering(vec![(frame(0x82, &[0x00]), 0)]);
        let err = cable.pointer(0, 10, 20).unwrap_err();
        assert!(err.contains("the reply to command 0x02"), "{err}");
        thread.join().unwrap();
    }

    #[test]
    fn an_error_reply_names_its_code() {
        let (mut cable, thread) = cable_answering(vec![(frame(0xC4, &[0xE5]), 0)]);
        let err = cable.pointer(0, 10, 20).unwrap_err();
        assert!(err.contains("turned down the mouse report: bad parameter (0xE5)"), "{err}");
        thread.join().unwrap();
        // A normal reply that carries a status is an error too.
        let (mut cable, thread) = cable_answering(vec![(frame(0x82, &[0xE6]), 0)]);
        let err = cable.press(&keys("a").unwrap(), Duration::ZERO).unwrap_err();
        assert!(err.contains("turned down the key report: operation failed (0xE6)"), "{err}");
        thread.join().unwrap();
    }

    #[test]
    fn a_cable_with_no_power_times_out_naming_the_fix() {
        let (fake, port) = FakeCh9329::start();
        fake.state.lock().unwrap().powered = false;
        let mut cable = Ch9329::over(port, "/dev/serial/by-path/usb-1");
        let err = cable.pointer(0, 0, 0).unwrap_err();
        assert!(err.contains("/dev/serial/by-path/usb-1 did not answer within 0.5 s"), "{err}");
        assert!(err.contains("plug.hid_baud"), "{err}");
        assert!(fake.events().is_empty());
    }

    #[test]
    fn info_reads_the_version_and_whether_the_machine_set_the_chip_up() {
        let (fake, port) = FakeCh9329::start();
        let mut cable = Ch9329::over(port, "fake");
        assert_eq!(cable.info().unwrap(), Info { version: 0x30, enumerated: true });
        fake.state.lock().unwrap().enumerated = false;
        fake.state.lock().unwrap().version = 0x31;
        assert_eq!(cable.info().unwrap(), Info { version: 0x31, enumerated: false });
        assert_eq!(fake.events(), [Event::Info, Event::Info]);
    }

    #[test]
    fn ctrl_alt_delete_is_six_reports_built_up_and_torn_down() {
        let (fake, port) = FakeCh9329::start();
        let mut cable = Ch9329::over(port, "fake");
        cable.press(&keys("ctrl-alt-delete").unwrap(), Duration::from_millis(1)).unwrap();
        let report = |modifiers, first| Event::Keyboard { modifiers, keys: [first, 0, 0, 0, 0, 0] };
        assert_eq!(
            fake.events(),
            [
                report(0x01, 0),
                report(0x05, 0),
                report(0x05, 0x4C),
                report(0x05, 0),
                report(0x01, 0),
                report(0x00, 0),
            ]
        );
    }

    #[test]
    fn six_plain_keys_go_down_together_and_seven_are_refused_before_anything_is_sent() {
        let (fake, port) = FakeCh9329::start();
        let mut cable = Ch9329::over(port, "fake");
        cable.press(&keys("shift-a-b-c-d-e-f").unwrap(), Duration::ZERO).unwrap();
        let down = fake.events()[6];
        assert_eq!(down, Event::Keyboard { modifiers: 0x02, keys: [4, 5, 6, 7, 8, 9] });
        let sent = fake.events().len();
        let err = cable.press(&keys("a-b-c-d-e-f-g").unwrap(), Duration::ZERO).unwrap_err();
        assert!(err.contains("holds 6 keys") && err.contains("has 7"), "{err}");
        assert_eq!(fake.events().len(), sent);
    }

    #[test]
    fn a_press_that_fails_partway_lets_every_key_up() {
        let (fake, port) = FakeCh9329::start();
        let mut cable = Ch9329::over(port, "the test cable");
        // Ctrl goes down; C, the second report, is turned down.
        fake.state.lock().unwrap().turn_down = Some(1);
        let err = cable.press(&keys("ctrl-c").unwrap(), Duration::ZERO).unwrap_err();
        assert!(err.contains("operation failed (0xE6)"), "{err}");
        let ctrl = 0x01;
        assert_eq!(
            fake.events(),
            [
                Event::Keyboard { modifiers: ctrl, keys: [0; 6] },
                Event::Keyboard { modifiers: 0, keys: [0; 6] },
            ]
        );
    }

    #[test]
    fn the_pointer_report_carries_the_buttons_and_both_axes() {
        let (fake, port) = FakeCh9329::start();
        let mut cable = Ch9329::over(port, "fake");
        cable.pointer(0b101, 4095, 0x0102).unwrap();
        assert_eq!(fake.events(), [Event::Pointer { buttons: 5, x: 4095, y: 0x0102 }]);
    }

    #[test]
    fn the_fake_answers_bad_frames_with_error_codes() {
        let (fake, mut port) = FakeCh9329::start();
        let mut bad_sum = frame(0x04, &[2, 0, 0, 0, 0, 0, 0]);
        *bad_sum.last_mut().unwrap() ^= 0x01;
        assert_eq!(answer_to(&mut port, &bad_sum), frame(0xC4, &[0xE4]));
        assert_eq!(answer_to(&mut port, &[0x00, 0x00, 0x00]), frame(0xC0, &[0xE2]));
        assert_eq!(answer_to(&mut port, &frame(0x05, &[])), frame(0xC5, &[0xE3]));
        assert_eq!(answer_to(&mut port, &frame(0x04, &[2, 0])), frame(0xC4, &[0xE5]));
        // A frame cut short gets no answer, and does not spoil the next.
        let whole = frame(0x01, &[]);
        assert_eq!(answer_to(&mut port, &whole[..4]), Vec::<u8>::new());
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(answer_to(&mut port, &whole)[3], 0x81);
        assert_eq!(fake.events(), [Event::Info]);
    }

    #[test]
    fn keys_are_named_as_qemu_names_them() {
        assert_eq!(
            keys("ctrl-alt-delete").unwrap(),
            [Key::Modifier(0x01), Key::Modifier(0x04), Key::Usage(0x4C)]
        );
        assert_eq!(keys("shift-a").unwrap(), [Key::Modifier(0x02), Key::Usage(0x04)]);
        assert_eq!(keys("z").unwrap(), [Key::Usage(0x1D)]);
        assert_eq!(keys("1").unwrap(), [Key::Usage(0x1E)]);
        assert_eq!(keys("9").unwrap(), [Key::Usage(0x26)]);
        assert_eq!(keys("0").unwrap(), [Key::Usage(0x27)]);
        assert_eq!(keys("f1").unwrap(), [Key::Usage(0x3A)]);
        assert_eq!(keys("f12").unwrap(), [Key::Usage(0x45)]);
        assert_eq!(keys("kp_1").unwrap(), [Key::Usage(0x59)]);
        assert_eq!(keys("kp_7").unwrap(), [Key::Usage(0x5F)]);
        assert_eq!(keys("kp_9").unwrap(), [Key::Usage(0x61)]);
        assert_eq!(keys("kp_0").unwrap(), [Key::Usage(0x62)]);
        assert_eq!(keys("meta_r").unwrap(), [Key::Modifier(0x80)]);
        assert_eq!(keys("print").unwrap(), keys("sysrq").unwrap());
    }

    #[test]
    fn an_unknown_key_is_named_with_its_combination() {
        let err = keys("ctrl-x-f13").unwrap_err();
        assert_eq!(err, "the CH9329 cable has no key named \"f13\" (in \"ctrl-x-f13\")");
        assert!(keys("").is_err());
    }

    #[test]
    fn every_key_the_remote_screen_takes_is_taken_here() {
        let names = crate::vnc::key_names();
        assert!(names.len() > 100, "{} names", names.len());
        for name in names {
            crate::vnc::keysyms(&name).unwrap();
            keys(&name).unwrap_or_else(|err| panic!("{name}: {err}"));
        }
    }

    #[test]
    fn absolute_spreads_the_pixels_over_the_whole_range() {
        assert_eq!(absolute(0, 1920), 0);
        assert_eq!(absolute(1919, 1920), 4095);
        assert_eq!(absolute(960, 1920), 2049);
        assert_eq!(absolute(0, 1), 0);
        assert_eq!(absolute(5, 1), 0);
        assert_eq!(absolute(0, 0), 0);
        assert_eq!(absolute(5000, 1920), 4095);
    }
}

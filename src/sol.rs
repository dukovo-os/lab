// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A real machine's serial console read over the lab network from Intel
//! AMT's Serial-over-LAN, so that the machine needs no serial cable.
//!
//! AMT's management engine shows the system a 16550 of its own (PCI
//! 00:16.3 on Intel boards, `ttyS4` on Linux and on Dukovo, which sends
//! its log to every serial port) and hands what the system writes to it
//! to one console on the network at a time. With no console there, the
//! text is dropped, so one thread keeps a session open for the whole run
//! and appends what comes to a [`ConsoleLog`], whichever log the driver
//! last named, as serial.rs's reader does for a cable.
//!
//! The session is AMT's redirection protocol on TCP port 16994, beside
//! WS-Management's 16992. Intel documents it only through its SDK's
//! library; the messages here are those Intel's own open-source consoles
//! send. Numbers are little-endian, and each message starts with its
//! type:
//!
//! - **0x10** asks for a `SOL ` session. AMT answers 0x11: a status, and
//!   13 bytes in all, the last the length of OEM data after them.
//! - **0x13** signs in. The first asks which ways AMT takes (0x14 lists
//!   them); the reader signs in only with HTTP digest with a client
//!   nonce (way 4, RFC 2617 with `qop`), never with the plain password
//!   (way 1). An empty first try brings AMT's realm, nonce and qop with
//!   status 1; the answer to them, for the URI `/RedirectionService` and
//!   the method `POST`, brings status 0.
//! - **0x20** starts Serial-over-LAN with the session's settings, and AMT
//!   answers 0x21, 23 bytes. **0x27** then sets the line's control
//!   signals, as Intel's consoles set them.
//! - Then AMT sends what the system wrote as **0x2A**: 10 bytes with the
//!   text's length at 8, and the text. **0x2B**, 8 bytes, keeps the
//!   session alive, and the reader sends one every [`KEEP_ALIVE`]: AMT
//!   ends a session it hears nothing from for [`QUIET_LIMIT`]. **0x29**,
//!   10 bytes, is passed over.
//!
//! A session that ends (the lab network, AMT's engine restarting, a
//! [`SolReader::hold`]) is opened again at once; what the system writes
//! in between is lost. The driver's log (`amt.log`) gets a line for each
//! session opened and each end, with its reason, and never the password.
//! When `serial-<n>.log` stays empty while the machine runs, look there
//! first; then at whether the system writes to its AMT port (Dukovo says
//! `Serial: the Intel AMT Serial-over-LAN port at ... is the terminal
//! /devices/ttyS4` when it finds it).

use std::io::{ErrorKind, Read as _, Write as _};
use std::net::{TcpStream, ToSocketAddrs as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::driver::real::DriverLog;
use crate::serial::ConsoleLog;
use crate::wsman::Digest;

/// AMT's port for redirection sessions without TLS.
pub const PORT: u16 = 16994;

const START_SESSION: u8 = 0x10;
const START_SESSION_REPLY: u8 = 0x11;
const AUTHENTICATE: u8 = 0x13;
const AUTHENTICATE_REPLY: u8 = 0x14;
const START_SOL: u8 = 0x20;
const START_SOL_REPLY: u8 = 0x21;
const LINE_CONTROL: u8 = 0x27;
const SOL_SETTINGS: u8 = 0x29;
const TEXT: u8 = 0x2A;
const KEEP_ALIVE_MESSAGE: u8 = 0x2B;

/// The ways of signing in: the question of which AMT takes, and HTTP
/// digest with a client nonce.
const WAY_QUERY: u8 = 0;
const WAY_DIGEST: u8 = 4;
/// The status of a sign-in AMT has answered with its challenge.
const CHALLENGED: u8 = 1;
/// What a digest answer is for, as Intel's consoles sign in.
const DIGEST_URI: &str = "/RedirectionService";
const DIGEST_METHOD: &str = "POST";
const NONCE_COUNT: &str = "00000002";

/// The session's settings, in AMT's order, each 16 bits, as Intel's
/// consoles set them: the most text AMT sends in one message, how long
/// it gathers text before sending it (ms), how long it lets text wait
/// when the console falls behind (0, not at all), [`QUIET_LIMIT`] in ms,
/// how long it gathers what the console types before handing it to the
/// system (ms), and AMT's own heartbeat (0, none).
const SESSION_SETTINGS: [u16; 6] = [10_000, 100, 0, QUIET_LIMIT_MS, 100, 0];

/// The line's control signals, as Intel's consoles send them once
/// Serial-over-LAN has started.
const LINE_SIGNALS: [u8; 6] = [0, 0, 0x1B, 0, 0, 0];

/// How long AMT keeps a session it hears nothing from.
const QUIET_LIMIT_MS: u16 = 10_000;
const QUIET_LIMIT: Duration = Duration::from_millis(QUIET_LIMIT_MS as u64);

/// How often the reader keeps the session alive: well inside
/// [`QUIET_LIMIT`].
const KEEP_ALIVE: Duration =
    if cfg!(test) { Duration::from_millis(100) } else { Duration::from_secs(2) };
const _: () = assert!(KEEP_ALIVE.as_millis() * 4 <= QUIET_LIMIT.as_millis());

/// How long connecting, and each answer while a session opens, may take.
const CONNECT_TIME: Duration = Duration::from_secs(5);
const ANSWER_TIME: Duration = Duration::from_secs(10);

/// How long an open session's thread waits for text before it looks at
/// whether to keep the session alive, close it, or stop.
const POLL_TIME: Duration = Duration::from_millis(200);

/// How long the first session may take to open: AMT that has been left
/// alone with the machine off dozes, and refuses connections for up to
/// 20 seconds; a console that went away without closing its session
/// holds it for [`QUIET_LIMIT`].
const FIRST_OPEN_TIME: Duration =
    if cfg!(test) { Duration::from_secs(3) } else { Duration::from_secs(45) };

/// How long after a session fails to open it is tried again, and after
/// AMT turns the sign-in down: AMT locks an account out for a while
/// after several wrong sign-ins.
const RETRY_GAP: Duration =
    if cfg!(test) { Duration::from_millis(50) } else { Duration::from_secs(1) };
const REFUSED_GAP: Duration =
    if cfg!(test) { Duration::from_millis(500) } else { Duration::from_secs(60) };

/// How often a session that keeps failing to open is noted again.
const NOTE_AGAIN: Duration = Duration::from_secs(60);

/// How long [`SolReader::hold`] waits for the session to close.
const HOLD_TIME: Duration = Duration::from_secs(5);

/// The most sign-in data AMT's answer may hold: a realm, a nonce and a
/// qop, each under 256 bytes.
const AUTH_DATA_LIMIT: u32 = 1024;

/// Where AMT is and who signs in.
#[derive(Clone)]
pub struct Sign {
    /// AMT's redirection port, `host:port`.
    pub address: String,
    pub user: String,
    pub password: String,
}

/// Why a session did not open, or ended.
#[derive(Debug)]
enum Failed {
    /// Something that may pass: the network, AMT dozing, another console
    /// holding the session.
    Again(String),
    /// AMT turned the user down, or cannot do Serial-over-LAN: trying
    /// again at once changes nothing.
    Stop(String),
}

impl Failed {
    fn why(&self) -> &str {
        match self {
            Failed::Again(why) | Failed::Stop(why) => why,
        }
    }
}

/// Opens a session and closes it, as `machines check` does to see that
/// the console can be read.
///
/// # Errors
///
/// Why the session did not open.
pub fn try_session(sign: &Sign) -> Result<(), String> {
    Session::open(sign).map(drop).map_err(|failed| failed.why().to_string())
}

/// Reads one machine's Serial-over-LAN into the current log, until
/// dropped.
pub struct SolReader {
    log: ConsoleLog,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

/// What the reader and its thread share.
struct Shared {
    done: AtomicBool,
    hold: Mutex<HoldState>,
    changed: Condvar,
}

/// Whether the driver holds the session closed, and whether the thread
/// has closed it.
#[derive(Default)]
struct HoldState {
    held: bool,
    closed: bool,
}

/// The session closed, until this is dropped.
pub struct Held {
    shared: Arc<Shared>,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.shared.hold.lock().unwrap_or_else(PoisonError::into_inner).held = false;
        self.shared.changed.notify_all();
    }
}

impl SolReader {
    /// Opens a session with AMT as `sign` says, and reads it into `log`
    /// from then on; `notes` is the driver's log. Returns once the first
    /// session is open.
    ///
    /// # Errors
    ///
    /// Fails when AMT turns the sign-in down or cannot start
    /// Serial-over-LAN, when no session opens within [`FIRST_OPEN_TIME`],
    /// or when `log` cannot be made.
    pub fn start(sign: Sign, log: &std::path::Path, notes: DriverLog) -> Result<SolReader, String> {
        let log = ConsoleLog::create(log)?;
        let deadline = Instant::now() + FIRST_OPEN_TIME;
        let session = loop {
            match Session::open(&sign) {
                Ok(session) => break session,
                Err(Failed::Stop(why)) => return Err(why),
                Err(Failed::Again(why)) if Instant::now() >= deadline => {
                    return Err(format!(
                        "{why} (no Serial-over-LAN session opened in {} s)",
                        FIRST_OPEN_TIME.as_secs()
                    ));
                }
                Err(Failed::Again(_)) => std::thread::sleep(RETRY_GAP),
            }
        };
        notes.note(&format!("console: Serial-over-LAN session open at {}", sign.address));
        let shared = Arc::new(Shared {
            done: AtomicBool::new(false),
            hold: Mutex::new(HoldState::default()),
            changed: Condvar::new(),
        });
        let thread = {
            let (log, shared) = (log.clone(), Arc::clone(&shared));
            std::thread::spawn(move || keep_reading(session, &sign, &log, &notes, &shared))
        };
        Ok(SolReader { log, shared, thread: Some(thread) })
    }

    /// The log it appends to.
    pub fn log(&self) -> &ConsoleLog {
        &self.log
    }

    /// Closes the session, and keeps it closed until the [`Held`] is
    /// dropped, when it opens again. AMT turns some requests down while a
    /// redirection session is open, as it does power changes while a
    /// remote screen session is.
    pub fn hold(&self) -> Held {
        let mut state = self.shared.hold.lock().unwrap_or_else(PoisonError::into_inner);
        state.held = true;
        self.shared.changed.notify_all();
        // A thread busy opening a session closes it once it is open.
        let (state, _) = self
            .shared
            .changed
            .wait_timeout_while(state, HOLD_TIME, |state| !state.closed)
            .unwrap_or_else(PoisonError::into_inner);
        drop(state);
        Held { shared: Arc::clone(&self.shared) }
    }
}

impl Drop for SolReader {
    fn drop(&mut self) {
        self.shared.done.store(true, Ordering::Relaxed);
        self.shared.changed.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The thread: reads `session` into `log`, and opens another each time
/// one ends, until the reader is dropped.
fn keep_reading(first: Session, sign: &Sign, log: &ConsoleLog, notes: &DriverLog, shared: &Shared) {
    let mut session = Some(first);
    // When the last session ended, and when a failure to open one was
    // last noted.
    let mut ended: Option<Instant> = None;
    let mut noted: Option<Instant> = None;
    while !shared.done.load(Ordering::Relaxed) {
        if shared.hold.lock().unwrap_or_else(PoisonError::into_inner).held {
            session = None;
            wait_out_hold(shared, notes);
            continue;
        }
        let Some(open) = session.take() else {
            match Session::open(sign) {
                Ok(open) => {
                    let after = ended.take().map_or(String::new(), |ended| {
                        format!(
                            " again, {:.0} s after the last ended",
                            ended.elapsed().as_secs_f64()
                        )
                    });
                    notes.note(&format!("console: Serial-over-LAN session open{after}"));
                    noted = None;
                    session = Some(open);
                }
                Err(failed) => {
                    if noted.is_none_or(|noted| noted.elapsed() >= NOTE_AGAIN) {
                        notes.note(&format!(
                            "console: no Serial-over-LAN session: {}",
                            failed.why()
                        ));
                        noted = Some(Instant::now());
                    }
                    let gap =
                        if matches!(failed, Failed::Stop(_)) { REFUSED_GAP } else { RETRY_GAP };
                    pause(shared, gap);
                }
            }
            continue;
        };
        if let Ended::Failed(why) = read_session(open, log, shared) {
            notes.note(&format!("console: the Serial-over-LAN session ended: {why}"));
            ended = Some(Instant::now());
        }
    }
}

/// Tells [`SolReader::hold`] the session is closed, and waits until the
/// hold ends or the reader is dropped.
fn wait_out_hold(shared: &Shared, notes: &DriverLog) {
    notes.note("console: Serial-over-LAN session closed while the driver holds it");
    let mut state = shared.hold.lock().unwrap_or_else(PoisonError::into_inner);
    state.closed = true;
    shared.changed.notify_all();
    while state.held && !shared.done.load(Ordering::Relaxed) {
        state = shared.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
    }
    state.closed = false;
}

/// Sleeps `gap`, or less if the reader is dropped or the session held.
fn pause(shared: &Shared, gap: Duration) {
    let state = shared.hold.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = shared.changed.wait_timeout_while(state, gap, |state| {
        !state.held && !shared.done.load(Ordering::Relaxed)
    });
}

/// How a session's reading ended.
enum Ended {
    /// The reader was dropped.
    Done,
    /// The driver holds the session closed.
    Held,
    /// The session failed, for this reason.
    Failed(String),
}

/// Appends the text `session` brings to `log`, keeping it alive, until
/// it fails, the driver holds it, or the reader is dropped.
fn read_session(mut session: Session, log: &ConsoleLog, shared: &Shared) -> Ended {
    if let Err(err) = session.stream.set_read_timeout(Some(POLL_TIME)) {
        return Ended::Failed(err.to_string());
    }
    let mut pending = Vec::new();
    let mut buffer = [0u8; 4096];
    let mut kept_alive = Instant::now();
    loop {
        if shared.done.load(Ordering::Relaxed) {
            return Ended::Done;
        }
        if shared.hold.lock().unwrap_or_else(PoisonError::into_inner).held {
            return Ended::Held;
        }
        if kept_alive.elapsed() >= KEEP_ALIVE {
            if let Err(err) = session.send_numbered(KEEP_ALIVE_MESSAGE, &[]) {
                return Ended::Failed(format!("keeping it alive: {err}"));
            }
            kept_alive = Instant::now();
        }
        match session.stream.read(&mut buffer) {
            Ok(0) => return Ended::Failed("AMT closed the connection".into()),
            Ok(read) => pending.extend_from_slice(&buffer[..read]),
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                continue;
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Ended::Failed(err.to_string()),
        }
        loop {
            match take_message(&pending) {
                Ok(Some((Incoming::Text(text), used))) => {
                    log.append(text);
                    pending.drain(..used);
                }
                Ok(Some((Incoming::Other, used))) => {
                    pending.drain(..used);
                }
                Ok(None) => break,
                Err(why) => return Ended::Failed(why),
            }
        }
    }
}

/// A message from AMT once Serial-over-LAN has started.
#[derive(Debug, PartialEq, Eq)]
enum Incoming<'a> {
    /// What the system wrote.
    Text(&'a [u8]),
    /// A keep-alive or the serial settings, which need nothing done.
    Other,
}

/// The first whole message in `pending`, and how many bytes it takes;
/// `None` until it has all come.
///
/// # Errors
///
/// A message of a type the reader does not know: its length is unknown,
/// so nothing after it can be read.
fn take_message(pending: &[u8]) -> Result<Option<(Incoming<'_>, usize)>, String> {
    let Some(&kind) = pending.first() else { return Ok(None) };
    let whole = |length: usize| (pending.len() >= length).then_some(length);
    Ok(match kind {
        TEXT => {
            let Some(&[low, high]) = pending.get(8..10) else { return Ok(None) };
            let length = 10 + usize::from(u16::from_le_bytes([low, high]));
            whole(length).map(|length| (Incoming::Text(&pending[10..length]), length))
        }
        KEEP_ALIVE_MESSAGE => whole(8).map(|length| (Incoming::Other, length)),
        SOL_SETTINGS => whole(10).map(|length| (Incoming::Other, length)),
        other => {
            return Err(format!(
                "AMT sent a message of type {other:#04x}, which the console reader does not know"
            ));
        }
    })
}

/// One open Serial-over-LAN session.
struct Session {
    stream: TcpStream,
    /// The number the next numbered message carries.
    sequence: u32,
}

impl Session {
    /// Connects, signs in and starts Serial-over-LAN.
    fn open(sign: &Sign) -> Result<Session, Failed> {
        let again = |err: std::io::Error| Failed::Again(format!("AMT at {}: {err}", sign.address));
        let target = sign
            .address
            .to_socket_addrs()
            .map_err(|err| Failed::Stop(format!("AMT's address {}: {err}", sign.address)))?
            .next()
            .ok_or_else(|| Failed::Stop(format!("AMT's address {} is no address", sign.address)))?;
        let stream = TcpStream::connect_timeout(&target, CONNECT_TIME).map_err(again)?;
        stream.set_read_timeout(Some(ANSWER_TIME)).map_err(again)?;
        stream.set_write_timeout(Some(ANSWER_TIME)).map_err(again)?;
        stream.set_nodelay(true).map_err(again)?;
        let mut session = Session { stream, sequence: 0 };
        session.start(sign).map_err(|failed| match failed {
            Failed::Again(why) => Failed::Again(format!("AMT at {}: {why}", sign.address)),
            Failed::Stop(why) => Failed::Stop(format!("AMT at {}: {why}", sign.address)),
        })?;
        Ok(session)
    }

    /// The messages that open the session, in order.
    fn start(&mut self, sign: &Sign) -> Result<(), Failed> {
        let again = |err: std::io::Error| Failed::Again(err.to_string());
        self.stream.write_all(&[START_SESSION, 0, 0, 0, b'S', b'O', b'L', b' ']).map_err(again)?;
        // A session turned down may be answered with the first four bytes
        // alone.
        let reply = self.read(4).map_err(again)?;
        if reply[0] != START_SESSION_REPLY {
            return Err(Failed::Stop(format!(
                "the port answered as no AMT redirection service does (type {:#04x})",
                reply[0]
            )));
        }
        if reply[1] != 0 {
            return Err(Failed::Again(format!(
                "AMT turned the Serial-over-LAN session down (status {}): another console may \
                 hold it, such as a person's terminal or another run on this machine, or it is \
                 off in MEBx (Ctrl+P as the machine starts)",
                reply[1]
            )));
        }
        let rest = self.read(9).map_err(again)?;
        self.read(usize::from(rest[8])).map_err(again)?;
        self.sign_in(sign)?;
        let mut settings = Vec::with_capacity(16);
        for value in SESSION_SETTINGS {
            settings.extend_from_slice(&value.to_le_bytes());
        }
        settings.extend_from_slice(&[0; 4]);
        self.send_numbered(START_SOL, &settings).map_err(again)?;
        let reply = self.read(23).map_err(again)?;
        // Intel's consoles look only at the reply's type.
        if reply[0] != START_SOL_REPLY {
            return Err(Failed::Again(format!(
                "AMT answered the start of Serial-over-LAN with a message of type {:#04x}",
                reply[0]
            )));
        }
        self.send_numbered(LINE_CONTROL, &LINE_SIGNALS).map_err(again)
    }

    /// Signs in with HTTP digest.
    fn sign_in(&mut self, sign: &Sign) -> Result<(), Failed> {
        let again = |err: std::io::Error| Failed::Again(err.to_string());
        self.stream.write_all(&[AUTHENTICATE, 0, 0, 0, WAY_QUERY, 0, 0, 0, 0]).map_err(again)?;
        let (_, ways) = self.read_sign_in_answer()?;
        if !ways.contains(&WAY_DIGEST) {
            return Err(Failed::Stop(format!(
                "AMT offers no digest sign-in for Serial-over-LAN (it offers ways {ways:?}), and \
                 the lab does not send the password in the clear"
            )));
        }
        let empty = sign_in_message(&[&sign.user, "", "", DIGEST_URI, "", "", "", ""])?;
        self.stream.write_all(&empty).map_err(again)?;
        let (status, data) = self.read_sign_in_answer()?;
        if status != CHALLENGED {
            return Err(Failed::Stop(format!(
                "AMT gave no digest challenge for Serial-over-LAN (status {status})"
            )));
        }
        let challenge = Challenge::parse(&data).ok_or_else(|| {
            Failed::Stop("AMT's digest challenge for Serial-over-LAN does not read".into())
        })?;
        let cnonce = crate::netboot::random_hex(16).map_err(Failed::Stop)?;
        let response = challenge.response(&sign.user, &sign.password, &cnonce);
        let answer = sign_in_message(&[
            &sign.user,
            &challenge.realm,
            &challenge.nonce,
            DIGEST_URI,
            &cnonce,
            NONCE_COUNT,
            &response,
            &challenge.qop,
        ])?;
        self.stream.write_all(&answer).map_err(again)?;
        let (status, _) = self.read_sign_in_answer()?;
        if status != 0 {
            return Err(Failed::Stop(format!(
                "AMT did not take the user and password for Serial-over-LAN (status {status}): \
                 check the machine file's amt.user and the password in its amt.password_file"
            )));
        }
        Ok(())
    }

    /// An answer to a sign-in message: its status and its data.
    fn read_sign_in_answer(&mut self) -> Result<(u8, Vec<u8>), Failed> {
        let again = |err: std::io::Error| Failed::Again(err.to_string());
        let head = self.read(9).map_err(again)?;
        if head[0] != AUTHENTICATE_REPLY {
            return Err(Failed::Stop(format!(
                "AMT answered a sign-in with a message of type {:#04x}",
                head[0]
            )));
        }
        let length = u32::from_le_bytes([head[5], head[6], head[7], head[8]]);
        if length > AUTH_DATA_LIMIT {
            return Err(Failed::Stop(format!("AMT's sign-in answer claims {length} bytes")));
        }
        let data = self.read(length as usize).map_err(again)?;
        Ok((head[1], data))
    }

    /// Sends a message that carries a sequence number: its type, three
    /// zero bytes, the number, and `body`.
    fn send_numbered(&mut self, kind: u8, body: &[u8]) -> std::io::Result<()> {
        let mut message = Vec::with_capacity(8 + body.len());
        message.extend_from_slice(&[kind, 0, 0, 0]);
        message.extend_from_slice(&self.sequence.to_le_bytes());
        message.extend_from_slice(body);
        self.sequence = self.sequence.wrapping_add(1);
        self.stream.write_all(&message)
    }

    /// Reads exactly `length` bytes.
    fn read(&mut self, length: usize) -> std::io::Result<Vec<u8>> {
        let mut bytes = vec![0; length];
        self.stream.read_exact(&mut bytes)?;
        Ok(bytes)
    }
}

/// A sign-in message with digest: its fields, each after its length in
/// one byte.
///
/// # Errors
///
/// A field longer than 255 bytes, which no length byte holds.
fn sign_in_message(fields: &[&str]) -> Result<Vec<u8>, Failed> {
    let mut data = Vec::new();
    for field in fields {
        let length = u8::try_from(field.len()).map_err(|_| {
            Failed::Stop(format!(
                "a sign-in field is {} bytes, over the 255 AMT takes",
                field.len()
            ))
        })?;
        data.push(length);
        data.extend_from_slice(field.as_bytes());
    }
    let length = u32::try_from(data.len()).unwrap_or(u32::MAX);
    let mut message = vec![AUTHENTICATE, 0, 0, 0, WAY_DIGEST];
    message.extend_from_slice(&length.to_le_bytes());
    message.extend_from_slice(&data);
    Ok(message)
}

/// AMT's digest challenge.
#[derive(Debug, PartialEq, Eq)]
struct Challenge {
    realm: String,
    nonce: String,
    qop: String,
}

impl Challenge {
    /// Reads the realm, nonce and qop, each after its length in one byte.
    fn parse(data: &[u8]) -> Option<Challenge> {
        let mut rest = data;
        let mut field = || {
            let (&length, after) = rest.split_first()?;
            let text = after.get(..usize::from(length))?;
            rest = &after[usize::from(length)..];
            String::from_utf8(text.to_vec()).ok()
        };
        Some(Challenge { realm: field()?, nonce: field()?, qop: field()? })
    }

    /// The digest response for `user` and `password`.
    fn response(&self, user: &str, password: &str, cnonce: &str) -> String {
        Digest {
            user,
            password,
            realm: &self.realm,
            nonce: &self.nonce,
            method: DIGEST_METHOD,
            uri: DIGEST_URI,
            count: NONCE_COUNT,
            cnonce,
            qop: &self.qop,
        }
        .response()
    }
}

/// A pretend AMT redirection service for tests: the same messages and
/// digest sign-in, one Serial-over-LAN session at a time, and a machine
/// whose writes to its AMT port go to the open session or are lost.
#[cfg(test)]
pub mod fake {
    use std::io::{ErrorKind, Read as _, Write as _};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::{DIGEST_METHOD, QUIET_LIMIT};
    use crate::amt::fake::{PASSWORD, USER};
    use crate::wsman::Digest;

    const REALM: &str = "Digest:0123456789ABCDEF0123456789ABCDEF";

    /// The pretend service's state.
    pub struct State {
        /// Sessions started, and how many are open now.
        pub sessions: u32,
        pub live: u32,
        /// Keep-alives heard.
        pub keep_alives: u32,
        /// The ways of signing in offered.
        pub ways: Vec<u8>,
        /// How many of the next sessions are turned down, as AMT does one
        /// asked for while another console's is open.
        pub refusals: u32,
        /// Sign-ins turned down for a wrong password.
        pub wrong_passwords: u32,
        /// What the machine wrote that the open session has yet to carry.
        waiting: Vec<u8>,
        /// Raised to end the open session.
        ending: u32,
        nonce: u32,
    }

    /// A pretend redirection service, listening until dropped.
    pub struct FakeSol {
        pub address: SocketAddr,
        pub state: Arc<Mutex<State>>,
        done: Arc<AtomicBool>,
    }

    impl FakeSol {
        pub fn start() -> FakeSol {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let state = Arc::new(Mutex::new(State {
                sessions: 0,
                live: 0,
                keep_alives: 0,
                ways: vec![1, 3, 4],
                refusals: 0,
                wrong_passwords: 0,
                waiting: Vec::new(),
                ending: 0,
                nonce: 0,
            }));
            let done = Arc::new(AtomicBool::new(false));
            {
                let (state, done) = (Arc::clone(&state), Arc::clone(&done));
                std::thread::spawn(move || {
                    while !done.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                let (state, done) = (Arc::clone(&state), Arc::clone(&done));
                                std::thread::spawn(move || serve(stream, &state, &done));
                            }
                            Err(_) => std::thread::sleep(Duration::from_millis(10)),
                        }
                    }
                });
            }
            FakeSol { address, state, done }
        }

        /// The machine writes `bytes` to its AMT port: the open session
        /// carries them, and with none they are lost, as on AMT.
        pub fn say(&self, bytes: &[u8]) {
            let mut state = self.state.lock().unwrap();
            if state.live > 0 {
                state.waiting.extend_from_slice(bytes);
            }
        }

        /// Ends the open session, as AMT's engine restarting does.
        pub fn end_session(&self) {
            self.state.lock().unwrap().ending += 1;
        }

        /// Waits until `what` holds of the state.
        pub fn wait(&self, what: impl Fn(&State) -> bool) -> bool {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if what(&self.state.lock().unwrap()) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        }
    }

    impl Drop for FakeSol {
        fn drop(&mut self) {
            self.done.store(true, Ordering::Relaxed);
        }
    }

    fn read(stream: &mut TcpStream, length: usize) -> Option<Vec<u8>> {
        let mut bytes = vec![0; length];
        stream.read_exact(&mut bytes).ok()?;
        Some(bytes)
    }

    /// A sign-in message's way and fields.
    fn read_sign_in(stream: &mut TcpStream) -> Option<(u8, Vec<String>)> {
        let head = read(stream, 9)?;
        assert_eq!(head[..4], [0x13, 0, 0, 0]);
        let length = u32::from_le_bytes([head[5], head[6], head[7], head[8]]);
        let data = read(stream, length as usize)?;
        let mut fields = Vec::new();
        let mut rest = &data[..];
        while let Some((&length, after)) = rest.split_first() {
            let (field, after) = after.split_at(usize::from(length));
            fields.push(String::from_utf8(field.to_vec()).unwrap());
            rest = after;
        }
        Some((head[4], fields))
    }

    fn sign_in_answer(status: u8, way: u8, data: &[u8]) -> Vec<u8> {
        let mut answer = vec![0x14, status, 0, 0, way];
        answer.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        answer.extend_from_slice(data);
        answer
    }

    /// One connection: a session if the sign-in and the start go right.
    fn serve(mut stream: TcpStream, state: &Arc<Mutex<State>>, done: &AtomicBool) {
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let Some(start) = read(&mut stream, 8) else { return };
        assert_eq!(start, *b"\x10\0\0\0SOL ");
        let refused = {
            let mut state = state.lock().unwrap();
            let refused = state.live > 0 || state.refusals > 0;
            state.refusals = state.refusals.saturating_sub(1);
            refused
        };
        // Status, then the OEM data's length and the data.
        let mut reply = vec![0x11, u8::from(refused), 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0xAB, 0xCD];
        if refused {
            reply.truncate(4);
        }
        let _ = stream.write_all(&reply);
        if refused {
            return;
        }
        let Some((0, query)) = read_sign_in(&mut stream) else { return };
        assert!(query.is_empty());
        let ways = state.lock().unwrap().ways.clone();
        let _ = stream.write_all(&sign_in_answer(0, 0, &ways));
        let Some((4, empty)) = read_sign_in(&mut stream) else { return };
        assert_eq!(empty, [USER, "", "", "/RedirectionService", "", "", "", ""]);
        let nonce = {
            let mut state = state.lock().unwrap();
            state.nonce += 1;
            format!("sol-nonce-{}", state.nonce)
        };
        let mut challenge = Vec::new();
        for field in [REALM, &nonce, "auth"] {
            challenge.push(u8::try_from(field.len()).unwrap());
            challenge.extend_from_slice(field.as_bytes());
        }
        let _ = stream.write_all(&sign_in_answer(1, 4, &challenge));
        let Some((4, answer)) = read_sign_in(&mut stream) else { return };
        let [user, realm, said_nonce, uri, cnonce, count, response, qop] = &answer[..] else {
            panic!("a digest answer has eight fields: {answer:?}");
        };
        let expected = Digest {
            user: USER,
            password: PASSWORD,
            realm: REALM,
            nonce: &nonce,
            method: DIGEST_METHOD,
            uri: "/RedirectionService",
            count,
            cnonce,
            qop: "auth",
        }
        .response();
        assert_eq!((user.as_str(), realm.as_str(), qop.as_str()), (USER, REALM, "auth"));
        assert_eq!((said_nonce, uri.as_str(), cnonce.len()), (&nonce, "/RedirectionService", 32));
        if *response != expected {
            state.lock().unwrap().wrong_passwords += 1;
            let _ = stream.write_all(&sign_in_answer(3, 4, &[]));
            return;
        }
        let _ = stream.write_all(&sign_in_answer(0, 4, &[]));
        let Some(settings) = read(&mut stream, 24) else { return };
        assert_eq!(settings[..4], [0x20, 0, 0, 0]);
        let values: Vec<u16> =
            settings[8..20].chunks(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
        assert_eq!(values, [10_000, 100, 0, 10_000, 100, 0]);
        let ending = {
            let mut state = state.lock().unwrap();
            state.sessions += 1;
            state.live += 1;
            state.ending
        };
        let mut reply = vec![0x21];
        reply.resize(23, 0);
        let _ = stream.write_all(&reply);
        carry(&mut stream, state, done, ending);
        state.lock().unwrap().live -= 1;
    }

    /// The open session: hands on what the machine writes, and hears the
    /// console's messages, until either side ends it.
    fn carry(stream: &mut TcpStream, state: &Arc<Mutex<State>>, done: &AtomicBool, ending: u32) {
        stream.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
        let mut heard = Instant::now();
        let mut pending = Vec::new();
        let mut sequence = 0u32;
        while !done.load(Ordering::Relaxed) && heard.elapsed() < QUIET_LIMIT {
            let waiting = {
                let mut state = state.lock().unwrap();
                if state.ending != ending {
                    return;
                }
                std::mem::take(&mut state.waiting)
            };
            for text in waiting.chunks(usize::from(u16::MAX)) {
                let mut message = vec![0x2A, 0, 0, 0];
                message.extend_from_slice(&sequence.to_le_bytes());
                message.extend_from_slice(&u16::try_from(text.len()).unwrap().to_le_bytes());
                message.extend_from_slice(text);
                sequence += 1;
                // In two parts, as the network may bring it.
                let (first, second) = message.split_at(5);
                if stream.write_all(first).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(2));
                if stream.write_all(second).is_err() {
                    return;
                }
            }
            let mut buffer = [0u8; 256];
            match stream.read(&mut buffer) {
                Ok(0) => return,
                Ok(read) => pending.extend_from_slice(&buffer[..read]),
                Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(_) => return,
            }
            loop {
                let length = match pending.first() {
                    Some(0x27) => 14,
                    Some(0x2B) => 8,
                    Some(other) => panic!("the console sent a message of type {other:#04x}"),
                    None => break,
                };
                if pending.len() < length {
                    break;
                }
                if pending[0] == 0x2B {
                    state.lock().unwrap().keep_alives += 1;
                } else {
                    assert_eq!(pending[8..14], [0, 0, 0x1B, 0, 0, 0]);
                }
                pending.drain(..length);
                heard = Instant::now();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeSol;
    use super::*;
    use crate::amt::fake::{PASSWORD, USER};
    use crate::testing::TempDir;
    use std::fs;

    fn sign(fake: &FakeSol, password: &str) -> Sign {
        Sign { address: fake.address.to_string(), user: USER.into(), password: password.into() }
    }

    fn wait_for(path: &std::path::Path, text: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if fs::read(path).is_ok_and(|log| String::from_utf8_lossy(&log).contains(text)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    struct Bench {
        dir: TempDir,
        fake: FakeSol,
        reader: SolReader,
    }

    impl Bench {
        fn new(label: &str) -> Bench {
            let dir = TempDir::new(label);
            let fake = FakeSol::start();
            let notes = DriverLog::open(&dir.path().join("amt.log")).unwrap();
            let reader =
                SolReader::start(sign(&fake, PASSWORD), &dir.path().join("serial-1.log"), notes)
                    .unwrap();
            Bench { dir, fake, reader }
        }

        fn log(&self, name: &str) -> std::path::PathBuf {
            self.dir.path().join(name)
        }

        fn notes(&self) -> String {
            fs::read_to_string(self.log("amt.log")).unwrap()
        }
    }

    #[test]
    fn reads_what_the_machine_writes_and_keeps_the_session_alive() {
        let bench = Bench::new("sol-read");
        assert_eq!(bench.fake.state.lock().unwrap().live, 1);
        bench.fake.say(b"\xffBoot complete.\r\n");
        assert!(wait_for(&bench.log("serial-1.log"), "Boot complete."));
        // Bytes as the machine wrote them, noise and all.
        assert_eq!(fs::read(bench.log("serial-1.log")).unwrap(), b"\xffBoot complete.\r\n");
        // The log the driver names next gets what comes next.
        bench.reader.log().switch_to(&bench.log("serial-2.log")).unwrap();
        bench.fake.say(b"second boot\n");
        assert!(wait_for(&bench.log("serial-2.log"), "second boot"));
        assert_eq!(fs::read(bench.log("serial-1.log")).unwrap(), b"\xffBoot complete.\r\n");
        assert!(bench.fake.wait(|state| state.keep_alives >= 3));
        let notes = bench.notes();
        assert!(notes.contains("console: Serial-over-LAN session open at 127.0.0.1:"), "{notes}");
        assert!(!notes.contains(PASSWORD), "{notes}");
    }

    #[test]
    fn a_session_that_ends_is_opened_again() {
        let bench = Bench::new("sol-again");
        bench.fake.end_session();
        assert!(bench.fake.wait(|state| state.sessions == 2 && state.live == 1));
        bench.fake.say(b"after\n");
        assert!(wait_for(&bench.log("serial-1.log"), "after"));
        let notes = bench.notes();
        assert!(
            notes.contains("console: the Serial-over-LAN session ended: AMT closed"),
            "{notes}"
        );
        assert!(
            notes.contains("console: Serial-over-LAN session open again, 0 s after"),
            "{notes}"
        );
    }

    #[test]
    fn a_held_session_stays_closed_until_let_go() {
        let bench = Bench::new("sol-hold");
        let held = bench.reader.hold();
        // AMT sees the connection close a moment later.
        assert!(bench.fake.wait(|state| state.live == 0));
        // What the machine writes with no session open is lost.
        bench.fake.say(b"lost\n");
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(bench.fake.state.lock().unwrap().sessions, 1);
        drop(held);
        assert!(bench.fake.wait(|state| state.sessions == 2 && state.live == 1));
        bench.fake.say(b"found\n");
        assert!(wait_for(&bench.log("serial-1.log"), "found"));
        assert!(!fs::read_to_string(bench.log("serial-1.log")).unwrap().contains("lost"));
        let notes = bench.notes();
        assert!(notes.contains("closed while the driver holds it"), "{notes}");
    }

    #[test]
    fn a_session_amt_turns_down_is_asked_for_again() {
        let dir = TempDir::new("sol-busy");
        let fake = FakeSol::start();
        fake.state.lock().unwrap().refusals = 3;
        let notes = DriverLog::open(&dir.path().join("amt.log")).unwrap();
        let reader =
            SolReader::start(sign(&fake, PASSWORD), &dir.path().join("serial.log"), notes).unwrap();
        assert_eq!(fake.state.lock().unwrap().sessions, 1);
        // Only one console at a time: a second is turned down until the
        // first is gone.
        let notes = DriverLog::open(&dir.path().join("amt-2.log")).unwrap();
        let second = std::thread::spawn({
            let sign = sign(&fake, PASSWORD);
            let log = dir.path().join("serial-2.log");
            move || SolReader::start(sign, &log, notes).map(drop)
        });
        std::thread::sleep(Duration::from_millis(300));
        drop(reader);
        second.join().unwrap().unwrap();
        assert_eq!(fake.state.lock().unwrap().sessions, 2);
    }

    #[test]
    fn a_wrong_password_ends_the_start_at_once() {
        let dir = TempDir::new("sol-password");
        let fake = FakeSol::start();
        let notes = DriverLog::open(&dir.path().join("amt.log")).unwrap();
        let started = Instant::now();
        let Err(err) = SolReader::start(sign(&fake, "wrong"), &dir.path().join("s.log"), notes)
        else {
            panic!("a wrong password started a session");
        };
        assert!(err.contains("did not take the user and password"), "{err}");
        assert!(err.contains("amt.password_file"), "{err}");
        // Once: AMT locks out a user after several wrong passwords.
        assert_eq!(fake.state.lock().unwrap().wrong_passwords, 1);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn the_password_is_never_sent_in_the_clear() {
        let dir = TempDir::new("sol-basic");
        let fake = FakeSol::start();
        fake.state.lock().unwrap().ways = vec![1];
        let notes = DriverLog::open(&dir.path().join("amt.log")).unwrap();
        let Err(err) = SolReader::start(sign(&fake, PASSWORD), &dir.path().join("s.log"), notes)
        else {
            panic!("signed in without digest");
        };
        assert!(err.contains("offers no digest sign-in"), "{err}");
    }

    #[test]
    fn no_amt_there_says_so_once_the_first_session_is_overdue() {
        let dir = TempDir::new("sol-nothing");
        // A port nothing listens on.
        let address = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let notes = DriverLog::open(&dir.path().join("amt.log")).unwrap();
        let sign =
            Sign { address: address.to_string(), user: USER.into(), password: PASSWORD.into() };
        let Err(err) = SolReader::start(sign, &dir.path().join("s.log"), notes) else {
            panic!("a session opened with nothing there");
        };
        assert!(err.contains(&format!("AMT at {address}")), "{err}");
        assert!(err.contains("no Serial-over-LAN session opened in 3 s"), "{err}");
    }

    #[test]
    fn messages_are_taken_whole() {
        let text = [0x2A, 0, 0, 0, 7, 0, 0, 0, 3, 0, b'a', b'b', b'c'];
        assert_eq!(take_message(&text), Ok(Some((Incoming::Text(b"abc"), 13))));
        // Not yet whole: the header, or the text, cut short.
        assert_eq!(take_message(&text[..9]), Ok(None));
        assert_eq!(take_message(&text[..12]), Ok(None));
        assert_eq!(take_message(&[]), Ok(None));
        let keep_alive = [0x2B, 0, 0, 0, 1, 0, 0, 0, 0x2A];
        assert_eq!(take_message(&keep_alive), Ok(Some((Incoming::Other, 8))));
        assert_eq!(take_message(&keep_alive[..7]), Ok(None));
        let settings = [0x29, 0, 0, 0, 2, 0, 0, 0, 0, 0];
        assert_eq!(take_message(&settings), Ok(Some((Incoming::Other, 10))));
        let err = take_message(&[0x55, 0, 0]).unwrap_err();
        assert!(err.contains("type 0x55"), "{err}");
    }

    #[test]
    fn reads_amt_s_challenge() {
        let challenge = Challenge::parse(b"\x05realm\x05nonce\x04auth").unwrap();
        assert_eq!(
            challenge,
            Challenge { realm: "realm".into(), nonce: "nonce".into(), qop: "auth".into() }
        );
        assert_eq!(Challenge::parse(b"\x05realm\x05non"), None);
        assert_eq!(Challenge::parse(b"\x05realm\x05nonce"), None);
        assert!(matches!(sign_in_message(&[&"x".repeat(256)]), Err(Failed::Stop(_))));
        let message = sign_in_message(&["ab", ""]).unwrap();
        assert_eq!(message, [0x13, 0, 0, 0, 4, 4, 0, 0, 0, 2, b'a', b'b', 0]);
    }
}

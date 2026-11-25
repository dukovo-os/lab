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
//! The session is one of AMT's redirection service's (see redirection.rs,
//! which opens it and signs in), over TLS from AMT release 16.1 on. The
//! messages here are those Intel's own open-source consoles send:
//!
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

use std::io::{ErrorKind, Read as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::driver::real::DriverLog;
use crate::redirection::{Failed, Kind, Session, Sign};
use crate::serial::ConsoleLog;

const START_SOL: u8 = 0x20;
const START_SOL_REPLY: u8 = 0x21;
const LINE_CONTROL: u8 = 0x27;
const SOL_SETTINGS: u8 = 0x29;
const TEXT: u8 = 0x2A;
const KEEP_ALIVE_MESSAGE: u8 = 0x2B;

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
pub(crate) const QUIET_LIMIT: Duration = Duration::from_millis(QUIET_LIMIT_MS as u64);

/// How often the reader keeps the session alive: well inside
/// [`QUIET_LIMIT`].
const KEEP_ALIVE: Duration =
    if cfg!(test) { Duration::from_millis(100) } else { Duration::from_secs(2) };
const _: () = assert!(KEEP_ALIVE.as_millis() * 4 <= QUIET_LIMIT.as_millis());

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

/// Opens a redirection session and starts Serial-over-LAN on it.
fn open(sign: &Sign) -> Result<Session, Failed> {
    let mut session = Session::open(sign, Kind::SerialOverLan)?;
    start(&mut session)
        .map_err(|failed| failed.map(|why| format!("AMT at {}: {why}", sign.address)))?;
    Ok(session)
}

/// Starts Serial-over-LAN on a session signed in to.
fn start(session: &mut Session) -> Result<(), Failed> {
    let again = |err: std::io::Error| Failed::Again(err.to_string());
    let mut settings = Vec::with_capacity(16);
    for value in SESSION_SETTINGS {
        settings.extend_from_slice(&value.to_le_bytes());
    }
    settings.extend_from_slice(&[0; 4]);
    session.send_numbered(START_SOL, &settings).map_err(again)?;
    let reply = session.read(23).map_err(again)?;
    // Intel's consoles look only at the reply's type.
    if reply[0] != START_SOL_REPLY {
        return Err(Failed::Again(format!(
            "AMT answered the start of Serial-over-LAN with a message of type {:#04x}",
            reply[0]
        )));
    }
    session.send_numbered(LINE_CONTROL, &LINE_SIGNALS).map_err(again)
}

/// Opens a session and closes it, as `machines check` does to see that
/// the console can be read.
///
/// # Errors
///
/// Why the session did not open.
pub fn try_session(sign: &Sign) -> Result<(), String> {
    open(sign).map(drop).map_err(|failed| failed.why().to_string())
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
            match open(&sign) {
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
            match open(sign) {
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
    if let Err(err) = session.link.socket().set_read_timeout(Some(POLL_TIME)) {
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
        match session.link.read(&mut buffer) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amt::fake::{PASSWORD, USER};
    use crate::link::Tls;
    use crate::link::fake::FakeTls;
    use crate::redirection::fake::FakeRedirection;
    use crate::testing::TempDir;
    use std::fs;

    fn sign(fake: &FakeRedirection, password: &str) -> Sign {
        Sign {
            address: fake.address.to_string(),
            user: USER.into(),
            password: password.into(),
            tls: None,
        }
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
        fake: FakeRedirection,
        reader: SolReader,
    }

    impl Bench {
        fn new(label: &str) -> Bench {
            let dir = TempDir::new(label);
            let fake = FakeRedirection::start();
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
    fn reads_the_console_over_tls() {
        let dir = TempDir::new("sol-tls");
        let fake = FakeRedirection::start();
        let front = FakeTls::start(fake.address);
        let sign = Sign {
            address: front.address.to_string(),
            tls: Some(Tls::new(&front.pin).unwrap()),
            ..sign(&fake, PASSWORD)
        };
        let notes = DriverLog::open(&dir.path().join("amt.log")).unwrap();
        let log = dir.path().join("serial.log");
        let _reader = SolReader::start(sign, &log, notes).unwrap();
        fake.say(b"over TLS\n");
        assert!(wait_for(&log, "over TLS"));
        // A session AMT ends with no TLS close_notify is opened again.
        fake.end_session();
        assert!(fake.wait(|state| state.sessions == 2 && state.live == 1));
        fake.say(b"and again\n");
        assert!(wait_for(&log, "and again"));
        assert!(fake.wait(|state| state.keep_alives >= 2));
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
        let fake = FakeRedirection::start();
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
        let fake = FakeRedirection::start();
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
        let fake = FakeRedirection::start();
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
        let sign = Sign {
            address: address.to_string(),
            user: USER.into(),
            password: PASSWORD.into(),
            tls: None,
        };
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
}

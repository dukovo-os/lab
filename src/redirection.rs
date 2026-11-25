// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A session with Intel AMT's redirection service, which carries both its
//! Serial-over-LAN (see sol.rs) and its remote screen (see vnc.rs).
//!
//! The service listens on TCP port 16994, beside WS-Management's 16992,
//! and from AMT release 16.1 on only on 16995, over TLS (see link.rs).
//! Intel documents it only through its SDK's library; the messages here
//! are those Intel's own open-source consoles send. Numbers are
//! little-endian, and each message starts with its type:
//!
//! - **0x10** asks for a session of a kind: `SOL ` for Serial-over-LAN,
//!   `KVMR` for the remote screen. AMT answers 0x11: a status, and 13
//!   bytes in all, the last the length of OEM data after them.
//! - **0x13** signs in. The first asks which ways AMT takes (0x14 lists
//!   them); the session signs in only with HTTP digest with a client
//!   nonce (way 4, RFC 2617 with `qop`), never with the plain password
//!   (way 1). An empty first try brings AMT's realm, nonce and qop with
//!   status 1; the answer to them, for the URI `/RedirectionService` and
//!   the method `POST`, brings status 0.
//!
//! What follows is the kind's own: Serial-over-LAN's messages, or the
//! remote screen's RFB, which then needs no sign-in of its own.
//!
//! When a session will not open, the driver's log (`amt.log`) says why,
//! never with the password.

use std::io::{Read as _, Write as _};
use std::time::Duration;

use crate::link::{self, Link, Target, Tls};
use crate::wsman::Digest;

/// AMT's ports for redirection sessions, without TLS and with it.
pub const PORT: u16 = 16994;
pub const TLS_PORT: u16 = 16995;

const START_SESSION: u8 = 0x10;
const START_SESSION_REPLY: u8 = 0x11;
const AUTHENTICATE: u8 = 0x13;
const AUTHENTICATE_REPLY: u8 = 0x14;

/// The ways of signing in: the question of which AMT takes, and HTTP
/// digest with a client nonce.
const WAY_QUERY: u8 = 0;
const WAY_DIGEST: u8 = 4;
/// The status of a sign-in AMT has answered with its challenge.
const CHALLENGED: u8 = 1;
/// What a digest answer is for, as Intel's consoles sign in.
const DIGEST_URI: &str = "/RedirectionService";
pub(crate) const DIGEST_METHOD: &str = "POST";
const NONCE_COUNT: &str = "00000002";

/// How long connecting, and each answer while a session opens, may take.
const CONNECT_TIME: Duration = Duration::from_secs(5);
const ANSWER_TIME: Duration = Duration::from_secs(10);

/// The most sign-in data AMT's answer may hold: a realm, a nonce and a
/// qop, each under 256 bytes.
const AUTH_DATA_LIMIT: u32 = 1024;

/// Which session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    SerialOverLan,
    Screen,
}

impl Kind {
    /// The message that asks for it, as Intel's consoles send it.
    fn request(self) -> [u8; 8] {
        match self {
            Kind::SerialOverLan => [START_SESSION, 0, 0, 0, b'S', b'O', b'L', b' '],
            Kind::Screen => [START_SESSION, 1, 0, 0, b'K', b'V', b'M', b'R'],
        }
    }

    /// Its name, for messages.
    pub fn name(self) -> &'static str {
        match self {
            Kind::SerialOverLan => "Serial-over-LAN",
            Kind::Screen => "the remote screen",
        }
    }

    /// Who else may hold the session, and where it is turned off.
    fn holders(self) -> &'static str {
        match self {
            Kind::SerialOverLan => {
                "another console may hold it, such as a person's terminal or another run on \
                 this machine, or it is off in MEBx (Ctrl+P as the machine starts)"
            }
            Kind::Screen => {
                "another viewer may hold it, such as a person's VNC viewer or another run on \
                 this machine, or KVM is off in MEBx (Ctrl+P as the machine starts)"
            }
        }
    }
}

/// Where AMT is and who signs in.
#[derive(Clone)]
pub struct Sign {
    /// AMT's redirection port, `host:port`.
    pub address: String,
    pub user: String,
    pub password: String,
    /// The TLS the session is made with: AMT's from release 16.1 on.
    pub tls: Option<Tls>,
}

/// Why a session did not open, or ended.
#[derive(Debug)]
pub enum Failed {
    /// Something that may pass: the network, AMT dozing, another console
    /// holding the session.
    Again(String),
    /// AMT turned the user down, or cannot do the kind of session, or its
    /// TLS key is not the pinned one: trying again at once changes nothing.
    Stop(String),
}

impl Failed {
    pub fn why(&self) -> &str {
        match self {
            Failed::Again(why) | Failed::Stop(why) => why,
        }
    }

    /// The same failure, its reason told to `say`.
    pub fn map(self, say: impl FnOnce(String) -> String) -> Failed {
        match self {
            Failed::Again(why) => Failed::Again(say(why)),
            Failed::Stop(why) => Failed::Stop(say(why)),
        }
    }
}

/// An open session, signed in.
pub struct Session {
    pub link: Link,
    /// The number the next numbered message carries.
    sequence: u32,
}

impl Session {
    /// Connects, asks for a session of `kind` and signs in.
    ///
    /// # Errors
    ///
    /// Why it did not open, after `AMT at <address>:`.
    pub fn open(sign: &Sign, kind: Kind) -> Result<Session, Failed> {
        let target = Target { address: sign.address.clone(), tls: sign.tls.clone() };
        let link = target.connect(CONNECT_TIME, ANSWER_TIME, Duration::ZERO).map_err(|err| {
            let why = format!("AMT at {}: {err}", sign.address);
            if link::is_lasting(&err) { Failed::Stop(why) } else { Failed::Again(why) }
        })?;
        let again = |err: std::io::Error| Failed::Again(format!("AMT at {}: {err}", sign.address));
        link.socket().set_nodelay(true).map_err(again)?;
        let mut session = Session { link, sequence: 0 };
        session
            .start(sign, kind)
            .map_err(|failed| failed.map(|why| format!("AMT at {}: {why}", sign.address)))?;
        Ok(session)
    }

    /// Asks for the session and signs in.
    fn start(&mut self, sign: &Sign, kind: Kind) -> Result<(), Failed> {
        let again = |err: std::io::Error| Failed::Again(err.to_string());
        self.link.write_all(&kind.request()).map_err(again)?;
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
                "AMT turned the session for {} down (status {}): {}",
                kind.name(),
                reply[1],
                kind.holders()
            )));
        }
        let rest = self.read(9).map_err(again)?;
        self.read(usize::from(rest[8])).map_err(again)?;
        self.sign_in(sign, kind)
    }

    /// Signs in with HTTP digest.
    fn sign_in(&mut self, sign: &Sign, kind: Kind) -> Result<(), Failed> {
        let again = |err: std::io::Error| Failed::Again(err.to_string());
        self.link.write_all(&[AUTHENTICATE, 0, 0, 0, WAY_QUERY, 0, 0, 0, 0]).map_err(again)?;
        let (_, ways) = self.read_sign_in_answer()?;
        if !ways.contains(&WAY_DIGEST) {
            return Err(Failed::Stop(format!(
                "AMT offers no digest sign-in for {} (it offers ways {ways:?}), and the lab \
                 does not send the password in the clear",
                kind.name()
            )));
        }
        let empty = sign_in_message(&[&sign.user, "", "", DIGEST_URI, "", "", "", ""])?;
        self.link.write_all(&empty).map_err(again)?;
        let (status, data) = self.read_sign_in_answer()?;
        if status != CHALLENGED {
            return Err(Failed::Stop(format!(
                "AMT gave no digest challenge for {} (status {status})",
                kind.name()
            )));
        }
        let challenge = Challenge::parse(&data).ok_or_else(|| {
            Failed::Stop(format!("AMT's digest challenge for {} does not read", kind.name()))
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
        self.link.write_all(&answer).map_err(again)?;
        let (status, _) = self.read_sign_in_answer()?;
        if status != 0 {
            return Err(Failed::Stop(format!(
                "AMT did not take the user and password for {} (status {status}): check the \
                 machine file's amt.user and the password in its amt.password_file",
                kind.name()
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
    pub fn send_numbered(&mut self, kind: u8, body: &[u8]) -> std::io::Result<()> {
        let mut message = Vec::with_capacity(8 + body.len());
        message.extend_from_slice(&[kind, 0, 0, 0]);
        message.extend_from_slice(&self.sequence.to_le_bytes());
        message.extend_from_slice(body);
        self.sequence = self.sequence.wrapping_add(1);
        self.link.write_all(&message)
    }

    /// Reads exactly `length` bytes.
    pub fn read(&mut self, length: usize) -> std::io::Result<Vec<u8>> {
        let mut bytes = vec![0; length];
        self.link.read_exact(&mut bytes)?;
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
/// whose writes to its AMT port go to the open session or are lost; and
/// remote screen sessions, whose RFB it carries to and from a pretend VNC
/// server, as AMT carries its own.
#[cfg(test)]
pub mod fake {
    use std::io::{ErrorKind, Read as _, Write as _};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::DIGEST_METHOD;
    use crate::amt::fake::{PASSWORD, USER};
    use crate::sol::QUIET_LIMIT;
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
        /// The pretend VNC server remote screen sessions are carried to,
        /// and how many of them have opened.
        pub screen: Option<SocketAddr>,
        pub screen_sessions: u32,
    }

    /// A pretend redirection service, listening until dropped.
    pub struct FakeRedirection {
        pub address: SocketAddr,
        pub state: Arc<Mutex<State>>,
        done: Arc<AtomicBool>,
    }

    impl FakeRedirection {
        pub fn start() -> FakeRedirection {
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
                screen: None,
                screen_sessions: 0,
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
            FakeRedirection { address, state, done }
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

    impl Drop for FakeRedirection {
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
        let screen = match &start[..] {
            b"\x10\0\0\0SOL " => false,
            b"\x10\x01\0\0KVMR" => true,
            other => panic!("a session of no kind AMT has: {other:?}"),
        };
        let refused = {
            let mut state = state.lock().unwrap();
            let refused = (!screen && state.live > 0) || state.refusals > 0;
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
        if screen {
            show_screen(stream, state);
            return;
        }
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

    /// A remote screen session: RFB both ways between the console and the
    /// pretend VNC server, until either closes.
    fn show_screen(stream: TcpStream, state: &Arc<Mutex<State>>) {
        let behind = {
            let mut state = state.lock().unwrap();
            state.screen_sessions += 1;
            state.screen.expect("a remote screen session with no pretend screen behind it")
        };
        let Ok(server) = TcpStream::connect(behind) else { return };
        let pump = |mut from: TcpStream, mut to: TcpStream| {
            std::thread::spawn(move || {
                let _ = from.set_read_timeout(None);
                let _ = std::io::copy(&mut from, &mut to);
                let _ = to.shutdown(std::net::Shutdown::Both);
            })
        };
        let up = pump(stream.try_clone().unwrap(), server.try_clone().unwrap());
        let down = pump(server, stream);
        let _ = up.join();
        let _ = down.join();
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
    use super::*;

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

    #[test]
    fn each_kind_asks_for_its_own_session() {
        assert_eq!(&Kind::SerialOverLan.request(), b"\x10\0\0\0SOL ");
        assert_eq!(&Kind::Screen.request(), b"\x10\x01\0\0KVMR");
    }
}

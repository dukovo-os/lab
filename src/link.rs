// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Connections to a machine's controller: plain TCP, or TLS that trusts
//! one key alone.
//!
//! Intel AMT from release 16.1 on (the 12th generation Core's, and older
//! machines whose firmware is updated) serves WS-Management and its
//! redirection sessions only over TLS, on ports 16993 and 16995, with a
//! certificate its engine makes for itself, which no authority a client
//! knows has signed. So a [`Tls`] trusts exactly one key, the one the
//! machine file pins, named as curl's `pinnedpubkey` names it: `sha256//`
//! and the base64 of the SHA-256 hash of the certificate's
//! SubjectPublicKeyInfo. That is the pin `openssl` reads
//! ([`crate::redfish::pin_of`]), so `machines check` can print it on the
//! first setup. The certificate's names and dates are not looked at: AMT's
//! certificate names no address the lab uses, and its clock may be wrong.
//! The key is what shows the lab is talking to its machine, and the
//! handshake's signature is checked against it, with ring's algorithms, so
//! a machine in between that offers a copy of the certificate cannot pass.
//!
//! When a connection fails because the key is not the pinned one, the
//! error gives the pin that was offered: AMT makes itself a new
//! certificate when its firmware is updated, and the person who checks it
//! is the machine's puts that pin in the file.
//!
//! A [`Link`] is either kind of stream; its socket's timeouts bound its
//! reads and writes, the TLS handshake's too.

use std::fmt;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs as _};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{
    CertificateError, ClientConfig, ClientConnection, DigitallySignedStruct, OtherError,
    SignatureScheme, StreamOwned,
};

use crate::hash::{Hasher as _, Sha256};
use crate::redteam::base64;

/// How long [`Target::connect`] waits between tries while a server
/// refuses.
const REFUSED_GAP: Duration = Duration::from_millis(500);

/// TLS that trusts only the key a machine file pins.
#[derive(Clone)]
pub struct Tls {
    config: Arc<ClientConfig>,
}

impl Tls {
    /// TLS 1.2 or 1.3 to a server whose key has pin `pin`.
    ///
    /// # Errors
    ///
    /// Fails when rustls has no protocol version ring can do, which only a
    /// broken build gives.
    pub fn new(pin: &str) -> Result<Tls, String> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = PinnedKey {
            pin: pin.to_string(),
            algorithms: provider.signature_verification_algorithms,
        };
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|err| format!("setting up TLS: {err}"))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        Ok(Tls { config: Arc::new(config) })
    }

    /// Makes the handshake over `socket` with `host`.
    fn handshake(&self, mut socket: TcpStream, host: &str) -> io::Result<Link> {
        let name = ServerName::try_from(host.to_string()).map_err(|_| {
            io::Error::new(ErrorKind::InvalidInput, format!("{host} is no host name or address"))
        })?;
        let mut connection = ClientConnection::new(Arc::clone(&self.config), name)
            .map_err(|err| io::Error::new(ErrorKind::InvalidData, err))?;
        while connection.is_handshaking() {
            connection.complete_io(&mut socket).map_err(explain)?;
        }
        Ok(Link::Tls(Box::new(StreamOwned::new(connection, socket))))
    }
}

/// Where a connection goes: `host:port`, and the TLS it is made with, if
/// any.
#[derive(Clone)]
pub struct Target {
    pub address: String,
    pub tls: Option<Tls>,
}

impl Target {
    /// Plain TCP to `address`.
    pub fn plain(address: &str) -> Target {
        Target { address: address.to_string(), tls: None }
    }

    /// Connects, giving up when connecting takes longer than
    /// `connect_time`, and with `answer_time` as the bound of each read and
    /// write from then on. A server that refuses the connection is tried
    /// again until `patience` has passed: AMT refuses for a moment when its
    /// engine has dozed off with the machine powered off.
    ///
    /// # Errors
    ///
    /// The network's error, or, for which [`is_lasting`] holds, an address
    /// that is no address or a TLS handshake that failed.
    pub fn connect(
        &self,
        connect_time: Duration,
        answer_time: Duration,
        patience: Duration,
    ) -> io::Result<Link> {
        let no_address = |why: String| io::Error::new(ErrorKind::InvalidInput, why);
        let to = self
            .address
            .to_socket_addrs()
            .map_err(|err| no_address(err.to_string()))?
            .next()
            .ok_or_else(|| no_address("it is no address".into()))?;
        let deadline = Instant::now() + patience;
        let socket = loop {
            match TcpStream::connect_timeout(&to, connect_time) {
                Ok(socket) => break socket,
                Err(err)
                    if err.kind() == ErrorKind::ConnectionRefused && Instant::now() < deadline =>
                {
                    std::thread::sleep(REFUSED_GAP);
                }
                Err(err) => return Err(err),
            }
        };
        socket.set_read_timeout(Some(answer_time))?;
        socket.set_write_timeout(Some(answer_time))?;
        match &self.tls {
            None => Ok(Link::Plain(socket)),
            Some(tls) => {
                let host = self.address.rsplit_once(':').map_or(&*self.address, |(host, _)| host);
                tls.handshake(socket, host)
            }
        }
    }
}

/// Whether a failure from [`Target::connect`] would come again on another
/// try: an address that is no address, or TLS the server will not make
/// with the lab's terms, a key that is not the pinned one among them.
pub fn is_lasting(err: &io::Error) -> bool {
    matches!(err.kind(), ErrorKind::InvalidInput | ErrorKind::InvalidData)
}

/// A connection, plain or TLS.
pub enum Link {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Link {
    /// The socket under the connection, for its timeouts and options.
    pub fn socket(&self) -> &TcpStream {
        match self {
            Link::Plain(socket) => socket,
            Link::Tls(stream) => &stream.sock,
        }
    }

    /// Whether a read would end at once: the server sent something, or
    /// closed the connection. Over TLS, only what the server sent as data
    /// counts, so a record TLS keeps to itself does not leave a read
    /// waiting for data that never comes.
    ///
    /// # Errors
    ///
    /// The socket's error, or TLS the server broke.
    pub fn has_input(&mut self) -> io::Result<bool> {
        let socket = self.socket();
        socket.set_nonblocking(true)?;
        let looked = match self {
            Link::Plain(socket) => match socket.peek(&mut [0u8; 1]) {
                Ok(_) => Ok(true),
                Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(false),
                Err(err) => Err(err),
            },
            Link::Tls(stream) => {
                let StreamOwned { conn, sock } = &mut **stream;
                match conn.read_tls(sock) {
                    Ok(0) => Ok(true),
                    Ok(_) => Ok(false),
                    Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(false),
                    Err(err) => Err(err),
                }
                .and_then(|closed| {
                    let state = conn
                        .process_new_packets()
                        .map_err(|err| io::Error::new(ErrorKind::InvalidData, err))?;
                    Ok(closed || state.plaintext_bytes_to_read() > 0 || state.peer_has_closed())
                })
            }
        };
        self.socket().set_nonblocking(false)?;
        looked
    }
}

impl Read for Link {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Link::Plain(socket) => socket.read(buffer),
            // AMT may close a connection with no close_notify. What came
            // before is judged as a plain connection's would be: an answer
            // cut short is found short by what it says of its length.
            Link::Tls(stream) => match stream.read(buffer) {
                Err(err) if err.kind() == ErrorKind::UnexpectedEof => Ok(0),
                read => read,
            },
        }
    }
}

impl Write for Link {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Link::Plain(socket) => socket.write(bytes),
            Link::Tls(stream) => stream.write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Link::Plain(socket) => socket.flush(),
            Link::Tls(stream) => stream.flush(),
        }
    }
}

/// The pin of the key in `certificate`.
///
/// # Errors
///
/// A certificate that does not read as X.509.
pub fn pin_of(certificate: &CertificateDer<'_>) -> Result<String, rustls::Error> {
    let parsed = ParsedCertificate::try_from(certificate)?;
    let mut hash = Sha256::new();
    hash.update(parsed.subject_public_key_info().as_ref());
    Ok(format!("sha256//{}", base64(&hash.finish())))
}

/// A key that was not the pinned one.
#[derive(Debug)]
struct Unpinned {
    offered: String,
}

impl fmt::Display for Unpinned {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "the key it offers, pin {}, is not the one pinned", self.offered)
    }
}

impl std::error::Error for Unpinned {}

/// A handshake's failure, in words: a key that is not the pinned one says
/// which it was.
fn explain(err: io::Error) -> io::Error {
    if err.kind() != ErrorKind::InvalidData {
        return err;
    }
    let unpinned =
        err.get_ref().and_then(|inner| inner.downcast_ref::<rustls::Error>()).and_then(|tls| {
            match tls {
                rustls::Error::InvalidCertificate(CertificateError::Other(other)) => {
                    other.0.downcast_ref::<Unpinned>()
                }
                _ => None,
            }
        });
    let why = match unpinned {
        Some(unpinned) => format!(
            "its TLS key is not the one the machine file pins: it offers pin {}; if it is \
             the machine's own (its firmware makes a new one when updated), put that pin in \
             the file",
            unpinned.offered
        ),
        None => format!("the TLS handshake failed: {err}"),
    };
    io::Error::new(ErrorKind::InvalidData, why)
}

/// Trusts the server whose certificate carries the pinned key, and no
/// other.
#[derive(Debug)]
struct PinnedKey {
    pin: String,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedKey {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let offered = pin_of(end_entity)?;
        if offered == self.pin {
            return Ok(ServerCertVerified::assertion());
        }
        Err(rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(
            Unpinned { offered },
        )))))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, certificate, signed, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, certificate, signed, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// A TLS front for the tests' pretend servers: it makes the handshake
/// with a certificate of its own, made by `openssl`, and carries what
/// comes through to a plain server behind it, as AMT's TLS ports carry
/// the same protocols as its plain ones.
#[cfg(test)]
pub mod fake {
    use std::io::{ErrorKind, Read as _, Write as _};
    use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use rustls::pki_types::pem::PemObject as _;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use rustls::{ServerConfig, ServerConnection, StreamOwned};

    use crate::testing::TempDir;

    /// The front, listening until dropped.
    pub struct FakeTls {
        pub address: SocketAddr,
        /// The pin of its key.
        pub pin: String,
        config: Arc<ServerConfig>,
        done: Arc<AtomicBool>,
    }

    impl FakeTls {
        /// A front for the server at `behind`, with a key of its own.
        pub fn start(behind: SocketAddr) -> FakeTls {
            let dir = TempDir::new("tls-front");
            let (key, certificate) = (dir.path().join("key.pem"), dir.path().join("cert.pem"));
            let made = Command::new("openssl")
                .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1"])
                .args(["-subj", "/CN=Intel(R) AMT"])
                .arg("-keyout")
                .arg(&key)
                .arg("-out")
                .arg(&certificate)
                .output()
                .expect("these tests need openssl");
            assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
            let certificate = CertificateDer::from_pem_file(&certificate).unwrap();
            let pin = super::pin_of(&certificate).unwrap();
            let config = ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certificate], PrivateKeyDer::from_pem_file(&key).unwrap())
            .unwrap();
            FakeTls::listen(behind, pin, Arc::new(config))
        }

        /// A front for the server at `behind` with this front's key, as
        /// AMT has one key for all its TLS ports.
        pub fn beside(&self, behind: SocketAddr) -> FakeTls {
            FakeTls::listen(behind, self.pin.clone(), Arc::clone(&self.config))
        }

        fn listen(behind: SocketAddr, pin: String, config: Arc<ServerConfig>) -> FakeTls {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let done = Arc::new(AtomicBool::new(false));
            {
                let (config, done) = (Arc::clone(&config), Arc::clone(&done));
                std::thread::spawn(move || {
                    while !done.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((client, _)) => {
                                let config = Arc::clone(&config);
                                let done = Arc::clone(&done);
                                std::thread::spawn(move || carry(client, behind, config, &done));
                            }
                            Err(_) => std::thread::sleep(Duration::from_millis(10)),
                        }
                    }
                });
            }
            FakeTls { address, pin, config, done }
        }
    }

    impl Drop for FakeTls {
        fn drop(&mut self) {
            self.done.store(true, Ordering::Relaxed);
        }
    }

    /// One connection: the handshake, then bytes each way until either
    /// side closes.
    fn carry(client: TcpStream, behind: SocketAddr, config: Arc<ServerConfig>, done: &AtomicBool) {
        let Ok(mut server) = TcpStream::connect(behind) else { return };
        let Ok(connection) = ServerConnection::new(config) else { return };
        let _ = client.set_nonblocking(false);
        let mut tls = StreamOwned::new(connection, client);
        let _ = tls.sock.set_read_timeout(Some(Duration::from_millis(5)));
        let _ = server.set_read_timeout(Some(Duration::from_millis(5)));
        let mut buffer = [0u8; 16 * 1024];
        let waiting = |err: &std::io::Error| {
            matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
        };
        while !done.load(Ordering::Relaxed) {
            match tls.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) if server.write_all(&buffer[..read]).is_err() => break,
                Ok(_) => {}
                Err(err) if waiting(&err) => {}
                Err(_) => break,
            }
            match server.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) if tls.write_all(&buffer[..read]).is_err() => break,
                Ok(_) => {}
                Err(err) if waiting(&err) => {}
                Err(_) => break,
            }
        }
        let _ = server.shutdown(Shutdown::Both);
        let _ = tls.sock.shutdown(Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::net::TcpListener;
    use std::time::Duration;

    use super::fake::FakeTls;
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    /// A plain server that says hello, then echoes each line.
    fn echo() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                std::thread::spawn(move || {
                    let _ = stream.write_all(b"hello\n");
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    while reader.read_line(&mut line).is_ok_and(|read| read > 0) {
                        let _ = stream.write_all(line.as_bytes());
                        line.clear();
                    }
                });
            }
        });
        address
    }

    fn target(front: &FakeTls, pin: &str) -> Target {
        Target { address: front.address.to_string(), tls: Some(Tls::new(pin).unwrap()) }
    }

    #[test]
    fn talks_through_tls_to_the_pinned_key() {
        let front = FakeTls::start(echo());
        let mut link = target(&front, &front.pin).connect(SECOND, 5 * SECOND, SECOND).unwrap();
        let mut hello = [0u8; 6];
        link.read_exact(&mut hello).unwrap();
        assert_eq!(&hello, b"hello\n");
        assert!(!link.has_input().unwrap());
        link.write_all(b"again\n").unwrap();
        let deadline = Instant::now() + 5 * SECOND;
        while !link.has_input().unwrap() {
            assert!(Instant::now() < deadline, "the echo never came");
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut again = [0u8; 6];
        link.read_exact(&mut again).unwrap();
        assert_eq!(&again, b"again\n");
    }

    #[test]
    fn another_key_is_refused_and_named() {
        let front = FakeTls::start(echo());
        let other = format!("sha256//{}=", "A".repeat(43));
        let Err(err) = target(&front, &other).connect(SECOND, 5 * SECOND, SECOND) else {
            panic!("a key that is not the pinned one was trusted");
        };
        assert!(is_lasting(&err), "{err}");
        let said = err.to_string();
        assert!(said.contains("not the one the machine file pins"), "{said}");
        assert!(said.contains(&front.pin), "{said}");
    }

    #[test]
    fn the_pin_is_the_one_openssl_reads() {
        let front = FakeTls::start(echo());
        let read = crate::redfish::pin_of(&front.address.to_string()).unwrap();
        assert_eq!(read, front.pin);
        assert!(crate::redfish::is_pin(&front.pin), "{}", front.pin);
    }

    #[test]
    fn a_plain_server_does_not_pass_for_tls() {
        let plain = echo();
        let target =
            Target { address: plain.to_string(), tls: Some(Tls::new(&"A".repeat(10)).unwrap()) };
        let Err(err) = target.connect(SECOND, 5 * SECOND, SECOND) else {
            panic!("plain text passed for TLS");
        };
        assert!(is_lasting(&err), "{err}");
        assert!(err.to_string().contains("TLS handshake failed"), "{err}");
    }

    #[test]
    fn plain_links_see_input_and_a_close() {
        let mut link =
            Target::plain(&echo().to_string()).connect(SECOND, 5 * SECOND, SECOND).unwrap();
        let deadline = Instant::now() + 5 * SECOND;
        while !link.has_input().unwrap() {
            assert!(Instant::now() < deadline, "the hello never came");
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut hello = [0u8; 6];
        link.read_exact(&mut hello).unwrap();
        assert!(!link.has_input().unwrap());
        let err =
            Target::plain("no address").connect(SECOND, SECOND, SECOND).map(drop).unwrap_err();
        assert!(is_lasting(&err), "{err}");
    }
}

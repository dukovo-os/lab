// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The console of a real machine with no serial port: Dukovo's netlog
//! (ADR 0196) sends the system's log over UDP to the lab PC, and this
//! reads it into the current boot's log.
//!
//! One thread listens for the whole run, as serial.rs's reader does for a
//! port, and appends to whichever log the driver last named, so each boot
//! gets a log of its own. Each datagram is one record, `<N>[ secs.micros]
//! text`, with the syslog priority in front. The priority goes and the
//! rest is kept, one line to a record, so the log reads as a serial log
//! does. Only the machine's own address is listened to: another machine
//! of the lab sends to the same port.
//!
//! The lab image names the lab PC (10.77.0.1, port 6666) as netlog's
//! target in `/system/settings/netlog`. A boot log that stays empty while
//! the machine runs means the image's netlog has no such target, or the
//! machine file's `lab_address` and `netlog_port` are not what the image
//! sends to.

use std::fs::File;
use std::io::{ErrorKind, Write as _};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::serial::open_log;

/// The longest a netlog datagram is: netlog cuts a record at 1400 bytes.
const DATAGRAM_LIMIT: usize = 2048;

/// How long the thread waits for a datagram before it looks whether the
/// run has ended.
const LISTEN_TIME: Duration = Duration::from_millis(100);

/// Reads one machine's netlog into the current log.
pub struct NetlogReader {
    log: Arc<Mutex<File>>,
    local: SocketAddr,
    done: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl NetlogReader {
    /// Starts listening on `bind` for the datagrams of the machine at
    /// `from`, into `log`.
    ///
    /// # Errors
    ///
    /// Fails when `bind` cannot be bound (the port is another run's, or
    /// the lab card lacks the address) or `log` cannot be made.
    pub fn start(bind: SocketAddr, from: IpAddr, log: &Path) -> Result<NetlogReader, String> {
        let socket = UdpSocket::bind(bind).map_err(|err| {
            format!(
                "the netlog reader cannot listen on {bind}: {err} (is the lab card up with that \
                 address, and is no other run using UDP port {}?)",
                bind.port()
            )
        })?;
        socket.set_read_timeout(Some(LISTEN_TIME)).map_err(|err| err.to_string())?;
        let local = socket.local_addr().map_err(|err| err.to_string())?;
        let log = Arc::new(Mutex::new(open_log(log)?));
        let done = Arc::new(AtomicBool::new(false));
        let thread = {
            let (log, done) = (Arc::clone(&log), Arc::clone(&done));
            std::thread::spawn(move || listen(&socket, from, &log, &done))
        };
        Ok(NetlogReader { log, local, done, thread: Some(thread) })
    }

    /// The address listened on, which is `bind` with the port the system
    /// chose when `bind` asked for port 0.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Sends what is read from now on to `log`, a new file.
    ///
    /// # Errors
    ///
    /// Fails when `log` cannot be made.
    pub fn switch_to(&self, log: &Path) -> Result<(), String> {
        let file = open_log(log)?;
        *self.log.lock().unwrap_or_else(PoisonError::into_inner) = file;
        Ok(())
    }
}

impl Drop for NetlogReader {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Appends each datagram from `from` to the log until `done`.
fn listen(socket: &UdpSocket, from: IpAddr, log: &Mutex<File>, done: &AtomicBool) {
    let mut buffer = [0u8; DATAGRAM_LIMIT];
    while !done.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buffer) {
            Ok((read, sender)) if sender.ip() == from => {
                let line = record_line(&buffer[..read]);
                let mut file = log.lock().unwrap_or_else(PoisonError::into_inner);
                // A log that cannot be written loses the line; the run goes on.
                let _ = file.write_all(line.as_bytes());
            }
            Ok(_) => {}
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(_) => std::thread::sleep(LISTEN_TIME),
        }
    }
}

/// The log's line for one datagram: its text without the `<N>` priority
/// in front, and a newline. Bytes that are not UTF-8 show as U+FFFD.
fn record_line(datagram: &[u8]) -> String {
    let text = String::from_utf8_lossy(datagram);
    let text = text.trim_end_matches(['\r', '\n']);
    let rest = text
        .strip_prefix('<')
        .and_then(|after| after.split_once('>'))
        .filter(|(priority, _)| {
            !priority.is_empty() && priority.bytes().all(|b| b.is_ascii_digit())
        })
        .map_or(text, |(_, rest)| rest);
    format!("{rest}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;
    use std::fs;
    use std::time::Instant;

    fn wait_for(path: &Path, text: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if fs::read_to_string(path).is_ok_and(|log| log.contains(text)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn send_from(address: &str, to: SocketAddr, datagram: &[u8]) {
        let socket = UdpSocket::bind((address, 0)).unwrap();
        socket.send_to(datagram, to).unwrap();
    }

    #[test]
    fn a_datagram_from_the_machine_is_in_the_log_without_its_priority() {
        let dir = TempDir::new("netlog");
        let log = dir.path().join("serial-1.log");
        let reader =
            NetlogReader::start("127.0.0.1:0".parse().unwrap(), "127.0.0.1".parse().unwrap(), &log)
                .unwrap();
        let line = "[    1.711449] cpu1: spark[1]: spark: started slot a, @system.a";
        send_from("127.0.0.1", reader.local_addr(), format!("<30>{line}").as_bytes());
        assert!(wait_for(&log, line));
        assert_eq!(fs::read_to_string(&log).unwrap(), format!("{line}\n"));
    }

    #[test]
    fn a_datagram_from_another_address_is_dropped() {
        let dir = TempDir::new("netlog-other");
        let log = dir.path().join("serial-1.log");
        let reader =
            NetlogReader::start("127.0.0.1:0".parse().unwrap(), "127.0.0.2".parse().unwrap(), &log)
                .unwrap();
        send_from("127.0.0.1", reader.local_addr(), b"<6>[    0.1] from the OptiPlex");
        send_from("127.0.0.2", reader.local_addr(), b"<6>[    0.2] from the M75q");
        assert!(wait_for(&log, "from the M75q"));
        assert!(!fs::read_to_string(&log).unwrap().contains("OptiPlex"));
    }

    #[test]
    fn each_boot_gets_its_own_log() {
        let dir = TempDir::new("netlog-switch");
        let first = dir.path().join("logs/serial-1.log");
        let reader = NetlogReader::start(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1".parse().unwrap(),
            &first,
        )
        .unwrap();
        send_from("127.0.0.1", reader.local_addr(), b"<6>[    0.1] first boot");
        assert!(wait_for(&first, "first boot"));
        let second = dir.path().join("logs/serial-2.log");
        reader.switch_to(&second).unwrap();
        send_from("127.0.0.1", reader.local_addr(), b"<6>[    0.1] second boot");
        assert!(wait_for(&second, "second boot"));
        assert!(!fs::read_to_string(&first).unwrap().contains("second boot"));
    }

    #[test]
    fn a_record_without_a_priority_is_kept_as_it_is() {
        assert_eq!(record_line(b"no prefix here"), "no prefix here\n");
        assert_eq!(record_line(b"<oops>text"), "<oops>text\n");
        assert_eq!(record_line(b"<>text"), "<>text\n");
        assert_eq!(record_line(b"<6>[    0.5] text\n"), "[    0.5] text\n");
        // Power-on noise is not UTF-8, and is shown, not lost.
        assert_eq!(record_line(b"<6>a\xffb"), "a\u{fffd}b\n");
    }

    #[test]
    fn a_port_in_use_is_an_error_naming_it() {
        let taken = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let dir = TempDir::new("netlog-taken");
        let err = NetlogReader::start(
            taken.local_addr().unwrap(),
            "127.0.0.1".parse().unwrap(),
            &dir.path().join("l"),
        )
        .err()
        .unwrap();
        assert!(err.contains(&format!("UDP port {port}")), "{err}");
    }
}

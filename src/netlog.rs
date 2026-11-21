// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The console of a real machine with no serial port: Dukovo's netlog
//! (ADR 0196) sends the system's log over UDP to the lab PC, and this
//! reads it into the current boot's log.
//!
//! Every lab image sends its netlog to the same port of the lab PC, and a
//! port can be bound once, so the machines of a run share one listener
//! per address bound: a thread for the whole run that sorts each datagram
//! by the address it comes from into that machine's log, whichever log
//! its driver last named, so each boot gets a log of its own. A datagram
//! from an address no machine of the run has is dropped. The listener
//! goes, and its port is free again, when the last machine's reader does.
//!
//! Each datagram is one record, `<N>[ secs.micros] text`, with the syslog
//! priority in front. The priority goes and the rest is kept, one line to
//! a record, so the log reads as a serial log does.
//!
//! The lab image names the lab PC (10.77.0.1, port 6666) as netlog's
//! target in `/system/settings/netlog`. A boot log that stays empty while
//! the machine runs means the image's netlog has no such target, the
//! machine file's `lab_address` and `netlog_port` are not what the image
//! sends to, or its `machine_address` is not the address the machine has.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::serial::ConsoleLog;

/// The longest a netlog datagram is: netlog cuts a record at 1400 bytes.
const DATAGRAM_LIMIT: usize = 2048;

/// How long the thread waits for a datagram before it looks whether its
/// last reader has gone.
const LISTEN_TIME: Duration = Duration::from_millis(100);

/// The listeners open in this process, one per address bound.
static LISTENERS: Mutex<Vec<Arc<Listener>>> = Mutex::new(Vec::new());

/// One bound socket, and the machines it reads for.
struct Listener {
    local: SocketAddr,
    /// Each machine's log, by the address its datagrams come from.
    logs: Mutex<HashMap<IpAddr, ConsoleLog>>,
    done: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Listener {
    /// Binds `bind` and starts the thread that reads it.
    fn bind(bind: SocketAddr) -> Result<Arc<Listener>, String> {
        let socket = UdpSocket::bind(bind).map_err(|err| {
            format!(
                "the netlog reader cannot listen on {bind}: {err} (is the lab card up with that \
                 address, and is no other run using UDP port {}?)",
                bind.port()
            )
        })?;
        socket.set_read_timeout(Some(LISTEN_TIME)).map_err(|err| err.to_string())?;
        let local = socket.local_addr().map_err(|err| err.to_string())?;
        let listener = Arc::new(Listener {
            local,
            logs: Mutex::new(HashMap::new()),
            done: AtomicBool::new(false),
            thread: Mutex::new(None),
        });
        let thread = {
            let listener = Arc::clone(&listener);
            std::thread::spawn(move || listen(&socket, &listener))
        };
        *listener.thread.lock().unwrap_or_else(PoisonError::into_inner) = Some(thread);
        Ok(listener)
    }
}

/// Reads one machine's netlog into its current log.
pub struct NetlogReader {
    listener: Arc<Listener>,
    from: IpAddr,
    log: ConsoleLog,
}

impl NetlogReader {
    /// Starts reading the datagrams of the machine at `from` that reach
    /// `bind`, into `log`: on the listener another machine of the run
    /// bound there already, or on a new one.
    ///
    /// # Errors
    ///
    /// Fails when `bind` cannot be bound (the port is another run's, or
    /// the lab card lacks the address), when another reader of the run
    /// reads `from` there already, or when `log` cannot be made.
    pub fn start(bind: SocketAddr, from: IpAddr, log: &Path) -> Result<NetlogReader, String> {
        let log = ConsoleLog::create(log)?;
        let mut listeners = LISTENERS.lock().unwrap_or_else(PoisonError::into_inner);
        let listener = match listeners.iter().find(|listener| listener.local == bind) {
            Some(listener) => Arc::clone(listener),
            None => {
                let listener = Listener::bind(bind)?;
                listeners.push(Arc::clone(&listener));
                listener
            }
        };
        let mut logs = listener.logs.lock().unwrap_or_else(PoisonError::into_inner);
        if logs.contains_key(&from) {
            return Err(format!(
                "two machines of this run send their netlog from {from}: each machine file \
                 needs a machine_address of its own"
            ));
        }
        logs.insert(from, log.clone());
        drop(logs);
        Ok(NetlogReader { listener, from, log })
    }

    /// The address listened on, which is `bind` with the port the system
    /// chose when `bind` asked for port 0.
    pub fn local_addr(&self) -> SocketAddr {
        self.listener.local
    }

    /// The log the machine's datagrams go to.
    pub fn log(&self) -> &ConsoleLog {
        &self.log
    }
}

impl Drop for NetlogReader {
    fn drop(&mut self) {
        // The registry stays locked until the last reader's socket is
        // closed, so a reader started meanwhile binds the port afresh
        // rather than finding it taken.
        let mut listeners = LISTENERS.lock().unwrap_or_else(PoisonError::into_inner);
        let mut logs = self.listener.logs.lock().unwrap_or_else(PoisonError::into_inner);
        logs.remove(&self.from);
        let last = logs.is_empty();
        drop(logs);
        if !last {
            return;
        }
        listeners.retain(|listener| !Arc::ptr_eq(listener, &self.listener));
        self.listener.done.store(true, Ordering::Relaxed);
        let thread = self.listener.thread.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

/// Appends each datagram to the log of the machine it comes from, until
/// the listener's last reader goes.
fn listen(socket: &UdpSocket, listener: &Listener) {
    let mut buffer = [0u8; DATAGRAM_LIMIT];
    while !listener.done.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buffer) {
            Ok((read, sender)) => {
                let logs = listener.logs.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(log) = logs.get(&sender.ip()) {
                    log.append(record_line(&buffer[..read]).as_bytes());
                }
            }
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
        reader.log().switch_to(&second).unwrap();
        send_from("127.0.0.1", reader.local_addr(), b"<6>[    0.1] second boot");
        assert!(wait_for(&second, "second boot"));
        assert!(!fs::read_to_string(&first).unwrap().contains("second boot"));
    }

    #[test]
    fn machines_that_send_to_one_port_each_get_their_own_lines() {
        let dir = TempDir::new("netlog-shared");
        let (first, second) = (dir.path().join("first.log"), dir.path().join("second.log"));
        let one = NetlogReader::start(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1".parse().unwrap(),
            &first,
        )
        .unwrap();
        let port = one.local_addr();
        let two = NetlogReader::start(port, "127.0.0.2".parse().unwrap(), &second).unwrap();
        assert_eq!(two.local_addr(), port);
        send_from("127.0.0.1", port, b"<6>[    0.1] from optiplex-2");
        send_from("127.0.0.2", port, b"<6>[    0.1] from optiplex-3");
        assert!(wait_for(&first, "from optiplex-2"));
        assert!(wait_for(&second, "from optiplex-3"));
        assert!(!fs::read_to_string(&first).unwrap().contains("optiplex-3"));
        assert!(!fs::read_to_string(&second).unwrap().contains("optiplex-2"));
        // One machine's reader going leaves the other's listening.
        drop(one);
        send_from("127.0.0.2", port, b"<6>[    0.2] still here");
        assert!(wait_for(&second, "still here"));
    }

    #[test]
    fn two_readers_for_one_sender_are_refused() {
        let dir = TempDir::new("netlog-twice");
        let from: IpAddr = "127.0.0.1".parse().unwrap();
        let reader =
            NetlogReader::start("127.0.0.1:0".parse().unwrap(), from, &dir.path().join("a.log"))
                .unwrap();
        let err = NetlogReader::start(reader.local_addr(), from, &dir.path().join("b.log"))
            .err()
            .unwrap();
        assert!(err.contains("machine_address of its own"), "{err}");
        // The refusal leaves the first reader reading.
        send_from("127.0.0.1", reader.local_addr(), b"<6>[    0.1] first");
        assert!(wait_for(&dir.path().join("a.log"), "first"));
    }

    #[test]
    fn the_port_is_free_once_its_last_reader_goes() {
        let dir = TempDir::new("netlog-free");
        let reader = NetlogReader::start(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1".parse().unwrap(),
            &dir.path().join("a.log"),
        )
        .unwrap();
        let port = reader.local_addr();
        drop(reader);
        UdpSocket::bind(port).unwrap();
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

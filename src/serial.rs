// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A real machine's serial console, read from a serial port of the lab
//! PC's (a USB serial cable, as a rule) into the current boot's log.
//!
//! One thread reads the port for the whole run and appends what comes to
//! whichever log the driver last named, so each boot, the writer's and
//! the Linux twin's too, gets a log of its own. The port is set to the
//! machine file's speed, raw, with `stty`, and read without waiting, so
//! the thread notices when to stop. A port that goes away (the cable
//! pulled out) is opened again until it comes back.

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{FileTypeExt as _, OpenOptionsExt as _};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

/// Linux's O_NOCTTY: opening the port must not make it the run's
/// controlling terminal, whose hang-up would end the run.
const O_NOCTTY: i32 = 0o400;

/// Linux's O_NONBLOCK: neither opening the port nor reading it waits, for
/// a modem's carrier or for text.
const O_NONBLOCK: i32 = 0o4000;

/// Reads one serial port into the current log.
pub struct SerialReader {
    log: Arc<Mutex<Log>>,
    done: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Where what is read goes now.
struct Log {
    file: Option<File>,
}

impl SerialReader {
    /// Starts reading `port` at `baud` into `log`.
    pub fn start(port: &Path, baud: u32, log: &Path) -> Result<SerialReader, String> {
        let file = open_log(log)?;
        let log = Arc::new(Mutex::new(Log { file: Some(file) }));
        let done = Arc::new(AtomicBool::new(false));
        let mut device = open_port(port, baud)?;
        let thread = {
            let (log, done, port) = (Arc::clone(&log), Arc::clone(&done), port.to_path_buf());
            std::thread::spawn(move || {
                let mut buffer = [0u8; 4096];
                while !done.load(Ordering::Relaxed) {
                    match device.read(&mut buffer) {
                        // Nothing yet, or the end of what a plain file
                        // holds so far.
                        Ok(0) => std::thread::sleep(Duration::from_millis(20)),
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        Ok(read) => {
                            let mut log = log.lock().unwrap_or_else(PoisonError::into_inner);
                            if let Some(file) = &mut log.file {
                                let _ = file.write_all(&buffer[..read]);
                            }
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => {
                            // Gone; wait for it to come back.
                            std::thread::sleep(Duration::from_secs(1));
                            if let Ok(again) = open_port(&port, baud) {
                                device = again;
                            }
                        }
                    }
                }
            })
        };
        Ok(SerialReader { log, done, thread: Some(thread) })
    }

    /// Sends what is read from now on to `log`, a new file.
    pub fn switch_to(&self, log: &Path) -> Result<(), String> {
        let file = open_log(log)?;
        self.log.lock().unwrap_or_else(PoisonError::into_inner).file = Some(file);
        Ok(())
    }
}

impl Drop for SerialReader {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Makes a log, empty.
fn open_log(path: &Path) -> Result<File, String> {
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder).map_err(|err| format!("{}: {err}", folder.display()))?;
    }
    File::create(path).map_err(|err| format!("making {}: {err}", path.display()))
}

/// Opens the port, set up for the lab: `baud`, raw, no echo, no flow
/// control, and no waiting for a modem's carrier. Reads never wait: with
/// nothing to read they say so at once. A plain file (in tests) is read
/// as it grows.
fn open_port(port: &Path, baud: u32) -> Result<File, String> {
    let file =
        OpenOptions::new().read(true).custom_flags(O_NOCTTY | O_NONBLOCK).open(port).map_err(
            |err| {
                format!(
                    "the serial port {}: {err} (is the cable plugged in, and is this user in the \
                 dialout group?)",
                    port.display()
                )
            },
        )?;
    let is_port = file.metadata().is_ok_and(|meta| meta.file_type().is_char_device());
    if is_port {
        // stty sets up the port it is given as its input, which this
        // open file is, so the settings hold while the lab reads.
        let input = file.try_clone().map_err(|err| err.to_string())?;
        let set = Command::new("stty")
            .args([&baud.to_string(), "raw", "-echo", "-ixon", "-crtscts", "clocal"])
            .stdin(input)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|err| format!("running stty: {err}"))?;
        if !set.status.success() {
            return Err(format!(
                "setting up the serial port {}: {}",
                port.display(),
                String::from_utf8_lossy(&set.stderr).trim()
            ));
        }
    }
    Ok(file)
}

/// Whether `port` opens for reading now, for `machines check`.
pub fn check_port(port: &Path, baud: u32) -> Result<(), String> {
    open_port(port, baud).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;
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

    #[test]
    fn each_boot_gets_its_own_log() {
        let dir = TempDir::new("serial");
        let port = dir.write("port", "");
        let first = dir.path().join("logs/serial-1.log");
        let reader = SerialReader::start(&port, 115_200, &first).unwrap();
        let mut writing = OpenOptions::new().append(true).open(&port).unwrap();
        writing.write_all(b"writer: wrote it\n").unwrap();
        assert!(wait_for(&first, "writer: wrote it"));
        let second = dir.path().join("logs/serial-2.log");
        reader.switch_to(&second).unwrap();
        writing.write_all(b"Boot complete.\n").unwrap();
        assert!(wait_for(&second, "Boot complete."));
        assert!(!fs::read_to_string(&first).unwrap().contains("Boot complete."));
        drop(reader);
    }

    #[test]
    fn a_missing_port_says_how_to_fix_it() {
        let dir = TempDir::new("serial-missing");
        let err = SerialReader::start(&dir.path().join("ttyUSB9"), 115_200, &dir.path().join("l"))
            .err()
            .unwrap();
        assert!(err.contains("dialout"), "{err}");
    }
}

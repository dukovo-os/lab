// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A server's serial console read over the lab network from its BMC's
//! IPMI Serial-over-LAN, through the system's `ipmitool`, so that the
//! machine needs no serial cable.
//!
//! One thread keeps `ipmitool sol activate usesolkeepalive` running for
//! the whole run and appends what it prints to a [`ConsoleLog`],
//! whichever log the driver last named, as serial.rs's reader does for a
//! cable. A BMC takes one Serial-over-LAN session at a time, so each
//! start is preceded by `sol deactivate`, which ends a session a crashed
//! run left. The password reaches ipmitool as a file (`-f`), never in its
//! arguments, and the file is the machine file's `password_file`.
//!
//! ipmitool is given a pipe for its standard input that the reader holds
//! open and never writes to: with the input at its end, ipmitool spins.
//! When it exits (the BMC restarting, the network dropping) the thread
//! notes the status and the last line ipmitool wrote to its standard
//! error in the driver's log (`console: ipmitool ended ...`), waits
//! [`RESTART_GAP`] and starts it again, until the reader is dropped.
//! What the machine writes in between is lost. When `serial-<n>.log`
//! stays empty while the machine runs, look for those lines first, then
//! at whether the BIOS redirects its console to the BMC's port
//! (`machines check` reads that setting).

use std::io::{BufRead as _, BufReader, Read as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::driver::real::{DriverLog, run_bounded};
use crate::serial::ConsoleLog;

/// How long after ipmitool exits it is started again.
const RESTART_GAP: Duration =
    if cfg!(test) { Duration::from_millis(10) } else { Duration::from_secs(2) };

/// How long `sol deactivate` and the check that ipmitool runs may take.
const SHORT_TIME: Duration = Duration::from_secs(15);

/// How often an end that repeats itself is noted again.
const NOTE_AGAIN: Duration = Duration::from_secs(60);

/// How many of ipmitool's standard error lines a run may add to the log.
const STDERR_LINES: usize = 20;

/// How the lab reaches the BMC's IPMI.
#[derive(Clone)]
pub struct Sign {
    /// The `ipmitool` program.
    pub program: PathBuf,
    /// The BMC's host, without a port.
    pub host: String,
    pub user: String,
    /// A file holding the password, which ipmitool reads.
    pub password_file: PathBuf,
}

impl Sign {
    /// ipmitool's arguments for IPMI over LAN with cipher suite 17 (the
    /// strongest an iDRAC offers), then `tail`.
    pub fn arguments(&self, tail: &[&str]) -> Vec<String> {
        let mut arguments: Vec<String> = ["-I", "lanplus", "-C", "17", "-H"]
            .into_iter()
            .map(String::from)
            .chain([self.host.clone(), "-U".into(), self.user.clone(), "-f".into()])
            .chain([self.password_file.display().to_string()])
            .collect();
        arguments.extend(tail.iter().map(|word| (*word).to_string()));
        arguments
    }

    /// Runs ipmitool with `tail` after the sign-in arguments, for up to
    /// `limit`, and gives what it printed.
    ///
    /// # Errors
    ///
    /// Fails when ipmitool is not there, does not end in time, or exits
    /// with a status other than 0, with the last line it wrote to its
    /// standard error.
    pub fn run(&self, tail: &[&str], limit: Duration) -> Result<String, String> {
        let output =
            run_bounded(Command::new(&self.program).args(self.arguments(tail)), b"", limit)?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
        let said = String::from_utf8_lossy(&output.stderr);
        let said = said.lines().rev().find(|line| !line.trim().is_empty());
        Err(format!("ipmitool {}: {}", tail.join(" "), said.unwrap_or("it gave no reason").trim()))
    }
}

/// Reads one machine's Serial-over-LAN into the current log, until
/// dropped.
pub struct IpmiSolReader {
    log: ConsoleLog,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

/// What the reader and its thread share.
struct Shared {
    done: AtomicBool,
    /// The running ipmitool, for the drop to end.
    running: Mutex<Option<Child>>,
}

impl IpmiSolReader {
    /// Starts reading the BMC's console into `log`; `notes` is the
    /// driver's log. Returns at once: the first session opens in the
    /// thread.
    ///
    /// # Errors
    ///
    /// Fails when ipmitool is not installed or will not run, and when
    /// `log` cannot be made.
    pub fn start(sign: Sign, log: &Path, notes: DriverLog) -> Result<IpmiSolReader, String> {
        run_bounded(Command::new(&sign.program).arg("-V"), b"", SHORT_TIME).map_err(|err| {
            format!(
                "{err}: Serial-over-LAN needs ipmitool (`sudo apt install ipmitool`, or \
                 redfish.ipmitool names it)"
            )
        })?;
        let log = ConsoleLog::create(log)?;
        let shared = Arc::new(Shared { done: AtomicBool::new(false), running: Mutex::new(None) });
        let thread = {
            let (log, shared) = (log.clone(), Arc::clone(&shared));
            std::thread::spawn(move || keep_reading(&sign, &log, &notes, &shared))
        };
        Ok(IpmiSolReader { log, shared, thread: Some(thread) })
    }

    /// The log it appends to.
    pub fn log(&self) -> &ConsoleLog {
        &self.log
    }
}

impl Drop for IpmiSolReader {
    fn drop(&mut self) {
        self.shared.done.store(true, Ordering::Relaxed);
        if let Some(child) =
            self.shared.running.lock().unwrap_or_else(PoisonError::into_inner).as_mut()
        {
            // The thread reaps it once its output ends.
            let _ = child.kill();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The thread: ends a session a crashed run left, reads a new one into
/// `log`, and starts over each time one ends, until the reader is
/// dropped.
fn keep_reading(sign: &Sign, log: &ConsoleLog, notes: &DriverLog, shared: &Shared) {
    // The last end noted, and when: an ipmitool that ends the same way
    // each time is noted once a minute, not each start.
    let mut noted: Option<(String, Instant)> = None;
    while !shared.done.load(Ordering::Relaxed) {
        // Fails when no session is open, as it usually is; a real fault
        // shows again as the activation's.
        let _ = sign.run(&["sol", "deactivate"], SHORT_TIME);
        if shared.done.load(Ordering::Relaxed) {
            break;
        }
        let ended = session(sign, log, notes, shared);
        if shared.done.load(Ordering::Relaxed) {
            break;
        }
        let line = format!("console: {ended}; starting it again");
        let repeated =
            noted.as_ref().is_some_and(|(last, at)| *last == line && at.elapsed() < NOTE_AGAIN);
        if !repeated {
            notes.note(&line);
            noted = Some((line, Instant::now()));
        }
        let wake = Instant::now() + RESTART_GAP;
        while Instant::now() < wake && !shared.done.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// One session: runs `sol activate` and appends what it prints to `log`
/// until it ends. Gives how it ended, for the driver's log.
fn session(sign: &Sign, log: &ConsoleLog, notes: &DriverLog, shared: &Shared) -> String {
    let started = Command::new(&sign.program)
        .args(sign.arguments(&["sol", "activate", "usesolkeepalive"]))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match started {
        Ok(child) => child,
        Err(err) => return format!("ipmitool did not start ({err})"),
    };
    // Held until the session ends: see the module's comment.
    let _stdin = child.stdin.take();
    let (Some(mut stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return "ipmitool gave no output to read".into();
    };
    {
        let mut running = shared.running.lock().unwrap_or_else(PoisonError::into_inner);
        if shared.done.load(Ordering::Relaxed) {
            // The reader was dropped while this started.
            let _ = child.kill();
            let _ = child.wait();
            return "ipmitool stopped".into();
        }
        *running = Some(child);
    }
    let last_said = Arc::new(Mutex::new(String::new()));
    let said = {
        let (notes, last_said) = (notes.clone(), Arc::clone(&last_said));
        std::thread::spawn(move || {
            for (at, line) in BufReader::new(stderr).lines().map_while(Result::ok).enumerate() {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                if at < STDERR_LINES {
                    notes.note(&format!("console: ipmitool: {line}"));
                }
                *last_said.lock().unwrap_or_else(PoisonError::into_inner) = line;
            }
        })
    };
    let mut buffer = [0u8; 4096];
    loop {
        match stdout.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => log.append(&buffer[..read]),
        }
    }
    let child = shared.running.lock().unwrap_or_else(PoisonError::into_inner).take();
    let status = child.map_or_else(
        || "it was not running".to_string(),
        |mut child| child.wait().map_or_else(|err| err.to_string(), |status| status.to_string()),
    );
    // Ends with the child, whose end closes its standard error.
    let _ = said.join();
    let last = last_said.lock().unwrap_or_else(PoisonError::into_inner).clone();
    if last.is_empty() {
        format!("ipmitool ended ({status})")
    } else {
        format!("ipmitool ended ({status}); it said: {last}")
    }
}

/// A stand-in `ipmitool` for tests: a shell script.
#[cfg(test)]
pub mod fake {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

    use crate::testing::TempDir;

    /// Runs `script` once with `--probe` until the system lets it run: a
    /// program another thread's `fork` inherited open for writing is
    /// busy until that child execs, and the first run of a script a test
    /// has just written may fail with it.
    pub fn settle(script: &Path) {
        for _ in 0..200 {
            match Command::new(script).arg("--probe").output() {
                Err(err) if err.raw_os_error() == Some(26) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => return,
            }
        }
    }

    /// Writes `body` as the script `name` in `dir`. The script answers
    /// `-V` and `--probe` at once; `body` sees ipmitool's arguments.
    pub fn script(dir: &TempDir, name: &str, body: &str) -> PathBuf {
        let text = format!("#!/bin/sh\ncase \"$1\" in -V|--probe) exit 0;; esac\n{body}\n");
        let path = dir.write(name, &text);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        settle(&path);
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;
    use std::fs;

    fn wait_for(path: &Path, text: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if fs::read(path).is_ok_and(|log| String::from_utf8_lossy(&log).contains(text)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn sign(program: PathBuf) -> Sign {
        Sign {
            program,
            host: "10.77.0.11".into(),
            user: "root".into(),
            password_file: "/lab/bmc-password".into(),
        }
    }

    fn notes(dir: &TempDir) -> (DriverLog, PathBuf) {
        let path = dir.path().join("redfish.log");
        (DriverLog::open(&path).unwrap(), path)
    }

    #[test]
    fn the_password_goes_to_ipmitool_as_a_file() {
        let arguments = sign("ipmitool".into()).arguments(&["sol", "info"]);
        assert_eq!(
            arguments.join(" "),
            "-I lanplus -C 17 -H 10.77.0.11 -U root -f /lab/bmc-password sol info"
        );
    }

    #[test]
    fn a_session_left_over_is_deactivated_and_an_ended_one_is_started_again() {
        let dir = TempDir::new("ipmi-restart");
        let base = dir.path().display();
        let program = fake::script(
            &dir,
            "ipmitool",
            &format!(
                "echo \"$*\" >> '{base}/calls'\ncase \"$*\" in *deactivate*) exit 0;; esac\n\
                 echo x >> '{base}/runs'\nn=$(wc -l < '{base}/runs')\n\
                 echo \"console line $n\"\nexit 3"
            ),
        );
        let (log, _) = notes(&dir);
        let first = dir.path().join("logs/serial-1.log");
        let reader = IpmiSolReader::start(sign(program), &first, log).unwrap();
        assert!(wait_for(&first, "console line 1"));
        // Started again, and the next log gets the next session's lines.
        assert!(wait_for(&first, "console line 2"));
        let second = dir.path().join("logs/serial-2.log");
        reader.log().switch_to(&second).unwrap();
        assert!(wait_for(&second, "console line"));
        assert!(!fs::read_to_string(&second).unwrap().contains("console line 1\n"));
        drop(reader);
        let calls = fs::read_to_string(dir.path().join("calls")).unwrap();
        let calls: Vec<&str> = calls.lines().collect();
        let head = "-I lanplus -C 17 -H 10.77.0.11 -U root -f /lab/bmc-password sol";
        assert_eq!(calls[0], format!("{head} deactivate"));
        assert_eq!(calls[1], format!("{head} activate usesolkeepalive"));
        assert_eq!(calls[2], format!("{head} deactivate"));
    }

    #[test]
    fn ipmitool_gets_an_input_that_stays_open_and_is_ended_with_the_reader() {
        let dir = TempDir::new("ipmi-stdin");
        let base = dir.path().display();
        let program = fake::script(
            &dir,
            "ipmitool",
            &format!(
                "case \"$*\" in *deactivate*) exit 0;; esac\necho x >> '{base}/runs'\n\
                 echo $$ > '{base}/pid'\necho started\n\
                 if read -r line; then echo stdin gave something; else echo stdin ended; fi\nexit 0"
            ),
        );
        let (log, _) = notes(&dir);
        let first = dir.path().join("serial.log");
        let reader = IpmiSolReader::start(sign(program), &first, log).unwrap();
        assert!(wait_for(&first, "started"));
        std::thread::sleep(Duration::from_millis(300));
        // Still the first run: its input did not end, so it did not exit.
        let runs = fs::read_to_string(dir.path().join("runs")).unwrap();
        assert_eq!(runs.lines().count(), 1);
        assert!(!fs::read_to_string(&first).unwrap().contains("stdin"));
        let pid = fs::read_to_string(dir.path().join("pid")).unwrap().trim().to_string();
        assert!(Path::new(&format!("/proc/{pid}")).exists());
        let dropped = Instant::now();
        drop(reader);
        assert!(dropped.elapsed() < Duration::from_secs(5));
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    fn an_end_is_noted_with_its_status_and_last_line_and_not_over_and_over() {
        let dir = TempDir::new("ipmi-notes");
        let program = fake::script(
            &dir,
            "ipmitool",
            "case \"$*\" in *deactivate*) exit 0;; esac\n\
             echo 'Error: Unable to establish IPMI v2 / RMCP+ session' >&2\nexit 1",
        );
        let (log, notes_path) = notes(&dir);
        let reader = IpmiSolReader::start(sign(program), &dir.path().join("s.log"), log).unwrap();
        assert!(wait_for(&notes_path, "starting it again"));
        std::thread::sleep(Duration::from_millis(300));
        drop(reader);
        let notes = fs::read_to_string(&notes_path).unwrap();
        let ended = "ipmitool ended (exit status: 1); it said: Error: Unable to establish IPMI v2";
        assert_eq!(notes.matches(ended).count(), 1, "{notes}");
        assert!(notes.contains("console: ipmitool: Error: Unable"), "{notes}");
    }

    #[test]
    fn a_missing_ipmitool_is_refused_at_the_start() {
        let dir = TempDir::new("ipmi-missing");
        let (log, _) = notes(&dir);
        let missing = dir.path().join("no-ipmitool");
        let err = IpmiSolReader::start(sign(missing), &dir.path().join("s.log"), log).err();
        let err = err.unwrap();
        assert!(err.contains("not installed") && err.contains("redfish.ipmitool"), "{err}");
    }

    #[test]
    fn a_failing_run_gives_the_last_line_ipmitool_wrote() {
        let dir = TempDir::new("ipmi-run");
        let program =
            fake::script(&dir, "ipmitool", "echo 'first' >&2\necho 'Error: no route' >&2\nexit 1");
        let err = sign(program).run(&["sol", "info"], Duration::from_secs(5)).unwrap_err();
        assert_eq!(err, "ipmitool sol info: Error: no route");
    }
}

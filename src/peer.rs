// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The lab PC as the machine's peer: the other end of a file share, a
//! Kerberos KDC, a WireGuard tunnel.
//!
//! A `host` step runs a command on the lab PC itself rather than on the
//! machine, so a test can stand up the far side of what it checks with
//! the lab PC's own programs (Samba, MIT Kerberos, NFS-Ganesha,
//! wireguard-go) and check Dukovo against them both ways. A step with
//! `background = true` starts a server that stays up until the test ends;
//! the runner then ends it, and everything it started, with SIGTERM, and
//! SIGKILL a few seconds later.
//!
//! Commands name the two ends with placeholders the runner fills in when
//! the step runs, so the same test reads on QEMU and on a real machine:
//!
//! - `{peer}`: the lab PC's address as the machine sees it;
//! - `{port:NAME}`: a port of the lab PC's that was free when the test
//!   first named it, the same for the whole test, for one of its servers;
//! - `{machine}` and `{machine-port:N}` (or `{machine-udp-port:N}`): the
//!   address and port where the lab PC reaches the machine's port N, which
//!   under QEMU is a port of the lab PC's forwarded to the guest's;
//! - `{amt}`: the address of the machine's Intel AMT, on a machine that
//!   has it, for a host step to reach its web and WS-Management ports;
//! - `{work}`: a folder of the test's own on the lab PC, which host steps
//!   run in and which goes when the test ends;
//! - `{here}`: the folder the test file is in, for scripts beside it.
//!
//! `run` steps may hold `{peer}` and `{port:NAME}`, as the machine needs
//! them to reach the lab PC's servers; the others name things only the
//! lab PC has, so they go only in `host` steps.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::net::{TcpListener, UdpSocket};
use std::ops::Range;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::machines::is_plain_name;
use crate::ssh::Output;
use crate::stop::Stop;

/// The exit status with which a host command says the lab PC cannot do
/// its part of the test (a kernel with no NFS client, say): the test is
/// skipped, not failed, with what the command printed as the reason. It is
/// the skip status of Automake's test harness and of many test tools.
pub const SKIP_STATUS: i32 = 77;

/// How long a server is given to end after SIGTERM before SIGKILL.
const END_GRACE: Duration = Duration::from_secs(5);

/// The most a step's `input` may hold: it is written in one go before the
/// command's output is read, so it must fit in a pipe even when the
/// command never reads it.
pub const INPUT_MAX: usize = 16 * 1024;

/// A placeholder in a command, filled in when its step runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Token {
    /// `{peer}`: the lab PC's address as the machine sees it.
    Peer,
    /// `{machine}`: the machine's address as the lab PC reaches it.
    Machine,
    /// `{machine-port:N}` or `{machine-udp-port:N}`: the lab PC's port
    /// that reaches the machine's port N.
    MachinePort { port: u16, udp: bool },
    /// `{port:NAME}`: a free port of the lab PC's for one of the test's
    /// servers.
    Port(String),
    /// `{amt}`: the machine's AMT's address, on the lab network.
    Amt,
    /// `{work}`: the test's own folder on the lab PC.
    Work,
    /// `{here}`: the folder the test file is in.
    Here,
}

impl Token {
    /// Whether a `run` step, on the machine, may hold it: the machine
    /// needs only to reach the lab PC's servers.
    pub fn fits_run(&self) -> bool {
        matches!(self, Token::Peer | Token::Port(_))
    }
}

/// Finds the placeholders in `text`, with where each is. Braces that are
/// not one (a shell's `${x}`, awk's `{print $1}`, krb5.conf's sections)
/// are left alone; a placeholder written wrong, as `{port:}` or
/// `{machine-port:http}`, is an error.
pub fn scan(text: &str) -> Result<Vec<(Range<usize>, Token)>, String> {
    let mut found = Vec::new();
    for (start, _) in text.match_indices('{') {
        let Some(length) = text[start + 1..].find('}') else { break };
        let end = start + 1 + length;
        let inside = &text[start + 1..end];
        let token = match inside {
            "peer" => Token::Peer,
            "machine" => Token::Machine,
            "amt" => Token::Amt,
            "work" => Token::Work,
            "here" => Token::Here,
            _ => {
                if let Some(name) = inside.strip_prefix("port:") {
                    if !is_plain_name(name) {
                        return Err(format!(
                            "{{{inside}}}: a port's name is a-z, 0-9 and -, as {{port:kdc}}"
                        ));
                    }
                    Token::Port(name.to_string())
                } else if let Some((udp, number)) = inside
                    .strip_prefix("machine-port:")
                    .map(|number| (false, number))
                    .or_else(|| inside.strip_prefix("machine-udp-port:").map(|n| (true, n)))
                {
                    match number.parse::<u16>() {
                        Ok(port) if port > 0 => Token::MachinePort { port, udp },
                        _ => {
                            return Err(format!(
                                "{{{inside}}}: the machine's port is a number from 1 to 65535, as {{machine-port:445}}"
                            ));
                        }
                    }
                } else {
                    continue;
                }
            }
        };
        found.push((start..end + 1, token));
    }
    Ok(found)
}

/// Whether `text` holds any placeholder.
pub fn uses_peer(text: &str) -> bool {
    scan(text).is_ok_and(|found| !found.is_empty())
}

/// Whether `text` names the machine's AMT (`{amt}`).
pub fn uses_amt(text: &str) -> bool {
    scan(text).is_ok_and(|found| found.iter().any(|(_, token)| *token == Token::Amt))
}

/// `text` with each placeholder replaced by what `value` gives for it.
pub fn fill<E>(
    text: &str,
    mut value: impl FnMut(&Token) -> Result<String, E>,
) -> Result<String, E> {
    // The suite checked every placeholder as it loaded, so none is wrong.
    let found = scan(text).unwrap_or_default();
    let mut filled = String::with_capacity(text.len());
    let mut from = 0;
    for (range, token) in found {
        filled.push_str(&text[from..range.start]);
        filled.push_str(&value(&token)?);
        from = range.end;
    }
    filled.push_str(&text[from..]);
    Ok(filled)
}

/// A TCP port of the lab PC's that is free for UDP too, on every address:
/// a KDC, say, answers on both.
pub fn free_port() -> Result<u16, String> {
    for _ in 0..50 {
        let tcp = TcpListener::bind("0.0.0.0:0")
            .map_err(|err| format!("finding a free port on the lab PC: {err}"))?;
        let port = tcp.local_addr().map_err(|err| err.to_string())?.port();
        if UdpSocket::bind(("0.0.0.0", port)).is_ok() {
            return Ok(port);
        }
    }
    Err("the lab PC has no port free for both TCP and UDP".into())
}

/// A UDP port of the lab PC's that nothing uses now.
pub fn free_udp_port() -> Result<u16, String> {
    let socket = UdpSocket::bind("127.0.0.1:0")
        .map_err(|err| format!("finding a free UDP port on the lab PC: {err}"))?;
    socket.local_addr().map(|addr| addr.port()).map_err(|err| err.to_string())
}

/// The programs of `names` the lab PC does not have: none on its PATH, nor
/// in the sbin folders, where servers such as krb5kdc and smbd live but
/// which an ordinary account's PATH often leaves out.
pub fn missing_programs(names: &[String]) -> Vec<String> {
    let mut folders: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    for sbin in ["/usr/local/sbin", "/usr/sbin", "/sbin"] {
        folders.push(PathBuf::from(sbin));
    }
    let runnable = |path: PathBuf| {
        fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    };
    names
        .iter()
        .filter(|name| !folders.iter().any(|folder| runnable(folder.join(name))))
        .cloned()
        .collect()
}

/// Why a host step cannot run as root.
pub const NO_ROOT: &str = "this step runs as root on the lab PC, and the lab runs as an ordinary account whose sudo asks for a password: let it run sudo without one, or run the lab as root";

/// What runs a host command as root: nothing when the lab already runs
/// as root, else `sudo -n`, once it is known not to ask for a password.
/// Fails with [`NO_ROOT`] when neither works.
pub fn root_prefix() -> Result<Vec<String>, String> {
    if effective_uid() == Some(0) {
        return Ok(Vec::new());
    }
    let works = Command::new("sudo")
        .args(["-n", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if works { Ok(vec!["sudo".into(), "-n".into(), "--".into()]) } else { Err(NO_ROOT.into()) }
}

/// The lab's own effective user id, from the kernel's status of it.
fn effective_uid() -> Option<u32> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("Uid:"))?;
    line.split_whitespace().nth(2)?.parse().ok()
}

/// `sh -c command`, through `root` (from [`root_prefix`]) when given, in
/// `folder`, as a process group of its own so it can be ended whole.
fn shell(command: &str, root: &[String], folder: &Path) -> Command {
    let mut shell = match root.split_first() {
        Some((first, rest)) => {
            let mut sudo = Command::new(first);
            sudo.args(rest).arg("sh");
            sudo
        }
        None => Command::new("sh"),
    };
    shell.arg("-c").arg(command).current_dir(folder).process_group(0);
    shell
}

/// Sends `signal` to every process of the group `group` leads. Errors are
/// left: the group may have ended already, or hold root's processes the
/// lab may not signal.
fn signal_group(group: u32, signal: &str) {
    let _ = Command::new("kill")
        .args(["-s", signal, "--", &format!("-{group}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Runs `command` on the lab PC until it ends, at `deadline`, or when the
/// run is stopped (its status is then none), with `input` as its standard
/// input, in `folder`. Its output and errors are gathered in `scratch`.
/// Whatever it left running is ended with it: a server that must stay up
/// belongs in a [`Job`].
pub fn run(
    command: &str,
    root: &[String],
    input: Option<&str>,
    folder: &Path,
    scratch: &Path,
    deadline: Instant,
    stop: &Stop,
) -> Result<Output, String> {
    let output_file =
        File::create(scratch).map_err(|err| format!("making {}: {err}", scratch.display()))?;
    let errors_file = output_file.try_clone().map_err(|err| err.to_string())?;
    let mut child = shell(command, root, folder)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(output_file)
        .stderr(errors_file)
        .spawn()
        .map_err(|err| format!("starting `{command}` on the lab PC: {err}"))?;
    let group = child.id();
    // The input is at most INPUT_MAX, which fits in the pipe, so writing it
    // all first never waits on the command. Dropping the pipe ends it.
    if let (Some(input), Some(mut pipe)) = (input, child.stdin.take()) {
        let _ = pipe.write_all(input.as_bytes());
    }
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
            break status.code();
        }
        if Instant::now() >= deadline || stop.requested() {
            signal_group(group, "KILL");
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // Anything it started and left behind goes too.
    signal_group(group, "KILL");
    let text = fs::read(scratch).map_err(|err| err.to_string())?;
    Ok(Output { status, text: String::from_utf8_lossy(&text).into_owned() })
}

/// A server a test runs on the lab PC until the test ends.
pub struct Job {
    child: Child,
    /// Its command, as the test's log names it.
    pub command: String,
    /// Its output and errors.
    pub log: PathBuf,
    /// How it ended, once it has.
    ended: Option<String>,
}

/// How waiting for a server to be ready went.
pub enum Readiness {
    Ready,
    /// It ended first, with this status (none: a signal).
    Exited(Option<i32>),
    /// The test's time ran out first.
    TimedOut,
    Stopped,
}

impl Job {
    /// Starts `command` on the lab PC, through `root` when given, in
    /// `folder`, its output and errors going to `log`.
    pub fn start(command: &str, root: &[String], folder: &Path, log: &Path) -> Result<Job, String> {
        let output_file =
            File::create(log).map_err(|err| format!("making {}: {err}", log.display()))?;
        let errors_file = output_file.try_clone().map_err(|err| err.to_string())?;
        let child = shell(command, root, folder)
            .stdin(Stdio::null())
            .stdout(output_file)
            .stderr(errors_file)
            .spawn()
            .map_err(|err| format!("starting `{command}` on the lab PC: {err}"))?;
        Ok(Job { child, command: command.into(), log: log.into(), ended: None })
    }

    /// Waits until its output holds `text`.
    pub fn wait_for(&mut self, text: &str, deadline: Instant, stop: &Stop) -> Readiness {
        loop {
            // Read before asking whether it ended, so a server that says
            // it is ready and then ends at once is still found ready.
            if fs::read(&self.log).is_ok_and(|log| String::from_utf8_lossy(&log).contains(text)) {
                return Readiness::Ready;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                return Readiness::Exited(status.code());
            }
            if stop.requested() {
                return Readiness::Stopped;
            }
            if Instant::now() >= deadline {
                return Readiness::TimedOut;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The last lines of its output, for a message.
    pub fn tail(&self) -> String {
        let log = fs::read(&self.log).unwrap_or_default();
        let log = String::from_utf8_lossy(&log);
        let lines: Vec<&str> = log.lines().collect();
        lines[lines.len().saturating_sub(20)..].join("\n")
    }

    /// Ends it and everything it started: SIGTERM to its process group,
    /// then SIGKILL to whatever is left after a few seconds. Says how it
    /// ended; ending it again says the same.
    pub fn end(&mut self) -> String {
        if let Some(ended) = &self.ended {
            return ended.clone();
        }
        let group = self.child.id();
        let ended = match self.child.try_wait() {
            Ok(Some(status)) => format!("it had already ended, {status}"),
            _ => {
                signal_group(group, "TERM");
                let until = Instant::now() + END_GRACE;
                loop {
                    match self.child.try_wait() {
                        Ok(Some(_)) => break "ended at the test's end".to_string(),
                        Ok(None) if Instant::now() < until => {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        _ => {
                            let _ = self.child.kill();
                            let _ = self.child.wait();
                            break format!(
                                "killed at the test's end, as it was still running {}s after SIGTERM",
                                END_GRACE.as_secs()
                            );
                        }
                    }
                }
            }
        };
        signal_group(group, "KILL");
        self.ended = Some(ended.clone());
        ended
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.end();
    }
}

/// The ports a test has named with `{port:NAME}`, each found free the
/// first time.
#[derive(Default)]
pub struct Ports(BTreeMap<String, u16>);

impl Ports {
    /// The port named `name`, found now if the test has not named it yet.
    pub fn get(&mut self, name: &str) -> Result<u16, String> {
        if let Some(&port) = self.0.get(name) {
            return Ok(port);
        }
        // Another name of this test may not get the same port.
        for _ in 0..20 {
            let port = free_port()?;
            if !self.0.values().any(|&taken| taken == port) {
                self.0.insert(name.to_string(), port);
                return Ok(port);
            }
        }
        Err("the lab PC has too few free ports".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn finds_placeholders_and_leaves_other_braces() {
        let text = "[realms]\nDUKOVO.TEST = { kdc = {peer}:{port:kdc} }\nawk '{print $1}' ${HOME} {machine-udp-port:51820}";
        let found: Vec<Token> = scan(text).unwrap().into_iter().map(|(_, token)| token).collect();
        assert_eq!(
            found,
            [Token::Peer, Token::Port("kdc".into()), Token::MachinePort { port: 51820, udp: true }]
        );
        let filled = fill(text, |token| {
            Ok::<_, ()>(match token {
                Token::Peer => "10.0.2.2".into(),
                Token::Port(_) => "8888".into(),
                _ => "40000".into(),
            })
        })
        .unwrap();
        assert!(filled.contains("DUKOVO.TEST = { kdc = 10.0.2.2:8888 }"), "{filled}");
        assert!(filled.contains("awk '{print $1}' ${HOME} 40000"), "{filled}");
        for wrong in ["{port:}", "{port:K D C}", "{machine-port:http}", "{machine-port:0}"] {
            assert!(scan(wrong).is_err(), "{wrong}");
        }
        assert!(!uses_peer("echo ${x} {target}"));
    }

    #[test]
    fn runs_on_the_lab_pc_and_ends_what_it_left() {
        let dir = TempDir::new("peer");
        let scratch = dir.path().join("output");
        let soon = || Instant::now() + Duration::from_secs(20);
        let stop = Stop::default();
        let output = run(
            "pwd; read line; echo \"got $line\"; echo oops >&2; exit 3",
            &[],
            Some("hello\n"),
            dir.path(),
            &scratch,
            soon(),
            &stop,
        )
        .unwrap();
        assert_eq!(output.status, Some(3));
        assert!(output.text.contains(&dir.path().display().to_string()), "{}", output.text);
        assert!(output.text.contains("got hello\noops"), "{}", output.text);

        // A command that outlives its time is ended, with what it started.
        let pid_file = dir.path().join("pid");
        let started = Instant::now();
        let output = run(
            &format!("sleep 30 & echo $! > {}; wait", pid_file.display()),
            &[],
            None,
            dir.path(),
            &scratch,
            Instant::now() + Duration::from_millis(500),
            &stop,
        )
        .unwrap();
        assert_eq!(output.status, None);
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid = fs::read_to_string(&pid_file).unwrap();
        assert!(gone(pid.trim()), "the sleep it started was ended");
    }

    #[test]
    fn a_server_runs_until_it_is_ended() {
        let dir = TempDir::new("peer-job");
        let log = dir.path().join("server.log");
        let stop = Stop::default();
        let soon = Instant::now() + Duration::from_secs(20);
        let mut job = Job::start(
            "trap 'echo told to stop; exit 0' TERM; sleep 30 & echo $! > child; echo ready; wait",
            &[],
            dir.path(),
            &log,
        )
        .unwrap();
        assert!(matches!(job.wait_for("ready", soon, &stop), Readiness::Ready));
        let child = fs::read_to_string(dir.path().join("child")).unwrap();
        assert_eq!(job.end(), "ended at the test's end");
        assert!(fs::read_to_string(&log).unwrap().contains("told to stop"));
        assert!(gone(child.trim()), "what the server started went with it");

        // One that ends before it is ready says so.
        let mut early = Job::start("echo failing; exit 2", &[], dir.path(), &log).unwrap();
        assert!(matches!(early.wait_for("ready", soon, &stop), Readiness::Exited(Some(2))));
        assert_eq!(early.tail(), "failing");
        assert!(early.end().starts_with("it had already ended"));
    }

    #[test]
    fn finds_programs_and_ports() {
        assert_eq!(
            missing_programs(&["sh".into(), "no-such-program-here".into()]),
            ["no-such-program-here"]
        );
        let mut ports = Ports::default();
        let kdc = ports.get("kdc").unwrap();
        assert_eq!(ports.get("kdc").unwrap(), kdc);
        assert_ne!(ports.get("smb").unwrap(), kdc);
    }

    /// Whether no process has the id `pid` any more (or it is a zombie
    /// waiting for a parent that is not this test).
    fn gone(pid: &str) -> bool {
        for _ in 0..100 {
            match fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(_) => return true,
                Ok(stat) if stat.split_whitespace().nth(2) == Some("Z") => return true,
                Ok(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        false
    }
}

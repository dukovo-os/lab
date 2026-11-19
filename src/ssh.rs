// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The SSH channel: how `run` steps run commands on a machine.
//!
//! Commands run through the lab PC's own OpenSSH client as the `lab`
//! account, which only `ob image --lab` images have, signing in with the
//! lab key. The key is made once by `dukovo-lab key` and never leaves the
//! lab PC: it is not copied into bundles, logs or output, and the client
//! is told to ignore the user's own SSH configuration and keys so a run
//! is the same on every PC.
//!
//! Each boot keeps the machine's host key in a known-hosts file of its
//! own, because every image makes a new host key. When a command fails
//! with exit status 255, the text in the test's log is the SSH client's
//! own explanation (refused, timed out, key not accepted).

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::peer;
use crate::stop::Stop;

/// The account `ob image --lab` makes for the lab.
pub const LAB_ACCOUNT: &str = "lab";

/// The private key's file name in the configuration folder; the public
/// key is beside it with `.pub` added.
pub const KEY_FILE: &str = "lab_ed25519";

/// Where to reach a machine's SSH server.
#[derive(Clone, Debug)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub key: PathBuf,
    pub known_hosts: PathBuf,
    /// Where the command's output is gathered while it runs.
    pub scratch: PathBuf,
}

/// When a command that has gone quiet is ended as stalled: it printed
/// nothing for `after`, and the machine's console, the log at `console`,
/// gained nothing either. A command that is silent while the console
/// scrolls is the machine at work, not hung.
#[derive(Clone, Debug)]
pub struct Stall {
    pub after: Duration,
    pub console: PathBuf,
}

/// Follows a command's output and the console, and says when both have
/// been still for as long as a [`Stall`] allows.
pub struct Quiet<'a> {
    stall: &'a Stall,
    seen: (u64, u64),
    moved: Instant,
}

impl<'a> Quiet<'a> {
    pub fn new(stall: &'a Stall, now: Instant) -> Quiet<'a> {
        Quiet { stall, seen: (0, 0), moved: now }
    }

    /// Whether the command is stalled at `now`, given how many bytes it
    /// has printed and the console log holds.
    pub fn stalled(&mut self, now: Instant, printed: u64, console: u64) -> bool {
        if (printed, console) != self.seen {
            self.seen = (printed, console);
            self.moved = now;
        }
        now.saturating_duration_since(self.moved) >= self.stall.after
    }
}

/// The size of the file at `path`; zero while it is not there.
pub fn size_of(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |meta| meta.len())
}

/// What a command did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    /// Its exit status; none when it was stopped at the deadline or by a
    /// signal.
    pub status: Option<i32>,
    /// Its output and errors, interleaved as they came.
    pub text: String,
}

impl Target {
    /// Runs `command` on the machine, stopping it at `deadline`, when the
    /// run is stopped, or when `stall` says it has gone quiet. The output
    /// then has no status, whichever of the three ended it.
    pub fn run(
        &self,
        command: &str,
        input: Option<&str>,
        deadline: Instant,
        stop: &Stop,
        stall: Option<&Stall>,
    ) -> Result<Output, String> {
        let output_file = File::create(&self.scratch)
            .map_err(|err| format!("making {}: {err}", self.scratch.display()))?;
        let errors_file = output_file.try_clone().map_err(|err| err.to_string())?;
        let mut child = self
            .command(command)
            .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(output_file)
            .stderr(errors_file)
            .spawn()
            .map_err(|err| format!("starting ssh: {err}; is OpenSSH's client installed?"))?;
        // The input is short (a password's line): it fits in the pipe, so
        // writing it all before reading never waits on the command.
        // Dropping the pipe ends the input.
        if let (Some(input), Some(mut pipe)) = (input, child.stdin.take()) {
            use std::io::Write as _;
            let _ = pipe.write_all(input.as_bytes());
        }
        let mut quiet = stall.map(|stall| Quiet::new(stall, Instant::now()));
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
                break status.code();
            }
            let stalled = match (&mut quiet, stall) {
                (Some(quiet), Some(stall)) => {
                    quiet.stalled(Instant::now(), size_of(&self.scratch), size_of(&stall.console))
                }
                _ => false,
            };
            if Instant::now() >= deadline || stop.requested() || stalled {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let text = fs::read(&self.scratch).map_err(|err| err.to_string())?;
        Ok(Output { status, text: String::from_utf8_lossy(&text).into_owned() })
    }

    /// The `ssh` command line for `command`.
    fn command(&self, command: &str) -> Command {
        let mut ssh = Command::new("ssh");
        ssh.args(self.arguments_before_host());
        ssh.arg(self.account());
        ssh.args(["--", command]);
        ssh
    }

    /// The `sh` command line of an `ssh` that forwards `local_port` on the
    /// lab PC's loopback to `port` on the machine's own, for as long as it
    /// runs. The machine sees each connection come from itself, so a page
    /// it serves to `localhost` is served here as well. `ssh` ends when it
    /// cannot bind the local port, so a forward that did not start does
    /// not look as if it had.
    pub fn forward_command(&self, local_port: u16, port: u16) -> String {
        let mut words = vec!["ssh".to_string()];
        words.extend(self.arguments_before_host());
        words.extend([
            "-N".into(),
            "-o".into(),
            "ExitOnForwardFailure=yes".into(),
            "-L".into(),
            format!("127.0.0.1:{local_port}:127.0.0.1:{port}"),
            self.account(),
        ]);
        words.iter().map(peer::quoted).collect::<Vec<_>>().join(" ")
    }

    fn account(&self) -> String {
        format!("{LAB_ACCOUNT}@{}", self.host)
    }

    /// Everything of the command line before the account: no user or
    /// system configuration, and every option the lab relies on.
    fn arguments_before_host(&self) -> Vec<String> {
        let mut words = vec!["-F".to_string(), "/dev/null".into()];
        for option in [
            "BatchMode=yes".to_string(),
            "IdentitiesOnly=yes".into(),
            format!("IdentityFile={}", self.key.display()),
            format!("UserKnownHostsFile={}", self.known_hosts.display()),
            "GlobalKnownHostsFile=/dev/null".into(),
            // Each boot of a new image has a new host key, and this file is
            // that boot's alone, so the first key seen is the machine's.
            "StrictHostKeyChecking=accept-new".into(),
            "ConnectTimeout=10".into(),
            "ServerAliveInterval=5".into(),
            "ServerAliveCountMax=3".into(),
            "LogLevel=ERROR".into(),
        ] {
            words.extend(["-o".to_string(), option]);
        }
        words.extend(["-p".to_string(), self.port.to_string()]);
        words
    }
}

/// The lab account's password's file name in the configuration folder.
/// `ob image --lab` gives the account this password; `root` steps give it
/// to `elevate`. Like the key, it never leaves the lab PC.
pub const PASSWORD_FILE: &str = "lab_password";

/// How many characters the password has: letters and digits, so about
/// 190 bits, past any guessing.
const PASSWORD_LENGTH: usize = 32;

/// The lab key's path in the configuration folder `config`.
pub fn key_path(config: &Path) -> PathBuf {
    config.join(KEY_FILE)
}

/// The lab password's path in the configuration folder `config`.
pub fn password_path(config: &Path) -> PathBuf {
    config.join(PASSWORD_FILE)
}

/// Makes the lab password at `path` unless it is there; says whether it
/// made it. The file is the lab PC user's alone (mode 0600) from the
/// moment it exists.
pub fn make_password(path: &Path) -> Result<bool, String> {
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::OpenOptionsExt as _;
    if path.exists() {
        return Ok(false);
    }
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder).map_err(|err| format!("making {}: {err}", folder.display()))?;
    }
    const LETTERS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut random = File::open("/dev/urandom").map_err(|err| format!("/dev/urandom: {err}"))?;
    let mut password = String::with_capacity(PASSWORD_LENGTH);
    let mut byte = [0u8; 1];
    while password.len() < PASSWORD_LENGTH {
        random.read_exact(&mut byte).map_err(|err| format!("/dev/urandom: {err}"))?;
        // 248 is the largest multiple of 62 a byte holds: bytes past it
        // are skipped, so every character is as likely as the next.
        if usize::from(byte[0]) < LETTERS.len() * 4 {
            password.push(char::from(LETTERS[usize::from(byte[0]) % LETTERS.len()]));
        }
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|err| format!("making {}: {err}", path.display()))?;
    file.write_all(format!("{password}\n").as_bytes())
        .map_err(|err| format!("writing {}: {err}", path.display()))?;
    Ok(true)
}

/// Reads the lab password, if the lab has one.
pub fn read_password(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let password = text.lines().next().unwrap_or_default().trim_end_matches('\r');
            if password.is_empty() {
                return Err(format!(
                    "{} is empty; remove it and run `dukovo-lab key`",
                    path.display()
                ));
            }
            Ok(Some(password.to_string()))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("reading {}: {err}", path.display())),
    }
}

/// Makes the lab key at `path` unless it is there; says whether it made it.
pub fn make_key(path: &Path) -> Result<bool, String> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder).map_err(|err| format!("making {}: {err}", folder.display()))?;
    }
    let output = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "dukovo-lab"])
        .arg("-f")
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("starting ssh-keygen: {err}; is OpenSSH's client installed?"))?;
    if !output.status.success() {
        return Err(format!(
            "ssh-keygen could not make {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forward_binds_the_lab_pc_s_loopback_and_ends_when_it_cannot() {
        let target = Target {
            host: "10.77.0.2".into(),
            port: 22,
            key: "/keys/it's".into(),
            known_hosts: "/work/known_hosts-1".into(),
            scratch: "/work/ssh-output".into(),
        };
        let command = target.forward_command(41000, 9090);
        assert!(command.starts_with("'ssh' '-F' '/dev/null' "), "{command}");
        assert!(command.contains("'IdentityFile=/keys/it'\\''s'"), "{command}");
        assert!(command.contains("'UserKnownHostsFile=/work/known_hosts-1'"), "{command}");
        assert!(command.contains("'ExitOnForwardFailure=yes'"), "{command}");
        assert!(command.ends_with("'-N' '-o' 'ExitOnForwardFailure=yes' '-L' '127.0.0.1:41000:127.0.0.1:9090' 'lab@10.77.0.2'"), "{command}");
    }

    #[test]
    fn a_command_is_stalled_only_when_it_and_the_console_are_both_still() {
        let stall = Stall { after: Duration::from_secs(180), console: "serial-1.log".into() };
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        let mut quiet = Quiet::new(&stall, start);
        assert!(!quiet.stalled(at(100), 0, 0));
        // The command prints a line, which restarts the count.
        assert!(!quiet.stalled(at(150), 10, 0));
        assert!(!quiet.stalled(at(320), 10, 0));
        // The console gains a line.
        assert!(!quiet.stalled(at(330), 10, 55));
        assert!(!quiet.stalled(at(500), 10, 55));
        assert!(quiet.stalled(at(510), 10, 55));
    }

    #[test]
    fn ignores_the_users_own_ssh_setup() {
        let target = Target {
            host: "127.0.0.1".into(),
            port: 40022,
            key: "/k/lab_ed25519".into(),
            known_hosts: "/w/known_hosts".into(),
            scratch: "/w/out".into(),
        };
        let command = target.command("uname -a");
        let args: Vec<String> =
            command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();
        assert_eq!(args[..2], ["-F", "/dev/null"]);
        assert!(args.contains(&"IdentitiesOnly=yes".to_string()));
        assert!(args.contains(&"IdentityFile=/k/lab_ed25519".to_string()));
        assert!(args.contains(&"UserKnownHostsFile=/w/known_hosts".to_string()));
        assert_eq!(args[args.len() - 4..], ["40022", "lab@127.0.0.1", "--", "uname -a"]);
    }
}

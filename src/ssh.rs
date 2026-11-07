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
    /// Runs `command` on the machine, stopping it at `deadline` or when
    /// the run is stopped.
    pub fn run(
        &self,
        command: &str,
        input: Option<&str>,
        deadline: Instant,
        stop: &Stop,
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
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
                break status.code();
            }
            if Instant::now() >= deadline || stop.requested() {
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
        // No user or system configuration: every option the lab relies on
        // is here.
        ssh.args(["-F", "/dev/null"]);
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
            ssh.args(["-o", &option]);
        }
        ssh.args(["-p", &self.port.to_string()]);
        ssh.arg(format!("{LAB_ACCOUNT}@{}", self.host));
        ssh.args(["--", command]);
        ssh
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

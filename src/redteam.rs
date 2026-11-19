// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The red-team kit: a Linux the attacker boots from, for attack tests
//! that need Linux's tools (`attacker = "linux"`).
//!
//! The kit is a folder with a `kit.toml`, read as [`bench::Kit`] reads its
//! own, naming a kernel, an initramfs and the kernel's command line. The
//! driver boots the attacker straight from them on the private lab
//! network and drives it over SSH. The kit's init has to do two things the
//! driver cannot, so the driver says what to do on the kernel command line
//! (see [`RedteamKit::boot_line`]): give the segment's network card the
//! address in `lab.addr`, and start an SSH server for `user` that accepts
//! the public key in `lab.key`. A kit that does neither never answers SSH,
//! and the run says so with the attacker's serial log, which is kept in
//! the bundle as `attacker-serial-<boot>.log`.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::bench::KIT_FILE;

/// The account an attack step runs as when the kit names none.
const DEFAULT_USER: &str = "root";

/// Linux for the attacker to boot from.
#[derive(Clone, Debug, PartialEq)]
pub struct RedteamKit {
    /// The kernel, booted directly by the driver.
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    /// The kernel's command line, to which the driver adds `lab.key` and
    /// `lab.addr`.
    pub cmdline: String,
    /// The account attack steps run as over SSH.
    pub user: String,
    /// What the attacker's serial console says once its SSH server is up,
    /// when the kit prints such a line: the driver then waits for it before
    /// it tries SSH.
    pub ready: Option<String>,
    /// The attacker's memory as QEMU's `-m` takes it (`3G`), when the kit
    /// unpacks to more than the machine's own memory holds: the initramfs
    /// is unpacked into RAM.
    pub memory: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KitFile {
    kernel: Option<String>,
    initrd: Option<String>,
    cmdline: Option<String>,
    user: Option<String>,
    ready: Option<String>,
    memory: Option<String>,
}

impl RedteamKit {
    /// Reads `dir/kit.toml`. Its `kernel` and `initrd` are files in `dir`.
    ///
    /// # Errors
    ///
    /// Fails when the file is missing or malformed, a key it needs is
    /// missing, or a file it names is not beside it.
    pub fn read(dir: &Path) -> Result<RedteamKit, String> {
        let path = dir.join(KIT_FILE);
        let text = std::fs::read_to_string(&path)
            .map_err(|err| format!("reading the red-team kit {}: {err}", path.display()))?;
        let file: KitFile =
            toml::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
        let missing = |key: &str| {
            format!(
                "{}: the red-team kit needs {key}; `ob lab redteam-kit` builds one",
                path.display()
            )
        };
        let kernel = file.kernel.ok_or_else(|| missing("a kernel"))?;
        let initrd = file.initrd.ok_or_else(|| missing("an initrd"))?;
        let cmdline = file.cmdline.ok_or_else(|| missing("a cmdline"))?;
        let in_dir = |name: &str, what: &str| -> Result<PathBuf, String> {
            let file = dir.join(name);
            if name.is_empty() || name.contains('/') || !file.is_file() {
                return Err(format!(
                    "{}: its {what} is a file beside it, and {name:?} is not",
                    path.display()
                ));
            }
            Ok(file)
        };
        let user = file.user.unwrap_or_else(|| DEFAULT_USER.into());
        if user.is_empty() || user.contains(|c: char| c.is_whitespace() || c == '@') {
            return Err(format!(
                "{}: user is an account name, and {user:?} is not",
                path.display()
            ));
        }
        if file.ready.as_deref().is_some_and(|ready| ready.trim().is_empty()) {
            return Err(format!(
                "{}: ready needs the text the attacker prints once SSH is up, or leave it out",
                path.display()
            ));
        }
        if let Some(memory) = &file.memory {
            let digits = memory.strip_suffix(['M', 'G']).unwrap_or("");
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(format!(
                    "{}: memory is a size such as \"3G\" or \"2048M\", and {memory:?} is not",
                    path.display()
                ));
            }
        }
        Ok(RedteamKit {
            kernel: in_dir(&kernel, "kernel")?,
            initrd: in_dir(&initrd, "initrd")?,
            cmdline,
            user,
            ready: file.ready,
            memory: file.memory,
        })
    }

    /// The kernel command line the attacker boots with: the kit's own, then
    /// `lab.key`, the lab's public key line in base64 (a command line has
    /// no room for the spaces in it), and `lab.addr`, the address with its
    /// prefix length that the segment's network card is to get.
    pub fn boot_line(&self, public_key: &str, address: &str) -> String {
        format!(
            "{} lab.key={} lab.addr={address}",
            self.cmdline,
            base64(public_key.trim().as_bytes())
        )
    }
}

/// `bytes` in standard base64, with padding.
fn base64(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let word = chunk
            .iter()
            .enumerate()
            .fold(0u32, |word, (at, byte)| word | u32::from(*byte) << (16 - 8 * at));
        for at in 0..4 {
            if at <= chunk.len() {
                text.push(char::from(DIGITS[(word >> (18 - 6 * at) & 63) as usize]));
            } else {
                text.push('=');
            }
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    fn folder(kit: &str) -> TempDir {
        let dir = TempDir::new("redteam");
        dir.write("vmlinuz", "kernel");
        dir.write("rootfs.cpio", "initrd");
        dir.write(KIT_FILE, kit);
        dir
    }

    const GOOD: &str =
        "kernel = \"vmlinuz\"\ninitrd = \"rootfs.cpio\"\ncmdline = \"console=ttyS0 quiet\"\n";

    #[test]
    fn a_kit_names_its_files_and_takes_root_by_default() {
        let dir = folder(GOOD);
        let kit = RedteamKit::read(dir.path()).unwrap();
        assert_eq!(kit.kernel, dir.path().join("vmlinuz"));
        assert_eq!(kit.initrd, dir.path().join("rootfs.cpio"));
        assert_eq!(kit.cmdline, "console=ttyS0 quiet");
        assert_eq!(kit.user, "root");
        assert_eq!(kit.ready, None);
        assert_eq!(kit.memory, None);
    }

    #[test]
    fn a_kit_may_ask_for_more_memory_than_the_machine_has() {
        let dir = folder(&format!("{GOOD}memory = \"3G\"\n"));
        assert_eq!(RedteamKit::read(dir.path()).unwrap().memory.as_deref(), Some("3G"));
        for bad in ["3", "G", "3 G", "lots", "-3G"] {
            let err = RedteamKit::read(folder(&format!("{GOOD}memory = \"{bad}\"\n")).path())
                .unwrap_err();
            assert!(err.contains("memory is a size"), "{bad}: {err}");
        }
    }

    #[test]
    fn a_kit_may_name_its_user_and_its_ready_line() {
        let dir = folder(&format!("{GOOD}user = \"red\"\nready = \"sshd: listening\"\n"));
        let kit = RedteamKit::read(dir.path()).unwrap();
        assert_eq!(kit.user, "red");
        assert_eq!(kit.ready.as_deref(), Some("sshd: listening"));
    }

    #[test]
    fn a_kit_that_lacks_a_key_says_which() {
        for (key, key_name) in
            [("a kernel", "kernel"), ("an initrd", "initrd"), ("a cmdline", "cmdline")]
        {
            let kept: Vec<&str> = GOOD.lines().filter(|line| !line.starts_with(key_name)).collect();
            let kept = kept.join("\n");
            let err = RedteamKit::read(folder(&kept).path()).unwrap_err();
            assert!(err.contains(&format!("needs {key}")), "{err}");
            assert!(err.contains("ob lab redteam-kit"), "{err}");
        }
    }

    #[test]
    fn a_kit_with_a_missing_file_or_a_bad_value_is_refused() {
        let dir = TempDir::new("redteam-empty");
        assert!(RedteamKit::read(dir.path()).unwrap_err().contains("reading the red-team kit"));

        let err = RedteamKit::read(folder(&GOOD.replace("vmlinuz", "absent")).path()).unwrap_err();
        assert!(err.contains("its kernel is a file beside it"), "{err}");

        let err = RedteamKit::read(folder(&format!("{GOOD}user = \"a b\"\n")).path()).unwrap_err();
        assert!(err.contains("user is an account name"), "{err}");

        let err = RedteamKit::read(folder(&format!("{GOOD}ready = \" \"\n")).path()).unwrap_err();
        assert!(err.contains("ready needs"), "{err}");

        let err = RedteamKit::read(folder(&format!("{GOOD}colour = 1\n")).path()).unwrap_err();
        assert!(err.contains("colour"), "{err}");
    }

    #[test]
    fn base64_pads_as_the_standard_does() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn the_boot_line_carries_the_key_and_the_address() {
        let kit = RedteamKit::read(folder(GOOD).path()).unwrap();
        assert_eq!(
            kit.boot_line("foo\n", "10.9.9.2/24"),
            "console=ttyS0 quiet lab.key=Zm9v lab.addr=10.9.9.2/24"
        );
    }
}

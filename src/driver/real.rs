// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What the drivers of real machines share: the lab PC's side of a net
//! boot (amt.rs and plug.rs differ in how they power the machine on and
//! where its console comes from, not in how a fresh boot is written),
//! the SSH target, and the checks of `dukovo-lab machines check` that
//! concern the lab PC alone.
//!
//! A driver's log (`logs/<machine>/amt.log`, `plug.log`) is made here: it
//! holds the driver's own lines and each request the net-boot server
//! saw. When a fresh boot fails with no request for the kernel, the log
//! says so, and docs/real-machines.md lists what to look at.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::Setup;
use crate::bench::Kit;
use crate::netboot::{self, Image, Offer, Outcome, Server};
use crate::ssh;
use crate::stop::Stop;

/// The longest a fresh boot's net boot and write may take: the firmware
/// starting twice, the writer's kernel loading, and a few hundred
/// megabytes written and read back.
pub const WRITE_TIME: Duration = Duration::from_secs(4 * 60);

/// The port the machine's SSH server listens on.
const SSH_PORT: u16 = 22;

/// What a machine file says about the lab network, which a real machine's
/// net boot and checks need.
pub struct LabSide<'a> {
    /// The lab PC's address on the lab network.
    pub lab_address: &'a str,
    pub boot_port: u16,
    pub tftp: &'a Path,
    /// The MAC address of the machine's card on the lab network.
    pub mac: &'a str,
    /// The writer's kit folder.
    pub writer: &'a Path,
}

impl LabSide<'_> {
    /// The lab PC's address, parsed.
    pub fn lab_ip(&self) -> Result<IpAddr, String> {
        self.lab_address
            .parse()
            .map_err(|_| format!("the lab address {} is not an address", self.lab_address))
    }
}

/// A driver's log file, whose lines begin with the seconds since the
/// driver did.
struct DriverLog {
    file: File,
    started: Instant,
}

/// The lab PC's side of a real machine's net boots: the server that hands
/// out the writer or the Linux twin, the boot script the machine's
/// network GRUB reads, and the image under test.
pub struct NetBoot {
    server: Server,
    /// Where this machine's boot script goes in the TFTP folder.
    script: PathBuf,
    /// The writer's kit.
    pub writer: Kit,
    /// The image under test, read once, on the first fresh boot.
    image: Option<Arc<Image>>,
    log: DriverLog,
    /// A second place for what the net-boot server saw, as lines of their
    /// own: the writer's log, on a machine whose writer has no console.
    event_copy: Option<File>,
}

impl NetBoot {
    /// Reads the writer's kit, starts the net-boot server and opens the
    /// driver's log at `log`.
    ///
    /// # Errors
    ///
    /// Fails when the kit is missing, the server cannot listen, or the
    /// log cannot be made.
    pub fn start(side: &LabSide<'_>, log: &Path) -> Result<NetBoot, String> {
        let writer = Kit::read(side.writer).map_err(|err| {
            format!("{err} (the writer's kit; `ob lab` installs it in {})", side.writer.display())
        })?;
        let server = Server::start(side.lab_ip()?, side.boot_port)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|err| format!("making {}: {err}", log.display()))?;
        Ok(NetBoot {
            server,
            script: netboot::script_path(side.tftp, side.mac),
            writer,
            image: None,
            log: DriverLog { file, started: Instant::now() },
            event_copy: None,
        })
    }

    /// Writes a line to the driver's log, with the seconds since the
    /// driver began.
    pub fn note(&mut self, line: &str) {
        let at = self.log.started.elapsed().as_secs_f64();
        // A log that cannot be written must not fail the boot it describes.
        let _ = writeln!(self.log.file, "{at:9.1}s {line}");
    }

    /// The ports the net-boot server listens on, for a line in the log.
    pub fn ports(&self) -> String {
        let ports: Vec<String> =
            self.server.addresses().iter().map(|address| address.port().to_string()).collect();
        ports.join(", ")
    }

    /// Where the boot script is written.
    pub fn script(&self) -> &Path {
        &self.script
    }

    /// Also writes what the net-boot server saw to `file`, one line each,
    /// from now on; none stops it.
    pub fn copy_events_to(&mut self, file: Option<File>) {
        self.event_copy = file;
    }

    /// Writes what the net-boot server did to the log.
    pub fn note_server(&mut self) {
        for event in self.server.take_events() {
            self.note(&format!("net boot: {event}"));
            if let Some(copy) = &mut self.event_copy {
                // As in `note`: the copy is for reading, not for the boot.
                let _ = writeln!(copy, "{event}");
            }
        }
    }

    /// Writes the boot script that has the machine's network GRUB load
    /// what `offer` hands out, with `cmdline` (`{base}` in it is the
    /// boot's URL), for the next boot from the network.
    ///
    /// # Errors
    ///
    /// Fails when the script cannot be written.
    pub fn begin(&mut self, what: &str, offer: Offer, cmdline: &str) -> Result<(), String> {
        let base = self.server.offer(offer)?;
        let cmdline = cmdline.replace("{base}", &base);
        netboot::write_script(&self.script, &netboot::grub_script(what, &base, &cmdline))?;
        self.note(&format!("net boot: {what}, boot script {}", self.script.display()));
        Ok(())
    }

    /// Ends a net boot: its script and offer go, so a later boot from the
    /// network goes on to the disk.
    pub fn finish(&mut self) {
        netboot::remove_script(&self.script);
        self.server.withdraw();
        self.note_server();
    }

    /// The image under test, read the first time it is wanted.
    ///
    /// # Errors
    ///
    /// Fails when the image cannot be read.
    pub fn image(&mut self, path: &Path) -> Result<Arc<Image>, String> {
        if let Some(image) = &self.image {
            return Ok(Arc::clone(image));
        }
        let started = Instant::now();
        let image = Arc::new(Image::scan(path)?);
        self.note(&format!(
            "image {}: {} bytes, {} of data in {} runs, SHA-256 {}, read in {:.1}s",
            path.display(),
            image.size,
            image.data_bytes(),
            image.extents.len(),
            image.sha256,
            started.elapsed().as_secs_f64()
        ));
        self.image = Some(Arc::clone(&image));
        Ok(image)
    }

    /// Waits until the writer says it wrote the image and the hash was
    /// right, for up to [`WRITE_TIME`].
    ///
    /// # Errors
    ///
    /// Fails with the writer's own reason when it failed, and when the run
    /// is stopped or the time is up.
    pub fn await_writer(&mut self, stop: &Stop) -> Result<(), String> {
        let deadline = Instant::now() + WRITE_TIME;
        loop {
            self.note_server();
            match self.server.outcome() {
                Some(Outcome::Written) => return Ok(()),
                Some(Outcome::Failed(why)) => return Err(why),
                None => {}
            }
            if stop.requested() {
                return Err("the run was stopped".into());
            }
            if Instant::now() >= deadline {
                let minutes = WRITE_TIME.as_secs() / 60;
                return Err(if self.server.loaded() {
                    format!("the writer did not finish within {minutes} minutes")
                } else {
                    format!(
                        "the machine did not load the writer within {minutes} minutes: no \
                         request for its kernel came, so its firmware or the network GRUB may \
                         have gone on to the disk"
                    )
                });
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for NetBoot {
    fn drop(&mut self) {
        netboot::remove_script(&self.script);
    }
}

/// Says that a person does `what` on a real machine, which `driver`
/// cannot.
pub fn persons_job(driver: &str, what: &str) -> String {
    format!(
        "the {driver} driver cannot {what}; a person does it on a real machine (\"person\" in \
         the machine file's [controls])"
    )
}

/// Where and how to run a command on the machine at `address` over SSH.
///
/// # Errors
///
/// Fails when the lab has no key.
pub fn ssh_target(setup: &Setup, address: &str, boot: u32) -> Result<ssh::Target, String> {
    let key = setup
        .key
        .clone()
        .ok_or("the lab has no key: run `dukovo-lab key`, then build the image again")?;
    Ok(ssh::Target {
        host: address.to_string(),
        port: SSH_PORT,
        key,
        known_hosts: setup.work.join(format!("known_hosts-{boot}")),
        scratch: setup.work.join("ssh-output"),
    })
}

/// How one of `machines check`'s checks came out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Ok,
    /// Worth knowing, but runs go ahead.
    Note,
    /// Runs on the machine will not work until it is put right.
    Problem,
}

/// One of `machines check`'s checks.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Check {
    pub check: &'static str,
    pub verdict: Verdict,
    pub detail: String,
}

/// A check that found a problem.
pub fn problem(detail: String) -> (Verdict, String) {
    (Verdict::Problem, detail)
}

/// A check's result as a [`Check`].
pub fn found(check: &'static str, result: Result<String, (Verdict, String)>) -> Check {
    let (verdict, detail) = match result {
        Ok(detail) => (Verdict::Ok, detail),
        Err((verdict, detail)) => (verdict, detail),
    };
    Check { check, verdict, detail }
}

/// The checks of what the lab PC itself needs for a real machine: its
/// address on the lab network, the net-boot server's ports, the TFTP
/// folder and network GRUB, the writer's kit and the lab key.
pub fn check_lab_side(side: &LabSide<'_>, key: Option<&Path>) -> Vec<Check> {
    let lab = side.lab_address;
    let mut checks = vec![found(
        "lab network",
        std::net::TcpListener::bind((lab, 0))
            .map(|_| format!("this PC has {lab}, the lab address"))
            .map_err(|err| {
                problem(format!(
                    "this PC does not have {lab} ({err}): give the lab card that address"
                ))
            }),
    )];
    checks.push(found(
        "boot port",
        netboot::ports(side.boot_port).map_err(problem).and_then(|ports| {
            ports.iter().try_for_each(|&port| {
                std::net::TcpListener::bind((lab, port)).map(drop).map_err(|err| {
                    (Verdict::Note, format!("port {port} is in use ({err}): by a run going now?"))
                })
            })?;
            Ok(format!(
                "ports {} to {} are free for the net-boot server",
                ports[0],
                ports[ports.len() - 1]
            ))
        }),
    ));
    let script = netboot::script_path(side.tftp, side.mac);
    checks.push(found(
        "tftp",
        netboot::write_script(&script.with_extension("check"), "# dukovo-lab machines check\n")
            .map(|()| {
                let _ = fs::remove_file(script.with_extension("check"));
                format!("boot scripts can be written in {}", side.tftp.display())
            })
            .map_err(problem),
    ));
    let grub = side.tftp.join(netboot::GRUB_FILE);
    checks.push(found(
        "network grub",
        if grub.is_file() {
            Ok(format!("{} is there", grub.display()))
        } else {
            Err(problem(format!("{} is missing: `ob lab` puts it there", grub.display())))
        },
    ));
    checks.push(found(
        "writer",
        Kit::read(side.writer)
            .map(|_| format!("the writer's kit is in {}", side.writer.display()))
            .map_err(|err| problem(format!("{err}: `ob lab` installs it"))),
    ));
    checks.push(found(
        "lab key",
        match key {
            Some(key) if key.is_file() => Ok("the lab has its key".into()),
            _ => Err(problem("the lab has no key: run `dukovo-lab key`".into())),
        },
    ));
    checks
}

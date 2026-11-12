// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The AMT driver: a real machine with Intel AMT on the lab network.
//!
//! - **Power** is AMT's: on, reset, a hard off for a power cut (the
//!   system gets no warning, as when the cord is pulled), and a graceful
//!   off for the power button, which only works when the system answers
//!   AMT's request.
//! - **A fresh boot** writes the image under test to the machine's disk
//!   first: AMT boots the machine from the network once, the writer the
//!   net-boot server hands it writes the image and checks it, and the
//!   machine restarts into its disk (see netboot.rs). Only the disk with
//!   the machine file's serial number is ever written.
//! - **The serial console** comes over a serial cable into a port of the
//!   lab PC's, one log per boot (see serial.rs).
//! - **SSH** and the lab PC's side of a test reach the machine on the lab
//!   network, where it has an address of its own, so the machine's ports
//!   are reached as they are.
//! - **The Linux twin** is the machine itself, net booted into the
//!   Linux kit; the runner boots afresh after it.
//! - **The screen, keyboard and mouse** are AMT's remote screen (KVM),
//!   reached as a VNC server on AMT's port 5900 (see vnc.rs). The driver
//!   turns it on the first time a test wants it, with a password made for
//!   the run, and keeps one session open from then on: AMT plugs its
//!   keyboard and mouse into the machine when a session opens, so a
//!   session that stays open keeps them there across boots.
//!
//! Cables, sticks, cards and discs are a person's to handle. What the
//! driver does goes in the bundle's `logs/<machine>/amt.log`, never with
//! the passwords.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{Driver, Setup};
use crate::amt::{self, Amt, PowerChange};
use crate::bench::Kit;
use crate::machines::AmtSettings;
use crate::netboot::{self, Image, Offer, Outcome, Server};
use crate::serial::SerialReader;
use crate::ssh;
use crate::suite::{Button, PowerAction, Stick};
use crate::vnc::{self, Vnc};
use crate::{png, screen};

/// The longest a fresh boot's net boot and write may take: the firmware
/// starting twice, the writer's kernel loading, and a few hundred
/// megabytes written and read back.
pub const WRITE_TIME: Duration = Duration::from_secs(4 * 60);

/// How long the machine is given to go off after a hard off.
const OFF_TIME: Duration = Duration::from_secs(30);

/// How long a hard off is waited for before AMT is asked again: AMT takes
/// one asked while the machine is starting and does nothing with it.
const HARD_OFF_AGAIN: Duration = Duration::from_secs(5);

/// How long AMT may go on saying the machine is off after turning it on.
const ON_TIME: Duration = Duration::from_secs(30);

/// How long a power state read from AMT is trusted: waits ask often.
const POWER_FRESH: Duration = Duration::from_secs(1);

/// The port the machine's SSH server listens on.
const SSH_PORT: u16 = 22;

/// How long AMT may take to see a remote screen session close, and take
/// power changes again.
const SCREEN_CLOSE_TIME: Duration = Duration::from_secs(10);

/// How long the remote screen may take to open a session: AMT may still
/// be closing the one before.
const SCREEN_OPEN_TIME: Duration = Duration::from_secs(30);

/// How long the keyboard and mouse AMT plugs in when a session opens are
/// given to appear to the system before they are used.
const SCREEN_SETTLE: Duration =
    if cfg!(test) { Duration::from_millis(100) } else { Duration::from_secs(2) };

/// How long the whole screen may take to arrive: a few megabytes of raw
/// pixels.
const CAPTURE_TIME: Duration = Duration::from_secs(90);

/// How long a key is held, and the pause after each combination.
const KEY_HOLD: Duration = Duration::from_millis(50);
const KEY_GAP: Duration = Duration::from_millis(100);

/// A real machine driven through AMT.
pub struct AmtDriver {
    settings: AmtSettings,
    setup: Setup,
    amt: Amt,
    server: Server,
    serial: Arc<SerialReader>,
    /// The current boot's serial log.
    serial_log: PathBuf,
    /// Where this boot's script goes in the TFTP folder.
    script: PathBuf,
    writer: Kit,
    /// The image under test, read once, on the first fresh boot.
    image: Option<Arc<Image>>,
    boot: u32,
    /// The power state last read, and when.
    power: Option<(Instant, bool)>,
    /// AMT's remote screen: its address, the password set for this run
    /// once it is turned on, the session and when it opened, and where
    /// the pointer was put last.
    screen_address: String,
    screen_password: Option<String>,
    screen: Option<(Vnc, Instant)>,
    pointer: (u16, u16),
    log: File,
    started: Instant,
}

impl AmtDriver {
    /// Gets ready to drive the machine: reads AMT's password and the
    /// writer's kit, and starts the net-boot server and the serial
    /// reader. Leaves the machine's power as it is.
    pub fn new(settings: AmtSettings, setup: Setup) -> Result<AmtDriver, String> {
        let password = read_password(&settings.password_file)?;
        let writer = Kit::read(&settings.writer).map_err(|err| {
            format!(
                "{err} (the writer's kit; `ob lab` installs it in {})",
                settings.writer.display()
            )
        })?;
        let lab: IpAddr = settings
            .lab_address
            .parse()
            .map_err(|_| format!("amt.lab_address {} is not an address", settings.lab_address))?;
        let server = Server::start(lab, settings.boot_port)?;
        fs::create_dir_all(&setup.logs)
            .and_then(|()| fs::create_dir_all(&setup.work))
            .map_err(|err| format!("making the machine's folders: {err}"))?;
        // Until the first boot, what the port says goes to a scratch log.
        let before = setup.work.join("serial-before.log");
        let serial = Arc::new(SerialReader::start(&settings.serial, settings.baud, &before)?);
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(setup.logs.join("amt.log"))
            .map_err(|err| format!("making amt.log: {err}"))?;
        let script = netboot::script_path(&settings.tftp, &settings.mac);
        let amt = Amt::new(&settings.address, &settings.user, &password);
        let host = settings.address.split(':').next().unwrap_or_default();
        let screen_address = format!("{host}:{}", vnc::PORT);
        let mut driver = AmtDriver {
            settings,
            setup,
            amt,
            server,
            serial,
            serial_log: before,
            script,
            writer,
            image: None,
            boot: 0,
            power: None,
            screen_address,
            screen_password: None,
            screen: None,
            pointer: (0, 0),
            log,
            started: Instant::now(),
        };
        let ports: Vec<String> =
            driver.server.addresses().iter().map(|address| address.port().to_string()).collect();
        driver.note(&format!(
            "AMT at {}; net-boot server at {}, ports {}; serial port {}",
            driver.settings.address,
            driver.settings.lab_address,
            ports.join(", "),
            driver.settings.serial.display()
        ));
        Ok(driver)
    }

    /// Writes a line to amt.log, with the seconds since the driver began.
    fn note(&mut self, line: &str) {
        let at = self.started.elapsed().as_secs_f64();
        let _ = writeln!(self.log, "{at:9.1}s {line}");
    }

    /// Writes what the net-boot server did to amt.log.
    fn note_server(&mut self) {
        for event in self.server.take_events() {
            self.note(&format!("net boot: {event}"));
        }
    }

    /// Changes the power through AMT, and notes it. AMT turns power
    /// changes down while a remote screen session is open (AMT 11.8 on an
    /// OptiPlex 7040 answers ReturnValue 2), so the session is closed
    /// first, and the change tried again while AMT sees it close; the next
    /// look at the screen opens another.
    fn change(&mut self, change: PowerChange) -> Result<(), String> {
        let screen_was_open = self.screen.take().is_some();
        if screen_was_open {
            self.note("remote screen: session closed for a power change");
        }
        self.power = None;
        self.note(&format!("power: {change:?}"));
        let deadline = Instant::now() + SCREEN_CLOSE_TIME;
        let changed = loop {
            let changed = self.amt.change_power(change);
            if changed.is_ok() || !screen_was_open || Instant::now() >= deadline {
                break changed;
            }
            std::thread::sleep(Duration::from_millis(500));
        };
        if let Err(err) = &changed {
            self.note(&format!("power: {err}"));
        }
        changed
    }

    /// Turns the machine off at once if it is on, and waits until it is,
    /// asking again while it stays on.
    fn hard_off(&mut self) -> Result<(), String> {
        if !self.read_power()? {
            return Ok(());
        }
        let deadline = Instant::now() + OFF_TIME;
        loop {
            self.change(PowerChange::HardOff)?;
            match self.wait_off((Instant::now() + HARD_OFF_AGAIN).min(deadline)) {
                Ok(()) => return Ok(()),
                Err(err) if Instant::now() >= deadline || self.setup.stop.requested() => {
                    return Err(err);
                }
                Err(_) => self.note("power: the machine is still on; asking again"),
            }
        }
    }

    /// Turns the machine on, and waits until AMT says it is: for a moment
    /// after, AMT still says it is off, which a wait would take as the
    /// machine going off again.
    fn turn_on(&mut self) -> Result<(), String> {
        self.change(PowerChange::On)?;
        let deadline = Instant::now() + ON_TIME;
        loop {
            let said = self.read_power();
            if said == Ok(true) {
                return Ok(());
            }
            if Instant::now() >= deadline || self.setup.stop.requested() {
                return Err(match said {
                    Err(err) => format!("the machine did not come on: {err}"),
                    _ => format!(
                        "the machine did not come on: AMT still said it was off {} s after \
                         turning it on",
                        ON_TIME.as_secs()
                    ),
                });
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Waits until AMT says the machine is off.
    fn wait_off(&mut self, deadline: Instant) -> Result<(), String> {
        loop {
            if !self.read_power()? {
                return Ok(());
            }
            if Instant::now() >= deadline || self.setup.stop.requested() {
                return Err("the machine was still on".into());
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Reads whether the machine is on from AMT now.
    fn read_power(&mut self) -> Result<bool, String> {
        let state = self.amt.power_state()?;
        let on = amt::is_on(state);
        self.power = Some((Instant::now(), on));
        Ok(on)
    }

    /// Boots the machine from the network once into what `offer` hands
    /// out, loading it with `cmdline`.
    fn net_boot(&mut self, what: &str, offer: Offer, cmdline: &str) -> Result<(), String> {
        let base = self.server.offer(offer)?;
        let cmdline = cmdline.replace("{base}", &base);
        netboot::write_script(&self.script, &netboot::grub_script(what, &base, &cmdline))?;
        self.note(&format!("net boot: {what}, boot script {}", self.script.display()));
        self.amt.boot_from_network_once()?;
        self.turn_on()
    }

    /// Ends a net boot: its script and offer go, so a later boot from the
    /// network goes on to the disk.
    fn end_net_boot(&mut self) {
        netboot::remove_script(&self.script);
        self.server.withdraw();
        self.note_server();
    }

    /// The image under test, read the first time it is wanted.
    fn image(&mut self) -> Result<Arc<Image>, String> {
        if let Some(image) = &self.image {
            return Ok(Arc::clone(image));
        }
        let started = Instant::now();
        let image = Arc::new(Image::scan(&self.setup.image)?);
        self.note(&format!(
            "image {}: {} bytes, {} of data in {} runs, SHA-256 {}, read in {:.1}s",
            self.setup.image.display(),
            image.size,
            image.data_bytes(),
            image.extents.len(),
            image.sha256,
            started.elapsed().as_secs_f64()
        ));
        self.image = Some(Arc::clone(&image));
        Ok(image)
    }

    /// Writes the image to the disk through the writer; returns once the
    /// writer said it wrote it and the hash was right.
    fn write_image(&mut self) -> Result<(), String> {
        let image = self.image()?;
        let writer_log = self.setup.logs.join(format!("writer-{}.log", self.boot));
        self.serial.switch_to(&writer_log)?;
        let written = {
            let (serial, next, script) =
                (Arc::clone(&self.serial), self.serial_log.clone(), self.script.clone());
            // Before the writer is told to restart: its next boot must not
            // find the script, and its console goes to the boot's log.
            Box::new(move || {
                netboot::remove_script(&script);
                let _ = serial.switch_to(&next);
            })
        };
        let offer = Offer::Writer {
            kit: self.writer.clone(),
            image,
            disk: self.settings.disk.clone(),
            written,
        };
        // The writer sets its own address: the card with the machine
        // file's MAC gets the machine's address on the lab's /24.
        let cmdline = format!(
            "{} dukovo.lab={{base}} dukovo.lab.ip={}/24 dukovo.lab.mac={}",
            with_console(&self.writer.cmdline, &self.settings.console),
            self.settings.machine_address,
            self.settings.mac.to_ascii_lowercase()
        );
        let deadline = Instant::now() + WRITE_TIME;
        let booted = self.net_boot("the writer", offer, &cmdline);
        let outcome = booted.and_then(|()| {
            loop {
                self.note_server();
                match self.server.outcome() {
                    Some(Outcome::Written) => break Ok(()),
                    Some(Outcome::Failed(why)) => break Err(why),
                    None => {}
                }
                if self.setup.stop.requested() {
                    break Err("the run was stopped".into());
                }
                if Instant::now() >= deadline {
                    let minutes = WRITE_TIME.as_secs() / 60;
                    break Err(if self.server.loaded() {
                        format!("the writer did not finish within {minutes} minutes")
                    } else {
                        format!(
                            "the machine did not load the writer within {minutes} minutes: no \
                             request for its kernel came, so its firmware or the network GRUB \
                             may have gone on to the disk"
                        )
                    });
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        self.end_net_boot();
        match outcome {
            Ok(()) => {
                self.note("net boot: the disk holds the image; the machine restarts into it");
                Ok(())
            }
            Err(why) => {
                // A writer that failed must not be left running.
                let _ = self.hard_off();
                let _ = self.serial.switch_to(&self.serial_log);
                self.note(&format!("net boot: {why}"));
                Err(format!(
                    "{why}; the writer's console is in {}, and what the driver did in amt.log",
                    writer_log.display()
                ))
            }
        }
    }
}

impl AmtDriver {
    /// Opens a remote screen session unless one is open, turning AMT's
    /// remote screen on first the first time.
    fn open_screen(&mut self) -> Result<(), String> {
        if let Some((vnc, _)) = &mut self.screen
            && vnc.is_open()
        {
            return Ok(());
        }
        self.screen = None;
        let password = match &self.screen_password {
            Some(password) => password.clone(),
            None => {
                let password = amt::screen_password()?;
                self.note("remote screen: turning on port 5900, with a password made for this run");
                if let Err(err) = self.amt.turn_on_remote_screen(&password) {
                    self.note(&format!("remote screen: {err}"));
                    return Err(err);
                }
                self.screen_password = Some(password.clone());
                password
            }
        };
        let deadline = Instant::now() + SCREEN_OPEN_TIME;
        match Vnc::connect(&self.screen_address, &password, deadline) {
            Ok(vnc) => {
                let (width, height) = vnc.size();
                self.note(&format!(
                    "remote screen: session open, {width}x{height} \"{}\"",
                    vnc.name
                ));
                self.screen = Some((vnc, Instant::now()));
                Ok(())
            }
            Err(err) => {
                self.note(&format!("remote screen: {err}"));
                Err(err)
            }
        }
    }

    /// Does `act` in the remote screen session, opening one if need be.
    /// Keys and the pointer (`input`) wait until the session's keyboard
    /// and mouse have had time to appear. A session that fails is closed,
    /// and the next action opens another.
    fn with_screen<T>(
        &mut self,
        input: bool,
        act: impl FnOnce(&mut Vnc) -> Result<T, String>,
    ) -> Result<T, String> {
        if !self.is_on() {
            return Err("the machine is off, so it has no screen".into());
        }
        self.open_screen()?;
        let Some((vnc, opened)) = self.screen.as_mut() else {
            return Err("the remote screen has no session".into());
        };
        if input && let Some(wait) = SCREEN_SETTLE.checked_sub(opened.elapsed()) {
            std::thread::sleep(wait);
        }
        let done = act(vnc);
        if let Err(err) = &done {
            self.screen = None;
            self.note(&format!("remote screen: {err}"));
        }
        done
    }

    /// The whole screen now, with one more try in a new session if the
    /// one open failed (AMT may end a session when the machine restarts).
    fn capture(&mut self) -> Result<screen::Image, String> {
        let take = |vnc: &mut Vnc| vnc.capture(Instant::now() + CAPTURE_TIME);
        self.with_screen(false, take).or_else(|_| self.with_screen(false, take))
    }
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

/// Checks what a run on the machine needs, without changing anything:
/// AMT's password, address and mode, its remote screen when the driver
/// does the machine's `screen`, the serial port, the lab network, the TFTP
/// folder and network GRUB, the writer's kit and the lab key.
pub fn check(settings: &AmtSettings, key: Option<&Path>, screen: bool) -> Vec<Check> {
    let mut checks = Vec::new();
    let mut add = |check: &'static str, result: Result<String, (Verdict, String)>| {
        let (verdict, detail) = match result {
            Ok(detail) => (Verdict::Ok, detail),
            Err((verdict, detail)) => (verdict, detail),
        };
        checks.push(Check { check, verdict, detail });
    };
    let problem = |detail: String| (Verdict::Problem, detail);
    let password = read_password(&settings.password_file);
    add(
        "password",
        password
            .as_ref()
            .map(|_| "AMT's password file is readable by this user alone".into())
            .map_err(|err| problem(err.clone())),
    );
    if let Ok(password) = &password {
        let amt = Amt::new(&settings.address, &settings.user, password);
        match amt.version() {
            Err(err) => add("amt", Err(problem(err))),
            Ok(version) => {
                add("amt", Ok(format!("AMT {version} at {}", settings.address)));
                add(
                    "control mode",
                    match amt.control_mode() {
                        Ok(2) => Ok("Admin Control Mode".into()),
                        Ok(mode) => Err(problem(format!(
                            "control mode {mode}, not Admin Control Mode (2): the remote screen \
                             would wait for someone at the machine; set it in MEBx (Ctrl+P)"
                        ))),
                        Err(err) => Err(problem(err)),
                    },
                );
                add(
                    "power",
                    amt.power_state()
                        .map(|state| format!("the machine is {}", amt::describe(state)))
                        .map_err(problem),
                );
                if screen {
                    add(
                        "remote screen",
                        match amt.remote_screen() {
                            Ok(found) if !found.in_mebx => Err(problem(
                                "KVM is off in MEBx: turn on KVM Feature Selection there, or set \
                                 keys, mouse and screen to \"none\" in the machine file"
                                    .into(),
                            )),
                            Ok(found) if found.consent != 0 => Err(problem(format!(
                                "AMT asks someone at the machine for a code before each remote \
                                 screen session (OptInRequired {}): set User Opt-in to None in MEBx",
                                found.consent
                            ))),
                            Ok(_) => Ok("KVM is on in MEBx and asks for no consent; the driver \
                                         turns on port 5900 when a test wants the screen"
                                .into()),
                            Err(err) => Err(problem(err)),
                        },
                    );
                }
                add(
                    "power button",
                    match amt.power_changes() {
                        Ok(states) if states.contains(&12) => {
                            Ok("AMT can ask the system to shut down".into())
                        }
                        Ok(_) => Err((
                            Verdict::Note,
                            "AMT cannot ask the system to shut down here, so steps that press \
                             the power button fail on this machine"
                                .into(),
                        )),
                        Err(err) => Err(problem(err)),
                    },
                );
            }
        }
    }
    add(
        "serial port",
        crate::serial::check_port(&settings.serial, settings.baud)
            .map(|()| format!("{} opens, at {} baud", settings.serial.display(), settings.baud))
            .map_err(problem),
    );
    let lab = settings.lab_address.as_str();
    add(
        "lab network",
        std::net::TcpListener::bind((lab, 0))
            .map(|_| format!("this PC has {lab}, the lab address"))
            .map_err(|err| {
                problem(format!(
                    "this PC does not have {lab} ({err}): give the lab card that address"
                ))
            }),
    );
    add(
        "boot port",
        netboot::ports(settings.boot_port).map_err(problem).and_then(|ports| {
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
    );
    let script = netboot::script_path(&settings.tftp, &settings.mac);
    add(
        "tftp",
        netboot::write_script(&script.with_extension("check"), "# dukovo-lab machines check\n")
            .map(|()| {
                let _ = fs::remove_file(script.with_extension("check"));
                format!("boot scripts can be written in {}", settings.tftp.display())
            })
            .map_err(problem),
    );
    let grub = settings.tftp.join(netboot::GRUB_FILE);
    add(
        "network grub",
        if grub.is_file() {
            Ok(format!("{} is there", grub.display()))
        } else {
            Err(problem(format!("{} is missing: `ob lab` puts it there", grub.display())))
        },
    );
    add(
        "writer",
        Kit::read(&settings.writer)
            .map(|_| format!("the writer's kit is in {}", settings.writer.display()))
            .map_err(|err| problem(format!("{err}: `ob lab` installs it"))),
    );
    add(
        "lab key",
        match key {
            Some(key) if key.is_file() => Ok("the lab has its key".into()),
            _ => Err(problem("the lab has no key: run `dukovo-lab key`".into())),
        },
    );
    checks
}

/// Reads AMT's password from its file, which only this user may read.
fn read_password(path: &Path) -> Result<String, String> {
    let meta = fs::metadata(path).map_err(|err| {
        format!("AMT's password file {}: {err} (amt.password_file)", path.display())
    })?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "AMT's password file {} can be read by other users: `chmod 600 {}`",
            path.display(),
            path.display()
        ));
    }
    let text = fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let password = text.trim_end_matches(['\n', '\r']);
    if password.is_empty() || password.contains('\n') {
        return Err(format!("{} must hold AMT's password alone, on one line", path.display()));
    }
    Ok(password.to_string())
}

/// A kit's kernel command line with the machine's console in place of
/// the kit's own. Kits name `console=ttyS0`, as QEMU's machines have it,
/// and Linux keeps the options of a console's first mention: a later
/// `console=ttyS0,115200` would leave the port at its default 9600 baud,
/// which the lab reads at 115200 as a run of zero bytes.
fn with_console(cmdline: &str, console: &str) -> String {
    let mut words: Vec<&str> =
        cmdline.split_whitespace().filter(|word| !word.starts_with("console=")).collect();
    let console = format!("console={console}");
    words.push(&console);
    words.join(" ")
}

/// Says that a person does `what` on a real machine.
fn persons_job(what: &str) -> String {
    format!(
        "the AMT driver cannot {what}; a person does it on a real machine (\"person\" in the \
         machine file's [controls])"
    )
}

impl Driver for AmtDriver {
    fn boot_fresh(&mut self) -> Result<(), String> {
        self.boot += 1;
        self.serial_log = self.setup.logs.join(format!("serial-{}.log", self.boot));
        self.note(&format!("boot {}: afresh", self.boot));
        self.hard_off()?;
        self.write_image()
    }

    fn boot_stick(&mut self, _stick: &Path, _disk: u64) -> Result<(), String> {
        Err("real machines do not boot the install stick yet; installer tests run on QEMU".into())
    }

    fn boot_dvd(&mut self, _dvd: &Path, _disk: u64) -> Result<(), String> {
        Err("real machines do not boot the install DVD yet; installer tests run on QEMU".into())
    }

    fn power(&mut self, action: PowerAction, deadline: Instant) -> Result<(), String> {
        let graceful = |err: String| {
            format!(
                "{err}: AMT asks the system to shut down only when the system answers it, \
                 which Dukovo does not yet"
            )
        };
        match action {
            PowerAction::On => {
                if self.read_power()? {
                    return Ok(());
                }
                self.turn_on()
            }
            PowerAction::Off => {
                self.change(PowerChange::SoftOff).map_err(graceful)?;
                self.wait_off(deadline)
            }
            PowerAction::Button => self.change(PowerChange::SoftOff).map_err(graceful),
            PowerAction::Reset => self.change(PowerChange::Reset),
            PowerAction::Cut => self.hard_off(),
        }
    }

    fn is_on(&mut self) -> bool {
        if let Some((read, on)) = self.power
            && read.elapsed() < POWER_FRESH
        {
            return on;
        }
        match self.read_power() {
            Ok(on) => on,
            Err(err) => {
                // As it was last seen, or on when nothing has been read
                // since a power change: waits go on to their deadlines, and
                // only AMT's word ends one as off.
                self.note(&format!("power state: {err}"));
                let last = self.power.is_none_or(|(_, on)| on);
                self.power = Some((Instant::now(), last));
                last
            }
        }
    }

    fn serial_log(&self) -> &Path {
        &self.serial_log
    }

    fn set_link(&mut self, _up: bool) -> Result<(), String> {
        Err(persons_job("plug or pull the network cable"))
    }

    fn plug_usb(&mut self, _stick: &Stick) -> Result<(), String> {
        Err(persons_job("plug in a USB stick"))
    }

    fn unplug_usb(&mut self) -> Result<(), String> {
        Err(persons_job("pull out a USB stick"))
    }

    fn press_keys(&mut self, combos: &[String]) -> Result<(), String> {
        // Every name is checked before any key goes down.
        let combos =
            combos.iter().map(|combo| vnc::keysyms(combo)).collect::<Result<Vec<_>, _>>()?;
        self.with_screen(true, |vnc| {
            for keys in &combos {
                for &key in keys {
                    vnc.key(key, true)?;
                }
                std::thread::sleep(KEY_HOLD);
                for &key in keys.iter().rev() {
                    vnc.key(key, false)?;
                }
                std::thread::sleep(KEY_GAP);
            }
            Ok(())
        })
    }

    fn point(&mut self, x: u32, y: u32) -> Result<(), String> {
        let (mut width, mut height) = self.with_screen(false, |vnc| Ok(vnc.size()))?;
        if x >= width || y >= height {
            // The size known may be from before the screen last changed.
            let now = self.capture()?;
            (width, height) = (now.width as u32, now.height as u32);
        }
        let (Ok(x), Ok(y)) = (u16::try_from(x), u16::try_from(y)) else {
            return Err(format!("({x}, {y}) is off the screen"));
        };
        if u32::from(x) >= width || u32::from(y) >= height {
            return Err(format!("({x}, {y}) is off the screen, which is {width}x{height} pixels"));
        }
        self.with_screen(true, |vnc| vnc.pointer(0, x, y))?;
        self.pointer = (x, y);
        Ok(())
    }

    fn click(&mut self, button: Button) -> Result<(), String> {
        let held = match button {
            Button::Left => 1,
            Button::Middle => 2,
            Button::Right => 4,
        };
        let (x, y) = self.pointer;
        self.with_screen(true, |vnc| {
            for buttons in [held, 0] {
                vnc.pointer(buttons, x, y)?;
                // As long as a person holds a button.
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(())
        })
    }

    fn screenshot(&mut self, to: &Path) -> Result<(), String> {
        let image = self.capture()?;
        fs::write(to, png::encode(&image)).map_err(|err| format!("{}: {err}", to.display()))
    }

    fn screen_dump(&mut self, to: &Path) -> Result<(), String> {
        let image = self.capture()?;
        fs::write(to, image.to_ppm()).map_err(|err| format!("{}: {err}", to.display()))
    }

    fn run_command(
        &mut self,
        command: &str,
        input: Option<&str>,
        deadline: Instant,
    ) -> Result<ssh::Output, String> {
        let key = self
            .setup
            .key
            .clone()
            .ok_or("the lab has no key: run `dukovo-lab key`, then build the image again")?;
        if !self.is_on() {
            return Err("the machine is off".into());
        }
        let target = ssh::Target {
            host: self.settings.machine_address.clone(),
            port: SSH_PORT,
            key,
            known_hosts: self.setup.work.join(format!("known_hosts-{}", self.boot)),
            scratch: self.setup.work.join("ssh-output"),
        };
        target.run(command, input, deadline, &self.setup.stop)
    }

    fn peer_address(&self) -> Option<String> {
        Some(self.settings.lab_address.clone())
    }

    fn machine_address(&self) -> Option<String> {
        Some(self.settings.machine_address.clone())
    }

    fn reach(&mut self, port: u16, _udp: bool) -> Result<u16, String> {
        // The lab PC is on the machine's network: its ports are its own.
        Ok(port)
    }

    fn slowdown(&self) -> u32 {
        1
    }

    fn boot_time(&self) -> Duration {
        WRITE_TIME
    }

    fn twin_ends_boot(&self) -> bool {
        true
    }

    fn linux_twin(&mut self, kit: &Kit, log: &Path, deadline: Instant) -> Result<String, String> {
        self.note("Linux twin: booting");
        self.hard_off()?;
        self.serial.switch_to(log)?;
        let cmdline = with_console(&kit.cmdline, &self.settings.console);
        let booted = self.net_boot("the Linux twin", Offer::Twin { kit: kit.clone() }, &cmdline);
        let measured = booted.and_then(|()| loop {
            let text = fs::read_to_string(log).unwrap_or_default();
            if text.contains(&kit.done) {
                break Ok(text);
            }
            if self.setup.stop.requested() {
                break Err("the run was stopped".into());
            }
            if Instant::now() >= deadline {
                break Err(format!(
                    "Linux did not print \"{}\" before the test's time limit; its console is in {}",
                    kit.done,
                    log.display()
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        });
        self.end_net_boot();
        let _ = self.hard_off();
        let _ = self.serial.switch_to(&self.setup.work.join("serial-after-twin.log"));
        self.note(&format!("Linux twin: {}", if measured.is_ok() { "measured" } else { "failed" }));
        measured
    }

    fn emulated(&self) -> bool {
        false
    }

    fn shut_off(&mut self) {
        self.screen = None;
        self.end_net_boot();
        if let Err(err) = self.hard_off() {
            self.note(&format!("shut off: {err}"));
        }
    }
}

impl Drop for AmtDriver {
    fn drop(&mut self) {
        netboot::remove_script(&self.script);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amt::fake::{FakeAmt, PASSWORD, USER};
    use crate::netboot::tests::{make_image, pretend_writer};
    use crate::stop::Stop;
    use crate::testing::TempDir;
    use crate::vnc::fake::{Event, FakeVnc, pattern};

    const MAC: &str = "aa:bb:cc:00:11:22";

    /// A machine behind a pretend AMT: a net boot runs the pretend writer
    /// (or prints what a Linux twin would), and the disk boots to its
    /// ready line, on a plain file standing in for the serial port.
    struct Bench {
        dir: TempDir,
        fake: FakeAmt,
        /// AMT's remote screen, open once the pretend AMT turns it on.
        screen: FakeVnc,
        driver: AmtDriver,
    }

    fn bench(label: &str, tamper: bool) -> Bench {
        let dir = TempDir::new(label);
        let port = dir.write("ttyUSB0", "");
        dir.write("writer/kernel", "KERNEL");
        dir.write("writer/initrd", "INITRD");
        dir.write(
            "writer/kit.toml",
            "kernel = \"kernel\"\ninitrd = \"initrd\"\ncmdline = \"console=ttyS0 rdinit=/writer\"\n\
             done = \"writer: done\"\n",
        );
        let password = dir.write("amt-password", &format!("{PASSWORD}\n"));
        fs::set_permissions(&password, fs::Permissions::from_mode(0o600)).unwrap();
        let fake = FakeAmt::start();
        let tftp = dir.path().join("tftp");
        let disk = dir.path().join("disk");
        {
            let (tftp, port, disk) = (tftp.clone(), port.clone(), disk.clone());
            fake.state.lock().unwrap().on_boot = Some(Box::new(move |network| {
                let (tftp, port, disk) = (tftp.clone(), port.clone(), disk.clone());
                // As the machine would: the firmware, then what it boots.
                std::thread::spawn(move || {
                    let say = |text: &str| {
                        let mut port = OpenOptions::new().append(true).open(&port).unwrap();
                        port.write_all(text.as_bytes()).unwrap();
                    };
                    let script = fs::read_to_string(netboot::script_path(&tftp, MAC));
                    let Some(script) = script.ok().filter(|_| network) else {
                        say("Boot complete.\n");
                        return;
                    };
                    let Some(base) =
                        script.split_whitespace().find_map(|word| word.strip_prefix("dukovo.lab="))
                    else {
                        say("BENCH name=spawn ns=120.0\nlinux-bench: done\n");
                        return;
                    };
                    // Kept for the test to read: the boot script is
                    // gone once the boot ends.
                    fs::write(tftp.with_file_name("writer-script"), &script).unwrap();
                    say("writer: writing\n");
                    if pretend_writer(base, &disk, tamper).unwrap() == "restart" {
                        say("Boot complete.\n");
                    }
                });
            }));
        }
        let settings = AmtSettings {
            address: fake.address.to_string(),
            user: USER.into(),
            password_file: password,
            mac: MAC.into(),
            disk: "TESTDISK".into(),
            serial: port,
            baud: 115_200,
            machine_address: "127.0.0.1".into(),
            lab_address: "127.0.0.1".into(),
            boot_port: 0,
            tftp,
            writer: dir.path().join("writer"),
            console: "ttyS4,115200".into(),
        };
        let setup = Setup {
            image: make_image(&dir, 400_000),
            logs: dir.path().join("logs"),
            work: dir.path().join("work"),
            sockets: dir.path().join("sockets"),
            key: None,
            ob: None,
            stop: Stop::default(),
        };
        let mut driver = AmtDriver::new(settings, setup).unwrap();
        let screen = {
            let state = Arc::clone(&fake.state);
            FakeVnc::start(Box::new(move || state.lock().unwrap().screen.open_password()))
        };
        let live = Arc::clone(&screen.state);
        fake.state.lock().unwrap().screen_busy =
            Some(Box::new(move || live.lock().unwrap().live > 0));
        driver.screen_address = screen.address.to_string();
        Bench { dir, fake, screen, driver }
    }

    fn wait_for(path: &Path, text: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if fs::read_to_string(path).is_ok_and(|log| log.contains(text)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn the_machine_console_takes_the_place_of_the_kits() {
        assert_eq!(
            with_console("console=ttyS0 loglevel=4", "ttyS0,115200"),
            "loglevel=4 console=ttyS0,115200"
        );
        assert_eq!(
            with_console("quiet console=ttyS0 console=tty0 rdinit=/writer", "ttyS5,115200"),
            "quiet rdinit=/writer console=ttyS5,115200"
        );
    }

    #[test]
    fn a_fresh_boot_writes_the_image_and_boots_the_disk() {
        let mut bench = bench("amt-fresh", false);
        bench.driver.boot_fresh().unwrap();
        let serial = bench.driver.serial_log().to_path_buf();
        assert!(serial.ends_with("logs/serial-1.log"));
        assert!(wait_for(&serial, "Boot complete."));
        let disk = fs::read(bench.dir.path().join("disk")).unwrap();
        assert_eq!(disk, fs::read(&bench.driver.setup.image).unwrap());
        // The writer's console has a log of its own, and the boot script
        // is gone so the next boot is the disk's.
        let writer_log = bench.dir.path().join("logs/writer-1.log");
        assert!(fs::read_to_string(writer_log).unwrap().contains("writer: writing"));
        assert!(!fs::read_to_string(&serial).unwrap().contains("writer:"));
        assert!(!netboot::script_path(&bench.driver.settings.tftp, MAC).exists());
        // The writer is told its own address and its card's MAC.
        let script = fs::read_to_string(bench.dir.path().join("writer-script")).unwrap();
        assert!(
            script.contains(&format!("dukovo.lab.ip=127.0.0.1/24 dukovo.lab.mac={MAC}")),
            "{script}"
        );
        assert_eq!(script.matches("console=").count(), 1, "{script}");
        assert!(script.contains("console=ttyS4,115200"), "{script}");
        assert!(bench.driver.is_on());
        assert_eq!(
            bench.fake.calls(),
            ["ChangeBootOrder", "SetBootConfigRole", "RequestPowerStateChange 2"]
        );

        // A power cut is AMT's hard off; on boots the disk again.
        let soon = Instant::now() + Duration::from_secs(5);
        bench.driver.power(PowerAction::Cut, soon).unwrap();
        assert!(!bench.driver.is_on());
        bench.driver.power(PowerAction::On, soon).unwrap();
        assert!(bench.driver.is_on());
        // The power button needs the system's answer, which it lacks.
        let err = bench.driver.power(PowerAction::Button, soon).unwrap_err();
        assert!(err.contains("does not yet"), "{err}");

        // A second fresh boot turns the machine off first.
        bench.driver.boot_fresh().unwrap();
        assert!(bench.driver.serial_log().ends_with("logs/serial-2.log"));
        assert!(wait_for(bench.driver.serial_log(), "Boot complete."));
        let calls = bench.fake.calls();
        assert!(calls.ends_with(&[
            "RequestPowerStateChange 8".into(),
            "ChangeBootOrder".into(),
            "SetBootConfigRole".into(),
            "RequestPowerStateChange 2".into(),
        ]));
        let log = fs::read_to_string(bench.dir.path().join("logs/amt.log")).unwrap();
        assert!(log.contains("net boot: GET plan"), "{log}");
        assert!(!log.contains(PASSWORD));
        assert_eq!(bench.driver.peer_address().as_deref(), Some("127.0.0.1"));
        assert_eq!(bench.driver.reach(445, false), Ok(445));
    }

    #[test]
    fn a_power_state_amt_does_not_give_is_as_last_seen_or_on() {
        let mut bench = bench("amt-power-unknown", false);
        let soon = Instant::now() + Duration::from_secs(5);
        bench.driver.power(PowerAction::On, soon).unwrap();
        // Nothing read since the change: on, so waits go on.
        bench.fake.state.lock().unwrap().answering = false;
        assert!(bench.driver.is_on());
        // Off as AMT last said.
        bench.fake.state.lock().unwrap().answering = true;
        bench.driver.power(PowerAction::Cut, soon).unwrap();
        bench.fake.state.lock().unwrap().answering = false;
        std::thread::sleep(POWER_FRESH);
        assert!(!bench.driver.is_on());
        let log = fs::read_to_string(bench.dir.path().join("logs/amt.log")).unwrap();
        assert!(log.contains("power state: AMT at"), "{log}");
    }

    #[test]
    fn power_changes_wait_out_amt_while_the_machine_starts() {
        let mut bench = bench("amt-power-timing", false);
        let soon = Instant::now() + Duration::from_secs(10);
        // AMT says off for a moment after turning the machine on: on
        // returns only once AMT says on, so a wait after it goes on.
        bench.fake.state.lock().unwrap().off_after_on = 3;
        bench.driver.power(PowerAction::On, soon).unwrap();
        assert!(bench.driver.is_on());
        // A hard off AMT takes but does nothing with is asked again.
        bench.fake.state.lock().unwrap().ignored_hard_offs = 1;
        bench.driver.power(PowerAction::Cut, soon).unwrap();
        assert!(!bench.driver.is_on());
        let hard_offs =
            bench.fake.calls().iter().filter(|call| *call == "RequestPowerStateChange 8").count();
        assert_eq!(hard_offs, 2);
        let log = fs::read_to_string(bench.dir.path().join("logs/amt.log")).unwrap();
        assert!(log.contains("power: the machine is still on; asking again"), "{log}");
    }

    #[test]
    fn a_disk_that_reads_back_wrong_fails_the_boot() {
        let mut bench = bench("amt-tamper", true);
        let err = bench.driver.boot_fresh().unwrap_err();
        assert!(err.contains("did not hold the image"), "{err}");
        assert!(err.contains("writer-1.log"), "{err}");
        // The machine is not left running the writer.
        assert!(!bench.driver.is_on());
    }

    #[test]
    fn the_linux_twin_is_the_machine_net_booted() {
        let mut bench = bench("amt-twin", false);
        bench.dir.write("linux/bzImage", "KERNEL");
        bench.dir.write("linux/initrd", "INITRD");
        bench.dir.write(
            "linux/kit.toml",
            "kernel = \"bzImage\"\ninitrd = \"initrd\"\ncmdline = \"console=ttyS0\"\n\
             done = \"linux-bench: done\"\n",
        );
        let kit = Kit::read(&bench.dir.path().join("linux")).unwrap();
        let log = bench.dir.path().join("logs/linux-boot-1.log");
        let deadline = Instant::now() + Duration::from_secs(10);
        let text = bench.driver.linux_twin(&kit, &log, deadline).unwrap();
        assert!(text.contains("BENCH name=spawn ns=120.0"), "{text}");
        // Linux is not left running: the runner boots afresh after.
        assert!(!bench.driver.is_on());
        assert!(bench.driver.twin_ends_boot());
    }

    #[test]
    fn the_remote_screen_shows_the_screen_and_takes_keys_and_the_mouse() {
        let mut bench = bench("amt-screen", false);
        let err = bench.driver.press_keys(&["a".into()]).unwrap_err();
        assert!(err.contains("the machine is off"), "{err}");
        let soon = Instant::now() + Duration::from_secs(5);
        bench.driver.power(PowerAction::On, soon).unwrap();

        bench.driver.press_keys(&["shift-a".into(), "ctrl-alt-delete".into()]).unwrap();
        bench.driver.point(39, 29).unwrap();
        bench.driver.click(Button::Right).unwrap();
        // The screen as PPM, for reading its text, and as PNG.
        let dump = bench.dir.path().join("screen.ppm");
        bench.driver.screen_dump(&dump).unwrap();
        let image = screen::Image::parse_ppm(&fs::read(&dump).unwrap()).unwrap();
        assert_eq!(image.pixels(), pattern(40, 30));
        let shot = bench.dir.path().join("screen.png");
        bench.driver.screenshot(&shot).unwrap();
        assert!(fs::read(&shot).unwrap().starts_with(b"\x89PNG\r\n\x1a\n"));
        // The screen answers in order, so all that was sent has arrived.
        let (shift, ctrl, alt, delete) = (0xffe1, 0xffe3, 0xffe9, 0xffff);
        assert_eq!(
            bench.screen.events(),
            [
                Event::Key(shift, true),
                Event::Key(0x41, true),
                Event::Key(0x41, false),
                Event::Key(shift, false),
                Event::Key(ctrl, true),
                Event::Key(alt, true),
                Event::Key(delete, true),
                Event::Key(delete, false),
                Event::Key(alt, false),
                Event::Key(ctrl, false),
                Event::Pointer(0, 39, 29),
                Event::Pointer(4, 39, 29),
                Event::Pointer(0, 39, 29),
            ]
        );
        // One session for all of it, and AMT set up once.
        assert_eq!(bench.screen.state.lock().unwrap().sessions, 1);
        let calls = bench.fake.calls();
        assert_eq!(calls.iter().filter(|call| call.starts_with("Put IPS_KVM")).count(), 1);

        // Nothing is sent for a key or pixel that is not there.
        let err = bench.driver.press_keys(&["a".into(), "f13".into()]).unwrap_err();
        assert!(err.contains("no key named \"f13\""), "{err}");
        let err = bench.driver.point(40, 10).unwrap_err();
        assert!(err.contains("off the screen, which is 40x30"), "{err}");
        assert_eq!(bench.screen.events().len(), 13);

        // A session AMT ends is opened again, and the screen's new size
        // is seen.
        bench.screen.close_sessions();
        bench.screen.state.lock().unwrap().resize = Some((64, 48, pattern(64, 48)));
        bench.driver.point(63, 47).unwrap();
        assert_eq!(bench.screen.state.lock().unwrap().sessions, 2);
        bench.driver.screen_dump(&dump).unwrap();
        assert_eq!(bench.screen.events().last(), Some(&Event::Pointer(0, 63, 47)));

        // AMT turns power changes down while a session is open, so the
        // driver closes it first, and the next look opens another.
        bench.driver.power(PowerAction::Cut, soon).unwrap();
        assert!(!bench.driver.is_on());
        assert_eq!(bench.screen.state.lock().unwrap().live, 0);
        bench.driver.power(PowerAction::On, soon).unwrap();
        bench.driver.screen_dump(&dump).unwrap();
        assert_eq!(bench.screen.state.lock().unwrap().sessions, 3);

        let log = fs::read_to_string(bench.dir.path().join("logs/amt.log")).unwrap();
        assert!(log.contains("remote screen: session open, 40x30"), "{log}");
        assert!(log.contains("remote screen: session closed for a power change"), "{log}");
        let password = bench.fake.state.lock().unwrap().screen.password.clone().unwrap();
        assert!(!log.contains(&password), "{log}");
    }

    #[test]
    fn a_remote_screen_amt_will_not_open_says_why() {
        let mut bench = bench("amt-no-screen", false);
        bench.fake.state.lock().unwrap().screen.consent = 4_294_967_295;
        let soon = Instant::now() + Duration::from_secs(5);
        bench.driver.power(PowerAction::On, soon).unwrap();
        let err = bench.driver.screen_dump(&bench.dir.path().join("x.ppm")).unwrap_err();
        assert!(err.contains("User Opt-in to None"), "{err}");
        let checks = check(&bench.driver.settings, None, true);
        let screen = checks.iter().find(|check| check.check == "remote screen").unwrap();
        assert_eq!(screen.verdict, Verdict::Problem);
        // A machine whose screen is no one's is not checked for one.
        let checks = check(&bench.driver.settings, None, false);
        assert!(checks.iter().all(|check| check.check != "remote screen"));
    }

    #[test]
    fn a_password_file_others_can_read_is_refused() {
        let dir = TempDir::new("amt-password");
        let open = dir.write("password", "secret\n");
        fs::set_permissions(&open, fs::Permissions::from_mode(0o644)).unwrap();
        let err = read_password(&open).unwrap_err();
        assert!(err.contains("chmod 600"), "{err}");
        fs::set_permissions(&open, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_password(&open).unwrap(), "secret");
    }

    #[test]
    fn checks_what_a_run_needs() {
        let bench = bench("amt-check", false);
        let checks = check(&bench.driver.settings, None, true);
        let verdict =
            |name: &str| checks.iter().find(|check| check.check == name).map(|check| check.verdict);
        assert_eq!(verdict("password"), Some(Verdict::Ok));
        assert_eq!(verdict("amt"), Some(Verdict::Ok));
        assert_eq!(verdict("control mode"), Some(Verdict::Ok));
        assert_eq!(verdict("power"), Some(Verdict::Ok));
        assert_eq!(verdict("remote screen"), Some(Verdict::Ok));
        assert_eq!(verdict("power button"), Some(Verdict::Note));
        assert_eq!(verdict("serial port"), Some(Verdict::Ok));
        assert_eq!(verdict("lab network"), Some(Verdict::Ok));
        assert_eq!(verdict("tftp"), Some(Verdict::Ok));
        // No network GRUB in this pretend TFTP folder, and no lab key.
        assert_eq!(verdict("network grub"), Some(Verdict::Problem));
        assert_eq!(verdict("writer"), Some(Verdict::Ok));
        assert_eq!(verdict("lab key"), Some(Verdict::Problem));
        let amt = checks.iter().find(|check| check.check == "amt").unwrap();
        assert!(amt.detail.contains("AMT 11.8.50"), "{}", amt.detail);
        // Checking leaves nothing behind in the TFTP folder.
        let boot = bench.driver.settings.tftp.join("dukovo-lab/boot");
        assert_eq!(fs::read_dir(boot).unwrap().count(), 0);
    }
}

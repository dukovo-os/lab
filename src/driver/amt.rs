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
//! - **The serial console** comes from AMT's Serial-over-LAN (see
//!   sol.rs), which AMT shows the system as a serial port of its own, or
//!   over a serial cable into a port of the lab PC's when the machine
//!   file names one (see serial.rs): one log per boot either way. The
//!   driver turns Serial-over-LAN on as it starts, and keeps its session
//!   open across boots: AMT drops what the system writes while no
//!   session is open.
//! - **SSH** and the lab PC's side of a test reach the machine on the lab
//!   network, where it has an address of its own, so the machine's ports
//!   are reached as they are.
//! - **The Linux twin** is the machine itself, net booted into the
//!   Linux kit; the runner boots afresh after it.
//! - **The screen, keyboard and mouse** are AMT's remote screen (KVM)
//!   (see vnc.rs and remote_screen.rs): a VNC server on AMT's port 5900,
//!   with a password made for the run, or, from AMT release 16.1 on,
//!   which has no port 5900, a session of AMT's redirection service. The
//!   driver turns it on the first time a test wants it, and keeps one
//!   session open from then on: AMT plugs its keyboard and mouse into the
//!   machine when a session opens, so a session that stays open keeps
//!   them there across boots.
//!
//! AMT from release 16.1 on (a 12th generation Core's, or an older
//! machine's whose firmware is updated) answers only over TLS: the
//! machine file's `amt.pin`, the pin of AMT's key, turns TLS on for power,
//! the console and the screen alike (see link.rs). With no pin, the driver
//! speaks plain HTTP and redirection sessions, as AMT before 16.1 serves
//! them.
//!
//! Cables, sticks, cards and discs are a person's to handle. What the
//! driver does goes in the bundle's `logs/<machine>/amt.log`, never with
//! the passwords.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::real::{
    self, Check, DriverLog, LabSide, NetBoot, Verdict, WRITE_TIME, found, persons_job, problem,
    ssh_target, with_console,
};
use super::remote_screen::{RemoteScreen, TurnOn};
use super::{Driver, Setup};
use crate::amt::{self, Amt, PowerChange};
use crate::bench::Kit;
use crate::link::Tls;
use crate::machines::AmtSettings;
use crate::netboot::{self, Offer};
use crate::redirection::{self, Sign};
use crate::serial::{ConsoleLog, SerialReader};
use crate::sol::{self, SolReader};
use crate::ssh;
use crate::suite::{Button, PowerAction, Stick};
use crate::vnc::{self, Door};
use crate::wsman;

/// How long the machine is given to go off after a hard off.
const OFF_TIME: Duration = Duration::from_secs(30);

/// How long a hard off is waited for before AMT is asked again: AMT takes
/// one asked while the machine is starting and does nothing with it.
const HARD_OFF_AGAIN: Duration = Duration::from_secs(5);

/// How long AMT may go on saying the machine is off after turning it on.
const ON_TIME: Duration = Duration::from_secs(30);

/// How long after turning the machine on or resetting it the next power
/// change waits: AMT 11.8 on an OptiPlex 7040 asked for a hard off 1.1 s
/// after an on went silent, its engine answering nothing on the network,
/// until the power cord was pulled.
const START_SETTLE: Duration = Duration::from_secs(10);

/// How long a power state read from AMT is trusted: waits ask often.
const POWER_FRESH: Duration = Duration::from_secs(1);

/// How long AMT may take to see a remote screen session close, and take
/// power changes again.
const SCREEN_CLOSE_TIME: Duration = Duration::from_secs(10);

/// What reads the machine's serial console, until the driver is
/// dropped.
enum ConsoleReader {
    Cable(SerialReader),
    Sol(SolReader),
}

impl ConsoleReader {
    /// The log the console goes to.
    fn log(&self) -> &ConsoleLog {
        match self {
            ConsoleReader::Cable(reader) => reader.log(),
            ConsoleReader::Sol(reader) => reader.log(),
        }
    }
}

/// A real machine driven through AMT.
pub struct AmtDriver {
    settings: AmtSettings,
    setup: Setup,
    amt: Arc<Amt>,
    netboot: NetBoot,
    console: ConsoleReader,
    /// The current boot's serial log.
    serial_log: PathBuf,
    boot: u32,
    /// The power state last read, and when.
    power: Option<(Instant, bool)>,
    /// When AMT last turned the machine on or reset it, and how long a
    /// power change after it waits ([`START_SETTLE`]).
    last_start: Option<Instant>,
    start_settle: Duration,
    /// AMT's remote screen.
    screen: RemoteScreen,
}

impl AmtDriver {
    /// Gets ready to drive the machine: reads AMT's password and the
    /// writer's kit, and starts the net-boot server and the console's
    /// reader, turning Serial-over-LAN on if the console is read from it.
    /// Leaves the machine's power as it is.
    pub fn new(settings: AmtSettings, setup: Setup) -> Result<AmtDriver, String> {
        let ports = Ports::of(&settings);
        AmtDriver::start(settings, setup, ports)
    }

    /// As [`AmtDriver::new`], with AMT's redirection service and its port
    /// 5900 at `ports`.
    fn start(settings: AmtSettings, setup: Setup, ports: Ports) -> Result<AmtDriver, String> {
        let password = read_password(&settings.password_file)?;
        let tls = tls(&settings)?;
        fs::create_dir_all(&setup.logs)
            .and_then(|()| fs::create_dir_all(&setup.work))
            .map_err(|err| format!("making the machine's folders: {err}"))?;
        let mut netboot = NetBoot::start(&lab_side(&settings), &setup.logs.join("amt.log"))?;
        let amt = Arc::new(Amt::new(&settings.address, &settings.user, &password, tls.clone()));
        let sign = Sign {
            address: ports.redirection,
            user: settings.user.clone(),
            password,
            tls: tls.clone(),
        };
        // Until the first boot, what the console says goes to a scratch log.
        let before = setup.work.join("serial-before.log");
        let console = match &settings.serial {
            Some(port) => {
                netboot.note(&format!("console: serial port {}", port.display()));
                ConsoleReader::Cable(SerialReader::start(port, settings.baud, &before)?)
            }
            None => {
                amt.turn_on_serial_over_lan().inspect_err(|err| netboot.note(err))?;
                ConsoleReader::Sol(SolReader::start(sign.clone(), &before, netboot.log())?)
            }
        };
        let screen = remote_screen(&amt, sign, ports.vnc, netboot.log());
        let mut driver = AmtDriver {
            settings,
            setup,
            amt,
            netboot,
            console,
            serial_log: before,
            boot: 0,
            power: None,
            last_start: None,
            start_settle: START_SETTLE,
            screen,
        };
        driver.note(&format!(
            "AMT at {}{}; net-boot server at {}, ports {}",
            driver.settings.address,
            if driver.settings.pin.is_some() { " over TLS" } else { "" },
            driver.settings.lab_address,
            driver.netboot.ports()
        ));
        Ok(driver)
    }

    /// Changes the power through AMT, and notes it. AMT turns power
    /// changes down while a remote screen session is open (AMT 11.8 on an
    /// OptiPlex 7040 answers ReturnValue 2), so the session is closed
    /// first, and the change tried again while AMT sees it close; the next
    /// look at the screen opens another. A Serial-over-LAN session stays
    /// open, so the console from the power on is read, unless AMT turns
    /// the change down: then it is closed until the change is made.
    fn change(&mut self, change: PowerChange) -> Result<(), String> {
        let screen_was_open = self.screen.close();
        if screen_was_open {
            self.note("remote screen: session closed for a power change");
        }
        self.power = None;
        if let Some(started) = self.last_start
            && change != PowerChange::On
            && started.elapsed() < self.start_settle
        {
            let rest = self.start_settle - started.elapsed();
            self.note(&format!(
                "power: waiting {:.1} s after the machine started before a power change",
                rest.as_secs_f64()
            ));
            std::thread::sleep(rest);
        }
        self.note(&format!("power: {change:?}"));
        let deadline = Instant::now() + SCREEN_CLOSE_TIME;
        let mut console_held = None;
        let changed = loop {
            let changed = self.amt.change_power(change);
            let Err(err) = &changed else { break changed };
            if Instant::now() >= deadline {
                break changed;
            }
            if console_held.is_none()
                && let ConsoleReader::Sol(reader) = &self.console
            {
                console_held = Some(reader.hold());
                self.note(&format!(
                    "power: {err}; asking again with the Serial-over-LAN session closed"
                ));
            } else if !screen_was_open && console_held.is_none() {
                break changed;
            }
            std::thread::sleep(Duration::from_millis(500));
        };
        drop(console_held);
        match &changed {
            Ok(()) if matches!(change, PowerChange::On | PowerChange::Reset) => {
                self.last_start = Some(Instant::now());
            }
            Ok(()) => {}
            Err(err) => self.note(&format!("power: {err}")),
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
        self.netboot.begin(what, offer, cmdline)?;
        self.amt.boot_from_network_once()?;
        self.turn_on()
    }

    /// Writes the image to the disk through the writer; returns once the
    /// writer said it wrote it and the hash was right.
    fn write_image(&mut self) -> Result<(), String> {
        let image = self.netboot.image(&self.setup.image)?;
        let writer_log = self.setup.logs.join(format!("writer-{}.log", self.boot));
        self.console.log().switch_to(&writer_log)?;
        let written = {
            let (console, next, script) = (
                self.console.log().clone(),
                self.serial_log.clone(),
                self.netboot.script().to_path_buf(),
            );
            // Before the writer is told to restart: its next boot must not
            // find the script, and its console goes to the boot's log.
            Box::new(move || {
                netboot::remove_script(&script);
                let _ = console.switch_to(&next);
            })
        };
        let offer = Offer::Writer {
            kit: self.netboot.writer.clone(),
            image,
            disk: self.settings.disk.clone(),
            written,
        };
        // The writer sets its own address: the card with the machine
        // file's MAC gets the machine's address on the lab's /24.
        let cmdline = format!(
            "{} dukovo.lab={{base}} dukovo.lab.ip={}/24 dukovo.lab.mac={}",
            with_console(&self.netboot.writer.cmdline, self.settings.console()),
            self.settings.machine_address,
            self.settings.mac.to_ascii_lowercase()
        );
        let booted = self.net_boot("the writer", offer, &cmdline);
        let outcome = booted.and_then(|()| self.netboot.await_writer(&self.setup.stop, WRITE_TIME));
        self.netboot.finish();
        match outcome {
            Ok(()) => {
                self.note("net boot: the disk holds the image; the machine restarts into it");
                Ok(())
            }
            Err(why) => {
                // A writer that failed must not be left running.
                let _ = self.hard_off();
                let _ = self.console.log().switch_to(&self.serial_log);
                self.note(&format!("net boot: {why}"));
                Err(format!(
                    "{why}; the writer's console is in {}, and what the driver did in amt.log",
                    writer_log.display()
                ))
            }
        }
    }
}

/// AMT's remote screen, which is turned on the first time a test wants
/// it: through its redirection service when `sign` has TLS, as AMT from
/// release 16.1 on has no port 5900, else on port 5900 at `vnc`, with a
/// password made for the run.
fn remote_screen(amt: &Arc<Amt>, sign: Sign, vnc: String, notes: DriverLog) -> RemoteScreen {
    let amt = Arc::clone(amt);
    if sign.tls.is_some() {
        let door = Door::Amt(sign);
        let turn_on: TurnOn = Box::new(move || {
            amt.turn_on_remote_screen(None)?;
            Ok(door.clone())
        });
        return RemoteScreen::new("KVM, through AMT's redirection port", true, notes, turn_on);
    }
    let turn_on: TurnOn = Box::new(move || {
        let password = amt::screen_password()?;
        amt.turn_on_remote_screen(Some(&password))?;
        Ok(Door::Vnc { address: vnc.clone(), password })
    });
    RemoteScreen::new("port 5900, with a password made for this run", true, notes, turn_on)
}

/// Where AMT's redirection service and its port 5900 are: on AMT's host,
/// on the ports for TLS when the machine file pins AMT's key.
struct Ports {
    redirection: String,
    vnc: String,
}

impl Ports {
    fn of(settings: &AmtSettings) -> Ports {
        let host = amt_host(settings);
        let port = if settings.pin.is_some() { redirection::TLS_PORT } else { redirection::PORT };
        Ports { redirection: format!("{host}:{port}"), vnc: format!("{host}:{}", vnc::PORT) }
    }
}

/// The TLS AMT is reached with: its key's, when the machine file pins it.
fn tls(settings: &AmtSettings) -> Result<Option<Tls>, String> {
    settings.pin.as_deref().map(Tls::new).transpose()
}

impl AmtDriver {
    /// The remote screen, once the machine is on: a machine that is off
    /// has no screen.
    fn screen_on(&mut self) -> Result<&mut RemoteScreen, String> {
        if !self.is_on() {
            return Err("the machine is off, so it has no screen".into());
        }
        Ok(&mut self.screen)
    }
}

/// Checks what a run on the machine needs, without changing anything:
/// AMT's password, address and mode, its remote screen when the driver
/// does the machine's `screen`, the console (Serial-over-LAN, or the
/// serial port), the lab network, the TFTP folder and network GRUB, the
/// writer's kit and the lab key.
pub fn check(settings: &AmtSettings, key: Option<&Path>, screen: bool) -> Vec<Check> {
    check_with(settings, key, screen, &Ports::of(settings).redirection)
}

/// As [`check`], with AMT's Serial-over-LAN at `console_address`.
fn check_with(
    settings: &AmtSettings,
    key: Option<&Path>,
    screen: bool,
    console_address: &str,
) -> Vec<Check> {
    let mut checks = Vec::new();
    let mut add = |check: &'static str, result: Result<String, (Verdict, String)>| {
        checks.push(found(check, result));
    };
    let password = read_password(&settings.password_file);
    add(
        "password",
        password
            .as_ref()
            .map(|_| "AMT's password file is readable by this user alone".into())
            .map_err(|err| problem(err.clone())),
    );
    let tls = tls(settings);
    if let Err(err) = &tls {
        add("amt", Err(problem(err.clone())));
    }
    if let (Ok(password), Ok(tls)) = (&password, &tls) {
        let amt = Amt::new(&settings.address, &settings.user, password, tls.clone());
        match amt.version() {
            Err(err) => {
                let tls_address = format!("{}:{}", amt_host(settings), wsman::TLS_PORT);
                add("amt", Err(problem(with_pin(settings, err, &tls_address))));
            }
            Ok(version) => {
                let over = if tls.is_some() { " over TLS" } else { "" };
                add("amt", Ok(format!("AMT {version} at {}{over}", settings.address)));
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
                            Ok(_) if tls.is_some() => Ok("KVM is on in MEBx and asks for no \
                                consent; the driver opens it through AMT's redirection port when \
                                a test wants the screen"
                                .into()),
                            Ok(_) => Ok("KVM is on in MEBx and asks for no consent; the driver \
                                         turns on port 5900 when a test wants the screen"
                                .into()),
                            Err(err) => Err(problem(err)),
                        },
                    );
                }
                if settings.serial.is_none() {
                    let sign = Sign {
                        address: console_address.to_string(),
                        user: settings.user.clone(),
                        password: password.clone(),
                        tls: tls.clone(),
                    };
                    add("serial over lan", check_serial_over_lan(&amt, &sign));
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
    if let Some(port) = &settings.serial {
        add(
            "serial port",
            crate::serial::check_port(port, settings.baud)
                .map(|()| format!("{} opens, at {} baud", port.display(), settings.baud))
                .map_err(problem),
        );
    }
    checks.extend(real::check_lab_side(&lab_side(settings), key));
    checks
}

/// The message of a failed call to AMT, with the pin of the key AMT
/// offers over TLS at `tls_address` when the machine file names none and
/// AMT answers there: AMT from release 16.1 on serves nothing in the
/// clear.
fn with_pin(settings: &AmtSettings, err: String, tls_address: &str) -> String {
    if settings.pin.is_some() {
        return err;
    }
    match crate::redfish::pin_of(tls_address) {
        Ok(pin) => format!(
            "{err}; AMT answers over TLS at {tls_address}, with a key whose pin is {pin}: if \
             this is the machine's AMT, put pin = \"{pin}\" in the machine file's [amt] section"
        ),
        Err(_) => err,
    }
}

/// Whether the console can be read from AMT's Serial-over-LAN: a session
/// opens, and closes, when it is on; when it is off, the driver turns it
/// on as a run starts, which a check leaves undone.
fn check_serial_over_lan(amt: &Amt, sign: &Sign) -> Result<String, (Verdict, String)> {
    let found = amt.serial_over_lan().map_err(problem)?;
    if !found.on || !found.listener {
        return Ok("off for now: the driver turns it on as a run starts (if AMT will not, turn \
                   SOL on in MEBx)"
            .into());
    }
    sol::try_session(sign).map(|()| format!("a session opens at {}", sign.address)).map_err(problem)
}

/// The URL of AMT's web server: over TLS, on its port for TLS, when the
/// machine file pins AMT's key.
fn web_url(settings: &AmtSettings) -> String {
    let host = amt_host(settings);
    match settings.pin {
        Some(_) => format!("https://{host}:{}/", wsman::TLS_PORT),
        None => format!("http://{host}:{}/", wsman::PORT),
    }
}

/// AMT's host, without the port.
fn amt_host(settings: &AmtSettings) -> &str {
    settings.address.split(':').next().unwrap_or_default()
}

/// What the machine file says about the lab network.
fn lab_side(settings: &AmtSettings) -> LabSide<'_> {
    LabSide {
        lab_address: &settings.lab_address,
        boot_port: settings.boot_port,
        tftp: &settings.tftp,
        mac: &settings.mac,
        writer: &settings.writer,
    }
}

/// Reads AMT's password from its file, which only this user may read.
fn read_password(path: &Path) -> Result<String, String> {
    real::read_password(path, "AMT", "amt.password_file")
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

    fn grows_root(&self) -> bool {
        self.settings.grow_root
    }

    fn sets_clock(&self) -> bool {
        true
    }

    fn checks_health(&self) -> bool {
        true
    }

    /// Writes a line to amt.log, with the seconds since the driver began.
    fn note(&mut self, line: &str) {
        self.netboot.note(line);
    }

    fn serial_log(&self) -> &Path {
        &self.serial_log
    }

    fn set_link(&mut self, _up: bool) -> Result<(), String> {
        Err(persons_job("AMT", "plug or pull the network cable"))
    }

    fn plug_usb(&mut self, _stick: &Stick) -> Result<(), String> {
        Err(persons_job("AMT", "plug in a USB stick"))
    }

    fn unplug_usb(&mut self) -> Result<(), String> {
        Err(persons_job("AMT", "pull out a USB stick"))
    }

    fn press_keys(&mut self, combos: &[String]) -> Result<(), String> {
        self.screen_on()?.press_keys(combos)
    }

    fn point(&mut self, x: i32, y: i32) -> Result<(), String> {
        self.screen_on()?.point(x, y)
    }

    fn click(&mut self, button: Button) -> Result<(), String> {
        self.screen_on()?.click(button)
    }

    fn screenshot(&mut self, to: &Path) -> Result<(), String> {
        self.screen_on()?.screenshot(to)
    }

    fn screen_dump(&mut self, to: &Path) -> Result<(), String> {
        self.screen_on()?.screen_dump(to)
    }

    fn run_command(
        &mut self,
        command: &str,
        input: Option<&str>,
        deadline: Instant,
        stall: Option<&ssh::Stall>,
    ) -> Result<ssh::Output, String> {
        let target = ssh_target(&self.setup, &self.settings.machine_address, self.boot)?;
        if !self.is_on() {
            return Err("the machine is off".into());
        }
        target.run(command, input, deadline, &self.setup.stop, stall)
    }

    fn peer_address(&self) -> Option<String> {
        Some(self.settings.lab_address.clone())
    }

    fn machine_address(&self) -> Option<String> {
        Some(self.settings.machine_address.clone())
    }

    fn amt_address(&self) -> Option<String> {
        Some(self.settings.address.clone())
    }

    fn amt_web(&self) -> Option<String> {
        Some(web_url(&self.settings))
    }

    fn reach(&mut self, port: u16, _udp: bool) -> Result<u16, String> {
        // The lab PC is on the machine's network: its ports are its own.
        Ok(port)
    }

    fn loopback_forward(&mut self, local_port: u16, port: u16) -> Result<Option<String>, String> {
        let target = ssh_target(&self.setup, &self.settings.machine_address, self.boot)?;
        Ok(Some(target.forward_command(local_port, port)))
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
        self.console.log().switch_to(log)?;
        let cmdline = with_console(&kit.cmdline, self.settings.console());
        let booted = self.net_boot("the Linux twin", Offer::Twin { kit: kit.clone() }, &cmdline);
        let measured = booted.and_then(|()| real::await_twin(kit, log, deadline, &self.setup.stop));
        self.netboot.finish();
        let _ = self.hard_off();
        let _ = self.console.log().switch_to(&self.setup.work.join("serial-after-twin.log"));
        self.note(&format!("Linux twin: {}", if measured.is_ok() { "measured" } else { "failed" }));
        measured
    }

    fn emulated(&self) -> bool {
        false
    }

    fn shut_off(&mut self) {
        self.screen.close();
        self.netboot.finish();
        if let Err(err) = self.hard_off() {
            self.note(&format!("shut off: {err}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amt::fake::{FakeAmt, PASSWORD, USER};
    use crate::link::fake::FakeTls;
    use crate::netboot::tests::{make_image, pretend_writer};
    use crate::redirection::fake::FakeRedirection;
    use crate::screen;
    use crate::stop::Stop;
    use crate::testing::TempDir;
    use crate::vnc::fake::{Event, FakeVnc, pattern};
    use std::fs::OpenOptions;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    const MAC: &str = "aa:bb:cc:00:11:22";

    /// A machine behind a pretend AMT: a net boot runs the pretend writer
    /// (or prints what a Linux twin would), and the disk boots to its
    /// ready line, on AMT's pretend Serial-over-LAN, or with `cable` on a
    /// plain file standing in for the serial port.
    struct Bench {
        dir: TempDir,
        fake: FakeAmt,
        /// AMT's Serial-over-LAN, when the console is read from it.
        sol: Option<Arc<FakeRedirection>>,
        /// AMT's remote screen, open once the pretend AMT turns it on.
        screen: FakeVnc,
        /// The TLS in front of AMT's ports, when the bench has it.
        fronts: Vec<FakeTls>,
        driver: AmtDriver,
    }

    fn bench(label: &str, tamper: bool) -> Bench {
        bench_with(label, tamper, false, false)
    }

    /// As [`bench`], with AMT as release 16.1 serves it: WS-Management and
    /// the redirection service over TLS alone, with one key, and the
    /// remote screen through the redirection service.
    fn bench_tls(label: &str) -> Bench {
        bench_with(label, false, false, true)
    }

    fn bench_with(label: &str, tamper: bool, cable: bool, tls: bool) -> Bench {
        let dir = TempDir::new(label);
        let port = dir.write("ttyUSB0", "");
        let sol = (!cable).then(|| Arc::new(FakeRedirection::start()));
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
            let (tftp, port, disk, sol) = (tftp.clone(), port.clone(), disk.clone(), sol.clone());
            fake.state.lock().unwrap().on_boot = Some(Box::new(move |network| {
                let (tftp, port, disk, sol) =
                    (tftp.clone(), port.clone(), disk.clone(), sol.clone());
                // As the machine would: the firmware, then what it boots.
                std::thread::spawn(move || {
                    let say_bytes = |bytes: &[u8]| match &sol {
                        Some(sol) => sol.say(bytes),
                        None => {
                            let mut port = OpenOptions::new().append(true).open(&port).unwrap();
                            port.write_all(bytes).unwrap();
                        }
                    };
                    let say = |text: &str| say_bytes(text.as_bytes());
                    let script = fs::read_to_string(netboot::script_path(&tftp, MAC));
                    let Some(script) = script.ok().filter(|_| network) else {
                        say("Boot complete.\n");
                        return;
                    };
                    let Some(base) =
                        script.split_whitespace().find_map(|word| word.strip_prefix("dukovo.lab="))
                    else {
                        // A real serial line often opens with a byte of
                        // noise from the power on, which is not UTF-8.
                        say_bytes(b"\xff\r\n");
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
        let mut fronts = Vec::new();
        if tls {
            let wsman = FakeTls::start(fake.address);
            let redirection = wsman.beside(sol.as_ref().unwrap().address);
            fronts = vec![wsman, redirection];
        }
        let settings = AmtSettings {
            address: fronts.first().map_or(fake.address, |front| front.address).to_string(),
            user: USER.into(),
            password_file: password,
            mac: MAC.into(),
            disk: "TESTDISK".into(),
            serial: cable.then_some(port),
            baud: 115_200,
            machine_address: "127.0.0.1".into(),
            lab_address: "127.0.0.1".into(),
            boot_port: 0,
            tftp,
            writer: dir.path().join("writer"),
            console: None,
            grow_root: true,
            pin: fronts.first().map(|front| front.pin.clone()),
        };
        let setup = Setup {
            image: make_image(&dir, 400_000),
            logs: dir.path().join("logs"),
            work: dir.path().join("work"),
            sockets: dir.path().join("sockets"),
            key: None,
            redteam: None,
            ob: None,
            stop: Stop::default(),
        };
        let screen = if tls {
            // Behind the redirection service, which has signed the session
            // in: RFB asks for no password of its own.
            let screen = FakeVnc::start(Box::new(|| Some(String::new())));
            screen.state.lock().unwrap().asks_password = false;
            sol.as_ref().unwrap().state.lock().unwrap().screen = Some(screen.address);
            screen
        } else {
            let state = Arc::clone(&fake.state);
            FakeVnc::start(Box::new(move || state.lock().unwrap().screen.open_password()))
        };
        let live = Arc::clone(&screen.state);
        fake.state.lock().unwrap().screen_busy =
            Some(Box::new(move || live.lock().unwrap().live > 0));
        let redirection = match (fronts.get(1), &sol) {
            (Some(front), _) => front.address.to_string(),
            (None, Some(sol)) => sol.address.to_string(),
            (None, None) => "unused".into(),
        };
        let ports = Ports { redirection, vnc: screen.address.to_string() };
        let mut driver = AmtDriver::start(settings, setup, ports).unwrap();
        // Tests of the wait after a start set their own.
        driver.start_settle = Duration::ZERO;
        Bench { dir, fake, sol, screen, fronts, driver }
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
        assert!(bench.driver.grows_root());
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
        // Serial-over-LAN was turned on as the driver started, and its
        // one session carried the writer's console and the disk's.
        assert_eq!(
            bench.fake.calls(),
            [
                "Put AMT_RedirectionService",
                "RequestStateChange 32771",
                "ChangeBootOrder",
                "SetBootConfigRole",
                "RequestPowerStateChange 2"
            ]
        );
        assert_eq!(bench.sol.as_ref().unwrap().state.lock().unwrap().sessions, 1);

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
        assert!(log.contains("console: Serial-over-LAN session open at"), "{log}");
        assert!(!log.contains(PASSWORD));
        assert_eq!(bench.driver.peer_address().as_deref(), Some("127.0.0.1"));
        assert_eq!(bench.driver.reach(445, false), Ok(445));
    }

    #[test]
    fn a_serial_cable_is_read_when_the_machine_file_names_one() {
        let mut bench = bench_with("amt-cable", false, true, false);
        bench.driver.boot_fresh().unwrap();
        assert!(wait_for(bench.driver.serial_log(), "Boot complete."));
        let writer_log = bench.dir.path().join("logs/writer-1.log");
        assert!(fs::read_to_string(writer_log).unwrap().contains("writer: writing"));
        // COM1's name for Linux, and Serial-over-LAN left as it was.
        let script = fs::read_to_string(bench.dir.path().join("writer-script")).unwrap();
        assert!(script.contains("console=ttyS0,115200"), "{script}");
        assert!(bench.fake.calls().iter().all(|call| !call.contains("Redirection")));
        let log = fs::read_to_string(bench.dir.path().join("logs/amt.log")).unwrap();
        assert!(log.contains("console: serial port "), "{log}");
        let checks = check(&bench.driver.settings, None, false);
        let verdict =
            |name: &str| checks.iter().find(|check| check.check == name).map(|check| check.verdict);
        assert_eq!(verdict("serial port"), Some(Verdict::Ok));
        assert_eq!(verdict("serial over lan"), None);
    }

    #[test]
    fn a_power_change_amt_turns_down_is_made_with_the_console_closed() {
        let mut bench = bench("amt-sol-busy", false);
        let sol = Arc::clone(bench.sol.as_ref().unwrap());
        // This AMT turns power changes down while Serial-over-LAN is open.
        let live = Arc::clone(&sol.state);
        bench.fake.state.lock().unwrap().screen_busy =
            Some(Box::new(move || live.lock().unwrap().live > 0));
        let soon = Instant::now() + Duration::from_secs(10);
        bench.driver.power(PowerAction::On, soon).unwrap();
        assert!(bench.driver.is_on());
        // The session opens again once the change is made.
        assert!(sol.wait(|state| state.sessions == 2 && state.live == 1));
        let log = fs::read_to_string(bench.dir.path().join("logs/amt.log")).unwrap();
        assert!(
            log.contains("returned 2; asking again with the Serial-over-LAN session closed"),
            "{log}"
        );
        assert!(log.contains("closed while the driver holds it"), "{log}");
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
    fn a_power_change_waits_a_while_after_the_machine_starts() {
        let mut bench = bench("amt-start-settle", false);
        bench.driver.start_settle = Duration::from_millis(800);
        let soon = Instant::now() + Duration::from_secs(10);
        bench.driver.power(PowerAction::On, soon).unwrap();
        let on = Instant::now();
        bench.driver.power(PowerAction::Cut, soon).unwrap();
        assert!(on.elapsed() >= Duration::from_millis(700), "{:?}", on.elapsed());
        // A reset starts the machine too; an on after an off does not wait.
        bench.driver.power(PowerAction::On, soon).unwrap();
        std::thread::sleep(Duration::from_millis(800));
        bench.driver.power(PowerAction::Reset, soon).unwrap();
        let reset = Instant::now();
        bench.driver.power(PowerAction::Cut, soon).unwrap();
        assert!(reset.elapsed() >= Duration::from_millis(700), "{:?}", reset.elapsed());
        let off = Instant::now();
        bench.driver.power(PowerAction::On, soon).unwrap();
        assert!(off.elapsed() < Duration::from_millis(700), "{:?}", off.elapsed());
        let log = fs::read_to_string(bench.dir.path().join("logs/amt.log")).unwrap();
        let waits = log.matches("after the machine started before a power change").count();
        assert_eq!(waits, 2, "{log}");
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
        // The screen answers in order, so all that was sent has arrived:
        // the session's first key a Shift tap, then the keys asked for.
        let (shift, ctrl, alt, delete) = (0xffe1, 0xffe3, 0xffe9, 0xffff);
        assert_eq!(
            bench.screen.events(),
            [
                Event::Key(shift, true),
                Event::Key(shift, false),
                Event::Key(shift, true),
                Event::Key(0x61, true),
                Event::Key(0x61, false),
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
        assert_eq!(bench.screen.events().len(), 15);

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
    fn drives_amt_over_tls_alone() {
        let mut bench = bench_tls("amt-tls");
        let host = amt_host(&bench.driver.settings).to_string();
        assert_eq!(bench.driver.amt_web(), Some(format!("https://{host}:16993/")));
        bench.driver.boot_fresh().unwrap();
        assert!(wait_for(bench.driver.serial_log(), "Boot complete."));
        let disk = fs::read(bench.dir.path().join("disk")).unwrap();
        assert_eq!(disk, fs::read(&bench.driver.setup.image).unwrap());
        let soon = Instant::now() + Duration::from_secs(5);
        bench.driver.power(PowerAction::Cut, soon).unwrap();
        assert!(!bench.driver.is_on());
        bench.driver.power(PowerAction::On, soon).unwrap();
        // The screen through the redirection service: no port 5900, and
        // no VNC password.
        bench.driver.press_keys(&["a".into()]).unwrap();
        let dump = bench.dir.path().join("screen.ppm");
        bench.driver.screen_dump(&dump).unwrap();
        let image = screen::Image::parse_ppm(&fs::read(&dump).unwrap()).unwrap();
        assert_eq!(image.pixels(), pattern(40, 30));
        assert_eq!(bench.screen.events()[2..], [Event::Key(0x61, true), Event::Key(0x61, false)]);
        let calls = bench.fake.calls();
        assert!(calls.contains(&"RequestStateChange 2".into()), "{calls:?}");
        assert!(!calls.iter().any(|call| call.contains("IPS_KVMRedirectionSettingData")));
        assert!(!bench.fake.state.lock().unwrap().screen.port_5900);
        let sol = bench.sol.as_ref().unwrap();
        assert_eq!(sol.state.lock().unwrap().screen_sessions, 1);
        // A power change closes the screen's session, which AMT holds
        // against it, and the next look opens another.
        bench.driver.power(PowerAction::Reset, soon).unwrap();
        bench.driver.screen_dump(&dump).unwrap();
        assert_eq!(sol.state.lock().unwrap().screen_sessions, 2);
        let log = fs::read_to_string(bench.dir.path().join("logs/amt.log")).unwrap();
        assert!(log.contains("over TLS; net-boot server"), "{log}");
        assert!(log.contains("turning on KVM, through AMT's redirection port"), "{log}");
        assert!(!log.contains(PASSWORD));
    }

    #[test]
    fn checks_amt_over_tls_and_names_the_pin_a_file_lacks() {
        let bench = bench_tls("amt-tls-check");
        let held = match &bench.driver.console {
            ConsoleReader::Sol(reader) => reader.hold(),
            ConsoleReader::Cable(_) => panic!("the bench reads Serial-over-LAN"),
        };
        let console = &bench.fronts[1];
        assert!(bench.sol.as_ref().unwrap().wait(|state| state.live == 0));
        let checks = check_with(&bench.driver.settings, None, true, &console.address.to_string());
        drop(held);
        let detail = |name: &str| {
            let found = checks.iter().find(|check| check.check == name).unwrap();
            assert_eq!(found.verdict, Verdict::Ok, "{name}: {}", found.detail);
            found.detail.clone()
        };
        assert!(detail("amt").ends_with("over TLS"), "{}", detail("amt"));
        assert!(
            detail("remote screen").contains("redirection port"),
            "{}",
            detail("remote screen")
        );
        assert!(detail("serial over lan").starts_with("a session opens at"));
        // A file with no pin, for an AMT that answers only over TLS, is
        // given the pin to check and put in.
        let mut settings = bench.driver.settings.clone();
        settings.pin = None;
        let front = &bench.fronts[0];
        let said = with_pin(&settings, "refused".into(), &front.address.to_string());
        assert!(said.starts_with("refused; AMT answers over TLS at"), "{said}");
        assert!(said.contains(&format!("pin = \"{}\"", front.pin)), "{said}");
        // A wrong pin is refused, and the pin offered named.
        settings.pin = Some(format!("sha256//{}=", "A".repeat(43)));
        let checks = check_with(&settings, None, false, "unused");
        let amt = checks.iter().find(|check| check.check == "amt").unwrap();
        assert_eq!(amt.verdict, Verdict::Problem);
        assert!(amt.detail.contains("not the one the machine file pins"), "{}", amt.detail);
        assert!(amt.detail.contains(&front.pin), "{}", amt.detail);
    }

    #[test]
    fn checks_what_a_run_needs() {
        let bench = bench("amt-check", false);
        let console = bench.sol.as_ref().unwrap();
        // A check opens a session of its own, which needs the driver's
        // closed: one at a time.
        let held = match &bench.driver.console {
            ConsoleReader::Sol(reader) => reader.hold(),
            ConsoleReader::Cable(_) => panic!("the bench reads Serial-over-LAN"),
        };
        assert!(console.wait(|state| state.live == 0));
        let checks = check_with(&bench.driver.settings, None, true, &console.address.to_string());
        drop(held);
        let verdict =
            |name: &str| checks.iter().find(|check| check.check == name).map(|check| check.verdict);
        assert_eq!(verdict("password"), Some(Verdict::Ok));
        assert_eq!(verdict("amt"), Some(Verdict::Ok));
        assert_eq!(verdict("control mode"), Some(Verdict::Ok));
        assert_eq!(verdict("power"), Some(Verdict::Ok));
        assert_eq!(verdict("remote screen"), Some(Verdict::Ok));
        assert_eq!(verdict("power button"), Some(Verdict::Note));
        assert_eq!(verdict("serial over lan"), Some(Verdict::Ok));
        assert_eq!(verdict("serial port"), None);
        assert_eq!(verdict("lab network"), Some(Verdict::Ok));
        assert_eq!(verdict("tftp"), Some(Verdict::Ok));
        // No network GRUB in this pretend TFTP folder, and no lab key.
        assert_eq!(verdict("network grub"), Some(Verdict::Problem));
        assert_eq!(verdict("writer"), Some(Verdict::Ok));
        assert_eq!(verdict("lab key"), Some(Verdict::Problem));
        let amt = checks.iter().find(|check| check.check == "amt").unwrap();
        assert!(amt.detail.contains("AMT 11.8.50"), "{}", amt.detail);
        let found = checks.iter().find(|check| check.check == "serial over lan").unwrap();
        assert!(found.detail.starts_with("a session opens at 127.0.0.1:"), "{}", found.detail);
        // Off, it is the driver's to turn on, and the check leaves it off.
        bench.fake.state.lock().unwrap().redirection = 32768;
        let checks = check_with(&bench.driver.settings, None, false, "unused");
        let found = checks.iter().find(|check| check.check == "serial over lan").unwrap();
        assert_eq!(found.verdict, Verdict::Ok);
        assert!(found.detail.starts_with("off for now"), "{}", found.detail);
        assert_eq!(bench.fake.state.lock().unwrap().redirection, 32768);
        // Checking leaves nothing behind in the TFTP folder.
        let boot = bench.driver.settings.tftp.join("dukovo-lab/boot");
        assert_eq!(fs::read_dir(boot).unwrap().count(), 0);
    }
}

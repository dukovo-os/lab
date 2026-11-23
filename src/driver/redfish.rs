// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The Redfish driver: a server with a BMC (a Dell iDRAC9, as a rule) on
//! the lab network.
//!
//! - **Power** is Redfish's `ComputerSystem.Reset`: `On`, `ForceOff` for
//!   a power cut (the system gets no warning, as when the cord is pulled)
//!   and the hard off before a fresh boot, `GracefulShutdown` for the
//!   power button and `off`, and `ForceRestart` for a reset (see
//!   redfish.rs). The power state is the system's `PowerState`.
//! - **A fresh boot** writes the image under test to the machine's disk
//!   first: the BMC sets the next boot to the network once, the writer
//!   the net-boot server hands it writes the image and checks it, and
//!   the machine restarts into its disk (see netboot.rs). Only the disk
//!   with the machine file's serial number is ever written. A two-socket
//!   server spends minutes in its firmware at each start, so the writer
//!   is given the machine file's `write_minutes`.
//! - **The serial console** comes from IPMI Serial-over-LAN (see
//!   ipmi_sol.rs), or over a serial cable into a port of the lab PC's
//!   (see serial.rs) when the machine file names one: one log per boot
//!   either way. The session is kept open across boots.
//! - **SSH** and the lab PC's side of a test reach the machine on the lab
//!   network, where it has an address of its own.
//! - **The Linux twin** is the machine itself, net booted into the Linux
//!   kit; the runner boots afresh after it.
//! - **The screen, keyboard and mouse** are a Dell iDRAC's VNC server,
//!   which the driver turns on through Redfish the first time a test
//!   wants it, with a password made for the run (see remote_screen.rs).
//!   Another BMC has none the lab can use, and its machine file sets
//!   those controls to `none`.
//! - **The BMC's event logs** are read when the driver starts and again
//!   at `shut_off`; the entries made in between go to
//!   `logs/<machine>/bmc-events.log`, one line each, for tests that
//!   compare what the system did with what the BMC saw.
//!
//! Cables, sticks, cards and discs are a person's to handle. What the
//! driver does goes in the bundle's `logs/<machine>/redfish.log`, never
//! with a password.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::real::{
    self, Check, LabSide, NetBoot, Verdict, found, persons_job, problem, ssh_target, with_console,
};
use super::remote_screen::{Enabled, RemoteScreen, TurnOn};
use super::{Driver, Setup};
use crate::bench::Kit;
use crate::ipmi_sol::{self, IpmiSolReader};
use crate::machines::RedfishSettings;
use crate::netboot::{self, Offer};
use crate::redfish::{self, Event, Redfish, ResetKind, Vendor};
use crate::serial::{ConsoleLog, SerialReader};
use crate::ssh;
use crate::suite::{Button, PowerAction, Stick};

/// How long the machine is given to go off after a hard off.
const OFF_TIME: Duration = Duration::from_secs(30);

/// How long a hard off is waited for before the BMC is asked again.
const HARD_OFF_AGAIN: Duration = Duration::from_secs(5);

/// How long the BMC may go on saying the machine is off after turning it
/// on.
const ON_TIME: Duration = Duration::from_secs(30);

/// How long after turning the machine on or resetting it the next power
/// change waits. An iDRAC takes a power change at any time, so the wait is
/// none; a BMC that goes silent after a start sets it.
const START_SETTLE: Duration = Duration::ZERO;

/// How long a power state read from the BMC is trusted: waits ask often.
const POWER_FRESH: Duration = Duration::from_secs(1);

/// How long `ipmitool sol info` may take in a check.
const SOL_INFO_TIME: Duration = Duration::from_secs(20);

/// What reads the machine's serial console, until the driver is
/// dropped.
enum ConsoleReader {
    Cable(SerialReader),
    IpmiSol(IpmiSolReader),
}

impl ConsoleReader {
    /// The log the console goes to.
    fn log(&self) -> &ConsoleLog {
        match self {
            ConsoleReader::Cable(reader) => reader.log(),
            ConsoleReader::IpmiSol(reader) => reader.log(),
        }
    }
}

/// A real machine driven through its BMC's Redfish.
pub struct RedfishDriver {
    settings: RedfishSettings,
    setup: Setup,
    redfish: Arc<Redfish>,
    netboot: NetBoot,
    console: ConsoleReader,
    /// The current boot's serial log.
    serial_log: PathBuf,
    boot: u32,
    /// The power state last read, and when.
    power: Option<(Instant, bool)>,
    /// When the BMC last turned the machine on or reset it, and how long a
    /// power change after it waits ([`START_SETTLE`]).
    last_start: Option<Instant>,
    start_settle: Duration,
    /// How long a fresh boot may take to load the writer and write the
    /// image.
    write_time: Duration,
    /// The iDRAC's VNC screen.
    screen: RemoteScreen,
    /// The event log entries there were when the driver started; none when
    /// they could not be read.
    events_before: Option<BTreeSet<(String, String)>>,
}

impl RedfishDriver {
    /// Gets ready to drive the machine: reads the BMC's password and the
    /// writer's kit, reads the BMC's event logs, and starts the net-boot
    /// server and the console's reader. Leaves the machine's power as it
    /// is.
    pub fn new(settings: RedfishSettings, setup: Setup) -> Result<RedfishDriver, String> {
        let password = read_password(&settings.password_file)?;
        fs::create_dir_all(&setup.logs)
            .and_then(|()| fs::create_dir_all(&setup.work))
            .map_err(|err| format!("making the machine's folders: {err}"))?;
        let mut netboot = NetBoot::start(&lab_side(&settings), &setup.logs.join("redfish.log"))?;
        let redfish = Arc::new(Redfish::new(
            &base_url(&settings),
            &settings.user,
            &password,
            settings.pin.as_deref(),
        )?);
        // Until the first boot, what the console says goes to a scratch log.
        let before = setup.work.join("serial-before.log");
        let console = match &settings.serial {
            Some(port) => {
                netboot.note(&format!("console: serial port {}", port.display()));
                ConsoleReader::Cable(SerialReader::start(port, settings.baud, &before)?)
            }
            None => ConsoleReader::IpmiSol(IpmiSolReader::start(
                ipmi_sign(&settings),
                &before,
                netboot.log(),
            )?),
        };
        let host = bmc_host(&settings).to_string();
        let screen = RemoteScreen::new(
            format!("{host}:{}", redfish::DEFAULT_VNC_PORT),
            "the iDRAC's VNC server",
            false,
            netboot.log(),
            turn_on_screen(&redfish, host),
        );
        let events_before = match redfish.event_entries() {
            Ok(events) => Some(events.iter().map(Event::key).collect()),
            Err(err) => {
                netboot.note(&format!("BMC events: {err}"));
                None
            }
        };
        let write_time = Duration::from_secs(u64::from(settings.write_minutes) * 60);
        let mut driver = RedfishDriver {
            settings,
            setup,
            redfish,
            netboot,
            console,
            serial_log: before,
            boot: 0,
            power: None,
            last_start: None,
            start_settle: START_SETTLE,
            write_time,
            screen,
            events_before,
        };
        driver.note(&format!(
            "BMC at {}; net-boot server at {}, ports {}",
            driver.settings.address,
            driver.settings.lab_address,
            driver.netboot.ports()
        ));
        Ok(driver)
    }

    /// Changes the power through the BMC, and notes it.
    fn change(&mut self, kind: ResetKind) -> Result<(), String> {
        self.power = None;
        if let Some(started) = self.last_start
            && kind != ResetKind::On
            && started.elapsed() < self.start_settle
        {
            let rest = self.start_settle - started.elapsed();
            self.note(&format!(
                "power: waiting {:.1} s after the machine started before a power change",
                rest.as_secs_f64()
            ));
            std::thread::sleep(rest);
        }
        self.note(&format!("power: {}", kind.name()));
        let changed = self.redfish.reset(kind);
        match &changed {
            Ok(()) if matches!(kind, ResetKind::On | ResetKind::ForceRestart) => {
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
            self.change(ResetKind::ForceOff)?;
            match self.wait_off((Instant::now() + HARD_OFF_AGAIN).min(deadline)) {
                Ok(()) => return Ok(()),
                Err(err) if Instant::now() >= deadline || self.setup.stop.requested() => {
                    return Err(err);
                }
                Err(_) => self.note("power: the machine is still on; asking again"),
            }
        }
    }

    /// Turns the machine on, and waits until the BMC says it is.
    fn turn_on(&mut self) -> Result<(), String> {
        self.change(ResetKind::On)?;
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
                        "the machine did not come on: the BMC still said it was off {} s after \
                         turning it on",
                        ON_TIME.as_secs()
                    ),
                });
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Waits until the BMC says the machine is off.
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

    /// Reads whether the machine is on from the BMC now.
    fn read_power(&mut self) -> Result<bool, String> {
        let on = self.redfish.power_state()?;
        self.power = Some((Instant::now(), on));
        Ok(on)
    }

    /// Boots the machine from the network once into what `offer` hands
    /// out, loading it with `cmdline`.
    fn net_boot(&mut self, what: &str, offer: Offer, cmdline: &str) -> Result<(), String> {
        self.netboot.begin(what, offer, cmdline)?;
        self.redfish.boot_from_network_once()?;
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
        let outcome =
            booted.and_then(|()| self.netboot.await_writer(&self.setup.stop, self.write_time));
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
                    "{why}; the writer's console is in {}, and what the driver did in redfish.log",
                    writer_log.display()
                ))
            }
        }
    }

    /// The screen, once the machine is on: a machine that is off has none.
    fn screen_on(&mut self) -> Result<&mut RemoteScreen, String> {
        if !self.is_on() {
            return Err("the machine is off, so it has no screen".into());
        }
        Ok(&mut self.screen)
    }

    /// Writes the entries the BMC's event logs gained since the driver
    /// started to `bmc-events.log`.
    fn write_events(&mut self) {
        let Some(before) = &self.events_before else {
            self.note("BMC events: the logs were not read at the start, so none are written");
            return;
        };
        let mut events = match self.redfish.event_entries() {
            Ok(events) => events,
            Err(err) => {
                self.note(&format!("BMC events: {err}"));
                return;
            }
        };
        events.retain(|event| !before.contains(&event.key()));
        events.sort_by(|a, b| a.created.cmp(&b.created));
        let lines: Vec<String> = events.iter().map(Event::line).collect();
        let lines = lines.iter().fold(String::new(), |text, line| text + line + "\n");
        let path = self.setup.logs.join("bmc-events.log");
        match fs::write(&path, lines) {
            Ok(()) => self.note(&format!("BMC events: {} new in {}", events.len(), path.display())),
            Err(err) => self.note(&format!("BMC events: {}: {err}", path.display())),
        }
    }
}

/// Turns the iDRAC's VNC server on with a password made for the run, and
/// says where it listens.
fn turn_on_screen(redfish: &Arc<Redfish>, host: String) -> TurnOn {
    let redfish = Arc::clone(redfish);
    Box::new(move || {
        let vendor = redfish.vendor()?;
        if vendor != Vendor::Dell {
            return Err(format!(
                "the BMC is {}'s, and the redfish driver shows only a Dell iDRAC's screen; set \
                 keys, mouse and screen to \"none\" in the machine file",
                vendor.name()
            ));
        }
        let password = redfish::screen_password()?;
        let port = redfish.enable_vnc(&password)?;
        Ok(Enabled { password, address: Some(format!("{host}:{port}")) })
    })
}

/// Checks what a run on the machine needs, without changing anything: the
/// BMC's password, its address and pin, the power state, the setting of
/// the next boot, a Dell's BIOS and iDRAC settings, the console
/// (Serial-over-LAN through ipmitool, or the serial port), the VNC screen
/// when the driver does the machine's `screen`, the lab network, the TFTP
/// folder and network GRUB, the writer's kit and the lab key.
pub fn check(settings: &RedfishSettings, key: Option<&Path>, screen: bool) -> Vec<Check> {
    let mut checks = Vec::new();
    let password = read_password(&settings.password_file);
    checks.push(found(
        "password",
        password
            .as_ref()
            .map(|_| "the BMC's password file is readable by this user alone".into())
            .map_err(|err| problem(err.clone())),
    ));
    if let Ok(password) = &password {
        checks.extend(check_bmc(settings, password, screen));
    }
    if let Some(port) = &settings.serial {
        checks.push(found(
            "serial port",
            crate::serial::check_port(port, settings.baud)
                .map(|()| format!("{} opens, at {} baud", port.display(), settings.baud))
                .map_err(problem),
        ));
    }
    checks.extend(real::check_lab_side(&lab_side(settings), key));
    checks
}

/// The checks that ask the BMC.
fn check_bmc(settings: &RedfishSettings, password: &str, screen: bool) -> Vec<Check> {
    let mut checks = Vec::new();
    let client = match Redfish::new(
        &base_url(settings),
        &settings.user,
        password,
        settings.pin.as_deref(),
    ) {
        Ok(client) => client,
        Err(err) => {
            checks.push(found("redfish", Err(problem(with_pin(settings, &err, true)))));
            return checks;
        }
    };
    let identity = match client.identity() {
        Ok(identity) => identity,
        Err(err) => {
            let wrong_pin = err.contains("pinned public key");
            checks.push(found("redfish", Err(problem(with_pin(settings, &err, wrong_pin)))));
            return checks;
        }
    };
    checks.push(found(
        "redfish",
        Ok(format!(
            "{} {}, BMC firmware {}, at {}",
            identity.vendor.name(),
            identity.model,
            identity.firmware,
            settings.address
        )),
    ));
    checks.push(found(
        "power",
        client
            .power_state()
            .map(|on| format!("the machine is {}", if on { "on" } else { "off" }))
            .map_err(problem),
    ));
    checks.push(found("net boot", check_network_boot(&client)));
    if identity.vendor == Vendor::Dell {
        checks.extend(check_bios(&client));
    }
    if settings.serial.is_none() {
        checks.push(found("ipmi lan", check_ipmi_lan(&client, identity.vendor)));
        checks.push(found("ipmitool", check_ipmitool(settings)));
    }
    if screen {
        checks.push(found("vnc licence", check_screen(&client, identity.vendor)));
    }
    checks
}

/// The message of a failed call to the BMC, with its pin as the BMC
/// shows it when the pin is what is wrong or missing.
fn with_pin(settings: &RedfishSettings, err: &str, pin_to_show: bool) -> String {
    if !pin_to_show {
        return err.to_string();
    }
    match redfish::pin_of(&settings.address) {
        Ok(pin) => format!(
            "{err}; the pin of the BMC at {} is {pin}: if this is the BMC, put pin = \"{pin}\" in \
             the machine file's [redfish] section",
            settings.address
        ),
        Err(shown) => format!("{err}; the BMC's pin could not be read: {shown}"),
    }
}

fn check_network_boot(client: &Redfish) -> Result<String, (Verdict, String)> {
    match client.network_boot_allowed() {
        Ok(true) => Ok("the BMC takes a one-time boot from the network".into()),
        Ok(false) => Err(problem(
            "the BMC's BootSourceOverrideTarget does not list Pxe, so it cannot boot the writer: \
             turn PXE boot on in the BIOS"
                .into(),
        )),
        Err(err) => Err(problem(err)),
    }
}

/// A Dell's BIOS settings the lab's boots rely on. Each is a note, with
/// what to set, since the BIOS is the person's to change while the machine
/// is off.
fn check_bios(client: &Redfish) -> Vec<Check> {
    let bios = match client.bios_attributes() {
        Ok(bios) => bios,
        Err(err) => {
            let detail = format!("the BIOS settings could not be read: {err}");
            return vec![found("bios", Err((Verdict::Note, detail)))];
        }
    };
    let setting = |name: &str| bios[name].as_str().unwrap_or_default().to_string();
    let verdict = |check: &'static str, right: bool, said: String, set: &str| {
        found(check, if right { Ok(said) } else { Err((Verdict::Note, format!("{said}: {set}"))) })
    };
    let (mode, serial, pxe) = (setting("BootMode"), setting("SerialComm"), setting("PxeDev1EnDis"));
    vec![
        verdict(
            "bios boot",
            mode == "Uefi",
            format!("BootMode is {mode}"),
            "set Boot Mode to UEFI, which the writer and the images boot with",
        ),
        verdict(
            "bios serial",
            serial.contains("Com2"),
            format!("SerialComm is {serial}"),
            "set Serial Communication to On with Console Redirection and the serial port to \
             COM2, which Serial-over-LAN reads",
        ),
        verdict(
            "bios pxe",
            pxe == "Enabled",
            format!("PxeDev1EnDis is {pxe}"),
            "enable PXE Device 1 on the card cabled to the lab switch",
        ),
    ]
}

fn check_ipmi_lan(client: &Redfish, vendor: Vendor) -> Result<String, (Verdict, String)> {
    let attributes = client.manager_attributes().map_err(problem)?;
    match attributes["IPMILan.1.Enable"].as_str() {
        Some("Enabled") => Ok("IPMI over LAN is on".into()),
        Some(other) => Err(problem(format!(
            "IPMI over LAN is {other}: turn it on in the iDRAC's network services, since the \
             console is read through it"
        ))),
        None if vendor == Vendor::Dell => {
            Err(problem("the iDRAC lists no IPMILan.1.Enable".into()))
        }
        None => Err((
            Verdict::Note,
            format!(
                "{} keeps this setting elsewhere: check that IPMI over LAN is on",
                vendor.name()
            ),
        )),
    }
}

/// Whether ipmitool is there and the BMC answers it.
fn check_ipmitool(settings: &RedfishSettings) -> Result<String, (Verdict, String)> {
    ipmi_sign(settings)
        .run(&["sol", "info"], SOL_INFO_TIME)
        .map(|_| {
            format!("ipmitool reaches the BMC at {} and answers `sol info`", bmc_host(settings))
        })
        .map_err(|err| {
            if err.contains("not installed") {
                problem(format!("{err}: `sudo apt install ipmitool`, or redfish.ipmitool names it"))
            } else {
                problem(format!("{err}: is IPMI over LAN on, and the user allowed to use it?"))
            }
        })
}

/// Whether the screen can be had: the keys, mouse and screen controls
/// are the driver's, and an iDRAC with a virtual console licence has the
/// VNC server's attributes.
fn check_screen(client: &Redfish, vendor: Vendor) -> Result<String, (Verdict, String)> {
    if vendor != Vendor::Dell {
        return Err(problem(format!(
            "the BMC is {}'s, and the driver shows only a Dell iDRAC's screen: set keys, mouse \
             and screen to \"none\" in the machine file",
            vendor.name()
        )));
    }
    let attributes = client.manager_attributes().map_err(problem)?;
    if attributes.get("VNCServer.1.Enable").is_some() {
        Ok("the iDRAC has a VNC server; the driver turns it on when a test wants the screen".into())
    } else {
        Err(problem(
            "the iDRAC lists no VNC server, which it does only with a virtual console licence \
             (iDRAC9 Enterprise has one): get the licence, or set keys, mouse and screen to \
             \"none\" in the machine file"
                .into(),
        ))
    }
}

/// The BMC's host, without the port.
fn bmc_host(settings: &RedfishSettings) -> &str {
    settings.address.split(':').next().unwrap_or_default()
}

/// The BMC's address as a URL: https, and plain http only for the tests'
/// pretend BMC on the loopback address, which [`Redfish::new`] holds to
/// that.
fn base_url(settings: &RedfishSettings) -> String {
    let scheme =
        if bmc_host(settings) == "127.0.0.1" && settings.pin.is_none() { "http" } else { "https" };
    format!("{scheme}://{}", settings.address)
}

/// How ipmitool signs in to the BMC.
fn ipmi_sign(settings: &RedfishSettings) -> ipmi_sol::Sign {
    ipmi_sol::Sign {
        program: settings.ipmitool.clone(),
        host: bmc_host(settings).to_string(),
        user: settings.user.clone(),
        password_file: settings.password_file.clone(),
    }
}

/// What the machine file says about the lab network.
fn lab_side(settings: &RedfishSettings) -> LabSide<'_> {
    LabSide {
        lab_address: &settings.lab_address,
        boot_port: settings.boot_port,
        tftp: &settings.tftp,
        mac: &settings.mac,
        writer: &settings.writer,
    }
}

/// Reads the BMC's password from its file, which only this user may read.
fn read_password(path: &Path) -> Result<String, String> {
    real::read_password(path, "the BMC", "redfish.password_file")
}

impl Driver for RedfishDriver {
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
        match action {
            PowerAction::On => {
                if self.read_power()? {
                    return Ok(());
                }
                self.turn_on()
            }
            PowerAction::Off => {
                self.change(ResetKind::GracefulShutdown)?;
                self.wait_off(deadline)
            }
            PowerAction::Button => self.change(ResetKind::GracefulShutdown),
            PowerAction::Reset => self.change(ResetKind::ForceRestart),
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
                // only the BMC's word ends one as off.
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

    /// Writes a line to redfish.log, with the seconds since the driver
    /// began.
    fn note(&mut self, line: &str) {
        self.netboot.note(line);
    }

    fn serial_log(&self) -> &Path {
        &self.serial_log
    }

    fn set_link(&mut self, _up: bool) -> Result<(), String> {
        Err(persons_job("redfish", "plug or pull the network cable"))
    }

    fn plug_usb(&mut self, _stick: &Stick) -> Result<(), String> {
        Err(persons_job("redfish", "plug in a USB stick"))
    }

    fn unplug_usb(&mut self) -> Result<(), String> {
        Err(persons_job("redfish", "pull out a USB stick"))
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
        self.write_time
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
        self.write_events();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipmi_sol::fake::script;
    use crate::netboot::tests::{make_image, pretend_writer};
    use crate::redfish::fake::{FakeBmc, PASSWORD, USER, entry};
    use crate::stop::Stop;
    use crate::testing::TempDir;
    use crate::vnc::fake::{Event as ScreenEvent, FakeVnc, pattern};
    use std::fs::OpenOptions;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    const MAC: &str = "aa:bb:cc:00:11:22";

    /// A machine behind a pretend BMC: a net boot runs the pretend writer
    /// (or prints what a Linux twin would), and the disk boots to its
    /// ready line. The console is a file the pretend machine appends to,
    /// which a pretend `ipmitool` follows, or which stands in for the
    /// serial port with `cable`.
    struct Bench {
        dir: TempDir,
        fake: FakeBmc,
        /// The pretend iDRAC's VNC server.
        screen: FakeVnc,
        driver: RedfishDriver,
    }

    fn bench(label: &str, tamper: bool) -> Bench {
        bench_with(label, tamper, false)
    }

    fn bench_with(label: &str, tamper: bool, cable: bool) -> Bench {
        let dir = TempDir::new(label);
        let console = dir.write("console", "");
        dir.write("writer/kernel", "KERNEL");
        dir.write("writer/initrd", "INITRD");
        dir.write(
            "writer/kit.toml",
            "kernel = \"kernel\"\ninitrd = \"initrd\"\ncmdline = \"console=ttyS0 rdinit=/writer\"\n\
             done = \"writer: done\"\n",
        );
        let password = dir.write("bmc-password", &format!("{PASSWORD}\n"));
        fs::set_permissions(&password, fs::Permissions::from_mode(0o600)).unwrap();
        let base = dir.path().display();
        let ipmitool = script(
            &dir,
            "ipmitool",
            &format!(
                "case \"$*\" in *deactivate*) exit 0;; *\"sol info\"*) echo 'Set in progress : \
                 set-complete'; exit 0;; esac\n: > '{base}/ready'\n\
                 exec tail -n 0 -f '{base}/console'"
            ),
        );
        let fake = FakeBmc::start();
        let tftp = dir.path().join("tftp");
        let disk = dir.path().join("disk");
        {
            let (tftp, console, disk) = (tftp.clone(), console.clone(), disk.clone());
            fake.state.lock().unwrap().on_boot = Some(Box::new(move |network| {
                let (tftp, console, disk) = (tftp.clone(), console.clone(), disk.clone());
                // As the machine would: the firmware, then what it boots.
                std::thread::spawn(move || {
                    let say_bytes = |bytes: &[u8]| {
                        let mut port = OpenOptions::new().append(true).open(&console).unwrap();
                        port.write_all(bytes).unwrap();
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
        let settings = RedfishSettings {
            address: fake.address.to_string(),
            user: USER.into(),
            password_file: password,
            pin: None,
            mac: MAC.into(),
            disk: "TESTDISK".into(),
            serial: cable.then_some(console),
            baud: 115_200,
            machine_address: "127.0.0.1".into(),
            lab_address: "127.0.0.1".into(),
            boot_port: 0,
            tftp,
            writer: dir.path().join("writer"),
            console: None,
            grow_root: true,
            ipmitool,
            write_minutes: 1,
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
        let driver = RedfishDriver::new(settings, setup).unwrap();
        let screen = {
            let state = Arc::clone(&fake.state);
            FakeVnc::start(Box::new(move || state.lock().unwrap().open_password()))
        };
        fake.state.lock().unwrap().vnc_port = screen.address.port();
        let mut bench = Bench { dir, fake, screen, driver };
        bench.driver.screen.address = bench.screen.address.to_string();
        if !cable {
            // The pretend ipmitool follows what the machine writes from
            // here on, as a real session carries only what comes while
            // it is open.
            assert!(wait_for(&bench.dir.path().join("ready"), ""));
            std::thread::sleep(Duration::from_millis(150));
        }
        bench
    }

    fn wait_for(path: &Path, text: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if fs::read(path).is_ok_and(|log| String::from_utf8_lossy(&log).contains(text)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn log(bench: &Bench) -> String {
        fs::read_to_string(bench.dir.path().join("logs/redfish.log")).unwrap()
    }

    #[test]
    fn a_fresh_boot_writes_the_image_and_boots_the_disk() {
        let mut bench = bench("redfish-fresh", false);
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
        // The writer is told its own address, its card's MAC and the
        // BMC's serial port, COM2.
        let script = fs::read_to_string(bench.dir.path().join("writer-script")).unwrap();
        assert!(
            script.contains(&format!("dukovo.lab.ip=127.0.0.1/24 dukovo.lab.mac={MAC}")),
            "{script}"
        );
        assert_eq!(script.matches("console=").count(), 1, "{script}");
        assert!(script.contains("console=ttyS1,115200"), "{script}");
        assert!(bench.driver.is_on());
        // The machine was off: the next boot is set to the network once,
        // then it is turned on.
        assert_eq!(bench.fake.calls(), ["PATCH Boot Pxe Once UEFI", "POST Reset On"]);

        // A power cut is the BMC's force off; on boots the disk again.
        let soon = Instant::now() + Duration::from_secs(5);
        bench.driver.power(PowerAction::Cut, soon).unwrap();
        assert!(!bench.driver.is_on());
        bench.driver.power(PowerAction::On, soon).unwrap();
        assert!(bench.driver.is_on());
        // Off asks the system to shut down and waits for it; the button
        // only asks; a reset restarts at once.
        bench.driver.power(PowerAction::Off, soon).unwrap();
        assert!(!bench.driver.is_on());
        bench.driver.power(PowerAction::On, soon).unwrap();
        bench.driver.power(PowerAction::Button, soon).unwrap();
        bench.driver.power(PowerAction::On, soon).unwrap();
        bench.driver.power(PowerAction::Reset, soon).unwrap();
        let calls = bench.fake.calls();
        assert_eq!(
            calls[2..],
            [
                "POST Reset ForceOff",
                "POST Reset On",
                "POST Reset GracefulShutdown",
                "POST Reset On",
                "POST Reset GracefulShutdown",
                "POST Reset On",
                "POST Reset ForceRestart",
            ]
        );

        // A second fresh boot turns the machine off first.
        bench.driver.boot_fresh().unwrap();
        assert!(bench.driver.serial_log().ends_with("logs/serial-2.log"));
        assert!(wait_for(bench.driver.serial_log(), "Boot complete."));
        let calls = bench.fake.calls();
        assert!(calls.ends_with(&[
            "POST Reset ForceOff".into(),
            "PATCH Boot Pxe Once UEFI".into(),
            "POST Reset On".into(),
        ]));
        let log = log(&bench);
        assert!(log.contains("net boot: GET plan"), "{log}");
        assert!(!log.contains(PASSWORD));
        assert_eq!(bench.driver.peer_address().as_deref(), Some("127.0.0.1"));
        assert_eq!(bench.driver.amt_address(), None);
        assert_eq!(bench.driver.reach(445, false), Ok(445));
        assert_eq!(bench.driver.boot_time(), Duration::from_secs(60));
    }

    #[test]
    fn a_serial_cable_is_read_when_the_machine_file_names_one() {
        let mut bench = bench_with("redfish-cable", false, true);
        bench.driver.boot_fresh().unwrap();
        assert!(wait_for(bench.driver.serial_log(), "Boot complete."));
        let writer_log = bench.dir.path().join("logs/writer-1.log");
        assert!(fs::read_to_string(writer_log).unwrap().contains("writer: writing"));
        // COM1's name for Linux, and ipmitool never run.
        let script = fs::read_to_string(bench.dir.path().join("writer-script")).unwrap();
        assert!(script.contains("console=ttyS0,115200"), "{script}");
        assert!(!bench.dir.path().join("ready").exists());
        assert!(log(&bench).contains("console: serial port "));
        let checks = check(&bench.driver.settings, None, false);
        let verdict =
            |name: &str| checks.iter().find(|check| check.check == name).map(|check| check.verdict);
        assert_eq!(verdict("serial port"), Some(Verdict::Ok));
        assert_eq!(verdict("ipmitool"), None);
        assert_eq!(verdict("ipmi lan"), None);
    }

    #[test]
    fn a_machine_that_is_powering_off_is_still_on() {
        let mut bench = bench("redfish-powering-off", false);
        bench.fake.state.lock().unwrap().power = "PoweringOff".into();
        assert!(bench.driver.is_on());
        let soon = Instant::now() + Duration::from_secs(5);
        bench.driver.power(PowerAction::Cut, soon).unwrap();
        assert_eq!(bench.fake.calls(), ["POST Reset ForceOff"]);
        assert!(!bench.driver.is_on());
    }

    #[test]
    fn a_power_state_the_bmc_does_not_give_is_as_last_seen_or_on() {
        let mut bench = bench("redfish-power-unknown", false);
        let soon = Instant::now() + Duration::from_secs(5);
        bench.driver.power(PowerAction::On, soon).unwrap();
        // Nothing read since the change: on, so waits go on.
        bench.fake.state.lock().unwrap().no_power_state = true;
        assert!(bench.driver.is_on());
        // Off as the BMC last said.
        bench.fake.state.lock().unwrap().no_power_state = false;
        bench.driver.power(PowerAction::Cut, soon).unwrap();
        bench.fake.state.lock().unwrap().no_power_state = true;
        std::thread::sleep(POWER_FRESH);
        assert!(!bench.driver.is_on());
        assert!(log(&bench).contains("power state: the BMC at"), "{}", log(&bench));
    }

    #[test]
    fn a_hard_off_the_bmc_ignores_is_asked_again() {
        let mut bench = bench("redfish-power-timing", false);
        let soon = Instant::now() + Duration::from_secs(10);
        bench.driver.power(PowerAction::On, soon).unwrap();
        bench.fake.state.lock().unwrap().ignored_force_offs = 1;
        bench.driver.power(PowerAction::Cut, soon).unwrap();
        assert!(!bench.driver.is_on());
        let offs = bench.fake.calls().iter().filter(|call| *call == "POST Reset ForceOff").count();
        assert_eq!(offs, 2);
        assert!(log(&bench).contains("power: the machine is still on; asking again"));
    }

    #[test]
    fn a_reset_the_bmc_turns_down_says_why() {
        let mut bench = bench("redfish-no-reset", false);
        bench.fake.state.lock().unwrap().reset_types = Some(vec!["On".into()]);
        let soon = Instant::now() + Duration::from_secs(5);
        bench.driver.power(PowerAction::On, soon).unwrap();
        let err = bench.driver.power(PowerAction::Reset, soon).unwrap_err();
        assert!(err.contains("does not take the reset type ForceRestart"), "{err}");
        assert!(log(&bench).contains("power: the BMC does not take"));
    }

    #[test]
    fn a_power_change_waits_a_while_after_the_machine_starts() {
        let mut bench = bench("redfish-start-settle", false);
        assert_eq!(bench.driver.start_settle, Duration::ZERO);
        bench.driver.start_settle = Duration::from_millis(800);
        let soon = Instant::now() + Duration::from_secs(10);
        bench.driver.power(PowerAction::On, soon).unwrap();
        let on = Instant::now();
        bench.driver.power(PowerAction::Cut, soon).unwrap();
        assert!(on.elapsed() >= Duration::from_millis(700), "{:?}", on.elapsed());
        // An on after an off does not wait.
        let off = Instant::now();
        bench.driver.power(PowerAction::On, soon).unwrap();
        assert!(off.elapsed() < Duration::from_millis(700), "{:?}", off.elapsed());
        assert_eq!(log(&bench).matches("before a power change").count(), 1);
    }

    #[test]
    fn a_disk_that_reads_back_wrong_fails_the_boot() {
        let mut bench = bench("redfish-tamper", true);
        let err = bench.driver.boot_fresh().unwrap_err();
        assert!(err.contains("did not hold the image"), "{err}");
        assert!(err.contains("writer-1.log") && err.contains("redfish.log"), "{err}");
        // The machine is not left running the writer.
        assert!(!bench.driver.is_on());
    }

    #[test]
    fn the_writer_is_given_the_time_the_machine_file_says() {
        let mut bench = bench("redfish-write-time", false);
        assert_eq!(bench.driver.boot_time(), Duration::from_secs(60));
        bench.driver.write_time = Duration::from_secs(2);
        // No writer loads: the machine file's time is what runs out.
        bench.fake.state.lock().unwrap().on_boot = None;
        let started = Instant::now();
        let err = bench.driver.boot_fresh().unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(err.contains("did not load the writer within 0 minutes"), "{err}");
        assert!(!bench.driver.is_on());
    }

    #[test]
    fn the_linux_twin_is_the_machine_net_booted() {
        let mut bench = bench("redfish-twin", false);
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
    fn the_screen_is_the_idracs_vnc_and_takes_keys_and_the_mouse() {
        let mut bench = bench("redfish-screen", false);
        let err = bench.driver.press_keys(&["a".into()]).unwrap_err();
        assert!(err.contains("the machine is off"), "{err}");
        let soon = Instant::now() + Duration::from_secs(5);
        bench.driver.power(PowerAction::On, soon).unwrap();

        bench.driver.press_keys(&["shift-a".into()]).unwrap();
        bench.driver.point(39, 29).unwrap();
        bench.driver.click(Button::Right).unwrap();
        let dump = bench.dir.path().join("screen.ppm");
        bench.driver.screen_dump(&dump).unwrap();
        let image = crate::screen::Image::parse_ppm(&fs::read(&dump).unwrap()).unwrap();
        assert_eq!(image.pixels(), pattern(40, 30));
        let shot = bench.dir.path().join("screen.png");
        bench.driver.screenshot(&shot).unwrap();
        assert!(fs::read(&shot).unwrap().starts_with(b"\x89PNG\r\n\x1a\n"));
        // No Shift tap first: that is AMT's quirk.
        let shift = 0xffe1;
        assert_eq!(
            bench.screen.events(),
            [
                ScreenEvent::Key(shift, true),
                ScreenEvent::Key(0x61, true),
                ScreenEvent::Key(0x61, false),
                ScreenEvent::Key(shift, false),
                ScreenEvent::Pointer(0, 39, 29),
                ScreenEvent::Pointer(4, 39, 29),
                ScreenEvent::Pointer(0, 39, 29),
            ]
        );
        // One session, and the iDRAC's VNC server set up once, over the
        // port the iDRAC named.
        assert_eq!(bench.screen.state.lock().unwrap().sessions, 1);
        let enables = |bench: &Bench| {
            bench.fake.calls().iter().filter(|call| call.starts_with("PATCH VNC")).count()
        };
        assert_eq!(enables(&bench), 1);

        // A session the iDRAC ends is opened again, with no new setup, and
        // a power change leaves a session alone.
        bench.screen.close_sessions();
        bench.driver.point(1, 1).unwrap();
        assert_eq!(bench.screen.state.lock().unwrap().sessions, 2);
        bench.driver.power(PowerAction::Cut, soon).unwrap();
        assert_eq!(bench.screen.state.lock().unwrap().live, 1);
        assert_eq!(enables(&bench), 1);
        let log = log(&bench);
        assert!(log.contains("remote screen: turning on the iDRAC's VNC server"), "{log}");
        assert!(log.contains("remote screen: session open, 40x30"), "{log}");
        let password = bench.fake.state.lock().unwrap().vnc_password.clone().unwrap();
        assert!(!log.contains(&password), "{log}");
    }

    #[test]
    fn a_screen_the_bmc_cannot_give_says_why() {
        let mut bench = bench("redfish-no-screen", false);
        let soon = Instant::now() + Duration::from_secs(5);
        bench.driver.power(PowerAction::On, soon).unwrap();
        let dump = bench.dir.path().join("x.ppm");
        bench.fake.state.lock().unwrap().licensed = false;
        let err = bench.driver.screen_dump(&dump).unwrap_err();
        assert!(err.contains("virtual console licence"), "{err}");
        let checks = check(&bench.driver.settings, None, true);
        let screen = checks.iter().find(|check| check.check == "vnc licence").unwrap();
        assert_eq!(screen.verdict, Verdict::Problem);
        // Another vendor's BMC has no screen the lab can use.
        {
            let mut state = bench.fake.state.lock().unwrap();
            state.licensed = true;
            state.manufacturer = "Acme".into();
            state.oem = "Acme".into();
        }
        let err = bench.driver.screen_dump(&dump).unwrap_err();
        assert!(err.contains("only a Dell iDRAC's screen"), "{err}");
        let checks = check(&bench.driver.settings, None, true);
        let screen = checks.iter().find(|check| check.check == "vnc licence").unwrap();
        assert!(screen.detail.contains("keys, mouse and screen to \"none\""), "{}", screen.detail);
        // A machine whose screen is no one's is not checked for one.
        let checks = check(&bench.driver.settings, None, false);
        assert!(checks.iter().all(|check| check.check != "vnc licence"));
    }

    #[test]
    fn the_events_made_during_the_run_are_written_when_it_ends() {
        let mut bench = bench("redfish-events", false);
        let soon = Instant::now() + Duration::from_secs(5);
        bench.driver.power(PowerAction::On, soon).unwrap();
        {
            let mut state = bench.fake.state.lock().unwrap();
            state.sel.push(entry(4, "2026-11-23T12:00:05-06:00", "OK", "The system powered on."));
            state.lclog.push(entry(2, "2026-11-23T12:00:01-06:00", "Critical", "Fan 3 failed"));
        }
        bench.driver.shut_off();
        assert!(!bench.driver.is_on());
        let events = fs::read_to_string(bench.dir.path().join("logs/bmc-events.log")).unwrap();
        assert_eq!(
            events,
            "2026-11-23T12:00:01-06:00 Critical SYS1000 Fan 3 failed\n\
             2026-11-23T12:00:05-06:00 OK SYS1000 The system powered on.\n"
        );
        assert!(log(&bench).contains("BMC events: 2 new in "));
    }

    #[test]
    fn a_bmc_that_cannot_be_read_at_the_start_writes_no_events() {
        let dir = TempDir::new("redfish-wrong-password");
        let password = dir.write("bmc-password", "not the password\n");
        fs::set_permissions(&password, fs::Permissions::from_mode(0o600)).unwrap();
        dir.write("writer/kernel", "K");
        dir.write("writer/initrd", "I");
        dir.write(
            "writer/kit.toml",
            "kernel = \"kernel\"\ninitrd = \"initrd\"\ncmdline = \"console=ttyS0\"\ndone = \"d\"\n",
        );
        let fake = FakeBmc::start();
        let settings = RedfishSettings {
            address: fake.address.to_string(),
            user: USER.into(),
            password_file: password,
            pin: None,
            mac: MAC.into(),
            disk: "TESTDISK".into(),
            serial: Some(dir.write("ttyUSB0", "")),
            baud: 115_200,
            machine_address: "127.0.0.1".into(),
            lab_address: "127.0.0.1".into(),
            boot_port: 0,
            tftp: dir.path().join("tftp"),
            writer: dir.path().join("writer"),
            console: None,
            grow_root: true,
            ipmitool: "ipmitool".into(),
            write_minutes: 1,
        };
        let setup = Setup {
            image: dir.path().join("image"),
            logs: dir.path().join("logs"),
            work: dir.path().join("work"),
            sockets: dir.path().join("sockets"),
            key: None,
            redteam: None,
            ob: None,
            stop: Stop::default(),
        };
        let mut driver = RedfishDriver::new(settings, setup).unwrap();
        let err = driver.boot_fresh().unwrap_err();
        assert!(err.contains("answered 401") && !err.contains("not the password"), "{err}");
        driver.shut_off();
        assert!(!dir.path().join("logs/bmc-events.log").exists());
        let log = fs::read_to_string(dir.path().join("logs/redfish.log")).unwrap();
        assert!(log.contains("BMC events: the BMC at"), "{log}");
        assert!(log.contains("so none are written"), "{log}");
        assert!(!log.contains("not the password"), "{log}");
    }

    #[test]
    fn a_password_file_others_can_read_is_refused() {
        let dir = TempDir::new("redfish-password");
        let open = dir.write("password", "secret\n");
        fs::set_permissions(&open, fs::Permissions::from_mode(0o644)).unwrap();
        let err = read_password(&open).unwrap_err();
        assert!(err.contains("chmod 600") && err.contains("the BMC's password file"), "{err}");
        fs::set_permissions(&open, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_password(&open).unwrap(), "secret");
    }

    #[test]
    fn a_bmc_off_the_loopback_is_not_talked_to_without_its_pin() {
        let mut bench = bench("redfish-pin", false);
        let mut settings = bench.driver.settings.clone();
        settings.address = "192.0.2.7".into();
        let checks = check(&settings, None, false);
        let found = checks.iter().find(|check| check.check == "redfish").unwrap();
        assert_eq!(found.verdict, Verdict::Problem);
        assert!(found.detail.contains("needs its pin"), "{}", found.detail);
        assert!(found.detail.contains("redfish.pin"), "{}", found.detail);
        bench.driver.shut_off();
    }

    #[test]
    fn checks_what_a_run_needs() {
        let bench = bench("redfish-check", false);
        let checks = check(&bench.driver.settings, None, true);
        let verdict =
            |name: &str| checks.iter().find(|check| check.check == name).map(|check| check.verdict);
        assert_eq!(verdict("password"), Some(Verdict::Ok));
        assert_eq!(verdict("redfish"), Some(Verdict::Ok));
        assert_eq!(verdict("power"), Some(Verdict::Ok));
        assert_eq!(verdict("net boot"), Some(Verdict::Ok));
        assert_eq!(verdict("bios boot"), Some(Verdict::Ok));
        assert_eq!(verdict("bios serial"), Some(Verdict::Ok));
        assert_eq!(verdict("bios pxe"), Some(Verdict::Ok));
        assert_eq!(verdict("ipmi lan"), Some(Verdict::Ok));
        assert_eq!(verdict("ipmitool"), Some(Verdict::Ok));
        assert_eq!(verdict("vnc licence"), Some(Verdict::Ok));
        assert_eq!(verdict("serial port"), None);
        assert_eq!(verdict("lab network"), Some(Verdict::Ok));
        assert_eq!(verdict("tftp"), Some(Verdict::Ok));
        // No network GRUB in this pretend TFTP folder, and no lab key.
        assert_eq!(verdict("network grub"), Some(Verdict::Problem));
        assert_eq!(verdict("writer"), Some(Verdict::Ok));
        assert_eq!(verdict("lab key"), Some(Verdict::Problem));
        let detail = |name: &str| {
            checks
                .iter()
                .find(|check| check.check == name)
                .map(|check| check.detail.clone())
                .unwrap()
        };
        assert_eq!(
            detail("redfish"),
            format!("Dell PowerEdge R740, BMC firmware 4.40.00.00, at {}", bench.fake.address)
        );
        assert_eq!(detail("power"), "the machine is off");
        // Checking leaves the machine as it was.
        assert!(bench.fake.calls().is_empty());

        // A BIOS set up otherwise is a note with what to set, and a BMC
        // with IPMI over LAN off, a problem.
        {
            let mut state = bench.fake.state.lock().unwrap();
            state.bios = serde_json::json!({ "BootMode": "Bios", "SerialComm": "Off",
                                              "PxeDev1EnDis": "Disabled" });
            state.ipmi_lan = "Disabled".into();
            state.boot_targets = vec!["None".into(), "Hdd".into()];
        }
        let checks = check(&bench.driver.settings, None, false);
        let found = |name: &str| checks.iter().find(|check| check.check == name).unwrap();
        for name in ["bios boot", "bios serial", "bios pxe"] {
            assert_eq!(found(name).verdict, Verdict::Note, "{name}");
        }
        assert!(found("bios serial").detail.contains("COM2"), "{}", found("bios serial").detail);
        assert_eq!(found("ipmi lan").verdict, Verdict::Problem);
        assert_eq!(found("net boot").verdict, Verdict::Problem);
        assert!(checks.iter().all(|check| check.check != "vnc licence"));
    }

    #[test]
    fn a_wrong_user_or_password_is_a_problem_that_names_the_file() {
        let bench = bench("redfish-check-wrong", false);
        let mut settings = bench.driver.settings.clone();
        settings.user = "someone".into();
        let checks = check(&settings, None, false);
        let found = checks.iter().find(|check| check.check == "redfish").unwrap();
        assert_eq!(found.verdict, Verdict::Problem);
        assert!(
            found.detail.contains("401") && found.detail.contains("password_file"),
            "{}",
            found.detail
        );
        // Nothing else asked of the BMC after that.
        assert!(checks.iter().all(|check| check.check != "power"));
    }

    #[test]
    fn ipmitool_that_is_missing_or_unanswered_is_a_problem() {
        let bench = bench("redfish-check-ipmitool", false);
        let mut settings = bench.driver.settings.clone();
        settings.ipmitool = bench.dir.path().join("missing");
        let checks = check(&settings, None, false);
        let found = checks.iter().find(|check| check.check == "ipmitool").unwrap();
        assert_eq!(found.verdict, Verdict::Problem);
        assert!(found.detail.contains("sudo apt install ipmitool"), "{}", found.detail);
        settings.ipmitool = script(
            &bench.dir,
            "silent-ipmitool",
            "echo 'Error: Unable to establish IPMI v2 / RMCP+ session' >&2\nexit 1",
        );
        let checks = check(&settings, None, false);
        let found = checks.iter().find(|check| check.check == "ipmitool").unwrap();
        assert!(found.detail.contains("Unable to establish IPMI v2"), "{}", found.detail);
    }
}

// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The plug driver: a real machine with no AMT, on the lab network,
//! powered through a Shelly smart plug, with whichever cables to the lab
//! PC its machine file names.
//!
//! - **Power** is the plug's. The machine's firmware is set to power on
//!   after a power loss, so switching the plug on starts the machine. A
//!   power cut and a reset are both a hard power loss (the system gets no
//!   warning, as when the cord is pulled); a reset is the plug off, a
//!   pause, and on again. The plug cannot ask the system to shut down, so
//!   the power button and a graceful off fail here. The machine is on
//!   when the plug is and the machine draws at least the machine file's
//!   `on_watts`: a machine that shut itself down leaves the plug on.
//! - **A fresh boot** writes the image under test to the machine's disk
//!   first. The machine's firmware boots from the network before its disk
//!   every time, so the driver writes the boot script and power-cycles
//!   the plug: the writer the net-boot server hands the machine writes the
//!   image and checks it, and the machine restarts into its disk, its
//!   next net boot finding no script (see netboot.rs and real.rs). Only
//!   the disk with the machine file's serial number is ever written. A
//!   serial number that no disk has fails the boot with the writer's
//!   list of the disks it found.
//! - **The console** comes over the network: Dukovo's netlog sends the
//!   log to the lab PC, one log per boot (see netlog.rs). The writer has
//!   no console there, so `writer-<n>.log` holds what the net-boot server
//!   saw it do. With a serial cable on the board's COM1 (`serial`), the
//!   console comes from the cable instead (see serial.rs), from the
//!   firmware's first line, and the writer's console is `writer-<n>.log`.
//! - **SSH** and the lab PC's side of a test reach the machine on the lab
//!   network, where it has an address of its own.
//! - **The screen** is a picture of the machine's video output, taken by
//!   an HDMI capture stick on the lab PC (`capture`, see capture.rs). Its
//!   pixels stray a little from those the machine drew, which the text
//!   reader allows for ([`Driver::screen_noise`]).
//! - **The keyboard and mouse** are a CH9329 cable (`hid`, see
//!   ch9329.rs): the lab PC writes reports to its serial end, and the
//!   machine sees a USB keyboard and an absolute mouse at the other.
//! - **The Linux twin** is the machine itself, net booted into the Linux
//!   kit, as on an AMT machine; its done line comes over the serial cable,
//!   so it needs one.
//!
//! Without its cable, each of these is `none` in the machine file, and
//! tests that need it are skipped. Cables, sticks, cards and discs are a
//! person's to handle. What the driver does goes in the bundle's
//! `logs/<machine>/plug.log`.

use std::fs::{self, File};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::real::{
    self, Check, LabSide, NetBoot, Verdict, WRITE_TIME, check_lab_side, found, persons_job,
    problem, ssh_target, with_console,
};
use super::{Driver, Setup};
use crate::bench::Kit;
use crate::capture::{self, Capture};
use crate::ch9329::{self, Ch9329};
use crate::machines::PlugSettings;
use crate::netboot::{self, Offer};
use crate::netlog::NetlogReader;
use crate::serial::{self, ConsoleLog, SerialReader};
use crate::shelly::{PlugStatus, Shelly};
use crate::ssh;
use crate::suite::{Button, PowerAction, Stick};
use crate::{png, screen};

/// How long the plug stays off in a power cycle: the machine's supply
/// loses its charge, and a relay switched back at once may not have let
/// go.
const CYCLE_OFF: Duration =
    if cfg!(test) { Duration::from_millis(50) } else { Duration::from_secs(5) };

/// How long after the plug is switched on the next change waits: relays
/// do not like being chattered.
const START_SETTLE: Duration = Duration::from_secs(5);

/// How long the machine is given to start drawing power once the plug is
/// switched on.
const ON_TIME: Duration = if cfg!(test) { Duration::from_secs(1) } else { Duration::from_secs(30) };

/// How long a reading of the plug is trusted: waits ask often.
const POWER_FRESH: Duration = Duration::from_secs(1);

/// How long the machine is given to set up the CH9329's keyboard and
/// mouse (its USB host to enumerate them) before keys are sent: after a
/// power on, the firmware and then the system each set them up afresh.
const HID_SETTLE: Duration =
    if cfg!(test) { Duration::from_millis(500) } else { Duration::from_secs(10) };

/// How long a key is held, and the pause after each combination: a few
/// of the keyboard's USB polls, so the machine sees each report.
const KEY_HOLD: Duration = Duration::from_millis(30);
const KEY_GAP: Duration = Duration::from_millis(30);

/// How long a mouse button is held, as long as a person holds one.
const CLICK_HOLD: Duration = Duration::from_millis(100);

/// What the screen says when the machine file names no capture stick.
const NO_SCREEN: &str = "the machine file names no capture stick (plug.capture), so the plug \
                         driver has no screen: its screen control is \"none\"";

/// What the keyboard and mouse say when the machine file names no CH9329
/// cable.
const NO_KEYBOARD: &str = "the machine file names no CH9329 cable (plug.hid), so the plug driver \
                           has no keyboard or mouse: its keys and mouse controls are \"none\"";

/// What reads the machine's console, until the driver is dropped.
enum ConsoleReader {
    Cable(SerialReader),
    Netlog(NetlogReader),
}

impl ConsoleReader {
    /// The log the console goes to.
    fn log(&self) -> &ConsoleLog {
        match self {
            ConsoleReader::Cable(reader) => reader.log(),
            ConsoleReader::Netlog(reader) => reader.log(),
        }
    }

    fn is_cable(&self) -> bool {
        matches!(self, ConsoleReader::Cable(_))
    }
}

/// A real machine powered through a Shelly plug.
pub struct PlugDriver {
    settings: PlugSettings,
    setup: Setup,
    plug: Shelly,
    netboot: NetBoot,
    console: ConsoleReader,
    /// The current boot's console log.
    console_log: PathBuf,
    boot: u32,
    /// Whether the machine was on when last read, and when.
    power: Option<(Instant, bool)>,
    /// When the plug was last switched on, and how long a change after it
    /// waits ([`START_SETTLE`]).
    last_start: Option<Instant>,
    start_settle: Duration,
    /// When the plug was last switched off: it is switched on again only
    /// [`CYCLE_OFF`] after.
    last_stop: Option<Instant>,
    /// The capture stick, when the machine file names one.
    capture: Option<Capture>,
    /// The screen's size as this boot's console log gives it, once read.
    framebuffer: Option<(u32, u32)>,
    /// The CH9329 cable, opened the first time a test wants it, and again
    /// after it fails.
    hid: Option<Ch9329>,
    /// Where the pointer was put last, as the cable's absolute X and Y.
    pointer: (u16, u16),
}

impl PlugDriver {
    /// Gets ready to drive the machine: reads the writer's kit, and starts
    /// the net-boot server and the console's reader. Leaves the machine's
    /// power as it is.
    pub fn new(settings: PlugSettings, setup: Setup) -> Result<PlugDriver, String> {
        fs::create_dir_all(&setup.logs)
            .and_then(|()| fs::create_dir_all(&setup.work))
            .map_err(|err| format!("making the machine's folders: {err}"))?;
        let side = lab_side(&settings);
        let lab = side.lab_ip()?;
        let mut netboot = NetBoot::start(&side, &setup.logs.join("plug.log"))?;
        // Until the first boot, what the console says goes to a scratch log.
        let before = setup.work.join("serial-before.log");
        let console = match &settings.serial {
            Some(port) => {
                netboot.note(&format!("console: serial port {}", port.display()));
                ConsoleReader::Cable(SerialReader::start(port, settings.baud, &before)?)
            }
            None => {
                let machine: IpAddr = settings.machine_address.parse().map_err(|_| {
                    format!("plug.machine_address {} is not an address", settings.machine_address)
                })?;
                let reader = NetlogReader::start(
                    SocketAddr::new(lab, settings.netlog_port),
                    machine,
                    &before,
                )?;
                netboot
                    .note(&format!("console: netlog on UDP port {}", reader.local_addr().port()));
                ConsoleReader::Netlog(reader)
            }
        };
        let capture = match &settings.capture {
            Some(device) => {
                let size = capture::parse_size(&settings.capture_size)?;
                netboot.note(&format!(
                    "screen: capture stick {}, {}x{}",
                    device.display(),
                    size.0,
                    size.1
                ));
                Some(Capture::new(device, size, setup.work.join("capture.yuyv")))
            }
            None => None,
        };
        if let Some(port) = &settings.hid {
            netboot.note(&format!("keyboard and mouse: CH9329 cable {}", port.display()));
        }
        let mut driver = PlugDriver {
            plug: Shelly::new(&settings.shelly),
            settings,
            setup,
            netboot,
            console,
            console_log: before,
            boot: 0,
            power: None,
            last_start: None,
            last_stop: None,
            start_settle: START_SETTLE,
            capture,
            framebuffer: None,
            hid: None,
            pointer: (0, 0),
        };
        driver.note(&format!(
            "plug at {}; net-boot server at {}, ports {}",
            driver.settings.shelly,
            driver.settings.lab_address,
            driver.netboot.ports()
        ));
        Ok(driver)
    }

    /// Whether a reading of the plug is a machine that runs.
    fn runs(&self, status: PlugStatus) -> bool {
        status.output && status.watts >= self.settings.on_watts
    }

    /// Switches the plug, and notes it. An off soon after an on waits,
    /// as [`START_SETTLE`] says.
    fn switch(&mut self, on: bool) -> Result<(), String> {
        self.power = None;
        if let Some(started) = self.last_start
            && !on
            && started.elapsed() < self.start_settle
        {
            let rest = self.start_settle - started.elapsed();
            self.note(&format!(
                "power: waiting {:.1} s after the plug was switched on before switching it off",
                rest.as_secs_f64()
            ));
            std::thread::sleep(rest);
        }
        // A test that pulls the cord and powers the machine on again asks
        // for both at once; switched on 0.1 s after it went off, the M75q's
        // supply never let go, and the machine never started again.
        if let Some(stopped) = self.last_stop
            && on
            && stopped.elapsed() < CYCLE_OFF
        {
            let rest = CYCLE_OFF - stopped.elapsed();
            self.note(&format!(
                "power: waiting {:.1} s after the plug was switched off before switching it on",
                rest.as_secs_f64()
            ));
            std::thread::sleep(rest);
        }
        let word = |on: bool| if on { "on" } else { "off" };
        match self.plug.set(on) {
            Ok(was_on) => {
                self.note(&format!("power: plug {} (it was {})", word(on), word(was_on)));
                if on {
                    self.last_start = Some(Instant::now());
                } else {
                    self.last_stop = Some(Instant::now());
                    self.power = Some((Instant::now(), false));
                }
                Ok(())
            }
            Err(err) => {
                self.note(&format!("power: {err}"));
                Err(err)
            }
        }
    }

    /// Switches the plug on, and waits until the machine draws power: a
    /// wait that took the first reading, before the machine starts, for
    /// the machine going off again would end too soon.
    fn turn_on(&mut self) -> Result<(), String> {
        self.switch(true)?;
        let deadline = Instant::now() + ON_TIME;
        loop {
            let status = self.plug.status()?;
            if self.runs(status) {
                self.power = Some((Instant::now(), true));
                return Ok(());
            }
            if Instant::now() >= deadline || self.setup.stop.requested() {
                return Err(format!(
                    "the machine did not come on: the plug is on, but the machine draws {:.1} W, \
                     under plug.on_watts ({:.1} W) after {} s (is the firmware's After Power \
                     Loss set to Power On?)",
                    status.watts,
                    self.settings.on_watts,
                    ON_TIME.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Cuts the machine's power, which is a hard power loss, if the plug
    /// is on.
    fn cut(&mut self) -> Result<(), String> {
        if self.plug.status()?.output {
            return self.switch(false);
        }
        self.power = Some((Instant::now(), false));
        Ok(())
    }

    /// Cuts the power and turns it on again, [`CYCLE_OFF`] later: a reset.
    fn cycle(&mut self) -> Result<(), String> {
        self.switch(false)?;
        self.turn_on()
    }

    /// Writes the image to the disk through the writer; returns once the
    /// writer said it wrote it and the hash was right.
    fn write_image(&mut self) -> Result<(), String> {
        let image = self.netboot.image(&self.setup.image)?;
        let writer_log = self.setup.logs.join(format!("writer-{}.log", self.boot));
        let cabled = self.console.is_cable();
        let cmdline = if cabled {
            // The writer's console is the cable until it is done, then the
            // boot's log takes over.
            self.console.log().switch_to(&writer_log)?;
            with_console(&self.netboot.writer.cmdline, self.settings.console())
        } else {
            self.console.log().switch_to(&self.console_log)?;
            let events = File::create(&writer_log)
                .map_err(|err| format!("making {}: {err}", writer_log.display()))?;
            self.netboot.copy_events_to(Some(events));
            self.netboot.writer.cmdline.clone()
        };
        let written = {
            let (console, next, script) = (
                self.console.log().clone(),
                self.console_log.clone(),
                self.netboot.script().to_path_buf(),
            );
            // Before the writer is told to restart: its next boot must not
            // find the script, and its console goes to the boot's log.
            Box::new(move || {
                netboot::remove_script(&script);
                // A boot log that cannot be made leaves the boot's lines in
                // the writer's log; the boot itself goes on.
                if cabled {
                    let _ = console.switch_to(&next);
                }
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
            "{cmdline} dukovo.lab={{base}} dukovo.lab.ip={}/24 dukovo.lab.mac={}",
            self.settings.machine_address,
            self.settings.mac.to_ascii_lowercase()
        );
        let booted = self.netboot.begin("the writer", offer, &cmdline).and_then(|()| self.cycle());
        let outcome = booted.and_then(|()| self.netboot.await_writer(&self.setup.stop, WRITE_TIME));
        self.netboot.finish();
        self.netboot.copy_events_to(None);
        match outcome {
            Ok(()) => {
                self.note("net boot: the disk holds the image; the machine restarts into it");
                Ok(())
            }
            Err(why) => {
                // A writer that failed must not be left running, and what
                // the console says next belongs to no writer.
                let _ = self.cut();
                let _ = self.console.log().switch_to(&self.console_log);
                self.note(&format!("net boot: {why}"));
                let held =
                    if cabled { "the writer's console is" } else { "what the writer did is" };
                Err(format!(
                    "{why}; {held} in {}, and what the driver did in plug.log",
                    writer_log.display()
                ))
            }
        }
    }

    /// The screen's size in pixels: as this boot's console log gives it,
    /// or else the size the capture stick's pictures are taken at.
    fn screen_size(&mut self) -> Result<(u32, u32), String> {
        if self.framebuffer.is_none() {
            self.framebuffer = screen::framebuffer_size(&serial::read_log(&self.console_log));
        }
        match (self.framebuffer, &self.capture) {
            (Some(size), _) => Ok(size),
            (None, Some(capture)) => Ok(capture.size()),
            (None, None) => capture::parse_size(&self.settings.capture_size),
        }
    }

    /// The screen now, through the capture stick.
    fn picture(&mut self) -> Result<screen::Image, String> {
        if self.capture.is_none() {
            return Err(NO_SCREEN.into());
        }
        if !self.is_on() {
            return Err("the machine is off, so it has no screen".into());
        }
        let picture = self.capture.as_ref().ok_or(NO_SCREEN)?.frame();
        if let Err(err) = &picture {
            self.note(&format!("screen: {err}"));
        }
        picture
    }

    /// Does `act` through the CH9329 cable once the machine has set up its
    /// keyboard and mouse, opening the cable the first time. A cable that
    /// fails is closed, and the next action opens it again.
    fn with_hid<T>(
        &mut self,
        act: impl FnOnce(&mut Ch9329) -> Result<T, String>,
    ) -> Result<T, String> {
        let Some(port) = self.settings.hid.clone() else {
            return Err(NO_KEYBOARD.into());
        };
        if !self.is_on() {
            return Err("the machine is off, so it has no keyboard or mouse".into());
        }
        if self.hid.is_none() {
            let opened = Ch9329::open(&port, self.settings.hid_baud);
            if let Err(err) = &opened {
                self.note(&format!("keyboard and mouse: {err}"));
            }
            self.hid = Some(opened?);
        }
        let Some(hid) = self.hid.as_mut() else {
            return Err(NO_KEYBOARD.into());
        };
        let done = await_set_up(hid, &self.setup.stop).and_then(|()| act(hid));
        if let Err(err) = &done {
            self.hid = None;
            self.note(&format!("keyboard and mouse: {err}"));
        }
        done
    }
}

/// Waits up to [`HID_SETTLE`] for the machine to set up the CH9329's
/// keyboard and mouse: keys sent before then are lost.
fn await_set_up(hid: &mut Ch9329, stop: &crate::stop::Stop) -> Result<(), String> {
    let deadline = Instant::now() + HID_SETTLE;
    loop {
        let why = match hid.info() {
            Ok(info) if info.enumerated => return Ok(()),
            Ok(_) => format!(
                "the machine has not set up the CH9329 cable's keyboard and mouse within {} s: \
                 is the cable's USB end plugged into the machine, and has it started?",
                HID_SETTLE.as_secs_f64()
            ),
            Err(err) => err,
        };
        if Instant::now() >= deadline || stop.requested() {
            return Err(why);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// What the machine file says about the lab network.
fn lab_side(settings: &PlugSettings) -> LabSide<'_> {
    LabSide {
        lab_address: &settings.lab_address,
        boot_port: settings.boot_port,
        tftp: &settings.tftp,
        mac: &settings.mac,
        writer: &settings.writer,
    }
}

/// Checks what a run on the machine needs, without changing anything:
/// the plug, the netlog port or the serial port, the capture stick and
/// the CH9329 cable when the machine file names them, the lab network,
/// the TFTP folder and network GRUB, the writer's kit and the lab key.
pub fn check(settings: &PlugSettings, key: Option<&Path>) -> Vec<Check> {
    let status = Shelly::new(&settings.shelly).status();
    let on = status.as_ref().is_ok_and(|status| status.output && status.watts >= settings.on_watts);
    let mut checks = vec![found(
        "plug",
        status
            .map(|status| {
                format!(
                    "the plug at {} answers; its switch is {} and the machine draws {:.1} W",
                    settings.shelly,
                    if status.output { "on" } else { "off" },
                    status.watts
                )
            })
            .map_err(problem),
    )];
    checks.push(found(
        "power button",
        Err((
            Verdict::Note,
            "a plug cannot ask the system to shut down, so steps that press the power button \
             or shut down fail on this machine"
                .into(),
        )),
    ));
    checks.extend(check_lab_side(&lab_side(settings), key));
    match &settings.serial {
        Some(port) => checks.push(found(
            "serial port",
            serial::check_port(port, settings.baud)
                .map(|()| format!("{} opens, at {} baud", port.display(), settings.baud))
                .map_err(problem),
        )),
        None => {
            let port = settings.netlog_port;
            checks.push(found(
                "netlog port",
                std::net::UdpSocket::bind((settings.lab_address.as_str(), port))
                    .map(|_| format!("UDP port {port} is free for the netlog reader"))
                    .map_err(|err| {
                        (
                            Verdict::Note,
                            format!("port {port} is in use ({err}): by a run going now?"),
                        )
                    }),
            ));
        }
    }
    if let Some(device) = &settings.capture {
        checks.push(found("capture", check_capture(device, &settings.capture_size, on)));
    }
    if let Some(port) = &settings.hid {
        checks.push(found("keyboard and mouse", check_hid(port, settings.hid_baud, on)));
    }
    checks
}

/// Whether the capture stick takes a picture, of the size asked for, and
/// whether anything is lit in it.
fn check_capture(device: &Path, size: &str, on: bool) -> Result<String, (Verdict, String)> {
    let (width, height) = capture::parse_size(size).map_err(problem)?;
    let scratch =
        std::env::temp_dir().join(format!("dukovo-lab-check-{}.yuyv", std::process::id()));
    let picture = match Capture::new(device, (width, height), scratch).frame() {
        Ok(picture) => picture,
        Err(err) if !on => {
            return Err((
                Verdict::Note,
                format!("{err}; the machine is off, so check again with it on"),
            ));
        }
        Err(err) => return Err(problem(err)),
    };
    let lit = capture::lit_share(&picture);
    if lit > 0.0 {
        return Ok(format!(
            "{} takes {width}x{height} pictures; {:.0}% of this one is lit",
            device.display(),
            lit * 100.0
        ));
    }
    let why = if on {
        "the machine is on: is the HDMI cable in the board's own video port, with no graphics \
         card fitted?"
    } else {
        "the machine is off"
    };
    Err((
        Verdict::Note,
        format!(
            "{} takes {width}x{height} pictures, but this one is black: {why}",
            device.display()
        ),
    ))
}

/// Whether the CH9329 cable answers, and whether the machine has set up
/// its keyboard and mouse.
fn check_hid(port: &Path, baud: u32, on: bool) -> Result<String, (Verdict, String)> {
    let mut hid = Ch9329::open(port, baud).map_err(problem)?;
    match hid.info() {
        Ok(info) if info.enumerated => Ok(format!(
            "the CH9329 cable at {} answers (version {:#04x}), and the machine has set up its \
             keyboard and mouse",
            port.display(),
            info.version
        )),
        Ok(info) => Err((
            Verdict::Note,
            format!(
                "the CH9329 cable at {} answers (version {:#04x}), but the machine has not set \
                 up its keyboard and mouse: {}",
                port.display(),
                info.version,
                if on { "is the cable's USB end plugged into the machine?" } else { "it is off" }
            ),
        )),
        Err(err) if !on => {
            Err((Verdict::Note, format!("{err}; the machine is off, so check again with it on")))
        }
        Err(err) => Err(problem(err)),
    }
}

impl Driver for PlugDriver {
    fn boot_fresh(&mut self) -> Result<(), String> {
        self.boot += 1;
        self.console_log = self.setup.logs.join(format!("serial-{}.log", self.boot));
        self.framebuffer = None;
        self.note(&format!("boot {}: afresh", self.boot));
        self.cut()?;
        self.write_image()
    }

    fn boot_stick(&mut self, _stick: &Path, _disk: u64) -> Result<(), String> {
        Err("real machines do not boot the install stick yet; installer tests run on QEMU".into())
    }

    fn boot_dvd(&mut self, _dvd: &Path, _disk: u64) -> Result<(), String> {
        Err("real machines do not boot the install DVD yet; installer tests run on QEMU".into())
    }

    fn power(&mut self, action: PowerAction, _deadline: Instant) -> Result<(), String> {
        let graceful = || {
            Err("a plug cannot ask the system to shut down: it can only cut the power, as \
                 `power = \"cut\"` does"
                .into())
        };
        match action {
            PowerAction::On => {
                let status = self.plug.status()?;
                if !status.output {
                    return self.turn_on();
                }
                if self.runs(status) {
                    return Ok(());
                }
                self.note(&format!(
                    "power: the plug is on but the machine draws {:.1} W, so it shut itself \
                     down; cycling the power",
                    status.watts
                ));
                self.cycle()
            }
            PowerAction::Off | PowerAction::Button => graceful(),
            PowerAction::Reset => self.cycle(),
            PowerAction::Cut => self.cut(),
        }
    }

    fn is_on(&mut self) -> bool {
        if let Some((read, on)) = self.power
            && read.elapsed() < POWER_FRESH
        {
            return on;
        }
        match self.plug.status() {
            Ok(status) => {
                let on = self.runs(status);
                self.power = Some((Instant::now(), on));
                on
            }
            Err(err) => {
                // As it was last seen, or on when nothing has been read
                // since a power change: waits go on to their deadlines, and
                // only the plug's word ends one as off.
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

    /// Writes a line to plug.log, with the seconds since the driver began.
    fn note(&mut self, line: &str) {
        self.netboot.note(line);
    }

    fn serial_log(&self) -> &Path {
        &self.console_log
    }

    fn set_link(&mut self, _up: bool) -> Result<(), String> {
        Err(persons_job("plug", "plug or pull the network cable"))
    }

    fn plug_usb(&mut self, _stick: &Stick) -> Result<(), String> {
        Err(persons_job("plug", "plug in a USB stick"))
    }

    fn unplug_usb(&mut self) -> Result<(), String> {
        Err(persons_job("plug", "pull out a USB stick"))
    }

    fn press_keys(&mut self, combos: &[String]) -> Result<(), String> {
        // Every name is checked before any key goes down.
        let combos =
            combos.iter().map(|combo| ch9329::keys(combo)).collect::<Result<Vec<_>, _>>()?;
        self.with_hid(|hid| {
            for keys in &combos {
                hid.press(keys, KEY_HOLD)?;
                std::thread::sleep(KEY_GAP);
            }
            Ok(())
        })
    }

    fn point(&mut self, x: i32, y: i32) -> Result<(), String> {
        let (width, height) = self.screen_size()?;
        let (Some(at_x), Some(at_y)) = (super::pixel(x, width), super::pixel(y, height)) else {
            return Err(format!("({x}, {y}) is off the screen, which is {width}x{height} pixels"));
        };
        let at = (ch9329::absolute(at_x, width), ch9329::absolute(at_y, height));
        self.with_hid(|hid| hid.pointer(0, at.0, at.1))?;
        self.pointer = at;
        Ok(())
    }

    fn click(&mut self, button: Button) -> Result<(), String> {
        let held = match button {
            Button::Left => 1,
            Button::Right => 2,
            Button::Middle => 4,
        };
        let (x, y) = self.pointer;
        self.with_hid(|hid| {
            for buttons in [held, 0] {
                hid.pointer(buttons, x, y)?;
                std::thread::sleep(CLICK_HOLD);
            }
            Ok(())
        })
    }

    fn screenshot(&mut self, to: &Path) -> Result<(), String> {
        let picture = self.picture()?;
        fs::write(to, png::encode(&picture)).map_err(|err| format!("{}: {err}", to.display()))
    }

    fn screen_dump(&mut self, to: &Path) -> Result<(), String> {
        let picture = self.picture()?;
        // A picture the stick scaled has no pixel of the machine's own, and
        // no text could be read from it.
        if let (Ok(screen), Some(capture)) = (self.screen_size(), &self.capture)
            && screen != capture.size()
        {
            let (width, height) = screen;
            let (taken_width, taken_height) = capture.size();
            return Err(format!(
                "the machine's screen is {width}x{height}, as its console log says, but the \
                 capture stick {} takes pictures of {taken_width}x{taken_height}: set \
                 plug.capture_size to \"{width}x{height}\" in the machine file",
                capture.device().display()
            ));
        }
        fs::write(to, picture.to_ppm()).map_err(|err| format!("{}: {err}", to.display()))
    }

    fn screen_noise(&self) -> u8 {
        if self.capture.is_some() { capture::NOISE } else { 0 }
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
        WRITE_TIME
    }

    fn twin_ends_boot(&self) -> bool {
        true
    }

    fn linux_twin(&mut self, kit: &Kit, log: &Path, deadline: Instant) -> Result<String, String> {
        if !self.console.is_cable() {
            return Err("the plug driver reads the Linux twin's numbers from a serial cable, and \
                        the machine file names none (plug.serial): its linux control is \"none\""
                .into());
        }
        self.note("Linux twin: booting");
        self.cut()?;
        self.console.log().switch_to(log)?;
        let cmdline = with_console(&kit.cmdline, self.settings.console());
        let booted = self
            .netboot
            .begin("the Linux twin", Offer::Twin { kit: kit.clone() }, &cmdline)
            .and_then(|()| self.cycle());
        let measured = booted.and_then(|()| real::await_twin(kit, log, deadline, &self.setup.stop));
        self.netboot.finish();
        // Linux is not left running whatever happened; the runner boots
        // afresh after the twin, which cuts the power again if this did not.
        let _ = self.cut();
        let _ = self.console.log().switch_to(&self.setup.work.join("serial-after-twin.log"));
        self.note(&format!("Linux twin: {}", if measured.is_ok() { "measured" } else { "failed" }));
        measured
    }

    fn emulated(&self) -> bool {
        false
    }

    fn shut_off(&mut self) {
        self.hid = None;
        self.netboot.finish();
        if let Err(err) = self.cut() {
            self.note(&format!("shut off: {err}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ch9329::fake::{Event, FakeCh9329};
    use crate::netboot::tests::{make_image, pretend_writer};
    use crate::shelly::fake::FakeShelly;
    use crate::stop::Stop;
    use crate::testing::TempDir;
    use std::fs::OpenOptions;
    use std::io::Write as _;
    use std::net::UdpSocket;

    const MAC: &str = "aa:bb:cc:00:11:22";

    /// A machine behind a pretend plug: switching the plug on runs the
    /// pretend writer when a boot script is there, and else boots the
    /// disk, which sends its ready line to the lab PC as netlog does, or
    /// over the serial cable when it has one.
    struct Bench {
        dir: TempDir,
        fake: FakeShelly,
        driver: PlugDriver,
        /// The CH9329 at the far end of the driver's cable, when it has one.
        chip: Option<FakeCh9329>,
    }

    /// The cables from the pretend machine to the lab PC.
    #[derive(Clone, Copy, Default)]
    struct Cables {
        /// A serial cable on COM1, which is a plain file here.
        serial: bool,
        /// A capture stick of 4x2 pixels, whose `v4l2-ctl` the test sets.
        capture: bool,
        /// A CH9329 cable, with a pretend chip the machine has set up.
        hid: bool,
    }

    /// A UDP port nothing listens on now.
    fn free_udp_port() -> u16 {
        UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    /// Sends `text` to `port` as a machine's netlog does.
    fn netlog_say(port: u16, text: &str) {
        let line = format!("<6>[    1.000000] {text}");
        UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .send_to(line.as_bytes(), ("127.0.0.1", port))
            .unwrap();
    }

    fn bench(label: &str, tamper: bool) -> Bench {
        bench_with(label, tamper, Cables::default())
    }

    fn bench_with(label: &str, tamper: bool, cables: Cables) -> Bench {
        let dir = TempDir::new(label);
        let port = dir.write("ttyUSB0", "");
        dir.write("writer/kernel", "KERNEL");
        dir.write("writer/initrd", "INITRD");
        dir.write(
            "writer/kit.toml",
            "kernel = \"kernel\"\ninitrd = \"initrd\"\ncmdline = \"console=ttyS0 rdinit=/writer\"\n\
             done = \"writer: done\"\n",
        );
        let fake = FakeShelly::start();
        let tftp = dir.path().join("tftp");
        let disk = dir.path().join("disk");
        let netlog_port = free_udp_port();
        {
            let (tftp, disk, port) = (tftp.clone(), disk.clone(), port.clone());
            fake.state.lock().unwrap().on_switch_on = Some(Box::new(move || {
                let (tftp, disk, port) = (tftp.clone(), disk.clone(), port.clone());
                // As the machine would: the firmware, then what it boots.
                std::thread::spawn(move || {
                    let say = |text: &str| {
                        if cables.serial {
                            let mut port = OpenOptions::new().append(true).open(&port).unwrap();
                            port.write_all(format!("{text}\n").as_bytes()).unwrap();
                        } else {
                            netlog_say(netlog_port, text);
                        }
                    };
                    let Ok(script) = fs::read_to_string(netboot::script_path(&tftp, MAC)) else {
                        say("Boot complete.");
                        return;
                    };
                    let Some(base) =
                        script.split_whitespace().find_map(|word| word.strip_prefix("dukovo.lab="))
                    else {
                        say("BENCH name=spawn ns=120.0");
                        say("linux-bench: done");
                        return;
                    };
                    // Kept for the test to read: the boot script is gone
                    // once the boot ends.
                    fs::write(tftp.with_file_name("writer-script"), &script).unwrap();
                    // The writer has a console only on the cable.
                    if cables.serial {
                        say("writer: writing");
                    }
                    if pretend_writer(base, &disk, tamper).unwrap() == "restart" {
                        say("Boot complete.");
                    }
                });
            }));
        }
        let settings = PlugSettings {
            shelly: fake.address.to_string(),
            password_file: None,
            mac: MAC.into(),
            disk: "TESTDISK".into(),
            machine_address: "127.0.0.1".into(),
            lab_address: "127.0.0.1".into(),
            boot_port: 0,
            tftp,
            writer: dir.path().join("writer"),
            netlog_port,
            on_watts: 5.0,
            grow_root: true,
            serial: cables.serial.then_some(port),
            baud: 115_200,
            console: None,
            capture: cables.capture.then(|| PathBuf::from("/dev/video9")),
            capture_size: if cables.capture { "4x2" } else { "1920x1080" }.into(),
            hid: cables.hid.then(|| dir.path().join("ch9329")),
            hid_baud: 9600,
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
        let mut driver = PlugDriver::new(settings, setup).unwrap();
        // Tests of the wait after a start set their own.
        driver.start_settle = Duration::ZERO;
        let chip = cables.hid.then(|| {
            let (chip, port) = FakeCh9329::start();
            driver.hid = Some(Ch9329::over(port, "the test cable"));
            chip
        });
        Bench { dir, fake, driver, chip }
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

    /// The switch changes made so far.
    fn sets(fake: &FakeShelly) -> Vec<String> {
        fake.calls().into_iter().filter(|call| call.starts_with("Set")).collect()
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    #[test]
    fn a_fresh_boot_writes_the_image_by_cycling_the_power() {
        let mut bench = bench("plug-fresh", false);
        bench.driver.boot_fresh().unwrap();
        assert!(bench.driver.grows_root());
        let serial = bench.driver.serial_log().to_path_buf();
        assert!(serial.ends_with("logs/serial-1.log"));
        assert!(wait_for(&serial, "[    1.000000] Boot complete."));
        let disk = fs::read(bench.dir.path().join("disk")).unwrap();
        assert_eq!(disk, fs::read(&bench.driver.setup.image).unwrap());
        // The writer's log holds what the net-boot server saw, the boot
        // script is gone so the next boot is the disk's, and the machine's
        // console log holds none of the writer's.
        let writer_log = fs::read_to_string(bench.dir.path().join("logs/writer-1.log")).unwrap();
        assert!(writer_log.contains("GET plan"), "{writer_log}");
        assert!(writer_log.contains("POST done"), "{writer_log}");
        assert!(!fs::read_to_string(&serial).unwrap().contains("GET plan"));
        assert!(!netboot::script_path(&bench.driver.settings.tftp, MAC).exists());
        // The writer is told its own address and its card's MAC.
        let script = fs::read_to_string(bench.dir.path().join("writer-script")).unwrap();
        assert!(
            script.contains(&format!("dukovo.lab.ip=127.0.0.1/24 dukovo.lab.mac={MAC}")),
            "{script}"
        );
        assert!(bench.driver.is_on());
        // The plug was off: it is cycled once (an off, then an on).
        assert_eq!(sets(&bench.fake), ["Set on=false", "Set on=true"]);

        // A second fresh boot cuts the running machine first, then cycles.
        bench.driver.boot_fresh().unwrap();
        assert!(bench.driver.serial_log().ends_with("logs/serial-2.log"));
        assert!(wait_for(bench.driver.serial_log(), "Boot complete."));
        let sets = sets(&bench.fake);
        assert_eq!(sets[2..], ["Set on=false", "Set on=false", "Set on=true"]);
        let log = fs::read_to_string(bench.dir.path().join("logs/plug.log")).unwrap();
        assert!(log.contains("net boot: GET plan"), "{log}");
        assert_eq!(bench.driver.peer_address().as_deref(), Some("127.0.0.1"));
        assert_eq!(bench.driver.reach(445, false), Ok(445));
    }

    #[test]
    fn a_disk_that_reads_back_wrong_fails_the_boot_and_cuts_the_power() {
        let mut bench = bench("plug-tamper", true);
        let err = bench.driver.boot_fresh().unwrap_err();
        assert!(err.contains("did not hold the image"), "{err}");
        assert!(err.contains("writer-1.log"), "{err}");
        assert!(!bench.driver.is_on());
        assert!(!bench.fake.state.lock().unwrap().output);
    }

    #[test]
    fn reset_is_off_then_on_and_a_cut_leaves_it_off() {
        let mut bench = bench("plug-power", false);
        bench.driver.power(PowerAction::On, soon()).unwrap();
        assert!(bench.driver.is_on());
        assert_eq!(sets(&bench.fake), ["Set on=true"]);
        // Already on and running: nothing to do.
        bench.driver.power(PowerAction::On, soon()).unwrap();
        assert_eq!(sets(&bench.fake).len(), 1);

        bench.driver.power(PowerAction::Reset, soon()).unwrap();
        assert!(bench.driver.is_on());
        assert_eq!(sets(&bench.fake)[1..], ["Set on=false", "Set on=true"]);

        bench.driver.power(PowerAction::Cut, soon()).unwrap();
        assert!(!bench.driver.is_on());
        assert_eq!(sets(&bench.fake).last().map(String::as_str), Some("Set on=false"));
        // A cut of a machine that is off is no change.
        let before = sets(&bench.fake).len();
        bench.driver.power(PowerAction::Cut, soon()).unwrap();
        assert_eq!(sets(&bench.fake).len(), before);
    }

    #[test]
    fn a_power_on_right_after_a_cut_waits_until_the_plug_has_been_off_long_enough() {
        let mut bench = bench("plug-cut-on", false);
        bench.driver.power(PowerAction::On, soon()).unwrap();
        bench.driver.power(PowerAction::Cut, soon()).unwrap();
        let cut = Instant::now();
        bench.driver.power(PowerAction::On, soon()).unwrap();
        assert!(cut.elapsed() >= CYCLE_OFF, "on after {:?}", cut.elapsed());
        assert_eq!(sets(&bench.fake)[1..], ["Set on=false", "Set on=true"]);
    }

    #[test]
    fn the_machine_is_on_when_it_draws_power() {
        let mut bench = bench("plug-watts", false);
        bench.fake.state.lock().unwrap().output = true;
        bench.fake.state.lock().unwrap().draws = 2.0;
        assert!(!bench.driver.is_on());
        // On with the plug on and the machine shut down cycles the power.
        bench.fake.state.lock().unwrap().draws = 33.0;
        bench.driver.power(PowerAction::On, soon()).unwrap();
        assert!(sets(&bench.fake).is_empty());
        bench.fake.state.lock().unwrap().draws = 2.0;
        assert!(bench.driver.power(PowerAction::On, soon()).is_err());
        assert_eq!(sets(&bench.fake), ["Set on=false", "Set on=true"]);
        let log = fs::read_to_string(bench.dir.path().join("logs/plug.log")).unwrap();
        assert!(log.contains("it shut itself down"), "{log}");
    }

    #[test]
    fn a_machine_that_does_not_start_says_why() {
        let mut bench = bench("plug-no-start", false);
        bench.fake.state.lock().unwrap().draws = 0.5;
        let err = bench.driver.power(PowerAction::On, soon()).unwrap_err();
        assert!(err.contains("the machine did not come on"), "{err}");
        assert!(err.contains("After Power Loss"), "{err}");
    }

    #[test]
    fn the_power_button_and_a_graceful_off_are_refused() {
        let mut bench = bench("plug-graceful", false);
        for action in [PowerAction::Off, PowerAction::Button] {
            let err = bench.driver.power(action, soon()).unwrap_err();
            assert!(err.contains("cannot ask the system to shut down"), "{err}");
        }
        assert!(bench.fake.calls().is_empty());
    }

    #[test]
    fn a_plug_that_does_not_answer_fails_with_its_address() {
        let mut bench = bench("plug-silent", false);
        bench.driver.power(PowerAction::On, soon()).unwrap();
        bench.fake.state.lock().unwrap().hang_ups = 100;
        let err = bench.driver.power(PowerAction::Cut, soon()).unwrap_err();
        assert!(err.contains(&bench.fake.address.to_string()), "{err}");
        // As the machine was last seen: waits go on to their deadlines.
        std::thread::sleep(POWER_FRESH);
        assert!(bench.driver.is_on());
        let log = fs::read_to_string(bench.dir.path().join("logs/plug.log")).unwrap();
        assert!(log.contains("power state: the Shelly plug at"), "{log}");
    }

    #[test]
    fn a_change_waits_a_while_after_the_plug_is_switched_on() {
        let mut bench = bench("plug-settle", false);
        bench.driver.start_settle = Duration::from_millis(800);
        bench.driver.power(PowerAction::On, soon()).unwrap();
        let on = Instant::now();
        bench.driver.power(PowerAction::Cut, soon()).unwrap();
        assert!(on.elapsed() >= Duration::from_millis(700), "{:?}", on.elapsed());
        // An on after an off does not wait.
        let off = Instant::now();
        bench.driver.power(PowerAction::On, soon()).unwrap();
        assert!(off.elapsed() < Duration::from_millis(700), "{:?}", off.elapsed());
    }

    #[test]
    fn what_a_plug_machine_cannot_do_says_so() {
        let mut bench = bench("plug-none", false);
        // Without its cable, each control says which key names the cable.
        assert!(bench.driver.press_keys(&["a".into()]).unwrap_err().contains("(plug.hid)"));
        assert!(bench.driver.click(Button::Left).unwrap_err().contains("(plug.hid)"));
        let err = bench.driver.screen_dump(Path::new("x.ppm")).unwrap_err();
        assert!(err.contains("(plug.capture)"), "{err}");
        assert_eq!(bench.driver.screen_noise(), 0);
        let kit = Kit::read(&bench.dir.path().join("writer")).unwrap();
        let log = bench.dir.path().join("logs/linux-boot-1.log");
        let err = bench.driver.linux_twin(&kit, &log, soon()).unwrap_err();
        assert!(err.contains("(plug.serial)"), "{err}");
        assert!(sets(&bench.fake).is_empty());
        let err = bench.driver.set_link(false).unwrap_err();
        assert!(err.contains("the plug driver cannot plug or pull the network cable"), "{err}");
        let err = bench.driver.run_command("true", None, soon(), None).unwrap_err();
        assert!(err.contains("the lab has no key"), "{err}");
    }

    #[test]
    fn checks_what_a_run_needs() {
        let bench = bench("plug-check", false);
        // The driver holds the netlog port; another is free.
        let checks = check(&bench.driver.settings, None);
        let verdict =
            |name: &str| checks.iter().find(|check| check.check == name).map(|check| check.verdict);
        assert_eq!(verdict("plug"), Some(Verdict::Ok));
        assert_eq!(verdict("power button"), Some(Verdict::Note));
        assert_eq!(verdict("lab network"), Some(Verdict::Ok));
        assert_eq!(verdict("tftp"), Some(Verdict::Ok));
        assert_eq!(verdict("netlog port"), Some(Verdict::Note));
        // No network GRUB in this pretend TFTP folder, and no lab key.
        assert_eq!(verdict("network grub"), Some(Verdict::Problem));
        assert_eq!(verdict("writer"), Some(Verdict::Ok));
        assert_eq!(verdict("lab key"), Some(Verdict::Problem));
        let mut elsewhere = bench.driver.settings.clone();
        elsewhere.netlog_port = free_udp_port();
        let checks = check(&elsewhere, None);
        let netlog = checks.iter().find(|check| check.check == "netlog port").unwrap();
        assert_eq!(netlog.verdict, Verdict::Ok);
        // Checking leaves nothing behind in the TFTP folder.
        let boot = bench.driver.settings.tftp.join("dukovo-lab/boot");
        assert_eq!(fs::read_dir(boot).unwrap().count(), 0);
        // A plug that does not answer is a problem.
        bench.fake.state.lock().unwrap().hang_ups = 100;
        let checks = check(&bench.driver.settings, None);
        let plug = checks.iter().find(|check| check.check == "plug").unwrap();
        assert_eq!(plug.verdict, Verdict::Problem);
    }

    #[test]
    fn a_serial_cable_carries_the_writer_s_console_then_the_boot_s() {
        let cables = Cables { serial: true, ..Cables::default() };
        let mut bench = bench_with("plug-serial", false, cables);
        bench.driver.boot_fresh().unwrap();
        let serial = bench.driver.serial_log().to_path_buf();
        assert!(wait_for(&serial, "Boot complete."));
        let boot = fs::read_to_string(&serial).unwrap();
        assert!(!boot.contains("writer: writing"), "{boot}");
        let writer_log = fs::read_to_string(bench.dir.path().join("logs/writer-1.log")).unwrap();
        assert!(writer_log.contains("writer: writing"), "{writer_log}");
        assert!(!writer_log.contains("Boot complete."), "{writer_log}");
        // The writer's console is the cable's, at the cable's speed.
        let script = fs::read_to_string(bench.dir.path().join("writer-script")).unwrap();
        assert!(script.contains("rdinit=/writer console=ttyS0,115200 "), "{script}");
        let log = fs::read_to_string(bench.dir.path().join("logs/plug.log")).unwrap();
        assert!(log.contains("console: serial port"), "{log}");
    }

    #[test]
    fn a_writer_that_fails_on_the_cable_names_its_console() {
        let cables = Cables { serial: true, ..Cables::default() };
        let mut bench = bench_with("plug-serial-tamper", true, cables);
        let err = bench.driver.boot_fresh().unwrap_err();
        assert!(err.contains("the writer's console is in"), "{err}");
        assert!(err.contains("writer-1.log"), "{err}");
        assert!(!bench.driver.is_on());
    }

    #[test]
    fn the_linux_twin_is_the_machine_net_booted_with_its_console_on_the_cable() {
        let cables = Cables { serial: true, ..Cables::default() };
        let mut bench = bench_with("plug-twin", false, cables);
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

    /// A capture stick bench whose stand-in `v4l2-ctl` copies
    /// `prepared.yuyv`, which holds a grey picture of 4x2 pixels.
    fn capture_bench(label: &str) -> Bench {
        let cables = Cables { capture: true, ..Cables::default() };
        let mut bench = bench_with(label, false, cables);
        let program =
            capture::tests::stand_in(&bench.dir, &capture::tests::copying(bench.dir.path()));
        bench.driver.capture.as_mut().unwrap().program = program;
        // Y 126 is grey 128 in full range.
        fs::write(bench.dir.path().join("prepared.yuyv"), [126, 128, 126, 128].repeat(4)).unwrap();
        bench
    }

    #[test]
    fn the_screen_is_the_capture_stick_s_picture() {
        let mut bench = capture_bench("plug-capture");
        let dump = bench.dir.path().join("screen.ppm");
        let err = bench.driver.screen_dump(&dump).unwrap_err();
        assert!(err.contains("the machine is off"), "{err}");
        bench.driver.power(PowerAction::On, soon()).unwrap();
        bench.driver.screen_dump(&dump).unwrap();
        let image = screen::Image::parse_ppm(&fs::read(&dump).unwrap()).unwrap();
        assert_eq!((image.width, image.height), (4, 2));
        assert!(image.pixels().iter().all(|&pixel| pixel == [128, 128, 128]));
        let shot = bench.dir.path().join("screen.png");
        bench.driver.screenshot(&shot).unwrap();
        assert!(fs::read(&shot).unwrap().starts_with(b"\x89PNG\r\n\x1a\n"));
        // The text reader allows for the stick's noise.
        assert_eq!(bench.driver.screen_noise(), capture::NOISE);
        let log = fs::read_to_string(bench.dir.path().join("logs/plug.log")).unwrap();
        assert!(log.contains("screen: capture stick /dev/video9, 4x2"), "{log}");
    }

    #[test]
    fn a_picture_of_another_size_than_the_screen_is_refused_naming_the_fix() {
        let mut bench = capture_bench("plug-capture-size");
        bench.driver.power(PowerAction::On, soon()).unwrap();
        let port = bench.driver.settings.netlog_port;
        netlog_say(port, "Framebuffer: 8x6, 32 bits per pixel, pitch 32, at 0xe0000000");
        assert!(wait_for(bench.driver.serial_log(), "Framebuffer: 8x6"));
        let err = bench.driver.screen_dump(&bench.dir.path().join("screen.ppm")).unwrap_err();
        assert!(err.contains("the machine's screen is 8x6"), "{err}");
        assert!(err.contains("capture stick /dev/video9 takes pictures of 4x2"), "{err}");
        assert!(err.contains("set plug.capture_size to \"8x6\""), "{err}");
        // A screenshot is only looked at, so any size will do.
        bench.driver.screenshot(&bench.dir.path().join("screen.png")).unwrap();
        // A fresh boot reads the size again from its own log.
        bench.driver.framebuffer = Some((8, 6));
        bench.driver.boot_fresh().unwrap();
        assert_eq!(bench.driver.framebuffer, None);
    }

    #[test]
    fn a_stick_that_fails_is_noted_in_the_driver_s_log() {
        let mut bench = capture_bench("plug-capture-fails");
        bench.driver.capture.as_mut().unwrap().program = bench.dir.path().join("no-such-program");
        bench.driver.power(PowerAction::On, soon()).unwrap();
        let err = bench.driver.screenshot(&bench.dir.path().join("screen.png")).unwrap_err();
        assert!(err.contains("v4l2-ctl is not installed"), "{err}");
        let log = fs::read_to_string(bench.dir.path().join("logs/plug.log")).unwrap();
        assert!(log.contains("screen: v4l2-ctl is not installed"), "{log}");
    }

    #[test]
    fn keys_and_the_mouse_go_through_the_ch9329_cable() {
        let cables = Cables { hid: true, ..Cables::default() };
        let mut bench = bench_with("plug-hid", false, cables);
        let err = bench.driver.press_keys(&["a".into()]).unwrap_err();
        assert!(err.contains("the machine is off"), "{err}");
        bench.driver.power(PowerAction::On, soon()).unwrap();
        // A name the keyboard lacks is refused before any key goes down.
        let err = bench.driver.press_keys(&["a".into(), "nosuchkey".into()]).unwrap_err();
        assert!(err.contains("nosuchkey"), "{err}");
        let chip = bench.chip.take().unwrap();
        assert!(chip.events().is_empty());

        bench.driver.press_keys(&["shift-a".into()]).unwrap();
        let shift = 0x02;
        let report = |modifiers, first| Event::Keyboard { modifiers, keys: [first, 0, 0, 0, 0, 0] };
        assert_eq!(
            chip.events(),
            [Event::Info, report(shift, 0), report(shift, 4), report(shift, 0), report(0, 0)]
        );
        // No framebuffer line yet: the screen is plug.capture_size's.
        bench.driver.point(1919, 0).unwrap();
        bench.driver.click(Button::Right).unwrap();
        let events = chip.events();
        assert_eq!(
            events[5..],
            [
                Event::Info,
                Event::Pointer { buttons: 0, x: 4095, y: 0 },
                Event::Info,
                Event::Pointer { buttons: 2, x: 4095, y: 0 },
                Event::Pointer { buttons: 0, x: 4095, y: 0 },
            ]
        );
        let err = bench.driver.point(1920, 0).unwrap_err();
        assert!(err.contains("off the screen, which is 1920x1080"), "{err}");
        let log = fs::read_to_string(bench.dir.path().join("logs/plug.log")).unwrap();
        assert!(log.contains("keyboard and mouse: CH9329 cable"), "{log}");
    }

    #[test]
    fn keys_wait_for_the_machine_to_set_the_cable_up_and_then_say_why() {
        let cables = Cables { hid: true, ..Cables::default() };
        let mut bench = bench_with("plug-hid-unset", false, cables);
        bench.driver.power(PowerAction::On, soon()).unwrap();
        let chip = bench.chip.take().unwrap();
        chip.state.lock().unwrap().enumerated = false;
        let asked = Instant::now();
        let err = bench.driver.press_keys(&["a".into()]).unwrap_err();
        assert!(asked.elapsed() >= HID_SETTLE, "{:?}", asked.elapsed());
        assert!(err.contains("has not set up the CH9329 cable's keyboard and mouse"), "{err}");
        assert!(chip.events().iter().all(|event| *event == Event::Info));
        // The failed cable is let go; the next action opens it again, and
        // this one is not there.
        assert!(bench.driver.hid.is_none());
        let err = bench.driver.press_keys(&["a".into()]).unwrap_err();
        assert!(err.contains("dialout"), "{err}");
        let log = fs::read_to_string(bench.dir.path().join("logs/plug.log")).unwrap();
        assert!(log.contains("keyboard and mouse: the machine has not set up"), "{log}");
    }

    #[test]
    fn checks_each_cable_the_machine_file_names() {
        let cables = Cables { serial: true, capture: true, hid: true };
        let bench = bench_with("plug-check-cables", false, cables);
        let checks = check(&bench.driver.settings, None);
        let found = |name: &str| {
            checks.iter().find(|check| check.check == name).unwrap_or_else(|| panic!("{name}"))
        };
        // The serial cable replaces the netlog.
        assert_eq!(found("serial port").verdict, Verdict::Ok);
        assert!(checks.iter().all(|check| check.check != "netlog port"));
        // With the machine off, a stick that takes no picture is a note.
        assert_eq!(found("capture").verdict, Verdict::Note, "{}", found("capture").detail);
        assert!(found("capture").detail.contains("check again with it on"));
        // A cable that is not there is a problem whether or not the
        // machine is on.
        assert_eq!(found("keyboard and mouse").verdict, Verdict::Problem);
        assert!(found("keyboard and mouse").detail.contains("dialout"));
    }
}

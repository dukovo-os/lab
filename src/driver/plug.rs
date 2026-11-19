// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The plug driver: a real machine with neither AMT nor a serial port,
//! on the lab network, powered through a Shelly smart plug.
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
//!   no console of its own, so `writer-<n>.log` holds what the net-boot
//!   server saw it do.
//! - **SSH** and the lab PC's side of a test reach the machine on the lab
//!   network, where it has an address of its own.
//! - **The screen, keyboard, mouse and Linux twin** are not done: the
//!   machine file says `none`, and tests that need them are skipped.
//!
//! Cables, sticks, cards and discs are a person's to handle. What the
//! driver does goes in the bundle's `logs/<machine>/plug.log`.

use std::fs::{self, File};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::real::{
    Check, LabSide, NetBoot, Verdict, WRITE_TIME, check_lab_side, found, persons_job, problem,
    ssh_target,
};
use super::{Driver, Setup};
use crate::bench::Kit;
use crate::machines::PlugSettings;
use crate::netboot::{self, Offer};
use crate::netlog::NetlogReader;
use crate::shelly::{PlugStatus, Shelly};
use crate::ssh;
use crate::suite::{Button, PowerAction, Stick};

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

/// What the screen, keyboard and mouse say when asked for.
const NO_SCREEN: &str = "the plug driver has no screen, keyboard or mouse: the machine file's \
                         keys, mouse and screen controls are \"none\"";

/// A real machine powered through a Shelly plug.
pub struct PlugDriver {
    settings: PlugSettings,
    setup: Setup,
    plug: Shelly,
    netboot: NetBoot,
    netlog: NetlogReader,
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
}

impl PlugDriver {
    /// Gets ready to drive the machine: reads the writer's kit, and starts
    /// the net-boot server and the netlog reader. Leaves the machine's
    /// power as it is.
    pub fn new(settings: PlugSettings, setup: Setup) -> Result<PlugDriver, String> {
        fs::create_dir_all(&setup.logs)
            .and_then(|()| fs::create_dir_all(&setup.work))
            .map_err(|err| format!("making the machine's folders: {err}"))?;
        let side = lab_side(&settings);
        let lab = side.lab_ip()?;
        let netboot = NetBoot::start(&side, &setup.logs.join("plug.log"))?;
        let machine: IpAddr = settings.machine_address.parse().map_err(|_| {
            format!("plug.machine_address {} is not an address", settings.machine_address)
        })?;
        // Until the first boot, what the machine sends goes to a scratch log.
        let before = setup.work.join("serial-before.log");
        let netlog =
            NetlogReader::start(SocketAddr::new(lab, settings.netlog_port), machine, &before)?;
        let mut driver = PlugDriver {
            plug: Shelly::new(&settings.shelly),
            settings,
            setup,
            netboot,
            netlog,
            console_log: before,
            boot: 0,
            power: None,
            last_start: None,
            last_stop: None,
            start_settle: START_SETTLE,
        };
        driver.note(&format!(
            "plug at {}; net-boot server at {}, ports {}; netlog on UDP port {}",
            driver.settings.shelly,
            driver.settings.lab_address,
            driver.netboot.ports(),
            driver.netlog.local_addr().port()
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
        let events = File::create(&writer_log)
            .map_err(|err| format!("making {}: {err}", writer_log.display()))?;
        self.netboot.copy_events_to(Some(events));
        // Before the writer is told to restart: its next boot must not
        // find the script.
        let script = self.netboot.script().to_path_buf();
        let offer = Offer::Writer {
            kit: self.netboot.writer.clone(),
            image,
            disk: self.settings.disk.clone(),
            written: Box::new(move || netboot::remove_script(&script)),
        };
        // The writer sets its own address: the card with the machine
        // file's MAC gets the machine's address on the lab's /24.
        let cmdline = format!(
            "{} dukovo.lab={{base}} dukovo.lab.ip={}/24 dukovo.lab.mac={}",
            self.netboot.writer.cmdline,
            self.settings.machine_address,
            self.settings.mac.to_ascii_lowercase()
        );
        let booted = self.netboot.begin("the writer", offer, &cmdline).and_then(|()| self.cycle());
        let outcome = booted.and_then(|()| self.netboot.await_writer(&self.setup.stop));
        self.netboot.finish();
        self.netboot.copy_events_to(None);
        match outcome {
            Ok(()) => {
                self.note("net boot: the disk holds the image; the machine restarts into it");
                Ok(())
            }
            Err(why) => {
                // A writer that failed must not be left running.
                let _ = self.cut();
                self.note(&format!("net boot: {why}"));
                Err(format!(
                    "{why}; what the writer did is in {}, and what the driver did in plug.log",
                    writer_log.display()
                ))
            }
        }
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
/// the plug, the netlog port, the lab network, the TFTP folder and
/// network GRUB, the writer's kit and the lab key.
pub fn check(settings: &PlugSettings, key: Option<&Path>) -> Vec<Check> {
    let mut checks = vec![found(
        "plug",
        Shelly::new(&settings.shelly)
            .status()
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
    let port = settings.netlog_port;
    checks.push(found(
        "netlog port",
        std::net::UdpSocket::bind((settings.lab_address.as_str(), port))
            .map(|_| format!("UDP port {port} is free for the netlog reader"))
            .map_err(|err| {
                (Verdict::Note, format!("port {port} is in use ({err}): by a run going now?"))
            }),
    ));
    checks
}

impl Driver for PlugDriver {
    fn boot_fresh(&mut self) -> Result<(), String> {
        self.boot += 1;
        self.console_log = self.setup.logs.join(format!("serial-{}.log", self.boot));
        self.note(&format!("boot {}: afresh", self.boot));
        self.cut()?;
        self.netlog.switch_to(&self.console_log)?;
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

    fn press_keys(&mut self, _combos: &[String]) -> Result<(), String> {
        Err(NO_SCREEN.into())
    }

    fn point(&mut self, _x: i32, _y: i32) -> Result<(), String> {
        Err(NO_SCREEN.into())
    }

    fn click(&mut self, _button: Button) -> Result<(), String> {
        Err(NO_SCREEN.into())
    }

    fn screenshot(&mut self, _to: &Path) -> Result<(), String> {
        Err(NO_SCREEN.into())
    }

    fn screen_dump(&mut self, _to: &Path) -> Result<(), String> {
        Err(NO_SCREEN.into())
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

    fn slowdown(&self) -> u32 {
        1
    }

    fn boot_time(&self) -> Duration {
        WRITE_TIME
    }

    fn linux_twin(
        &mut self,
        _kit: &Kit,
        _log: &Path,
        _deadline: Instant,
    ) -> Result<String, String> {
        Err("the plug driver has no Linux twin yet: the machine file's linux control is \"none\""
            .into())
    }

    fn emulated(&self) -> bool {
        false
    }

    fn shut_off(&mut self) {
        self.netboot.finish();
        if let Err(err) = self.cut() {
            self.note(&format!("shut off: {err}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netboot::tests::{make_image, pretend_writer};
    use crate::shelly::fake::FakeShelly;
    use crate::stop::Stop;
    use crate::testing::TempDir;
    use std::net::UdpSocket;

    const MAC: &str = "aa:bb:cc:00:11:22";

    /// A machine behind a pretend plug: switching the plug on runs the
    /// pretend writer when a boot script is there, and else boots the
    /// disk, which sends its ready line to the lab PC as netlog does.
    struct Bench {
        dir: TempDir,
        fake: FakeShelly,
        driver: PlugDriver,
    }

    /// A UDP port nothing listens on now.
    fn free_udp_port() -> u16 {
        UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    fn bench(label: &str, tamper: bool) -> Bench {
        let dir = TempDir::new(label);
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
            let (tftp, disk) = (tftp.clone(), disk.clone());
            fake.state.lock().unwrap().on_switch_on = Some(Box::new(move || {
                let (tftp, disk) = (tftp.clone(), disk.clone());
                // As the machine would: the firmware, then what it boots.
                std::thread::spawn(move || {
                    let say = |text: &str| {
                        let line = format!("<6>[    1.000000] {text}");
                        UdpSocket::bind("127.0.0.1:0")
                            .unwrap()
                            .send_to(line.as_bytes(), ("127.0.0.1", netlog_port))
                            .unwrap();
                    };
                    let Ok(script) = fs::read_to_string(netboot::script_path(&tftp, MAC)) else {
                        say("Boot complete.");
                        return;
                    };
                    let base = script
                        .split_whitespace()
                        .find_map(|word| word.strip_prefix("dukovo.lab="))
                        .unwrap();
                    // Kept for the test to read: the boot script is gone
                    // once the boot ends.
                    fs::write(tftp.with_file_name("writer-script"), &script).unwrap();
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
        let mut driver = PlugDriver::new(settings, setup).unwrap();
        // Tests of the wait after a start set their own.
        driver.start_settle = Duration::ZERO;
        Bench { dir, fake, driver }
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
        assert!(bench.driver.press_keys(&["a".into()]).unwrap_err().contains("no screen"));
        assert!(bench.driver.screen_dump(Path::new("x.ppm")).unwrap_err().contains("no screen"));
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
}

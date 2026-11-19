// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A pretend machine, so whole runs are tested without QEMU: it writes
//! serial lines as a booting Dukovo would and answers a few commands.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::*;
use crate::ssh::Output;
use crate::testing::TempDir;

struct FakeMachine {
    logs: PathBuf,
    serial: PathBuf,
    boot: u32,
    on: bool,
    stop: crate::stop::Stop,
    /// Whether it boots again after reset; the machine named `stuck`
    /// does not.
    restarts: bool,
    /// Whether its numbers are an emulator's; the machine named
    /// `emulated`'s are.
    emulated: bool,
    /// What its console shows, a row a string, and what has been typed
    /// on the row being written.
    screen: Vec<String>,
    typing: String,
    /// Where the pointer points, which a click shows on the console.
    pointer: (u32, u32),
    /// Whether the next boot gives the machine its private network port.
    want_attacker: bool,
    /// Whether the attacker is up on that network.
    attacker_on: bool,
    /// Whether that attacker is the red-team kit's Linux.
    linux_attacker: bool,
    /// The card reader the next boot gives the machine, if any.
    card_reader: Option<crate::suite::Reader>,
    /// Whether the next boot gives the machine an optical drive.
    disc_drive: bool,
    /// Whether the next boot gives the machine a floppy drive.
    floppy_drive: bool,
    /// USB sticks plugged in so far, to name the next, and the one in now.
    /// A real driver refuses a second stick while one is in; the machine
    /// named `jammed` cannot let one go.
    sticks: u32,
    stick: Option<u32>,
    jammed: bool,
    /// Whether its Linux twin is the machine itself, as a real machine's
    /// is; the machine named `real`'s is.
    real: bool,
    /// Whether it asks for its root to be grown after a fresh boot, as a
    /// real machine's driver does; the machines named `grows` and
    /// `grows-badly` do, and the second's `storage grow` fails.
    grows_root: bool,
    grow_fails: bool,
    /// Whether it asks for its clock to be set after a fresh boot, as a
    /// real machine's driver does; the machine named `clocked` does.
    sets_clock: bool,
    /// What the lab PC's loopback forward to the machine's ports listens
    /// on, while the machine is up.
    listeners: Vec<std::net::TcpListener>,
    /// The machine's name, which says how its health check answers: the
    /// machines named `checked...` have one. `checked` is healthy,
    /// `checked-once` has a full root on its first boot only, and
    /// `checked-bad` has a clock 41 seconds ahead of the lab PC's.
    name: String,
}

/// The characters the fake console draws, in the test font.
const FAKE_CHARS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789().:$#?-/";

/// The fake console: 24 by 6 cells of the 4x6 test font, from (2, 2), on a
/// screen of 100 by 40 pixels.
const FAKE_GEOMETRY: &str = "consoled: console of 24x6 cells with font 4x6 at (2, 2)";

fn fake_font() -> Vec<u8> {
    crate::screen::tests::test_font(FAKE_CHARS)
}

impl FakeMachine {
    /// Whether the lab PC reaches it by an address, as a real machine on
    /// the lab network: the machines named `addressed...`.
    fn addressed(&self) -> bool {
        self.name.starts_with("addressed")
    }

    fn say(&self, line: &str) {
        let mut log = OpenOptions::new().create(true).append(true).open(&self.serial).unwrap();
        writeln!(log, "{line}").unwrap();
    }

    /// A new boot's serial log, which says where the console's cells are.
    fn new_serial(&mut self) {
        self.boot += 1;
        fs::create_dir_all(&self.logs).unwrap();
        self.serial = self.logs.join(format!("serial-{}.log", self.boot));
        self.on = true;
        // A new boot starts with no attacker up, as a real machine does.
        self.attacker_on = false;
        // A stick plugged in while the machine ran goes with that boot.
        self.stick = None;
        self.say(FAKE_GEOMETRY);
    }

    /// Shows `row` at the bottom of the console, the rows above scrolling up.
    fn show(&mut self, row: &str) {
        self.screen.push(row.to_string());
        let rows = self.screen.len();
        self.screen.drain(..rows.saturating_sub(6));
    }

    /// A key pressed at the console: a sign-in that answers Name:, then
    /// Password: (sesame), then runs whoami; or the install stick's
    /// installer, which starts installing at Enter.
    fn key(&mut self, combo: &str) {
        let c = match combo {
            "ret" => '\n',
            "spc" => ' ',
            "slash" => '/',
            key if key.len() == 1 => key.chars().next().unwrap(),
            key if key.starts_with("shift-") && key.len() == 7 => {
                key.chars().last().unwrap().to_ascii_uppercase()
            }
            _ => return,
        };
        if c != '\n' {
            self.typing.push(c);
            return;
        }
        let typed = std::mem::take(&mut self.typing);
        let prompt = self.screen.last().cloned().unwrap_or_default();
        if prompt == "Install Dukovo" {
            self.show("Installing");
        } else if prompt == "Name:" {
            self.show(&typed);
            self.show("Password:");
        } else if prompt == "Password:" {
            // A password is never shown.
            if typed == "sesame" {
                self.show("$");
            } else {
                self.show("Sorry");
                self.show("Name:");
            }
        } else if prompt == "$" {
            self.show(&format!("$ {typed}"));
            if typed == "whoami" {
                self.show("lab");
            }
            self.show("$");
        }
    }
}

impl Driver for FakeMachine {
    fn boot_fresh(&mut self) -> Result<(), String> {
        self.new_serial();
        self.screen = vec!["Dukovo 0.13.0 (tty1)".into(), "Name:".into()];
        self.say("\u{1b}[1mRadix 0.13.0\u{1b}[0m\r");
        self.say("Boot complete.");
        Ok(())
    }

    fn boot_stick(&mut self, stick: &Path, disk: u64) -> Result<(), String> {
        assert!(stick.is_file(), "the stick is given a second name, not lost");
        self.new_serial();
        // The install stick is in, as the machine's next stick.
        self.sticks += 1;
        self.stick = Some(self.sticks);
        self.say(&format!("live: a blank disk of {disk} bytes"));
        self.screen = vec!["Install Dukovo".into()];
        Ok(())
    }

    fn boot_dvd(&mut self, dvd: &Path, disk: u64) -> Result<(), String> {
        assert!(dvd.is_file(), "the DVD is given a second name, not lost");
        if !self.disc_drive {
            return Err("this boot was not given an optical drive".into());
        }
        self.new_serial();
        self.say(&format!("live: from the DVD, a blank disk of {disk} bytes"));
        self.screen = vec!["Install Dukovo".into()];
        Ok(())
    }

    fn power(&mut self, action: PowerAction, _deadline: Instant) -> Result<(), String> {
        match action {
            PowerAction::On | PowerAction::Reset => {
                self.on = true;
                if self.restarts {
                    self.say("Boot complete.");
                }
            }
            PowerAction::Off | PowerAction::Button | PowerAction::Cut => self.on = false,
        }
        Ok(())
    }

    fn is_on(&mut self) -> bool {
        self.on
    }

    fn grows_root(&self) -> bool {
        self.grows_root
    }

    fn sets_clock(&self) -> bool {
        self.sets_clock
    }

    fn checks_health(&self) -> bool {
        self.name.starts_with("checked")
    }

    /// Keeps the driver's lines in a log of its own, as a real driver's.
    fn note(&mut self, line: &str) {
        fs::create_dir_all(&self.logs).unwrap();
        let path = self.logs.join("driver.log");
        let mut log = OpenOptions::new().create(true).append(true).open(path).unwrap();
        writeln!(log, "{line}").unwrap();
    }

    fn serial_log(&self) -> &Path {
        &self.serial
    }

    fn set_link(&mut self, up: bool) -> Result<(), String> {
        self.say(if up { "net: link up" } else { "net: link down" });
        Ok(())
    }

    fn plug_usb(&mut self, _stick: &crate::suite::Stick) -> Result<(), String> {
        if let Some(stick) = self.stick {
            return Err(format!("USB stick {stick} is still plugged in; pull it first"));
        }
        self.sticks += 1;
        self.stick = Some(self.sticks);
        Ok(())
    }

    fn unplug_usb(&mut self) -> Result<(), String> {
        if self.jammed {
            return Err("the guest would not let go of the device".into());
        }
        self.stick.take().map(drop).ok_or_else(|| "no USB stick is plugged in".into())
    }

    fn take_out_media(&mut self) -> Vec<Result<String, String>> {
        let Some(number) = self.stick else { return Vec::new() };
        let what = format!("USB stick {number} plugged in");
        vec![self.unplug_usb().map(|()| what.clone()).map_err(|err| format!("{what}: {err}"))]
    }

    fn want_card_reader(&mut self, reader: Option<crate::suite::Reader>) {
        self.card_reader = reader;
    }

    fn insert_card(&mut self, _card: &crate::suite::Stick) -> Result<(), String> {
        if self.card_reader.is_none() {
            return Err("this boot was not given a card reader".into());
        }
        Ok(())
    }

    fn remove_card(&mut self) -> Result<(), String> {
        if self.card_reader.is_none() {
            return Err("this boot was not given a card reader".into());
        }
        Ok(())
    }

    fn reinsert_card(&mut self) -> Result<(), String> {
        self.remove_card()
    }

    fn want_disc_drive(&mut self, yes: bool) {
        self.disc_drive = yes;
    }

    fn insert_disc(&mut self, _image: &Path) -> Result<(), String> {
        self.eject_disc()
    }

    fn insert_blank_disc(&mut self, _disc: crate::suite::BlankDisc) -> Result<(), String> {
        self.eject_disc()
    }

    fn eject_disc(&mut self) -> Result<(), String> {
        if !self.disc_drive {
            return Err("this boot was not given an optical drive".into());
        }
        Ok(())
    }

    fn reinsert_disc(&mut self) -> Result<(), String> {
        self.eject_disc()
    }

    fn press_disc_button(&mut self) -> Result<(), String> {
        self.eject_disc()
    }

    fn want_floppy_drive(&mut self, yes: bool) {
        self.floppy_drive = yes;
    }

    fn insert_floppy(&mut self, _floppy: &crate::suite::Floppy) -> Result<(), String> {
        self.eject_floppy()
    }

    fn eject_floppy(&mut self) -> Result<(), String> {
        if !self.floppy_drive {
            return Err("this boot was not given a floppy drive".into());
        }
        Ok(())
    }

    fn reinsert_floppy(&mut self) -> Result<(), String> {
        self.eject_floppy()
    }

    fn press_keys(&mut self, combos: &[String]) -> Result<(), String> {
        for combo in combos {
            self.key(combo);
        }
        Ok(())
    }

    fn point(&mut self, x: i32, y: i32) -> Result<(), String> {
        // The fake screen is 100 by 40 pixels, as `screen_dump` draws it.
        let (Some(x), Some(y)) = (crate::driver::pixel(x, 100), crate::driver::pixel(y, 40)) else {
            return Err(format!("({x}, {y}) is off the screen, which is 100x40 pixels"));
        };
        self.pointer = (x, y);
        Ok(())
    }

    fn click(&mut self, button: crate::suite::Button) -> Result<(), String> {
        let (x, y) = self.pointer;
        self.show(&format!("{} click at {x} {y}", button.name()));
        Ok(())
    }

    fn screenshot(&mut self, to: &Path) -> Result<(), String> {
        fs::write(to, b"\x89PNG").map_err(|err| err.to_string())
    }

    fn screen_dump(&mut self, to: &Path) -> Result<(), String> {
        let rows: Vec<&str> = self.screen.iter().map(String::as_str).collect();
        let ppm = crate::screen::tests::draw(&fake_font(), &rows, (100, 40), (2, 2));
        fs::write(to, ppm).map_err(|err| err.to_string())
    }

    fn run_command(
        &mut self,
        command: &str,
        input: Option<&str>,
        deadline: Instant,
        stall: Option<&crate::ssh::Stall>,
    ) -> Result<Output, String> {
        self.note(&format!("ran {command}"));
        let (status, text) = match command {
            // The root's growth, as root: `storage grow` fails on a disk
            // with no room.
            cmd if cmd.starts_with("elevate -S sh -c 'part=") => match (input, self.grow_fails) {
                (Some("sesame\n"), false) => (Some(0), "storage: grown\n".into()),
                (Some("sesame\n"), true) => (Some(1), "storage: no room after the root\n".into()),
                _ => (Some(1), "elevate: a password is needed\n".into()),
            },
            // The clock set to the lab PC's, as root.
            cmd if cmd.starts_with("elevate -S timedatectl set-time '") => match input {
                Some("sesame\n") => (Some(0), String::new()),
                _ => (Some(1), "elevate: a password is needed\n".into()),
            },
            // As Dukovo's elevate -S: the password is a line of the input.
            // This one says it back, as a careless command might.
            "elevate -S id" => match input {
                Some("sesame\n") => (Some(0), "uid=0(root) heard sesame\n".into()),
                _ => (Some(1), "elevate: a password is needed\n".into()),
            },
            "elevate -S poweroff" => {
                self.on = false;
                (Some(0), String::new())
            }
            // A benchmark program: faster than the fake Linux below at
            // fork, slower at pipes.
            "bench" => (
                Some(0),
                "BENCH name=fork ns=90 iterations=10\nBENCH name=pipe ns=130 iterations=10\n"
                    .into(),
            ),
            "bench-broken" => (
                Some(1),
                "BENCH name=fork ns=90 iterations=10\nbench: listen failed: Permission denied\n"
                    .into(),
            ),
            "true" => (Some(0), String::new()),
            // Fails the first time anywhere in the run and passes after,
            // as a flaky test does.
            "flip" => {
                let bundle = self.logs.ancestors().find(|path| path.ends_with("logs")).unwrap();
                let marker = bundle.with_file_name("flip-seen");
                if marker.exists() {
                    (Some(0), "ok\n".into())
                } else {
                    fs::write(marker, "").unwrap();
                    (Some(0), "bad\n".into())
                }
            }
            // Takes long enough that machines of a pool all get a turn at
            // its queue.
            "slow" => {
                std::thread::sleep(std::time::Duration::from_millis(100));
                (Some(0), String::new())
            }
            // Says what it was given on its input, as a command a test
            // hands a file's text to does with it.
            "cat" => (Some(0), input.unwrap_or_default().to_string()),
            // Reaching a server of the lab PC's, as kinit reaches a KDC.
            cmd if cmd.starts_with("reach ") => {
                (Some(0), format!("reached {}\n", cmd.trim_start_matches("reach ")))
            }
            "uname" => (Some(0), "Dukovo\n".into()),
            "false" => (Some(1), String::new()),
            // Giving the machine its address on the private network, as
            // root, when an attack test sets the attacker up.
            cmd if cmd.contains("ip address add") => match input {
                Some("sesame\n") => (Some(0), String::new()),
                _ => (Some(1), "elevate: a password is needed\n".into()),
            },
            // A fault in the lab itself, not in the machine under test.
            "panic" => panic!("the fake machine broke"),
            // Prints nothing until the time limit, or until the stall ends
            // it; `chatter` also keeps the console scrolling meanwhile.
            "hang" | "chatter" => {
                let mut quiet = stall.map(|stall| crate::ssh::Quiet::new(stall, Instant::now()));
                while Instant::now() < deadline && !self.stop.requested() {
                    if command == "chatter" {
                        self.say("tick");
                    }
                    if let (Some(quiet), Some(stall)) = (&mut quiet, stall)
                        && quiet.stalled(Instant::now(), 0, crate::ssh::size_of(&stall.console))
                    {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                (None, String::new())
            }
            // The health check: the root's free space and the clock.
            cmd if cmd == crate::health::COMMAND => {
                let free = if self.name == "checked-once" && self.boot == 1 {
                    300 * 1024
                } else {
                    6_000_000
                };
                let ahead = if self.name == "checked-bad" { 41 } else { 0 };
                let clock = crate::clock::Utc::now().seconds() + ahead;
                let text = format!(
                    "Filesystem 1K-blocks Used Available Use% Mounted on\n\
                     /dev/sda2 7000000 1000000 {free} 15% /\nclock={clock}\n"
                );
                (Some(0), text)
            }
            other => (Some(127), format!("{other}: not found\n")),
        };
        Ok(Output { status, text })
    }

    fn want_attacker(&mut self, yes: bool) {
        self.want_attacker = yes;
    }

    fn want_linux_attacker(&mut self, yes: bool) {
        self.linux_attacker = yes;
    }

    fn start_attacker(&mut self, _deadline: Instant) -> Result<(), String> {
        if !self.want_attacker {
            return Err("this boot was not given an attacker".into());
        }
        self.attacker_on = true;
        Ok(())
    }

    fn attack(
        &mut self,
        command: &str,
        input: Option<&str>,
        _deadline: Instant,
    ) -> Result<Output, String> {
        if !self.attacker_on {
            return Err("the attacker is not up".into());
        }
        let (status, text) = match command {
            // The kit's init sets its own address; a step that tries to is
            // the runner running the Dukovo guest's setup on the kit.
            cmd if cmd.contains("ip address add") && self.linux_attacker => {
                (Some(127), "the kit sets its own address\n".into())
            }
            // The attacker's own address on the private network, as root.
            cmd if cmd.contains("ip address add") => match input {
                Some("sesame\n") => (Some(0), String::new()),
                _ => (Some(1), "elevate: a password is needed\n".into()),
            },
            cmd if cmd.starts_with("echo ") => (Some(0), format!("{}\n", &cmd[5..])),
            // A port scan of the machine under test, which answers on 22.
            "scan 10.9.9.1" => (Some(0), "10.9.9.1 22 open\n".into()),
            other => (Some(127), format!("{other}: not found\n")),
        };
        Ok(Output { status, text })
    }

    fn peer_address(&self) -> Option<String> {
        Some("10.0.2.2".into())
    }

    fn machine_address(&self) -> Option<String> {
        Some(if self.addressed() { "10.77.0.2" } else { "127.0.0.1" }.into())
    }

    /// The machine's port N is at the lab PC's 40000 + N; an addressed
    /// machine's, as a real machine's on the lab network, at N.
    fn reach(&mut self, port: u16, udp: bool) -> Result<u16, String> {
        if self.addressed() {
            return Ok(port);
        }
        if udp {
            return Err("this fake forwards no UDP".into());
        }
        port.checked_add(40000).ok_or_else(|| "this fake forwards only low ports".into())
    }

    /// An addressed machine forwards, as the real drivers do, with a
    /// `sleep` that writes its pid and what it forwards. `addressed-refused`
    /// cannot start one and `addressed-ends` has one that ends at once.
    fn loopback_forward(&mut self, local_port: u16, port: u16) -> Result<Option<String>, String> {
        if !self.addressed() {
            return Ok(None);
        }
        if self.name == "addressed-refused" {
            return Err("the lab has no key".into());
        }
        if self.name == "addressed-ends" {
            return Ok(Some("echo Permission denied; exit 255".into()));
        }
        let listener = std::net::TcpListener::bind(("127.0.0.1", local_port))
            .map_err(|err| err.to_string())?;
        self.listeners.push(listener);
        let pid = self.logs.join("forward.pid");
        Ok(Some(format!(
            "echo forward 127.0.0.1:{local_port} to {port}; echo $$ > {}; exec sleep 300",
            pid.display()
        )))
    }

    fn slowdown(&self) -> u32 {
        1
    }

    fn linux_twin(
        &mut self,
        kit: &crate::bench::Kit,
        log: &Path,
        _deadline: Instant,
    ) -> Result<String, String> {
        let text = format!(
            "Linux booting\nBENCH name=fork ns=100 iterations=10\nBENCH name=pipe ns=100 iterations=10\n{}\n",
            kit.done
        );
        fs::write(log, &text).map_err(|err| err.to_string())?;
        // Linux ran on the machine itself, and the machine is off after.
        if self.real {
            self.on = false;
        }
        Ok(text)
    }

    fn twin_ends_boot(&self) -> bool {
        self.real
    }

    fn emulated(&self) -> bool {
        self.emulated
    }

    fn shut_off(&mut self) {
        self.on = false;
    }
}

/// Starts a fake machine, or fails for one named `broken`.
fn start_fake(machine: &Machine, setup: Setup) -> Result<Box<dyn Driver>, String> {
    if machine.name == "broken" {
        return Err("no such machine".into());
    }
    Ok(Box::new(FakeMachine {
        logs: setup.logs,
        serial: PathBuf::new(),
        boot: 0,
        on: false,
        stop: setup.stop,
        restarts: machine.name != "stuck",
        emulated: machine.name == "emulated",
        screen: Vec::new(),
        typing: String::new(),
        pointer: (0, 0),
        want_attacker: false,
        card_reader: None,
        disc_drive: false,
        floppy_drive: false,
        attacker_on: false,
        linux_attacker: false,
        sticks: 0,
        stick: None,
        jammed: machine.name == "jammed",
        real: machine.name == "real",
        grows_root: machine.name.starts_with("grows") || machine.name.starts_with("checked"),
        grow_fails: machine.name == "grows-badly",
        sets_clock: machine.name == "clocked",
        listeners: Vec::new(),
        name: machine.name.clone(),
    }))
}

/// The tests every run here picks, in the order they run.
const TESTS: &[(&str, &str)] = &[
    (
        "boot/ready.toml",
        "about = \"boots\"\ntime = \"1m\"\nfresh = true\n[[step]]\nserial = \"Radix 0.13.0\"\n",
    ),
    (
        "features/uname.toml",
        "about = \"uname\"\ntime = \"1m\"\n[[step]]\nrun = \"uname\"\nexpect = \"Dukovo\"\n",
    ),
    (
        "features/wrong.toml",
        "about = \"fails\"\ntime = \"1m\"\n[[step]]\nrun = \"uname\"\nexpect = \"Linux\"\n",
    ),
    (
        "drivers/link.toml",
        "about = \"link\"\ntime = \"1m\"\n[[step]]\nlink = \"down\"\n[[step]]\nserial = \"link down\"\n\
         [[step]]\nlink = \"up\"\n[[step]]\nscreenshot = \"after\"\n",
    ),
    ("stress/cord.toml", "about = \"cord\"\ntime = \"1m\"\n[[step]]\npower = \"cut\"\n"),
    ("stress/hang.toml", "about = \"hangs\"\ntime = \"1s\"\n[[step]]\nrun = \"hang\"\n"),
];

struct Setting {
    dir: TempDir,
    tests: Vec<Test>,
    qemu: Machine,
}

fn setting() -> Setting {
    setting_with(TESTS)
}

fn setting_with(tests: &[(&str, &str)]) -> Setting {
    let dir = TempDir::new("run");
    for (file, text) in tests {
        dir.write(&format!("tests/{file}"), text);
    }
    dir.write("lab_ed25519", "not a real key");
    let tests = crate::suite::load(&dir.path().join("tests")).unwrap();
    let qemu = crate::machines::load(&dir.path().join("none")).unwrap().remove(0);
    Setting { dir, tests, qemu }
}

/// What a run on fake machines left: how it ended, its bundle, its results.
struct Done {
    summary: Summary,
    bundle: PathBuf,
}

fn run_on(setting: &Setting, machines: Vec<&Machine>) -> (Done, Vec<ResultRecord>) {
    run_with(setting, machines, false, None)
}

/// A run with Needs you tests or without, under a chosen id or a new one.
fn run_with(
    setting: &Setting,
    machines: Vec<&Machine>,
    with_person: bool,
    id: Option<&str>,
) -> (Done, Vec<ResultRecord>) {
    let request = Request {
        tests: setting.tests.iter().collect(),
        machines,
        image: "dukovo.img".into(),
        build: Some("0.13.0 test".into()),
        store: setting.dir.path().join("store"),
        key: Some(setting.dir.path().join("lab_ed25519")),
        password: None,
        with_person,
        linux: None,
        fonts: None,
        browsers: None,
        stick: None,
        dvd: None,
        ob: None,
        preset: None,
        schedule: None,
        id: Some(
            id.map_or_else(|| crate::bundle::new_run_id(crate::clock::Utc::now()), String::from),
        ),
        ..Request::default()
    };
    let ended = run(&request, &Reporter::silent(), &start_fake).unwrap();
    let bundle = crate::store::bundle_dir(&request.store, request.id.as_deref().unwrap());
    let results: crate::bundle::ResultsFile =
        serde_json::from_str(&fs::read_to_string(bundle.join("results.json")).unwrap()).unwrap();
    (Done { summary: ended.summary, bundle }, results.results)
}

/// Waits until the run's events hold `text`, as a person watching the
/// page would, then does `then`.
fn when_events_say(
    store: PathBuf,
    id: &'static str,
    text: &'static str,
    then: impl FnOnce() + Send + 'static,
) {
    std::thread::spawn(move || {
        let events = crate::store::bundle_dir(&store, id).join("events.jsonl");
        for _ in 0..500 {
            if fs::read_to_string(&events).is_ok_and(|events| events.contains(text)) {
                then();
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the events never said {text}");
    });
}

fn outcomes(results: &[ResultRecord], machine: &str) -> Vec<(String, Outcome, Option<u32>)> {
    results
        .iter()
        .filter(|result| result.machine == machine)
        .map(|result| (result.test.clone(), result.result, result.boot))
        .collect()
}

#[test]
fn runs_tests_shares_boots_and_writes_the_bundle() {
    let setting = setting();
    let (ended, results) = run_on(&setting, vec![&setting.qemu]);
    assert_eq!(
        outcomes(&results, "qemu"),
        [
            ("boot/ready".into(), Outcome::Pass, Some(1)),
            ("drivers/link".into(), Outcome::Pass, Some(1)),
            ("features/uname".into(), Outcome::Pass, Some(1)),
            // A failure means the next test boots afresh.
            ("features/wrong".into(), Outcome::Fail, Some(1)),
            ("stress/cord".into(), Outcome::Pass, Some(2)),
            // The cord was pulled, so the machine is booted again.
            ("stress/hang".into(), Outcome::Fail, Some(3)),
        ]
    );
    assert_eq!(ended.summary, Summary { passed: 4, failed: 2, skipped: 0, errors: 0 });

    let wrong = &results[3];
    let reason = wrong.reason.as_deref().unwrap();
    assert!(reason.starts_with("step 1: its output lacks \"Linux\"\n$ uname\nDukovo"), "{reason}");
    assert_eq!(wrong.screenshots, ["screenshots/qemu/features-wrong-at-failure.png"]);
    let hang = results[5].reason.as_deref().unwrap();
    assert!(
        hang.contains("still running at the test's time limit")
            && hang.ends_with("(time limit 1s)"),
        "{hang}"
    );

    let link = &results[1];
    assert_eq!(link.steps.len(), 4, "a shared boot adds no boot step");
    assert_eq!(link.screenshots, ["screenshots/qemu/drivers-link-after.png"]);
    let log = fs::read_to_string(ended.bundle.join(link.log.as_ref().unwrap())).unwrap();
    assert!(log.contains("Sharing boot 1 with the test before."), "{log}");
    assert!(ended.bundle.join("logs/qemu/serial-3.log").is_file());

    let run: RunRecord =
        serde_json::from_str(&fs::read_to_string(ended.bundle.join("run.json")).unwrap()).unwrap();
    // CI checks this bundle's JSON against schema/.
    if let Some(keep) = std::env::var_os("DUKOVO_LAB_KEEP_BUNDLE") {
        fs::create_dir_all(&keep).unwrap();
        for file in ["run.json", "results.json"] {
            fs::copy(ended.bundle.join(file), Path::new(&keep).join(file)).unwrap();
        }
    }
    assert_eq!(run.summary, Some(ended.summary));
    assert!(run.ended.is_some());
    assert_eq!(run.tests.len(), 6);
    assert!(
        !setting.dir.path().join("store/work").join(&run.id).exists(),
        "work files are removed"
    );
}

#[test]
fn bench_steps_hold_dukovo_to_linux_measured_once_a_run() {
    let setting = setting_with(&[
        (
            "performance/fork.toml",
            "about = \"fork\"\ntime = \"1m\"\n[[step]]\nbench = \"bench\"\nrows = [\"fork\"]\n",
        ),
        (
            "performance/pipe.toml",
            "about = \"pipe\"\ntime = \"1m\"\n[[step]]\nbench = \"bench\"\nrows = [\"pipe\"]\n",
        ),
        (
            "performance/pipe-loose.toml",
            "about = \"pipe within 1.5\"\ntime = \"1m\"\n[[step]]\nbench = \"bench\"\n\
             rows = [\"fork\", \"pipe\"]\nlimit = 1.5\n",
        ),
        (
            "performance/unknown.toml",
            "about = \"a row Linux lacks\"\ntime = \"1m\"\n[[step]]\nbench = \"bench\"\n\
             rows = [\"exec\"]\n",
        ),
        (
            "performance/broken.toml",
            "about = \"the program breaks\"\ntime = \"1m\"\n[[step]]\nbench = \"bench-broken\"\n\
             rows = [\"fork\"]\n",
        ),
    ]);
    let kit_dir = setting.dir.path().join("kit");
    setting.dir.write("kit/vmlinuz", "kernel");
    setting.dir.write("kit/initramfs.img", "initrd");
    setting.dir.write(
        "kit/kit.toml",
        "kernel = \"vmlinuz\"\ninitrd = \"initramfs.img\"\ncmdline = \"console=ttyS0\"\n\
         done = \"linux-bench: done\"\n",
    );
    let mut emulated = setting.qemu.clone();
    emulated.name = "emulated".into();
    let request = |linux: Option<crate::bench::Kit>, machines| Request {
        tests: setting.tests.iter().collect(),
        machines,
        image: "dukovo.img".into(),
        build: None,
        store: setting.dir.path().join("store"),
        key: Some(setting.dir.path().join("lab_ed25519")),
        password: None,
        with_person: false,
        linux,
        fonts: None,
        browsers: None,
        stick: None,
        dvd: None,
        ob: None,
        preset: None,
        schedule: None,
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
        ..Request::default()
    };
    let kit = crate::bench::Kit::read(&kit_dir).unwrap();
    let with_kit = request(Some(kit.clone()), vec![&setting.qemu]);
    let ended = run(&with_kit, &Reporter::silent(), &start_fake).unwrap();
    let bundle = crate::store::bundle_dir(&with_kit.store, with_kit.id.as_deref().unwrap());
    let results: crate::bundle::ResultsFile =
        serde_json::from_str(&fs::read_to_string(bundle.join("results.json")).unwrap()).unwrap();
    let by_test = |test: &str| results.results.iter().find(|r| r.test == test).unwrap().clone();
    assert_eq!(by_test("performance/fork").result, Outcome::Pass);
    let pipe = by_test("performance/pipe");
    assert_eq!(pipe.result, Outcome::Fail);
    let reason = pipe.reason.unwrap();
    assert!(reason.starts_with("step 1: slower than Linux:"), "{reason}");
    assert!(reason.contains("pipe        130 ns        100 ns    1.30   1.00  over"), "{reason}");
    assert_eq!(by_test("performance/pipe-loose").result, Outcome::Pass);
    assert_eq!(by_test("performance/unknown").result, Outcome::Error);
    // A program that breaks says why in the first line, its finding's name.
    let broken = by_test("performance/broken");
    assert_eq!(broken.result, Outcome::Fail);
    let why = broken.reason.unwrap();
    assert!(
        why.starts_with(
            "step 1: the benchmark program exited with status 1: \
             bench: listen failed: Permission denied\n"
        ),
        "{why}"
    );
    assert_eq!(ended.summary, Summary { passed: 2, failed: 2, skipped: 0, errors: 1 });
    // Linux booted once for the whole run, and its console is kept.
    let logs: Vec<_> = fs::read_dir(bundle.join("logs/qemu"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("linux-boot-"))
        .collect();
    assert_eq!(logs.len(), 1, "{logs:?}");
    // A machine whose Linux twin is itself (a real one) goes on in a
    // fresh boot after Linux, and the results are the same.
    let mut real = setting.qemu.clone();
    real.name = "real".into();
    let on_itself = request(Some(kit.clone()), vec![&real]);
    let ended = run(&on_itself, &Reporter::silent(), &start_fake).unwrap();
    assert_eq!(ended.summary, Summary { passed: 2, failed: 2, skipped: 0, errors: 1 });
    let real_bundle = crate::store::bundle_dir(&on_itself.store, on_itself.id.as_deref().unwrap());
    // The first test to measure (they run in order of id) booted Linux.
    let first = fs::read_to_string(real_bundle.join("logs/real/performance-broken.log"))
        .unwrap_or_default();
    assert!(first.contains("Linux ran on the machine itself"), "{first}");
    assert!(first.contains("Boot 2: serial log logs/real/serial-2.log"), "{first}");
    // The finding of a slow row names no number, so it comes back as one.
    assert_eq!(
        crate::findings::signature(&reason),
        "step 1: slower than Linux: the rows marked over take more than their share of Linux's time"
    );

    // Without a kit the tests skip, saying how to get one; under an
    // emulator they run and keep their numbers, but hold none to Linux.
    let without = request(None, vec![&setting.qemu]);
    let ended = run(&without, &Reporter::silent(), &start_fake).unwrap();
    assert_eq!(ended.summary, Summary { passed: 0, failed: 0, skipped: 5, errors: 0 });
    let under_emulator = request(Some(kit), vec![&emulated]);
    let ended = run(&under_emulator, &Reporter::silent(), &start_fake).unwrap();
    assert_eq!(ended.summary, Summary { passed: 0, failed: 1, skipped: 3, errors: 1 });
}

#[test]
fn root_steps_give_elevate_the_password_and_never_show_it() {
    let setting = setting_with(&[
        (
            "features/root.toml",
            "about = \"root\"\ntime = \"1m\"\n[[step]]\nrun = \"id\"\nroot = true\nexpect = \"uid=0\"\n",
        ),
        (
            "boot/off.toml",
            "about = \"off\"\ntime = \"1m\"\n[[step]]\nrun = \"poweroff\"\nroot = true\n[[step]]\nwait = \"off\"\n",
        ),
    ]);
    let request = Request {
        tests: setting.tests.iter().collect(),
        machines: vec![&setting.qemu],
        image: "dukovo.img".into(),
        build: None,
        store: setting.dir.path().join("store"),
        key: Some(setting.dir.path().join("lab_ed25519")),
        password: Some("sesame".into()),
        with_person: false,
        linux: None,
        fonts: None,
        browsers: None,
        stick: None,
        dvd: None,
        ob: None,
        preset: None,
        schedule: None,
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
        ..Request::default()
    };
    let ended = run(&request, &Reporter::silent(), &start_fake).unwrap();
    assert_eq!(ended.summary, Summary { passed: 2, failed: 0, skipped: 0, errors: 0 });
    // Nothing the run kept holds the password, though the command said it.
    let bundle = crate::store::bundle_dir(&request.store, request.id.as_deref().unwrap());
    let mut folders = vec![bundle];
    let mut files = 0;
    while let Some(folder) = folders.pop() {
        for entry in fs::read_dir(&folder).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                folders.push(path);
                continue;
            }
            files += 1;
            let text = String::from_utf8_lossy(&fs::read(&path).unwrap()).into_owned();
            assert!(!text.contains("sesame"), "{} holds the password", path.display());
            if text.contains("uid=0(root)") {
                assert!(text.contains("heard ********"), "{text}");
            }
        }
    }
    assert!(files > 3, "the bundle has its files");
}

#[test]
fn attack_steps_run_on_a_private_lab_network() {
    let setting = setting_with(&[
        (
            "security/scan.toml",
            "about = \"a scan from the attacker\"\ntime = \"1m\"\nfresh = true\n\
             [[step]]\nattack = \"scan {target}\"\nexpect = \"{target} 22 open\"\n",
        ),
        // Sorts after `scan` in the same category, so it runs next.
        (
            "security/zz-after.toml",
            "about = \"after the attack boot\"\ntime = \"1m\"\n[[step]]\nrun = \"uname\"\nexpect = \"Dukovo\"\n",
        ),
    ]);
    let request = Request {
        tests: setting.tests.iter().collect(),
        machines: vec![&setting.qemu],
        image: "dukovo.img".into(),
        build: None,
        store: setting.dir.path().join("store"),
        key: Some(setting.dir.path().join("lab_ed25519")),
        // The address setup on each side runs as root, through elevate.
        password: Some("sesame".into()),
        with_person: false,
        linux: None,
        fonts: None,
        browsers: None,
        stick: None,
        dvd: None,
        ob: None,
        preset: None,
        schedule: None,
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
        ..Request::default()
    };
    let ended = run(&request, &Reporter::silent(), &start_fake).unwrap();
    let bundle = crate::store::bundle_dir(&request.store, request.id.as_deref().unwrap());
    let results: crate::bundle::ResultsFile =
        serde_json::from_str(&fs::read_to_string(bundle.join("results.json")).unwrap()).unwrap();
    let by_test = |test: &str| results.results.iter().find(|r| r.test == test).unwrap().clone();

    // The scan ran on the attacker and saw the machine's SSH port; its log
    // shows the command with `{target}` resolved to the machine's address.
    let scan = by_test("security/scan");
    assert_eq!(scan.result, Outcome::Pass, "{:?}", scan.reason);
    let log = fs::read_to_string(bundle.join(scan.log.as_ref().unwrap())).unwrap();
    assert!(log.contains("scan 10.9.9.1"), "{log}");

    // The attack boot is not shared, so the next test boots afresh and its
    // private network port is gone.
    let after = by_test("security/zz-after");
    assert_eq!(after.result, Outcome::Pass, "{:?}", after.reason);
    assert_eq!(after.steps[0].step, "boot afresh and wait until booted");
    assert_eq!(ended.summary, Summary { passed: 2, failed: 0, skipped: 0, errors: 0 });
}

#[test]
fn a_step_keeps_its_output_for_the_steps_after_it() {
    let setting = setting_with(&[(
        "features/saved.toml",
        "about = \"a value carried between steps\"\ntime = \"1m\"\nfresh = true\n\
             [[step]]\nrun = \"uname\"\nsave = \"os\"\n\
             [[step]]\nrun = \"cat\"\ninput = \"seen {saved:os}\"\nexpect = \"seen Dukovo\"\n\
             [[step]]\nattack = \"echo from {saved:os}\"\nexpect = \"from Dukovo\"\nsave = \"echoed\"\n\
             [[step]]\nhost = \"echo {saved:echoed}\"\nexpect = \"from Dukovo\"\n",
    )]);
    let request = Request {
        tests: setting.tests.iter().collect(),
        machines: vec![&setting.qemu],
        image: "dukovo.img".into(),
        store: setting.dir.path().join("store"),
        key: Some(setting.dir.path().join("lab_ed25519")),
        // The attack step's address setup on the machine runs as root.
        password: Some("sesame".into()),
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
        ..Request::default()
    };
    let ended = run(&request, &Reporter::silent(), &start_fake).unwrap();
    assert_eq!(ended.summary, Summary { passed: 1, failed: 0, skipped: 0, errors: 0 });
}

/// A run of one `attacker = "linux"` test and one default one, given the
/// red-team kit or not, and each test's result.
fn linux_attacker_run(kit: Option<&str>) -> (Outcome, Option<String>, Outcome) {
    let setting = setting_with(&[
        (
            "security/linux.toml",
            "about = \"a scan from Linux\"\ntime = \"1m\"\nfresh = true\nattacker = \"linux\"\n\
             [[step]]\nattack = \"scan {target}\"\nexpect = \"{target} 22 open\"\nroot = true\n",
        ),
        // Without `attacker`, a test keeps the Dukovo guest, kit or no kit.
        (
            "security/zz-image.toml",
            "about = \"a scan from a Dukovo guest\"\ntime = \"1m\"\nfresh = true\n\
             [[step]]\nattack = \"scan {target}\"\nexpect = \"{target} 22 open\"\n",
        ),
    ]);
    let folder = setting.dir.path().join("kit");
    let redteam = kit.map(|text| {
        setting.dir.write("kit/vmlinuz", "");
        setting.dir.write("kit/rootfs.cpio", "");
        setting.dir.write("kit/kit.toml", text);
        crate::redteam::RedteamKit::read(&folder).unwrap()
    });
    let request = Request {
        tests: setting.tests.iter().collect(),
        machines: vec![&setting.qemu],
        image: "dukovo.img".into(),
        store: setting.dir.path().join("store"),
        key: Some(setting.dir.path().join("lab_ed25519")),
        password: Some("sesame".into()),
        redteam,
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
        ..Request::default()
    };
    run(&request, &Reporter::silent(), &start_fake).unwrap();
    let bundle = crate::store::bundle_dir(&request.store, request.id.as_deref().unwrap());
    let results: crate::bundle::ResultsFile =
        serde_json::from_str(&fs::read_to_string(bundle.join("results.json")).unwrap()).unwrap();
    let by_test = |test: &str| results.results.iter().find(|r| r.test == test).unwrap().clone();
    let linux = by_test("security/linux");
    (linux.result, linux.reason, by_test("security/zz-image").result)
}

const KIT: &str = "kernel = \"vmlinuz\"\ninitrd = \"rootfs.cpio\"\ncmdline = \"console=ttyS0\"\n";

#[test]
fn a_linux_attacker_test_attacks_from_the_kit_without_the_guest_s_address_setup() {
    let (linux, reason, image) = linux_attacker_run(Some(KIT));
    assert_eq!(linux, Outcome::Pass, "{reason:?}");
    assert_eq!(image, Outcome::Pass);
}

#[test]
fn a_linux_attacker_test_is_skipped_without_a_red_team_kit() {
    let (linux, reason, image) = linux_attacker_run(None);
    assert_eq!(linux, Outcome::Skip);
    let reason = reason.unwrap();
    assert!(reason.contains("--redteam") && reason.contains("ob lab redteam-kit"), "{reason}");
    assert_eq!(image, Outcome::Pass);
}

#[test]
fn a_step_that_needs_root_fails_when_the_kit_runs_steps_as_another_user() {
    let (linux, reason, _) = linux_attacker_run(Some(&format!("{KIT}user = \"red\"\n")));
    assert_eq!(linux, Outcome::Error);
    assert!(reason.unwrap().contains("set user = \"root\""));
}

#[test]
fn card_tests_boot_with_a_card_reader_of_their_own() {
    let setting = setting_with(&[
        (
            "drivers/a-boot.toml",
            "about = \"a boot to share\"\ntime = \"1m\"\n[[step]]\nrun = \"uname\"\nexpect = \"Dukovo\"\n",
        ),
        (
            "drivers/card.toml",
            "about = \"a card in and out\"\ntime = \"1m\"\n\
             [[step]]\ncard = \"insert\"\nblank = \"16M\"\n[[step]]\ncard = \"remove\"\n\
             [[step]]\ncard = \"reinsert\"\n",
        ),
        // Sorts after `card`: its boot has an SD card reader instead.
        (
            "drivers/sd.toml",
            "about = \"an SD card in and out\"\ntime = \"1m\"\n\
             [[step]]\nsd = \"insert\"\nblank = \"16M\"\n[[step]]\nsd = \"remove\"\n",
        ),
        // Sorts after `sd` in the same category, so it runs next, and
        // would fail had it been given the reader's boot.
        (
            "drivers/zz-after.toml",
            "about = \"after the card boot\"\ntime = \"1m\"\n[[step]]\nrun = \"uname\"\nexpect = \"Dukovo\"\n",
        ),
    ]);
    let request = Request {
        tests: setting.tests.iter().collect(),
        machines: vec![&setting.qemu],
        image: "dukovo.img".into(),
        build: None,
        store: setting.dir.path().join("store"),
        key: Some(setting.dir.path().join("lab_ed25519")),
        password: None,
        with_person: false,
        linux: None,
        fonts: None,
        browsers: None,
        stick: None,
        dvd: None,
        ob: None,
        preset: None,
        schedule: None,
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
        ..Request::default()
    };
    let ended = run(&request, &Reporter::silent(), &start_fake).unwrap();
    let bundle = crate::store::bundle_dir(&request.store, request.id.as_deref().unwrap());
    let results: crate::bundle::ResultsFile =
        serde_json::from_str(&fs::read_to_string(bundle.join("results.json")).unwrap()).unwrap();
    let by_test = |test: &str| results.results.iter().find(|r| r.test == test).unwrap().clone();

    // The card test does not share the boot before it, which had no
    // reader, and the fake driver refuses a card on a boot with none.
    let card = by_test("drivers/card");
    assert_eq!(card.result, Outcome::Pass, "{:?}", card.reason);
    assert_eq!(card.steps[0].step, "boot afresh and wait until booted");
    assert_eq!(card.steps[1].step, "put a blank memory card of 16M in the card reader");
    assert_eq!(card.steps[2].step, "take the memory card out of the card reader");
    assert_eq!(card.steps[3].step, "put the memory card back in the card reader");
    // The SD card test boots afresh too, for its own reader.
    let sd = by_test("drivers/sd");
    assert_eq!(sd.result, Outcome::Pass, "{:?}", sd.reason);
    assert_eq!(sd.steps[0].step, "boot afresh and wait until booted");
    assert_eq!(sd.steps[1].step, "put a blank memory card of 16M in the SD card slot");
    assert_eq!(sd.steps[2].step, "take the memory card out of the SD card slot");
    // Nor is its boot shared with the test after it.
    let after = by_test("drivers/zz-after");
    assert_eq!(after.steps[0].step, "boot afresh and wait until booted");
    assert_eq!(ended.summary, Summary { passed: 4, failed: 0, skipped: 0, errors: 0 });
}

#[test]
fn disc_tests_boot_with_an_optical_drive_of_their_own() {
    let setting = setting_with(&[
        (
            "drivers/a-boot.toml",
            "about = \"a boot to share\"\ntime = \"1m\"\n[[step]]\nrun = \"uname\"\nexpect = \"Dukovo\"\n",
        ),
        ("drivers/disc.iso", ""),
        (
            "drivers/disc.toml",
            "about = \"a disc in and out\"\ntime = \"1m\"\n\
             [[step]]\ndisc = \"insert\"\nimage = \"disc.iso\"\n[[step]]\ndisc = \"eject\"\n\
             [[step]]\ndisc = \"reinsert\"\n",
        ),
        // Sorts after `disc` in the same category, so it runs next, and
        // would fail had it been given the drive's boot.
        (
            "drivers/zz-after.toml",
            "about = \"after the disc boot\"\ntime = \"1m\"\n[[step]]\nrun = \"uname\"\nexpect = \"Dukovo\"\n",
        ),
    ]);
    let request = Request {
        tests: setting.tests.iter().collect(),
        machines: vec![&setting.qemu],
        image: "dukovo.img".into(),
        build: None,
        store: setting.dir.path().join("store"),
        key: Some(setting.dir.path().join("lab_ed25519")),
        password: None,
        with_person: false,
        linux: None,
        fonts: None,
        browsers: None,
        stick: None,
        dvd: None,
        ob: None,
        preset: None,
        schedule: None,
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
        ..Request::default()
    };
    let ended = run(&request, &Reporter::silent(), &start_fake).unwrap();
    let bundle = crate::store::bundle_dir(&request.store, request.id.as_deref().unwrap());
    let results: crate::bundle::ResultsFile =
        serde_json::from_str(&fs::read_to_string(bundle.join("results.json")).unwrap()).unwrap();
    let by_test = |test: &str| results.results.iter().find(|r| r.test == test).unwrap().clone();

    // The disc test does not share the boot before it, which had no
    // drive, and the fake driver refuses a disc on a boot with none.
    let disc = by_test("drivers/disc");
    assert_eq!(disc.result, Outcome::Pass, "{:?}", disc.reason);
    assert_eq!(disc.steps[0].step, "boot afresh and wait until booted");
    assert_eq!(disc.steps[1].step, "put a disc holding disc.iso in the optical drive");
    assert_eq!(disc.steps[2].step, "take the disc out of the optical drive");
    assert_eq!(disc.steps[3].step, "put the disc back in the optical drive");
    // Nor is its boot shared with the test after it.
    let after = by_test("drivers/zz-after");
    assert_eq!(after.steps[0].step, "boot afresh and wait until booted");
    assert_eq!(ended.summary, Summary { passed: 3, failed: 0, skipped: 0, errors: 0 });
}

#[test]
fn floppy_tests_boot_with_a_floppy_drive_of_their_own() {
    let setting = setting_with(&[
        (
            "drivers/a-boot.toml",
            "about = \"a boot to share\"\ntime = \"1m\"\n[[step]]\nrun = \"uname\"\nexpect = \"Dukovo\"\n",
        ),
        (
            "drivers/floppy.toml",
            "about = \"a floppy disk in and out\"\ntime = \"1m\"\n\
             [[step]]\nfloppy = \"insert\"\nblank = \"1.44M\"\nprotected = true\n\
             [[step]]\nfloppy = \"eject\"\n[[step]]\nfloppy = \"reinsert\"\n",
        ),
        // Sorts after `floppy` in the same category, so it runs next, and
        // would fail had it been given the drive's boot.
        (
            "drivers/zz-after.toml",
            "about = \"after the floppy boot\"\ntime = \"1m\"\n[[step]]\nrun = \"uname\"\nexpect = \"Dukovo\"\n",
        ),
    ]);
    let request = Request {
        tests: setting.tests.iter().collect(),
        machines: vec![&setting.qemu],
        image: "dukovo.img".into(),
        build: None,
        store: setting.dir.path().join("store"),
        key: Some(setting.dir.path().join("lab_ed25519")),
        password: None,
        with_person: false,
        linux: None,
        fonts: None,
        browsers: None,
        stick: None,
        dvd: None,
        ob: None,
        preset: None,
        schedule: None,
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
        ..Request::default()
    };
    let ended = run(&request, &Reporter::silent(), &start_fake).unwrap();
    let bundle = crate::store::bundle_dir(&request.store, request.id.as_deref().unwrap());
    let results: crate::bundle::ResultsFile =
        serde_json::from_str(&fs::read_to_string(bundle.join("results.json")).unwrap()).unwrap();
    let by_test = |test: &str| results.results.iter().find(|r| r.test == test).unwrap().clone();

    // The floppy test does not share the boot before it, which had no
    // drive, and the fake driver refuses a disk on a boot with none.
    let floppy = by_test("drivers/floppy");
    assert_eq!(floppy.result, Outcome::Pass, "{:?}", floppy.reason);
    assert_eq!(floppy.steps[0].step, "boot afresh and wait until booted");
    assert_eq!(
        floppy.steps[1].step,
        "put a blank 1.44M floppy disk, write-protected, in the floppy drive"
    );
    assert_eq!(floppy.steps[2].step, "take the floppy disk out of the floppy drive");
    assert_eq!(floppy.steps[3].step, "put the floppy disk back in the floppy drive");
    // Nor is its boot shared with the test after it.
    let after = by_test("drivers/zz-after");
    assert_eq!(after.steps[0].step, "boot afresh and wait until booted");
    assert_eq!(ended.summary, Summary { passed: 3, failed: 0, skipped: 0, errors: 0 });
}

#[test]
fn screen_steps_read_the_console_type_on_it_and_boot_the_install_stick() {
    let setting = setting_with(&[
        (
            "console/sign-in.toml",
            "about = \"sign in\"\ntime = \"1m\"\n\
             [[step]]\nscreen = [\"(tty1)\", \"Name:\"]\n\
             [[step]]\ntype = \"lab\\n\"\n[[step]]\nscreen = \"Password:\"\n\
             [[step]]\ntype = \"{password}\\n\"\n\
             [[step]]\ntype = \"whoami\\n\"\n[[step]]\nscreen = \"$ whoami\"\n\
             [[step]]\nscreen_gone = \"Name:\"\n[[step]]\nscreenshot = \"signed-in\"\n\
             [[step]]\npoint = [10, 20]\n[[step]]\nclick = \"left\"\n\
             [[step]]\nscreen = \"left click at 10 20\"\n",
        ),
        (
            "console/off-screen.toml",
            "about = \"points past the edge\"\ntime = \"1s\"\n[[step]]\npoint = [100, 0]\n",
        ),
        (
            "console/missing.toml",
            "about = \"misses\"\ntime = \"1s\"\n[[step]]\nscreen = [\"Name:\", \"Nothing like this\"]\n",
        ),
        (
            "console/stays.toml",
            "about = \"stays\"\ntime = \"1s\"\n[[step]]\nscreen_gone = \"lab\"\n",
        ),
        (
            "boot/install.toml",
            "about = \"install\"\ntime = \"1m\"\nboot = \"stick\"\ndisk = \"4G\"\n\
             [[step]]\nserial = \"a blank disk of 4294967296 bytes\"\n\
             [[step]]\nscreen = \"Install Dukovo\"\n[[step]]\ntype = \"\\n\"\n\
             [[step]]\nscreen = \"Installing\"\n[[step]]\nusb = \"unplug\"\n",
        ),
        (
            "boot/install-dvd.toml",
            "about = \"install from the DVD\"\ntime = \"1m\"\nboot = \"dvd\"\n\
             [[step]]\nserial = \"from the DVD, a blank disk of 8589934592 bytes\"\n\
             [[step]]\nscreen = \"Install Dukovo\"\n[[step]]\ndisc = \"eject\"\n",
        ),
        (
            "features/uname.toml",
            "about = \"after the install\"\ntime = \"1m\"\n[[step]]\nrun = \"uname\"\nexpect = \"Dukovo\"\n",
        ),
    ]);
    let fonts = setting.dir.path().join("fonts");
    fs::create_dir_all(&fonts).unwrap();
    fs::write(fonts.join("test-4x6.psfu"), fake_font()).unwrap();
    let stick = setting.dir.write("dukovo.img", "the install stick");
    let dvd = setting.dir.write("dukovo.iso", "the install DVD");
    let request = |fonts: Option<Vec<crate::screen::Font>>,
                   stick: Option<PathBuf>,
                   dvd: Option<PathBuf>| Request {
        tests: setting.tests.iter().collect(),
        machines: vec![&setting.qemu],
        image: "dukovo.img".into(),
        build: None,
        store: setting.dir.path().join("store"),
        key: Some(setting.dir.path().join("lab_ed25519")),
        password: Some("sesame".into()),
        with_person: false,
        linux: None,
        fonts,
        browsers: None,
        stick,
        dvd,
        ob: None,
        preset: None,
        schedule: None,
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
        ..Request::default()
    };
    let with_both = request(
        Some(crate::screen::Font::read_dir(&fonts).unwrap()),
        Some(stick.clone()),
        Some(dvd.clone()),
    );
    let ended = run(&with_both, &Reporter::silent(), &start_fake).unwrap();
    let bundle = crate::store::bundle_dir(&with_both.store, with_both.id.as_deref().unwrap());
    let results: crate::bundle::ResultsFile =
        serde_json::from_str(&fs::read_to_string(bundle.join("results.json")).unwrap()).unwrap();
    let by_test = |test: &str| results.results.iter().find(|r| r.test == test).unwrap().clone();

    // Signed in by keys, the password typed but never shown.
    let sign_in = by_test("console/sign-in");
    assert_eq!(sign_in.result, Outcome::Pass, "{:?}", sign_in.reason);
    assert!(
        sign_in.steps.iter().any(|step| step.step == "type \"{password}\\n\""),
        "{:?}",
        sign_in.steps
    );
    let text =
        fs::read_to_string(bundle.join("screenshots/qemu/console-sign-in-signed-in.txt")).unwrap();
    assert!(text.contains("$ whoami\nlab\n$\n"), "{text}");
    assert!(!fs::read_to_string(bundle.join("results.json")).unwrap().contains("sesame"));

    // A miss names only the missing text first, then shows the screen.
    let missing = by_test("console/missing");
    assert_eq!(missing.result, Outcome::Fail);
    let why = missing.reason.unwrap();
    assert!(
        why.starts_with("step 1: the screen did not show \"Nothing like this\"; it showed:\n"),
        "{why}"
    );
    assert!(why.contains("\nName:\n"), "{why}");
    let stays = by_test("console/stays");
    assert_eq!(stays.result, Outcome::Fail);
    assert!(stays.reason.unwrap().starts_with("step 1: the screen still showed \"lab\":"));
    let off_screen = by_test("console/off-screen");
    assert_eq!(off_screen.result, Outcome::Error);
    assert_eq!(
        off_screen.reason.as_deref(),
        Some("step 1: (100, 0) is off the screen, which is 100x40 pixels")
    );

    // The install stick boots with a blank disk; the test after boots
    // afresh, as the disk no longer holds the image under test.
    let install = by_test("boot/install");
    assert_eq!(install.result, Outcome::Pass, "{:?}", install.reason);
    assert_eq!(install.steps[0].step, "boot from the install stick, with a blank disk");
    let after = by_test("features/uname");
    assert_eq!(after.result, Outcome::Pass);
    assert_eq!(after.steps[0].step, "boot afresh and wait until booted");
    // So does the install DVD, in the optical drive the test is given.
    let from_dvd = by_test("boot/install-dvd");
    assert_eq!(from_dvd.result, Outcome::Pass, "{:?}", from_dvd.reason);
    assert_eq!(from_dvd.steps[0].step, "boot from the install DVD, with a blank disk");
    assert_eq!(ended.summary, Summary { passed: 4, failed: 2, skipped: 0, errors: 1 });

    // Without fonts the screen steps skip, and without a stick or a DVD
    // the installer's tests do, each saying how to get one.
    let ended = run(&request(None, None, None), &Reporter::silent(), &start_fake).unwrap();
    assert_eq!(ended.summary, Summary { passed: 1, failed: 0, skipped: 5, errors: 1 });
}

/// Two tests that each plug in a stick and never pull it: the second
/// fails to plug its own while the first's is in.
const STICK_TESTS: &[(&str, &str)] = &[
    (
        "drivers/a-leaves.toml",
        "about = \"leaves a stick in\"\ntime = \"1m\"\n[[step]]\nusb = \"plug\"\nblank = \"16M\"\n",
    ),
    (
        "drivers/b-plugs.toml",
        "about = \"plugs a stick\"\ntime = \"1m\"\n[[step]]\nusb = \"plug\"\nblank = \"16M\"\n",
    ),
];

fn log_of(done: &Done, result: &ResultRecord) -> String {
    fs::read_to_string(done.bundle.join(result.log.as_ref().unwrap())).unwrap()
}

#[test]
fn a_stick_left_in_is_pulled_when_the_test_ends() {
    let setting = setting_with(STICK_TESTS);
    let (ended, results) = run_on(&setting, vec![&setting.qemu]);
    assert_eq!(
        outcomes(&results, "qemu"),
        [
            ("drivers/a-leaves".into(), Outcome::Pass, Some(1)),
            ("drivers/b-plugs".into(), Outcome::Pass, Some(1)),
        ],
        "the next test shares the boot and plugs its own stick in"
    );
    let log = log_of(&ended, &results[0]);
    assert!(log.contains("The test left USB stick 1 plugged in; the lab took it out."), "{log}");
    let log = log_of(&ended, &results[1]);
    assert!(log.contains("The test left USB stick 2 plugged in; the lab took it out."), "{log}");
}

#[test]
fn a_failed_test_leaves_its_stick_to_the_next_boot() {
    let setting = setting_with(&[
        (
            "drivers/a-fails.toml",
            "about = \"plugs a stick and fails\"\ntime = \"1m\"\n[[step]]\nusb = \"plug\"\nblank = \"16M\"\n\
             [[step]]\nrun = \"false\"\n",
        ),
        STICK_TESTS[1],
    ]);
    let (ended, results) = run_on(&setting, vec![&setting.qemu]);
    assert_eq!(
        outcomes(&results, "qemu"),
        [
            ("drivers/a-fails".into(), Outcome::Fail, Some(1)),
            ("drivers/b-plugs".into(), Outcome::Pass, Some(2)),
        ]
    );
    let log = log_of(&ended, &results[0]);
    assert!(
        !log.contains("the lab took it out"),
        "a failed boot is not shared, so nothing is pulled: {log}"
    );
}

#[test]
fn a_stick_that_will_not_come_out_ends_the_shared_boot() {
    let setting = setting_with(STICK_TESTS);
    let mut jammed = setting.qemu.clone();
    jammed.name = "jammed".into();
    let (ended, results) = run_on(&setting, vec![&jammed]);
    assert_eq!(
        outcomes(&results, "jammed"),
        [
            ("drivers/a-leaves".into(), Outcome::Pass, Some(1)),
            ("drivers/b-plugs".into(), Outcome::Pass, Some(2)),
        ],
        "the next test boots afresh, whose ports are empty"
    );
    let log = log_of(&ended, &results[0]);
    assert!(
        log.contains(
            "The lab could not take out what the test left in (USB stick 1 plugged in: \
             the guest would not let go of the device); the next test boots afresh."
        ),
        "{log}"
    );
}

#[test]
fn machines_run_side_by_side_and_skip_what_they_cannot_do() {
    let setting = setting();
    let mut person = setting.qemu.clone();
    person.name = "person".into();
    person.controls.insert(Control::Cord, Who::Person);
    let mut no_link = setting.qemu.clone();
    no_link.name = "no-link".into();
    no_link.controls.insert(Control::Link, Who::None);
    let mut broken = setting.qemu.clone();
    broken.name = "broken".into();
    let (ended, results) = run_on(&setting, vec![&person, &no_link, &broken]);

    let skip_reason = |machine: &str, test: &str| {
        let result = results.iter().find(|r| r.machine == machine && r.test == test).unwrap();
        assert_eq!(result.result, Outcome::Skip);
        result.reason.clone().unwrap()
    };
    assert_eq!(skip_reason("person", "stress/cord"), "needs a person for cord");
    assert_eq!(skip_reason("no-link", "drivers/link"), "the machine has no link");
    assert!(results.iter().filter(|r| r.machine == "broken").all(|r| r.result == Outcome::Error
        && r.reason.as_deref() == Some("the broken machine did not start: no such machine")));
    assert_eq!(ended.summary, Summary { passed: 6, failed: 4, skipped: 2, errors: 6 });
}

#[test]
fn a_new_boot_is_waited_for_and_an_off_machine_runs_nothing() {
    let setting = setting_with(&[
        (
            "stress/reset.toml",
            "about = \"reset\"\ntime = \"1s\"\nfresh = true\n[[step]]\npower = \"reset\"\n\
             [[step]]\nwait = \"ready\"\n",
        ),
        (
            "stress/off.toml",
            "about = \"off\"\ntime = \"1m\"\nfresh = true\n[[step]]\npower = \"cut\"\n\
             [[step]]\nrun = \"true\"\n",
        ),
    ]);
    let mut stuck = setting.qemu.clone();
    stuck.name = "stuck".into();
    let (_, results) = run_on(&setting, vec![&setting.qemu, &stuck]);
    let reason = |machine: &str, test: &str| {
        let result = results.iter().find(|r| r.machine == machine && r.test == test).unwrap();
        (result.result, result.reason.clone().unwrap_or_default())
    };
    assert_eq!(reason("qemu", "stress/reset").0, Outcome::Pass);
    // The boot before's "Boot complete." does not count for the new one.
    let (result, why) = reason("stuck", "stress/reset");
    assert_eq!(result, Outcome::Fail);
    assert!(why.starts_with("step 2: \"Boot complete.\" did not appear"), "{why}");
    // A machine off when a command must run is Dukovo's failure.
    let (result, why) = reason("qemu", "stress/off");
    assert_eq!(result, Outcome::Fail);
    assert_eq!(why, "step 2: the machine was off, so `true` could not run");
}

#[test]
fn a_stopped_run_skips_what_it_did_not_finish() {
    let setting = setting_with(&[
        ("stress/hang.toml", "about = \"hangs\"\ntime = \"1m\"\n[[step]]\nrun = \"hang\"\n"),
        ("stress/later.toml", "about = \"later\"\ntime = \"1m\"\n[[step]]\nrun = \"true\"\n"),
    ]);
    let id = "20261107-153012-0001";
    let store = setting.dir.path().join("store");
    let asker = store.clone();
    when_events_say(store.clone(), id, "\"step\":\"boot afresh", move || {
        crate::store::request_stop(&asker, id).unwrap();
    });
    let started = Instant::now();
    let (ended, results) = run_with(&setting, vec![&setting.qemu], false, Some(id));
    assert!(started.elapsed().as_secs() < 30, "the hanging command was cut short");
    let reasons: Vec<(&str, Outcome, &str)> = results
        .iter()
        .map(|r| (r.test.as_str(), r.result, r.reason.as_deref().unwrap_or("")))
        .collect();
    assert_eq!(
        reasons,
        [
            ("stress/hang", Outcome::Skip, "the run was stopped during this test"),
            ("stress/later", Outcome::Skip, "the run was stopped before this test"),
        ]
    );
    let run: RunRecord =
        serde_json::from_str(&fs::read_to_string(ended.bundle.join("run.json")).unwrap()).unwrap();
    assert!(run.stopped);
    assert!(!crate::store::is_running(&store, id), "the run let go of its lock");
}

#[test]
fn a_resumed_run_runs_only_what_the_stopped_run_did_not_finish() {
    let setting = setting_with(&[
        ("features/uname.toml", TESTS[1].1),
        ("stress/hang.toml", "about = \"hangs\"\ntime = \"1s\"\n[[step]]\nrun = \"hang\"\n"),
        ("stress/later.toml", "about = \"later\"\ntime = \"1m\"\n[[step]]\nrun = \"true\"\n"),
    ]);
    let id = "20261107-153012-0002";
    let store = setting.dir.path().join("store");
    let asker = store.clone();
    when_events_say(
        store.clone(),
        id,
        "\"event\":\"test-started\",\"machine\":\"qemu\",\"test\":\"stress/hang\"",
        move || crate::store::request_stop(&asker, id).unwrap(),
    );
    let (_, first) = run_with(&setting, vec![&setting.qemu], true, Some(id));
    assert_eq!(
        first.iter().map(|r| r.result).collect::<Vec<_>>(),
        [Outcome::Pass, Outcome::Skip, Outcome::Skip]
    );

    let image = Path::new("dukovo.img");
    let resume = crate::resume::plan(&store, id, image, Some("0.13.0 test")).unwrap();
    let names: Vec<String> = resume.tests().into_iter().map(String::from).collect();
    let request = Request {
        tests: crate::suite::select(&setting.tests, &names).unwrap(),
        machines: vec![&setting.qemu],
        image: image.into(),
        build: Some("0.13.0 test".into()),
        store: store.clone(),
        key: Some(setting.dir.path().join("lab_ed25519")),
        with_person: true,
        resume: Some(resume),
        id: Some("20261107-153013-0003".into()),
        ..Request::default()
    };
    run(&request, &Reporter::silent(), &start_fake).unwrap();
    let bundle = crate::store::bundle_dir(&store, "20261107-153013-0003");
    let results: crate::bundle::ResultsFile =
        serde_json::from_str(&fs::read_to_string(bundle.join("results.json")).unwrap()).unwrap();
    assert_eq!(
        outcomes(&results.results, "qemu"),
        [
            ("stress/hang".into(), Outcome::Fail, Some(1)),
            ("stress/later".into(), Outcome::Pass, Some(2)),
        ]
    );
    let run: RunRecord =
        serde_json::from_str(&fs::read_to_string(bundle.join("run.json")).unwrap()).unwrap();
    assert_eq!(run.resumes.as_deref(), Some(id));
    assert_eq!(run.tests, ["stress/hang", "stress/later"]);
    assert_eq!(run.build.as_deref(), Some("0.13.0 test"));
    assert!(run.options.unwrap().with_person, "the options the run had are recorded");
}

#[test]
fn a_needs_you_step_waits_for_a_person() {
    let setting = setting_with(&[(
        "drivers/link.toml",
        "about = \"link\"\ntime = \"2s\"\n[[step]]\nlink = \"down\"\n[[step]]\nrun = \"true\"\n",
    )]);
    let mut person = setting.qemu.clone();
    person.name = "person".into();
    person.controls.insert(Control::Link, Who::Person);

    // Without --with-person, it is skipped.
    let (_, results) = run_with(&setting, vec![&person], false, Some("20261107-153012-0002"));
    assert_eq!(results[0].reason.as_deref(), Some("needs a person for link"));

    // With it, the run asks and waits for `done`.
    let id = "20261107-153012-0003";
    let store = setting.dir.path().join("store");
    let teller = store.clone();
    when_events_say(store, id, "person-needed", move || {
        crate::store::say_done(&teller, id, "person").unwrap();
    });
    let (ended, results) = run_with(&setting, vec![&person], true, Some(id));
    assert_eq!(results[0].result, Outcome::Pass, "{:?}", results[0].reason);
    assert_eq!(results[0].steps[1].note.as_deref(), Some("a person said it was done"));
    let events = fs::read_to_string(ended.bundle.join("events.jsonl")).unwrap();
    assert!(events.contains(&format!("\"done\":\"dukovo-lab done {id} --machine person\"")));

    // No one answers: skipped at the time limit, not failed.
    let (_, results) = run_with(&setting, vec![&person], true, Some("20261107-153012-0004"));
    assert_eq!(results[0].result, Outcome::Skip);
    let why = results[0].reason.as_deref().unwrap();
    assert!(why.contains("no one said \"pull the network cable\" was done"), "{why}");
}

#[test]
fn a_long_step_reports_it_is_still_running() {
    // The hang step runs to its one-second limit. The beat is 100ms under
    // test, so events.jsonl gains step-progress lines while it runs, naming
    // the step and how long it has run.
    let setting = setting_with(&[(
        "stress/hang.toml",
        "about = \"hangs\"\ntime = \"1s\"\n[[step]]\nrun = \"hang\"\n",
    )]);
    let (ended, _) = run_on(&setting, vec![&setting.qemu]);
    let events = fs::read_to_string(ended.bundle.join("events.jsonl")).unwrap();
    let beats: Vec<&str> =
        events.lines().filter(|line| line.contains("\"event\":\"step-progress\"")).collect();
    assert!(!beats.is_empty(), "a long step should report it is still running:\n{events}");
    assert!(
        beats.iter().all(|line| line.contains("\"test\":\"stress/hang\"")),
        "every beat names the running test: {beats:?}"
    );
    assert!(beats[0].contains("run `hang`"), "the beat names the step: {}", beats[0]);
    assert!(beats[0].contains("\"milliseconds\":"), "the beat says how long: {}", beats[0]);
}

#[test]
fn a_step_that_panics_does_not_hang_the_run() {
    // A panic inside a step must reach the caller, not leave the run
    // waiting for ever on the step's heartbeat. The run goes on its own
    // thread, so a hang shows as a failed wait here rather than a stuck
    // test.
    let (sent, got) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let setting = setting_with(&[(
            "stress/broken.toml",
            "about = \"breaks\"\ntime = \"1m\"\n[[step]]\nrun = \"panic\"\n",
        )]);
        let ended = std::panic::catch_unwind(|| run_on(&setting, vec![&setting.qemu]));
        let _ = sent.send(ended.is_err());
    });
    let panicked = got
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("a panicking step should end the run, not hang it");
    assert!(panicked, "the step's panic should reach the caller");
}

#[test]
fn host_steps_run_the_lab_pc_side_and_end_its_servers() {
    let setting = setting_with(&[
        (
            "features/peer.toml",
            "about = \"a server on the lab PC, reached both ways\"\ntime = \"1m\"\n\
             host_needs = [\"sh\"]\n\
             [[step]]\nhost = \"echo started > server.txt; echo $$ > pid; echo ready; exec sleep 30\"\n\
             background = true\nready = \"ready\"\n\
             [[step]]\nhost = \"cat server.txt; pwd; echo here is {here}\"\n\
             expect = [\"started\", \"{work}\", \"here is \"]\n\
             [[step]]\nrun = \"reach {peer}:{port:kdc}\"\nexpect = \"reached 10.0.2.2:\"\n\
             [[step]]\nhost = \"echo {machine}:{machine-port:445} {port:kdc}\"\nexpect = \"127.0.0.1:40445\"\n\
             [[step]]\nrun = \"cat\"\ninput = \"realm at {peer}:{port:kdc} for {password}\\n\"\n\
             expect = \"realm at 10.0.2.2:\"\nexpect_not = \"sesame\"\n\
             [[step]]\nhost = \"read word; echo heard $word; exit 2\"\ninput = \"lab\\n\"\n\
             exit = 2\nexpect = \"heard lab\"\n",
        ),
        (
            "features/peer-missing.toml",
            "about = \"needs a server the lab PC lacks\"\ntime = \"1m\"\n\
             host_needs = [\"no-such-server-anywhere\"]\n\
             [[step]]\nhost = \"true\"\n",
        ),
        (
            "features/peer-cannot.toml",
            "about = \"the lab PC cannot do its part\"\ntime = \"1m\"\n\
             [[step]]\nhost = \"echo this kernel has no NFS client; exit 77\"\n\
             [[step]]\nrun = \"uname\"\n",
        ),
        (
            "features/peer-broken.toml",
            "about = \"the lab PC's server breaks\"\ntime = \"1m\"\n\
             [[step]]\nhost = \"echo no such realm; exit 1\"\nbackground = true\nready = \"ready\"\n",
        ),
    ]);
    let request = Request {
        tests: setting.tests.iter().collect(),
        machines: vec![&setting.qemu],
        image: "dukovo.img".into(),
        build: None,
        store: setting.dir.path().join("store"),
        key: Some(setting.dir.path().join("lab_ed25519")),
        password: Some("sesame".into()),
        with_person: false,
        linux: None,
        fonts: None,
        browsers: None,
        stick: None,
        dvd: None,
        ob: None,
        preset: None,
        schedule: None,
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
        ..Request::default()
    };
    assert!(setting.tests.iter().all(|test| test.controls().contains(&Control::Peer)));
    let ended = run(&request, &Reporter::silent(), &start_fake).unwrap();
    let bundle = crate::store::bundle_dir(&request.store, request.id.as_deref().unwrap());
    let results: crate::bundle::ResultsFile =
        serde_json::from_str(&fs::read_to_string(bundle.join("results.json")).unwrap()).unwrap();
    let by_test = |test: &str| results.results.iter().find(|r| r.test == test).unwrap().clone();

    let peer = by_test("features/peer");
    assert_eq!(peer.result, Outcome::Pass, "{:?}", peer.reason);
    let log = fs::read_to_string(bundle.join(peer.log.as_ref().unwrap())).unwrap();
    // The server ran in the test's folder until the test ended, and its
    // output was kept.
    assert!(log.contains("Lab PC server `echo started"), "{log}");
    assert!(log.contains("ended at the test's end"), "{log}");
    let server_log = bundle.join("logs/qemu/features-peer-host-1.log");
    assert_eq!(fs::read_to_string(&server_log).unwrap(), "ready\n");
    // The lab password reached the command, but no log shows it.
    assert!(log.contains("for ********"), "{log}");
    assert!(log.contains("lab-pc $ echo 127.0.0.1:40445"), "{log}");
    assert!(log.contains("here is /"), "{{here}} is a whole path: {log}");

    let missing = by_test("features/peer-missing");
    assert_eq!(missing.result, Outcome::Skip);
    assert!(missing.reason.as_ref().unwrap().contains("no-such-server-anywhere"));

    let cannot = by_test("features/peer-cannot");
    assert_eq!(cannot.result, Outcome::Skip);
    assert!(cannot.reason.as_ref().unwrap().contains("this kernel has no NFS client"));

    let broken = by_test("features/peer-broken");
    assert_eq!(broken.result, Outcome::Error);
    let reason = broken.reason.as_ref().unwrap();
    assert!(reason.contains("ended (status 1) before it said \"ready\""), "{reason}");
    assert!(reason.contains("no such realm"), "{reason}");
    assert_eq!(ended.summary, Summary { passed: 1, failed: 0, skipped: 2, errors: 1 });
}

/// A browser kit whose Playwright opens nothing: its chromium starts, its
/// firefox is not installed, and its webkit will not start.
const FAKE_PLAYWRIGHT: &str = r#"
const fs = require("fs");
const page = {
  goto: async () => {},
  isClosed: () => false,
  screenshot: async ({ path }) => fs.writeFileSync(path, "png"),
};
const context = {
  tracing: { start: async () => {}, stop: async (saved) => saved && fs.writeFileSync(saved.path, "trace") },
  newPage: async () => page,
  on: () => {},
};
const started = { newContext: async () => context, close: async () => {} };
module.exports = {
  errors: { TimeoutError: class extends Error {} },
  chromium: { executablePath: () => process.execPath, launch: async () => started },
  firefox: { executablePath: () => "/no/such/firefox" },
  webkit: {
    executablePath: () => process.execPath,
    launch: async () => { throw new Error("missing libraries\nsecond line"); },
  },
};
"#;

#[test]
fn browser_steps_run_the_runner_in_each_engine_and_skip_without_a_kit() {
    let step = |script: &str, engines: &str| {
        format!(
            "about = \"drives a page\"\ntime = \"1m\"\n[[step]]\nbrowser = \"{script}\"\n\
             port = 443\n{engines}"
        )
    };
    let setting = setting_with(&[
        ("features/pages.toml", &step("pages.mjs", "")),
        (
            "features/pages.mjs",
            "export default async function check(t) {\n\
             if (t.url !== \"https://127.0.0.1:40443\") throw new Error(`url ${t.url}`);\n\
             await t.screenshot(await t.open(\"/page.html\"), \"start\");\n}\n",
        ),
        ("features/pages-broken.toml", &step("broken.mjs", "engines = [\"chromium\"]\n")),
        (
            "features/broken.mjs",
            "export default async (t) => { await t.open(\"/\"); throw new Error(\"the heading is not visible\"); };\n",
        ),
        (
            "features/pages-skipped.toml",
            &step("pages.mjs", "engines = [\"firefox\", \"webkit\"]\n"),
        ),
    ]);
    setting.dir.write("kit/node_modules/playwright/index.js", FAKE_PLAYWRIGHT);
    setting.dir.write("kit/node_modules/axe-core/package.json", "{ \"main\": \"axe.js\" }");
    setting.dir.write("kit/node_modules/axe-core/axe.js", "");
    let request = |browsers: Option<PathBuf>| Request {
        tests: setting.tests.iter().collect(),
        machines: vec![&setting.qemu],
        image: "dukovo.img".into(),
        build: None,
        store: setting.dir.path().join("store"),
        key: Some(setting.dir.path().join("lab_ed25519")),
        password: None,
        with_person: false,
        linux: None,
        fonts: None,
        browsers,
        stick: None,
        dvd: None,
        ob: None,
        preset: None,
        schedule: None,
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
        ..Request::default()
    };
    let results_of = |request: &Request<'_>| {
        run(request, &Reporter::silent(), &start_fake).unwrap();
        let bundle = crate::store::bundle_dir(&request.store, request.id.as_deref().unwrap());
        let results: crate::bundle::ResultsFile =
            serde_json::from_str(&fs::read_to_string(bundle.join("results.json")).unwrap())
                .unwrap();
        (bundle, results.results)
    };
    let by_test = |results: &[ResultRecord], test: &str| {
        results.iter().find(|result| result.test == test).unwrap().clone()
    };

    let (bundle, results) = results_of(&request(Some(setting.dir.path().join("kit"))));
    let pages = by_test(&results, "features/pages");
    assert_eq!(pages.result, Outcome::Pass, "{:?}", pages.reason);
    let note = pages.steps[1].note.as_ref().unwrap();
    assert!(
        note.starts_with(
            "passed in chromium; skipped firefox (firefox is not in the browser kit), \
             webkit (webkit would not start on the lab PC: missing libraries)\n"
        ),
        "{note}"
    );
    assert_eq!(pages.screenshots, ["screenshots/qemu/features-pages-chromium-start.png"]);
    assert!(bundle.join(&pages.screenshots[0]).is_file());

    let broken = by_test(&results, "features/pages-broken");
    assert_eq!(broken.result, Outcome::Fail);
    assert!(
        broken
            .reason
            .as_ref()
            .unwrap()
            .starts_with("step 1: chromium: the heading is not visible\n"),
        "{:?}",
        broken.reason
    );
    // The machine's own screen at the failure comes after the engine's.
    assert_eq!(
        broken.screenshots[0],
        "screenshots/qemu/features-pages-broken-chromium-failed-1.png"
    );
    assert!(bundle.join("logs/qemu/features-pages-broken-chromium-trace-1.zip").is_file());

    let skipped = by_test(&results, "features/pages-skipped");
    assert_eq!(skipped.result, Outcome::Skip);
    assert_eq!(
        skipped.reason.as_deref(),
        Some(
            "step 1: firefox: firefox is not in the browser kit; \
             webkit: webkit would not start on the lab PC: missing libraries"
        )
    );

    // Without a kit, every test that drives a browser is skipped.
    let (_, results) = results_of(&request(None));
    for test in ["features/pages", "features/pages-broken", "features/pages-skipped"] {
        let result = by_test(&results, test);
        assert_eq!(result.result, Outcome::Skip, "{test}");
        assert_eq!(result.reason, Some(format!("step 1: {NO_BROWSERS}")), "{test}");
    }
}

#[test]
fn a_run_step_may_not_name_the_lab_pc_s_own_places() {
    let dir = TempDir::new("peer-places");
    for (step, wrong) in [
        ("run = \"ls {work}\"", "{work} names something of the lab PC's"),
        ("run = \"nc {machine} 22\"", "{machine} names something of the lab PC's"),
        ("attack = \"nc {peer} 88\"", "{peer} goes in run and host steps"),
        ("run = \"nc {peer} {port:}\"", "a port's name is"),
        (
            "host = \"smbd\"\nbackground = true\nexpect = \"x\"",
            "expect does not go with background",
        ),
        ("host = \"smbd\"\nready = \"x\"", "ready goes only with host and background"),
        ("run = \"x\"\nbackground = true", "background goes only with host"),
        ("attack = \"x\"\ninput = \"y\"", "input goes only with run or host"),
    ] {
        dir.write(
            "tests/features/wrong.toml",
            &format!("about = \"x\"\ntime = \"1m\"\n[[step]]\n{step}\n"),
        );
        let err = format!("{:?}", crate::suite::load(&dir.path().join("tests")).unwrap_err());
        assert!(err.contains(wrong), "{step}: {err}");
    }
}

#[test]
fn a_real_machine_s_root_grows_after_a_fresh_boot_and_before_the_test() {
    let setting = setting_with(&[(
        "features/uname.toml",
        "about = \"uname\"\ntime = \"1m\"\nfresh = true\n[[step]]\nrun = \"uname\"\nexpect = \"Dukovo\"\n",
    )]);
    let mut grows = setting.qemu.clone();
    grows.name = "grows".into();
    // A machine whose driver does not grow the root (QEMU's, or a real
    // machine file with grow_root = false) never gets the command.
    let (done, results) = run_with_password(&setting, vec![&grows, &setting.qemu]);
    assert_eq!(done.summary, Summary { passed: 2, failed: 0, skipped: 0, errors: 0 });
    assert_eq!(results.len(), 2);
    let driver_log = |machine: &str| {
        fs::read_to_string(done.bundle.join(format!("logs/{machine}/driver.log"))).unwrap()
    };
    let grown = driver_log("grows");
    let lines: Vec<&str> = grown.lines().filter(|line| line.starts_with("ran ")).collect();
    assert_eq!(lines.len(), 3, "{grown}");
    assert_eq!(lines[0], "ran true");
    assert_eq!(lines[1], format!("ran elevate -S {GROW_ROOT}"));
    assert_eq!(lines[2], "ran uname");
    assert!(grown.contains("grow root: # sh -c 'part="), "{grown}");
    assert!(grown.contains("storage: grown"), "{grown}");
    assert!(!grown.contains("sesame"), "{grown}");
    assert!(!driver_log("qemu").contains("storage"));
}

#[test]
fn a_real_machine_s_clock_is_set_to_the_lab_pc_s_after_a_fresh_boot() {
    let setting = setting_with(&[(
        "features/uname.toml",
        "about = \"uname\"\ntime = \"1m\"\nfresh = true\n[[step]]\nrun = \"uname\"\nexpect = \"Dukovo\"\n",
    )]);
    let mut clocked = setting.qemu.clone();
    clocked.name = "clocked".into();
    let before = crate::clock::Utc::now().timedatectl();
    let (done, _) = run_with_password(&setting, vec![&clocked, &setting.qemu]);
    assert_eq!(done.summary, Summary { passed: 2, failed: 0, skipped: 0, errors: 0 });
    let log = fs::read_to_string(done.bundle.join("logs/clocked/driver.log")).unwrap();
    let lines: Vec<&str> = log.lines().filter(|line| line.starts_with("ran ")).collect();
    assert_eq!(lines.len(), 3, "{log}");
    assert_eq!(lines[0], "ran true");
    // The lab PC's time, to the minute: the run takes less than one.
    let set = lines[1].strip_prefix("ran elevate -S timedatectl set-time '").unwrap();
    assert_eq!(set[..16], before[..16], "{log}");
    assert_eq!(lines[2], "ran uname");
    let qemu = fs::read_to_string(done.bundle.join("logs/qemu/driver.log")).unwrap();
    assert!(!qemu.contains("timedatectl"), "{qemu}");
}

#[test]
fn a_root_that_does_not_grow_fails_the_fresh_boot_with_the_command_s_output() {
    let setting = setting_with(&[(
        "features/uname.toml",
        "about = \"uname\"\ntime = \"1m\"\nfresh = true\n[[step]]\nrun = \"uname\"\nexpect = \"Dukovo\"\n",
    )]);
    let mut grows = setting.qemu.clone();
    grows.name = "grows-badly".into();
    let (done, results) = run_with_password(&setting, vec![&grows]);
    assert_eq!(done.summary, Summary { passed: 0, failed: 0, skipped: 0, errors: 1 });
    let why = results[0].reason.clone().unwrap();
    assert!(why.contains("could not be grown"), "{why}");
    assert!(why.contains("storage: no room after the root"), "{why}");
    // The test's own step never ran.
    let log = fs::read_to_string(done.bundle.join("logs/grows-badly/driver.log")).unwrap();
    assert!(!log.contains("ran uname"), "{log}");
}

#[test]
fn growing_the_root_needs_the_lab_password() {
    let setting = setting_with(&[(
        "features/uname.toml",
        "about = \"uname\"\ntime = \"1m\"\nfresh = true\n[[step]]\nrun = \"uname\"\nexpect = \"Dukovo\"\n",
    )]);
    let mut grows = setting.qemu.clone();
    grows.name = "grows".into();
    let (done, results) = run_on(&setting, vec![&grows]);
    assert_eq!(done.summary, Summary { passed: 0, failed: 0, skipped: 0, errors: 1 });
    let why = results[0].reason.clone().unwrap();
    assert!(why.contains("growing the root needs the lab's password"), "{why}");
}

/// A run with the lab password, which a root step gives `elevate`.
fn run_with_password(setting: &Setting, machines: Vec<&Machine>) -> (Done, Vec<ResultRecord>) {
    let request = Request {
        tests: setting.tests.iter().collect(),
        machines,
        image: "dukovo.img".into(),
        build: None,
        store: setting.dir.path().join("store"),
        key: Some(setting.dir.path().join("lab_ed25519")),
        password: Some("sesame".into()),
        with_person: false,
        linux: None,
        fonts: None,
        browsers: None,
        stick: None,
        dvd: None,
        ob: None,
        preset: None,
        schedule: None,
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
        ..Request::default()
    };
    let ended = run(&request, &Reporter::silent(), &start_fake).unwrap();
    let bundle = crate::store::bundle_dir(&request.store, request.id.as_deref().unwrap());
    let results: crate::bundle::ResultsFile =
        serde_json::from_str(&fs::read_to_string(bundle.join("results.json")).unwrap()).unwrap();
    (Done { summary: ended.summary, bundle }, results.results)
}

/// Three tests that each boot afresh, so each meets the health check.
const FRESH_TESTS: &[(&str, &str)] = &[
    (
        "boot/a.toml",
        "about = \"a\"\ntime = \"1m\"\nfresh = true\n[[step]]\nrun = \"uname\"\nexpect = \"Dukovo\"\n",
    ),
    (
        "boot/b.toml",
        "about = \"b\"\ntime = \"1m\"\nfresh = true\n[[step]]\nrun = \"uname\"\nexpect = \"Dukovo\"\n",
    ),
    (
        "boot/c.toml",
        "about = \"c\"\ntime = \"1m\"\nfresh = true\n[[step]]\nrun = \"uname\"\nexpect = \"Dukovo\"\n",
    ),
];

fn named(setting: &Setting, name: &str) -> Machine {
    let mut machine = setting.qemu.clone();
    machine.name = name.into();
    machine
}

#[test]
fn a_healthy_machine_passes_its_health_check_and_the_log_says_so() {
    let setting = setting_with(&FRESH_TESTS[..1]);
    let checked = named(&setting, "checked");
    let (done, results) = run_with_password(&setting, vec![&checked, &setting.qemu]);
    assert_eq!(done.summary, Summary { passed: 2, failed: 0, skipped: 0, errors: 0 });
    let log = log_of(&done, results.iter().find(|result| result.machine == "checked").unwrap());
    let line = log.lines().find(|line| line.starts_with("Health check")).expect(&log);
    assert!(
        line.starts_with("Health check after boot 1: root 5859 MiB free; clock ")
            && line.ends_with("s off the lab PC's: ok"),
        "{line}"
    );
    // A machine whose driver does not check never gets the command.
    let qemu = results.iter().find(|result| result.machine == "qemu").unwrap();
    assert!(!log_of(&done, qemu).contains("Health check"));
}

#[test]
fn a_failed_health_check_is_a_lab_error_and_the_next_fresh_boot_is_tried() {
    let setting = setting_with(FRESH_TESTS);
    let checked = named(&setting, "checked-once");
    let (done, results) = run_with_password(&setting, vec![&checked]);
    assert_eq!(done.summary, Summary { passed: 2, failed: 0, skipped: 0, errors: 1 });
    assert_eq!(
        outcomes(&results, "checked-once"),
        [
            ("boot/a".into(), Outcome::Error, Some(1)),
            ("boot/b".into(), Outcome::Pass, Some(2)),
            ("boot/c".into(), Outcome::Pass, Some(3)),
        ]
    );
    let why = results[0].reason.as_deref().unwrap();
    assert!(
        why.starts_with(
            "health check failed after the fresh boot: root has 300 MiB free, \
             under the 1024 MiB needed. "
        ) && why.contains("`storage grow`"),
        "{why}"
    );
    let log = log_of(&done, &results[0]);
    assert!(log.contains("Health check after boot 1: root 300 MiB free;"), "{log}");
    assert!(log.contains("FAILED"), "{log}");
    assert!(!log.contains("benched"), "{log}");
}

#[test]
fn a_machine_that_fails_two_fresh_boots_in_a_row_is_benched_and_the_others_carry_on() {
    let setting = setting_with(FRESH_TESTS);
    let bad = named(&setting, "checked-bad");
    let (done, results) = run_with_password(&setting, vec![&bad, &setting.qemu]);
    assert_eq!(done.summary, Summary { passed: 3, failed: 0, skipped: 1, errors: 2 });
    let bad_results: Vec<&ResultRecord> =
        results.iter().filter(|result| result.machine == "checked-bad").collect();
    assert_eq!(bad_results[0].result, Outcome::Error);
    assert_eq!(bad_results[1].result, Outcome::Error);
    let why = bad_results[0].reason.as_deref().unwrap();
    assert!(
        why.contains("clock 4") && why.contains("s off the lab PC's, over the 5 s allowed"),
        "{why}"
    );
    assert!(why.contains("`timedatectl set-time`"), "{why}");
    let benched = bad_results[2];
    assert_eq!(benched.result, Outcome::Skip);
    assert_eq!(benched.boot, None);
    let reason = benched.reason.as_deref().unwrap();
    assert!(
        reason.starts_with("checked-bad was benched: its health check failed twice (clock 4")
            && reason.ends_with("s off the lab PC's, over the 5 s allowed)"),
        "{reason}"
    );
    assert!(
        log_of(&done, bad_results[1]).contains("checked-bad is benched for the rest of the run.")
    );
    assert_eq!(
        outcomes(&results, "qemu").iter().map(|(_, outcome, _)| *outcome).collect::<Vec<_>>(),
        [Outcome::Pass; 3]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn each_step_records_the_lab_pc_s_load() {
    let setting = setting_with(&FRESH_TESTS[..1]);
    let (_, results) = run_on(&setting, vec![&setting.qemu]);
    assert!(!results[0].steps.is_empty());
    assert!(results[0].steps.iter().all(|step| step.lab_pc_load.is_some()), "{:?}", results[0]);
}

/// A test whose one step is `step`, under `header`'s keys.
fn stall_test(header: &str, step: &str) -> Setting {
    setting_with(&[(
        "stress/quiet.toml",
        &format!("about = \"quiet\"\n{header}[[step]]\n{step}\n"),
    )])
}

#[test]
fn a_command_that_goes_silent_ends_as_stalled_and_fails_the_test() {
    let setting = stall_test("time = \"1m\"\nstall = \"1s\"\n", "run = \"hang\"");
    let (_, results) = run_on(&setting, vec![&setting.qemu]);
    let result = &results[0];
    assert_eq!(result.result, Outcome::Fail);
    assert!(result.milliseconds < 20_000, "{} ms", result.milliseconds);
    let reason = result.reason.as_deref().unwrap();
    assert!(
        reason.starts_with("step 1: stalled: no output for 1 s\nthe last console line was \"")
            && reason.contains("$ hang"),
        "{reason}"
    );
    // The same hang is one finding, whatever the console's last line was.
    assert_eq!(crate::findings::signature(reason), "step 1: stalled: no output for 1 s");
}

#[test]
fn a_command_is_not_stalled_while_the_console_scrolls() {
    let setting = stall_test("time = \"2s\"\nstall = \"1s\"\n", "run = \"chatter\"");
    let (_, results) = run_on(&setting, vec![&setting.qemu]);
    let reason = results[0].reason.as_deref().unwrap();
    assert!(reason.contains("it was still running at the test's time limit"), "{reason}");
    assert!(!reason.contains("stalled"), "{reason}");
}

#[test]
fn a_wait_for_console_text_that_never_comes_ends_as_stalled() {
    let setting = stall_test("time = \"1m\"\nstall = \"1s\"\n", "serial = \"never printed\"");
    let (_, results) = run_on(&setting, vec![&setting.qemu]);
    assert_eq!(results[0].result, Outcome::Fail);
    let reason = results[0].reason.as_deref().unwrap();
    assert!(reason.starts_with("step 1: stalled: no output for 1 s\n"), "{reason}");
    assert!(reason.contains("waiting for \"never printed\""), "{reason}");
}

#[test]
fn stall_off_and_steps_quiet_by_design_are_never_cut() {
    // A wait is quiet by design, whatever the test's stall.
    let setting = stall_test("time = \"1m\"\nstall = \"1s\"\n", "wait = \"2s\"");
    let (_, results) = run_on(&setting, vec![&setting.qemu]);
    assert_eq!(results[0].result, Outcome::Pass, "{:?}", results[0].reason);
    // `stall = "off"` waits out the time limit.
    let setting = stall_test("time = \"3s\"\nstall = \"off\"\n", "run = \"hang\"");
    let (_, results) = run_on(&setting, vec![&setting.qemu]);
    let reason = results[0].reason.as_deref().unwrap();
    assert!(reason.contains("still running at the test's time limit"), "{reason}");
    // A step's own stall wins over the test's.
    let setting = stall_test("time = \"1m\"\nstall = \"off\"\n", "run = \"hang\"\nstall = \"1s\"");
    let (_, results) = run_on(&setting, vec![&setting.qemu]);
    let reason = results[0].reason.as_deref().unwrap();
    assert!(reason.starts_with("step 1: stalled: no output for 1 s"), "{reason}");
}

/// Tests of one step each, which take a pool's machines a moment so that
/// each gets a turn at the queue.
fn slow_tests(count: usize) -> Setting {
    let tests: Vec<(String, String)> = (1..=count)
        .map(|number| {
            (
                format!("boot/t{number}.toml"),
                format!(
                    "about = \"t{number}\"\ntime = \"1m\"\nfresh = true\n[[step]]\nrun = \"slow\"\n"
                ),
            )
        })
        .collect();
    let tests: Vec<(&str, &str)> =
        tests.iter().map(|(file, text)| (file.as_str(), text.as_str())).collect();
    setting_with(&tests)
}

fn optiplex<'a>(members: Vec<&'a Machine>) -> Pool<'a> {
    Pool { name: "optiplex".into(), members, spares: Vec::new() }
}

/// A run that shares tests out among pools, as `--machine pool:...` does,
/// given the lab password that machines named `checked...` need.
fn run_pools(
    setting: &Setting,
    machines: Vec<&Machine>,
    pools: Vec<Pool<'_>>,
    typical: HashMap<(String, String), u64>,
) -> (Done, Vec<ResultRecord>) {
    let request = Request {
        tests: setting.tests.iter().collect(),
        machines,
        pools,
        typical,
        image: "dukovo.img".into(),
        store: setting.dir.path().join("store"),
        key: Some(setting.dir.path().join("lab_ed25519")),
        password: Some("sesame".into()),
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
        ..Request::default()
    };
    let ended = run(&request, &Reporter::silent(), &start_fake).unwrap();
    let bundle = crate::store::bundle_dir(&request.store, request.id.as_deref().unwrap());
    let results: crate::bundle::ResultsFile =
        serde_json::from_str(&fs::read_to_string(bundle.join("results.json")).unwrap()).unwrap();
    (Done { summary: ended.summary, bundle }, results.results)
}

/// The tests each machine ran, sorted, by machine.
fn ran_by(results: &[ResultRecord]) -> BTreeMap<&str, Vec<&str>> {
    let mut by: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for result in results {
        by.entry(&result.machine).or_default().push(&result.test);
    }
    by.values_mut().for_each(|tests| tests.sort_unstable());
    by
}

#[test]
fn a_pool_of_three_shares_nine_tests_and_runs_each_once() {
    let setting = slow_tests(9);
    let members: Vec<Machine> =
        ["optiplex-1", "optiplex-2", "optiplex-3"].map(|name| named(&setting, name)).into();
    let (done, results) =
        run_pools(&setting, Vec::new(), vec![optiplex(members.iter().collect())], HashMap::new());
    assert_eq!(done.summary, Summary { passed: 9, failed: 0, skipped: 0, errors: 0 });
    let mut tests: Vec<&str> = results.iter().map(|result| result.test.as_str()).collect();
    tests.sort_unstable();
    tests.dedup();
    assert_eq!(results.len(), 9, "no test ran twice");
    assert_eq!(tests.len(), 9, "every test ran");
    let by = ran_by(&results);
    assert!(by.len() >= 2, "more than one machine took tests: {by:?}");
    assert!(by.keys().all(|machine| machine.starts_with("optiplex-")), "{by:?}");

    let run: RunRecord =
        serde_json::from_str(&fs::read_to_string(done.bundle.join("run.json")).unwrap()).unwrap();
    assert_eq!(run.pools.len(), 1);
    assert_eq!(run.pools[0].name, "optiplex");
    assert_eq!(run.pools[0].members, ["optiplex-1", "optiplex-2", "optiplex-3"]);
    assert_eq!(run.machines.len(), 3);
}

#[test]
fn a_pool_runs_its_longest_tests_first() {
    let setting = setting_with(&[
        ("boot/a.toml", "about = \"a\"\ntime = \"1m\"\n[[step]]\nrun = \"true\"\n"),
        ("boot/b.toml", "about = \"b\"\ntime = \"1m\"\n[[step]]\nrun = \"true\"\n"),
        ("boot/c.toml", "about = \"c\"\ntime = \"1m\"\n[[step]]\nrun = \"true\"\n"),
        ("boot/d.toml", "about = \"d\"\ntime = \"1m\"\n[[step]]\nrun = \"true\"\n"),
    ]);
    let member = named(&setting, "optiplex-1");
    // A test with no time yet counts as its time limit, 60 s, so c starts
    // before the tests known to be shorter.
    let typical = HashMap::from([
        (("optiplex-1".to_string(), "boot/a".to_string()), 2_000),
        (("optiplex-1".to_string(), "boot/b".to_string()), 90_000),
        (("optiplex-1".to_string(), "boot/d".to_string()), 30_000),
    ]);
    let (_, results) = run_pools(&setting, Vec::new(), vec![optiplex(vec![&member])], typical);
    let order: Vec<&str> = results.iter().map(|result| result.test.as_str()).collect();
    assert_eq!(order, ["boot/b", "boot/c", "boot/d", "boot/a"]);
}

#[test]
fn a_benched_machine_s_queue_is_taken_by_the_rest_of_the_pool() {
    let setting = slow_tests(6);
    let bad = named(&setting, "checked-bad");
    let good = named(&setting, "optiplex-2");
    let (done, results) =
        run_pools(&setting, Vec::new(), vec![optiplex(vec![&bad, &good])], HashMap::new());
    let by = ran_by(&results);
    assert_eq!(by["checked-bad"].len(), 2, "it takes tests until its health check benches it");
    assert_eq!(by["optiplex-2"].len(), 4, "the rest carry on with the queue");
    assert_eq!(done.summary, Summary { passed: 4, failed: 0, skipped: 0, errors: 2 });
    let events = fs::read_to_string(done.bundle.join("events.jsonl")).unwrap();
    let out = events.lines().find(|line| line.contains("\"machine-out\"")).expect(&events);
    assert!(
        out.contains("\"machine\":\"checked-bad\"") && out.contains("checked-bad was benched"),
        "{out}"
    );
}

#[test]
fn a_pool_with_every_machine_out_skips_what_is_left_with_a_reason_that_says_so() {
    let setting = slow_tests(6);
    let broken = named(&setting, "broken");
    let bad = named(&setting, "checked-bad");
    let (done, results) =
        run_pools(&setting, Vec::new(), vec![optiplex(vec![&broken, &bad])], HashMap::new());
    assert_eq!(done.summary, Summary { passed: 0, failed: 0, skipped: 4, errors: 2 });
    let skipped: Vec<&ResultRecord> =
        results.iter().filter(|result| result.result == Outcome::Skip).collect();
    for result in skipped {
        assert_eq!(result.machine, "pool:optiplex");
        let reason = result.reason.as_deref().unwrap();
        assert!(
            reason.starts_with("no machine of pool:optiplex was left to run this test: ")
                && reason.contains("the broken machine did not start: no such machine")
                && reason.contains("checked-bad was benched"),
            "{reason}"
        );
    }

    let alone = slow_tests(2);
    let broken = named(&alone, "broken");
    let (done, results) =
        run_pools(&alone, Vec::new(), vec![optiplex(vec![&broken])], HashMap::new());
    assert_eq!(done.summary, Summary { passed: 0, failed: 0, skipped: 2, errors: 0 });
    assert!(results.iter().all(|result| result.machine == "pool:optiplex"));
}

#[test]
fn a_test_the_pool_cannot_run_is_skipped_once_for_the_pool() {
    let setting = setting_with(&[
        ("boot/ok.toml", "about = \"ok\"\ntime = \"1m\"\n[[step]]\nrun = \"true\"\n"),
        ("stress/cord.toml", TESTS[4].1),
        ("drivers/link.toml", TESTS[3].1),
    ]);
    let members: Vec<Machine> = ["optiplex-1", "optiplex-2", "optiplex-3"]
        .map(|name| {
            let mut machine = named(&setting, name);
            machine.controls.insert(Control::Cord, Who::Person);
            machine.controls.insert(Control::Link, Who::None);
            machine
        })
        .into();
    let (done, results) =
        run_pools(&setting, Vec::new(), vec![optiplex(members.iter().collect())], HashMap::new());
    assert_eq!(done.summary, Summary { passed: 1, failed: 0, skipped: 2, errors: 0 });
    let skipped: Vec<(&str, &str, &str)> = results
        .iter()
        .filter(|result| result.result == Outcome::Skip)
        .map(|result| {
            (result.test.as_str(), result.machine.as_str(), result.reason.as_deref().unwrap())
        })
        .collect();
    assert_eq!(
        skipped,
        [
            ("drivers/link", "pool:optiplex", "the machine has no link"),
            ("stress/cord", "pool:optiplex", "needs a person for cord"),
        ]
    );
}

#[test]
fn a_machine_named_on_its_own_runs_every_test_beside_a_pool_that_runs_each_once() {
    let setting = slow_tests(4);
    let first = named(&setting, "optiplex-1");
    let second = named(&setting, "optiplex-2");
    let (done, results) = run_pools(
        &setting,
        vec![&setting.qemu],
        vec![optiplex(vec![&first, &second])],
        HashMap::new(),
    );
    assert_eq!(done.summary.passed, 8);
    let by = ran_by(&results);
    assert_eq!(by["qemu"], ["boot/t1", "boot/t2", "boot/t3", "boot/t4"]);
    let in_pool: usize = by["optiplex-1"].len() + by["optiplex-2"].len();
    assert_eq!(in_pool, 4);
    let mut pooled: Vec<&str> = by["optiplex-1"].iter().chain(&by["optiplex-2"]).copied().collect();
    pooled.sort_unstable();
    pooled.dedup();
    assert_eq!(pooled.len(), 4, "each test ran once in the pool");
}

#[test]
fn a_resumed_sharded_run_queues_what_was_left_to_the_same_pool() {
    let setting = setting_with(&[
        ("features/uname.toml", TESTS[1].1),
        ("stress/hang.toml", "about = \"hangs\"\ntime = \"1s\"\n[[step]]\nrun = \"hang\"\n"),
        ("stress/later.toml", "about = \"later\"\ntime = \"1m\"\n[[step]]\nrun = \"true\"\n"),
    ]);
    let id = "20261107-153012-0004";
    let store = setting.dir.path().join("store");
    let asker = store.clone();
    // The hanging test goes first, as its last time was the longest.
    let typical = HashMap::from([(("optiplex-1".to_string(), "stress/hang".to_string()), 100_000)]);
    when_events_say(
        store.clone(),
        id,
        "\"event\":\"test-started\",\"machine\":\"optiplex-1\",\"test\":\"stress/hang\"",
        move || crate::store::request_stop(&asker, id).unwrap(),
    );
    let first = named(&setting, "optiplex-1");
    let request = Request {
        tests: setting.tests.iter().collect(),
        pools: vec![optiplex(vec![&first])],
        typical: typical.clone(),
        image: "dukovo.img".into(),
        build: Some("0.13.0 test".into()),
        store: store.clone(),
        key: Some(setting.dir.path().join("lab_ed25519")),
        id: Some(id.into()),
        ..Request::default()
    };
    run(&request, &Reporter::silent(), &start_fake).unwrap();
    let status = crate::status::status(&store, id).unwrap();
    assert_eq!(status.counts.skipped, 3);

    let resume =
        crate::resume::plan(&store, id, Path::new("dukovo.img"), Some("0.13.0 test")).unwrap();
    assert_eq!(resume.pools(), ["pool:optiplex"]);
    assert!(resume.machines().is_empty(), "a pool's machine is not named on its own");
    let names: Vec<String> = resume.tests().into_iter().map(String::from).collect();
    assert_eq!(names, ["features/uname", "stress/hang", "stress/later"]);
    let second = named(&setting, "optiplex-2");
    let request = Request {
        tests: crate::suite::select(&setting.tests, &names).unwrap(),
        pools: vec![optiplex(vec![&first, &second])],
        typical,
        image: "dukovo.img".into(),
        build: Some("0.13.0 test".into()),
        store: store.clone(),
        key: Some(setting.dir.path().join("lab_ed25519")),
        resume: Some(resume),
        id: Some("20261107-153013-0005".into()),
        ..Request::default()
    };
    run(&request, &Reporter::silent(), &start_fake).unwrap();
    let bundle = crate::store::bundle_dir(&store, "20261107-153013-0005");
    let results: crate::bundle::ResultsFile =
        serde_json::from_str(&fs::read_to_string(bundle.join("results.json")).unwrap()).unwrap();
    let mut ran: Vec<(&str, Outcome)> =
        results.results.iter().map(|result| (result.test.as_str(), result.result)).collect();
    ran.sort_unstable_by_key(|(test, _)| *test);
    assert_eq!(
        ran,
        [
            ("features/uname", Outcome::Pass),
            ("stress/hang", Outcome::Fail),
            ("stress/later", Outcome::Pass),
        ]
    );
    let run: RunRecord =
        serde_json::from_str(&fs::read_to_string(bundle.join("run.json")).unwrap()).unwrap();
    assert_eq!(run.resumes.as_deref(), Some(id));
    assert_eq!(run.pools[0].members, ["optiplex-1", "optiplex-2"]);
}

/// A run on `machines` and `pools` with reruns on or off.
fn run_with_reruns(
    setting: &Setting,
    machines: Vec<&Machine>,
    pools: Vec<Pool<'_>>,
    rerun: bool,
) -> (Done, Vec<ResultRecord>, String) {
    let request = Request {
        tests: setting.tests.iter().collect(),
        machines,
        pools,
        image: "dukovo.img".into(),
        store: setting.dir.path().join("store"),
        key: Some(setting.dir.path().join("lab_ed25519")),
        password: Some("sesame".into()),
        rerun,
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
        ..Request::default()
    };
    let ended = run(&request, &Reporter::silent(), &start_fake).unwrap();
    let bundle = crate::store::bundle_dir(&request.store, request.id.as_deref().unwrap());
    let results: crate::bundle::ResultsFile =
        serde_json::from_str(&fs::read_to_string(bundle.join("results.json")).unwrap()).unwrap();
    let events = fs::read_to_string(bundle.join("events.jsonl")).unwrap();
    (Done { summary: ended.summary, bundle }, results.results, events)
}

const FLIP: (&str, &str) = (
    "features/flip.toml",
    "about = \"flips\"\ntime = \"1m\"\n[[step]]\nrun = \"flip\"\nexpect = \"ok\"\n",
);

#[test]
fn a_test_that_fails_and_then_passes_is_flaky() {
    let setting = setting_with(&[FLIP, TESTS[2]]);
    let (done, results, events) = run_with_reruns(&setting, vec![&setting.qemu], Vec::new(), true);
    let flip = results.iter().find(|result| result.test == "features/flip").unwrap();
    assert_eq!(flip.result, Outcome::Fail, "the first try is the result");
    let rerun = flip.rerun.as_ref().unwrap();
    assert_eq!((rerun.result, rerun.machine.as_str()), (Outcome::Pass, "qemu"));
    assert!(flip.flaky());
    assert_eq!(rerun.log.as_deref(), Some("logs/qemu/rerun/features-flip.log"));
    assert!(done.bundle.join("logs/qemu/features-flip.log").is_file(), "the first log stays");
    assert!(done.bundle.join("logs/qemu/rerun/features-flip.log").is_file());
    // The run counts one failure per test, and findings hold each once.
    assert_eq!(done.summary, Summary { passed: 0, failed: 2, skipped: 0, errors: 0 });
    let findings = crate::findings::load_all(&setting.dir.path().join("store"));
    assert_eq!(findings.len(), 2);
    assert!(findings.iter().all(|finding| finding.count == 1));
    assert!(
        events
            .contains("\"event\":\"rerun-ended\",\"machine\":\"qemu\",\"test\":\"features/flip\"")
    );
}

#[test]
fn a_test_that_fails_again_is_not_flaky() {
    let setting = setting_with(&[TESTS[2]]);
    let (_, results, _) = run_with_reruns(&setting, vec![&setting.qemu], Vec::new(), true);
    let rerun = results[0].rerun.as_ref().unwrap();
    assert_eq!(rerun.result, Outcome::Fail);
    assert!(rerun.reason.as_deref().unwrap().contains("its output lacks \"Linux\""));
    assert!(!results[0].flaky());
}

#[test]
fn no_rerun_runs_each_test_once() {
    let setting = setting_with(&[FLIP]);
    let (done, results, events) = run_with_reruns(&setting, vec![&setting.qemu], Vec::new(), false);
    assert_eq!(results[0].rerun, None);
    assert!(!events.contains("rerun"));
    assert!(!done.bundle.join("logs/qemu/rerun").exists());
}

#[test]
fn an_error_and_a_skip_are_not_rerun() {
    let setting = setting_with(&[TESTS[4]]);
    let bad = named(&setting, "checked-bad");
    let mut no_cord = named(&setting, "no-cord");
    no_cord.controls.insert(Control::Cord, Who::None);
    let (_, results, events) = run_with_reruns(&setting, vec![&bad, &no_cord], Vec::new(), true);
    let kinds: Vec<Outcome> = results.iter().map(|result| result.result).collect();
    assert!(kinds.contains(&Outcome::Error) && kinds.contains(&Outcome::Skip), "{kinds:?}");
    assert!(results.iter().all(|result| result.rerun.is_none()));
    assert!(!events.contains("rerun"));
}

#[test]
fn a_test_of_a_pool_is_rerun_on_its_spare_first() {
    let setting = setting_with(&[FLIP]);
    let first = named(&setting, "optiplex-1");
    let second = named(&setting, "optiplex-2");
    let mut spare = named(&setting, "optiplex-3");
    spare.spare = true;
    let mut pool = optiplex(vec![&first, &second]);
    pool.spares = vec![&spare];
    let (_, results, events) = run_with_reruns(&setting, Vec::new(), vec![pool], true);
    assert_eq!(results.len(), 1);
    let rerun = results[0].rerun.as_ref().unwrap();
    assert_eq!(rerun.machine, "optiplex-3");
    assert_ne!(results[0].machine, "optiplex-3");
    assert!(results[0].flaky());
    assert_eq!(rerun.log.as_deref(), Some("logs/optiplex-3/rerun/features-flip.log"));
    assert!(events.contains("\"event\":\"rerun-started\",\"machine\":\"optiplex-3\""));
}

#[test]
fn a_test_of_a_pool_without_a_spare_is_rerun_on_another_member() {
    let setting = setting_with(&[FLIP, TESTS[2]]);
    let first = named(&setting, "optiplex-1");
    let second = named(&setting, "optiplex-2");
    let (_, results, _) =
        run_with_reruns(&setting, Vec::new(), vec![optiplex(vec![&first, &second])], true);
    for result in &results {
        let rerun = result.rerun.as_ref().unwrap();
        assert_ne!(rerun.machine, result.machine, "{}", result.test);
    }
    // A pool of one has no other machine: the same one, afresh.
    let alone = optiplex(vec![&first]);
    let again = setting_with(&[TESTS[2]]);
    let (_, results, _) = run_with_reruns(&again, Vec::new(), vec![alone], true);
    assert_eq!(results[0].rerun.as_ref().unwrap().machine, "optiplex-1");
}

#[test]
fn a_run_in_progress_shows_the_rerun_from_its_events() {
    let setting = setting_with(&[FLIP]);
    let (done, _, _) = run_with_reruns(&setting, vec![&setting.qemu], Vec::new(), true);
    let id = done.bundle.file_name().unwrap().to_str().unwrap();
    let store = setting.dir.path().join("store");
    // Without results.json, as a run that died part way, the events say it.
    fs::remove_file(done.bundle.join("results.json")).unwrap();
    let results = crate::status::results(&store, id).unwrap();
    assert!(results.results[0].flaky(), "{:?}", results.results[0]);
}

/// A browser kit whose Playwright opens nothing, to check the address the
/// step gives its script.
fn browser_kit(setting: &Setting) -> PathBuf {
    setting.dir.write("kit/node_modules/playwright/index.js", FAKE_PLAYWRIGHT);
    setting.dir.write("kit/node_modules/axe-core/package.json", "{ \"main\": \"axe.js\" }");
    setting.dir.write("kit/node_modules/axe-core/axe.js", "");
    setting.dir.path().join("kit")
}

/// Runs `setting`'s tests on `machine` with the browser kit, and gives the
/// bundle and the results.
fn run_browsers(setting: &Setting, machine: &Machine) -> (PathBuf, Vec<ResultRecord>) {
    let request = Request {
        tests: setting.tests.iter().collect(),
        machines: vec![machine],
        image: "dukovo.img".into(),
        build: None,
        store: setting.dir.path().join("store"),
        key: Some(setting.dir.path().join("lab_ed25519")),
        browsers: Some(browser_kit(setting)),
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
        ..Request::default()
    };
    run(&request, &Reporter::silent(), &start_fake).unwrap();
    let bundle = crate::store::bundle_dir(&request.store, request.id.as_deref().unwrap());
    let results: crate::bundle::ResultsFile =
        serde_json::from_str(&fs::read_to_string(bundle.join("results.json")).unwrap()).unwrap();
    (bundle, results.results)
}

/// A test with a host step and a browser step; the browser's script fails
/// unless the page is at `url`, and whose host steps expect `host_step`.
fn console_test(url: &str, host_step: &str) -> Setting {
    let host = format!(
        "[[step]]\nhost = \"echo {{machine}}:{{machine-port:9090}}\"\nexpect = \"{host_step}\"\n"
    );
    let test = format!(
        "about = \"opens the console\"\ntime = \"1m\"\n{host}\
         [[step]]\nbrowser = \"console.mjs\"\nport = 9090\n{host}"
    );
    let script = format!(
        "export default async function check(t) {{\n\
         if (!t.url.startsWith({url:?})) throw new Error(`url ${{t.url}}`);\n}}\n"
    );
    setting_with(&[("web/console.toml", &test), ("web/console.mjs", &script)])
}

#[test]
fn a_machine_reached_by_an_address_is_opened_by_the_browser_through_a_forward_to_its_loopback() {
    let setting = console_test("https://127.0.0.1:", "10.77.0.2:9090");
    let (bundle, results) = run_browsers(&setting, &named(&setting, "addressed"));
    let result = &results[0];
    assert_eq!(result.result, Outcome::Pass, "{:?}", result.reason);
    let log = fs::read_to_string(bundle.join(result.log.as_ref().unwrap())).unwrap();
    // The host steps, before and after, kept the machine's address.
    assert!(log.contains("lab-pc $ echo 10.77.0.2:9090"), "{log}");
    assert!(!log.contains("browser opens the machine by its address"), "{log}");
    let forward =
        fs::read_to_string(bundle.join("logs/addressed/web-console-forward-9090.log")).unwrap();
    assert!(forward.starts_with("forward 127.0.0.1:"), "{forward}");
    assert!(forward.ends_with(" to 9090\n"), "{forward}");
    // The forward ended with the test.
    assert!(log.contains("ended at the test's end"), "{log}");
    let pid = fs::read_to_string(bundle.join("logs/addressed/forward.pid")).unwrap();
    assert!(!Path::new("/proc").join(pid.trim()).exists(), "the forward's process {pid} lives");
}

#[test]
fn a_qemu_machine_s_browser_needs_no_forward() {
    // Its ports are already on the lab PC's loopback, as the host steps show.
    let setting = console_test("https://127.0.0.1:49090", "127.0.0.1:49090");
    let (bundle, results) = run_browsers(&setting, &setting.qemu);
    assert_eq!(results[0].result, Outcome::Pass, "{:?}", results[0].reason);
    assert!(!bundle.join("logs/qemu/web-console-forward-9090.log").exists());
}

#[test]
fn a_browser_falls_back_to_the_machine_s_address_when_its_forward_will_not_start() {
    for (machine, why) in [
        ("addressed-refused", "the lab has no key"),
        ("addressed-ends", "ended (status 255) at once"),
    ] {
        let setting = console_test("https://10.77.0.2:9090", "10.77.0.2:9090");
        let (bundle, results) = run_browsers(&setting, &named(&setting, machine));
        let result = &results[0];
        assert_eq!(result.result, Outcome::Pass, "{machine}: {:?}", result.reason);
        let log = fs::read_to_string(bundle.join(result.log.as_ref().unwrap())).unwrap();
        assert!(log.contains("The browser opens the machine by its address"), "{machine}: {log}");
        assert!(log.contains(why), "{machine}: {log}");
    }
}

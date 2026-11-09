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
    /// Whether the next boot gives the machine a card reader.
    card_reader: bool,
    /// Whether the next boot gives the machine an optical drive.
    disc_drive: bool,
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

    fn serial_log(&self) -> &Path {
        &self.serial
    }

    fn set_link(&mut self, up: bool) -> Result<(), String> {
        self.say(if up { "net: link up" } else { "net: link down" });
        Ok(())
    }

    fn plug_usb(&mut self, _stick: &crate::suite::Stick) -> Result<(), String> {
        Ok(())
    }

    fn unplug_usb(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn want_card_reader(&mut self, yes: bool) {
        self.card_reader = yes;
    }

    fn insert_card(&mut self, _card: &crate::suite::Stick) -> Result<(), String> {
        if !self.card_reader {
            return Err("this boot was not given a card reader".into());
        }
        Ok(())
    }

    fn remove_card(&mut self) -> Result<(), String> {
        if !self.card_reader {
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

    fn press_keys(&mut self, combos: &[String]) -> Result<(), String> {
        for combo in combos {
            self.key(combo);
        }
        Ok(())
    }

    fn point(&mut self, x: u32, y: u32) -> Result<(), String> {
        // The fake screen is 100 by 40 pixels, as `screen_dump` draws it.
        if x >= 100 || y >= 40 {
            return Err(format!("({x}, {y}) is off the screen, which is 100x40 pixels"));
        }
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
    ) -> Result<Output, String> {
        let (status, text) = match command {
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
            "hang" => {
                while Instant::now() < deadline && !self.stop.requested() {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                (None, String::new())
            }
            other => (Some(127), format!("{other}: not found\n")),
        };
        Ok(Output { status, text })
    }

    fn want_attacker(&mut self, yes: bool) {
        self.want_attacker = yes;
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
            // The attacker's own address on the private network, as root.
            cmd if cmd.contains("ip address add") => match input {
                Some("sesame\n") => (Some(0), String::new()),
                _ => (Some(1), "elevate: a password is needed\n".into()),
            },
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
        Some("127.0.0.1".into())
    }

    /// The machine's port N is at the lab PC's 40000 + N.
    fn reach(&mut self, port: u16, udp: bool) -> Result<u16, String> {
        if udp {
            return Err("this fake forwards no UDP".into());
        }
        port.checked_add(40000).ok_or_else(|| "this fake forwards only low ports".into())
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
        Ok(text)
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
        card_reader: false,
        disc_drive: false,
        attacker_on: false,
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
        stick: None,
        dvd: None,
        preset: None,
        schedule: None,
        id: Some(
            id.map_or_else(|| crate::bundle::new_run_id(crate::clock::Utc::now()), String::from),
        ),
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
        stick: None,
        dvd: None,
        preset: None,
        schedule: None,
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
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
        stick: None,
        dvd: None,
        preset: None,
        schedule: None,
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
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
        stick: None,
        dvd: None,
        preset: None,
        schedule: None,
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
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
        // Sorts after `card` in the same category, so it runs next, and
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
        stick: None,
        dvd: None,
        preset: None,
        schedule: None,
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
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
    // Nor is its boot shared with the test after it.
    let after = by_test("drivers/zz-after");
    assert_eq!(after.steps[0].step, "boot afresh and wait until booted");
    assert_eq!(ended.summary, Summary { passed: 3, failed: 0, skipped: 0, errors: 0 });
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
        stick: None,
        dvd: None,
        preset: None,
        schedule: None,
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
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
        stick,
        dvd,
        preset: None,
        schedule: None,
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
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
        stick: None,
        dvd: None,
        preset: None,
        schedule: None,
        id: Some(crate::bundle::new_run_id(crate::clock::Utc::now())),
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

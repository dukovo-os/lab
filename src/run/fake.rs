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
}

impl FakeMachine {
    fn say(&self, line: &str) {
        let mut log = OpenOptions::new().create(true).append(true).open(&self.serial).unwrap();
        writeln!(log, "{line}").unwrap();
    }
}

impl Driver for FakeMachine {
    fn boot_fresh(&mut self) -> Result<(), String> {
        self.boot += 1;
        fs::create_dir_all(&self.logs).unwrap();
        self.serial = self.logs.join(format!("serial-{}.log", self.boot));
        self.on = true;
        self.say("\u{1b}[1mRadix 0.13.0\u{1b}[0m\r");
        self.say("Boot complete.");
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

    fn press_keys(&mut self, _combos: &[String]) -> Result<(), String> {
        Ok(())
    }

    fn screenshot(&mut self, to: &Path) -> Result<(), String> {
        fs::write(to, b"\x89PNG").map_err(|err| err.to_string())
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
            "uname" => (Some(0), "Dukovo\n".into()),
            "false" => (Some(1), String::new()),
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

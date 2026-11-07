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
                self.say("Boot complete.");
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

    fn plug_usb(&mut self, _image: &Path) -> Result<(), String> {
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

    fn run_command(&mut self, command: &str, deadline: Instant) -> Result<Output, String> {
        let (status, text) = match command {
            "true" => (Some(0), String::new()),
            "uname" => (Some(0), "Dukovo\n".into()),
            "false" => (Some(1), String::new()),
            "hang" => {
                std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                (None, String::new())
            }
            other => (Some(127), format!("{other}: not found\n")),
        };
        Ok(Output { status, text })
    }

    fn slowdown(&self) -> u32 {
        1
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
    Ok(Box::new(FakeMachine { logs: setup.logs, serial: PathBuf::new(), boot: 0, on: false }))
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
    let dir = TempDir::new("run");
    for (file, text) in TESTS {
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
    let request = Request {
        tests: setting.tests.iter().collect(),
        machines,
        image: "dukovo.img".into(),
        build: Some("0.13.0 test".into()),
        store: setting.dir.path().join("store"),
        key: Some(setting.dir.path().join("lab_ed25519")),
    };
    let ended = run(&request, &Reporter::new(true), &start_fake).unwrap();
    // The store holds this one run: runs/<date>/<id>/.
    let runs = request.store.join("runs");
    let date = fs::read_dir(&runs).unwrap().next().unwrap().unwrap().path();
    let bundle = fs::read_dir(date).unwrap().next().unwrap().unwrap().path();
    let results: crate::bundle::ResultsFile =
        serde_json::from_str(&fs::read_to_string(bundle.join("results.json")).unwrap()).unwrap();
    (Done { summary: ended.summary, bundle }, results.results)
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

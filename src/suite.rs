// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The test suite: one TOML file per test, in `<tests>/<category>/`.
//!
//! Tests live in dukovo-os/dukovo under `lab/tests/`, next to the code
//! they test; `ob lab` points this tool at that folder. A test's id is its
//! category and file name, as `drivers/link-pull`. docs/test-files.md
//! describes the format.
//!
//! Loading is strict: a file that does not parse, an unknown key or an
//! unknown category folder stops every command with the file and what is
//! wrong, so a broken test is never skipped without anyone noticing.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::machines::{Control, Machine, Who, is_plain_name};
use crate::units::{format_duration, parse_duration};

/// The categories as (folder, title), in the order lists show them: the
/// runner page's order.
pub const CATEGORIES: &[(&str, &str)] = &[
    ("boot", "Boot and install"),
    ("drivers", "Drivers and hardware"),
    ("features", "Features"),
    ("performance", "Performance"),
    ("stress", "Stress and soak"),
    ("security", "Security"),
    ("console", "Console UI"),
    ("web", "Web console"),
    ("desktop", "Desktop"),
];

/// The longest a test may take: a soak test runs overnight, not longer.
const LONGEST_TEST: Duration = Duration::from_secs(24 * 3600);

/// One test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Test {
    /// `<category>/<name>`.
    pub id: String,
    pub category: String,
    pub file: PathBuf,
    /// What it checks, in one line.
    pub about: String,
    /// The longest it may take on KVM or real hardware, boot included.
    pub time: Duration,
    /// Whether it needs a boot of its own instead of sharing one.
    pub fresh: bool,
    pub steps: Vec<Step>,
}

/// One step of a test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Runs a command over SSH and checks what it did.
    Run(Command),
    /// Waits for text on the serial console, after the last text found.
    Serial(String),
    Power(PowerAction),
    /// Plugs the network cable in (`true`) or pulls it (`false`).
    Link(bool),
    /// Plugs in a USB stick holding a copy of this image file.
    PlugUsb(PathBuf),
    /// Pulls the USB stick out.
    UnplugUsb,
    /// Presses keys, each a combination such as `ctrl-alt-delete`.
    Keys(Vec<String>),
    /// Keeps a screenshot under this name.
    Screenshot(String),
    /// Waits until the machine has booted and answers.
    WaitReady,
    /// Waits this long.
    Sleep(Duration),
}

/// A command a `run` step runs, and what passes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub command: String,
    /// The exit status it must end with.
    pub exit: i32,
    /// Text its output must hold.
    pub expect: Vec<String>,
    /// Text its output must not hold.
    pub expect_not: Vec<String>,
}

/// What a `power` step does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PowerAction {
    /// Powers the machine on.
    On,
    /// Shuts it down as its power button would, and waits until it is off.
    Off,
    /// Presses the power button and goes on at once.
    Button,
    /// Presses the reset button.
    Reset,
    /// Pulls the power cord.
    Cut,
}

impl Step {
    /// The control this step needs, if any.
    pub fn control(&self) -> Option<Control> {
        match self {
            Step::Run(_) => Some(Control::Ssh),
            Step::Serial(_) => Some(Control::Serial),
            Step::Power(PowerAction::Cut) => Some(Control::Cord),
            Step::Power(_) => Some(Control::Power),
            Step::Link(_) => Some(Control::Link),
            Step::PlugUsb(_) | Step::UnplugUsb => Some(Control::Usb),
            Step::Keys(_) => Some(Control::Keys),
            Step::Screenshot(_) => Some(Control::Screen),
            Step::WaitReady | Step::Sleep(_) => None,
        }
    }

    /// The step in a few words, for `show` and the logs.
    pub fn describe(&self) -> String {
        match self {
            Step::Run(command) => describe_command(command),
            Step::Serial(text) => format!("wait for \"{text}\" on the serial console"),
            Step::Power(action) => match action {
                PowerAction::On => "power on".into(),
                PowerAction::Off => "shut down and wait until off".into(),
                PowerAction::Button => "press the power button".into(),
                PowerAction::Reset => "press reset".into(),
                PowerAction::Cut => "pull the power cord".into(),
            },
            Step::Link(true) => "plug the network cable in".into(),
            Step::Link(false) => "pull the network cable".into(),
            Step::PlugUsb(image) => {
                let name = image.file_name().unwrap_or_default().to_string_lossy();
                format!("plug in a USB stick holding {name}")
            }
            Step::UnplugUsb => "pull the USB stick".into(),
            Step::Keys(keys) => format!("press {}", keys.join(", ")),
            Step::Screenshot(name) => format!("take screenshot \"{name}\""),
            Step::WaitReady => "wait until booted".into(),
            Step::Sleep(time) => format!("wait {}", format_duration(*time)),
        }
    }
}

fn describe_command(command: &Command) -> String {
    let mut text = format!("run `{}`, exit status {}", command.command, command.exit);
    for expect in &command.expect {
        text.push_str(&format!(", output has \"{expect}\""));
    }
    for expect_not in &command.expect_not {
        text.push_str(&format!(", output lacks \"{expect_not}\""));
    }
    text
}

impl Test {
    /// Every control the test needs: its steps', and power and serial for
    /// the boot every test starts with.
    pub fn controls(&self) -> BTreeSet<Control> {
        let mut controls: BTreeSet<Control> = self.steps.iter().filter_map(Step::control).collect();
        controls.insert(Control::Power);
        controls.insert(Control::Serial);
        controls
    }

    /// Whether, and how, the test runs on `machine`.
    pub fn fit(&self, machine: &Machine) -> Fit {
        let controls = self.controls();
        let lacking: Vec<Control> =
            controls.iter().copied().filter(|&c| machine.who(c) == Who::None).collect();
        if !lacking.is_empty() {
            return Fit::Lacks(lacking);
        }
        let by_person: Vec<Control> =
            controls.iter().copied().filter(|&c| machine.who(c) == Who::Person).collect();
        if !by_person.is_empty() {
            return Fit::NeedsPerson(by_person);
        }
        Fit::Runs
    }
}

/// Whether a test runs on a machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "fit", content = "controls", rename_all = "kebab-case")]
pub enum Fit {
    /// With no one at the machine.
    Runs,
    /// Only with a person doing these controls: a Needs you test there.
    NeedsPerson(Vec<Control>),
    /// Not at all: the machine cannot do these controls.
    Lacks(Vec<Control>),
}

impl Fit {
    /// Why a run skips the test, if it does. Needs you tests are skipped
    /// until the runner page can ask a person to act (PR 2).
    pub fn skip_reason(&self) -> Option<String> {
        let names =
            |controls: &[Control]| controls.iter().map(|c| c.name()).collect::<Vec<_>>().join(", ");
        match self {
            Fit::Runs => None,
            Fit::NeedsPerson(controls) => Some(format!("needs a person for {}", names(controls))),
            Fit::Lacks(controls) => Some(format!("the machine has no {}", names(controls))),
        }
    }
}

/// A test file as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TestFile {
    about: String,
    time: String,
    #[serde(default)]
    fresh: bool,
    #[serde(rename = "step", default)]
    steps: Vec<StepFile>,
}

/// A step as written: exactly one action key, and the options that go with it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepFile {
    run: Option<String>,
    exit: Option<i32>,
    expect: Option<OneOrMore>,
    expect_not: Option<OneOrMore>,
    serial: Option<String>,
    power: Option<PowerAction>,
    link: Option<LinkState>,
    usb: Option<UsbAction>,
    image: Option<String>,
    keys: Option<Vec<String>>,
    screenshot: Option<String>,
    wait: Option<String>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum LinkState {
    Up,
    Down,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum UsbAction {
    Plug,
    Unplug,
}

/// `expect = "text"` or `expect = ["one", "two"]`.
#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMore {
    One(String),
    More(Vec<String>),
}

impl OneOrMore {
    fn into_vec(self) -> Vec<String> {
        match self {
            OneOrMore::One(text) => vec![text],
            OneOrMore::More(texts) => texts,
        }
    }
}

/// Reads every test under `dir`, sorted by category and then name.
pub fn load(dir: &Path) -> Result<Vec<Test>, Error> {
    let read = |path: &Path| {
        fs::read_dir(path).and_then(Iterator::collect::<Result<Vec<_>, _>>).map_err(|err| {
            Error::Usage(format!("reading the test folder {}: {err}", path.display()))
        })
    };
    let mut tests = Vec::new();
    for category in read(dir)? {
        let category_dir = category.path();
        if !category_dir.is_dir() {
            continue;
        }
        let name = category.file_name().to_string_lossy().into_owned();
        if !CATEGORIES.iter().any(|&(folder, _)| folder == name) {
            let known: Vec<&str> = CATEGORIES.iter().map(|&(folder, _)| folder).collect();
            return Err(Error::Usage(format!(
                "{}: not a test category; the categories are {}",
                category_dir.display(),
                known.join(", ")
            )));
        }
        // Other files beside the tests, such as a USB stick's image, are
        // the tests' data.
        for entry in read(&category_dir)? {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "toml") {
                tests.push(load_file(&path, &name)?);
            }
        }
    }
    let order = |test: &Test| CATEGORIES.iter().position(|&(folder, _)| folder == test.category);
    tests.sort_by(|a, b| order(a).cmp(&order(b)).then_with(|| a.id.cmp(&b.id)));
    Ok(tests)
}

/// Reads one test file.
fn load_file(path: &Path, category: &str) -> Result<Test, Error> {
    let bad = |what: String| Error::Usage(format!("{}: {what}", path.display()));
    let name = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default();
    if !is_plain_name(name) {
        return Err(bad("a test file's name may hold only a-z, 0-9 and -".into()));
    }
    let text = fs::read_to_string(path).map_err(|err| bad(err.to_string()))?;
    let file: TestFile = toml::from_str(&text).map_err(|err| bad(err.to_string()))?;
    if file.about.trim().is_empty() || file.about.contains('\n') {
        return Err(bad("about must be one line saying what the test checks".into()));
    }
    let time = parse_duration(&file.time).map_err(|err| bad(format!("time: {err}")))?;
    if time.is_zero() || time > LONGEST_TEST {
        return Err(bad("time must be more than 0s and at most 24h".into()));
    }
    if file.steps.is_empty() {
        return Err(bad("a test needs at least one [[step]]".into()));
    }
    let folder = path.parent().unwrap_or(Path::new("."));
    let mut steps = Vec::new();
    for (index, step) in file.steps.into_iter().enumerate() {
        let step =
            parse_step(step, folder).map_err(|err| bad(format!("step {}: {err}", index + 1)))?;
        steps.push(step);
    }
    let mut screenshots = BTreeSet::new();
    for step in &steps {
        if let Step::Screenshot(shot) = step
            && !screenshots.insert(shot)
        {
            return Err(bad(format!("two screenshots are named \"{shot}\"")));
        }
    }
    Ok(Test {
        id: format!("{category}/{name}"),
        category: category.into(),
        file: path.to_path_buf(),
        about: file.about,
        time,
        fresh: file.fresh,
        steps,
    })
}

/// Turns a step as written into a [`Step`], checking that it has exactly
/// one action and only that action's options.
fn parse_step(step: StepFile, folder: &Path) -> Result<Step, String> {
    let actions = [
        ("run", step.run.is_some()),
        ("serial", step.serial.is_some()),
        ("power", step.power.is_some()),
        ("link", step.link.is_some()),
        ("usb", step.usb.is_some()),
        ("keys", step.keys.is_some()),
        ("screenshot", step.screenshot.is_some()),
        ("wait", step.wait.is_some()),
    ];
    let given: Vec<&str> = actions.iter().filter(|(_, set)| *set).map(|(key, _)| *key).collect();
    match given.as_slice() {
        [] => {
            return Err(
                "no action: give one of run, serial, power, link, usb, keys, screenshot or wait"
                    .into(),
            );
        }
        [_] => {}
        more => return Err(format!("one action per step, but this has {}", more.join(" and "))),
    }
    let only_with = |option: &str, set: bool, action: &str| {
        if set && given[0] != action {
            Err(format!("{option} goes only with {action}"))
        } else {
            Ok(())
        }
    };
    only_with("exit", step.exit.is_some(), "run")?;
    only_with("expect", step.expect.is_some(), "run")?;
    only_with("expect_not", step.expect_not.is_some(), "run")?;
    only_with("image", step.image.is_some(), "usb")?;

    if let Some(command) = step.run {
        if command.trim().is_empty() {
            return Err("run needs a command".into());
        }
        return Ok(Step::Run(Command {
            command,
            exit: step.exit.unwrap_or(0),
            expect: step.expect.map(OneOrMore::into_vec).unwrap_or_default(),
            expect_not: step.expect_not.map(OneOrMore::into_vec).unwrap_or_default(),
        }));
    }
    if let Some(text) = step.serial {
        if text.is_empty() {
            return Err("serial needs the text to wait for".into());
        }
        return Ok(Step::Serial(text));
    }
    if let Some(action) = step.power {
        return Ok(Step::Power(action));
    }
    if let Some(state) = step.link {
        return Ok(Step::Link(matches!(state, LinkState::Up)));
    }
    if let Some(action) = step.usb {
        return parse_usb(action, step.image, folder);
    }
    if let Some(keys) = step.keys {
        if keys.is_empty() || !keys.iter().all(|combo| is_key_combo(combo)) {
            return Err(
                "keys are combinations of QEMU key names joined by -, such as ctrl-alt-delete"
                    .into(),
            );
        }
        return Ok(Step::Keys(keys));
    }
    if let Some(name) = step.screenshot {
        if !is_plain_name(&name) {
            return Err("a screenshot's name may hold only a-z, 0-9 and -".into());
        }
        return Ok(Step::Screenshot(name));
    }
    let wait = step.wait.unwrap_or_default();
    if wait == "ready" {
        return Ok(Step::WaitReady);
    }
    parse_duration(&wait)
        .map(Step::Sleep)
        .map_err(|err| format!("wait is \"ready\" or a time: {err}"))
}

fn parse_usb(action: UsbAction, image: Option<String>, folder: &Path) -> Result<Step, String> {
    match (action, image) {
        (UsbAction::Unplug, _) => Ok(Step::UnplugUsb),
        (UsbAction::Plug, None) => Err("usb = \"plug\" needs image, a file beside the test".into()),
        (UsbAction::Plug, Some(image)) => {
            // Only files beside the test, so a test cannot read the lab
            // PC's other files into a machine.
            if image.contains('/') || image.starts_with('.') {
                return Err(format!("image \"{image}\" must be a file name in the test's folder"));
            }
            let path = folder.join(&image);
            if !path.is_file() {
                return Err(format!("image {} does not exist", path.display()));
            }
            Ok(Step::PlugUsb(path))
        }
    }
}

/// Whether `combo` is key names joined by `-`, as QEMU's `send-key` reads them.
fn is_key_combo(combo: &str) -> bool {
    combo.split('-').all(|key| {
        !key.is_empty()
            && key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    })
}

/// Picks tests by id or by category (`drivers` or `drivers/`), in the
/// order given, each once.
pub fn select<'a>(tests: &'a [Test], names: &[String]) -> Result<Vec<&'a Test>, Error> {
    let mut picked: Vec<&Test> = Vec::new();
    for name in names {
        let category = name.strip_suffix('/').unwrap_or(name);
        let matching: Vec<&Test> = if CATEGORIES.iter().any(|&(folder, _)| folder == category) {
            tests.iter().filter(|test| test.category == category).collect()
        } else {
            tests.iter().filter(|test| test.id == *name).collect()
        };
        if matching.is_empty() {
            return Err(Error::Usage(format!(
                "no test or category named \"{name}\"; `dukovo-lab list` lists them"
            )));
        }
        for test in matching {
            if !picked.iter().any(|already| already.id == test.id) {
                picked.push(test);
            }
        }
    }
    Ok(picked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    const LINK_PULL: &str = r#"
about = "The network link drops and comes back"
time = "3m"

[[step]]
link = "down"
[[step]]
serial = "link down"
[[step]]
link = "up"
[[step]]
run = "ping -c 1 10.0.2.2"
expect = "1 received"
"#;

    #[test]
    fn reads_a_test() {
        let dir = TempDir::new("suite-read");
        dir.write("drivers/link-pull.toml", LINK_PULL);
        let tests = load(dir.path()).unwrap();
        assert_eq!(tests.len(), 1);
        let test = &tests[0];
        assert_eq!(test.id, "drivers/link-pull");
        assert_eq!(test.time, Duration::from_secs(180));
        assert!(!test.fresh);
        assert_eq!(test.steps[0], Step::Link(false));
        assert_eq!(test.steps[1], Step::Serial("link down".into()));
        assert_eq!(
            test.steps[3],
            Step::Run(Command {
                command: "ping -c 1 10.0.2.2".into(),
                exit: 0,
                expect: vec!["1 received".into()],
                expect_not: vec![],
            })
        );
        let controls: Vec<Control> = test.controls().into_iter().collect();
        assert_eq!(controls, [Control::Power, Control::Serial, Control::Link, Control::Ssh]);
    }

    #[test]
    fn sorts_by_category_then_name() {
        let dir = TempDir::new("suite-sort");
        let tiny = "about = \"x\"\ntime = \"1m\"\n[[step]]\nwait = \"ready\"\n";
        for file in ["features/b.toml", "boot/z.toml", "features/a.toml", "drivers/m.toml"] {
            dir.write(file, tiny);
        }
        dir.write("features/notes.txt", "not a test");
        let ids: Vec<String> = load(dir.path()).unwrap().into_iter().map(|test| test.id).collect();
        assert_eq!(ids, ["boot/z", "drivers/m", "features/a", "features/b"]);
    }

    #[test]
    fn bad_tests_say_what_is_wrong() {
        let head = "about = \"x\"\ntime = \"1m\"\n";
        for (file, text, complaint) in [
            ("boot/a.toml", "about = \"x\"\n[[step]]\nwait = \"ready\"\n", "missing field `time`"),
            (
                "boot/b.toml",
                &format!("{head}[[step]]\nwait = \"ready\"\nfoo = 1\n"),
                "unknown field `foo`",
            ),
            ("boot/c.toml", head, "at least one [[step]]"),
            (
                "boot/d.toml",
                &format!("{head}[[step]]\nlink = \"down\"\nserial = \"x\"\n"),
                "step 1: one action per step",
            ),
            (
                "boot/e.toml",
                &format!("{head}[[step]]\nserial = \"x\"\nexit = 1\n"),
                "exit goes only with run",
            ),
            ("boot/f.toml", &format!("{head}[[step]]\nusb = \"plug\"\n"), "needs image"),
            (
                "boot/g.toml",
                &format!("{head}[[step]]\nusb = \"plug\"\nimage = \"../x\"\n"),
                "in the test's folder",
            ),
            (
                "boot/h.toml",
                &format!("{head}[[step]]\nwait = \"soon\"\n"),
                "wait is \"ready\" or a time",
            ),
            ("boot/i.toml", &format!("{head}[[step]]\npower = \"sideways\"\n"), "unknown variant"),
            ("boot/j.toml", &format!("{head}[[step]]\nkeys = [\"Ctrl+C\"]\n"), "key names"),
            ("boot/k.toml", "about = \"x\"\ntime = \"2d\"\n[[step]]\nwait = \"ready\"\n", "time:"),
            (
                "boot/l.toml",
                &format!("{head}[[step]]\nscreenshot = \"a\"\n[[step]]\nscreenshot = \"a\"\n"),
                "two screenshots",
            ),
            ("boots/m.toml", &format!("{head}[[step]]\nwait = \"ready\"\n"), "not a test category"),
        ] {
            let dir = TempDir::new("suite-bad");
            dir.write(file, text);
            let Err(Error::Usage(message)) = load(dir.path()) else {
                panic!("{file} should not load");
            };
            assert!(message.contains(file.split('/').next().unwrap()), "{message}");
            assert!(message.contains(complaint), "{file}: {message}");
        }
    }

    #[test]
    fn a_usb_image_is_a_file_beside_the_test() {
        let dir = TempDir::new("suite-usb");
        dir.write("features/stick.img", "");
        dir.write(
            "features/stick.toml",
            "about = \"x\"\ntime = \"1m\"\n[[step]]\nusb = \"plug\"\nimage = \"stick.img\"\n[[step]]\nusb = \"unplug\"\n",
        );
        let tests = load(dir.path()).unwrap();
        assert_eq!(tests[0].steps[0], Step::PlugUsb(dir.path().join("features/stick.img")));
        assert_eq!(tests[0].steps[1], Step::UnplugUsb);
    }

    #[test]
    fn fits_machines_by_their_controls() {
        let dir = TempDir::new("suite-fit");
        dir.write("drivers/link-pull.toml", LINK_PULL);
        let test = &load(dir.path()).unwrap()[0];
        let mut machine = crate::machines::load(&dir.path().join("none")).unwrap().remove(0);
        assert_eq!(test.fit(&machine), Fit::Runs);
        machine.controls.insert(Control::Link, Who::Person);
        assert_eq!(test.fit(&machine), Fit::NeedsPerson(vec![Control::Link]));
        machine.controls.insert(Control::Ssh, Who::None);
        assert_eq!(test.fit(&machine), Fit::Lacks(vec![Control::Ssh]));
        assert_eq!(test.fit(&machine).skip_reason().unwrap(), "the machine has no ssh");
    }

    #[test]
    fn selects_tests_and_categories_once_each() {
        let dir = TempDir::new("suite-select");
        let tiny = "about = \"x\"\ntime = \"1m\"\n[[step]]\nwait = \"ready\"\n";
        for file in ["boot/a.toml", "drivers/b.toml", "drivers/c.toml"] {
            dir.write(file, tiny);
        }
        let tests = load(dir.path()).unwrap();
        let ids = |names: &[&str]| -> Vec<String> {
            let names: Vec<String> = names.iter().map(|&name| name.to_string()).collect();
            select(&tests, &names).unwrap().into_iter().map(|test| test.id.clone()).collect()
        };
        assert_eq!(ids(&["drivers/c", "drivers/"]), ["drivers/c", "drivers/b"]);
        assert_eq!(ids(&["boot"]), ["boot/a"]);
        assert!(select(&tests, &["drivers/x".into()]).is_err());
    }
}

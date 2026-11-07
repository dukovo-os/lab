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

/// The categories when a tests folder has no `categories.toml`: those
/// the first suites had, so an older Dukovo checkout still loads.
const BUILT_IN_CATEGORIES: &[(&str, &str)] = &[
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

/// The longest a category's title may be: the runner page's left side
/// shows it on one line.
const TITLE_MAX: usize = 40;

/// A group of tests: a folder of the tests folder, with the title lists
/// and the runner page show it under. The suite names its categories, and
/// their order, in `categories.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Category {
    /// The folder, and what `run` takes for every test in it.
    pub id: String,
    pub title: String,
    /// What its tests check, in a sentence; may be empty.
    pub about: String,
}

/// categories.toml as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CategoriesFile {
    #[serde(rename = "category", default)]
    categories: Vec<CategoryFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CategoryFile {
    name: String,
    title: String,
    #[serde(default)]
    about: String,
}

/// Reads `<dir>/categories.toml`, in order; the built-in categories when
/// there is none. Like a test file, one that is wrong stops every
/// command.
pub fn load_categories(dir: &Path) -> Result<Vec<Category>, Error> {
    let path = dir.join("categories.toml");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BUILT_IN_CATEGORIES
                .iter()
                .map(|&(id, title)| Category {
                    id: id.into(),
                    title: title.into(),
                    about: String::new(),
                })
                .collect());
        }
        Err(err) => return Err(Error::Usage(format!("reading {}: {err}", path.display()))),
    };
    let file: CategoriesFile = toml::from_str(&text)
        .map_err(|err| Error::Usage(format!("{}: {}", path.display(), err.message())))?;
    let mut categories: Vec<Category> = Vec::new();
    for category in file.categories {
        let bad = |why: &str| {
            Error::Usage(format!("{}: category \"{}\": {why}", path.display(), category.name))
        };
        if !is_plain_name(&category.name) {
            return Err(bad("a name is a folder's: letters, digits and dashes"));
        }
        if categories.iter().any(|known| known.id == category.name) {
            return Err(bad("named twice"));
        }
        let title = category.title.trim();
        if title.is_empty() || title.chars().count() > TITLE_MAX {
            return Err(bad("a title is 1 to 40 characters"));
        }
        categories.push(Category {
            id: category.name.clone(),
            title: title.into(),
            about: category.about.trim().into(),
        });
    }
    if categories.is_empty() {
        return Err(Error::Usage(format!("{}: it names no category", path.display())));
    }
    Ok(categories)
}

/// The longest a test may take: a soak test runs overnight, not longer.
const LONGEST_TEST: Duration = Duration::from_secs(24 * 3600);

/// One test.
#[derive(Clone, Debug, PartialEq)]
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
    /// What the machine boots from when the test boots it.
    pub boot: Boot,
    pub steps: Vec<Step>,
}

/// What a test's machine boots from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Boot {
    /// A clean disk holding the image under test, and the runner waits
    /// until it is ready: every test but the installer's.
    Image,
    /// The install stick the run was given, with a blank disk of this
    /// many bytes for the installer, and the runner waits for nothing:
    /// the test's own steps follow the stick's boot. The machine boots
    /// afresh for the test after, as its disk no longer holds the image.
    Stick { disk: u64 },
}

/// What a `type` step types in place of this text: the lab password, which
/// never shows in a test file, a step's description or a log.
pub const PASSWORD_TOKEN: &str = "{password}";

/// One step of a test.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// Runs a command over SSH and checks what it did.
    Run(Command),
    /// Waits for text on the serial console, after the last text found.
    Serial(String),
    Power(PowerAction),
    /// Plugs the network cable in (`true`) or pulls it (`false`).
    Link(bool),
    /// Plugs in a USB stick.
    PlugUsb(Stick),
    /// Pulls the USB stick out.
    UnplugUsb,
    /// Presses keys, each a combination such as `ctrl-alt-delete`.
    Keys(Vec<String>),
    /// Keeps a screenshot under this name.
    Screenshot(String),
    /// Waits until the machine has booted and answers.
    WaitReady,
    /// Waits until the machine has turned itself off.
    WaitOff,
    /// Waits this long.
    Sleep(Duration),
    /// Runs a benchmark program and holds its rows to Linux's.
    Bench(Bench),
    /// Waits until the screen shows every one of these texts.
    Screen(Vec<String>),
    /// Waits until the screen no longer shows this text.
    ScreenGone(String),
    /// Types this text on the keyboard: `\n` is Enter, and
    /// [`PASSWORD_TOKEN`] types the lab password.
    Type(String),
    /// Moves the pointer to this pixel of the screen, counted from its
    /// top left corner.
    Point {
        x: u32,
        y: u32,
    },
    /// Presses a pointer button and lets it go, where the pointer is.
    Click(Button),
}

/// A pointer button.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    /// Its name in test files, and QEMU's.
    pub fn name(self) -> &'static str {
        match self {
            Button::Left => "left",
            Button::Right => "right",
            Button::Middle => "middle",
        }
    }
}

/// What a `bench` step measures, and the bar it holds it to.
#[derive(Clone, Debug, PartialEq)]
pub struct Bench {
    /// The benchmark program, run over SSH as a `run` step's command is.
    pub command: String,
    /// Run as root, through elevate with the lab password.
    pub root: bool,
    /// The rows of its output to hold to Linux's.
    pub rows: Vec<String>,
    /// The most each row may take, as a share of Linux's time.
    pub limit: f64,
}

/// The bar a row is held to unless its step says: parity with Linux, as
/// `ob bench` holds every row.
pub const DEFAULT_LIMIT: f64 = 1.00;

/// What a plugged-in USB stick holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stick {
    /// A copy of this image file, beside the test.
    Image(PathBuf),
    /// Nothing: this many bytes of zeros, for a test that formats it.
    Blank(u64),
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
    /// Whether it runs as root, through `elevate -S` and the lab's
    /// password.
    pub root: bool,
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
            Step::Power(_) | Step::WaitOff => Some(Control::Power),
            Step::Link(_) => Some(Control::Link),
            Step::PlugUsb(_) | Step::UnplugUsb => Some(Control::Usb),
            Step::Keys(_) | Step::Type(_) => Some(Control::Keys),
            Step::Point { .. } | Step::Click(_) => Some(Control::Mouse),
            Step::Screenshot(_) | Step::Screen(_) | Step::ScreenGone(_) => Some(Control::Screen),
            Step::WaitReady | Step::Sleep(_) => None,
            // It also runs over SSH: see `Test::controls`.
            Step::Bench(_) => Some(Control::Linux),
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
            Step::PlugUsb(Stick::Image(image)) => {
                let name = image.file_name().unwrap_or_default().to_string_lossy();
                format!("plug in a USB stick holding {name}")
            }
            Step::PlugUsb(Stick::Blank(bytes)) => {
                format!("plug in a blank USB stick of {}", format_size(*bytes))
            }
            Step::UnplugUsb => "pull the USB stick".into(),
            Step::Keys(keys) => format!("press {}", keys.join(", ")),
            Step::Type(text) => format!("type \"{}\"", text.escape_debug()),
            Step::Point { x, y } => format!("point at ({x}, {y}) on the screen"),
            Step::Click(button) => format!("click the {} button", button.name()),
            Step::Screen(texts) => {
                let texts: Vec<String> = texts.iter().map(|text| format!("\"{text}\"")).collect();
                format!("wait until the screen shows {}", texts.join(", "))
            }
            Step::ScreenGone(text) => format!("wait until the screen no longer shows \"{text}\""),
            Step::Screenshot(name) => format!("take screenshot \"{name}\""),
            Step::WaitReady => "wait until booted".into(),
            Step::WaitOff => "wait until it turns itself off".into(),
            Step::Sleep(time) => format!("wait {}", format_duration(*time)),
            Step::Bench(bench) => {
                let who = if bench.root { " as root" } else { "" };
                format!(
                    "measure `{}`{who} against Linux: {} at most {:.2} of Linux's time",
                    bench.command,
                    bench.rows.join(", "),
                    bench.limit
                )
            }
        }
    }
}

fn describe_command(command: &Command) -> String {
    let who = if command.root { " as root" } else { "" };
    let mut text = format!("run `{}`{who}, exit status {}", command.command, command.exit);
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
        // A bench step runs its program over SSH as well as booting Linux.
        if self.steps.iter().any(|step| matches!(step, Step::Bench(_))) {
            controls.insert(Control::Ssh);
        }
        // The install stick is plugged in as the machine powers on.
        if matches!(self.boot, Boot::Stick { .. }) {
            controls.insert(Control::Usb);
        }
        controls.insert(Control::Power);
        controls.insert(Control::Serial);
        controls
    }

    /// Whether a step needs the lab password: one run as root, or one that
    /// types it.
    pub fn uses_password(&self) -> bool {
        self.steps.iter().any(|step| match step {
            Step::Run(command) => command.root,
            Step::Bench(bench) => bench.root,
            Step::Type(text) => text.contains(PASSWORD_TOKEN),
            _ => false,
        })
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
    /// Why a run skips the test, if it does. A run given `--with-person`
    /// runs Needs you tests instead, asking a person to do their steps.
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
    boot: Option<BootFrom>,
    disk: Option<String>,
    #[serde(rename = "step", default)]
    steps: Vec<StepFile>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum BootFrom {
    Image,
    Stick,
}

/// The installer's disk unless a test says: room for an installed system,
/// as Dukovo's own install tests give it.
const DEFAULT_INSTALL_DISK: u64 = 8 << 30;

/// The largest blank disk a test may ask for. QEMU's disks take room only
/// as they are written, so this is a bar against typing slips.
const DISK_MAX: u64 = 2 << 40;

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
    blank: Option<String>,
    root: Option<bool>,
    keys: Option<Vec<String>>,
    screenshot: Option<String>,
    wait: Option<String>,
    bench: Option<String>,
    rows: Option<Vec<String>>,
    limit: Option<f64>,
    screen: Option<OneOrMore>,
    screen_gone: Option<String>,
    #[serde(rename = "type")]
    typed: Option<String>,
    point: Option<Vec<u32>>,
    click: Option<Button>,
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
    let categories = load_categories(dir)?;
    let mut tests = Vec::new();
    for category in read(dir)? {
        let category_dir = category.path();
        if !category_dir.is_dir() {
            continue;
        }
        let name = category.file_name().to_string_lossy().into_owned();
        if !categories.iter().any(|known| known.id == name) {
            let known: Vec<&str> = categories.iter().map(|known| known.id.as_str()).collect();
            return Err(Error::Usage(format!(
                "{}: not a test category; the categories are {} (a new one goes in categories.toml)",
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
    let order = |test: &Test| categories.iter().position(|known| known.id == test.category);
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
    let boot = match (file.boot, file.disk) {
        (None | Some(BootFrom::Image), None) => Boot::Image,
        (None | Some(BootFrom::Image), Some(_)) => {
            return Err(bad("disk goes only with boot = \"stick\"".into()));
        }
        (Some(BootFrom::Stick), None) => Boot::Stick { disk: DEFAULT_INSTALL_DISK },
        (Some(BootFrom::Stick), Some(size)) => match parse_size(&size) {
            Some(disk) if (1 << 30..=DISK_MAX).contains(&disk) => Boot::Stick { disk },
            _ => {
                return Err(bad(format!(
                    "disk = \"{size}\": a size from 1G to 2048G, as a number and M or G, such as \"8G\""
                )));
            }
        },
    };
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
        boot,
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
        ("bench", step.bench.is_some()),
        ("screen", step.screen.is_some()),
        ("screen_gone", step.screen_gone.is_some()),
        ("type", step.typed.is_some()),
        ("point", step.point.is_some()),
        ("click", step.click.is_some()),
    ];
    let given: Vec<&str> = actions.iter().filter(|(_, set)| *set).map(|(key, _)| *key).collect();
    match given.as_slice() {
        [] => {
            return Err(
                "no action: give one of run, serial, power, link, usb, keys, type, point, click, screen, screen_gone, screenshot, wait or bench"
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
    if step.root.is_some() && !matches!(given[0], "run" | "bench") {
        return Err("root goes only with run or bench".into());
    }
    only_with("rows", step.rows.is_some(), "bench")?;
    only_with("limit", step.limit.is_some(), "bench")?;
    only_with("image", step.image.is_some(), "usb")?;
    only_with("blank", step.blank.is_some(), "usb")?;

    if let Some(command) = step.run {
        if command.trim().is_empty() {
            return Err("run needs a command".into());
        }
        return Ok(Step::Run(Command {
            command,
            exit: step.exit.unwrap_or(0),
            expect: step.expect.map(OneOrMore::into_vec).unwrap_or_default(),
            expect_not: step.expect_not.map(OneOrMore::into_vec).unwrap_or_default(),
            root: step.root.unwrap_or(false),
        }));
    }
    if let Some(command) = step.bench {
        return parse_bench(command, step.root, step.rows, step.limit);
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
        return parse_usb(action, step.image, step.blank, folder);
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
    if let Some(texts) = step.screen {
        let texts = texts.into_vec();
        if texts.is_empty()
            || texts.iter().any(|text| text.trim().is_empty() || text.contains('\n'))
        {
            return Err(
                "screen needs the text to wait for, each within one row of the screen".into()
            );
        }
        return Ok(Step::Screen(texts));
    }
    if let Some(text) = step.screen_gone {
        if text.trim().is_empty() || text.contains('\n') {
            return Err("screen_gone needs the text to wait to go, within one row".into());
        }
        return Ok(Step::ScreenGone(text));
    }
    if let Some(text) = step.typed {
        if text.is_empty() {
            return Err("type needs the text to type".into());
        }
        // The password is letters and digits; everything else must have a
        // key, which is checked now rather than halfway through a test.
        crate::screen::key_combos(&text.replace(PASSWORD_TOKEN, ""))?;
        return Ok(Step::Type(text));
    }
    if let Some(at) = step.point {
        let [x, y] = at[..] else {
            return Err("point needs the pixel to point at: [x, y], from the top left".into());
        };
        return Ok(Step::Point { x, y });
    }
    if let Some(button) = step.click {
        return Ok(Step::Click(button));
    }
    if let Some(name) = step.screenshot {
        if !is_plain_name(&name) {
            return Err("a screenshot's name may hold only a-z, 0-9 and -".into());
        }
        return Ok(Step::Screenshot(name));
    }
    let wait = step.wait.unwrap_or_default();
    match wait.as_str() {
        "ready" => return Ok(Step::WaitReady),
        "off" => return Ok(Step::WaitOff),
        _ => {}
    }
    parse_duration(&wait)
        .map(Step::Sleep)
        .map_err(|err| format!("wait is \"ready\", \"off\" or a time: {err}"))
}

/// The range a `limit` may take: a bar below a tenth of Linux's time, or
/// above ten times it, is a typing slip rather than a bar.
const LIMIT_RANGE: std::ops::RangeInclusive<f64> = 0.1..=10.0;

fn parse_bench(
    command: String,
    root: Option<bool>,
    rows: Option<Vec<String>>,
    limit: Option<f64>,
) -> Result<Step, String> {
    if command.trim().is_empty() {
        return Err("bench needs the benchmark program to run".into());
    }
    // The rows are named, so a row the program stops printing fails the
    // test rather than leaving it.
    let rows = rows.unwrap_or_default();
    if rows.is_empty() {
        return Err("bench needs rows: the names of the rows to hold to Linux's".into());
    }
    if let Some(bad) = rows.iter().find(|row| !is_row_name(row)) {
        return Err(format!("{bad:?} is not a row's name: letters, digits and _"));
    }
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    if !LIMIT_RANGE.contains(&limit) {
        return Err(format!("limit is a share of Linux's time from 0.1 to 10, not {limit}"));
    }
    Ok(Step::Bench(Bench { command, root: root.unwrap_or(false), rows, limit }))
}

/// A benchmark row's name, as `BENCH name=` prints it.
fn is_row_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The largest blank stick: enough for any filesystem's smallest size,
/// and quick to copy.
const BLANK_MAX: u64 = 4 << 30;

fn parse_usb(
    action: UsbAction,
    image: Option<String>,
    blank: Option<String>,
    folder: &Path,
) -> Result<Step, String> {
    match (action, image, blank) {
        (UsbAction::Unplug, None, None) => Ok(Step::UnplugUsb),
        (UsbAction::Unplug, ..) => Err("usb = \"unplug\" takes no image or blank".into()),
        (UsbAction::Plug, None, None) => Err(
            "usb = \"plug\" needs image, a file beside the test, or blank, a size such as \"64M\""
                .into(),
        ),
        (UsbAction::Plug, Some(_), Some(_)) => Err("a stick is an image or blank, not both".into()),
        (UsbAction::Plug, None, Some(size)) => match parse_size(&size) {
            Some(bytes) if (1 << 20..=BLANK_MAX).contains(&bytes) => {
                Ok(Step::PlugUsb(Stick::Blank(bytes)))
            }
            _ => Err(format!(
                "blank = \"{size}\": a size from 1M to 4G, as a number and M or G, such as \"64M\""
            )),
        },
        (UsbAction::Plug, Some(image), None) => {
            // Only files beside the test, so a test cannot read the lab
            // PC's other files into a machine.
            if image.contains('/') || image.starts_with('.') {
                return Err(format!("image \"{image}\" must be a file name in the test's folder"));
            }
            let path = folder.join(&image);
            if !path.is_file() {
                return Err(format!("image {} does not exist", path.display()));
            }
            Ok(Step::PlugUsb(Stick::Image(path)))
        }
    }
}

/// `64M` or `2G` in bytes (binary units, as QEMU and `truncate` read them).
fn parse_size(text: &str) -> Option<u64> {
    let (number, unit) = text.split_at(text.len().checked_sub(1)?);
    let shift = match unit {
        "M" => 20,
        "G" => 30,
        _ => return None,
    };
    let number: u64 = number.parse().ok()?;
    number.checked_mul(1 << shift)
}

/// Bytes as `parse_size` reads them.
fn format_size(bytes: u64) -> String {
    if bytes.is_multiple_of(1 << 30) {
        format!("{}G", bytes >> 30)
    } else {
        format!("{}M", bytes >> 20)
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
        // A category is a folder the tests are in: `load` let in only
        // the suite's own.
        let matching: Vec<&Test> = if tests.iter().any(|test| test.category == category) {
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

/// A named set of tests to run together, such as a quick check before a
/// merge. The runner page offers them; `run --preset <name>` runs one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Preset {
    /// What `--preset` takes, such as `quick`.
    pub name: String,
    /// What the runner page shows, such as "Quick check".
    pub title: String,
    pub about: String,
    /// Test ids, in the order they run.
    pub tests: Vec<String>,
}

/// presets.toml as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PresetsFile {
    #[serde(rename = "preset", default)]
    presets: Vec<PresetFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PresetFile {
    name: String,
    title: String,
    #[serde(default)]
    about: String,
    /// Tests and categories, as `run` takes them.
    tests: Vec<String>,
}

/// Reads `<dir>/presets.toml`, if there is one, naming its tests by id.
/// Like a test file, a preset that is wrong stops every command.
pub fn load_presets(dir: &Path, tests: &[Test]) -> Result<Vec<Preset>, Error> {
    let path = dir.join("presets.toml");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(Error::Usage(format!("reading {}: {err}", path.display()))),
    };
    let file: PresetsFile = toml::from_str(&text)
        .map_err(|err| Error::Usage(format!("{}: {}", path.display(), err.message())))?;
    let mut presets: Vec<Preset> = Vec::new();
    for preset in file.presets {
        let bad = |why: String| {
            Error::Usage(format!("{}: preset \"{}\": {why}", path.display(), preset.name))
        };
        if !is_plain_name(&preset.name) {
            return Err(bad("a name is letters, digits and dashes".into()));
        }
        if presets.iter().any(|known| known.name == preset.name) {
            return Err(bad("named twice".into()));
        }
        if preset.tests.is_empty() {
            return Err(bad("it has no tests".into()));
        }
        let picked = select(tests, &preset.tests).map_err(|err| match err {
            Error::Usage(why) | Error::Harness(why) | Error::Busy(why) => bad(why),
        })?;
        presets.push(Preset {
            tests: picked.iter().map(|test| test.id.clone()).collect(),
            name: preset.name,
            title: preset.title,
            about: preset.about,
        });
    }
    Ok(presets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn presets_name_tests_and_categories() {
        let dir = TempDir::new("suite-presets");
        let tiny = "about = \"x\"\ntime = \"1m\"\n[[step]]\nwait = \"ready\"\n";
        for file in ["boot/a.toml", "drivers/b.toml", "drivers/c.toml"] {
            dir.write(file, tiny);
        }
        let tests = load(dir.path()).unwrap();
        assert_eq!(load_presets(dir.path(), &tests).unwrap(), [], "no file, no presets");
        dir.write(
            "presets.toml",
            "[[preset]]\nname = \"quick\"\ntitle = \"Quick check\"\ntests = [\"drivers\", \"boot/a\"]\n",
        );
        let tests = load(dir.path()).unwrap();
        let presets = load_presets(dir.path(), &tests).unwrap();
        assert_eq!(presets[0].tests, ["drivers/b", "drivers/c", "boot/a"]);
        dir.write(
            "presets.toml",
            "[[preset]]\nname = \"q\"\ntitle = \"Q\"\ntests = [\"boot/x\"]\n",
        );
        let err = load_presets(dir.path(), &tests).unwrap_err();
        assert!(
            matches!(err, Error::Usage(why) if why.contains("preset \"q\"") && why.contains("boot/x"))
        );
    }

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
                root: false,
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
    fn the_suite_names_its_categories_and_their_order() {
        let dir = TempDir::new("suite-categories");
        let tiny = "about = \"x\"\ntime = \"1m\"\n[[step]]\nwait = \"ready\"\n";
        for file in ["network/a.toml", "boot/b.toml", "storage/c.toml"] {
            dir.write(file, tiny);
        }
        let categories = "[[category]]\nname = \"storage\"\ntitle = \"Disks and files\"\n\
                          [[category]]\nname = \"boot\"\ntitle = \"Boot and power\"\n\
                          about = \"Booting, restarting, turning off\"\n\
                          [[category]]\nname = \"network\"\ntitle = \"Network\"\n";
        dir.write("categories.toml", categories);
        let ids: Vec<String> = load(dir.path()).unwrap().into_iter().map(|test| test.id).collect();
        assert_eq!(ids, ["storage/c", "boot/b", "network/a"]);
        let loaded = load_categories(dir.path()).unwrap();
        assert_eq!(loaded[1].title, "Boot and power");
        assert_eq!(loaded[1].about, "Booting, restarting, turning off");

        // A folder the file does not name stops everything, saying where
        // to add it.
        dir.write("hardware/d.toml", tiny);
        let Err(Error::Usage(why)) = load(dir.path()) else { panic!("loaded") };
        assert!(why.contains("not a test category") && why.contains("categories.toml"), "{why}");

        // Without the file, the built-in ones.
        let old = TempDir::new("suite-categories-old");
        old.write("drivers/link.toml", tiny);
        assert_eq!(load(old.path()).unwrap()[0].category, "drivers");
        assert_eq!(load_categories(old.path()).unwrap()[1].title, "Drivers and hardware");

        for (text, complaint) in [
            ("[[category]]\nname = \"Boot\"\ntitle = \"x\"\n", "letters, digits and dashes"),
            (
                "[[category]]\nname = \"a\"\ntitle = \"x\"\n[[category]]\nname = \"a\"\ntitle = \"y\"\n",
                "named twice",
            ),
            ("[[category]]\nname = \"a\"\ntitle = \" \"\n", "1 to 40 characters"),
            ("[[category]]\nname = \"a\"\ntitle = \"x\"\ncolour = 1\n", "unknown field"),
            ("", "names no category"),
        ] {
            dir.write("categories.toml", text);
            let Err(Error::Usage(why)) = load_categories(dir.path()) else { panic!("{text}") };
            assert!(why.contains(complaint), "{text}: {why}");
        }
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
                "wait is \"ready\", \"off\" or a time",
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
    fn a_usb_stick_is_a_file_beside_the_test_or_blank() {
        let dir = TempDir::new("suite-usb");
        dir.write("features/stick.img", "");
        dir.write(
            "features/stick.toml",
            "about = \"x\"\ntime = \"1m\"\n[[step]]\nusb = \"plug\"\nimage = \"stick.img\"\n\
             [[step]]\nusb = \"unplug\"\n[[step]]\nusb = \"plug\"\nblank = \"64M\"\n",
        );
        let tests = load(dir.path()).unwrap();
        let image = dir.path().join("features/stick.img");
        assert_eq!(tests[0].steps[0], Step::PlugUsb(Stick::Image(image)));
        assert_eq!(tests[0].steps[1], Step::UnplugUsb);
        assert_eq!(tests[0].steps[2], Step::PlugUsb(Stick::Blank(64 << 20)));
        assert_eq!(tests[0].steps[2].describe(), "plug in a blank USB stick of 64M");
        assert_eq!(parse_size("2G"), Some(2 << 30));
        for size in ["64", "64K", "0M", "8G", "-1M", "M"] {
            dir.write(
                "features/stick.toml",
                &format!(
                    "about = \"x\"\ntime = \"1m\"\n[[step]]\nusb = \"plug\"\nblank = \"{size}\"\n"
                ),
            );
            let Err(Error::Usage(why)) = load(dir.path()) else { panic!("{size} loaded") };
            assert!(why.contains("a size from 1M to 4G"), "{size}: {why}");
        }
    }

    #[test]
    fn root_goes_with_run_and_off_with_wait() {
        let dir = TempDir::new("suite-root");
        dir.write(
            "boot/off.toml",
            "about = \"x\"\ntime = \"1m\"\n[[step]]\nrun = \"poweroff\"\nroot = true\n\
             [[step]]\nwait = \"off\"\n",
        );
        let test = &load(dir.path()).unwrap()[0];
        let Step::Run(command) = &test.steps[0] else { panic!("not a run") };
        assert!(command.root);
        assert_eq!(test.steps[0].describe(), "run `poweroff` as root, exit status 0");
        assert_eq!(test.steps[1], Step::WaitOff);
        assert_eq!(test.steps[1].control(), Some(Control::Power));
        dir.write(
            "boot/off.toml",
            "about = \"x\"\ntime = \"1m\"\n[[step]]\nwait = \"off\"\nroot = true\n",
        );
        let Err(Error::Usage(why)) = load(dir.path()) else { panic!("loaded") };
        assert!(why.contains("root goes only with run"), "{why}");
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

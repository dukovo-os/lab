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
use crate::peer::{self, INPUT_MAX};
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
    /// Programs the lab PC must have for the test's host steps; without
    /// one, the test is skipped.
    pub host_needs: Vec<String>,
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
    /// As [`Boot::Stick`], from the install DVD the run was given, in the
    /// machine's optical drive, which the firmware starts first.
    Dvd { disk: u64 },
}

/// What a `type` step types in place of this text: the lab password, which
/// never shows in a test file, a step's description or a log.
pub const PASSWORD_TOKEN: &str = "{password}";

/// What an `attack` step's command holds in place of this text: the
/// machine under test's address on the private lab network.
pub const TARGET_TOKEN: &str = "{target}";

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
    /// Puts a memory card in the machine's card reader: the image or
    /// blank a [`Stick`] says, on a card.
    InsertCard(Stick),
    /// Takes the memory card out of the card reader, whether or not the
    /// machine let it go.
    RemoveCard,
    /// Puts the memory card taken out last back in, holding what the
    /// machine wrote on it.
    ReinsertCard,
    /// Puts a disc holding this image, beside the test, in the machine's
    /// optical drive.
    InsertDisc(PathBuf),
    /// Takes the disc out of the optical drive, whether or not the
    /// machine let it go.
    EjectDisc,
    /// Puts the disc taken out last back in.
    ReinsertDisc,
    /// Presses the optical drive's eject button: a drive whose disc the
    /// machine locked in keeps it and asks the machine to let it go; one
    /// it did not opens, and the disc is taken out.
    PressDiscButton,
    /// Puts a floppy disk in the machine's floppy drive.
    InsertFloppy(Floppy),
    /// Takes the floppy disk out of the floppy drive, whether or not the
    /// machine let it go.
    EjectFloppy,
    /// Puts the floppy disk taken out last back in, holding what the
    /// machine wrote on it, its write-protect tab as it was.
    ReinsertFloppy,
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
    /// Runs a command on the attacker machine, on a private lab network
    /// with the machine under test, and checks what it did. `{target}`
    /// in the command becomes the machine's address on that network.
    Attack(Command),
    /// Runs a command on the lab PC, the machine's peer, and checks what
    /// it did (see peer.rs).
    Host(Command),
    /// Starts a server on the lab PC that runs until the test ends.
    Serve(Serve),
}

/// A server a `host` step with `background = true` starts on the lab PC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Serve {
    pub command: String,
    /// Run as root on the lab PC.
    pub root: bool,
    /// Text its output holds once it is ready, which the step waits for.
    pub ready: Option<String>,
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

/// What a plugged-in USB stick, or a memory card put in a card reader,
/// holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stick {
    /// A copy of this image file, beside the test.
    Image(PathBuf),
    /// Nothing: this many bytes of zeros, for a test that formats it.
    Blank(u64),
}

/// The floppy disk a `floppy = "insert"` step puts in a floppy drive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Floppy {
    /// What it holds: a copy of an image, or a blank disk of 720 KB or
    /// 1.44 MB, all zeros, as a disk never formatted reads.
    pub holds: Stick,
    /// Whether its write-protect tab is open, so the drive writes nothing
    /// to it.
    pub protected: bool,
}

/// The floppy disks a 3.5-inch drive takes, as `blank` names them, and
/// their sizes: a disk's image is exactly one of these.
pub const FLOPPY_SIZES: [(&str, u64); 2] = [("720K", 737_280), ("1.44M", 1_474_560)];

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
    /// Whether it runs as root: on the machine through `elevate -S` and
    /// the lab's password; on the lab PC as the lab is, or through sudo.
    pub root: bool,
    /// Its standard input, where `{password}` is the lab password; none
    /// gives it none.
    pub input: Option<String>,
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
            Step::InsertCard(_) | Step::RemoveCard | Step::ReinsertCard => Some(Control::Card),
            Step::InsertDisc(_) | Step::EjectDisc | Step::ReinsertDisc | Step::PressDiscButton => {
                Some(Control::Disc)
            }
            Step::InsertFloppy(_) | Step::EjectFloppy | Step::ReinsertFloppy => {
                Some(Control::Floppy)
            }
            Step::Keys(_) | Step::Type(_) => Some(Control::Keys),
            Step::Point { .. } | Step::Click(_) => Some(Control::Mouse),
            Step::Attack(_) => Some(Control::Attack),
            Step::Host(_) | Step::Serve(_) => Some(Control::Peer),
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
            Step::InsertCard(Stick::Image(image)) => {
                let name = image.file_name().unwrap_or_default().to_string_lossy();
                format!("put a memory card holding {name} in the card reader")
            }
            Step::InsertCard(Stick::Blank(bytes)) => {
                format!("put a blank memory card of {} in the card reader", format_size(*bytes))
            }
            Step::RemoveCard => "take the memory card out of the card reader".into(),
            Step::ReinsertCard => "put the memory card back in the card reader".into(),
            Step::InsertDisc(image) => {
                let name = image.file_name().unwrap_or_default().to_string_lossy();
                format!("put a disc holding {name} in the optical drive")
            }
            Step::EjectDisc => "take the disc out of the optical drive".into(),
            Step::ReinsertDisc => "put the disc back in the optical drive".into(),
            Step::PressDiscButton => "press the optical drive's eject button".into(),
            Step::InsertFloppy(floppy) => {
                let disk = match &floppy.holds {
                    Stick::Image(image) => {
                        let name = image.file_name().unwrap_or_default().to_string_lossy();
                        format!("a floppy disk holding {name}")
                    }
                    Stick::Blank(bytes) => {
                        let size = FLOPPY_SIZES
                            .iter()
                            .find(|(_, size)| size == bytes)
                            .map_or("odd", |(name, _)| name);
                        format!("a blank {size} floppy disk")
                    }
                };
                let tab = if floppy.protected { ", write-protected," } else { "" };
                format!("put {disk}{tab} in the floppy drive")
            }
            Step::EjectFloppy => "take the floppy disk out of the floppy drive".into(),
            Step::ReinsertFloppy => "put the floppy disk back in the floppy drive".into(),
            Step::Keys(keys) => format!("press {}", keys.join(", ")),
            Step::Type(text) => format!("type \"{}\"", text.escape_debug()),
            Step::Point { x, y } => format!("point at ({x}, {y}) on the screen"),
            Step::Click(button) => format!("click the {} button", button.name()),
            Step::Attack(command) => format!("on the attacker, {}", describe_command(command)),
            Step::Host(command) => format!("on the lab PC, {}", describe_command(command)),
            Step::Serve(serve) => {
                let who = if serve.root { " as root" } else { "" };
                match &serve.ready {
                    Some(ready) => format!(
                        "on the lab PC, start `{}`{who} and wait until it says \"{ready}\"",
                        serve.command
                    ),
                    None => format!("on the lab PC, start `{}`{who}", serve.command),
                }
            }
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
    if command.input.is_some() {
        text.push_str(", with input");
    }
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
        // An attack runs commands on the attacker, but the harness also
        // uses the machine's own SSH to give it its address on the
        // private network.
        if self.steps.iter().any(|step| matches!(step, Step::Attack(_))) {
            controls.insert(Control::Ssh);
        }
        // A command on the machine that names the lab PC's address or one
        // of its ports reaches a server of the lab PC's.
        let names_peer = |command: &Command| {
            peer::uses_peer(&command.command)
                || command.input.as_deref().is_some_and(peer::uses_peer)
        };
        if self.steps.iter().any(|step| matches!(step, Step::Run(command) if names_peer(command))) {
            controls.insert(Control::Peer);
        }
        // The install stick is plugged in as the machine powers on, and
        // the install DVD is in the optical drive.
        match self.boot {
            Boot::Stick { .. } => {
                controls.insert(Control::Usb);
            }
            Boot::Dvd { .. } => {
                controls.insert(Control::Disc);
            }
            Boot::Image => {}
        }
        controls.insert(Control::Power);
        controls.insert(Control::Serial);
        controls
    }

    /// Whether a step needs the lab password: one run as root, or one that
    /// types it.
    pub fn uses_password(&self) -> bool {
        let types_it = |command: &Command| {
            command.input.as_deref().is_some_and(|input| input.contains(PASSWORD_TOKEN))
        };
        self.steps.iter().any(|step| match step {
            Step::Run(command) => command.root || types_it(command),
            Step::Host(command) => types_it(command),
            Step::Bench(bench) => bench.root,
            Step::Type(text) => text.contains(PASSWORD_TOKEN),
            // The harness elevates on both machines to give them their
            // addresses on the private network, so any attack needs it.
            Step::Attack(_) => true,
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
    #[serde(default)]
    host_needs: Vec<String>,
    #[serde(rename = "step", default)]
    steps: Vec<StepFile>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum BootFrom {
    Image,
    Stick,
    Dvd,
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
    card: Option<CardAction>,
    disc: Option<DiscAction>,
    floppy: Option<FloppyAction>,
    image: Option<String>,
    blank: Option<String>,
    protected: Option<bool>,
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
    attack: Option<String>,
    host: Option<String>,
    background: Option<bool>,
    ready: Option<String>,
    input: Option<String>,
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

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum CardAction {
    Insert,
    Remove,
    Reinsert,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum DiscAction {
    Insert,
    Eject,
    Reinsert,
    Button,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum FloppyAction {
    Insert,
    Eject,
    Reinsert,
}

/// `expect = "text"` or `expect = ["one", "two"]`.
#[derive(Deserialize, Clone)]
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
    let disk = match file.disk {
        None => DEFAULT_INSTALL_DISK,
        Some(_) if matches!(file.boot, None | Some(BootFrom::Image)) => {
            return Err(bad("disk goes only with boot = \"stick\" or \"dvd\"".into()));
        }
        Some(size) => match parse_size(&size) {
            Some(disk) if (1 << 30..=DISK_MAX).contains(&disk) => disk,
            _ => {
                return Err(bad(format!(
                    "disk = \"{size}\": a size from 1G to 2048G, as a number and M or G, such as \"8G\""
                )));
            }
        },
    };
    let boot = match file.boot {
        None | Some(BootFrom::Image) => Boot::Image,
        Some(BootFrom::Stick) => Boot::Stick { disk },
        Some(BootFrom::Dvd) => Boot::Dvd { disk },
    };
    // A program's name, as the lab PC's PATH finds it: no folders.
    if let Some(bad_name) = file.host_needs.iter().find(|name| {
        name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
    }) {
        return Err(bad(format!(
            "host_needs: {bad_name:?} is not a program's name, as krb5kdc or smbd"
        )));
    }
    let folder = path.parent().unwrap_or(Path::new("."));
    let mut steps = Vec::new();
    for (index, step) in file.steps.into_iter().enumerate() {
        let step =
            parse_step(step, folder).map_err(|err| bad(format!("step {}: {err}", index + 1)))?;
        steps.push(step);
    }
    // A card, disc or floppy disk is put back only once one was taken
    // out.
    let (mut card_out, mut disc_out, mut floppy_out) = (false, false, false);
    for (index, step) in steps.iter().enumerate() {
        match step {
            Step::RemoveCard => card_out = true,
            Step::InsertCard(_) => card_out = false,
            Step::ReinsertCard if !card_out => {
                return Err(bad(format!(
                    "step {}: card = \"reinsert\" puts back a card the test took out, and none is out",
                    index + 1
                )));
            }
            Step::ReinsertCard => card_out = false,
            Step::EjectDisc | Step::PressDiscButton => disc_out = true,
            Step::InsertDisc(_) => disc_out = false,
            Step::ReinsertDisc if !disc_out => {
                return Err(bad(format!(
                    "step {}: disc = \"reinsert\" puts back a disc the test took out, and none is out",
                    index + 1
                )));
            }
            Step::ReinsertDisc => disc_out = false,
            Step::EjectFloppy => floppy_out = true,
            Step::InsertFloppy(_) => floppy_out = false,
            Step::ReinsertFloppy if !floppy_out => {
                return Err(bad(format!(
                    "step {}: floppy = \"reinsert\" puts back a floppy disk the test took out, and none is out",
                    index + 1
                )));
            }
            Step::ReinsertFloppy => floppy_out = false,
            _ => {}
        }
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
        host_needs: file.host_needs,
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
        ("card", step.card.is_some()),
        ("disc", step.disc.is_some()),
        ("floppy", step.floppy.is_some()),
        ("keys", step.keys.is_some()),
        ("screenshot", step.screenshot.is_some()),
        ("wait", step.wait.is_some()),
        ("bench", step.bench.is_some()),
        ("screen", step.screen.is_some()),
        ("screen_gone", step.screen_gone.is_some()),
        ("type", step.typed.is_some()),
        ("point", step.point.is_some()),
        ("click", step.click.is_some()),
        ("attack", step.attack.is_some()),
        ("host", step.host.is_some()),
    ];
    let given: Vec<&str> = actions.iter().filter(|(_, set)| *set).map(|(key, _)| *key).collect();
    match given.as_slice() {
        [] => {
            return Err(
                "no action: give one of run, attack, host, serial, power, link, usb, card, disc, floppy, keys, type, point, click, screen, screen_gone, screenshot, wait or bench"
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
    // A command's checks go with `run`, `attack` or `host`, which all run
    // one and wait for its end; a server started in the background has no
    // end to check.
    let background = step.background.unwrap_or(false);
    let checks = |option: &str, set: bool| {
        if set && !matches!(given[0], "run" | "attack" | "host") {
            Err(format!("{option} goes only with run, attack or host"))
        } else if set && background {
            Err(format!(
                "{option} does not go with background = true: a server has no end to check"
            ))
        } else {
            Ok(())
        }
    };
    checks("exit", step.exit.is_some())?;
    checks("expect", step.expect.is_some())?;
    checks("expect_not", step.expect_not.is_some())?;
    if step.root.is_some() && !matches!(given[0], "run" | "bench" | "attack" | "host") {
        return Err("root goes only with run, bench, attack or host".into());
    }
    if step.input.is_some() && !matches!(given[0], "run" | "host") {
        return Err("input goes only with run or host".into());
    }
    if step.input.is_some() && background {
        return Err("input does not go with background = true".into());
    }
    if let Some(input) = &step.input
        && input.len() > INPUT_MAX
    {
        return Err(format!("input holds at most {} KiB", INPUT_MAX / 1024));
    }
    only_with("background", step.background.is_some(), "host")?;
    if step.ready.is_some() && !background {
        return Err("ready goes only with host and background = true".into());
    }
    only_with("rows", step.rows.is_some(), "bench")?;
    only_with("limit", step.limit.is_some(), "bench")?;
    // A stick's, a card's, a disc's or a floppy disk's: what it holds. A
    // disc is never blank: a blank one holds nothing to read.
    if step.image.is_some() && !matches!(given[0], "usb" | "card" | "disc" | "floppy") {
        return Err("image goes only with usb, card, disc or floppy".into());
    }
    if step.blank.is_some() && !matches!(given[0], "usb" | "card" | "floppy") {
        return Err("blank goes only with usb, card or floppy".into());
    }
    only_with("protected", step.protected.is_some(), "floppy")?;

    let command = |text: String, step: &StepFile| Command {
        command: text,
        exit: step.exit.unwrap_or(0),
        expect: step.expect.clone().map(OneOrMore::into_vec).unwrap_or_default(),
        expect_not: step.expect_not.clone().map(OneOrMore::into_vec).unwrap_or_default(),
        root: step.root.unwrap_or(false),
        input: step.input.clone(),
    };
    if let Some(text) = step.run.clone() {
        if text.trim().is_empty() {
            return Err("run needs a command".into());
        }
        let command = command(text, &step);
        check_placeholders(&command, "run")?;
        return Ok(Step::Run(command));
    }
    if let Some(text) = step.attack.clone() {
        if text.trim().is_empty() {
            return Err("attack needs a command to run on the attacker".into());
        }
        let command = command(text, &step);
        check_placeholders(&command, "attack")?;
        return Ok(Step::Attack(command));
    }
    if let Some(text) = step.host.clone() {
        if text.trim().is_empty() {
            return Err("host needs a command to run on the lab PC".into());
        }
        if background {
            let ready = step.ready.clone();
            if ready.as_deref().is_some_and(|ready| ready.trim().is_empty()) {
                return Err("ready needs the text the server prints once it is ready".into());
            }
            for text in std::iter::once(&text).chain(&ready) {
                peer::scan(text)?;
            }
            return Ok(Step::Serve(Serve {
                command: text,
                root: step.root.unwrap_or(false),
                ready,
            }));
        }
        let command = command(text, &step);
        check_placeholders(&command, "host")?;
        return Ok(Step::Host(command));
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
        return match action {
            UsbAction::Unplug if step.image.is_some() || step.blank.is_some() => {
                Err("usb = \"unplug\" takes no image or blank".into())
            }
            UsbAction::Unplug => Ok(Step::UnplugUsb),
            UsbAction::Plug => {
                parse_medium("usb = \"plug\"", "stick", step.image, step.blank, folder)
                    .map(Step::PlugUsb)
            }
        };
    }
    if let Some(action) = step.card {
        return match action {
            CardAction::Remove | CardAction::Reinsert
                if step.image.is_some() || step.blank.is_some() =>
            {
                Err("only card = \"insert\" takes an image or blank".into())
            }
            CardAction::Remove => Ok(Step::RemoveCard),
            CardAction::Reinsert => Ok(Step::ReinsertCard),
            CardAction::Insert => {
                parse_medium("card = \"insert\"", "card", step.image, step.blank, folder)
                    .map(Step::InsertCard)
            }
        };
    }
    if let Some(action) = step.disc {
        return match (action, step.image) {
            (DiscAction::Eject | DiscAction::Reinsert | DiscAction::Button, Some(_)) => {
                Err("only disc = \"insert\" takes an image".into())
            }
            (DiscAction::Eject, None) => Ok(Step::EjectDisc),
            (DiscAction::Reinsert, None) => Ok(Step::ReinsertDisc),
            (DiscAction::Button, None) => Ok(Step::PressDiscButton),
            (DiscAction::Insert, None) => {
                Err("disc = \"insert\" needs image, a disc image beside the test".into())
            }
            (DiscAction::Insert, image) => {
                match parse_medium("disc = \"insert\"", "disc", image, None, folder)? {
                    Stick::Image(path) => Ok(Step::InsertDisc(path)),
                    Stick::Blank(_) => Err("a disc is never blank".into()),
                }
            }
        };
    }
    if let Some(action) = step.floppy {
        return match action {
            FloppyAction::Eject | FloppyAction::Reinsert
                if step.image.is_some() || step.blank.is_some() || step.protected.is_some() =>
            {
                Err("only floppy = \"insert\" takes an image, blank or protected".into())
            }
            FloppyAction::Eject => Ok(Step::EjectFloppy),
            FloppyAction::Reinsert => Ok(Step::ReinsertFloppy),
            FloppyAction::Insert => Ok(Step::InsertFloppy(Floppy {
                holds: parse_floppy(step.image, step.blank, folder)?,
                protected: step.protected.unwrap_or(false),
            })),
        };
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

/// Checks the placeholders of a `run`, `attack` or `host` step's command
/// (see peer.rs): each written right, and each one the step may hold.
fn check_placeholders(command: &Command, action: &str) -> Result<(), String> {
    let texts = std::iter::once(&command.command)
        .chain(&command.input)
        .chain(&command.expect)
        .chain(&command.expect_not);
    for text in texts {
        for (range, token) in peer::scan(text)? {
            let written = &text[range];
            match action {
                "host" => {}
                "run" if token.fits_run() => {}
                "run" => {
                    return Err(format!(
                        "{written} names something of the lab PC's, so it goes only in a host step"
                    ));
                }
                _ => {
                    return Err(format!(
                        "{written} goes in run and host steps; the attacker's network has no lab PC"
                    ));
                }
            }
        }
    }
    Ok(())
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

/// The largest blank stick or card: enough for any filesystem's smallest
/// size, and quick to copy.
const BLANK_MAX: u64 = 4 << 30;

/// What a stick or card that `action` (`usb = "plug"`, `card = "insert"`)
/// puts in holds: a copy of `image`, or `blank` bytes of zeros. `what`
/// names it in the messages.
fn parse_medium(
    action: &str,
    what: &str,
    image: Option<String>,
    blank: Option<String>,
    folder: &Path,
) -> Result<Stick, String> {
    match (image, blank) {
        (None, None) => Err(format!(
            "{action} needs image, a file beside the test, or blank, a size such as \"64M\""
        )),
        (Some(_), Some(_)) => Err(format!("a {what} is an image or blank, not both")),
        (None, Some(size)) => match parse_size(&size) {
            Some(bytes) if (1 << 20..=BLANK_MAX).contains(&bytes) => Ok(Stick::Blank(bytes)),
            _ => Err(format!(
                "blank = \"{size}\": a size from 1M to 4G, as a number and M or G, such as \"64M\""
            )),
        },
        (Some(image), None) => {
            // Only files beside the test, so a test cannot read the lab
            // PC's other files into a machine.
            if image.contains('/') || image.starts_with('.') {
                return Err(format!("image \"{image}\" must be a file name in the test's folder"));
            }
            let path = folder.join(&image);
            if !path.is_file() {
                return Err(format!("image {} does not exist", path.display()));
            }
            Ok(Stick::Image(path))
        }
    }
}

/// What a floppy disk that `floppy = "insert"` puts in holds: a copy of
/// `image`, which is the size of a disk, or a blank disk of a size
/// [`FLOPPY_SIZES`] names.
fn parse_floppy(
    image: Option<String>,
    blank: Option<String>,
    folder: &Path,
) -> Result<Stick, String> {
    const SIZES: &str = "\"720K\" or \"1.44M\"";
    match (image, blank) {
        (None, None) => Err(format!(
            "floppy = \"insert\" needs image, a disk image beside the test, or blank, {SIZES}"
        )),
        (Some(_), Some(_)) => Err("a floppy disk is an image or blank, not both".into()),
        (None, Some(size)) => FLOPPY_SIZES
            .iter()
            .find(|(name, _)| *name == size)
            .map(|&(_, bytes)| Stick::Blank(bytes))
            .ok_or_else(|| format!("blank = \"{size}\": a floppy disk is {SIZES}")),
        (image, None) => {
            let disk = parse_medium("floppy = \"insert\"", "floppy disk", image, None, folder)?;
            if let Stick::Image(path) = &disk {
                // A drive reads a disk of another size wrongly, or not at
                // all, which would fail the test for the wrong reason.
                let bytes = fs::metadata(path)
                    .map_err(|err| format!("image {}: {err}", path.display()))?
                    .len();
                if !FLOPPY_SIZES.iter().any(|&(_, size)| size == bytes) {
                    return Err(format!(
                        "image {} is {bytes} bytes, and a floppy disk's image is 737280 (720 KB) or 1474560 (1.44 MB)",
                        path.display()
                    ));
                }
            }
            Ok(disk)
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
                input: None,
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
            ("boot/n.toml", &format!("{head}[[step]]\ntype = \"café\"\n"), "no key for 'é'"),
            (
                "boot/o.toml",
                &format!("{head}[[step]]\nscreen = \"two\\nrows\"\n"),
                "each within one row",
            ),
            ("boot/p.toml", &format!("{head}[[step]]\npoint = [1]\n"), "point needs the pixel"),
            ("boot/q.toml", &format!("{head}[[step]]\nclick = \"thumb\"\n"), "unknown variant"),
            (
                "boot/r.toml",
                &format!("{head}disk = \"8G\"\n[[step]]\nwait = \"ready\"\n"),
                "disk goes only with boot",
            ),
            (
                "boot/s.toml",
                &format!("{head}boot = \"stick\"\ndisk = \"4T\"\n[[step]]\nwait = \"ready\"\n"),
                "disk",
            ),
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
    fn a_memory_card_is_put_in_and_taken_out_of_the_card_reader() {
        let dir = TempDir::new("suite-card");
        dir.write("features/card.img", "");
        dir.write(
            "features/card.toml",
            "about = \"x\"\ntime = \"1m\"\n[[step]]\ncard = \"insert\"\nimage = \"card.img\"\n\
             [[step]]\ncard = \"remove\"\n[[step]]\ncard = \"insert\"\nblank = \"32M\"\n\
             [[step]]\ncard = \"remove\"\n[[step]]\ncard = \"reinsert\"\n",
        );
        let tests = load(dir.path()).unwrap();
        let image = dir.path().join("features/card.img");
        assert_eq!(tests[0].steps[0], Step::InsertCard(Stick::Image(image)));
        assert_eq!(
            tests[0].steps[0].describe(),
            "put a memory card holding card.img in the card reader"
        );
        assert_eq!(tests[0].steps[1], Step::RemoveCard);
        assert_eq!(tests[0].steps[1].describe(), "take the memory card out of the card reader");
        assert_eq!(tests[0].steps[2], Step::InsertCard(Stick::Blank(32 << 20)));
        assert_eq!(tests[0].steps[4], Step::ReinsertCard);
        assert_eq!(tests[0].steps[4].describe(), "put the memory card back in the card reader");
        assert!(tests[0].steps.iter().all(|step| step.control() == Some(Control::Card)));
        assert!(tests[0].controls().contains(&Control::Card));
        for (step, complaint) in [
            ("card = \"insert\"", "card = \"insert\" needs image"),
            ("card = \"remove\"\nblank = \"8M\"", "only card = \"insert\" takes an image or blank"),
            ("card = \"reinsert\"", "none is out"),
            ("card = \"insert\"\nblank = \"8M\"\n[[step]]\ncard = \"reinsert\"", "none is out"),
            (
                "card = \"insert\"\nimage = \"card.img\"\nblank = \"8M\"",
                "a card is an image or blank",
            ),
            ("card = \"insert\"\nblank = \"8G\"", "a size from 1M to 4G"),
            ("card = \"open\"", "card"),
            ("run = \"ls\"\nblank = \"8M\"", "blank"),
        ] {
            dir.write(
                "features/card.toml",
                &format!("about = \"x\"\ntime = \"1m\"\n[[step]]\n{step}\n"),
            );
            let Err(Error::Usage(why)) = load(dir.path()) else { panic!("{step} loaded") };
            assert!(why.contains(complaint), "{step}: {why}");
        }
    }

    #[test]
    fn a_disc_is_put_in_and_taken_out_of_the_optical_drive() {
        let dir = TempDir::new("suite-disc");
        dir.write("features/disc.iso", "");
        dir.write(
            "features/disc.toml",
            "about = \"x\"\ntime = \"1m\"\n[[step]]\ndisc = \"insert\"\nimage = \"disc.iso\"\n\
             [[step]]\ndisc = \"eject\"\n[[step]]\ndisc = \"reinsert\"\n\
             [[step]]\ndisc = \"button\"\n",
        );
        let tests = load(dir.path()).unwrap();
        let image = dir.path().join("features/disc.iso");
        assert_eq!(tests[0].steps[0], Step::InsertDisc(image));
        assert_eq!(
            tests[0].steps[0].describe(),
            "put a disc holding disc.iso in the optical drive"
        );
        assert_eq!(tests[0].steps[1], Step::EjectDisc);
        assert_eq!(tests[0].steps[1].describe(), "take the disc out of the optical drive");
        assert_eq!(tests[0].steps[2], Step::ReinsertDisc);
        assert_eq!(tests[0].steps[2].describe(), "put the disc back in the optical drive");
        assert_eq!(tests[0].steps[3], Step::PressDiscButton);
        assert_eq!(tests[0].steps[3].describe(), "press the optical drive's eject button");
        assert!(tests[0].steps.iter().all(|step| step.control() == Some(Control::Disc)));
        assert!(tests[0].controls().contains(&Control::Disc));
        for (step, complaint) in [
            ("disc = \"insert\"", "disc = \"insert\" needs image"),
            ("disc = \"insert\"\nblank = \"8M\"", "blank goes only with usb, card or floppy"),
            ("disc = \"insert\"\nimage = \"none.iso\"", "does not exist"),
            ("disc = \"eject\"\nimage = \"disc.iso\"", "only disc = \"insert\" takes an image"),
            ("disc = \"button\"\nimage = \"disc.iso\"", "only disc = \"insert\" takes an image"),
            ("disc = \"reinsert\"", "none is out"),
            ("disc = \"open\"", "disc"),
            (
                "run = \"ls\"\nimage = \"disc.iso\"",
                "image goes only with usb, card, disc or floppy",
            ),
        ] {
            dir.write(
                "features/disc.toml",
                &format!("about = \"x\"\ntime = \"1m\"\n[[step]]\n{step}\n"),
            );
            let Err(Error::Usage(why)) = load(dir.path()) else { panic!("{step} loaded") };
            assert!(why.contains(complaint), "{step}: {why}");
        }
    }

    #[test]
    fn a_floppy_disk_is_put_in_and_taken_out_of_the_floppy_drive() {
        let dir = TempDir::new("suite-floppy");
        let image = dir.write("features/disk.img", "");
        fs::File::options().write(true).open(&image).unwrap().set_len(1_474_560).unwrap();
        dir.write("features/odd.img", "a disk of no size a drive takes");
        dir.write(
            "features/floppy.toml",
            "about = \"x\"\ntime = \"1m\"\n[[step]]\nfloppy = \"insert\"\nimage = \"disk.img\"\n\
             [[step]]\nfloppy = \"eject\"\n[[step]]\nfloppy = \"reinsert\"\n[[step]]\nfloppy = \"eject\"\n\
             [[step]]\nfloppy = \"insert\"\nblank = \"720K\"\nprotected = true\n",
        );
        let tests = load(dir.path()).unwrap();
        let disk = |holds, protected| Step::InsertFloppy(Floppy { holds, protected });
        assert_eq!(tests[0].steps[0], disk(Stick::Image(image), false));
        assert_eq!(
            tests[0].steps[0].describe(),
            "put a floppy disk holding disk.img in the floppy drive"
        );
        assert_eq!(tests[0].steps[1], Step::EjectFloppy);
        assert_eq!(tests[0].steps[1].describe(), "take the floppy disk out of the floppy drive");
        assert_eq!(tests[0].steps[2], Step::ReinsertFloppy);
        assert_eq!(tests[0].steps[2].describe(), "put the floppy disk back in the floppy drive");
        assert_eq!(tests[0].steps[4], disk(Stick::Blank(737_280), true));
        assert_eq!(
            tests[0].steps[4].describe(),
            "put a blank 720K floppy disk, write-protected, in the floppy drive"
        );
        assert!(tests[0].steps.iter().all(|step| step.control() == Some(Control::Floppy)));
        assert!(tests[0].controls().contains(&Control::Floppy));
        for (step, complaint) in [
            ("floppy = \"insert\"", "floppy = \"insert\" needs image"),
            ("floppy = \"insert\"\nblank = \"64M\"", "a floppy disk is \"720K\" or \"1.44M\""),
            ("floppy = \"insert\"\nimage = \"odd.img\"", "is 31 bytes, and a floppy disk's image"),
            ("floppy = \"insert\"\nimage = \"none.img\"", "does not exist"),
            (
                "floppy = \"insert\"\nimage = \"disk.img\"\nblank = \"720K\"",
                "an image or blank, not both",
            ),
            (
                "floppy = \"eject\"\nprotected = true",
                "only floppy = \"insert\" takes an image, blank or protected",
            ),
            ("floppy = \"reinsert\"", "none is out"),
            ("floppy = \"open\"", "floppy"),
            (
                "card = \"insert\"\nblank = \"8M\"\nprotected = true",
                "protected goes only with floppy",
            ),
        ] {
            dir.write(
                "features/floppy.toml",
                &format!("about = \"x\"\ntime = \"1m\"\n[[step]]\n{step}\n"),
            );
            let Err(Error::Usage(why)) = load(dir.path()) else { panic!("{step} loaded") };
            assert!(why.contains(complaint), "{step}: {why}");
        }
    }

    #[test]
    fn reads_the_screen_and_pointer_steps_and_the_stick() {
        let dir = TempDir::new("suite-screen");
        dir.write(
            "console/screens.toml",
            "about = \"x\"\ntime = \"1m\"\nboot = \"stick\"\ndisk = \"16G\"\n\
             [[step]]\nscreen = [\"Step 1 of 5\", \"Welcome\"]\n\
             [[step]]\nscreen_gone = \"Esc closes\"\n\
             [[step]]\ntype = \"Ada {password}\\n\"\n\
             [[step]]\npoint = [10, 20]\n[[step]]\nclick = \"right\"\n",
        );
        let test = &load(dir.path()).unwrap()[0];
        assert_eq!(test.boot, Boot::Stick { disk: 16 << 30 });
        assert!(test.uses_password());
        let described: Vec<String> = test.steps.iter().map(Step::describe).collect();
        assert_eq!(
            described,
            [
                "wait until the screen shows \"Step 1 of 5\", \"Welcome\"",
                "wait until the screen no longer shows \"Esc closes\"",
                "type \"Ada {password}\\n\"",
                "point at (10, 20) on the screen",
                "click the right button",
            ]
        );
        let controls: Vec<_> = test.steps.iter().map(Step::control).collect();
        assert_eq!(
            controls,
            [
                Some(Control::Screen),
                Some(Control::Screen),
                Some(Control::Keys),
                Some(Control::Mouse),
                Some(Control::Mouse),
            ]
        );
        dir.write(
            "console/screens.toml",
            "about = \"x\"\ntime = \"1m\"\n[[step]]\nwait = \"ready\"\n",
        );
        let test = &load(dir.path()).unwrap()[0];
        assert_eq!(test.boot, Boot::Image);
        assert!(!test.uses_password());
        // The install DVD: in the optical drive, with the default disk.
        dir.write(
            "console/screens.toml",
            "about = \"x\"\ntime = \"1m\"\nboot = \"dvd\"\n[[step]]\nscreen = \"Step 1 of 6\"\n",
        );
        let test = &load(dir.path()).unwrap()[0];
        assert_eq!(test.boot, Boot::Dvd { disk: DEFAULT_INSTALL_DISK });
        assert!(test.controls().contains(&Control::Disc));
        assert!(!test.controls().contains(&Control::Usb));
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
    fn reads_an_attack_step() {
        let dir = TempDir::new("suite-attack");
        dir.write(
            "security/scan.toml",
            "about = \"x\"\ntime = \"1m\"\nfresh = true\n\
             [[step]]\nattack = \"nmap {target}\"\nexpect = \"22/tcp open\"\n",
        );
        let test = &load(dir.path()).unwrap()[0];
        assert_eq!(
            test.steps[0],
            Step::Attack(Command {
                command: "nmap {target}".into(),
                exit: 0,
                expect: vec!["22/tcp open".into()],
                expect_not: vec![],
                root: false,
                input: None,
            })
        );
        assert_eq!(
            test.steps[0].describe(),
            "on the attacker, run `nmap {target}`, exit status 0, output has \"22/tcp open\""
        );
        assert_eq!(test.steps[0].control(), Some(Control::Attack));
        // An attack reaches the machine over SSH, so the test needs it.
        let controls: Vec<Control> = test.controls().into_iter().collect();
        assert!(
            controls.contains(&Control::Attack) && controls.contains(&Control::Ssh),
            "{controls:?}"
        );
        assert!(test.uses_password(), "an attack sets up addresses as root");

        // An attack with no command says so.
        dir.write("security/scan.toml", "about = \"x\"\ntime = \"1m\"\n[[step]]\nattack = \"\"\n");
        let Err(Error::Usage(why)) = load(dir.path()) else { panic!("loaded") };
        assert!(why.contains("attack needs a command"), "{why}");
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

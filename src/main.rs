// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `dukovo-lab`, the Dukovo Lab harness: runs Dukovo's lab tests on QEMU
//! and on real machines, and writes what happened as a results bundle.
//!
//! The commands are thin: [`suite`] reads test files, [`machines`] reads
//! machine files, [`run`] runs tests on machines through their
//! [`driver`]s, [`bundle`] writes what came out, [`status`] reads it back
//! and [`ui`] serves the runner page. Every command can answer in JSON
//! (`--json`), never prompts, and exits with a documented status (see
//! [`HELP`]), so scripts and agents drive it as a person does; the runner
//! page itself works by running these commands. docs/ holds the manual.

mod amt;
mod bench;
mod bundle;
mod clock;
mod des;
mod driver;
mod findings;
mod hash;
mod http;
mod machines;
mod netboot;
mod output;
mod peer;
mod png;
mod qmp;
mod run;
mod schedule;
mod screen;
mod send;
mod serial;
mod ssh;
mod status;
mod stop;
mod store;
mod suite;
mod ui;
mod units;
mod vnc;
mod wsman;
mod zone;

#[cfg(test)]
mod testing;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use machines::{Control, Machine, Who};
use status::State;
use suite::{Fit, Preset, Step, Test};

const HELP: &str = "\
dukovo-lab: run Dukovo's lab tests on QEMU and real machines

Usage: dukovo-lab [options] <command> [arguments]

Commands:
  list [<test or category>...]
                   The tests by category, and how each fits each machine
  show <test>      What a test checks, its steps, its time limit, its file
  machines         The machines and who does each of their controls
  machines check [<machine>...]
                   Checks what runs on real machines need: AMT, the
                   serial port, the lab network, net boot and the key
  run <test or category>... | --all | --preset <name>
                   Runs tests on one or more machines at once, and writes
                   a results bundle
  status [<run>]   How a run is going, or how it went (default: the run
                   going now, else the last)
  stop [<run>]     Stops a run part way; what it did not finish is skipped
  done [<run>] --machine <name>
                   Says a person did the step a run waits for on a machine
  results [<run>]  A run's results: each test on each machine, and why
  runs             The runs so far, newest first
  findings [<finding>]
                   What failed, each failure with the runs it came back
                   in (default: those still open); or one finding
  send <finding>... | --all | --run <run>
                   Sends findings, and the runs they failed in, to the
                   lab-results repository, where they are worked on
  schedule         The schedules: when each runs next and how it went last
  schedule save <name> <test or category>... | --all | --preset <name>
                   (--at <HH:MM> [--days <mon,...>] | --each-build)
                   Makes a schedule, or changes it: tests that run by
                   themselves at a time (in Mountain Time) or on each new
                   build on main, on the lab's own Dukovo checkout
  schedule remove | pause | resume | start <name>
                   Removes, pauses or resumes a schedule, or starts it now
  schedule install Puts in place the systemd timer that starts schedules
  schedule tick    What the timer runs each minute: starts those due
  ui               Serves the runner page on this PC, for a browser here
  key              Makes the lab key, once, and prints its public key's path
  help             This text

Options:
  --tests <dir>    The test folder (default: lab/tests)
  --config <dir>   The configuration folder: machines/ and the lab key
                   (default: ~/.config/dukovo-lab)
  --store <dir>    Where results bundles go (default: ~/.local/share/dukovo-lab)
  --json           Answer in JSON; `run` prints one JSON object per line
  --version        Print the version

Options for run:
  --image <file>   The image under test (from `ob image --lab`)
  --linux <dir>    The Linux kit performance tests hold Dukovo to (`ob lab`
                   builds it in out/lab-linux)
  --fonts <dir>    The console's PSF fonts, for steps that read the screen
                   (`ob lab` gives Dukovo's)
  --stick <file>   The install stick's image, for tests that boot it (`ob
                   lab` builds it with `ob image`)
  --dvd <file>     The install DVD's ISO, for tests that boot it (`ob lab`
                   builds it with `ob image --iso`)
  --ob <file>      Dukovo's ob, whose `ob disc-drive` holds a music CD in a
                   QEMU machine's emulated USB CD drive (`ob lab` gives its
                   own)
  --machine <a,b>  The machines to run on (default: qemu)
  --build <text>   The build's name for the bundle, such as \"0.13.0 fd2a9e8\"
  --all            Every test
  --preset <name>  The tests of a preset in presets.toml
  --with-person    Also run Needs you tests, asking a person to do their steps
  --detach         Start the run in the background, print its id and return

Options for other commands:
  --wait           status: wait until the run ends
  --failed         results: only what failed or broke
  --limit <n>      runs: how many (default 20)
  --port <n>       ui: the port on 127.0.0.1 (default 7420)
  --no-open        ui: do not open a browser
  --run <run>      findings: those of a run; send: a run and its findings
  --all            findings: every finding, closed ones too;
                   send: every finding not sent yet, and its runs
  --not-a-bug      findings <finding>: says it is not a bug; it stays so
                   and is not offered to send again
  --note <text>    with --not-a-bug: why
  --reopen         findings <finding>: takes back --not-a-bug
  --results <dir>  send: the lab-results clone (default: lab-results in
                   the store)

Options for schedule save (and --machine, --preset, --all as for run):
  --at <HH:MM>     The time it runs, 24-hour, in the lab's zone
  --days <a,b>     The days it runs on, such as sun or mon,wed,fri
                   (default: every day)
  --each-build     Runs on each new commit on Dukovo's main instead
  --send           Sends its findings to lab-results when it ends (default)
  --hold           Holds its findings for a person to send from the page
  --title <text>   What to call it, such as \"Full run\"

Exit status:
  0  every test that ran passed (tests may be skipped)
  1  a test failed
  2  a bad argument, test file or machine file
  3  the harness or a machine broke, so some results say nothing about Dukovo
  4  a machine the run needs is busy with another run; nothing ran
status and results exit as the run did (0, 1 or 3; 3 for a run that died),
or 0 while it is going. schedule start exits 4 when that schedule is
already going.
";

/// Why a command did not finish.
#[derive(Debug)]
pub enum Error {
    /// A bad argument or file: exit status 2.
    Usage(String),
    /// The harness or a machine broke: exit status 3.
    Harness(String),
    /// A machine the run needs is busy with another run: exit status 4.
    Busy(String),
}

/// The command line, read.
#[derive(Default)]
struct Arguments {
    command: Option<String>,
    names: Vec<String>,
    tests: Option<PathBuf>,
    config: Option<PathBuf>,
    store: Option<PathBuf>,
    image: Option<PathBuf>,
    /// The Linux kit performance tests hold Dukovo to.
    linux: Option<PathBuf>,
    /// The console's fonts, for reading the screen.
    fonts: Option<PathBuf>,
    /// The install stick's image.
    stick: Option<PathBuf>,
    /// The install DVD's ISO.
    dvd: Option<PathBuf>,
    /// Dukovo's ob, for music CDs in QEMU.
    ob: Option<PathBuf>,
    machine: Option<String>,
    build: Option<String>,
    preset: Option<String>,
    /// The id a detached run's parent chose for it.
    run_id: Option<String>,
    port: Option<u16>,
    limit: Option<usize>,
    all: bool,
    json: bool,
    detach: bool,
    wait: bool,
    with_person: bool,
    failed: bool,
    no_open: bool,
    /// findings and send: a run.
    run: Option<String>,
    /// send: the lab-results clone.
    results: Option<PathBuf>,
    note: Option<String>,
    not_a_bug: bool,
    reopen: bool,
    /// schedule save: when, which days, or each build; send or hold.
    at: Option<String>,
    days: Option<String>,
    each_build: bool,
    send: bool,
    hold: bool,
    title: Option<String>,
    /// run: the schedule that started it.
    schedule: Option<String>,
}

fn main() -> ExitCode {
    let arguments = match parse(std::env::args().skip(1)) {
        Ok(arguments) => arguments,
        Err(err) => return fail(&err, false),
    };
    let json = arguments.json;
    match dispatch(&arguments) {
        Ok(status) => ExitCode::from(status),
        Err(err) => fail(&err, json),
    }
}

/// Prints `err` and gives its exit status.
fn fail(err: &Error, json: bool) -> ExitCode {
    let (message, status) = match err {
        Error::Usage(message) => (message, 2),
        Error::Harness(message) => (message, 3),
        Error::Busy(message) => (message, 4),
    };
    if json {
        println!("{}", json!({ "error": message, "status": status }));
    } else {
        eprintln!("dukovo-lab: {message}");
    }
    ExitCode::from(status)
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Arguments, Error> {
    let mut arguments = Arguments::default();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .ok_or_else(|| Error::Usage(format!("{name} needs a value; see `dukovo-lab help`")))
        };
        match arg.as_str() {
            "--tests" => arguments.tests = Some(value("--tests")?.into()),
            "--config" => arguments.config = Some(value("--config")?.into()),
            "--store" => arguments.store = Some(value("--store")?.into()),
            "--image" => arguments.image = Some(value("--image")?.into()),
            "--linux" => arguments.linux = Some(value("--linux")?.into()),
            "--fonts" => arguments.fonts = Some(value("--fonts")?.into()),
            "--stick" => arguments.stick = Some(value("--stick")?.into()),
            "--dvd" => arguments.dvd = Some(value("--dvd")?.into()),
            "--ob" => arguments.ob = Some(value("--ob")?.into()),
            "--machine" => arguments.machine = Some(value("--machine")?),
            "--build" => arguments.build = Some(value("--build")?),
            "--preset" => arguments.preset = Some(value("--preset")?),
            "--run-id" => arguments.run_id = Some(value("--run-id")?),
            "--port" => arguments.port = Some(number(&value("--port")?, "--port")?),
            "--limit" => arguments.limit = Some(number(&value("--limit")?, "--limit")?),
            "--run" => arguments.run = Some(value("--run")?),
            "--results" => arguments.results = Some(value("--results")?.into()),
            "--note" => arguments.note = Some(value("--note")?),
            "--at" => arguments.at = Some(value("--at")?),
            "--days" => arguments.days = Some(value("--days")?),
            "--title" => arguments.title = Some(value("--title")?),
            "--schedule" => arguments.schedule = Some(value("--schedule")?),
            "--each-build" => arguments.each_build = true,
            "--send" => arguments.send = true,
            "--hold" => arguments.hold = true,
            "--not-a-bug" => arguments.not_a_bug = true,
            "--reopen" => arguments.reopen = true,
            "--all" => arguments.all = true,
            "--json" => arguments.json = true,
            "--detach" => arguments.detach = true,
            "--wait" => arguments.wait = true,
            "--with-person" => arguments.with_person = true,
            "--failed" => arguments.failed = true,
            "--no-open" => arguments.no_open = true,
            "--help" | "-h" => arguments.command = Some("help".into()),
            "--version" | "-V" => arguments.command = Some("version".into()),
            flag if flag.starts_with('-') => {
                return Err(Error::Usage(format!("unknown option {flag}; see `dukovo-lab help`")));
            }
            _ if arguments.command.is_none() => arguments.command = Some(arg),
            _ => arguments.names.push(arg),
        }
    }
    Ok(arguments)
}

/// An option's value read as a number.
fn number<T: std::str::FromStr>(text: &str, name: &str) -> Result<T, Error> {
    text.parse().map_err(|_| Error::Usage(format!("{name} takes a number, not \"{text}\"")))
}

/// Runs the command; gives the exit status.
fn dispatch(arguments: &Arguments) -> Result<u8, Error> {
    match arguments.command.as_deref() {
        None | Some("help") => {
            print!("{HELP}");
            Ok(0)
        }
        Some("version") => {
            println!("dukovo-lab {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        Some("list") => list(arguments),
        Some("show") => show(arguments),
        Some("machines") => list_machines(arguments),
        Some("run") => run_tests(arguments),
        Some("status") => show_status(arguments),
        Some("stop") => stop_run(arguments),
        Some("done") => say_done(arguments),
        Some("results") => show_results(arguments),
        Some("runs") => list_runs(arguments),
        Some("findings") => show_findings(arguments),
        Some("send") => send_findings(arguments),
        Some("schedule") => schedule::command(arguments),
        Some("ui") => ui::serve(arguments),
        Some("key") => make_key(arguments),
        Some(other) => Err(Error::Usage(format!("no command \"{other}\"; see `dukovo-lab help`"))),
    }
}

/// A folder under the home folder, for the defaults.
fn home(relative: &str) -> Result<PathBuf, Error> {
    std::env::var_os("HOME").map(|home| Path::new(&home).join(relative)).ok_or_else(|| {
        Error::Usage("HOME is not set; give the folders with --config and --store".into())
    })
}

impl Arguments {
    fn tests_dir(&self) -> PathBuf {
        self.tests.clone().unwrap_or_else(|| "lab/tests".into())
    }

    fn config_dir(&self) -> Result<PathBuf, Error> {
        self.config.clone().map_or_else(|| home(".config/dukovo-lab"), Ok)
    }

    fn store_dir(&self) -> Result<PathBuf, Error> {
        self.store.clone().map_or_else(|| home(".local/share/dukovo-lab"), Ok)
    }

    fn load_tests(&self) -> Result<Vec<Test>, Error> {
        suite::load(&self.tests_dir())
    }

    fn load_machines(&self) -> Result<Vec<Machine>, Error> {
        machines::load(&self.config_dir()?.join("machines"))
    }

    fn load_presets(&self, tests: &[Test]) -> Result<Vec<Preset>, Error> {
        suite::load_presets(&self.tests_dir(), tests)
    }

    /// The one run a command names, if it names one.
    fn run_name(&self) -> Result<Option<&str>, Error> {
        match self.names.as_slice() {
            [] => Ok(None),
            [id] => Ok(Some(id)),
            _ => Err(Error::Usage("name one run".into())),
        }
    }
}

/// How a test fits each machine, as JSON.
fn fits_json(test: &Test, machines: &[&Machine]) -> Value {
    let fits: Vec<Value> = machines
        .iter()
        .map(|machine| {
            let (fit, controls) = match test.fit(machine) {
                Fit::Runs => ("runs", Vec::new()),
                Fit::NeedsPerson(controls) => ("needs-person", controls),
                Fit::Lacks(controls) => ("lacks", controls),
            };
            json!({ "machine": machine.name, "fit": fit, "controls": controls })
        })
        .collect();
    Value::Array(fits)
}

fn test_json(test: &Test, machines: &[&Machine]) -> Value {
    let steps: Vec<Value> = test
        .steps
        .iter()
        .map(|step| json!({ "step": step.describe(), "control": step.control() }))
        .collect();
    json!({
        "id": test.id,
        "category": test.category,
        "about": test.about,
        "time": units::format_duration(test.time),
        "fresh": test.fresh,
        "boot": match test.boot {
            suite::Boot::Image => "image",
            suite::Boot::Stick { .. } => "stick",
            suite::Boot::Dvd { .. } => "dvd",
        },
        "file": test.file,
        "controls": test.controls(),
        "machines": fits_json(test, machines),
        "steps": steps,
    })
}

/// `list`: the tests by category.
fn list(arguments: &Arguments) -> Result<u8, Error> {
    let tests = arguments.load_tests()?;
    let all_machines = arguments.load_machines()?;
    let machines: Vec<&Machine> = match &arguments.machine {
        Some(names) => machines::select(&all_machines, names)?,
        None => all_machines.iter().collect(),
    };
    let picked = if arguments.names.is_empty() {
        tests.iter().collect()
    } else {
        suite::select(&tests, &arguments.names)?
    };
    let presets = arguments.load_presets(&tests)?;
    let categories = suite::load_categories(&arguments.tests_dir())?;
    if arguments.json {
        // How long each test took last time, by machine, for time estimates.
        let typical = arguments
            .store_dir()
            .map(|store| status::typical_durations(&store, None))
            .unwrap_or_default();
        let tests: Vec<Value> = picked
            .iter()
            .map(|test| {
                let mut value = test_json(test, &machines);
                let last: serde_json::Map<String, Value> = machines
                    .iter()
                    .filter_map(|machine| {
                        let key = (machine.name.clone(), test.id.clone());
                        typical.get(&key).map(|&ms| (machine.name.clone(), json!(ms)))
                    })
                    .collect();
                value["last_milliseconds"] = Value::Object(last);
                value
            })
            .collect();
        println!("{}", json!({ "tests": tests, "categories": categories, "presets": presets }));
        return Ok(0);
    }
    for category in &categories {
        let in_category: Vec<&&Test> =
            picked.iter().filter(|test| test.category == category.id).collect();
        if in_category.is_empty() {
            continue;
        }
        println!("{}", category.title);
        for test in in_category {
            println!("  {:<32} {:>4}  {}", test.id, units::format_duration(test.time), test.about);
            for machine in &machines {
                if let Some(reason) = test.fit(machine).skip_reason() {
                    println!("  {:<32}       on {}: {reason}", "", machine.name);
                }
            }
        }
    }
    if !presets.is_empty() {
        println!("Presets (run --preset <name>)");
        for preset in &presets {
            println!("  {:<32} {} test(s)  {}", preset.name, preset.tests.len(), preset.title);
        }
    }
    Ok(0)
}

/// `show`: one test in full.
fn show(arguments: &Arguments) -> Result<u8, Error> {
    let [name] = arguments.names.as_slice() else {
        return Err(Error::Usage(
            "show takes one test, such as `dukovo-lab show boot/reaches-ready`".into(),
        ));
    };
    let tests = arguments.load_tests()?;
    let test = tests.iter().find(|test| test.id == *name).ok_or_else(|| {
        Error::Usage(format!("no test named \"{name}\"; `dukovo-lab list` lists them"))
    })?;
    let all_machines = arguments.load_machines()?;
    let machines: Vec<&Machine> = all_machines.iter().collect();
    if arguments.json {
        println!("{}", test_json(test, &machines));
        return Ok(0);
    }
    println!("{}: {}", test.id, test.about);
    println!("File: {}", test.file.display());
    // An attack test's boot has the private network, a card test's the
    // card reader (USB or SD), a disc test's the optical drive, a floppy
    // test's the floppy drive and a CompactFlash test's the card, that no
    // other test may find: never shared.
    let controls = test.controls();
    let alone = [
        machines::Control::Attack,
        machines::Control::Card,
        machines::Control::Sd,
        machines::Control::Disc,
        machines::Control::Burner,
        machines::Control::Floppy,
        machines::Control::Cf,
    ]
    .iter()
    .any(|control| controls.contains(control));
    let boot = if test.fresh || alone {
        "a boot of its own"
    } else {
        "shares a boot with the test before when it can"
    };
    println!("Time limit: {}, boot included; {boot}", units::format_duration(test.time));
    let controls: Vec<&str> = test.controls().iter().map(|control| control.name()).collect();
    println!("Controls: {}", controls.join(", "));
    println!("Steps:");
    for (index, step) in test.steps.iter().enumerate() {
        println!("  {}. {}", index + 1, step.describe());
    }
    println!("Machines:");
    for machine in machines {
        let fit = test.fit(machine).skip_reason().unwrap_or_else(|| "runs".into());
        println!("  {}: {fit}", machine.name);
    }
    Ok(0)
}

/// `machines`: each machine and its controls.
fn list_machines(arguments: &Arguments) -> Result<u8, Error> {
    match arguments.names.split_first() {
        Some((first, names)) if first == "check" => return check_machines(arguments, names),
        Some((other, _)) => {
            return Err(Error::Usage(format!(
                "no machines command \"{other}\"; see `dukovo-lab help`"
            )));
        }
        None => {}
    }
    let machines = arguments.load_machines()?;
    if arguments.json {
        println!("{}", json!({ "machines": machines }));
        return Ok(0);
    }
    for machine in &machines {
        let file =
            machine.file.as_ref().map_or("built in".into(), |file| file.display().to_string());
        println!("{}: {} ({file})", machine.name, machine.about);
        for who in [machines::Who::Driver, machines::Who::Person, machines::Who::None] {
            let controls: Vec<&str> = machine
                .controls
                .iter()
                .filter(|(_, w)| **w == who)
                .map(|(c, _)| c.name())
                .collect();
            if !controls.is_empty() {
                let label = match who {
                    machines::Who::Driver => "by its driver",
                    machines::Who::Person => "by a person",
                    machines::Who::None => "cannot",
                };
                println!("  {label}: {}", controls.join(", "));
            }
        }
    }
    Ok(0)
}

/// `machines check`: what a run on each real machine (or those named)
/// needs, checked without changing anything.
fn check_machines(arguments: &Arguments, names: &[String]) -> Result<u8, Error> {
    let all_machines = arguments.load_machines()?;
    let machines: Vec<&Machine> = if names.is_empty() {
        all_machines.iter().collect()
    } else {
        machines::select(&all_machines, &names.join(","))?
    };
    let key = ssh::key_path(&arguments.config_dir()?);
    let mut results = Vec::new();
    for machine in &machines {
        let checks = match &machine.amt {
            Some(settings) => {
                let screen = [Control::Keys, Control::Mouse, Control::Screen]
                    .into_iter()
                    .any(|control| machine.who(control) == Who::Driver);
                driver::amt::check(settings, Some(&key), screen)
            }
            None => Vec::new(),
        };
        results.push((machine, checks));
    }
    let problems = results
        .iter()
        .flat_map(|(_, checks)| checks)
        .any(|check| check.verdict == driver::amt::Verdict::Problem);
    if arguments.json {
        let machines: Vec<Value> = results
            .iter()
            .map(|(machine, checks)| json!({ "name": machine.name, "checks": checks }))
            .collect();
        println!("{}", json!({ "machines": machines }));
    } else {
        for (machine, checks) in &results {
            if checks.is_empty() {
                println!("{}: nothing to check; QEMU runs on this PC", machine.name);
                continue;
            }
            println!("{}: {}", machine.name, machine.about);
            for check in checks {
                let verdict = match check.verdict {
                    driver::amt::Verdict::Ok => "ok",
                    driver::amt::Verdict::Note => "note",
                    driver::amt::Verdict::Problem => "PROBLEM",
                };
                println!("  {verdict:<8} {:<13} {}", check.check, check.detail);
            }
        }
    }
    Ok(if problems { 3 } else { 0 })
}

/// `run`: runs tests and writes a bundle.
fn run_tests(arguments: &Arguments) -> Result<u8, Error> {
    let tests = arguments.load_tests()?;
    let presets = arguments.load_presets(&tests)?;
    let picked = match (arguments.all, arguments.names.is_empty(), &arguments.preset) {
        (true, true, None) => tests.iter().collect(),
        (false, false, None) => suite::select(&tests, &arguments.names)?,
        (false, true, Some(name)) => {
            let preset = presets.iter().find(|preset| preset.name == *name).ok_or_else(|| {
                let known: Vec<&str> = presets.iter().map(|preset| preset.name.as_str()).collect();
                Error::Usage(format!(
                    "no preset named \"{name}\"; the presets are: {}",
                    if known.is_empty() { "none".to_string() } else { known.join(", ") }
                ))
            })?;
            suite::select(&tests, &preset.tests)?
        }
        (false, true, None) => {
            return Err(Error::Usage(
                "name the tests or categories to run, or give --all or --preset".into(),
            ));
        }
        _ => return Err(Error::Usage("give tests, --all or --preset: only one".into())),
    };
    let all_machines = arguments.load_machines()?;
    let machines = machines::select(&all_machines, arguments.machine.as_deref().unwrap_or("qemu"))?;
    let image = arguments.image.clone().ok_or_else(|| {
        Error::Usage("run needs --image, the image to test (`ob lab` gives it)".into())
    })?;
    if !image.is_file() {
        return Err(Error::Usage(format!("the image {} does not exist", image.display())));
    }
    let key = ssh::key_path(&arguments.config_dir()?);
    let key = key.is_file().then_some(key);
    if key.is_none() {
        refuse_commands_without_key(&picked, &machines, arguments.with_person)?;
    }
    let password =
        ssh::read_password(&ssh::password_path(&arguments.config_dir()?)).map_err(Error::Usage)?;
    if password.is_none() {
        refuse_password_without_one(&picked)?;
    }
    // Without a kit, performance tests skip, saying how to get one.
    let linux = match &arguments.linux {
        Some(dir) => Some(bench::Kit::read(dir).map_err(Error::Usage)?),
        None => None,
    };
    // Without fonts, steps that read the screen skip; without a stick,
    // tests that boot it skip; each says how to get one.
    let fonts = match &arguments.fonts {
        Some(dir) => Some(screen::Font::read_dir(dir).map_err(Error::Usage)?),
        None => None,
    };
    if let Some(stick) = &arguments.stick
        && !stick.is_file()
    {
        return Err(Error::Usage(format!("the install stick {} does not exist", stick.display())));
    }
    if let Some(dvd) = &arguments.dvd
        && !dvd.is_file()
    {
        return Err(Error::Usage(format!("the install DVD {} does not exist", dvd.display())));
    }
    if let Some(ob) = &arguments.ob
        && !ob.is_file()
    {
        return Err(Error::Usage(format!("Dukovo's ob {} does not exist", ob.display())));
    }
    let store = arguments.store_dir()?;
    if arguments.detach {
        return detach(arguments, &store, &machines);
    }
    if let Some(id) = &arguments.run_id
        && !store::valid_run_id(id)
    {
        return Err(Error::Usage(format!("--run-id {id} is not a run id")));
    }
    let request = run::Request {
        tests: picked,
        machines,
        image,
        build: arguments.build.clone(),
        store,
        key,
        password,
        with_person: arguments.with_person,
        linux,
        fonts,
        stick: arguments.stick.clone(),
        dvd: arguments.dvd.clone(),
        ob: arguments.ob.clone(),
        preset: arguments.preset.clone(),
        schedule: arguments.schedule.clone(),
        id: arguments.run_id.clone(),
    };
    let reporter = output::Reporter::new(arguments.json);
    let ended = run::run(&request, &reporter, &driver::start)?;
    Ok(match ended.summary {
        summary if summary.errors > 0 => 3,
        summary if summary.failed > 0 => 1,
        _ => 0,
    })
}

/// Stops a run before it starts when a test that would run has to run
/// commands and the lab has no key to run them with.
fn refuse_commands_without_key(
    tests: &[&Test],
    machines: &[&Machine],
    with_person: bool,
) -> Result<(), Error> {
    let needs_key = tests.iter().find(|test| {
        test.steps.iter().any(|step| matches!(step, Step::Run(_) | Step::Bench(_)))
            && machines.iter().any(|machine| match test.fit(machine) {
                Fit::Runs => true,
                Fit::NeedsPerson(_) => with_person,
                Fit::Lacks(_) => false,
            })
    });
    match needs_key {
        Some(test) => Err(Error::Usage(format!(
            "{} runs commands, and the lab has no key: run `dukovo-lab key`, then build the image again with `ob image --lab`",
            test.id
        ))),
        None => Ok(()),
    }
}

/// Stops a run before it starts when a test would run a command as root,
/// or type the lab password, and the lab has none.
fn refuse_password_without_one(tests: &[&Test]) -> Result<(), Error> {
    match tests.iter().find(|test| test.uses_password()) {
        Some(test) => Err(Error::Usage(format!("{}: {}", test.id, run::NO_PASSWORD))),
        None => Ok(()),
    }
}

/// How long a detached run may take to start before `run --detach`
/// returns anyway.
const DETACH_WAIT: Duration = Duration::from_secs(15);

/// `run --detach`: starts this same run in a process of its own, which
/// carries on after this one returns and after the terminal closes, and
/// says how to follow it. Everything is checked before it starts, so a bad
/// argument still exits 2 here.
fn detach(arguments: &Arguments, store: &Path, machines: &[&Machine]) -> Result<u8, Error> {
    for machine in machines {
        if let Some(user) = store::machine_user(store, &machine.name) {
            return Err(Error::Busy(format!(
                "the {} machine is busy with run {user}; wait for it (`dukovo-lab status --wait {user}`) or stop it (`dukovo-lab stop {user}`)",
                machine.name
            )));
        }
    }
    let id = bundle::new_run_id(clock::Utc::now());
    let output = store::open_output_log(store, &id)?;
    let errors = output.try_clone().map_err(|err| Error::Harness(err.to_string()))?;
    // The same run, named, in plain words: what it says goes to its
    // output.log, where `status` finds it if the run dies, and where this
    // process reads why if it fails to start.
    let mut args: Vec<OsString> =
        std::env::args_os().skip(1).filter(|arg| arg != "--detach" && arg != "--json").collect();
    args.extend(["--run-id".into(), id.clone().into()]);
    let program = std::env::current_exe()
        .map_err(|err| Error::Harness(format!("finding this program to start the run: {err}")))?;
    let mut child = {
        use std::os::unix::process::CommandExt as _;
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(output)
            .stderr(errors)
            // A process group of its own, so the terminal's Ctrl+C and
            // hang-up do not reach it.
            .process_group(0)
            .spawn()
            .map_err(|err| Error::Harness(format!("starting the run: {err}")))?
    };
    // The run writes run.json only once it holds its machines, so run.json
    // means it got going, even if it has ended already (every test
    // skipped). Its lock is not looked at: it may be gone by now.
    let bundle = store::bundle_dir(store, &id);
    let started = Instant::now();
    while !bundle.join("run.json").is_file() {
        if let Ok(Some(status)) = child.try_wait() {
            if bundle.join("run.json").is_file() {
                break;
            }
            // It ended before it got going: say why, as it would have.
            let said = std::fs::read_to_string(store::output_log(store, &id)).unwrap_or_default();
            let _ = std::fs::remove_dir_all(store.join("running").join(&id));
            let said = said.trim().trim_start_matches("dukovo-lab: ").to_string();
            let said =
                if said.is_empty() { format!("the run ended at once ({status})") } else { said };
            return Err(match status.code() {
                Some(2) => Error::Usage(said),
                Some(4) => Error::Busy(said),
                _ => Error::Harness(said),
            });
        }
        if started.elapsed() > DETACH_WAIT {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if arguments.json {
        println!(
            "{}",
            json!({ "event": "run-detached", "id": id, "bundle": bundle, "pid": child.id() })
        );
    } else {
        println!("Started run {id} in the background.");
        println!("Follow it: dukovo-lab status --wait {id}");
        println!("Stop it:   dukovo-lab stop {id}");
        println!("Bundle: {}", bundle.display());
    }
    Ok(0)
}

/// The exit status that tells how a run went, as `run` would have exited.
fn run_status(state: State, counts: status::Counts) -> u8 {
    match state {
        State::Running => 0,
        State::Died => 3,
        State::Ended | State::Stopped if counts.errors > 0 => 3,
        State::Ended | State::Stopped if counts.failed > 0 => 1,
        State::Ended | State::Stopped => 0,
    }
}

/// `status`: how a run is going.
fn show_status(arguments: &Arguments) -> Result<u8, Error> {
    let store = arguments.store_dir()?;
    let id = store::pick_run(&store, arguments.run_name()?)?;
    let mut status = status::status(&store, &id)?;
    while arguments.wait && status.state == State::Running {
        std::thread::sleep(Duration::from_secs(1));
        status = status::status(&store, &id)?;
    }
    if arguments.json {
        println!("{}", serde_json::to_string(&status).expect("statuses always serialize"));
    } else {
        print_status(&status);
    }
    Ok(run_status(status.state, status.counts))
}

fn print_status(status: &status::Status) {
    let build = status.build.as_deref().map(|build| format!(", build {build}")).unwrap_or_default();
    println!("Run {}: {}{build}", status.id, status.state.word());
    let counts = &status.counts;
    let mut line = format!(
        "  {} passed, {} failed, {} skipped, {} errors",
        counts.passed, counts.failed, counts.skipped, counts.errors
    );
    if status.state == State::Running {
        line.push_str(&format!(", {} running, {} queued", counts.running, counts.queued));
        if let Some(left) = status.left_seconds {
            line.push_str(&format!("; about {} left", output::seconds(left * 1000)));
        }
    }
    println!("{line}");
    for machine in &status.machines {
        let doing =
            machine.running.as_deref().map(|test| format!("running {test}; ")).unwrap_or_default();
        println!(
            "  {}: {doing}{} passed, {} failed",
            machine.name, machine.counts.passed, machine.counts.failed
        );
    }
    for wait in &status.waiting {
        println!(
            "  NEEDS YOU on {}: {} ({}); once done, run `{}`",
            wait.machine, wait.step, wait.test, wait.done
        );
    }
    if let Some(output) = &status.output {
        println!("  Its last messages:\n{}", output.trim_end());
    }
    println!("  Bundle: {}", status.bundle.display());
}

/// The run a command acts on, which must be going: the one named, else
/// the newest going.
fn going_run(store: &Path, given: Option<&str>) -> Result<String, Error> {
    let id = match given {
        Some(_) => store::pick_run(store, given)?,
        None => store::running_ids(store)
            .into_iter()
            .next()
            .ok_or_else(|| Error::Usage("no run is going".into()))?,
    };
    if !store::is_running(store, &id) {
        return Err(Error::Usage(format!(
            "run {id} is not going; `dukovo-lab status {id}` says how it ended"
        )));
    }
    Ok(id)
}

/// How long `stop` waits for a run to wind down: its machines to power
/// off and its bundle to be written.
const STOP_WAIT: Duration = Duration::from_secs(120);

/// `stop`: asks a run to stop and waits until it has.
fn stop_run(arguments: &Arguments) -> Result<u8, Error> {
    let store = arguments.store_dir()?;
    let id = going_run(&store, arguments.run_name()?)?;
    // A run that ended just now took its folder with it: nothing to stop.
    if let Err(err) = store::request_stop(&store, &id)
        && store::is_running(&store, &id)
    {
        return Err(err);
    }
    let asked = Instant::now();
    while store::is_running(&store, &id) {
        if asked.elapsed() > STOP_WAIT {
            return Err(Error::Harness(format!(
                "run {id} was asked to stop but is still going after {}s",
                STOP_WAIT.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let status = status::status(&store, &id)?;
    if arguments.json {
        println!("{}", serde_json::to_string(&status).expect("statuses always serialize"));
    } else {
        print_status(&status);
    }
    Ok(0)
}

/// `done`: a person did the step a run waits for on a machine.
fn say_done(arguments: &Arguments) -> Result<u8, Error> {
    let store = arguments.store_dir()?;
    let id = going_run(&store, arguments.run_name()?)?;
    let machine = arguments.machine.as_deref().ok_or_else(|| {
        Error::Usage("done needs --machine, the machine where the step was done".into())
    })?;
    let status = status::status(&store, &id)?;
    if !status.waiting.iter().any(|wait| wait.machine == machine) {
        return Err(Error::Usage(format!("run {id} is not waiting for a person on {machine}")));
    }
    store::say_done(&store, &id, machine)?;
    if arguments.json {
        println!("{}", json!({ "done": true, "run": id, "machine": machine }));
    } else {
        println!("Told run {id} the step on {machine} is done.");
    }
    Ok(0)
}

/// `results`: a run's results.
fn show_results(arguments: &Arguments) -> Result<u8, Error> {
    let store = arguments.store_dir()?;
    let id = store::pick_run(&store, arguments.run_name()?)?;
    let mut results = status::results(&store, &id)?;
    let mut counts = status::Counts::default();
    for result in &results.results {
        match result.result {
            bundle::Outcome::Pass => counts.passed += 1,
            bundle::Outcome::Fail => counts.failed += 1,
            bundle::Outcome::Skip => counts.skipped += 1,
            bundle::Outcome::Error => counts.errors += 1,
        }
    }
    if arguments.failed {
        results.results.retain(|result| {
            matches!(result.result, bundle::Outcome::Fail | bundle::Outcome::Error)
        });
    }
    if arguments.json {
        println!("{}", serde_json::to_string(&results).expect("results always serialize"));
        return Ok(run_status(results.state, counts));
    }
    let build =
        results.run.build.as_deref().map(|build| format!(", build {build}")).unwrap_or_default();
    println!("Run {}: {}{build}", results.run.id, results.state.word());
    for result in &results.results {
        let word = match result.result {
            bundle::Outcome::Pass => "passed",
            bundle::Outcome::Fail => "FAILED",
            bundle::Outcome::Skip => "skipped",
            bundle::Outcome::Error => "ERROR",
        };
        let reason = result.reason.as_deref().map(|why| format!(": {why}")).unwrap_or_default();
        let first_line = reason.lines().next().unwrap_or_default();
        println!(
            "  [{}] {}: {word} in {}{first_line}",
            result.machine,
            result.test,
            output::seconds(result.milliseconds)
        );
        if matches!(result.result, bundle::Outcome::Fail | bundle::Outcome::Error)
            && let Some(log) = &result.log
        {
            println!("      log: {}", results.bundle.join(log).display());
        }
    }
    Ok(run_status(results.state, counts))
}

/// `runs`: the runs so far.
fn list_runs(arguments: &Arguments) -> Result<u8, Error> {
    let store = arguments.store_dir()?;
    let runs = status::runs(&store, arguments.limit.unwrap_or(20));
    if arguments.json {
        println!("{}", json!({ "runs": runs }));
        return Ok(0);
    }
    for run in &runs {
        let counts = run
            .summary
            .map(|s| format!("{} passed, {} failed", s.passed, s.failed))
            .unwrap_or_default();
        println!(
            "{}  {:<8} {:<22} {:<16} {}",
            run.id,
            run.state.word(),
            counts,
            run.machines.join(","),
            run.build.as_deref().unwrap_or("")
        );
    }
    Ok(0)
}

/// `findings`: the findings, one finding, or a change to one.
fn show_findings(arguments: &Arguments) -> Result<u8, Error> {
    let store = arguments.store_dir()?;
    if arguments.not_a_bug && arguments.reopen {
        return Err(Error::Usage("give --not-a-bug or --reopen, not both".into()));
    }
    if arguments.note.is_some() && !arguments.not_a_bug {
        return Err(Error::Usage("--note goes with --not-a-bug".into()));
    }
    match arguments.names.as_slice() {
        [] if arguments.not_a_bug || arguments.reopen => Err(Error::Usage(
            "say which finding: `dukovo-lab findings <finding> --not-a-bug`".into(),
        )),
        [] => list_findings(arguments, &store),
        [id] => {
            let finding = if arguments.not_a_bug {
                findings::set_not_a_bug(&store, id, Some(arguments.note.clone()))?
            } else if arguments.reopen {
                findings::set_not_a_bug(&store, id, None)?
            } else {
                findings::find(&store, id)?
            };
            if arguments.json {
                println!("{}", serde_json::to_string(&finding).expect("findings always serialize"));
            } else {
                print_finding(&finding, &store);
            }
            Ok(0)
        }
        _ => Err(Error::Usage("findings takes one finding at a time".into())),
    }
}

/// `findings` with no finding named: the open ones, a run's, or every one.
fn list_findings(arguments: &Arguments, store: &Path) -> Result<u8, Error> {
    let list = match &arguments.run {
        Some(run) => findings::of_run(store, &store::pick_run(store, Some(run))?),
        None if arguments.all => findings::load_all(store),
        None => findings::load_all(store)
            .into_iter()
            .filter(|finding| {
                !matches!(finding.state, findings::State::NotABug | findings::State::Fixed)
            })
            .collect(),
    };
    if arguments.json {
        println!("{}", json!({ "findings": list }));
        return Ok(0);
    }
    if list.is_empty() {
        println!("No findings{}.", if arguments.run.is_some() { " in that run" } else { "" });
    }
    for finding in &list {
        let unsent = if finding.unsent() && finding.state != findings::State::NotABug {
            ", not sent"
        } else {
            ""
        };
        println!(
            "{}  {:<12} {}x{unsent}  {}: {}",
            finding.id,
            finding.state.word(),
            finding.count,
            finding.test,
            finding.signature
        );
    }
    Ok(0)
}

fn print_finding(finding: &findings::Finding, store: &Path) {
    println!("{} {}: {}", finding.id, finding.test, finding.signature);
    let note = finding.note.as_deref().map(|note| format!(" ({note})")).unwrap_or_default();
    println!("  state: {}{note}", finding.state.word());
    println!(
        "  failed {} time{} on {}, first {}, last {}",
        finding.count,
        if finding.count == 1 { "" } else { "s" },
        finding.machines.join(", "),
        finding.first_seen,
        finding.last_seen
    );
    if !finding.builds.is_empty() {
        println!("  builds: {}", finding.builds.join(", "));
    }
    for (machine, passing) in &finding.passing {
        println!("  passed again on {machine} in run {} ({})", passing.run, passing.at);
    }
    for seen in finding.runs.iter().rev() {
        let sent = if seen.sent { ", sent" } else { "" };
        println!(
            "  run {} on {}{sent}: {}",
            seen.run,
            seen.machine,
            store::bundle_dir(store, &seen.run).display()
        );
    }
    if let Some(last) = finding.runs.last() {
        println!("  last time it said:");
        for line in last.reason.lines() {
            println!("    {line}");
        }
    }
}

/// `send`: findings and runs to lab-results.
fn send_findings(arguments: &Arguments) -> Result<u8, Error> {
    let store = arguments.store_dir()?;
    let mut finding_ids = arguments.names.clone();
    let mut run_ids = Vec::new();
    if arguments.all {
        // Every finding with something lab-results lacks, and each one it
        // already holds that a person here has since said is not a bug
        // (sending it again changes nothing when nothing changed).
        finding_ids.extend(
            findings::load_all(&store)
                .into_iter()
                .filter(|finding| match finding.state {
                    findings::State::NotABug => finding.sent.is_some(),
                    _ => finding.unsent(),
                })
                .map(|finding| finding.id),
        );
    }
    if let Some(run) = &arguments.run {
        let run = store::pick_run(&store, Some(run))?;
        finding_ids.extend(
            findings::of_run(&store, &run)
                .into_iter()
                .filter(|finding| finding.state != findings::State::NotABug)
                .map(|finding| finding.id),
        );
        run_ids.push(run);
    }
    if finding_ids.is_empty() && run_ids.is_empty() {
        if arguments.all {
            if arguments.json {
                println!("{}", json!({ "commit": null, "runs": [], "findings": [] }));
            } else {
                println!("Nothing to send: lab-results has every finding.");
            }
            return Ok(0);
        }
        return Err(Error::Usage(
            "say what to send: finding ids, --all or --run <run>; see `dukovo-lab help`".into(),
        ));
    }
    let clone = send::clone_dir(&store, arguments.results.as_deref());
    let sent = send::send(&store, &clone, &finding_ids, &run_ids)?;
    if arguments.json {
        println!("{}", serde_json::to_string(&sent).expect("sends always serialize"));
    } else if let Some(commit) = &sent.commit {
        println!(
            "Sent {} finding{} and {} run{} to lab-results (commit {}).",
            sent.findings.len(),
            if sent.findings.len() == 1 { "" } else { "s" },
            sent.runs.len(),
            if sent.runs.len() == 1 { "" } else { "s" },
            &commit[..commit.len().min(12)]
        );
    } else {
        println!("lab-results already had all of that; nothing new went.");
    }
    Ok(0)
}

/// `key`: makes the lab key and the lab password, each if there is none.
/// Only their paths are printed: the password and the private key never
/// leave their files.
fn make_key(arguments: &Arguments) -> Result<u8, Error> {
    let config = arguments.config_dir()?;
    let path = ssh::key_path(&config);
    let made = ssh::make_key(&path).map_err(Error::Harness)?;
    let password = ssh::password_path(&config);
    let made_password = ssh::make_password(&password).map_err(Error::Harness)?;
    let public = path.with_file_name(format!("{}.pub", ssh::KEY_FILE));
    if arguments.json {
        println!(
            "{}",
            json!({
                "public_key": public,
                "made": made,
                "password_file": password,
                "made_password": made_password,
            })
        );
        return Ok(0);
    }
    if made {
        println!("Made the lab key. Its public key, for `ob image --lab`, is {}", public.display());
    } else {
        println!("The lab key is already made. Its public key is {}", public.display());
    }
    if made_password {
        println!(
            "Made the lab password, for `ob image --lab` and tests that run as root, in {}",
            password.display()
        );
    }
    if made || made_password {
        println!("Build the lab image again so it holds them: `ob lab` does.");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_line(line: &str) -> Result<Arguments, Error> {
        parse(line.split_whitespace().map(String::from))
    }

    #[test]
    fn options_go_anywhere() {
        let arguments =
            parse_line("--json run boot --machine qemu,optiplex drivers/link-pull --image x.img")
                .unwrap();
        assert_eq!(arguments.command.as_deref(), Some("run"));
        assert_eq!(arguments.names, ["boot", "drivers/link-pull"]);
        assert_eq!(arguments.machine.as_deref(), Some("qemu,optiplex"));
        assert!(arguments.json);
        assert!(matches!(parse_line("run --machine"), Err(Error::Usage(_))));
        assert!(matches!(parse_line("run --fast"), Err(Error::Usage(_))));
    }
}

// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `dukovo-lab`, the Dukovo Lab harness: runs Dukovo's lab tests on QEMU
//! and on real machines, and writes what happened as a results bundle.
//!
//! The commands are thin: [`suite`] reads test files, [`machines`] reads
//! machine files, [`run`] runs tests on machines through their
//! [`driver`]s, and [`bundle`] writes what came out. Every command can
//! answer in JSON (`--json`), never prompts, and exits with a documented
//! status (see [`HELP`]), so scripts and agents drive it as a person does.
//! docs/ holds the manual.

mod bundle;
mod clock;
mod driver;
mod machines;
mod output;
mod qmp;
mod run;
mod ssh;
mod suite;
mod units;

#[cfg(test)]
mod testing;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Value, json};

use machines::Machine;
use suite::{Fit, Step, Test};

const HELP: &str = "\
dukovo-lab: run Dukovo's lab tests on QEMU and real machines

Usage: dukovo-lab [options] <command> [arguments]

Commands:
  list [<test or category>...]
                   The tests by category, and how each fits each machine
  show <test>      What a test checks, its steps, its time limit, its file
  machines         The machines and who does each of their controls
  run <test or category>... | --all
                   Runs tests on one or more machines at once, and writes
                   a results bundle
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
  --machine <a,b>  The machines to run on (default: qemu)
  --build <text>   The build's name for the bundle, such as \"0.13.0 fd2a9e8\"
  --all            Every test

Exit status:
  0  every test that ran passed (tests may be skipped)
  1  a test failed
  2  a bad argument, test file or machine file
  3  the harness or a machine broke, so some results say nothing about Dukovo
";

/// Why a command did not finish.
#[derive(Debug)]
pub enum Error {
    /// A bad argument or file: exit status 2.
    Usage(String),
    /// The harness or a machine broke: exit status 3.
    Harness(String),
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
    machine: Option<String>,
    build: Option<String>,
    all: bool,
    json: bool,
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
            "--machine" => arguments.machine = Some(value("--machine")?),
            "--build" => arguments.build = Some(value("--build")?),
            "--all" => arguments.all = true,
            "--json" => arguments.json = true,
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
    json!({
        "id": test.id,
        "category": test.category,
        "about": test.about,
        "time": units::format_duration(test.time),
        "fresh": test.fresh,
        "file": test.file,
        "controls": test.controls(),
        "machines": fits_json(test, machines),
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
    if arguments.json {
        let tests: Vec<Value> = picked.iter().map(|test| test_json(test, &machines)).collect();
        println!("{}", json!({ "tests": tests }));
        return Ok(0);
    }
    for &(category, title) in suite::CATEGORIES {
        let in_category: Vec<&&Test> =
            picked.iter().filter(|test| test.category == category).collect();
        if in_category.is_empty() {
            continue;
        }
        println!("{title}");
        for test in in_category {
            println!("  {:<32} {:>4}  {}", test.id, units::format_duration(test.time), test.about);
            for machine in &machines {
                if let Some(reason) = test.fit(machine).skip_reason() {
                    println!("  {:<32}       on {}: {reason}", "", machine.name);
                }
            }
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
        let mut value = test_json(test, &machines);
        let steps: Vec<Value> = test
            .steps
            .iter()
            .map(|step| json!({ "step": step.describe(), "control": step.control() }))
            .collect();
        value["steps"] = Value::Array(steps);
        println!("{value}");
        return Ok(0);
    }
    println!("{}: {}", test.id, test.about);
    println!("File: {}", test.file.display());
    let boot = if test.fresh {
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

/// `run`: runs tests and writes a bundle.
fn run_tests(arguments: &Arguments) -> Result<u8, Error> {
    let tests = arguments.load_tests()?;
    let picked = match (arguments.all, arguments.names.is_empty()) {
        (true, true) => tests.iter().collect(),
        (false, false) => suite::select(&tests, &arguments.names)?,
        (true, false) => return Err(Error::Usage("give tests or --all, not both".into())),
        (false, true) => {
            return Err(Error::Usage("name the tests or categories to run, or give --all".into()));
        }
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
        refuse_commands_without_key(&picked, &machines)?;
    }
    let request = run::Request {
        tests: picked,
        machines,
        image,
        build: arguments.build.clone(),
        store: arguments.store_dir()?,
        key,
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
fn refuse_commands_without_key(tests: &[&Test], machines: &[&Machine]) -> Result<(), Error> {
    let needs_key = tests.iter().find(|test| {
        test.steps.iter().any(|step| matches!(step, Step::Run(_)))
            && machines.iter().any(|machine| test.fit(machine) == Fit::Runs)
    });
    match needs_key {
        Some(test) => Err(Error::Usage(format!(
            "{} runs commands, and the lab has no key: run `dukovo-lab key`, then build the image again with `ob image --lab`",
            test.id
        ))),
        None => Ok(()),
    }
}

/// `key`: makes the lab key if there is none.
fn make_key(arguments: &Arguments) -> Result<u8, Error> {
    let path = ssh::key_path(&arguments.config_dir()?);
    let made = ssh::make_key(&path).map_err(Error::Harness)?;
    let public = path.with_file_name(format!("{}.pub", ssh::KEY_FILE));
    if arguments.json {
        println!("{}", json!({ "public_key": public, "made": made }));
    } else if made {
        println!("Made the lab key. Its public key, for `ob image --lab`, is {}", public.display());
    } else {
        println!("The lab key is already made. Its public key is {}", public.display());
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

// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Running tests: the picked tests on each picked machine, the machines
//! side by side.
//!
//! Each machine gets a worker thread that runs the tests in order. A test
//! with `fresh = true` gets a boot of its own; the others share the boot
//! the last test left, which keeps a full run short. A boot is shared only
//! after a test passed, because a failed test leaves the machine in a
//! state no later test should depend on.
//!
//! A test fails when a step's check fails or the test passes its time
//! limit (several times longer under QEMU's emulator, see
//! [`Driver::slowdown`]). It is an error, not a failure, when the harness
//! or the machine breaks. Each test's log in the bundle lists its steps
//! with what each did; start there when a result surprises you, then the
//! boot's serial log.
//!
//! A step whose control a person does on the machine (a Needs you step,
//! run only with `--with-person`) is not done by the driver: the run
//! reports a `person-needed` event and waits until someone runs
//! `dukovo-lab done` or presses Done on the runner page. A run can be
//! stopped part way (see stop.rs); what it did not finish is skipped.

use std::fs::{self, File};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::Error;
use crate::bundle::{
    Bundle, MachineRecord, Outcome, ResultRecord, RunRecord, SCHEMA, StepRecord, Summary,
    new_run_id,
};
use crate::clock::Utc;
use crate::driver::{Driver, Setup};
use crate::machines::{Control, Machine, Who};
use crate::output::{Event, Reporter, short_note};
use crate::stop::Stop;
use crate::store::Running;
use crate::suite::{Command, Fit, PowerAction, Step, Test};
use crate::units::format_duration;

/// What a stopped test's result says.
const STOPPED_DURING: &str = "the run was stopped during this test";
const STOPPED_BEFORE: &str = "the run was stopped before this test";

/// What to run.
pub struct Request<'a> {
    pub tests: Vec<&'a Test>,
    pub machines: Vec<&'a Machine>,
    /// The image under test.
    pub image: PathBuf,
    /// The build's name for run.json, as `ob lab` gives it.
    pub build: Option<String>,
    /// Where bundles and work files go.
    pub store: PathBuf,
    /// The lab key, when there is one.
    pub key: Option<PathBuf>,
    /// Whether to run Needs you tests, asking a person to do their steps.
    pub with_person: bool,
    /// The preset the tests came from, for run.json.
    pub preset: Option<String>,
    /// The run's id, when the caller chose it (a detached run's parent
    /// does, to name the run before it starts); else a new one.
    pub id: Option<String>,
}

/// How a run went.
pub struct Ended {
    pub summary: Summary,
}

/// What every machine's worker shares.
struct Shared<'a> {
    run_id: &'a str,
    bundle: &'a Bundle,
    reporter: &'a Reporter,
    running: &'a Running,
    stop: &'a Stop,
    with_person: bool,
}

/// Starts a machine's driver; [`crate::driver::start`] outside tests.
pub type StartDriver = dyn Fn(&Machine, Setup) -> Result<Box<dyn Driver>, String> + Sync;

/// Runs `request`, reporting as it goes, and writes the bundle.
pub fn run(
    request: &Request<'_>,
    reporter: &Reporter,
    start: &StartDriver,
) -> Result<Ended, Error> {
    let started = Utc::now();
    let id = request.id.clone().unwrap_or_else(|| new_run_id(started));
    // Before anything is written: a machine another run is using stops
    // this run here, with nothing left behind.
    let names: Vec<&str> = request.machines.iter().map(|machine| machine.name.as_str()).collect();
    let running = Running::start(&request.store, &id, &names)?;
    let record = RunRecord {
        schema: SCHEMA,
        id: id.clone(),
        harness: format!("dukovo-lab {}", env!("CARGO_PKG_VERSION")),
        started: started.rfc3339(),
        ended: None,
        build: request.build.clone(),
        image: request.image.display().to_string(),
        machines: request
            .machines
            .iter()
            .map(|machine| MachineRecord {
                name: machine.name.clone(),
                about: machine.about.clone(),
                driver: format!("{:?}", machine.driver).to_lowercase(),
            })
            .collect(),
        tests: request.tests.iter().map(|test| test.id.clone()).collect(),
        preset: request.preset.clone(),
        summary: None,
        stopped: false,
    };
    let mut bundle = Bundle::create(&request.store, record).map_err(Error::Harness)?;
    let events = bundle.dir().join("events.jsonl");
    let events = File::create(&events)
        .map_err(|err| Error::Harness(format!("making {}: {err}", events.display())))?;
    reporter.keep_in(events);
    reporter.report(&Event::RunStarted {
        id: &id,
        bundle: bundle.dir(),
        machines: request.machines.iter().map(|machine| machine.name.as_str()).collect(),
        tests: request.tests.iter().map(|test| test.id.as_str()).collect(),
    });

    let work = request.store.join("work").join(&id);
    let sockets = socket_folder(&id);
    let stop = Stop::default();
    let finished = AtomicBool::new(false);
    let shared = Shared {
        run_id: &id,
        bundle: &bundle,
        reporter,
        running: &running,
        stop: &stop,
        with_person: request.with_person,
    };
    let results: Vec<ResultRecord> = std::thread::scope(|scope| {
        // Watches for `dukovo-lab stop` while the machines work.
        scope.spawn(|| {
            while !finished.load(Ordering::Relaxed) {
                if running.stop_requested() {
                    stop.request();
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        let workers: Vec<_> = request
            .machines
            .iter()
            .enumerate()
            .map(|(index, &machine)| {
                let setup = Setup {
                    image: request.image.clone(),
                    logs: bundle.logs(&machine.name),
                    work: work.join(&machine.name),
                    // By number, not name: socket paths must stay short.
                    sockets: sockets.join(index.to_string()),
                    key: request.key.clone(),
                    stop: stop.clone(),
                };
                let shared = &shared;
                let tests = &request.tests;
                scope.spawn(move || run_machine(machine, tests, setup, shared, start))
            })
            .collect();
        let results = workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("a machine's worker panicked"))
            .collect();
        finished.store(true, Ordering::Relaxed);
        results
    });
    let _ = fs::remove_dir_all(&work);
    let _ = fs::remove_dir_all(&sockets);

    let summary = Summary::of(&results);
    let stopped = stop.requested();
    bundle.write_results(&results).map_err(Error::Harness)?;
    bundle.record.ended = Some(Utc::now().rfc3339());
    bundle.record.summary = Some(summary);
    bundle.record.stopped = stopped;
    bundle.write_run().map_err(Error::Harness)?;
    reporter.report(&Event::RunEnded { id: &id, bundle: bundle.dir(), summary, stopped });
    // The run's locks go only now, once run.json says it ended, so a
    // reader never sees a run that is neither going nor ended.
    drop(running);
    Ok(Ended { summary })
}

/// A short folder for the run's sockets: the user's runtime folder when
/// there is one, else the system's temporary folder.
fn socket_folder(id: &str) -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    base.join("dukovo-lab").join(id)
}

/// Runs every test on one machine, in order.
fn run_machine(
    machine: &Machine,
    tests: &[&Test],
    setup: Setup,
    shared: &Shared<'_>,
    start: &StartDriver,
) -> Vec<ResultRecord> {
    let has_key = setup.key.is_some();
    let (mut worker, broken) = match start(machine, setup) {
        Ok(driver) => (Some(Worker::new(machine, driver, has_key, shared)), None),
        Err(err) => (None, Some(format!("the {} machine did not start: {err}", machine.name))),
    };
    let mut results = Vec::new();
    for &test in tests {
        let skip = match test.fit(machine) {
            _ if shared.stop.requested() => Some(STOPPED_BEFORE.to_string()),
            Fit::NeedsPerson(_) if shared.with_person => None,
            fit => fit.skip_reason(),
        };
        let ran_here = skip.is_none() && worker.is_some();
        let result = match (skip, &mut worker) {
            (Some(reason), _) => skipped(test, machine, reason),
            (None, Some(worker)) => worker.run_test(test),
            (None, None) => errored(test, machine, broken.clone().unwrap_or_default()),
        };
        // A test that ran reported its own end.
        if !ran_here {
            shared.reporter.report(&Event::TestEnded {
                machine: &machine.name,
                test: &test.id,
                result: result.result,
                reason: result.reason.as_deref(),
                milliseconds: 0,
                boot: None,
                log: None,
                screenshots: &[],
            });
        }
        results.push(result);
    }
    if let Some(worker) = &mut worker {
        worker.driver.shut_off();
    }
    results
}

fn skipped(test: &Test, machine: &Machine, reason: String) -> ResultRecord {
    blank_result(test, machine, Outcome::Skip, reason)
}

fn errored(test: &Test, machine: &Machine, reason: String) -> ResultRecord {
    blank_result(test, machine, Outcome::Error, reason)
}

fn blank_result(test: &Test, machine: &Machine, result: Outcome, reason: String) -> ResultRecord {
    ResultRecord {
        test: test.id.clone(),
        machine: machine.name.clone(),
        result,
        reason: Some(reason),
        milliseconds: 0,
        boot: None,
        log: None,
        steps: Vec::new(),
        screenshots: Vec::new(),
    }
}

/// Why a step did not pass.
enum StepFailure {
    /// Its check failed: a finding about Dukovo.
    Fail(String),
    /// The harness or machine broke.
    Error(String),
    /// It could not go on, and that says nothing about Dukovo or the
    /// lab: the run was stopped, or no one did a Needs you step in time.
    Skip(String),
}

/// One machine's worker.
struct Worker<'a> {
    machine: &'a Machine,
    driver: Box<dyn Driver>,
    /// Whether the lab has a key, so the machine's SSH channel can work.
    has_key: bool,
    shared: &'a Shared<'a>,
    /// The machine's current boot, counted from 1; 0 before the first.
    boot: u32,
    /// Whether the last test passed and left the machine on, so the next
    /// may share its boot.
    reusable: bool,
    serial: SerialWatch,
    /// The test being run: its log lines and screenshots.
    log: Vec<String>,
    screenshots: Vec<String>,
}

impl<'a> Worker<'a> {
    fn new(
        machine: &'a Machine,
        driver: Box<dyn Driver>,
        has_key: bool,
        shared: &'a Shared<'a>,
    ) -> Self {
        Worker {
            machine,
            driver,
            has_key,
            shared,
            boot: 0,
            reusable: false,
            serial: SerialWatch::new(PathBuf::new()),
            log: Vec::new(),
            screenshots: Vec::new(),
        }
    }

    /// Runs one test, from its boot (if it needs one) to its last step.
    fn run_test(&mut self, test: &Test) -> ResultRecord {
        let machine = &self.machine.name;
        self.shared.reporter.report(&Event::TestStarted { machine, test: &test.id });
        let started = Instant::now();
        let limit = test.time * self.driver.slowdown();
        let deadline = started + limit;
        self.log = vec![format!("Test {} on {machine}: {}", test.id, test.about)];
        self.screenshots = Vec::new();
        let mut steps = Vec::new();

        let mut failure = None;
        if test.fresh || !self.reusable || !self.driver.is_on() {
            let (record, outcome) =
                self.timed(test, "boot afresh and wait until booted", |worker| {
                    worker.boot_fresh(deadline)
                });
            steps.push(record);
            failure = outcome.err();
        } else {
            self.log.push(format!("Sharing boot {} with the test before.", self.boot));
            self.serial.skip_to_end();
        }
        for (index, step) in test.steps.iter().enumerate() {
            if failure.is_some() {
                break;
            }
            if self.shared.stop.requested() {
                failure = Some(StepFailure::Skip(STOPPED_DURING.into()));
                break;
            }
            let (record, outcome) =
                self.timed(test, &step.describe(), |worker| worker.run_step(test, step, deadline));
            steps.push(record);
            failure = outcome.err().map(|failure| match failure {
                StepFailure::Fail(why) => StepFailure::Fail(format!("step {}: {why}", index + 1)),
                StepFailure::Error(why) => StepFailure::Error(format!("step {}: {why}", index + 1)),
                StepFailure::Skip(why) => StepFailure::Skip(format!("step {}: {why}", index + 1)),
            });
        }

        let (result, reason) = match failure {
            None => (Outcome::Pass, None),
            // Whatever the step said as it was cut short, the stop is why.
            Some(_) if self.shared.stop.requested() => {
                (Outcome::Skip, Some(STOPPED_DURING.to_string()))
            }
            Some(StepFailure::Fail(why)) => {
                (Outcome::Fail, Some(self.with_time_limit(why, deadline, test)))
            }
            Some(StepFailure::Error(why)) => (Outcome::Error, Some(why)),
            Some(StepFailure::Skip(why)) => (Outcome::Skip, Some(why)),
        };
        if matches!(result, Outcome::Fail | Outcome::Error) {
            self.screenshot_after_failure(test);
        }
        self.reusable = result == Outcome::Pass && self.driver.is_on();
        let log = self.write_log(test, result, reason.as_deref());
        let milliseconds = started.elapsed().as_millis() as u64;
        let boot = (self.boot > 0).then_some(self.boot);
        let screenshots = std::mem::take(&mut self.screenshots);
        self.shared.reporter.report(&Event::TestEnded {
            machine,
            test: &test.id,
            result,
            reason: reason.as_deref(),
            milliseconds,
            boot,
            log: log.as_deref(),
            screenshots: &screenshots,
        });
        ResultRecord {
            test: test.id.clone(),
            machine: machine.clone(),
            result,
            reason,
            milliseconds,
            boot,
            log,
            steps,
            screenshots,
        }
    }

    /// Runs `action` as a step of `test` named `what`, logging, timing
    /// and reporting it.
    fn timed(
        &mut self,
        test: &Test,
        what: &str,
        action: impl FnOnce(&mut Self) -> Result<Option<String>, StepFailure>,
    ) -> (StepRecord, Result<(), StepFailure>) {
        let started = Instant::now();
        let outcome = action(self);
        let milliseconds = started.elapsed().as_millis() as u64;
        let (result, note, outcome) = match outcome {
            Ok(note) => (Outcome::Pass, note, Ok(())),
            Err(StepFailure::Fail(why)) => {
                (Outcome::Fail, Some(why.clone()), Err(StepFailure::Fail(why)))
            }
            Err(StepFailure::Error(why)) => {
                (Outcome::Error, Some(why.clone()), Err(StepFailure::Error(why)))
            }
            Err(StepFailure::Skip(why)) => {
                (Outcome::Skip, Some(why.clone()), Err(StepFailure::Skip(why)))
            }
        };
        let word = match result {
            Outcome::Pass => "passed",
            Outcome::Fail => "FAILED",
            Outcome::Skip => "skipped",
            Outcome::Error => "ERROR",
        };
        self.log.push(format!("{what}: {word} after {milliseconds} ms"));
        if let Some(note) = &note {
            self.log.extend(note.lines().map(|line| format!("    {line}")));
        }
        self.shared.reporter.report(&Event::StepEnded {
            machine: &self.machine.name,
            test: &test.id,
            step: what,
            result,
            milliseconds,
            note: note.as_deref().map(short_note),
        });
        (StepRecord { step: what.into(), result, milliseconds, note }, outcome)
    }

    /// Says how long the limit was when a failure came from reaching it.
    fn with_time_limit(&self, why: String, deadline: Instant, test: &Test) -> String {
        if Instant::now() < deadline {
            return why;
        }
        let slowdown = self.driver.slowdown();
        let limit = format_duration(test.time);
        if slowdown > 1 {
            let slow = format_duration(test.time * slowdown);
            format!("{why} (time limit {limit}, {slow} under QEMU's emulator)")
        } else {
            format!("{why} (time limit {limit})")
        }
    }

    fn boot_fresh(&mut self, deadline: Instant) -> Result<Option<String>, StepFailure> {
        self.driver.boot_fresh().map_err(StepFailure::Error)?;
        self.boot += 1;
        self.serial = SerialWatch::new(self.driver.serial_log().to_path_buf());
        self.log.push(format!("Boot {}: serial log {}", self.boot, self.relative_serial()));
        let note = self.wait_ready(deadline)?;
        // The test that boots sees its whole boot: its serial steps may
        // check what came before the ready line.
        self.serial.rewind();
        Ok(note)
    }

    fn relative_serial(&self) -> String {
        self.shared.bundle.relative(self.driver.serial_log())
    }

    /// Waits for the machine's ready line on the serial console, then,
    /// when the machine has an SSH channel, for SSH to answer.
    fn wait_ready(&mut self, deadline: Instant) -> Result<Option<String>, StepFailure> {
        let ready = self.machine.ready.clone();
        self.wait_serial(&ready, deadline)?;
        // Without a key, tests that have no `run` steps still run, on
        // the serial console alone; the run refuses the others before
        // it starts.
        if self.machine.who(Control::Ssh) != Who::Driver || !self.has_key {
            return Ok(None);
        }
        let mut last = String::new();
        while Instant::now() < deadline {
            if self.shared.stop.requested() {
                return Err(StepFailure::Skip(STOPPED_DURING.into()));
            }
            let attempt = deadline.min(Instant::now() + Duration::from_secs(20));
            match self.driver.run_command("true", attempt) {
                Ok(output) if output.status == Some(0) => return Ok(None),
                Ok(output) => last = output.text.trim().to_string(),
                Err(err) => return Err(StepFailure::Error(err)),
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        Err(StepFailure::Fail(format!(
            "booted, but SSH did not answer as the lab account; last answer: {last}"
        )))
    }

    fn run_step(
        &mut self,
        test: &Test,
        step: &Step,
        deadline: Instant,
    ) -> Result<Option<String>, StepFailure> {
        // A control a person does here is theirs to do, whichever it is.
        if let Some(control) = step.control()
            && self.machine.who(control) == Who::Person
        {
            return self.ask_person(test, step, deadline);
        }
        match step {
            Step::Run(command) => self.run_command(command, deadline),
            Step::Serial(text) => self.wait_serial(text, deadline),
            Step::Power(action) => self.power(*action, deadline),
            Step::Link(up) => self.driver.set_link(*up).map(|()| None).map_err(StepFailure::Error),
            Step::PlugUsb(image) => {
                self.driver.plug_usb(image).map(|()| None).map_err(StepFailure::Error)
            }
            Step::UnplugUsb => self.driver.unplug_usb().map(|()| None).map_err(StepFailure::Error),
            Step::Keys(keys) => {
                self.driver.press_keys(keys).map(|()| None).map_err(StepFailure::Error)
            }
            Step::Screenshot(name) => {
                self.screenshot(test, name).map(|()| None).map_err(StepFailure::Error)
            }
            Step::WaitReady => self.wait_ready(deadline),
            Step::Sleep(time) => {
                let until = Instant::now() + *time;
                let end = until.min(deadline);
                while Instant::now() < end {
                    if self.shared.stop.requested() {
                        return Err(StepFailure::Skip(STOPPED_DURING.into()));
                    }
                    let slice = end.saturating_duration_since(Instant::now());
                    std::thread::sleep(slice.min(Duration::from_millis(100)));
                }
                if until > deadline {
                    return Err(StepFailure::Fail(
                        "the wait ran past the test's time limit".into(),
                    ));
                }
                Ok(None)
            }
        }
    }

    /// Asks a person to do `step` on the machine, and waits until they
    /// say they did (`dukovo-lab done`, or Done on the runner page).
    fn ask_person(
        &mut self,
        test: &Test,
        step: &Step,
        deadline: Instant,
    ) -> Result<Option<String>, StepFailure> {
        let machine = &self.machine.name;
        let what = step.describe();
        let done = format!("dukovo-lab done {} --machine {machine}", self.shared.run_id);
        // Only a `done` given after this ask counts.
        self.shared.running.clear_done(machine);
        self.shared.reporter.report(&Event::PersonNeeded {
            machine,
            test: &test.id,
            step: &what,
            done: &done,
        });
        self.log.push(format!("Asked a person to {what}; waiting for `{done}`."));
        loop {
            if self.shared.running.take_done(machine) {
                return Ok(Some("a person said it was done".into()));
            }
            if self.shared.stop.requested() {
                return Err(StepFailure::Skip(STOPPED_DURING.into()));
            }
            if Instant::now() >= deadline {
                return Err(StepFailure::Skip(format!(
                    "no one said \"{what}\" was done before the test's time limit"
                )));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn run_command(
        &mut self,
        command: &Command,
        deadline: Instant,
    ) -> Result<Option<String>, StepFailure> {
        // The machine went off when the test needed it on: Dukovo's
        // failure, as when it turns off before a serial step's text.
        if !self.driver.is_on() {
            return Err(StepFailure::Fail(format!(
                "the machine was off, so `{}` could not run",
                command.command
            )));
        }
        let output =
            self.driver.run_command(&command.command, deadline).map_err(StepFailure::Error)?;
        // Cut short by a stop: the command's end says nothing.
        if output.status.is_none() && self.shared.stop.requested() {
            return Err(StepFailure::Skip(STOPPED_DURING.into()));
        }
        let shown = format!("$ {}\n{}", command.command, output.text.trim_end());
        let fail = |why: String| Err(StepFailure::Fail(format!("{why}\n{shown}")));
        match output.status {
            None => return fail("it was still running at the test's time limit".into()),
            Some(255) if command.exit != 255 => {
                return fail(
                    "SSH could not run it (exit status 255); SSH's message is below".into(),
                );
            }
            Some(status) if status != command.exit => {
                return fail(format!("it exited with status {status}, not {}", command.exit));
            }
            Some(_) => {}
        }
        if let Some(missing) =
            command.expect.iter().find(|text| !output.text.contains(text.as_str()))
        {
            return fail(format!("its output lacks \"{missing}\""));
        }
        if let Some(found) =
            command.expect_not.iter().find(|text| output.text.contains(text.as_str()))
        {
            return fail(format!("its output holds \"{found}\""));
        }
        Ok(Some(shown))
    }

    /// Waits for `text` on the serial console after the last text found.
    fn wait_serial(
        &mut self,
        text: &str,
        deadline: Instant,
    ) -> Result<Option<String>, StepFailure> {
        loop {
            if self.serial.find(text) {
                return Ok(None);
            }
            if !self.driver.is_on() {
                // The last lines may have landed as the machine went off.
                if self.serial.find(text) {
                    return Ok(None);
                }
                return Err(StepFailure::Fail(format!(
                    "the machine turned off before \"{text}\" appeared on the serial console ({})",
                    self.relative_serial()
                )));
            }
            if Instant::now() >= deadline {
                return Err(StepFailure::Fail(format!(
                    "\"{text}\" did not appear on the serial console ({})",
                    self.relative_serial()
                )));
            }
            if self.shared.stop.requested() {
                return Err(StepFailure::Skip(STOPPED_DURING.into()));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn power(
        &mut self,
        action: PowerAction,
        deadline: Instant,
    ) -> Result<Option<String>, StepFailure> {
        // A new boot starts: serial steps and `wait = "ready"` after this
        // look only at what it prints, never at the boot before.
        if matches!(action, PowerAction::On | PowerAction::Reset) {
            self.serial.skip_to_end();
        }
        match self.driver.power(action, deadline) {
            Ok(()) => Ok(None),
            // A machine that does not shut down when asked is Dukovo's
            // failure; anything else here is the driver's.
            Err(why) if action == PowerAction::Off && self.driver.is_on() => {
                Err(StepFailure::Fail(why))
            }
            Err(why) => Err(StepFailure::Error(why)),
        }
    }

    fn screenshot(&mut self, test: &Test, name: &str) -> Result<(), String> {
        let folder = self.shared.bundle.screenshots(&self.machine.name);
        fs::create_dir_all(&folder).map_err(|err| format!("making {}: {err}", folder.display()))?;
        let path = folder.join(format!("{}-{name}.png", test.id.replace('/', "-")));
        self.driver.screenshot(&path)?;
        self.screenshots.push(self.shared.bundle.relative(&path));
        Ok(())
    }

    /// Keeps the screen as a failure left it, when the machine can show it.
    fn screenshot_after_failure(&mut self, test: &Test) {
        if self.machine.who(Control::Screen) == Who::Driver
            && self.driver.is_on()
            && let Err(err) = self.screenshot(test, "at-failure")
        {
            self.log.push(format!("No screenshot at the failure: {err}"));
        }
    }

    /// Writes the test's log; returns its path in the bundle.
    fn write_log(&mut self, test: &Test, result: Outcome, reason: Option<&str>) -> Option<String> {
        self.log.push(match reason {
            Some(reason) => format!("Result: {result:?}: {reason}"),
            None => format!("Result: {result:?}"),
        });
        let folder = self.shared.bundle.logs(&self.machine.name);
        let path = folder.join(format!("{}.log", test.id.replace('/', "-")));
        let mut text = std::mem::take(&mut self.log).join("\n");
        text.push('\n');
        fs::create_dir_all(&folder).and_then(|()| fs::write(&path, text)).ok()?;
        Some(self.shared.bundle.relative(&path))
    }
}

/// Follows a serial log, finding text in order.
///
/// The log is read as it grows, only its new bytes each time, since a
/// soak test's log grows for hours. Whole lines are cleaned once and kept;
/// the line still being written is cleaned again each time, with any
/// character or escape sequence cut short at its end left out until the
/// rest arrives. So the text only ever grows, and a place found in it
/// stays valid.
struct SerialWatch {
    path: PathBuf,
    /// How many bytes of the log have been read.
    read: u64,
    /// The bytes read after the last newline.
    partial: Vec<u8>,
    /// The log's whole lines so far, cleaned as [`without_escapes`] cleans
    /// them, then the line being written, cleaned the same way.
    text: String,
    /// Where in `text` the line being written starts.
    lines_end: usize,
    /// Where the next search starts: the end of the last text found.
    cursor: usize,
}

impl SerialWatch {
    fn new(path: PathBuf) -> SerialWatch {
        SerialWatch {
            path,
            read: 0,
            partial: Vec::new(),
            text: String::new(),
            lines_end: 0,
            cursor: 0,
        }
    }

    /// Reads what the log gained since the last time.
    fn refresh(&mut self) {
        let Ok(mut file) = fs::File::open(&self.path) else { return };
        let mut new = Vec::new();
        if file.seek(SeekFrom::Start(self.read)).is_err() || file.read_to_end(&mut new).is_err() {
            return;
        }
        if new.is_empty() {
            return;
        }
        self.read += new.len() as u64;
        self.partial.extend_from_slice(&new);
        // Whole lines are cleaned once. A newline is never inside a
        // character, and it ends an escape sequence, so a line cleaned
        // alone reads as it does in the whole log.
        self.text.truncate(self.lines_end);
        if let Some(last) = self.partial.iter().rposition(|&byte| byte == b'\n') {
            let rest = self.partial.split_off(last + 1);
            let lines = std::mem::replace(&mut self.partial, rest);
            self.text.push_str(&without_escapes(&String::from_utf8_lossy(&lines)));
            self.lines_end = self.text.len();
        }
        let partial = whole_characters(&self.partial);
        self.text.push_str(&without_escapes(&String::from_utf8_lossy(partial)));
    }

    /// Searches from the end of the log as it is now: a test that shares a
    /// boot sees only what comes after it starts.
    fn skip_to_end(&mut self) {
        self.refresh();
        self.cursor = self.text.len();
    }

    /// Searches from the start of the log again.
    fn rewind(&mut self) {
        self.cursor = 0;
    }

    /// Whether `needle` has appeared since the last text found; moves past it.
    fn find(&mut self, needle: &str) -> bool {
        self.refresh();
        // The text only grows, and the cursor sits at the end of text
        // found or of the text there was, so it is a character's start;
        // this keeps a search from stopping for good if it ever is not.
        while !self.text.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
        let Some(at) = self.text.get(self.cursor..).and_then(|rest| rest.find(needle)) else {
            return false;
        };
        self.cursor += at + needle.len();
        true
    }
}

/// `text` without terminal escape sequences and carriage returns, as a
/// person reading the console sees it. Removing a sequence cut short at
/// the end keeps what came before unchanged as the log grows, so places
/// found in it stay valid. A newline ends a sequence, even one not
/// finished, so each line cleans the same alone as in the whole log.
fn without_escapes(text: &str) -> String {
    let mut clean = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                if chars.peek() == Some(&'[') {
                    // CSI: ESC [ parameters, then a final byte from @ to ~.
                    chars.next();
                    while let Some(&c) = chars.peek() {
                        if c == '\n' {
                            break;
                        }
                        chars.next();
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                } else if chars.peek().is_some_and(|&c| c != '\n') {
                    // Any other escape: ESC and one character.
                    chars.next();
                }
            }
            '\r' => {}
            c => clean.push(c),
        }
    }
    clean
}

/// `bytes` less a UTF-8 character cut short at its end, which the next
/// read finishes.
fn whole_characters(bytes: &[u8]) -> &[u8] {
    // The last character starts within the last four bytes: find its
    // first byte, which is not a continuation byte (10xxxxxx), and see
    // whether all the bytes it says it has are there.
    for back in 1..=bytes.len().min(4) {
        let at = bytes.len() - back;
        let length = match bytes[at] {
            0x80..=0xbf => continue,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => 1,
        };
        return if back < length { &bytes[..at] } else { bytes };
    }
    bytes
}

/// Lets tests run whole runs without QEMU.
#[cfg(test)]
mod fake;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn removes_escapes_the_same_way_as_the_log_grows() {
        let log = "\u{1b}[2J\u{1b}[1;32mBoot complete.\u{1b}[0m\r\nlogin: ";
        assert_eq!(without_escapes(log), "Boot complete.\nlogin: ");
        // Cut inside an escape: the text before it is the same.
        assert_eq!(without_escapes(&log[..8]), "");
        assert_eq!(without_escapes(&log[..12]), "B");
    }

    #[test]
    fn a_line_cleans_the_same_alone_as_in_the_whole_log() {
        // An escape cut short by a newline ends there, and the newline stays.
        assert_eq!(without_escapes("a\u{1b}[12\nb\u{1b}\nc"), "a\nb\nc");
        assert_eq!(without_escapes("a\u{1b}[12\n") + &without_escapes("b"), "a\nb");
    }

    #[test]
    fn waits_for_the_rest_of_a_character_cut_short() {
        let check = "\u{2713}".as_bytes(); // ✓, three bytes
        assert_eq!(whole_characters(&[b'a', check[0], check[1]]), b"a");
        assert_eq!(whole_characters(&[b'a', check[0], check[1], check[2]]).len(), 4);
        assert_eq!(whole_characters(b"plain"), b"plain");
        assert_eq!(whole_characters(&[]), b"");

        let dir = crate::testing::TempDir::new("serial-utf8");
        let path = dir.write("serial.log", "");
        let mut log = fs::OpenOptions::new().append(true).open(&path).unwrap();
        let mut watch = SerialWatch::new(path);
        log.write_all(&[b'o', b'k', b' ', check[0]]).unwrap();
        // A test sharing the boot starts here, the ✓ half written.
        watch.skip_to_end();
        log.write_all(&check[1..]).unwrap();
        log.write_all("\x1b[1mdone\x1b[0".as_bytes()).unwrap();
        assert!(watch.find("\u{2713}done"), "the character finished after the place");
        log.write_all("m\nlogin: ".as_bytes()).unwrap();
        assert!(watch.find("login: "), "a prompt with no newline after it");
        assert_eq!(watch.text, "ok \u{2713}done\nlogin: ");
    }

    #[test]
    fn finds_serial_text_in_order() {
        let dir = crate::testing::TempDir::new("serial");
        let path = dir.write("serial.log", "one\ntwo\none\n");
        let mut watch = SerialWatch::new(path.clone());
        assert!(watch.find("one"));
        assert!(watch.find("one"), "the second one");
        assert!(!watch.find("two"), "two came before the second one");
        fs::write(&path, "one\ntwo\none\ntwo\n").unwrap();
        assert!(watch.find("two"));
        watch.skip_to_end();
        assert!(!watch.find("one"));
    }
}

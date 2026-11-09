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

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::Error;
use crate::bench::{self, Kit, Row};
use crate::bundle::{
    Bundle, MachineRecord, Outcome, ResultRecord, RunRecord, SCHEMA, StepRecord, Summary,
    new_run_id,
};
use crate::clock::Utc;
use crate::driver::{ATTACKER_ON_SEGMENT, Driver, Setup, TARGET_ON_SEGMENT};
use crate::findings;
use crate::machines::{Control, Machine, Who};
use crate::output::{Event, Reporter, short_note};
use crate::peer::{self, Job, Ports, Readiness, SKIP_STATUS, Token};
use crate::screen::{self, Font};
use crate::ssh;
use crate::stop::Stop;
use crate::store::Running;
use crate::suite::{
    Bench, Boot, Command, Fit, PASSWORD_TOKEN, PowerAction, Serve, Step, TARGET_TOKEN, Test,
};
use crate::units::format_duration;

/// What a stopped test's result says.
const STOPPED_DURING: &str = "the run was stopped during this test";
const STOPPED_BEFORE: &str = "the run was stopped before this test";

/// How often a step that is still running reports it is alive (a
/// `step-progress` event), so a long step — a soak, a slow boot — is not
/// silent for minutes on end. Short under test, so the beat can be checked
/// without a slow wait.
const HEARTBEAT: Duration =
    if cfg!(test) { Duration::from_millis(100) } else { Duration::from_secs(30) };
/// Why a `root` step cannot run.
pub const NO_PASSWORD: &str = "this step runs as root, and the lab has no password: run `dukovo-lab key`, then build the image again with `ob image --lab`";

/// Why a `bench` step cannot run.
pub const NO_KIT: &str = "this step holds Dukovo to Linux, and the run was given no Linux kit: `ob lab` builds one and passes it with --linux";

/// Why a step that reads the screen's text is skipped.
pub const NO_FONTS: &str = "this step reads the screen, and the run was given no console fonts: `ob lab` passes Dukovo's with --fonts";

/// Why a test that boots the install stick is skipped.
pub const NO_STICK: &str = "this test boots the install stick, and the run was given none: `ob lab` builds one and passes it with --stick";

/// Why a test that boots the install DVD is skipped.
pub const NO_DVD: &str = "this test boots the install DVD, and the run was given none: `ob lab` builds one and passes it with --dvd";

/// Why a `bench` step's numbers are kept but not held to Linux's.
pub const EMULATED: &str = "QEMU's emulator, not the machine, made these numbers (no KVM), so they are kept but not held to Linux's";

/// The command that gives a machine the address `ip` on net1, the private
/// network, and sets that port up. Both run in one shell, so that when it
/// is run as root (through `elevate`, which elevates only its first
/// command) the port is set up as root too.
fn address_setup(ip: &str) -> String {
    format!("sh -c 'ip address add {ip}/24 dev net1 && ip link set net1 up'")
}

/// Holds `output` of a `run`, `attack` or `host` command to what the
/// command asked: its exit status, and the text its output must and must
/// not hold. `shown` is how a log shows the command and what it printed,
/// for a failure message. `over_ssh` says it ran over SSH, whose own
/// failures exit with 255. Returns `shown` on success, so the step's log
/// line is the run.
fn check_command(
    command: &Command,
    output: &ssh::Output,
    shown: String,
    over_ssh: bool,
) -> Result<Option<String>, StepFailure> {
    let fail = |why: String| Err(StepFailure::Fail(format!("{why}\n{shown}")));
    match output.status {
        None => return fail("it was still running at the test's time limit".into()),
        Some(255) if over_ssh && command.exit != 255 => {
            return fail("SSH could not run it (exit status 255); SSH's message is below".into());
        }
        Some(status) if status != command.exit => {
            return fail(format!("it exited with status {status}, not {}", command.exit));
        }
        Some(_) => {}
    }
    if let Some(missing) = command.expect.iter().find(|text| !output.text.contains(text.as_str())) {
        return fail(format!("its output lacks \"{missing}\""));
    }
    if let Some(found) = command.expect_not.iter().find(|text| output.text.contains(text.as_str()))
    {
        return fail(format!("its output holds \"{found}\""));
    }
    Ok(Some(shown))
}

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
    /// The lab account's password, which `root` steps give `elevate`,
    /// when the lab has one.
    pub password: Option<String>,
    /// Whether to run Needs you tests, asking a person to do their steps.
    pub with_person: bool,
    /// The Linux that performance tests hold Dukovo to, when given.
    pub linux: Option<Kit>,
    /// The console's fonts, for steps that read the screen, when given.
    pub fonts: Option<Vec<Font>>,
    /// The install stick's image, for tests that boot it, when given.
    pub stick: Option<PathBuf>,
    /// The install DVD's ISO, for tests that boot it, when given.
    pub dvd: Option<PathBuf>,
    /// The preset the tests came from, for run.json.
    pub preset: Option<String>,
    /// The schedule that started the run, for run.json.
    pub schedule: Option<String>,
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
    password: Option<&'a str>,
    linux: Option<&'a Kit>,
    /// Held while a machine measures performance, so that machines run
    /// side by side never time each other's load.
    measuring: &'a Mutex<()>,
    fonts: Option<&'a [Font]>,
    /// The install stick, given a second name in the work folder as the
    /// image is.
    stick: Option<&'a Path>,
    /// The install DVD, likewise.
    dvd: Option<&'a Path>,
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
        schedule: request.schedule.clone(),
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
    let image = pin_image(&request.image, &work, "under-test");
    let stick = request.stick.as_ref().map(|stick| pin_image(stick, &work, "install-stick"));
    let dvd = request.dvd.as_ref().map(|dvd| pin_image(dvd, &work, "install-dvd"));
    let sockets = socket_folder(&id);
    let stop = Stop::default();
    let finished = AtomicBool::new(false);
    let measuring = Mutex::new(());
    let shared = Shared {
        run_id: &id,
        bundle: &bundle,
        reporter,
        running: &running,
        stop: &stop,
        with_person: request.with_person,
        password: request.password.as_deref(),
        linux: request.linux.as_ref(),
        measuring: &measuring,
        fonts: request.fonts.as_deref(),
        stick: stick.as_deref(),
        dvd: dvd.as_deref(),
    };
    let results: Vec<ResultRecord> = std::thread::scope(|scope| {
        // Ends the watcher below however this ends, a worker's panic too,
        // or the scope would wait for it for ever.
        let _finished = SetOnDrop(&finished);
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
                    image: image.clone(),
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
        workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("a machine's worker panicked"))
            .collect()
    });
    let _ = fs::remove_dir_all(&work);
    let _ = fs::remove_dir_all(&sockets);

    let summary = Summary::of(&results);
    let stopped = stop.requested();
    bundle.write_results(&results).map_err(Error::Harness)?;
    // Its failures join the findings they repeat, or make new ones, before
    // run.json says it ended: whoever sees it ended sees its findings too.
    let findings = findings::record(&request.store, &bundle.record, &results);
    bundle.record.ended = Some(Utc::now().rfc3339());
    bundle.record.summary = Some(summary);
    bundle.record.stopped = stopped;
    bundle.write_run().map_err(Error::Harness)?;
    let findings = findings?;
    reporter.report(&Event::RunEnded {
        id: &id,
        bundle: bundle.dir(),
        summary,
        stopped,
        findings: &findings,
    });
    // The run's locks go only now, once run.json says it ended, so a
    // reader never sees a run that is neither going nor ended.
    running.end();
    Ok(Ended { summary })
}

/// A short folder for the run's sockets: the user's runtime folder when
/// there is one, else the system's temporary folder.
/// Sets its flag when dropped.
struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Keeps the image under test (or the install stick) the same for the
/// whole run: a second name for it, `name` and its extension, in the run's
/// work folder, which every boot then uses.
///
/// `ob image` makes a new file where the image was rather than writing
/// over it, so a build started while this run goes (another `ob lab run`,
/// say) leaves this run's image as it was, instead of changing it between
/// one test's boot and the next. When the store is on another file system
/// than the image, there can be no second name, and the run uses the
/// image where it is.
fn pin_image(image: &Path, work: &Path, name: &str) -> PathBuf {
    let Ok(real) = fs::canonicalize(image) else { return image.to_path_buf() };
    // A dot, so no machine's folder beside it can have its name; the
    // extension stays, as it says the image's format.
    let qcow2 = real.extension().is_some_and(|ext| ext == "qcow2");
    let pinned = work.join(format!("{name}.{}", if qcow2 { "qcow2" } else { "img" }));
    match fs::create_dir_all(work).and_then(|()| fs::hard_link(&real, &pinned)) {
        Ok(()) => pinned,
        Err(_) => image.to_path_buf(),
    }
}

/// `path` from the root: a host step runs in its folder, so a placeholder
/// naming a folder must not depend on where the lab was started.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

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
    let screen_file = setup.work.join("screen.ppm");
    // The tests' folders on the lab PC go in the short socket folder, as
    // servers run in them make sockets of their own.
    let peer_folder = setup.sockets.join("peer");
    let (mut worker, broken) = match start(machine, setup) {
        Ok(driver) => {
            (Some(Worker::new(machine, driver, has_key, shared, screen_file, peer_folder)), None)
        }
        Err(err) => (None, Some(format!("the {} machine did not start: {err}", machine.name))),
    };
    let mut results = Vec::new();
    for &test in tests {
        let skip = match test.fit(machine) {
            _ if shared.stop.requested() => Some(STOPPED_BEFORE.to_string()),
            Fit::NeedsPerson(_) if shared.with_person => None,
            fit => fit.skip_reason(),
        };
        // A lab PC without a server the test runs on it cannot do its side.
        let skip = skip.or_else(|| {
            let missing = peer::missing_programs(&test.host_needs);
            (!missing.is_empty()).then(|| {
                format!(
                    "the lab PC has no {}, which this test runs there: install it to run the test",
                    missing.join(", ")
                )
            })
        });
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
    /// Linux's numbers, from the machine's Linux twin, once measured: once
    /// a run is enough, as nothing about Linux changes between tests.
    linux: Option<BTreeMap<String, f64>>,
    /// Where the screen is saved to read its text, in the work folder.
    screen_file: PathBuf,
    /// Whether this boot's attacker is up and both sides have their
    /// addresses on the private network; set up on the first attack step.
    attacker_ready: bool,
    /// The test's side on the lab PC: see [`PeerSide`].
    peer: PeerSide,
    /// How host steps run as root, found the first time one does (as
    /// they are, or through sudo), or why they cannot.
    host_root: Option<Result<Vec<String>, String>>,
}

/// What a test has on the lab PC, which goes when the test ends.
#[derive(Default)]
struct PeerSide {
    /// Where each test's folder goes, in the run's short socket folder.
    base: PathBuf,
    /// Tests run so far, which names each test's folder.
    tests: u32,
    /// The ports named with `{port:NAME}`.
    ports: Ports,
    /// The test's folder (`{work}`), once a step needed it.
    work: Option<PathBuf>,
    /// The test file's folder (`{here}`).
    here: PathBuf,
    /// Servers started by `background` steps.
    jobs: Vec<Job>,
    /// Whether a host step ran as root, and so may have left root's files
    /// in the test's folder.
    rooted: bool,
}

impl<'a> Worker<'a> {
    fn new(
        machine: &'a Machine,
        driver: Box<dyn Driver>,
        has_key: bool,
        shared: &'a Shared<'a>,
        screen_file: PathBuf,
        peer_folder: PathBuf,
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
            linux: None,
            screen_file,
            attacker_ready: false,
            peer: PeerSide { base: peer_folder, ..PeerSide::default() },
            host_root: None,
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
        self.peer.tests += 1;
        self.peer.here = test.file.parent().map(absolute).unwrap_or_default();

        // A test that attacks the machine wants its private network port,
        // which only an armed boot has; one that does not must not have it
        // (an idle port would show up in a network test), so tell the
        // driver which kind of boot to give before it boots.
        let attacks = test.controls().contains(&Control::Attack);
        self.driver.want_attacker(attacks);
        // So does a test that puts cards in a card reader, whose reader,
        // an empty drive, a disk test must not find either.
        let cards = test.controls().contains(&Control::Card);
        self.driver.want_card_reader(cards);
        // And one that puts discs in an optical drive, as empty.
        let discs = test.controls().contains(&Control::Disc);
        self.driver.want_disc_drive(discs);
        let own_boot = attacks || cards || discs;

        let mut failure = None;
        if let Boot::Stick { disk } = test.boot {
            let (record, outcome) =
                self.timed(test, "boot from the install stick, with a blank disk", |worker| {
                    worker.boot_stick(disk)
                });
            steps.push(record);
            failure = outcome.err();
        } else if let Boot::Dvd { disk } = test.boot {
            let (record, outcome) =
                self.timed(test, "boot from the install DVD, with a blank disk", |worker| {
                    worker.boot_dvd(disk)
                });
            steps.push(record);
            failure = outcome.err();
        } else if test.fresh || own_boot || !self.reusable || !self.driver.is_on() {
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

        // The test's servers on the lab PC end with it, failed or not.
        self.end_peer_side();

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
        // After the installer, the disk no longer holds the image under
        // test, so the next test boots afresh. An attack boot has the
        // private network port a later test must not find, a card test's
        // boot the card reader and a disc test's the optical drive, so
        // they are not shared either.
        self.reusable =
            result == Outcome::Pass && self.driver.is_on() && test.boot == Boot::Image && !own_boot;
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
        // While the step runs, a background beat reports it is still alive,
        // so a long step (a soak, a slow boot) is not silent for minutes.
        // The reporter lives for the whole run ('a), so the beat borrows it
        // beside the step's own `&mut self`; the step's name and the ids are
        // copied in, as a thread cannot borrow them from `self`.
        let reporter = self.shared.reporter;
        let machine = self.machine.name.clone();
        let test_id = test.id.clone();
        let step = what.to_string();
        // The beat sleeps on this channel between beats. Nothing is ever
        // sent: the step's end drops `alive`, which wakes the beat at once,
        // so a step does not wait out the rest of a sleep before it ends.
        // A step that panics drops `alive` as it unwinds too, so the scope
        // below never waits for ever on a beat that would not stop.
        let (alive, ended) = mpsc::channel::<()>();
        let outcome = std::thread::scope(|scope| {
            scope.spawn(move || {
                let mut next = HEARTBEAT;
                loop {
                    let wait = next.saturating_sub(started.elapsed());
                    match ended.recv_timeout(wait) {
                        Err(RecvTimeoutError::Timeout) => {}
                        // The step is over (or nothing was sent, which
                        // cannot happen): stop beating.
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                    }
                    let elapsed = started.elapsed();
                    if elapsed >= next {
                        reporter.report(&Event::StepProgress {
                            machine: &machine,
                            test: &test_id,
                            step: &step,
                            milliseconds: elapsed.as_millis() as u64,
                        });
                        next += HEARTBEAT;
                    }
                }
            });
            let outcome = action(self);
            drop(alive);
            outcome
        });
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
        // A new boot starts with no attacker and no addresses on the
        // private network; the first attack step sets them up again.
        self.attacker_ready = false;
        self.serial = SerialWatch::new(self.driver.serial_log().to_path_buf());
        self.log.push(format!("Boot {}: serial log {}", self.boot, self.relative_serial()));
        let note = self.wait_ready(deadline)?;
        // The test that boots sees its whole boot: its serial steps may
        // check what came before the ready line.
        self.serial.rewind();
        Ok(note)
    }

    /// Boots the install stick with a blank disk, and waits for nothing:
    /// the test's steps follow the stick's boot.
    fn boot_stick(&mut self, disk: u64) -> Result<Option<String>, StepFailure> {
        let Some(stick) = self.shared.stick else {
            return Err(StepFailure::Skip(NO_STICK.into()));
        };
        self.driver.boot_stick(stick, disk).map_err(StepFailure::Error)?;
        self.boot += 1;
        self.attacker_ready = false;
        self.serial = SerialWatch::new(self.driver.serial_log().to_path_buf());
        self.log.push(format!(
            "Boot {}: from the install stick; serial log {}",
            self.boot,
            self.relative_serial()
        ));
        Ok(None)
    }

    fn boot_dvd(&mut self, disk: u64) -> Result<Option<String>, StepFailure> {
        let Some(dvd) = self.shared.dvd else {
            return Err(StepFailure::Skip(NO_DVD.into()));
        };
        self.driver.boot_dvd(dvd, disk).map_err(StepFailure::Error)?;
        self.boot += 1;
        self.attacker_ready = false;
        self.serial = SerialWatch::new(self.driver.serial_log().to_path_buf());
        self.log.push(format!(
            "Boot {}: from the install DVD; serial log {}",
            self.boot,
            self.relative_serial()
        ));
        Ok(None)
    }

    fn relative_serial(&self) -> String {
        self.shared.bundle.relative(self.driver.serial_log())
    }

    /// Waits for the machine's ready line on the serial console, then,
    /// when the machine has an SSH channel, for SSH to answer.
    fn wait_ready(&mut self, deadline: Instant) -> Result<Option<String>, StepFailure> {
        // The ready line counts wherever it is in this boot, so a test may
        // wait for it with a serial step first.
        let ready = self.machine.ready.clone();
        self.wait_serial_where(&ready, Search::ThisBoot, deadline)?;
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
            match self.driver.run_command("true", None, attempt) {
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
            Step::Attack(command) => self.run_attack(command, deadline),
            Step::Host(command) => self.run_host(command, deadline),
            Step::Serve(serve) => self.serve(test, serve, deadline),
            Step::Bench(bench) => self.bench(bench, deadline),
            Step::Serial(text) => self.wait_serial(text, deadline),
            Step::Power(action) => self.power(*action, deadline),
            Step::Link(up) => self.driver.set_link(*up).map(|()| None).map_err(StepFailure::Error),
            Step::PlugUsb(stick) => {
                self.driver.plug_usb(stick).map(|()| None).map_err(StepFailure::Error)
            }
            Step::UnplugUsb => self.driver.unplug_usb().map(|()| None).map_err(StepFailure::Error),
            Step::InsertCard(card) => {
                self.driver.insert_card(card).map(|()| None).map_err(StepFailure::Error)
            }
            Step::RemoveCard => {
                self.driver.remove_card().map(|()| None).map_err(StepFailure::Error)
            }
            Step::ReinsertCard => {
                self.driver.reinsert_card().map(|()| None).map_err(StepFailure::Error)
            }
            Step::InsertDisc(image) => {
                self.driver.insert_disc(image).map(|()| None).map_err(StepFailure::Error)
            }
            Step::EjectDisc => self.driver.eject_disc().map(|()| None).map_err(StepFailure::Error),
            Step::ReinsertDisc => {
                self.driver.reinsert_disc().map(|()| None).map_err(StepFailure::Error)
            }
            Step::PressDiscButton => {
                self.driver.press_disc_button().map(|()| None).map_err(StepFailure::Error)
            }
            Step::Keys(keys) => {
                self.driver.press_keys(keys).map(|()| None).map_err(StepFailure::Error)
            }
            Step::Type(text) => self.type_text(text),
            Step::Point { x, y } => {
                self.driver.point(*x, *y).map(|()| None).map_err(StepFailure::Error)
            }
            Step::Click(button) => {
                self.driver.click(*button).map(|()| None).map_err(StepFailure::Error)
            }
            Step::Screen(texts) => self.wait_screen(texts, true, deadline),
            Step::ScreenGone(text) => self.wait_screen(std::slice::from_ref(text), false, deadline),
            Step::Screenshot(name) => {
                self.screenshot(test, name).map(|()| None).map_err(StepFailure::Error)
            }
            Step::WaitReady => self.wait_ready(deadline),
            Step::WaitOff => self.wait_off(deadline),
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
        let command = self.fill_command(command)?;
        let input = self.fill_input(&command)?;
        let (output, shown) =
            self.execute(&command.command, command.root, input.as_deref(), deadline)?;
        check_command(&command, &output, shown, true)
    }

    /// Runs a `host` step's command on the lab PC and checks it as a `run`
    /// step's is. One that exits with [`SKIP_STATUS`] says the lab PC
    /// cannot do its part, and the test is skipped with what it printed.
    fn run_host(
        &mut self,
        command: &Command,
        deadline: Instant,
    ) -> Result<Option<String>, StepFailure> {
        let command = self.fill_command(command)?;
        let input = self.fill_input(&command)?;
        let root = self.host_root(command.root)?;
        let folder = self.peer_work()?;
        let scratch = self.peer.base.join("output");
        let output = peer::run(
            &command.command,
            &root,
            input.as_deref(),
            &folder,
            &scratch,
            deadline,
            self.shared.stop,
        )
        .map_err(StepFailure::Error)?;
        let prompt = if command.root { "lab-pc #" } else { "lab-pc $" };
        let (output, shown) = self.finish(&command.command, output, prompt)?;
        if output.status == Some(SKIP_STATUS) && command.exit != SKIP_STATUS {
            return Err(StepFailure::Skip(format!(
                "the lab PC cannot do its part of this test: {}",
                output.text.trim()
            )));
        }
        check_command(&command, &output, shown, false)
    }

    /// Starts a server on the lab PC for the rest of the test, and waits
    /// until it says it is ready when the step says what it prints then.
    /// A server that ends or never gets ready is the lab's fault, not
    /// Dukovo's: an error, or a skip when it exits with [`SKIP_STATUS`].
    fn serve(
        &mut self,
        test: &Test,
        serve: &Serve,
        deadline: Instant,
    ) -> Result<Option<String>, StepFailure> {
        let command = self.fill(&serve.command)?;
        let ready = serve.ready.as_deref().map(|ready| self.fill(ready)).transpose()?;
        let root = self.host_root(serve.root)?;
        let folder = self.peer_work()?;
        let logs = self.shared.bundle.logs(&self.machine.name);
        fs::create_dir_all(&logs)
            .map_err(|err| StepFailure::Error(format!("making {}: {err}", logs.display())))?;
        let log = logs.join(format!(
            "{}-host-{}.log",
            test.id.replace('/', "-"),
            self.peer.jobs.len() + 1
        ));
        let job = Job::start(&command, &root, &folder, &log).map_err(StepFailure::Error)?;
        self.peer.rooted |= serve.root;
        self.peer.jobs.push(job);
        let shown = self.shared.bundle.relative(&log);
        let Some(ready) = ready else {
            return Ok(Some(format!("started; its output goes to {shown}")));
        };
        let job = self.peer.jobs.last_mut().expect("pushed above");
        let readiness = job.wait_for(&ready, deadline, self.shared.stop);
        let tail = job.tail();
        match readiness {
            Readiness::Ready => Ok(Some(format!("ready; its output goes to {shown}"))),
            Readiness::Stopped => Err(StepFailure::Skip(STOPPED_DURING.into())),
            Readiness::Exited(Some(SKIP_STATUS)) => Err(StepFailure::Skip(format!(
                "the lab PC cannot do its part of this test: {}",
                tail.trim()
            ))),
            Readiness::Exited(status) => {
                let status = status.map_or("a signal".to_string(), |s| format!("status {s}"));
                Err(StepFailure::Error(format!(
                    "the lab PC's server ended ({status}) before it said \"{ready}\"; the end of {shown}:\n{tail}"
                )))
            }
            Readiness::TimedOut => Err(StepFailure::Error(format!(
                "the lab PC's server did not say \"{ready}\" before the test's time limit; the end of {shown}:\n{tail}"
            ))),
        }
    }

    /// How a host step runs: as it is, or as root through
    /// [`peer::root_prefix`], found once a run. A lab that cannot run it
    /// as root skips the test, as that says nothing about Dukovo.
    fn host_root(&mut self, root: bool) -> Result<Vec<String>, StepFailure> {
        if !root {
            return Ok(Vec::new());
        }
        self.peer.rooted = true;
        self.host_root.get_or_insert_with(peer::root_prefix).clone().map_err(StepFailure::Skip)
    }

    /// The test's folder on the lab PC (`{work}`), made the first time a
    /// step needs it.
    fn peer_work(&mut self) -> Result<PathBuf, StepFailure> {
        if let Some(work) = &self.peer.work {
            return Ok(work.clone());
        }
        let work = self.peer.base.join(self.peer.tests.to_string());
        let _ = fs::remove_dir_all(&work);
        fs::create_dir_all(&work)
            .map_err(|err| StepFailure::Error(format!("making {}: {err}", work.display())))?;
        let work = absolute(&work);
        self.peer.work = Some(work.clone());
        Ok(work)
    }

    /// `text` with its placeholders (see peer.rs) filled in.
    fn fill(&mut self, text: &str) -> Result<String, StepFailure> {
        peer::fill(text, |token| match token {
            Token::Peer => self.driver.peer_address().ok_or_else(|| {
                StepFailure::Error("this machine's driver has no address for the lab PC".into())
            }),
            Token::Machine => self.driver.machine_address().ok_or_else(|| {
                StepFailure::Error("this machine's driver has no address for the machine".into())
            }),
            Token::MachinePort { port, udp } => self
                .driver
                .reach(*port, *udp)
                .map(|port| port.to_string())
                .map_err(StepFailure::Error),
            Token::Port(name) => {
                self.peer.ports.get(name).map(|port| port.to_string()).map_err(StepFailure::Error)
            }
            Token::Work => self.peer_work().map(|work| work.display().to_string()),
            Token::Here => Ok(self.peer.here.display().to_string()),
        })
    }

    /// `command` with the placeholders of its command and checks filled in.
    fn fill_command(&mut self, command: &Command) -> Result<Command, StepFailure> {
        Ok(Command {
            command: self.fill(&command.command)?,
            exit: command.exit,
            expect: command.expect.iter().map(|text| self.fill(text)).collect::<Result<_, _>>()?,
            expect_not: command
                .expect_not
                .iter()
                .map(|text| self.fill(text))
                .collect::<Result<_, _>>()?,
            root: command.root,
            input: command.input.as_deref().map(|input| self.fill(input)).transpose()?,
        })
    }

    /// A command's input, with `{password}` the lab password. It is never
    /// logged, and what the command prints has the password taken out.
    fn fill_input(&self, command: &Command) -> Result<Option<String>, StepFailure> {
        let Some(input) = &command.input else { return Ok(None) };
        if !input.contains(PASSWORD_TOKEN) {
            return Ok(Some(input.clone()));
        }
        let Some(password) = self.shared.password else {
            return Err(StepFailure::Error(NO_PASSWORD.into()));
        };
        Ok(Some(input.replace(PASSWORD_TOKEN, password)))
    }

    /// Ends the test's servers on the lab PC, noting in its log how each
    /// ended, and clears its ports and folder for the next test.
    fn end_peer_side(&mut self) {
        for mut job in std::mem::take(&mut self.peer.jobs) {
            let ended = job.end();
            let log = self.shared.bundle.relative(&job.log);
            self.log
                .push(format!("Lab PC server `{}`: {ended}; its output is in {log}", job.command));
        }
        self.peer.ports = Ports::default();
        if let Some(work) = self.peer.work.take()
            && fs::remove_dir_all(&work).is_err()
            && self.peer.rooted
            && let Some(Ok(root)) = &self.host_root
            && let Some((first, rest)) = root.split_first()
        {
            // Root's files, which the lab's own account cannot remove.
            let _ = std::process::Command::new(first)
                .args(rest)
                .args(["rm", "-rf", "--"])
                .arg(&work)
                .stdin(std::process::Stdio::null())
                .status();
        }
        self.peer.rooted = false;
    }

    /// Runs an `attack` step on the attacker, bringing the attacker up and
    /// giving both machines their private-network addresses the first time
    /// this boot. The step's `{target}` names the machine under test on
    /// that network, so a test need not know the address.
    fn run_attack(
        &mut self,
        command: &Command,
        deadline: Instant,
    ) -> Result<Option<String>, StepFailure> {
        self.ready_attacker(deadline)?;
        // `{target}` is the machine under test's address on the private
        // network, in the command and in the text its output must hold.
        let resolved = Command {
            command: command.command.replace(TARGET_TOKEN, TARGET_ON_SEGMENT),
            exit: command.exit,
            expect: command
                .expect
                .iter()
                .map(|text| text.replace(TARGET_TOKEN, TARGET_ON_SEGMENT))
                .collect(),
            expect_not: command
                .expect_not
                .iter()
                .map(|text| text.replace(TARGET_TOKEN, TARGET_ON_SEGMENT))
                .collect(),
            root: command.root,
            input: None,
        };
        let (output, shown) = self.execute_attack(&resolved.command, resolved.root, deadline)?;
        check_command(&resolved, &output, shown, true)
    }

    /// Brings the attacker up and gives it and the machine under test their
    /// addresses on the private network, once per boot. Later attack steps
    /// in the same boot find it ready and return at once.
    fn ready_attacker(&mut self, deadline: Instant) -> Result<(), StepFailure> {
        if self.attacker_ready {
            return Ok(());
        }
        self.driver.start_attacker(deadline).map_err(StepFailure::Error)?;
        // Neither machine can reach the other until each has an address on
        // net1, the port on their private network, and that port is up.
        // Both commands go to a shell run as root (`elevate` elevates only
        // the first command it is given), so setting the port up is root's.
        let (output, shown) =
            self.execute(&address_setup(TARGET_ON_SEGMENT), true, None, deadline)?;
        if output.status != Some(0) {
            return Err(StepFailure::Error(format!(
                "could not give the machine its address on the private network\n{shown}"
            )));
        }
        let (output, shown) =
            self.execute_attack(&address_setup(ATTACKER_ON_SEGMENT), true, deadline)?;
        if output.status != Some(0) {
            return Err(StepFailure::Error(format!(
                "could not give the attacker its address on the private network\n{shown}"
            )));
        }
        self.attacker_ready = true;
        Ok(())
    }

    /// Runs `command` on the machine, as root through elevate when `root`,
    /// with `input` as its standard input, and gives back its output and
    /// how a log shows it: a prompt, the command and what it printed.
    fn execute(
        &mut self,
        command: &str,
        root: bool,
        input: Option<&str>,
        deadline: Instant,
    ) -> Result<(ssh::Output, String), StepFailure> {
        // The machine went off when the test needed it on: Dukovo's
        // failure, as when it turns off before a serial step's text.
        if !self.driver.is_on() {
            return Err(StepFailure::Fail(format!(
                "the machine was off, so `{command}` could not run"
            )));
        }
        let (line, input, prompt) = self.as_root(command, root, input)?;
        let output = self
            .driver
            .run_command(&line, input.as_deref(), deadline)
            .map_err(StepFailure::Error)?;
        self.finish(command, output, prompt)
    }

    /// Runs `command` on the attacker, as root through elevate when `root`,
    /// like [`Worker::execute`] but on the attacker rather than the machine
    /// under test.
    fn execute_attack(
        &mut self,
        command: &str,
        root: bool,
        deadline: Instant,
    ) -> Result<(ssh::Output, String), StepFailure> {
        let (line, input, prompt) = self.as_root(command, root, None)?;
        let output =
            self.driver.attack(&line, input.as_deref(), deadline).map_err(StepFailure::Error)?;
        self.finish(command, output, prompt)
    }

    /// Wraps `command` to run as root when `root`: `elevate -S` reads the
    /// lab's password from the command's input, so it is never on a command
    /// line, and leaves the rest of the input, `input`, to the command (it
    /// reads the password a byte at a time). Gives back the line to run,
    /// its input, and the prompt a log shows it under (`#` as root, `$`
    /// otherwise).
    fn as_root(
        &self,
        command: &str,
        root: bool,
        input: Option<&str>,
    ) -> Result<(String, Option<String>, &'static str), StepFailure> {
        if root {
            let Some(password) = self.shared.password else {
                return Err(StepFailure::Error(NO_PASSWORD.into()));
            };
            let input = format!("{password}\n{}", input.unwrap_or_default());
            Ok((format!("elevate -S {command}"), Some(input), "#"))
        } else {
            Ok((command.to_string(), input.map(String::from), "$"))
        }
    }

    /// Takes the password out of `output`, turns a stop into a skip, and
    /// gives back the output with how a log shows the run.
    fn finish(
        &self,
        command: &str,
        mut output: ssh::Output,
        prompt: &str,
    ) -> Result<(ssh::Output, String), StepFailure> {
        // The password is taken out of whatever comes back, so a log never
        // shows it.
        if let Some(password) = self.shared.password {
            output.text = output.text.replace(password, "********");
        }
        // Cut short by a stop: the command's end says nothing.
        if output.status.is_none() && self.shared.stop.requested() {
            return Err(StepFailure::Skip(STOPPED_DURING.into()));
        }
        let shown = format!("{prompt} {command}\n{}", output.text.trim_end());
        Ok((output, shown))
    }

    /// Runs a benchmark program and holds the rows the step names to
    /// Linux's, which the machine's Linux twin measures once per run.
    fn bench(&mut self, bench: &Bench, deadline: Instant) -> Result<Option<String>, StepFailure> {
        let Some(kit) = self.shared.linux else {
            return Err(StepFailure::Skip(NO_KIT.into()));
        };
        // One machine measures at a time; the others wait here.
        let _measuring = self.shared.measuring.lock().unwrap_or_else(PoisonError::into_inner);
        let linux = match self.linux.take() {
            Some(linux) => linux,
            None => {
                let log = self
                    .shared
                    .bundle
                    .logs(&self.machine.name)
                    .join(format!("linux-boot-{}.log", self.boot));
                let text =
                    self.driver.linux_twin(kit, &log, deadline).map_err(StepFailure::Error)?;
                bench::parse(&text)
            }
        };
        let outcome = self.hold_to_linux(bench, &linux, deadline);
        self.linux = Some(linux);
        outcome
    }

    fn hold_to_linux(
        &mut self,
        bench: &Bench,
        linux: &BTreeMap<String, f64>,
        deadline: Instant,
    ) -> Result<Option<String>, StepFailure> {
        // A row Linux's kit does not measure is the test's mistake, or
        // the kit's, not Dukovo's.
        let unmeasured: Vec<&str> =
            bench.rows.iter().filter(|row| !linux.contains_key(*row)).map(String::as_str).collect();
        if !unmeasured.is_empty() {
            return Err(StepFailure::Error(format!(
                "the Linux kit measures no row {}",
                unmeasured.join(", ")
            )));
        }
        let (output, shown) = self.execute(&bench.command, bench.root, None, deadline)?;
        let fail = |why: String| Err(StepFailure::Fail(format!("{why}\n{shown}")));
        match output.status {
            None => return fail("it was still running at the test's time limit".into()),
            Some(0) => {}
            Some(status) => {
                // The program's own last word (its error, usually) makes the
                // first line, so each way of breaking is a finding of its own.
                let said = output
                    .text
                    .lines()
                    .rev()
                    .map(str::trim)
                    .find(|line| !line.is_empty() && !line.starts_with("BENCH"))
                    .unwrap_or("nothing");
                return fail(format!("the benchmark program exited with status {status}: {said}"));
            }
        }
        let rows =
            match bench::compare(&bench.rows, bench.limit, &bench::parse(&output.text), linux) {
                Ok(rows) => rows,
                Err(why) => return fail(why),
            };
        let table = bench::table(&rows);
        if self.driver.emulated() {
            return Err(StepFailure::Skip(format!("{EMULATED}\n{table}")));
        }
        if rows.iter().all(Row::passes) {
            Ok(Some(table))
        } else {
            // The first line names no number, so a row that comes back over
            // the limit joins the same finding, whatever it measured.
            Err(StepFailure::Fail(format!(
                "slower than Linux: the rows marked over take more than their share of Linux's time\n{table}"
            )))
        }
    }

    /// Waits until the machine turns itself off, as after `poweroff`.
    fn wait_off(&mut self, deadline: Instant) -> Result<Option<String>, StepFailure> {
        while self.driver.is_on() {
            if self.shared.stop.requested() {
                return Err(StepFailure::Skip(STOPPED_DURING.into()));
            }
            if Instant::now() >= deadline {
                return Err(StepFailure::Fail(
                    "the machine was still on at the test's time limit".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok(None)
    }

    /// Waits for `text` on the serial console after the last text found.
    fn wait_serial(
        &mut self,
        text: &str,
        deadline: Instant,
    ) -> Result<Option<String>, StepFailure> {
        self.wait_serial_where(text, Search::AfterLastFound, deadline)
    }

    fn wait_serial_where(
        &mut self,
        text: &str,
        search: Search,
        deadline: Instant,
    ) -> Result<Option<String>, StepFailure> {
        let found = |serial: &mut SerialWatch| match search {
            Search::AfterLastFound => serial.find(text),
            Search::ThisBoot => serial.seen_this_boot(text),
        };
        loop {
            if found(&mut self.serial) {
                return Ok(None);
            }
            if !self.driver.is_on() {
                // The last lines may have landed as the machine went off.
                if found(&mut self.serial) {
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
            self.serial.new_boot();
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
        // Its text beside it, when it can be read, for findings and for
        // agents, which read text better than pictures.
        if self.shared.fonts.is_some()
            && let Ok(screen) = self.read_screen()
        {
            let _ = fs::write(path.with_extension("txt"), screen::as_text(&screen));
        }
        Ok(())
    }

    /// Types `text`, with the lab password where it says so.
    fn type_text(&mut self, text: &str) -> Result<Option<String>, StepFailure> {
        let text = if text.contains(PASSWORD_TOKEN) {
            // The run refuses a test that types the password when there is
            // none, before it starts.
            let password =
                self.shared.password.ok_or_else(|| StepFailure::Error(NO_PASSWORD.into()))?;
            text.replace(PASSWORD_TOKEN, password)
        } else {
            text.to_string()
        };
        let combos = screen::key_combos(&text).map_err(StepFailure::Error)?;
        self.driver.press_keys(&combos).map(|()| None).map_err(StepFailure::Error)
    }

    /// The text on the machine's screen now, a string a row.
    fn read_screen(&mut self) -> Result<Vec<String>, String> {
        let fonts = self.shared.fonts.ok_or(NO_FONTS)?;
        // Where the cells are, as this boot said last; as the log said
        // last, if this boot has not said yet.
        let geometry = screen::Geometry::from_log(self.serial.this_boot())
            .or_else(|| screen::Geometry::from_log(self.serial.whole()))
            .ok_or("the serial console has not said where the console's cells are (\"console of ... cells with font ...\")")?;
        if let Some(folder) = self.screen_file.parent() {
            fs::create_dir_all(folder)
                .map_err(|err| format!("making {}: {err}", folder.display()))?;
        }
        self.driver.screen_dump(&self.screen_file)?;
        let bytes = fs::read(&self.screen_file)
            .map_err(|err| format!("reading the screen {}: {err}", self.screen_file.display()))?;
        screen::read_text(&screen::Image::parse_ppm(&bytes)?, geometry, fonts)
    }

    /// Waits until the screen shows every one of `texts` (`shown`), or no
    /// longer shows the one text (not `shown`). The serial console is
    /// faster than drawing, the more so under the emulator, so it looks
    /// again until the time limit.
    fn wait_screen(
        &mut self,
        texts: &[String],
        shown: bool,
        deadline: Instant,
    ) -> Result<Option<String>, StepFailure> {
        if self.shared.fonts.is_none() {
            return Err(StepFailure::Skip(NO_FONTS.into()));
        }
        let last = loop {
            if !self.driver.is_on() {
                return Err(StepFailure::Fail(
                    "the machine turned off while the step watched its screen".into(),
                ));
            }
            let read = self.read_screen();
            if let Ok(screen) = &read
                && screen::shows(screen, texts) == shown
            {
                return Ok(Some(screen::as_text(screen)));
            }
            if Instant::now() >= deadline {
                break read;
            }
            if self.shared.stop.requested() {
                return Err(StepFailure::Skip(STOPPED_DURING.into()));
            }
            std::thread::sleep(Duration::from_millis(500));
        };
        match last {
            // The first line names only the text, so the same miss is one
            // finding however the rest of the screen looked.
            Ok(screen) if shown => {
                let missing: Vec<String> = texts
                    .iter()
                    .filter(|text| !screen::shows(&screen, std::slice::from_ref(*text)))
                    .map(|text| format!("\"{text}\""))
                    .collect();
                Err(StepFailure::Fail(format!(
                    "the screen did not show {}; it showed:\n{}",
                    missing.join(", "),
                    screen::as_text(&screen)
                )))
            }
            Ok(screen) => Err(StepFailure::Fail(format!(
                "the screen still showed \"{}\":\n{}",
                texts[0],
                screen::as_text(&screen)
            ))),
            Err(why) => Err(StepFailure::Error(format!("could not read the screen: {why}"))),
        }
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
/// Where a wait for serial text looks.
#[derive(Clone, Copy)]
enum Search {
    /// After the last text a step found, as `serial` steps do.
    AfterLastFound,
    /// Anywhere in the boot going now, as `wait = "ready"` does.
    ThisBoot,
}

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
    /// Where the boot going now starts: the machine's last power on or
    /// reset, or the log's start. `wait = "ready"` looks for the ready
    /// line from here, whatever serial steps found since.
    boot: usize,
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
            boot: 0,
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

    /// A new boot starts at the end of the log as it is now: power on or
    /// reset. Searches start there, and so does the boot.
    fn new_boot(&mut self) {
        self.skip_to_end();
        self.boot = self.cursor;
    }

    /// Searches from the start of the log again, a fresh boot's.
    fn rewind(&mut self) {
        self.cursor = 0;
        self.boot = 0;
    }

    /// The log's text from the start of the boot going now.
    fn this_boot(&mut self) -> &str {
        self.refresh();
        self.text.get(self.boot..).unwrap_or_default()
    }

    /// The log's whole text, every boot of this log.
    fn whole(&mut self) -> &str {
        self.refresh();
        &self.text
    }

    /// Whether `needle` has appeared anywhere in the boot going now. It
    /// moves nothing, so a serial step that already found the same text
    /// does not hide it.
    fn seen_this_boot(&mut self, needle: &str) -> bool {
        self.refresh();
        self.text.get(self.boot..).is_some_and(|boot| boot.contains(needle))
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

    #[test]
    fn the_ready_line_counts_anywhere_in_the_boot_going_now() {
        let dir = crate::testing::TempDir::new("serial-boot");
        let path = dir.write("serial.log", "Boot complete.\n");
        let mut watch = SerialWatch::new(path.clone());
        // A serial step found the ready line; waiting for ready still sees it.
        assert!(watch.find("Boot complete."));
        assert!(watch.seen_this_boot("Boot complete."));
        // A test sharing the boot starts after it, and still sees it.
        watch.skip_to_end();
        assert!(watch.seen_this_boot("Boot complete."));
        // After power on, only the new boot's own counts.
        watch.new_boot();
        assert!(!watch.seen_this_boot("Boot complete."));
        fs::write(&path, "Boot complete.\nRadix\nBoot complete.\n").unwrap();
        assert!(watch.seen_this_boot("Boot complete."));
        assert!(watch.find("Boot complete."), "the serial step's own place is untouched");
    }

    #[test]
    fn a_new_build_does_not_change_a_run_s_image() {
        let dir = crate::testing::TempDir::new("pin-image");
        let image = dir.write("out/dukovo-lab.img", "build one");
        let pinned = pin_image(&image, &dir.path().join("work/run"), "under-test");
        assert_ne!(pinned, image);
        // `ob image` makes a new file where the old one was.
        fs::remove_file(&image).unwrap();
        fs::write(&image, "build two").unwrap();
        assert_eq!(fs::read_to_string(&pinned).unwrap(), "build one");
        assert_eq!(pinned.extension().unwrap(), "img");
        // No image there: the run uses the path it was given, and fails
        // on it as before.
        let missing = dir.path().join("none.img");
        assert_eq!(pin_image(&missing, &dir.path().join("work/other"), "under-test"), missing);
    }
}

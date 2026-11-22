// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Running tests: the picked tests on each picked machine, the machines
//! side by side.
//!
//! Each machine gets a worker thread that runs the tests in order. A pool
//! (`pool:<name>`, see machines.rs) is the exception: its machines are
//! interchangeable, so they share one queue of the pool's tests, longest
//! first, and each worker takes the next test when it is free. A test the
//! pool's machines cannot run is skipped once for the pool, and a machine
//! that is benched or will not start leaves the pool without stopping the
//! others. A test
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

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::Error;
use crate::bench::{self, Kit, Row};
use crate::browser;
use crate::bundle::{
    Bundle, MachineRecord, Outcome, PoolRecord, Rerun, ResultRecord, RunOptions, RunRecord, SCHEMA,
    StepRecord, Summary, new_run_id,
};
use crate::clock::Utc;
use crate::driver::{ATTACKER_ON_SEGMENT, Driver, SEGMENT_PREFIX_LENGTH, Setup, TARGET_ON_SEGMENT};
use crate::findings;
use crate::health;
use crate::load;
use crate::machines::{Control, Machine, Pool, Who};
use crate::output::{Event, Reporter, short_note};
use crate::peer::{self, Job, Ports, Readiness, SKIP_STATUS, Token};
use crate::redteam::RedteamKit;
use crate::resume::Resume;
use crate::screen::{self, Font};
use crate::ssh;
use crate::stop::Stop;
use crate::store::Running;
use crate::suite::{
    AttackerKind, Bench, Boot, Browser, Command, Engine, Fit, PASSWORD_TOKEN, PowerAction,
    SAVED_PREFIX, Serve, Step, TARGET_TOKEN, Test, saved_names,
};
use crate::units::{describe_duration, format_duration};

/// What a stopped test's result says; a run that is resumed runs again the
/// tests with these results.
pub const STOPPED_DURING: &str = "the run was stopped during this test";
pub const STOPPED_BEFORE: &str = "the run was stopped before this test";

/// How often a step that is still running reports it is alive (a
/// `step-progress` event), so a long step — a soak, a slow boot — is not
/// silent for minutes on end. Short under test, so the beat can be checked
/// without a slow wait.
const HEARTBEAT: Duration =
    if cfg!(test) { Duration::from_millis(100) } else { Duration::from_secs(30) };
/// Why a `root` step cannot run.
pub const NO_PASSWORD: &str = "this step runs as root, and the lab has no password: run `dukovo-lab key`, then build the image again with `ob image --lab`";

/// The command, run as root on a real machine after a fresh boot, that
/// grows the root to the end of its disk. The lab image's root is 512 MiB,
/// too little to install a PHP or PostgreSQL bundle on; an
/// installed system's root takes the rest of its disk, and the image the
/// lab writes leaves that room empty. `/system/live/spark/root` names the
/// root's partition, which is the disk's name with a number, or with `p`
/// and a number when the disk's name ends in a digit (`nvme0n1p2`).
/// `storage grow` does nothing when the root already fills the disk.
pub const GROW_ROOT: &str = "sh -c 'part=$(sed -n \"s/^disk //p\" /system/live/spark/root); case \"$part\" in *[0-9]p[0-9]*) disk=${part%p*};; *) disk=$(echo \"$part\" | sed \"s/[0-9]*$//\");; esac; storage grow \"$disk\"'";

/// The command, given the lab PC's time in UTC, that sets a real machine's
/// clock after a fresh boot ([`crate::driver::Driver::sets_clock`]). The
/// lab network reaches no time server, so the machine's clock is whatever
/// its real-time clock kept from the last test.
pub const SET_CLOCK: &str = "timedatectl set-time";

/// The longest the health check's command may take: it reads two numbers.
const HEALTH_TIME: Duration = Duration::from_secs(30);

/// Why a `bench` step cannot run.
pub const NO_KIT: &str = "this step holds Dukovo to Linux, and the run was given no Linux kit: `ob lab` builds one and passes it with --linux";

/// Why a step that reads the screen's text is skipped.
pub const NO_FONTS: &str = "this step reads the screen, and the run was given no console fonts: `ob lab` passes Dukovo's with --fonts";

/// How long a browser step waits for its forward to listen.
const FORWARD_WAIT: Duration = Duration::from_secs(15);

/// Why a `browser` step is skipped.
pub const NO_BROWSERS: &str = "this step drives a browser, and the run was given no browser kit: `ob lab` makes one and passes it with --browsers";

/// Why a test that boots the install stick is skipped.
pub const NO_STICK: &str = "this test boots the install stick, and the run was given none: `ob lab` builds one and passes it with --stick";

/// Why a test that boots the install DVD is skipped.
pub const NO_DVD: &str = "this test boots the install DVD, and the run was given none: `ob lab` builds one and passes it with --dvd";

/// Why a `bench` step's numbers are kept but not held to Linux's.
pub const EMULATED: &str = "QEMU's emulator, not the machine, made these numbers (no KVM), so they are kept but not held to Linux's";

/// Why a test that attacks from Linux is skipped when the run has no kit.
pub const NO_REDTEAM: &str = "this test attacks from Linux, and the run was given no red-team kit: `ob lab redteam-kit` builds one and `ob lab` passes it with --redteam";

/// The command that gives a machine the address `ip` on net1, the private
/// network, and sets that port up. Both run in one shell, so that when it
/// is run as root (through `elevate`, which elevates only its first
/// command) the port is set up as root too.
fn address_setup(ip: &str) -> String {
    format!("sh -c 'ip address add {ip}/{SEGMENT_PREFIX_LENGTH} dev net1 && ip link set net1 up'")
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

/// Checks a `run` step's command. An `or_skip` step checks that the
/// machine is of the kind the test needs: a command that ended but did not
/// end or print as the step says skips the test, with what the check found
/// below the step's reason. One SSH could not run, or that was still
/// running at the time limit, fails as any step's does: it says nothing
/// about the machine's kind.
fn check_run(
    command: &Command,
    output: &ssh::Output,
    shown: String,
) -> Result<Option<String>, StepFailure> {
    let checked = check_command(command, output, shown, true);
    let ended = match output.status {
        None => false,
        Some(255) => command.exit == 255,
        Some(_) => true,
    };
    match (checked, &command.or_skip) {
        (Err(StepFailure::Fail(why)), Some(reason)) if ended => {
            Err(StepFailure::Skip(format!("{reason}\n{why}")))
        }
        (checked, _) => checked,
    }
}

/// What to run.
#[derive(Default)]
pub struct Request<'a> {
    pub tests: Vec<&'a Test>,
    /// The machines named on their own, each of which runs every test.
    pub machines: Vec<&'a Machine>,
    /// The pools named, whose machines share the tests out.
    pub pools: Vec<Pool<'a>>,
    /// How long each test took last time on each machine, in milliseconds
    /// by `(machine, test)`, which puts a pool's longest tests first.
    pub typical: HashMap<(String, String), u64>,
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
    /// The folder the Linux kit was read from, for run.json.
    pub linux_folder: Option<PathBuf>,
    /// The Linux the attacker boots from for tests that attack with
    /// `attacker = "linux"`, when given.
    pub redteam: Option<RedteamKit>,
    /// The folder the red-team kit was read from, for run.json.
    pub redteam_folder: Option<PathBuf>,
    /// The console's fonts, for steps that read the screen, when given.
    pub fonts: Option<Vec<Font>>,
    /// The folder the fonts were read from, for run.json.
    pub fonts_folder: Option<PathBuf>,
    /// The browser kit, for steps that drive a browser, when given.
    pub browsers: Option<PathBuf>,
    /// The install stick's image, for tests that boot it, when given.
    pub stick: Option<PathBuf>,
    /// The install DVD's ISO, for tests that boot it, when given.
    pub dvd: Option<PathBuf>,
    /// Dukovo's ob, whose `ob disc-drive` holds a test's music CD in a
    /// QEMU machine, when given.
    pub ob: Option<PathBuf>,
    /// The preset the tests came from, for run.json.
    pub preset: Option<String>,
    /// The schedule that started the run, for run.json.
    pub schedule: Option<String>,
    /// The run this one resumes, with the tests it has left: each machine
    /// runs only the ones named for it.
    pub resume: Option<Resume>,
    /// Whether a test that fails is run once more before the run ends: on
    /// another machine of its pool, a spare first, else on the same machine
    /// afresh. A test that then passes is flaky.
    pub rerun: bool,
    /// The run's id, when the caller chose it (a detached run's parent
    /// does, to name the run before it starts); else a new one.
    pub id: Option<String>,
}

/// How a run went.
pub struct Ended {
    /// The run's id.
    pub id: String,
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
    redteam: Option<&'a RedteamKit>,
    /// Held while a machine measures performance, so that machines run
    /// side by side never time each other's load.
    measuring: &'a Mutex<()>,
    fonts: Option<&'a [Font]>,
    browsers: Option<&'a Path>,
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
    let every: Vec<&Machine> = request
        .machines
        .iter()
        .copied()
        .chain(request.pools.iter().flat_map(|pool| pool.members.iter().copied()))
        .collect();
    let names: Vec<&str> = every.iter().map(|machine| machine.name.as_str()).collect();
    let running = Running::start(&request.store, &id, &names)?;
    let record = RunRecord {
        schema: SCHEMA,
        id: id.clone(),
        harness: format!("dukovo-lab {}", env!("CARGO_PKG_VERSION")),
        started: started.rfc3339(),
        ended: None,
        build: request.build.clone(),
        image: request.image.display().to_string(),
        machines: every
            .iter()
            .map(|machine| MachineRecord {
                name: machine.name.clone(),
                about: machine.about.clone(),
                driver: format!("{:?}", machine.driver).to_lowercase(),
            })
            .collect(),
        pools: request
            .pools
            .iter()
            .map(|pool| PoolRecord {
                name: pool.name.clone(),
                members: pool.members.iter().map(|machine| machine.name.clone()).collect(),
            })
            .collect(),
        tests: request.tests.iter().map(|test| test.id.clone()).collect(),
        preset: request.preset.clone(),
        schedule: request.schedule.clone(),
        resumes: request.resume.as_ref().map(|resume| resume.run.clone()),
        options: Some(RunOptions {
            with_person: request.with_person,
            linux: given(request.linux_folder.as_deref()),
            redteam: given(request.redteam_folder.as_deref()),
            fonts: given(request.fonts_folder.as_deref()),
            browsers: given(request.browsers.as_deref()),
            stick: given(request.stick.as_deref()),
            dvd: given(request.dvd.as_deref()),
            ob: given(request.ob.as_deref()),
        }),
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
        machines: names.clone(),
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
        redteam: request.redteam.as_ref(),
        measuring: &measuring,
        fonts: request.fonts.as_deref(),
        browsers: request.browsers.as_deref(),
        stick: stick.as_deref(),
        dvd: dvd.as_deref(),
    };
    let queues: Vec<Queue<'_>> =
        request.pools.iter().map(|pool| Queue::new(pool, request, &shared)).collect();
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
        let setup = |index: usize, machine: &Machine| Setup {
            image: image.clone(),
            logs: bundle.logs(&machine.name),
            work: work.join(&machine.name),
            // By number, not name: socket paths must stay short.
            sockets: sockets.join(index.to_string()),
            key: request.key.clone(),
            redteam: request.redteam.clone(),
            ob: request.ob.clone(),
            stop: stop.clone(),
        };
        let shared = &shared;
        let mut workers: Vec<_> = request
            .machines
            .iter()
            .enumerate()
            .map(|(index, &machine)| {
                let setup = setup(index, machine);
                let tests: Vec<&Test> = request
                    .tests
                    .iter()
                    .copied()
                    .filter(|test| {
                        request
                            .resume
                            .as_ref()
                            .is_none_or(|resume| resume.has_left(&machine.name, &test.id))
                    })
                    .collect();
                scope.spawn(move || run_machine(machine, &tests, setup, shared, start))
            })
            .collect();
        let mut index = request.machines.len();
        for (pool, queue) in request.pools.iter().zip(&queues) {
            for &machine in &pool.members {
                let setup = setup(index, machine);
                index += 1;
                workers.push(
                    scope.spawn(move || run_pool_machine(machine, queue, setup, shared, start)),
                );
            }
        }
        let mut results: Vec<ResultRecord> = workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("a machine's worker panicked"))
            .collect();
        for queue in &queues {
            results.extend(queue.finish(shared));
        }
        if request.rerun && !stop.requested() {
            let rerun_setup = |index: usize, machine: &Machine| Setup {
                logs: bundle.logs(&format!("{}/rerun", machine.name)),
                work: work.join(&machine.name).join("rerun"),
                sockets: sockets.join(format!("rerun-{index}")),
                ..setup(index, machine)
            };
            rerun_failures(&mut results, request, shared, &rerun_setup, start);
        }
        results
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
    Ok(Ended { id, summary })
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

/// `path` as run.json names it, when one was given.
fn given(path: Option<&Path>) -> Option<String> {
    path.map(|path| path.display().to_string())
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

/// Starts a machine's driver and makes its worker; the error says the
/// machine did not start, as a result's reason does.
fn start_worker<'a>(
    machine: &'a Machine,
    setup: Setup,
    shared: &'a Shared<'a>,
    start: &StartDriver,
    rerun: bool,
) -> Result<Worker<'a>, String> {
    let has_key = setup.key.is_some();
    let screen_file = setup.work.join("screen.ppm");
    // The tests' folders on the lab PC go in the short socket folder, as
    // servers run in them make sockets of their own.
    let peer_folder = setup.sockets.join("peer");
    let driver = start(machine, setup)
        .map_err(|err| format!("the {} machine did not start: {err}", machine.name))?;
    Ok(Worker::new(machine, driver, has_key, shared, screen_file, peer_folder, rerun))
}

/// Why `test` cannot run on `machine` however the run goes: the machine
/// lacks a control it needs, a Needs you test is run without a person, or
/// the lab PC has no program the test runs there.
fn cannot_run(test: &Test, machine: &Machine, with_person: bool) -> Option<String> {
    let skip = match test.fit(machine) {
        Fit::NeedsPerson(_) if with_person => None,
        fit => fit.skip_reason(),
    };
    // A lab PC without a server the test runs on it cannot do its side.
    skip.or_else(|| {
        let missing = peer::missing_programs(&test.host_needs);
        (!missing.is_empty()).then(|| {
            format!(
                "the lab PC has no {}, which this test runs there: install it to run the test",
                missing.join(", ")
            )
        })
    })
}

/// Runs every test on one machine, in order.
fn run_machine(
    machine: &Machine,
    tests: &[&Test],
    setup: Setup,
    shared: &Shared<'_>,
    start: &StartDriver,
) -> Vec<ResultRecord> {
    let (mut worker, broken) = match start_worker(machine, setup, shared, start, false) {
        Ok(worker) => (Some(worker), None),
        Err(why) => (None, Some(why)),
    };
    let mut results = Vec::new();
    for &test in tests {
        let skip = if shared.stop.requested() {
            Some(STOPPED_BEFORE.to_string())
        } else {
            cannot_run(test, machine, shared.with_person)
        };
        // A benched machine is not given the rest of the run's tests.
        let skip = skip.or_else(|| worker.as_ref().and_then(|worker| worker.benched.clone()));
        let ran_here = skip.is_none() && worker.is_some();
        let result = match (skip, &mut worker) {
            (Some(reason), _) => skipped(test, &machine.name, reason),
            (None, Some(worker)) => worker.run_test(test),
            (None, None) => errored(test, machine, broken.clone().unwrap_or_default()),
        };
        // A test that ran reported its own end.
        if !ran_here {
            report_unrun(shared, &result);
        }
        results.push(result);
    }
    if let Some(worker) = &mut worker {
        worker.driver.shut_off();
    }
    results
}

/// The tests to run again on one machine: each with the index of its first
/// result.
struct RerunGroup<'a> {
    machine: &'a Machine,
    jobs: Vec<(usize, &'a Test)>,
}

/// Runs each test that failed once more, once every machine is done, and
/// records the second try beside the first. A test of a pool goes to
/// another of its machines, a spare first; a test of a machine named on
/// its own, and of a pool with no other machine, goes to the same machine
/// on a fresh boot. Errors are not rerun: they say the lab broke, not that
/// Dukovo did.
fn rerun_failures(
    results: &mut [ResultRecord],
    request: &Request<'_>,
    shared: &Shared<'_>,
    setup: &dyn Fn(usize, &Machine) -> Setup,
    start: &StartDriver,
) {
    // A spare this run borrowed holds its lock until the reruns are done.
    let mut borrowed: Vec<(&str, File)> = Vec::new();
    // Per machine, the tests to run again and the machine each failed on.
    let mut groups: BTreeMap<&str, RerunGroup<'_>> = BTreeMap::new();
    let failed = results.iter().enumerate().filter(|(_, result)| result.result == Outcome::Fail);
    for (turn, (index, result)) in failed.enumerate() {
        let Some(test) = request.tests.iter().find(|test| test.id == result.test) else { continue };
        let Some(machine) = rerun_machine(request, shared, &result.machine, turn, &mut borrowed)
        else {
            continue;
        };
        groups
            .entry(&machine.name)
            .or_insert_with(|| RerunGroup { machine, jobs: Vec::new() })
            .jobs
            .push((index, test));
    }
    let reruns: Vec<(usize, Rerun)> = std::thread::scope(|scope| {
        let workers: Vec<_> = groups
            .into_values()
            .enumerate()
            .map(|(number, RerunGroup { machine, jobs })| {
                let jobs: Vec<(usize, &Test, &str)> = jobs
                    .into_iter()
                    .map(|(index, test)| (index, test, results[index].machine.as_str()))
                    .collect();
                let setup = setup(number, machine);
                scope.spawn(move || rerun_on(machine, &jobs, setup, shared, start))
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("a machine's worker panicked"))
            .collect()
    });
    for (index, rerun) in reruns {
        results[index].rerun = Some(rerun);
    }
}

/// The machine a test that failed on `first` runs on again.
fn rerun_machine<'a>(
    request: &Request<'a>,
    shared: &Shared<'_>,
    first: &str,
    turn: usize,
    borrowed: &mut Vec<(&'a str, File)>,
) -> Option<&'a Machine> {
    if let Some(&machine) = request.machines.iter().find(|machine| machine.name == first) {
        return Some(machine);
    }
    let pool = request.pools.iter().find(|pool| pool.members.iter().any(|m| m.name == first))?;
    for &spare in &pool.spares {
        // One named on its own is this run's already.
        let named = request.machines.iter().any(|machine| machine.name == spare.name);
        let held = borrowed.iter().any(|(name, _)| *name == spare.name);
        if named || held {
            return Some(spare);
        }
        if let Ok(Some(lock)) = shared.running.borrow_machine(&spare.name) {
            borrowed.push((&spare.name, lock));
            return Some(spare);
        }
    }
    let others: Vec<&Machine> =
        pool.members.iter().copied().filter(|machine| machine.name != first).collect();
    if others.is_empty() {
        return pool.members.iter().copied().find(|machine| machine.name == first);
    }
    Some(others[turn % others.len()])
}

/// Runs `jobs` (result index, test, machine it failed on) on `machine`,
/// each on a fresh boot, and answers the second tries.
fn rerun_on(
    machine: &Machine,
    jobs: &[(usize, &Test, &str)],
    setup: Setup,
    shared: &Shared<'_>,
    start: &StartDriver,
) -> Vec<(usize, Rerun)> {
    let mut worker = match start_worker(machine, setup, shared, start, true) {
        Ok(worker) => worker,
        Err(why) => {
            let broke = |&(index, test, first): &(usize, &Test, &str)| {
                let rerun = Rerun {
                    machine: machine.name.clone(),
                    result: Outcome::Error,
                    reason: Some(why.clone()),
                    milliseconds: 0,
                    boot: None,
                    log: None,
                };
                report_rerun(shared, first, test, &rerun);
                (index, rerun)
            };
            return jobs.iter().map(broke).collect();
        }
    };
    let mut reruns = Vec::new();
    for &(index, test, first) in jobs {
        if shared.stop.requested() || worker.benched.is_some() {
            break;
        }
        shared.reporter.report(&Event::RerunStarted { machine: &machine.name, test: &test.id });
        let again = worker.run_test(test);
        let rerun = Rerun {
            machine: machine.name.clone(),
            result: again.result,
            reason: again.reason,
            milliseconds: again.milliseconds,
            boot: again.boot,
            log: again.log,
        };
        report_rerun(shared, first, test, &rerun);
        reruns.push((index, rerun));
    }
    worker.driver.shut_off();
    reruns
}

fn report_rerun(shared: &Shared<'_>, first: &str, test: &Test, rerun: &Rerun) {
    shared.reporter.report(&Event::RerunEnded {
        machine: first,
        test: &test.id,
        on: &rerun.machine,
        result: rerun.result,
        reason: rerun.reason.as_deref(),
        milliseconds: rerun.milliseconds,
        boot: rerun.boot,
        log: rerun.log.as_deref(),
    });
}

/// Reports the end of a test that did not run, which has no worker to say
/// so.
fn report_unrun(shared: &Shared<'_>, result: &ResultRecord) {
    shared.reporter.report(&Event::TestEnded {
        machine: &result.machine,
        test: &result.test,
        result: result.result,
        reason: result.reason.as_deref(),
        milliseconds: 0,
        boot: None,
        log: None,
        screenshots: &[],
    });
}

/// A pool's tests, which its machines take from in turn.
struct Queue<'a> {
    /// The pool as results name it (`pool:<name>`), for a test no machine
    /// of it ran.
    label: String,
    /// The tests no machine has taken yet, longest first.
    tests: Mutex<VecDeque<&'a Test>>,
    /// Why each machine left the pool, in the order they did.
    out: Mutex<Vec<String>>,
    /// The tests the pool's machines cannot run, skipped once for the
    /// pool.
    skipped: Vec<ResultRecord>,
}

impl<'a> Queue<'a> {
    /// Sorts the tests the pool is to run: those it cannot run are skipped
    /// here, once, and the rest are queued longest first by how long they
    /// took last time on the pool's machines. A test with no earlier time
    /// counts as its time limit, so it is started early rather than found
    /// to be slow at the end. Every machine of a pool has the same
    /// controls (see machines.rs), so the first one's fit stands for all.
    fn new(pool: &Pool<'_>, request: &Request<'a>, shared: &Shared<'_>) -> Queue<'a> {
        let label = pool.label();
        let mut tests: Vec<&'a Test> = request
            .tests
            .iter()
            .copied()
            .filter(|test| {
                let resume = request.resume.as_ref();
                resume.is_none_or(|resume| resume.has_left_in_pool(&label, &test.id))
            })
            .collect();
        let mut unrunnable = Vec::new();
        tests.retain(|test| {
            let Some(reason) = cannot_run(test, pool.members[0], shared.with_person) else {
                return true;
            };
            let result = skipped(test, &label, reason);
            report_unrun(shared, &result);
            unrunnable.push(result);
            false
        });
        let last = |test: &Test| {
            pool.members
                .iter()
                .filter_map(|machine| request.typical.get(&(machine.name.clone(), test.id.clone())))
                .max()
                .map_or(test.time, |&milliseconds| Duration::from_millis(milliseconds))
        };
        tests.sort_by_key(|test| Reverse(last(test)));
        Queue {
            label,
            tests: Mutex::new(tests.into()),
            out: Mutex::new(Vec::new()),
            skipped: unrunnable,
        }
    }

    /// The next test for a machine that is free, if any is left.
    fn next(&self) -> Option<&'a Test> {
        self.tests.lock().unwrap_or_else(PoisonError::into_inner).pop_front()
    }

    /// Takes `machine` out of the pool for the rest of the run, and says
    /// why. The others carry on with the queue.
    fn leave(&self, machine: &Machine, reason: String, shared: &Shared<'_>) {
        shared.reporter.report(&Event::MachineOut { machine: &machine.name, reason: &reason });
        self.out.lock().unwrap_or_else(PoisonError::into_inner).push(reason);
    }

    /// The pool's results that no machine produced, once its machines are
    /// done: the tests it cannot run, and what is left in the queue, which
    /// only a stop or every machine being out leaves.
    fn finish(&self, shared: &Shared<'_>) -> Vec<ResultRecord> {
        let mut results = self.skipped.clone();
        let left: Vec<&Test> =
            self.tests.lock().unwrap_or_else(PoisonError::into_inner).drain(..).collect();
        let out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        for test in left {
            let reason = if shared.stop.requested() {
                STOPPED_BEFORE.to_string()
            } else {
                format!(
                    "no machine of {} was left to run this test: {}",
                    self.label,
                    out.join("; ")
                )
            };
            let result = skipped(test, &self.label, reason);
            report_unrun(shared, &result);
            results.push(result);
        }
        results
    }
}

/// Takes tests from a pool's queue on one of its machines until the queue
/// is empty, the run is stopped, or the machine leaves the pool: it will
/// not start, or its health check benches it.
fn run_pool_machine(
    machine: &Machine,
    queue: &Queue<'_>,
    setup: Setup,
    shared: &Shared<'_>,
    start: &StartDriver,
) -> Vec<ResultRecord> {
    let mut worker = match start_worker(machine, setup, shared, start, false) {
        Ok(worker) => worker,
        Err(why) => {
            queue.leave(machine, why, shared);
            return Vec::new();
        }
    };
    let mut results = Vec::new();
    while worker.benched.is_none() && !shared.stop.requested() {
        let Some(test) = queue.next() else { break };
        results.push(worker.run_test(test));
    }
    if let Some(why) = worker.benched.clone() {
        queue.leave(machine, why, shared);
    }
    worker.driver.shut_off();
    results
}

fn skipped(test: &Test, machine: &str, reason: String) -> ResultRecord {
    blank_result(test, machine, Outcome::Skip, reason)
}

fn errored(test: &Test, machine: &Machine, reason: String) -> ResultRecord {
    blank_result(test, &machine.name, Outcome::Error, reason)
}

fn blank_result(test: &Test, machine: &str, result: Outcome, reason: String) -> ResultRecord {
    ResultRecord {
        test: test.id.clone(),
        machine: machine.to_string(),
        result,
        reason: Some(reason),
        milliseconds: 0,
        boot: None,
        log: None,
        steps: Vec::new(),
        screenshots: Vec::new(),
        rerun: None,
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
    /// Where the machine's logs and screenshots go under `logs/` and
    /// `screenshots/`: its name, or a folder inside it for a rerun, whose
    /// files must not replace the first try's.
    folder: String,
    /// Whether this worker only reruns tests that failed, and so reports
    /// no test of its own (see [`Event::RerunStarted`]).
    rerun: bool,
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
    /// What the test's steps kept with `save`, by name.
    saved: BTreeMap<String, String>,
    /// Whether the test being run attacks from the red-team kit's Linux
    /// instead of a Dukovo guest.
    linux_attacker: bool,
    /// The test's side on the lab PC: see [`PeerSide`].
    peer: PeerSide,
    /// How host steps run as root, found the first time one does (as
    /// they are, or through sudo), or why they cannot.
    host_root: Option<Result<Vec<String>, String>>,
    /// How many fresh boots in a row failed the health check.
    unhealthy: u32,
    /// Why the machine is benched, once its health check has failed
    /// [`health::FAILURES_TO_BENCH`] fresh boots in a row: the rest of its
    /// tests are skipped with this.
    benched: Option<String>,
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
    /// Servers started by `background` steps, and the forwards browser
    /// steps open pages through.
    jobs: Vec<Job>,
    /// The lab PC's loopback port forwarded to each machine port a browser
    /// step opened.
    forwards: BTreeMap<u16, u16>,
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
        rerun: bool,
    ) -> Self {
        Worker {
            machine,
            folder: if rerun { format!("{}/rerun", machine.name) } else { machine.name.clone() },
            rerun,
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
            saved: BTreeMap::new(),
            linux_attacker: false,
            peer: PeerSide { base: peer_folder, ..PeerSide::default() },
            host_root: None,
            unhealthy: 0,
            benched: None,
        }
    }

    /// Runs one test, from its boot (if it needs one) to its last step.
    fn run_test(&mut self, test: &Test) -> ResultRecord {
        let machine = &self.machine.name;
        if !self.rerun {
            self.shared.reporter.report(&Event::TestStarted { machine, test: &test.id });
        }
        let started = Instant::now();
        let limit = test.time * self.driver.slowdown();
        let mut deadline = started + limit;
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
        self.saved.clear();
        self.linux_attacker = attacks && test.attacker == AttackerKind::Linux;
        self.driver.want_linux_attacker(self.linux_attacker);
        // So does a test that puts cards in a card reader, whose reader,
        // an empty drive, a disk test must not find either.
        let reader = test.card_reader();
        self.driver.want_card_reader(reader);
        let cards = reader.is_some();
        // And one that puts discs in an optical drive, as empty, blank
        // ones to burn among them, or floppy disks in a floppy drive.
        let discs = [Control::Disc, Control::Burner]
            .iter()
            .any(|control| test.controls().contains(control));
        self.driver.want_disc_drive(discs);
        let floppies = test.controls().contains(&Control::Floppy);
        self.driver.want_floppy_drive(floppies);
        // And one that asks for a virtual machine's devices: a guest agent
        // channel the agent answers on, or a SCSI disk.
        self.driver.want_hardware(&test.hardware);
        // And one that formats a CompactFlash card, given one blank.
        self.driver.want_cf_card(test.cf);
        let own_boot =
            attacks || cards || discs || floppies || test.cf.is_some() || !test.hardware.is_empty();
        let installs = matches!(test.boot, Boot::Stick { .. } | Boot::Dvd { .. });
        // A machine that is its own twin boots Linux before the test's boot,
        // so the steps before the bench step (a profile they set) run on
        // the boot the bench uses, and the bench step has no boot to end.
        let measures_twin =
            self.linux.is_none() && test.steps.iter().any(|step| matches!(step, Step::Bench(_)));
        let twin_kit = self.shared.linux.filter(|_| measures_twin && self.driver.twin_ends_boot());
        let twin_first = twin_kit.is_some();
        let fresh = !installs
            && (test.fresh || own_boot || twin_first || !self.reusable || !self.driver.is_on());
        // A real machine's disk is written before each fresh boot, and
        // Linux's boot on the machine itself takes the same time: that time
        // is the machine's, not the test's.
        if fresh {
            deadline += self.driver.boot_time();
        }
        if twin_first {
            deadline += self.driver.boot_time();
        }
        // The kit's benchmarks run on the test's clock, but their time is
        // the kit's, not the test's.
        if let Some(kit) = self.shared.linux.filter(|_| measures_twin) {
            deadline += kit.time * self.driver.slowdown();
        }

        let mut failure = None;
        // Whether the test was skipped at its first step, a check that the
        // machine fits it, which leaves the boot as it found it.
        let mut unfit = false;
        if let Some(kit) = twin_kit {
            let (record, outcome) =
                self.timed(test, "boot Linux on the machine and measure it", true, |worker| {
                    worker.measure_twin_first(kit, deadline)
                });
            steps.push(record);
            failure = outcome.err();
        }
        if failure.is_none() {
            if let Boot::Stick { disk } = test.boot {
                let (record, outcome) = self.timed(
                    test,
                    "boot from the install stick, with a blank disk",
                    false,
                    |worker| worker.boot_stick(disk),
                );
                steps.push(record);
                failure = outcome.err();
            } else if let Boot::Dvd { disk } = test.boot {
                let (record, outcome) = self.timed(
                    test,
                    "boot from the install DVD, with a blank disk",
                    false,
                    |worker| worker.boot_dvd(disk),
                );
                steps.push(record);
                failure = outcome.err();
            } else if fresh {
                let (record, outcome) =
                    self.timed(test, "boot afresh and wait until booted", false, |worker| {
                        worker.boot_fresh(deadline)
                    });
                steps.push(record);
                failure = outcome.err();
            } else {
                self.log.push(format!("Sharing boot {} with the test before.", self.boot));
                self.serial.skip_to_end();
            }
        }
        for (index, step) in test.steps.iter().enumerate() {
            if failure.is_some() {
                break;
            }
            if self.shared.stop.requested() {
                failure = Some(StepFailure::Skip(STOPPED_DURING.into()));
                break;
            }
            // The lab PC does its side of these, so they are slow when it
            // is busy.
            let times_matter = matches!(step, Step::Bench(_) | Step::Browser(_));
            let left_out = test.optional_steps.contains(&index).then(|| self.left_out(step));
            let (record, outcome) = self.timed(test, &step.describe(), times_matter, |worker| {
                match left_out.flatten() {
                    Some(note) => Ok(Some(note)),
                    None => worker.run_step(test, step, deadline),
                }
            });
            steps.push(record);
            self.write_progress(test, index + 1);
            unfit = index == 0
                && matches!(step, Step::Run(command) if command.or_skip.is_some())
                && matches!(outcome, Err(StepFailure::Skip(_)));
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
        // boot the card reader, a disc test's the optical drive, a floppy
        // test's the floppy drive and a CompactFlash test's the card, so
        // they are not shared either. A test skipped at its first step, as
        // the machine does not fit it, changed nothing: its boot goes on.
        self.reusable = (result == Outcome::Pass || unfit && result == Outcome::Skip)
            && self.driver.is_on()
            && test.boot == Boot::Image
            && !own_boot;
        if self.reusable {
            self.take_out_media();
        }
        let log = self.write_log(test, result, reason.as_deref());
        let milliseconds = started.elapsed().as_millis() as u64;
        let boot = (self.boot > 0).then_some(self.boot);
        let screenshots = std::mem::take(&mut self.screenshots);
        if !self.rerun {
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
        }
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
            rerun: None,
        }
    }

    /// Takes out what the test left in the machine's ports and drives, which
    /// the next test, sharing the boot, would otherwise find in the way.
    /// A boot where something would not come out is not shared: a fresh
    /// boot has the machine's ports empty again.
    fn take_out_media(&mut self) {
        for outcome in self.driver.take_out_media() {
            match outcome {
                Ok(what) => {
                    self.log.push(format!("The test left {what}; the lab took it out."));
                }
                Err(why) => {
                    self.log.push(format!(
                        "The lab could not take out what the test left in ({why}); the next test boots afresh."
                    ));
                    self.reusable = false;
                }
            }
        }
    }

    /// Runs `action` as a step of `test` named `what`, logging, timing
    /// and reporting it, with the lab PC's load as it ended. When
    /// `times_matter` and that load is [`load::BUSY`], the step's note and
    /// the log say the step's times may be slow; its result is not changed.
    fn timed(
        &mut self,
        test: &Test,
        what: &str,
        times_matter: bool,
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
        let lab_pc_load = load::now();
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
        let busy = load::busy_line(lab_pc_load).filter(|_| times_matter && result != Outcome::Skip);
        let note = match (note, &busy) {
            (Some(note), Some(busy)) => Some(format!("{note}\n{busy}")),
            (None, Some(busy)) => Some(busy.clone()),
            (note, None) => note,
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
            lab_pc_load,
        });
        (StepRecord { step: what.into(), result, milliseconds, note, lab_pc_load }, outcome)
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
        let grown = self.grow_root(deadline)?;
        self.set_clock(deadline)?;
        self.check_health(deadline)?;
        // The test that boots sees its whole boot: its serial steps may
        // check what came before the ready line.
        self.serial.rewind();
        Ok(match (note, grown) {
            (Some(note), Some(grown)) => Some(format!("{note}; {grown}")),
            (note, grown) => note.or(grown),
        })
    }

    /// Grows the root of a real machine to the end of its disk, as the
    /// first thing after a fresh boot, so that no test meets the lab
    /// image's small root (see [`GROW_ROOT`]). Gives back what the command
    /// printed, which the driver's log also holds. A root that does not
    /// grow is an error: tests that fill it would fail for the lab's sake.
    /// A machine with no SSH channel or no lab key has nothing to run it
    /// with, as with [`Worker::wait_ready`]'s SSH wait.
    fn grow_root(&mut self, deadline: Instant) -> Result<Option<String>, StepFailure> {
        if !self.driver.grows_root()
            || self.machine.who(Control::Ssh) != Who::Driver
            || !self.has_key
        {
            return Ok(None);
        }
        if self.shared.password.is_none() {
            return Err(StepFailure::Error(
                "growing the root needs the lab's password, and the lab has none: run \
                 `dukovo-lab key`, then build the image again with `ob image --lab`, or set \
                 grow_root = false in the machine file"
                    .into(),
            ));
        }
        let (output, shown) = self.execute(GROW_ROOT, true, None, deadline)?;
        self.driver.note(&format!("grow root: {shown}"));
        match output.status {
            Some(0) => Ok(Some(shown)),
            Some(status) => Err(StepFailure::Error(format!(
                "the root could not be grown to the end of the disk (exit status {status}): {}",
                output.text.trim()
            ))),
            None => Err(StepFailure::Error(
                "the root was not grown to the end of the disk before the time limit".into(),
            )),
        }
    }

    /// Sets a real machine's clock to the lab PC's, as root, after a fresh
    /// boot (see [`SET_CLOCK`]), on the same terms as [`Worker::grow_root`].
    /// A clock that cannot be set is an error: the tests after it would
    /// fail on certificates and signatures for the lab's sake.
    fn set_clock(&mut self, deadline: Instant) -> Result<(), StepFailure> {
        if !self.driver.sets_clock()
            || self.machine.who(Control::Ssh) != Who::Driver
            || !self.has_key
            || self.shared.password.is_none()
        {
            return Ok(());
        }
        let command = format!("{SET_CLOCK} '{}'", crate::clock::Utc::now().timedatectl());
        let (output, shown) = self.execute(&command, true, None, deadline)?;
        self.driver.note(&format!("set clock: {shown}"));
        match output.status {
            Some(0) => Ok(()),
            Some(status) => Err(StepFailure::Error(format!(
                "the machine's clock could not be set to the lab PC's (exit status {status}): {}",
                output.text.trim()
            ))),
            None => Err(StepFailure::Error(
                "the machine's clock was not set before the time limit".into(),
            )),
        }
    }

    /// Checks a real machine's health once after a fresh boot, when the
    /// root has grown and the clock is set (see health.rs), and puts what
    /// it found in the test's log. A machine that fails is a lab error for
    /// this test; one that fails [`health::FAILURES_TO_BENCH`] fresh boots
    /// in a row is benched, and its remaining tests are skipped. A machine
    /// with no SSH channel or no lab key has nothing to ask, as with
    /// [`Worker::grow_root`].
    fn check_health(&mut self, deadline: Instant) -> Result<(), StepFailure> {
        if !self.driver.checks_health()
            || self.machine.who(Control::Ssh) != Who::Driver
            || !self.has_key
        {
            return Ok(());
        }
        let until = deadline.min(Instant::now() + HEALTH_TIME);
        let (output, shown) = self.execute(health::COMMAND, false, None, until)?;
        let judgement = match output.status {
            Some(0) => health::judge(
                &health::read(&output.text),
                Utc::now().seconds(),
                self.driver.grows_root(),
            ),
            status => {
                let status = status.map_or("no answer".to_string(), |status| status.to_string());
                health::Judgement {
                    summary: format!("the check did not run (exit status: {status})"),
                    faults: vec![health::Fault {
                        found: format!("the check did not run on the machine: {shown}"),
                        advice: "check that the machine answers SSH, then run again".into(),
                    }],
                }
            }
        };
        let verdict = if judgement.faults.is_empty() { "ok" } else { "FAILED" };
        let line =
            format!("Health check after boot {}: {}: {verdict}", self.boot, judgement.summary);
        self.log.push(line.clone());
        self.driver.note(&line);
        if judgement.faults.is_empty() {
            self.unhealthy = 0;
            return Ok(());
        }
        self.unhealthy += 1;
        let found: Vec<&str> = judgement.faults.iter().map(|fault| fault.found.as_str()).collect();
        if self.unhealthy >= health::FAILURES_TO_BENCH {
            self.benched = Some(format!(
                "{} was benched: its health check failed twice ({})",
                self.machine.name,
                found.join(", ")
            ));
            self.log.push(format!("{} is benched for the rest of the run.", self.machine.name));
        }
        let advice: Vec<&str> =
            judgement.faults.iter().map(|fault| fault.advice.as_str()).collect();
        Err(StepFailure::Error(format!(
            "health check failed after the fresh boot: {}. {}",
            found.join("; "),
            advice.join(" ")
        )))
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
        self.wait_serial_where(&ready, Search::ThisBoot, None, deadline)?;
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
            match self.driver.run_command("true", None, attempt, None) {
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
            Step::Run(command) => self.run_command(command, step.stall_limit(test), deadline),
            Step::Attack(command) => self.run_attack(command, deadline),
            Step::Host(command) => self.run_host(command, deadline),
            Step::Browser(browser) => self.run_browser(test, browser, deadline),
            Step::Serve(serve) => self.serve(test, serve, deadline),
            Step::Bench(bench) => self.bench(bench, deadline),
            Step::Serial { text, .. } => self.wait_serial(text, step.stall_limit(test), deadline),
            Step::Power(action) => self.power(*action, deadline),
            Step::Link(up) => self.driver.set_link(*up).map(|()| None).map_err(StepFailure::Error),
            Step::PlugUsb(stick) => {
                self.driver.plug_usb(stick).map(|()| None).map_err(StepFailure::Error)
            }
            Step::UnplugUsb => self.driver.unplug_usb().map(|()| None).map_err(StepFailure::Error),
            // The boot gave the machine the reader the test's steps name.
            Step::InsertCard(_, card) => {
                self.driver.insert_card(card).map(|()| None).map_err(StepFailure::Error)
            }
            Step::RemoveCard(_) => {
                self.driver.remove_card().map(|()| None).map_err(StepFailure::Error)
            }
            Step::ReinsertCard(_) => {
                self.driver.reinsert_card().map(|()| None).map_err(StepFailure::Error)
            }
            Step::InsertDisc(image) => {
                self.driver.insert_disc(image).map(|()| None).map_err(StepFailure::Error)
            }
            Step::InsertBlankDisc(disc) => {
                self.driver.insert_blank_disc(*disc).map(|()| None).map_err(StepFailure::Error)
            }
            Step::EjectDisc => self.driver.eject_disc().map(|()| None).map_err(StepFailure::Error),
            Step::ReinsertDisc => {
                self.driver.reinsert_disc().map(|()| None).map_err(StepFailure::Error)
            }
            Step::PressDiscButton => {
                self.driver.press_disc_button().map(|()| None).map_err(StepFailure::Error)
            }
            Step::InsertFloppy(floppy) => {
                self.driver.insert_floppy(floppy).map(|()| None).map_err(StepFailure::Error)
            }
            Step::EjectFloppy => {
                self.driver.eject_floppy().map(|()| None).map_err(StepFailure::Error)
            }
            Step::ReinsertFloppy => {
                self.driver.reinsert_floppy().map(|()| None).map_err(StepFailure::Error)
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

    /// Runs a `run` step's command. It ends as stalled when it prints
    /// nothing for `stall` while the console shows no new line: that is
    /// Dukovo hanging, so it fails the test, where waiting out the time
    /// limit would cost the test's whole time.
    fn run_command(
        &mut self,
        command: &Command,
        stall: Option<Duration>,
        deadline: Instant,
    ) -> Result<Option<String>, StepFailure> {
        let command = self.fill_command(command)?;
        let input = self.fill_input(&command)?;
        let watch = stall
            .map(|after| ssh::Stall { after, console: self.driver.serial_log().to_path_buf() });
        let (output, shown) = self.execute_watched(
            &command.command,
            command.root,
            input.as_deref(),
            watch.as_ref(),
            deadline,
        )?;
        // A command with no status was cut short: by the stop, which
        // `finish` turned into a skip already, by the time limit, or by
        // the stall.
        if let Some(after) = stall.filter(|_| output.status.is_none() && Instant::now() < deadline)
        {
            return Err(self.stalled(after, &shown));
        }
        let checked = check_run(&command, &output, shown)?;
        self.keep(command.save.as_deref(), &output);
        Ok(checked)
    }

    /// The failure of a step that was silent for `after`: the first line
    /// says how long, so the same hang is one finding wherever it is, and
    /// the console's last line and what the step showed follow.
    fn stalled(&mut self, after: Duration, shown: &str) -> StepFailure {
        let last = match self.serial.last_line() {
            Some(line) => format!("the last console line was \"{line}\""),
            None => "the console showed no line".to_string(),
        };
        StepFailure::Fail(format!(
            "stalled: no output for {}\n{last} ({})\n{shown}",
            describe_duration(after),
            self.relative_serial()
        ))
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
        let checked = check_command(&command, &output, shown, false)?;
        self.keep(command.save.as_deref(), &output);
        Ok(checked)
    }

    /// Runs a `browser` step's script in each of its engines on the lab PC,
    /// one after another, and folds how each went into the step's result
    /// (see browser.rs). Every engine runs, so that one that fails does not
    /// hide another's failure. The kit and node are the lab's to have, not
    /// Dukovo's: a run without them skips the step.
    fn run_browser(
        &mut self,
        test: &Test,
        browser: &Browser,
        deadline: Instant,
    ) -> Result<Option<String>, StepFailure> {
        let Some(kit) = self.shared.browsers else {
            return Err(StepFailure::Skip(NO_BROWSERS.into()));
        };
        if !peer::missing_programs(&["node".to_string()]).is_empty() {
            return Err(StepFailure::Skip(
                "this step drives a browser, and the lab PC has no node: install Node.js 22 or later"
                    .into(),
            ));
        }
        let file = self.peer_work()?.join("browser-runner.mjs");
        fs::write(&file, browser::RUNNER)
            .map_err(|err| StepFailure::Error(format!("writing {}: {err}", file.display())))?;
        let shots = self.shared.bundle.screenshots(&self.folder);
        let logs = self.shared.bundle.logs(&self.folder);
        for folder in [&shots, &logs] {
            fs::create_dir_all(folder)
                .map_err(|err| StepFailure::Error(format!("making {}: {err}", folder.display())))?;
        }
        let url = self.browser_url(test, browser.port, deadline)?;
        let machine = self.fill(&format!("{{machine}}:{{machine-port:{}}}", browser.port))?;
        let (kit, script) = (absolute(kit), self.peer.here.join(&browser.script));
        let (absolute_shots, absolute_logs) = (absolute(&shots), absolute(&logs));
        let runner = browser::Runner {
            file: &file,
            kit: &kit,
            url: &url,
            machine: &machine,
            script: &script,
            shots: &absolute_shots,
            logs: &absolute_logs,
            name: &test.id.replace('/', "-"),
        };
        let mut runs = Vec::new();
        let mut shown = Vec::new();
        for &engine in &browser.engines {
            let (outcome, log) = self.run_engine(&runner, engine, deadline)?;
            if let browser::Outcome::Passed { screenshots }
            | browser::Outcome::Failed { screenshots, .. } = &outcome
            {
                for name in screenshots {
                    self.screenshots.push(self.shared.bundle.relative(&shots.join(name)));
                }
            }
            runs.push((engine, outcome));
            shown.push(log);
        }
        let shown = shown.join("\n");
        match browser::verdict(&runs) {
            browser::Verdict::Passed(note) => Ok(Some(format!("{note}\n{shown}"))),
            browser::Verdict::Skipped(reason) => Err(StepFailure::Skip(reason)),
            browser::Verdict::Failed(why) => Err(StepFailure::Fail(format!("{why}\n{shown}"))),
        }
    }

    /// The page address a browser opens for the machine's `port`. A machine
    /// the lab PC reaches by an address is opened through a forward on the
    /// lab PC's loopback, so that the machine's own name for itself,
    /// `localhost`, is the page's. Without the forward the browser is given
    /// the machine's address, and the test's log says why.
    fn browser_url(
        &mut self,
        test: &Test,
        port: u16,
        deadline: Instant,
    ) -> Result<String, StepFailure> {
        let folder = self.peer_work()?;
        match self.forward_for_browser(test, port, &folder, deadline) {
            Ok(Some(local)) => return Ok(format!("https://127.0.0.1:{local}")),
            Ok(None) => {}
            Err(why) => self.log.push(format!(
                "The browser opens the machine by its address, not through a forward to its \
                 loopback: {why}"
            )),
        }
        self.fill(&format!("https://{{machine}}:{{machine-port:{port}}}"))
    }

    /// The lab PC's loopback port forwarded to the machine's `port`, started
    /// the first time a browser step needs it and ended with the test's other
    /// lab PC processes. None when the driver needs no forward.
    fn forward_for_browser(
        &mut self,
        test: &Test,
        port: u16,
        folder: &Path,
        deadline: Instant,
    ) -> Result<Option<u16>, String> {
        if let Some(&local) = self.peer.forwards.get(&port) {
            return Ok(Some(local));
        }
        let local = peer::free_port()?;
        let Some(command) = self.driver.loopback_forward(local, port)? else { return Ok(None) };
        let logs = self.shared.bundle.logs(&self.folder);
        fs::create_dir_all(&logs).map_err(|err| format!("making {}: {err}", logs.display()))?;
        let log = logs.join(format!("{}-forward-{port}.log", test.id.replace('/', "-")));
        self.peer.jobs.push(Job::start(&command, &[], folder, &log)?);
        let job = self.peer.jobs.last_mut().expect("pushed above");
        let until = deadline.min(Instant::now() + FORWARD_WAIT);
        let readiness = job.wait_for_port(local, until, self.shared.stop);
        let tail = job.tail();
        let shown = self.shared.bundle.relative(&log);
        let why = match readiness {
            Readiness::Ready => {
                self.peer.forwards.insert(port, local);
                return Ok(Some(local));
            }
            Readiness::Exited(status) => {
                let status = status.map_or("a signal".to_string(), |s| format!("status {s}"));
                format!("the ssh forward ended ({status}) at once; the end of {shown}:\n{tail}")
            }
            Readiness::TimedOut => {
                format!(
                    "the ssh forward was not listening after {}s; {shown}:\n{tail}",
                    FORWARD_WAIT.as_secs()
                )
            }
            Readiness::Stopped => STOPPED_DURING.to_string(),
        };
        if let Some(mut job) = self.peer.jobs.pop() {
            job.end();
        }
        Err(why)
    }

    /// Runs the runner for `engine` as a host step runs its command, and
    /// gives how it went and how a log shows the run.
    fn run_engine(
        &mut self,
        runner: &browser::Runner<'_>,
        engine: Engine,
        deadline: Instant,
    ) -> Result<(browser::Outcome, String), StepFailure> {
        let command = runner.command(engine);
        let folder = self.peer_work()?;
        let scratch = self.peer.base.join("output");
        let output = peer::run(&command, &[], None, &folder, &scratch, deadline, self.shared.stop)
            .map_err(StepFailure::Error)?;
        let (output, shown) = self.finish(&command, output, "lab-pc $")?;
        let Some(status) = output.status else {
            return Err(StepFailure::Fail("the browser ran past the test's time limit".into()));
        };
        Ok((browser::outcome(status, &output.text), shown))
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
        let logs = self.shared.bundle.logs(&self.folder);
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
            Token::Amt => self.driver.amt_address().ok_or_else(|| {
                StepFailure::Error("this machine's driver has no address for its AMT".into())
            }),
            Token::GuestAgent => self
                .driver
                .guest_agent_socket()
                .map(|socket| socket.display().to_string())
                .ok_or_else(|| {
                    StepFailure::Error("this boot has no QEMU guest agent channel".into())
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

    /// Keeps `output`, trimmed, under `name` for the later steps of the
    /// test, when the step that made it says to.
    fn keep(&mut self, name: Option<&str>, output: &ssh::Output) {
        if let Some(name) = name {
            self.saved.insert(name.to_string(), output.text.trim().to_string());
        }
    }

    /// `text` with each `{saved:NAME}` replaced by what an earlier step
    /// kept under NAME.
    fn fill_saved(&self, text: &str) -> Result<String, StepFailure> {
        let mut filled = text.to_string();
        for name in saved_names(text) {
            let Some(value) = self.saved.get(name) else {
                return Err(StepFailure::Error(format!(
                    "{{saved:{name}}} has no value: no earlier step of this test kept one"
                )));
            };
            filled = filled.replace(&format!("{SAVED_PREFIX}{name}}}"), value);
        }
        Ok(filled)
    }

    /// `text` with its placeholders and saved values filled in.
    fn fill_text(&mut self, text: &str) -> Result<String, StepFailure> {
        let text = self.fill(text)?;
        self.fill_saved(&text)
    }

    /// `command` with the placeholders of its command and checks filled in.
    fn fill_command(&mut self, command: &Command) -> Result<Command, StepFailure> {
        Ok(Command {
            command: self.fill_text(&command.command)?,
            exit: command.exit,
            expect: command
                .expect
                .iter()
                .map(|text| self.fill_text(text))
                .collect::<Result<_, _>>()?,
            expect_not: command
                .expect_not
                .iter()
                .map(|text| self.fill_text(text))
                .collect::<Result<_, _>>()?,
            root: command.root,
            input: command.input.as_deref().map(|input| self.fill_text(input)).transpose()?,
            or_skip: command.or_skip.clone(),
            stall: command.stall,
            save: command.save.clone(),
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
        self.peer.forwards.clear();
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
        let resolve =
            |text: &String| self.fill_saved(&text.replace(TARGET_TOKEN, TARGET_ON_SEGMENT));
        let resolved = Command {
            command: resolve(&command.command)?,
            exit: command.exit,
            expect: command.expect.iter().map(resolve).collect::<Result<_, _>>()?,
            expect_not: command.expect_not.iter().map(resolve).collect::<Result<_, _>>()?,
            root: command.root,
            input: None,
            or_skip: None,
            stall: None,
            save: None,
        };
        let (output, shown) = self.execute_attack(&resolved.command, resolved.root, deadline)?;
        let checked = check_command(&resolved, &output, shown, true)?;
        self.keep(command.save.as_deref(), &output);
        Ok(checked)
    }

    /// Brings the attacker up and gives it and the machine under test their
    /// addresses on the private network, once per boot. Later attack steps
    /// in the same boot find it ready and return at once.
    fn ready_attacker(&mut self, deadline: Instant) -> Result<(), StepFailure> {
        if self.attacker_ready {
            return Ok(());
        }
        if self.linux_attacker && self.shared.redteam.is_none() {
            return Err(StepFailure::Skip(NO_REDTEAM.into()));
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
        // The kit's init gives itself its address from the kernel command
        // line, and has no `elevate` to run this with.
        if !self.linux_attacker {
            let (output, shown) =
                self.execute_attack(&address_setup(ATTACKER_ON_SEGMENT), true, deadline)?;
            if output.status != Some(0) {
                return Err(StepFailure::Error(format!(
                    "could not give the attacker its address on the private network\n{shown}"
                )));
            }
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
        self.execute_watched(command, root, input, None, deadline)
    }

    /// As [`Worker::execute`], ending the command early when `stall` says
    /// it has gone quiet.
    fn execute_watched(
        &mut self,
        command: &str,
        root: bool,
        input: Option<&str>,
        stall: Option<&ssh::Stall>,
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
            .run_command(&line, input.as_deref(), deadline, stall)
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
        let (line, input, prompt) = match self.shared.redteam.filter(|_| self.linux_attacker) {
            // The kit has no `elevate`: a step that needs root runs as the
            // kit's user when that is root, and fails when it is not.
            Some(kit) if root && kit.user != "root" => {
                return Err(StepFailure::Error(format!(
                    "this step needs root, and the red-team kit's attacker runs steps as {}: set user = \"root\" in its kit.toml",
                    kit.user
                )));
            }
            Some(_) => (command.to_string(), None, if root { "#" } else { "$" }),
            None => self.as_root(command, root, None)?,
        };
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
            None => self.measure_twin(kit, self.boot, deadline)?,
        };
        let outcome = self.hold_to_linux(bench, &linux, deadline);
        self.linux = Some(linux);
        outcome
    }

    /// Boots the machine's Linux twin and gives its rows, keeping its
    /// console as `linux-boot-N.log` for Dukovo's boot `boot`, the one it
    /// ran beside or before. The bench step gets here only on a machine
    /// that sets its boot aside for the twin (QEMU), as a machine that is
    /// its own twin measures before the test's boot, in
    /// [`Worker::measure_twin_first`].
    fn measure_twin(
        &mut self,
        kit: &Kit,
        boot: u32,
        deadline: Instant,
    ) -> Result<BTreeMap<String, f64>, StepFailure> {
        let log = self.shared.bundle.logs(&self.folder).join(format!("linux-boot-{boot}.log"));
        let text = self.driver.linux_twin(kit, &log, deadline).map_err(StepFailure::Error)?;
        Ok(bench::parse(&text))
    }

    /// Measures the twin of a machine that is its own twin, before the
    /// test's boot, which Linux's boot on the machine ends.
    fn measure_twin_first(
        &mut self,
        kit: &Kit,
        deadline: Instant,
    ) -> Result<Option<String>, StepFailure> {
        // One machine measures at a time; the others wait here.
        let _measuring = self.shared.measuring.lock().unwrap_or_else(PoisonError::into_inner);
        self.linux = Some(self.measure_twin(kit, self.boot + 1, deadline)?);
        self.log.push("Linux ran on the machine itself; the test boots afresh.".into());
        Ok(None)
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
        let dukovo = bench::parse(&output.text);
        let rows = match bench::compare(&bench.rows, bench.limit, &dukovo, linux) {
            Ok(rows) => rows,
            Err(why) => return fail(why),
        };
        let clocks = bench::clocks(&dukovo, linux);
        let table = match &clocks {
            Some(clocks) => format!("{}\n{}", clocks.line(), bench::table(&rows)),
            None => bench::table(&rows),
        };
        if self.driver.emulated() {
            return Err(StepFailure::Skip(format!("{EMULATED}\n{table}")));
        }
        let printed = bench::printed(&output.text);
        // Times taken at different clocks say nothing about the systems, so
        // the step is neither a pass nor a finding.
        if let Some(clocks) = clocks.filter(|clocks| !clocks.agree()) {
            return Err(StepFailure::Error(format!("{}\n{table}{printed}", clocks.disagreement())));
        }
        if rows.iter().all(Row::passes) {
            Ok(Some(format!("{table}{printed}")))
        } else {
            // The first line names no number, so a row that comes back over
            // the limit joins the same finding, whatever it measured.
            Err(StepFailure::Fail(format!(
                "slower than Linux: the rows marked over take more than their share of Linux's time\n{table}{printed}"
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

    /// Waits for `text` on the serial console after the last text found,
    /// ending as stalled when the console shows no new line for `stall`.
    fn wait_serial(
        &mut self,
        text: &str,
        stall: Option<Duration>,
        deadline: Instant,
    ) -> Result<Option<String>, StepFailure> {
        self.wait_serial_where(text, Search::AfterLastFound, stall, deadline)
    }

    /// Waits for `text` where `search` says. A boot's wait passes no
    /// `stall`: a machine may take long to show its first lines.
    fn wait_serial_where(
        &mut self,
        text: &str,
        search: Search,
        stall: Option<Duration>,
        deadline: Instant,
    ) -> Result<Option<String>, StepFailure> {
        let found = |serial: &mut SerialWatch| match search {
            Search::AfterLastFound => serial.find(text),
            Search::ThisBoot => serial.seen_this_boot(text),
        };
        let (mut seen, mut moved) = (self.serial.read, Instant::now());
        loop {
            if found(&mut self.serial) {
                return Ok(None);
            }
            if self.serial.read != seen {
                (seen, moved) = (self.serial.read, Instant::now());
            }
            if let Some(after) = stall
                && moved.elapsed() >= after
            {
                let waiting = format!("waiting for \"{text}\"");
                return Err(self.stalled(after, &waiting));
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
        let folder = self.shared.bundle.screenshots(&self.folder);
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

    /// Whether this boot came up with no console to read: the kernel
    /// says where the console's cells are long before the machine is
    /// ready, so a boot that is ready without saying so has no screen,
    /// and waiting longer will not give it one.
    fn came_up_without_screen(&mut self) -> bool {
        let ready = self.machine.ready.clone();
        came_up_without_screen(self.serial.this_boot(), &ready)
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
        screen::read_text(
            &screen::Image::parse_ppm(&bytes)?,
            geometry,
            fonts,
            self.driver.screen_noise(),
        )
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
            if read.is_err() && self.came_up_without_screen() {
                return Err(StepFailure::Fail(format!(
                    "the machine has no screen to read: this boot said \"{}\" but never \
                     where the console's cells are (\"console of ... cells\"), as when no \
                     monitor or dummy plug is attached",
                    self.machine.ready
                )));
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
        let path = self.save_log(test);
        self.log.clear();
        path
    }

    /// Writes the log so far after step number `done`, with a last line
    /// that says the test is still running: a week-long test that the lab
    /// loses (a restart of the lab PC) keeps what it did. The final
    /// [`Worker::write_log`] replaces the file.
    fn write_progress(&mut self, test: &Test, done: usize) {
        self.log.push(format!(
            "Still running: step {done} of {} done at {}",
            test.steps.len(),
            Utc::now().rfc3339()
        ));
        // A log that cannot be written here is tried again after the next
        // step, and the final write reports it.
        let _ = self.save_log(test);
        self.log.pop();
    }

    /// Writes the log to `<logs>/<test id with / as ->.log`; returns its
    /// path in the bundle.
    fn save_log(&self, test: &Test) -> Option<String> {
        let folder = self.shared.bundle.logs(&self.folder);
        let path = folder.join(format!("{}.log", test.id.replace('/', "-")));
        let mut text = self.log.join("\n");
        text.push('\n');
        fs::create_dir_all(&folder).and_then(|()| fs::write(&path, text)).ok()?;
        Some(self.shared.bundle.relative(&path))
    }

    /// Why an optional `step` is left out on this machine, if it is: the
    /// machine has no control for it, or a person would have to do it and
    /// the run has none.
    fn left_out(&self, step: &Step) -> Option<String> {
        let control = step.control()?;
        let machine = &self.machine.name;
        match self.machine.who(control) {
            Who::None => Some(format!("left out: {machine} has no {}", control.name())),
            Who::Person if !self.shared.with_person => Some(format!(
                "left out: {machine} needs a person for {}, and the run has no --with-person",
                control.name()
            )),
            _ => None,
        }
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

    /// The last line the console showed, as a person reads it, if any.
    fn last_line(&mut self) -> Option<String> {
        self.refresh();
        self.text.lines().rev().map(str::trim).find(|line| !line.is_empty()).map(String::from)
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

/// Whether `boot`, one boot's serial log, shows the machine `ready`
/// without the kernel having said where the console's cells are.
fn came_up_without_screen(boot: &str, ready: &str) -> bool {
    boot.contains(ready) && screen::Geometry::from_log(boot).is_none()
}

/// Lets tests run whole runs without QEMU.
#[cfg(test)]
mod fake;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machines::DEFAULT_READY;
    use std::io::Write as _;

    #[test]
    fn a_machine_of_another_kind_skips_an_or_skip_test() {
        let azure = Command {
            command: "cat /kernel/class/dmi/id/sys_vendor".into(),
            exit: 0,
            expect: vec!["Microsoft Corporation".into()],
            expect_not: vec![],
            root: false,
            input: None,
            or_skip: Some("needs an Azure machine".into()),
            stall: None,
            save: None,
        };
        let output = |status, text: &str| ssh::Output { status, text: text.into() };
        let check = |command: &Command, status, text| {
            check_run(command, &output(status, text), "$ cat".into())
        };
        assert!(matches!(check(&azure, Some(0), "Microsoft Corporation\n"), Ok(Some(_))));
        // Another vendor, or no such file: the test is skipped, and says why.
        let Err(StepFailure::Skip(why)) = check(&azure, Some(0), "Dell Inc.\n") else {
            panic!("another vendor did not skip");
        };
        assert!(why.starts_with("needs an Azure machine\n"), "{why}");
        assert!(why.contains("its output lacks \"Microsoft Corporation\""), "{why}");
        assert!(matches!(check(&azure, Some(1), ""), Err(StepFailure::Skip(_))));
        // SSH not running it, or the time limit, says nothing of the kind.
        assert!(matches!(check(&azure, Some(255), ""), Err(StepFailure::Fail(_))));
        assert!(matches!(check(&azure, None, ""), Err(StepFailure::Fail(_))));
        // Without or_skip, a check that fails fails the test.
        let plain = Command { or_skip: None, ..azure.clone() };
        assert!(matches!(check(&plain, Some(0), "Dell Inc.\n"), Err(StepFailure::Fail(_))));
    }

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
    fn a_boot_ready_without_a_console_has_no_screen_to_wait_for() {
        let console = "[0.3] Framebuffer: console of 160x45 cells with font 8x16 at (0, 0)\n";
        // Still booting: the console may yet come.
        assert!(!came_up_without_screen("Radix\n", DEFAULT_READY));
        // Ready with a console: the screen can be read.
        assert!(!came_up_without_screen(&format!("{console}Boot complete.\n"), DEFAULT_READY));
        // Ready with none, as with no monitor: no use waiting.
        assert!(came_up_without_screen("Radix\nBoot complete.\n", DEFAULT_READY));
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

// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What a run is doing, or did, read from its bundle: the answers of
//! `status`, `results` and `runs`, which the runner page shows.
//!
//! A run going now has no results.json yet, so its progress comes from
//! its `events.jsonl`: a test is queued until its `test-started`, running
//! until its `test-ended`. The same events rebuild the results of a run
//! that died part way. Whether a run is going comes from its lock (see
//! store.rs), never from guessing at times.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::Error;
use crate::bundle::{Outcome, ResultRecord, ResultsFile, RunRecord, StepRecord};
use crate::clock::Utc;
use crate::store;

/// How many of the last events a status carries, for following a run.
const RECENT_EVENTS: usize = 40;

/// How many earlier runs say how long tests take.
const HISTORY_RUNS: usize = 20;

/// Where a run is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Its process is running it.
    Running,
    /// It ran to the end.
    Ended,
    /// `dukovo-lab stop` cut it short.
    Stopped,
    /// Its process ended before the run did: killed, crashed, or the lab
    /// PC lost power. Its results say what it finished.
    Died,
}

impl State {
    pub fn word(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Ended => "ended",
            State::Stopped => "stopped",
            State::Died => "died",
        }
    }
}

/// Results by kind; `running` and `queued` only while a run goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
    pub errors: u32,
    pub running: u32,
    pub queued: u32,
}

impl Counts {
    fn add(&mut self, state: TestStateKind) {
        match state {
            TestStateKind::Queued => self.queued += 1,
            TestStateKind::Running => self.running += 1,
            TestStateKind::Pass => self.passed += 1,
            TestStateKind::Fail => self.failed += 1,
            TestStateKind::Skip => self.skipped += 1,
            TestStateKind::Error => self.errors += 1,
        }
    }
}

/// Where one test is on one machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TestStateKind {
    Queued,
    Running,
    Pass,
    Fail,
    Skip,
    Error,
}

impl From<Outcome> for TestStateKind {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Pass => TestStateKind::Pass,
            Outcome::Fail => TestStateKind::Fail,
            Outcome::Skip => TestStateKind::Skip,
            Outcome::Error => TestStateKind::Error,
        }
    }
}

/// One test on one machine in a status.
#[derive(Clone, Debug, Serialize)]
pub struct TestState {
    pub test: String,
    pub machine: String,
    pub state: TestStateKind,
    pub reason: Option<String>,
    /// How long it took, once it ended.
    pub milliseconds: Option<u64>,
    /// When it started, once it has.
    pub started: Option<String>,
}

/// One machine in a status.
#[derive(Clone, Debug, Serialize)]
pub struct MachineStatus {
    pub name: String,
    /// The test it is running now.
    pub running: Option<String>,
    pub counts: Counts,
}

/// A step waiting for a person.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Waiting {
    pub machine: String,
    pub test: String,
    pub step: String,
    /// The command that says it is done.
    pub done: String,
}

/// What `status` answers.
#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub id: String,
    pub state: State,
    pub bundle: PathBuf,
    pub build: Option<String>,
    pub preset: Option<String>,
    pub started: String,
    pub ended: Option<String>,
    pub counts: Counts,
    pub machines: Vec<MachineStatus>,
    pub tests: Vec<TestState>,
    /// Steps waiting for a person, at most one per machine.
    pub waiting: Vec<Waiting>,
    /// About how many seconds the run has left, from how long the same
    /// tests took on the same machines before; none when any is unknown.
    pub left_seconds: Option<u64>,
    /// The last events, oldest first.
    pub recent: Vec<Value>,
    /// A dead run's last messages of its own, when it was detached.
    pub output: Option<String>,
}

/// A run's events, skipping a line still being written.
fn read_events(bundle: &Path) -> Vec<Value> {
    fs::read_to_string(bundle.join("events.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn text(event: &Value, field: &str) -> String {
    event[field].as_str().unwrap_or_default().to_string()
}

/// A run's record and where it is.
fn read_state(store: &Path, id: &str) -> Result<(PathBuf, RunRecord, State), Error> {
    let bundle = store::bundle_dir(store, id);
    // The lock first: a run writes its end to run.json before it lets go
    // of the lock, so "no end, no lock" can only mean it died.
    let going = store::is_running(store, id);
    let record = store::read_run(&bundle)
        .ok_or_else(|| Error::Usage(format!("run {id} has no readable run.json")))?;
    let state = state_of(&record, going);
    Ok((bundle, record, state))
}

/// Where a run is, from its record and whether its lock is held.
fn state_of(record: &RunRecord, going: bool) -> State {
    match (&record.ended, record.stopped, going) {
        (Some(_), true, _) => State::Stopped,
        (Some(_), false, _) => State::Ended,
        (None, _, true) => State::Running,
        (None, _, false) => State::Died,
    }
}

/// Reads run `id`'s status.
pub fn status(store: &Path, id: &str) -> Result<Status, Error> {
    let (bundle, record, state) = read_state(store, id)?;
    let events = read_events(&bundle);

    let mut tests: Vec<TestState> = record
        .machines
        .iter()
        .flat_map(|machine| {
            record.tests.iter().map(|test| TestState {
                test: test.clone(),
                machine: machine.name.clone(),
                state: TestStateKind::Queued,
                reason: None,
                milliseconds: None,
                started: None,
            })
        })
        .collect();
    let mut waiting: Vec<Waiting> = Vec::new();
    for event in &events {
        let machine = text(event, "machine");
        let test = text(event, "test");
        let entry = tests.iter_mut().find(|entry| entry.machine == machine && entry.test == test);
        match event["event"].as_str() {
            Some("test-started") => {
                if let Some(entry) = entry {
                    entry.state = TestStateKind::Running;
                    entry.started = event["at"].as_str().map(String::from);
                }
            }
            Some("test-ended") => {
                if let Some(entry) = entry
                    && let Ok(outcome) = serde_json::from_value::<Outcome>(event["result"].clone())
                {
                    entry.state = outcome.into();
                    entry.reason = event["reason"].as_str().map(String::from);
                    entry.milliseconds = event["milliseconds"].as_u64();
                }
                waiting.retain(|wait| wait.machine != machine);
            }
            Some("person-needed") => {
                waiting.retain(|wait| wait.machine != machine);
                waiting.push(Waiting {
                    machine,
                    test,
                    step: text(event, "step"),
                    done: text(event, "done"),
                });
            }
            // The step a person was asked to do has ended either way.
            Some("step-ended") => waiting.retain(|wait| wait.machine != machine),
            _ => {}
        }
    }
    if state == State::Died {
        waiting.clear();
        for entry in tests.iter_mut().filter(|entry| entry.state == TestStateKind::Running) {
            entry.state = TestStateKind::Error;
            entry.reason = Some("the run's process ended during this test".into());
        }
    }

    let mut counts = Counts::default();
    let mut machines: Vec<MachineStatus> = record
        .machines
        .iter()
        .map(|machine| MachineStatus {
            name: machine.name.clone(),
            running: None,
            counts: Counts::default(),
        })
        .collect();
    for entry in &tests {
        counts.add(entry.state);
        if let Some(machine) = machines.iter_mut().find(|machine| machine.name == entry.machine) {
            machine.counts.add(entry.state);
            if entry.state == TestStateKind::Running {
                machine.running = Some(entry.test.clone());
            }
        }
    }
    let left_seconds = (state == State::Running)
        .then(|| time_left(&tests, &typical_durations(store, Some(id))))
        .flatten();
    let output = (state == State::Died)
        .then(|| fs::read_to_string(store::output_log(store, id)).ok())
        .flatten()
        .filter(|text| !text.trim().is_empty());
    let recent = events[events.len().saturating_sub(RECENT_EVENTS)..].to_vec();
    Ok(Status {
        id: record.id,
        state,
        bundle,
        build: record.build,
        preset: record.preset,
        started: record.started,
        ended: record.ended,
        counts,
        machines,
        tests,
        waiting,
        left_seconds,
        recent,
        output,
    })
}

/// How long each test took on each machine the last time it ran to its
/// end, from the newest earlier runs: `(machine, test)` to milliseconds.
pub fn typical_durations(store: &Path, leave_out: Option<&str>) -> HashMap<(String, String), u64> {
    // The time, and whether the test ran to its end (passed or failed)
    // rather than being skipped, stopped or broken part way.
    let mut typical: HashMap<(String, String), (u64, bool)> = HashMap::new();
    for (bundle, record) in store::recent_runs(store, HISTORY_RUNS + 1) {
        if Some(record.id.as_str()) == leave_out {
            continue;
        }
        let Ok(text) = fs::read_to_string(bundle.join("results.json")) else { continue };
        let Ok(file) = serde_json::from_str::<ResultsFile>(&text) else { continue };
        for result in file.results {
            let whole = matches!(result.result, Outcome::Pass | Outcome::Fail);
            // Runs come newest first: the newest whole time wins, else the
            // newest time at all (a test this machine always skips).
            let entry = typical
                .entry((result.machine, result.test))
                .or_insert((result.milliseconds, whole));
            if whole && !entry.1 {
                *entry = (result.milliseconds, true);
            }
        }
    }
    typical.into_iter().map(|(key, (milliseconds, _))| (key, milliseconds)).collect()
}

/// About how long the tests not yet done will take: machines run side by
/// side, so the slowest machine's share. None if any test's time is not
/// known.
fn time_left(tests: &[TestState], typical: &HashMap<(String, String), u64>) -> Option<u64> {
    let now = Utc::now();
    let mut per_machine: HashMap<&str, u64> = HashMap::new();
    for entry in tests {
        let usual = || typical.get(&(entry.machine.clone(), entry.test.clone())).copied();
        let left_ms = match entry.state {
            TestStateKind::Queued => usual()?,
            TestStateKind::Running => {
                let started = entry.started.as_deref().and_then(Utc::parse_rfc3339)?;
                usual()?.saturating_sub(now.seconds_since(started) * 1000)
            }
            _ => 0,
        };
        *per_machine.entry(&entry.machine).or_default() += left_ms;
    }
    Some(per_machine.values().max().copied().unwrap_or(0).div_ceil(1000))
}

/// What `results` answers: a run's results, in full once it has ended,
/// else as far as it got.
#[derive(Serialize)]
pub struct Results {
    pub state: State,
    pub run: RunRecord,
    pub bundle: PathBuf,
    pub results: Vec<ResultRecord>,
}

/// Reads run `id`'s results.
pub fn results(store: &Path, id: &str) -> Result<Results, Error> {
    let (bundle, run, state) = read_state(store, id)?;
    let file = fs::read_to_string(bundle.join("results.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<ResultsFile>(&text).ok());
    let results = match file {
        Some(file) => file.results,
        None => results_from_events(&run, &read_events(&bundle)),
    };
    Ok(Results { state, run, bundle, results })
}

/// The results of the tests that ended, from the events, in the order
/// results.json lists them: by machine, then by test. Step notes are as
/// short as the events carry them.
fn results_from_events(run: &RunRecord, events: &[Value]) -> Vec<ResultRecord> {
    let mut steps: HashMap<(String, String), Vec<StepRecord>> = HashMap::new();
    let mut ended: Vec<ResultRecord> = Vec::new();
    for event in events {
        let key = (text(event, "machine"), text(event, "test"));
        match event["event"].as_str() {
            Some("test-started") => {
                steps.insert(key, Vec::new());
            }
            Some("step-ended") => {
                if let Ok(result) = serde_json::from_value(event["result"].clone()) {
                    steps.entry(key).or_default().push(StepRecord {
                        step: text(event, "step"),
                        result,
                        milliseconds: event["milliseconds"].as_u64().unwrap_or(0),
                        note: event["note"].as_str().map(String::from),
                    });
                }
            }
            Some("test-ended") => {
                let Ok(result) = serde_json::from_value(event["result"].clone()) else {
                    continue;
                };
                ended.push(ResultRecord {
                    steps: steps.remove(&key).unwrap_or_default(),
                    test: key.1,
                    machine: key.0,
                    result,
                    reason: event["reason"].as_str().map(String::from),
                    milliseconds: event["milliseconds"].as_u64().unwrap_or(0),
                    boot: event["boot"].as_u64().and_then(|boot| u32::try_from(boot).ok()),
                    log: event["log"].as_str().map(String::from),
                    screenshots: serde_json::from_value(event["screenshots"].clone())
                        .unwrap_or_default(),
                });
            }
            _ => {}
        }
    }
    let place = |result: &ResultRecord| {
        let machine = run.machines.iter().position(|machine| machine.name == result.machine);
        let test = run.tests.iter().position(|test| *test == result.test);
        (machine, test)
    };
    ended.sort_by_key(place);
    ended
}

/// One run in what `runs` answers.
#[derive(Serialize)]
pub struct RunSummary {
    pub id: String,
    pub state: State,
    pub started: String,
    pub ended: Option<String>,
    pub build: Option<String>,
    pub preset: Option<String>,
    pub machines: Vec<String>,
    pub tests: usize,
    pub summary: Option<crate::bundle::Summary>,
    pub bundle: PathBuf,
}

/// The newest runs, newest first.
pub fn runs(store: &Path, limit: usize) -> Vec<RunSummary> {
    store::recent_runs(store, limit)
        .into_iter()
        .map(|(bundle, mut record)| {
            let mut state = state_of(&record, store::is_running(store, &record.id));
            // The record was read before the lock was looked at, so a run
            // that ended just then would look dead: read it again.
            if state == State::Died
                && let Some(again) = store::read_run(&bundle)
            {
                record = again;
                state = state_of(&record, false);
            }
            RunSummary {
                state,
                started: record.started,
                ended: record.ended,
                build: record.build,
                preset: record.preset,
                machines: record.machines.into_iter().map(|machine| machine.name).collect(),
                tests: record.tests.len(),
                summary: record.summary,
                id: record.id,
                bundle,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::{Bundle, MachineRecord, SCHEMA};
    use crate::testing::TempDir;

    fn record(id: &str) -> RunRecord {
        RunRecord {
            schema: SCHEMA,
            id: id.into(),
            harness: "dukovo-lab 0.1.0".into(),
            started: "2026-11-07T15:30:12Z".into(),
            ended: None,
            build: None,
            image: "lab.img".into(),
            machines: ["qemu", "bios"]
                .map(|name| MachineRecord {
                    name: name.into(),
                    about: String::new(),
                    driver: "qemu".into(),
                })
                .into(),
            tests: vec!["boot/a".into(), "boot/b".into()],
            preset: None,
            summary: None,
            stopped: false,
        }
    }

    #[test]
    fn follows_a_run_from_its_events_and_sees_it_died() {
        let store = TempDir::new("status");
        let id = "20261107-153012-4f2a";
        let bundle = Bundle::create(store.path(), record(id)).unwrap();
        let events = [
            r#"{"event":"test-started","machine":"qemu","test":"boot/a","at":"2026-11-07T15:30:13Z"}"#,
            r#"{"event":"step-ended","machine":"qemu","test":"boot/a","step":"boot","result":"pass","milliseconds":9,"note":null}"#,
            r#"{"event":"test-ended","machine":"qemu","test":"boot/a","result":"fail","reason":"step 1: no","milliseconds":40,"boot":1,"log":"logs/qemu/boot-a.log","screenshots":[]}"#,
            r#"{"event":"test-started","machine":"qemu","test":"boot/b"}"#,
            r#"{"event":"person-needed","machine":"qemu","test":"boot/b","step":"pull the power cord","done":"dukovo-lab done x --machine qemu"}"#,
            r#"{"event":"test-ended","machine":"bios","test":"boot/a","result":"skip","reason":"needs a person for cord","milliseconds":0}"#,
            r#"{"event":"test-ended","machine":"bios","test":"bo"#,
        ];
        fs::write(bundle.dir().join("events.jsonl"), events.join("\n")).unwrap();

        // Going: its process holds the lock.
        let running = store::Running::start(store.path(), id, &["qemu", "bios"]).unwrap();
        let now = status(store.path(), id).unwrap();
        assert_eq!(now.state, State::Running);
        assert_eq!(
            now.counts,
            Counts { passed: 0, failed: 1, skipped: 1, errors: 0, running: 1, queued: 1 }
        );
        assert_eq!(now.machines[0].running.as_deref(), Some("boot/b"));
        assert_eq!(now.waiting.len(), 1);
        assert_eq!(now.waiting[0].step, "pull the power cord");
        assert_eq!(now.left_seconds, None, "no earlier runs say how long tests take");
        assert_eq!(now.recent.len(), 6, "the line cut short is left out");

        let partial = results(store.path(), id).unwrap();
        let ended: Vec<(&str, &str)> = partial
            .results
            .iter()
            .map(|result| (result.machine.as_str(), result.test.as_str()))
            .collect();
        assert_eq!(ended, [("qemu", "boot/a"), ("bios", "boot/a")]);
        assert_eq!(partial.results[0].steps.len(), 1);
        assert_eq!(partial.results[0].log.as_deref(), Some("logs/qemu/boot-a.log"));

        // Its process ends without writing the end: it died.
        drop(running);
        let now = status(store.path(), id).unwrap();
        assert_eq!(now.state, State::Died);
        assert!(now.waiting.is_empty());
        assert_eq!(now.counts.errors, 1, "the test it was running");
        assert_eq!(runs(store.path(), 10)[0].state, State::Died);
    }

    #[test]
    fn earlier_runs_say_how_long_is_left() {
        let tests = vec![
            TestState {
                test: "boot/a".into(),
                machine: "qemu".into(),
                state: TestStateKind::Pass,
                reason: None,
                milliseconds: Some(5000),
                started: None,
            },
            TestState {
                test: "boot/b".into(),
                machine: "qemu".into(),
                state: TestStateKind::Queued,
                reason: None,
                milliseconds: None,
                started: None,
            },
        ];
        let mut typical = HashMap::new();
        assert_eq!(time_left(&tests, &typical), None);
        typical.insert(("qemu".to_string(), "boot/b".to_string()), 61_500);
        assert_eq!(time_left(&tests, &typical), Some(62));
    }

    #[test]
    fn a_stopped_run_does_not_say_how_long_tests_take() {
        let store = TempDir::new("status-typical");
        let result = |test: &str, result: Outcome, milliseconds: u64| ResultRecord {
            test: test.into(),
            machine: "qemu".into(),
            result,
            reason: None,
            milliseconds,
            boot: None,
            log: None,
            steps: Vec::new(),
            screenshots: Vec::new(),
        };
        // An older run that ran both tests, then a newer one stopped part
        // way through boot/a, before boot/b.
        let older = Bundle::create(store.path(), record("20261107-153012-0001")).unwrap();
        older
            .write_results(&[
                result("boot/a", Outcome::Pass, 60_000),
                result("boot/b", Outcome::Skip, 0),
            ])
            .unwrap();
        let newer = Bundle::create(store.path(), record("20261107-163012-0002")).unwrap();
        newer
            .write_results(&[
                result("boot/a", Outcome::Skip, 9_000),
                result("boot/b", Outcome::Skip, 0),
            ])
            .unwrap();
        let typical = typical_durations(store.path(), None);
        let key = |test: &str| ("qemu".to_string(), test.to_string());
        assert_eq!(typical[&key("boot/a")], 60_000, "the older whole run's time");
        assert_eq!(typical[&key("boot/b")], 0, "never ran: skipped again, most likely");
    }
}

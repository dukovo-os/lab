// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What the runner page's Home and Tests views read: the latest result of
//! each test on each machine and pool, with how it got there.
//!
//! `grid` reads the newest [`GRID_RUNS`] bundles' results.json once, with
//! only the fields it needs, so it stays cheap when the store holds a
//! hundred runs. A *column* is a machine that is in no pool, or a pool
//! (`pool:<name>`): a test that ran on any machine of a pool counts for
//! the pool, whatever machine took it. A cell holds the newest result of a
//! test in a column, its class (findings.rs), the first line of its reason
//! and its last [`HISTORY`] results as a string, one letter each, oldest
//! first: `p` passed, `f` failed, `k` failed then passed when run again,
//! `e` errored, `x` skipped. A test that is red (failed or errored) there
//! also says when it first went red and the build it first failed on: the
//! start of its unbroken run of red results, skips left out.
//!
//! `history` is the same data for one test across every machine, newest
//! first, which `show --json` gives the page's Tests view. A run that is
//! still going has no results.json, so it shows only as `running`.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::bundle::{Outcome, Rerun, RunRecord};
use crate::clock::Utc;
use crate::findings::{self, Class, Finding};
use crate::machines::{Machine, POOL_PREFIX};
use crate::status::{self, TestStateKind};
use crate::store;
use crate::suite::Test;

/// How many of the newest runs the grid reads.
pub const GRID_RUNS: usize = 100;

/// How many results a cell's history holds.
pub const HISTORY: usize = 14;

/// A machine or a pool, as the grid's columns name them.
#[derive(Serialize)]
pub struct Column {
    /// The machine's name, or the pool as `--machine` writes it.
    pub name: String,
    pub kind: &'static str,
    /// For a pool: the machines a run on it shares its tests among, and
    /// its spares.
    pub members: Vec<String>,
    pub spares: Vec<String>,
}

/// One test in one column: its newest result.
#[derive(Serialize)]
pub struct Cell {
    pub result: Outcome,
    /// What the page draws: `pass`, `fail`, `flaky`, `stalled`, `error`,
    /// `benched` or `skip`.
    pub state: &'static str,
    /// Whose it is, for a failure or an error.
    pub class: Option<Class>,
    /// The first line of why it failed, was skipped or broke.
    pub reason: Option<String>,
    /// For a skip, what the reason comes to, so skips add up by cause.
    pub skip_group: Option<String>,
    pub milliseconds: u64,
    pub run: String,
    /// When that run started.
    pub at: String,
    /// The machine that ran it, which differs from the column for a pool.
    pub machine: String,
    pub rerun: Option<CellRerun>,
    pub finding: Option<String>,
    pub history: String,
    /// How many red results in a row it has, 0 when it is not red.
    pub streak: usize,
    pub red_since: Option<RedSince>,
}

/// Where a test that failed was run again, and how that came out.
#[derive(Serialize)]
pub struct CellRerun {
    pub machine: String,
    pub result: Outcome,
}

/// The first run of a red streak.
#[derive(Clone, Serialize)]
pub struct RedSince {
    pub run: String,
    pub at: String,
    pub build: Option<String>,
}

/// One test across the columns.
#[derive(Serialize)]
pub struct Row {
    pub test: String,
    pub category: String,
    pub about: String,
    /// Red in some column.
    pub failing: bool,
    /// Red for the first time in its newest result somewhere, and Dukovo's.
    pub new: bool,
    /// The class of the failure that matters most in the row.
    pub class: Option<Class>,
    /// The worst result of each of the last runs that held the test,
    /// oldest first, in the letters of a cell's history.
    pub history: String,
    pub red_since: Option<RedSince>,
    pub cells: BTreeMap<String, Cell>,
}

/// A test running now in a column.
#[derive(Serialize)]
pub struct Running {
    pub test: String,
    pub column: String,
    pub machine: String,
    pub run: String,
}

/// The newest run that ended, and how its time split.
#[derive(Serialize)]
pub struct Round {
    pub run: String,
    pub started: String,
    pub build: Option<String>,
    pub seconds: u64,
    pub pools: Vec<RoundPool>,
}

/// A pool of a round: how many machines shared its tests, and what they
/// took one after another.
#[derive(Serialize)]
pub struct RoundPool {
    pub name: String,
    pub machines: usize,
    pub busy_seconds: u64,
}

/// What `grid` answers.
#[derive(Serialize)]
pub struct Grid {
    pub columns: Vec<Column>,
    pub rows: Vec<Row>,
    pub running: Vec<Running>,
    pub round: Option<Round>,
    /// How many runs the grid was read from.
    pub runs: usize,
}

/// The fields of a result that the grid reads, so a bundle's steps, which
/// are most of results.json, are skipped as they are parsed.
#[derive(Deserialize)]
struct Slim {
    test: String,
    machine: String,
    result: Outcome,
    reason: Option<String>,
    milliseconds: u64,
    rerun: Option<Rerun>,
}

#[derive(Deserialize)]
struct SlimFile {
    results: Vec<Slim>,
}

/// A run that has ended, with its results.
struct Loaded {
    record: RunRecord,
    results: Vec<Slim>,
}

/// The newest `limit` runs that have a results.json, newest first.
fn load_runs(store: &Path, limit: usize) -> Vec<Loaded> {
    store::recent_runs(store, limit)
        .into_iter()
        .filter_map(|(bundle, record)| {
            let text = fs::read_to_string(bundle.join("results.json")).ok()?;
            let file: SlimFile = serde_json::from_str(&text).ok()?;
            Some(Loaded { record, results: file.results })
        })
        .collect()
}

/// One result of a test in a column, as the history keeps it.
struct Seen<'a> {
    run: &'a RunRecord,
    result: &'a Slim,
}

impl Seen<'_> {
    fn flaky(&self) -> bool {
        self.result.result == Outcome::Fail
            && self.result.rerun.as_ref().is_some_and(|rerun| rerun.result == Outcome::Pass)
    }

    fn letter(&self) -> char {
        match self.result.result {
            Outcome::Pass => 'p',
            Outcome::Fail if self.flaky() => 'k',
            Outcome::Fail => 'f',
            Outcome::Skip => 'x',
            Outcome::Error => 'e',
        }
    }

    fn red(&self) -> bool {
        matches!(self.result.result, Outcome::Fail | Outcome::Error)
    }
}

/// The columns machines make: those in no pool, then each pool.
fn columns(machines: &[Machine]) -> Vec<Column> {
    let mut columns: Vec<Column> = machines
        .iter()
        .filter(|machine| machine.pool.is_none())
        .map(|machine| Column {
            name: machine.name.clone(),
            kind: "machine",
            members: Vec::new(),
            spares: Vec::new(),
        })
        .collect();
    for machine in machines {
        let Some(pool) = &machine.pool else { continue };
        let label = format!("{POOL_PREFIX}{pool}");
        let at = match columns.iter().position(|column| column.name == label) {
            Some(at) => at,
            None => {
                columns.push(Column {
                    name: label,
                    kind: "pool",
                    members: Vec::new(),
                    spares: Vec::new(),
                });
                columns.len() - 1
            }
        };
        let list = if machine.spare { &mut columns[at].spares } else { &mut columns[at].members };
        list.push(machine.name.clone());
    }
    columns
}

/// The column each machine's results go in, by machine name, and the pools
/// by label.
fn column_names(machines: &[Machine]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for machine in machines {
        let column = match &machine.pool {
            Some(pool) => format!("{POOL_PREFIX}{pool}"),
            None => machine.name.clone(),
        };
        names.insert(column.clone(), column.clone());
        names.insert(machine.name.clone(), column);
    }
    names
}

/// The first line of `reason`.
fn first_line(reason: &str) -> String {
    reason.lines().next().unwrap_or_default().to_string()
}

/// What a skip's reason comes to, so skips add up by cause.
pub fn skip_group(reason: &str) -> String {
    let first = reason.lines().next().unwrap_or_default();
    if first.starts_with("needs a person for") {
        "need a person".into()
    } else if let Some(controls) = first.strip_prefix("the machine has no ") {
        format!("no {controls}")
    } else if let Some(programs) = first.strip_prefix("the lab PC has no ") {
        let programs = programs.split(", which").next().unwrap_or(programs);
        format!("lab PC lacks {programs}")
    } else if first.contains("was stopped") {
        "run stopped".into()
    } else if first.contains("was benched") {
        "machine benched".into()
    } else if first.starts_with("no machine of ") {
        "pool machines out".into()
    } else if first.contains("the run was given no") {
        "run has no kit".into()
    } else {
        first.chars().take(40).collect()
    }
}

/// How a result looks in the grid.
fn state_of(seen: &Seen<'_>) -> &'static str {
    let reason = seen.result.reason.as_deref().unwrap_or_default();
    match seen.result.result {
        Outcome::Pass => "pass",
        Outcome::Fail if seen.flaky() => "flaky",
        Outcome::Fail if reason.lines().next().is_some_and(|line| line.contains(": stalled: ")) => {
            "stalled"
        }
        Outcome::Fail => "fail",
        Outcome::Error => "error",
        Outcome::Skip if reason.contains("was benched") => "benched",
        Outcome::Skip => "skip",
    }
}

/// The cell for a test in a column, from its results there, newest first.
fn cell(seen: &[Seen<'_>], findings: &[Finding]) -> Cell {
    let latest = &seen[0];
    let result = latest.result;
    let reason = result.reason.as_deref().unwrap_or_default();
    let finding = findings::finding_of(findings, &result.test, reason)
        .filter(|_| result.result == Outcome::Fail);
    let class = latest.red().then(|| findings::classify(result.result, latest.flaky(), finding));
    let history: String =
        seen.iter().take(HISTORY).map(Seen::letter).collect::<Vec<_>>().into_iter().rev().collect();
    // Skips say nothing about whether a test is red: they are passed over.
    let ran: Vec<&Seen<'_>> =
        seen.iter().filter(|each| each.result.result != Outcome::Skip).collect();
    let streak = if latest.red() { ran.iter().take_while(|each| each.red()).count() } else { 0 };
    let red_since = (streak > 0).then(|| {
        let first = ran[streak - 1];
        RedSince {
            run: first.run.id.clone(),
            at: first.run.started.clone(),
            build: first.run.build.clone(),
        }
    });
    Cell {
        result: result.result,
        state: state_of(latest),
        class,
        reason: result.reason.as_deref().map(first_line),
        skip_group: (result.result == Outcome::Skip).then(|| skip_group(reason)),
        milliseconds: result.milliseconds,
        run: latest.run.id.clone(),
        at: latest.run.started.clone(),
        machine: result.machine.clone(),
        rerun: result
            .rerun
            .as_ref()
            .map(|rerun| CellRerun { machine: rerun.machine.clone(), result: rerun.result }),
        finding: finding.map(|finding| finding.id.clone()),
        history,
        streak,
        red_since,
    }
}

/// Which class matters most when a row is red in several columns.
fn rank(class: Class) -> u8 {
    match class {
        Class::Dukovo => 0,
        Class::Lab => 1,
        Class::Known => 2,
        Class::Flaky => 3,
        Class::Test => 4,
    }
}

/// A row's history: the worst result of each of the last runs that held
/// the test, oldest first.
fn row_history(runs: &[Loaded], test: &str) -> String {
    let worst = |letters: &[char]| {
        ['f', 'e', 'k', 'p', 'x'].into_iter().find(|letter| letters.contains(letter)).unwrap_or('x')
    };
    let mut history: Vec<char> = Vec::new();
    for run in runs {
        let seen: Vec<char> = run
            .results
            .iter()
            .filter(|result| result.test == test)
            .map(|result| Seen { run: &run.record, result }.letter())
            .collect();
        if !seen.is_empty() {
            history.push(worst(&seen));
        }
        if history.len() == HISTORY {
            break;
        }
    }
    history.into_iter().rev().collect()
}

/// The newest run that ended and how its time split between pools.
fn round(runs: &[Loaded]) -> Option<Round> {
    let run = runs.iter().find(|run| run.record.ended.is_some())?;
    let started = Utc::parse_rfc3339(&run.record.started)?;
    let ended = run.record.ended.as_deref().and_then(Utc::parse_rfc3339)?;
    let pools = run
        .record
        .pools
        .iter()
        .map(|pool| RoundPool {
            name: pool.name.clone(),
            machines: pool.members.len(),
            busy_seconds: run
                .results
                .iter()
                .filter(|result| pool.members.contains(&result.machine))
                .map(|result| result.milliseconds)
                .sum::<u64>()
                .div_ceil(1000),
        })
        .collect();
    Some(Round {
        run: run.record.id.clone(),
        started: run.record.started.clone(),
        build: run.record.build.clone(),
        seconds: ended.seconds_since(started),
        pools,
    })
}

/// The grid: every test in `tests` against every machine and pool.
pub fn grid(store: &Path, tests: &[Test], machines: &[Machine]) -> Grid {
    let runs = load_runs(store, GRID_RUNS);
    let findings = findings::load_all(store);
    let column_of = column_names(machines);
    let columns = columns(machines);

    // Each test's results per column, newest first: a run's results are
    // read in order and runs come newest first.
    let mut seen: HashMap<(&str, &str), Vec<Seen<'_>>> = HashMap::new();
    for run in &runs {
        for result in &run.results {
            let Some(column) = column_of.get(&result.machine) else { continue };
            let list = seen.entry((&result.test, column)).or_default();
            // A test is in a column once a run.
            if list.last().is_some_and(|last| last.run.id == run.record.id) {
                continue;
            }
            list.push(Seen { run: &run.record, result });
        }
    }
    let rows = tests
        .iter()
        .map(|test| {
            let cells: BTreeMap<String, Cell> = columns
                .iter()
                .filter_map(|column| {
                    let seen = seen.get(&(test.id.as_str(), column.name.as_str()))?;
                    Some((column.name.clone(), cell(seen, &findings)))
                })
                .collect();
            let red: Vec<&Cell> = cells.values().filter(|cell| cell.streak > 0).collect();
            let class = red.iter().filter_map(|cell| cell.class).min_by_key(|class| rank(*class));
            let new = red.iter().any(|cell| cell.streak == 1 && cell.class == Some(Class::Dukovo));
            let red_since =
                red.iter().filter_map(|cell| cell.red_since.clone()).min_by(|a, b| a.at.cmp(&b.at));
            Row {
                test: test.id.clone(),
                category: test.category.clone(),
                about: test.about.clone(),
                failing: !red.is_empty(),
                new,
                class,
                history: row_history(&runs, &test.id),
                red_since,
                cells,
            }
        })
        .collect();
    Grid {
        columns,
        rows,
        running: running_now(store, &column_of),
        round: round(&runs),
        runs: runs.len(),
    }
}

/// The tests running now, in the column of the machine that runs each.
fn running_now(store: &Path, column_of: &HashMap<String, String>) -> Vec<Running> {
    let mut running = Vec::new();
    for id in store::running_ids(store) {
        let Ok(status) = status::status(store, &id) else { continue };
        for entry in status.tests.iter().filter(|entry| entry.state == TestStateKind::Running) {
            let Some(column) = column_of.get(&entry.machine) else { continue };
            running.push(Running {
                test: entry.test.clone(),
                column: column.clone(),
                machine: entry.machine.clone(),
                run: id.clone(),
            });
        }
    }
    running
}

/// One result of a test, on any machine, as `history` lists them.
#[derive(Serialize)]
pub struct Past {
    pub run: String,
    pub at: String,
    pub build: Option<String>,
    pub machine: String,
    pub result: Outcome,
    pub flaky: bool,
    pub class: Option<Class>,
    pub reason: Option<String>,
    pub milliseconds: u64,
}

/// The newest results of `test` on every machine, newest first, at most
/// `limit`.
pub fn history(store: &Path, test: &str, limit: usize) -> Vec<Past> {
    let findings = findings::load_all(store);
    let mut past = Vec::new();
    for run in load_runs(store, GRID_RUNS) {
        for result in run.results.iter().filter(|result| result.test == test) {
            let seen = Seen { run: &run.record, result };
            let reason = result.reason.as_deref().unwrap_or_default();
            let finding = findings::finding_of(&findings, test, reason)
                .filter(|_| result.result == Outcome::Fail);
            past.push(Past {
                run: run.record.id.clone(),
                at: run.record.started.clone(),
                build: run.record.build.clone(),
                machine: result.machine.clone(),
                result: result.result,
                flaky: seen.flaky(),
                class: seen.red().then(|| findings::classify(result.result, seen.flaky(), finding)),
                reason: result.reason.as_deref().map(first_line),
                milliseconds: result.milliseconds,
            });
            if past.len() == limit {
                return past;
            }
        }
    }
    past
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::{Bundle, MachineRecord, PoolRecord, ResultRecord, SCHEMA};
    use crate::testing::TempDir;

    fn machine(name: &str, pool: Option<&str>, spare: bool) -> Machine {
        let mut machine = crate::machines::load(Path::new("/nonexistent")).unwrap().remove(0);
        machine.name = name.into();
        machine.pool = pool.map(String::from);
        machine.spare = spare;
        machine
    }

    fn result(test: &str, machine: &str, outcome: Outcome, reason: Option<&str>) -> ResultRecord {
        ResultRecord {
            test: test.into(),
            machine: machine.into(),
            result: outcome,
            reason: reason.map(String::from),
            milliseconds: 2000,
            boot: Some(1),
            log: None,
            steps: Vec::new(),
            screenshots: Vec::new(),
            rerun: None,
        }
    }

    /// Writes a run that ended, with its results, and records its failures.
    fn write_run(store: &Path, id: &str, started: &str, results: &[ResultRecord]) {
        let record = RunRecord {
            schema: SCHEMA,
            id: id.into(),
            harness: "dukovo-lab 0.1.0".into(),
            started: started.into(),
            ended: Some(started.replace(":00Z", ":30Z")),
            build: Some(format!("0.13.0 {}", &id[16..])),
            image: "lab.img".into(),
            machines: ["qemu", "box-1", "box-2"]
                .map(|name| MachineRecord {
                    name: name.into(),
                    about: String::new(),
                    driver: "qemu".into(),
                })
                .into(),
            pools: vec![PoolRecord {
                name: "boxes".into(),
                members: vec!["box-1".into(), "box-2".into()],
            }],
            tests: Vec::new(),
            preset: None,
            schedule: None,
            summary: None,
            stopped: false,
            resumes: None,
            options: None,
        };
        let bundle = Bundle::create(store, record.clone()).unwrap();
        bundle.write_results(results).unwrap();
        findings::record(store, &record, results).unwrap();
    }

    fn tests(ids: &[&str]) -> Vec<Test> {
        let dir = TempDir::new("grid-tests");
        for id in ids {
            dir.write(
                &format!("tests/{id}.toml"),
                "about = \"x\"\ntime = \"1m\"\n[[step]]\nrun = \"true\"\n",
            );
        }
        crate::suite::load(&dir.path().join("tests")).unwrap()
    }

    #[test]
    fn a_pool_is_one_column_and_a_cell_has_its_history_and_first_red_run() {
        let store = TempDir::new("grid");
        let machines = [
            machine("qemu", None, false),
            machine("box-1", Some("boxes"), false),
            machine("box-2", Some("boxes"), false),
            machine("box-3", Some("boxes"), true),
        ];
        let step = |n: u32| format!("20261107-0{n}0000-000{n}");
        let started = |n: u32| format!("2026-11-07T0{n}:00:00Z");
        let flaky = |mut result: ResultRecord| {
            result.rerun = Some(Rerun {
                machine: "box-3".into(),
                result: Outcome::Pass,
                reason: None,
                milliseconds: 1,
                boot: None,
                log: None,
            });
            result
        };
        let bad = "step 1: its output lacks \"ok\"\n$ check";
        write_run(
            store.path(),
            &step(1),
            &started(1),
            &[
                result("boot/a", "qemu", Outcome::Pass, None),
                result("boot/a", "box-1", Outcome::Pass, None),
                result("boot/b", "box-2", Outcome::Pass, None),
            ],
        );
        write_run(
            store.path(),
            &step(2),
            &started(2),
            &[
                result("boot/a", "qemu", Outcome::Pass, None),
                result("boot/a", "box-2", Outcome::Fail, Some(bad)),
                result("boot/b", "pool:boxes", Outcome::Skip, Some("needs a person for usb")),
            ],
        );
        write_run(
            store.path(),
            &step(3),
            &started(3),
            &[
                result("boot/a", "qemu", Outcome::Pass, None),
                result("boot/a", "box-1", Outcome::Fail, Some(bad)),
                flaky(result("boot/b", "box-2", Outcome::Fail, Some("step 2: no"))),
            ],
        );
        let grid = grid(store.path(), &tests(&["boot/a", "boot/b", "boot/c"]), &machines);

        let names: Vec<&str> = grid.columns.iter().map(|column| column.name.as_str()).collect();
        assert_eq!(names, ["qemu", "pool:boxes"]);
        assert_eq!(grid.columns[1].members, ["box-1", "box-2"]);
        assert_eq!(grid.columns[1].spares, ["box-3"]);
        assert_eq!(grid.runs, 3);

        let a = &grid.rows[0];
        let pool = &a.cells["pool:boxes"];
        assert_eq!((pool.state, pool.history.as_str()), ("fail", "pff"));
        assert_eq!(pool.machine, "box-1", "the column is the pool, the cell names the machine");
        assert_eq!(pool.reason.as_deref(), Some("step 1: its output lacks \"ok\""));
        assert_eq!(pool.class, Some(Class::Dukovo));
        assert_eq!(pool.streak, 2);
        let since = pool.red_since.as_ref().unwrap();
        assert_eq!(
            (since.run.as_str(), since.build.as_deref()),
            (step(2).as_str(), Some("0.13.0 0002"))
        );
        assert_eq!(a.cells["qemu"].history, "ppp");
        assert!(a.failing && !a.new, "red for two runs is not new");
        assert_eq!(a.class, Some(Class::Dukovo));

        // Failed, then passed when run again: flaky, and a skip does not
        // break its history.
        let b = &grid.rows[1];
        let cell = &b.cells["pool:boxes"];
        assert_eq!(
            (cell.state, cell.class, cell.history.as_str()),
            ("flaky", Some(Class::Flaky), "pxk")
        );
        assert_eq!(cell.rerun.as_ref().map(|rerun| rerun.machine.as_str()), Some("box-3"));
        assert_eq!(cell.streak, 1);
        assert!(!b.new, "flaky is not a new Dukovo failure");
        assert_eq!(b.history, "pxk");

        let c = &grid.rows[2];
        assert!(c.cells.is_empty() && !c.failing, "never run");
        let round = grid.round.unwrap();
        assert_eq!((round.run.as_str(), round.seconds), (step(3).as_str(), 30));
        assert_eq!((round.pools[0].name.as_str(), round.pools[0].machines), ("boxes", 2));
        assert_eq!(round.pools[0].busy_seconds, 4);
    }

    #[test]
    fn a_finding_being_fixed_makes_its_failures_known_and_a_person_s_class_wins() {
        let store = TempDir::new("grid-class");
        let machines = [machine("qemu", None, false)];
        let reason = "step 1: no";
        write_run(
            store.path(),
            "20261107-010000-0001",
            "2026-11-07T01:00:00Z",
            &[
                result("boot/a", "qemu", Outcome::Fail, Some(reason)),
                result("boot/b", "qemu", Outcome::Error, Some("QEMU would not start")),
            ],
        );
        let tests = tests(&["boot/a", "boot/b"]);
        let class = |store: &TempDir, row: usize| {
            grid(store.path(), &tests, &machines).rows[row].cells["qemu"].class
        };
        assert_eq!(class(&store, 0), Some(Class::Dukovo));
        assert_eq!(class(&store, 1), Some(Class::Lab), "an error is the lab's");

        let id = findings::finding_id("boot/a", &findings::signature(reason));
        let mut finding = findings::find(store.path(), &id).unwrap();
        finding.fix = Some("https://github.com/dukovo-os/dukovo/pull/9".into());
        findings::write(store.path(), &finding).unwrap();
        assert_eq!(class(&store, 0), Some(Class::Known));

        findings::set_class(store.path(), &id, Some(Class::Flaky)).unwrap();
        assert_eq!(class(&store, 0), Some(Class::Flaky), "a person's word holds");
        findings::set_not_a_bug(store.path(), &id, Some(Some("the test is wrong".into()))).unwrap();
        assert_eq!(class(&store, 0), Some(Class::Test), "not a bug says the test is wrong");
        findings::set_not_a_bug(store.path(), &id, None).unwrap();
        findings::set_class(store.path(), &id, None).unwrap();
        assert_eq!(class(&store, 0), Some(Class::Known));
    }

    #[test]
    fn skips_add_up_by_cause() {
        let group = |reason: &str| skip_group(reason);
        assert_eq!(group("needs a person for usb, cord"), "need a person");
        assert_eq!(group("the machine has no serial"), "no serial");
        assert_eq!(
            group("the lab PC has no go, which this test runs there: install it to run the test"),
            "lab PC lacks go"
        );
        assert_eq!(group("the run was stopped before this test"), "run stopped");
        assert_eq!(
            group("optiplex-3 was benched: its health check failed twice"),
            "machine benched"
        );
        assert_eq!(
            group("no machine of pool:boxes was left to run this test: x"),
            "pool machines out"
        );
    }

    #[test]
    fn a_test_s_history_runs_across_every_machine_newest_first() {
        let store = TempDir::new("grid-history");
        write_run(
            store.path(),
            "20261107-010000-0001",
            "2026-11-07T01:00:00Z",
            &[
                result("boot/a", "qemu", Outcome::Pass, None),
                result("boot/other", "qemu", Outcome::Pass, None),
            ],
        );
        write_run(
            store.path(),
            "20261107-020000-0002",
            "2026-11-07T02:00:00Z",
            &[result("boot/a", "box-1", Outcome::Fail, Some("step 1: no\nmore"))],
        );
        let past = history(store.path(), "boot/a", 10);
        let seen: Vec<(&str, &str, Outcome)> = past
            .iter()
            .map(|past| (past.run.as_str(), past.machine.as_str(), past.result))
            .collect();
        assert_eq!(
            seen,
            [
                ("20261107-020000-0002", "box-1", Outcome::Fail),
                ("20261107-010000-0001", "qemu", Outcome::Pass),
            ]
        );
        assert_eq!(past[0].reason.as_deref(), Some("step 1: no"));
        assert_eq!(past[0].class, Some(Class::Dukovo));
        assert_eq!(history(store.path(), "boot/a", 1).len(), 1);
    }
}

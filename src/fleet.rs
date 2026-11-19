// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What the runner page's Fleet view reads: each machine, whether it is
//! healthy, benched or busy, and the steps waiting for a person.
//!
//! A machine's health is the line its last fresh boot's health check
//! printed (health.rs), found in the test log of the newest run that
//! booted it: the first test of a boot has the line, and a test log is in
//! the bundle as `logs/<machine>/<test>.log`. A machine is benched when
//! the newest run it took part in says so, in a `machine-out` event (a
//! machine of a pool) or as the reason of the tests it skipped. What a
//! machine is doing now comes from the run holding its lock (store.rs).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::health;
use crate::load;
use crate::machines::{Machine, Who};
use crate::status::{self, Results, TestStateKind, Waiting};
use crate::store;

/// How many of the newest runs a machine's health and benching are looked
/// for in.
const LOOK_BACK: usize = 10;

/// The first step of a test that boots afresh, whose test log holds the
/// health check's line.
const BOOT_STEP: &str = "boot afresh and wait until booted";

/// One check of the health line.
#[derive(Debug, PartialEq, Serialize)]
pub struct HealthCheck {
    pub label: &'static str,
    pub detail: String,
    pub ok: bool,
}

/// The health check of a machine's last fresh boot.
#[derive(Debug, PartialEq, Serialize)]
pub struct Health {
    pub run: String,
    pub at: String,
    /// The log line, as the harness wrote it.
    pub line: String,
    pub ok: bool,
    pub checks: Vec<HealthCheck>,
}

/// Why a machine is out of its run's pool or skipped its tests.
#[derive(Serialize)]
pub struct Benched {
    pub run: String,
    pub at: String,
    pub reason: String,
}

/// What a machine is doing now.
#[derive(Serialize)]
pub struct Doing {
    pub run: String,
    pub test: Option<String>,
    pub started: Option<String>,
    /// How long the test took last time here, for a progress bar.
    pub typical_milliseconds: Option<u64>,
    /// How many of its tests have ended, and how many it has: a machine
    /// of a pool has no count of its own, as it takes tests from a queue.
    pub done: usize,
    pub total: Option<usize>,
}

/// One machine of the fleet.
#[derive(Serialize)]
pub struct FleetMachine {
    pub name: String,
    pub about: String,
    pub driver: String,
    pub pool: Option<String>,
    pub spare: bool,
    /// Who does each control: `driver`, `person` or `none`.
    pub controls: BTreeMap<String, &'static str>,
    pub health: Option<Health>,
    pub benched: Option<Benched>,
    pub doing: Option<Doing>,
}

/// A step waiting for a person, in a run going now.
#[derive(Serialize)]
pub struct WaitingStep {
    pub run: String,
    #[serde(flatten)]
    pub step: Waiting,
}

/// What `fleet` answers.
#[derive(Serialize)]
pub struct Fleet {
    pub lab_pc_load: Option<f64>,
    pub machines: Vec<FleetMachine>,
    pub waiting: Vec<WaitingStep>,
}

/// Reads a health line, `Health check after boot 1: root 5800 MiB free;
/// clock 0 s off the lab PC's: ok`, into its checks. None for a line that
/// is not one.
fn read_health(line: &str, run: &str, at: &str) -> Option<Health> {
    let rest = line.strip_prefix("Health check after boot ")?;
    let (_, rest) = rest.split_once(": ")?;
    let (found, verdict) = rest.rsplit_once(": ")?;
    let number = |text: &str| text.split_whitespace().find_map(|word| word.parse::<u64>().ok());
    let checks = found
        .split("; ")
        .map(|part| {
            if part.starts_with("root") {
                let ok = number(part).is_none_or(|mib| mib >= health::MIN_FREE_KIB / 1024);
                HealthCheck { label: "Root space", detail: part.into(), ok }
            } else {
                let ok = number(part).is_none_or(|seconds| seconds <= health::CLOCK_TOLERANCE);
                HealthCheck { label: "Clock", detail: part.into(), ok }
            }
        })
        .collect();
    Some(Health { run: run.into(), at: at.into(), line: line.into(), ok: verdict == "ok", checks })
}

/// The health line of the newest run in `runs` that booted `machine`.
fn last_health(machine: &str, runs: &[(Results, String)]) -> Option<Health> {
    for (results, started) in runs {
        // The last boot of the run is the freshest.
        let booted = results.results.iter().filter(|result| {
            result.machine == machine && result.steps.first().is_some_and(|s| s.step == BOOT_STEP)
        });
        for result in booted.rev() {
            let Some(log) = &result.log else { continue };
            let text = fs::read_to_string(results.bundle.join(log)).unwrap_or_default();
            let line = text.lines().rev().find(|line| line.starts_with("Health check after boot"));
            if let Some(health) = line.and_then(|line| read_health(line, &results.run.id, started))
            {
                return Some(health);
            }
        }
    }
    None
}

/// Why `machine` was benched in the newest run it took part in, if it was.
fn benched(machine: &str, runs: &[(Results, String)]) -> Option<Benched> {
    let (results, started) =
        runs.iter().find(|(results, _)| results.run.machines.iter().any(|m| m.name == machine))?;
    let text = fs::read_to_string(results.bundle.join("events.jsonl")).unwrap_or_default();
    let reason = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|event| {
            let why = event["reason"].as_str()?;
            let named = event["machine"].as_str() == Some(machine);
            let benched = why.starts_with(&format!("{machine} was benched"));
            (named && benched).then(|| why.to_string())
        })?;
    Some(Benched { run: results.run.id.clone(), at: started.clone(), reason })
}

/// What `machine` is doing in the run that holds it.
fn doing(store: &Path, machine: &str, typical: &HashMap<(String, String), u64>) -> Option<Doing> {
    let run = store::machine_user(store, machine)?;
    let status = status::status(store, &run).ok()?;
    let own = status.machines.iter().find(|each| each.name == machine);
    let entry = status
        .tests
        .iter()
        .find(|entry| entry.machine == machine && entry.state == TestStateKind::Running);
    let in_pool = store::read_run(&status.bundle).is_some_and(|record| {
        record.pools.iter().any(|pool| pool.members.iter().any(|m| m == machine))
    });
    let ended =
        |counts: status::Counts| counts.passed + counts.failed + counts.skipped + counts.errors;
    let done = own.map_or(0, |own| usize::try_from(ended(own.counts)).unwrap_or(0));
    let total = own.filter(|_| !in_pool).map(|own| {
        usize::try_from(ended(own.counts) + own.counts.running + own.counts.queued).unwrap_or(0)
    });
    Some(Doing {
        run,
        test: entry.map(|entry| entry.test.clone()).or_else(|| own?.running.clone()),
        started: entry.and_then(|entry| entry.started.clone()),
        typical_milliseconds: entry
            .and_then(|entry| typical.get(&(machine.to_string(), entry.test.clone())).copied()),
        done,
        total,
    })
}

/// The fleet: every machine in `machines`.
pub fn fleet(store: &Path, machines: &[Machine]) -> Fleet {
    let runs: Vec<(Results, String)> = store::recent_runs(store, LOOK_BACK)
        .into_iter()
        .filter_map(|(_, record)| {
            let results = status::results(store, &record.id).ok()?;
            Some((results, record.started))
        })
        .collect();
    let typical = status::typical_durations(store, None);
    let machines = machines
        .iter()
        .map(|machine| FleetMachine {
            name: machine.name.clone(),
            about: machine.about.clone(),
            driver: format!("{:?}", machine.driver).to_lowercase(),
            pool: machine.pool.clone(),
            spare: machine.spare,
            controls: machine
                .controls
                .iter()
                .map(|(control, who)| {
                    let who = match who {
                        Who::Driver => "driver",
                        Who::Person => "person",
                        Who::None => "none",
                    };
                    (control.name().to_string(), who)
                })
                .collect(),
            health: last_health(&machine.name, &runs),
            benched: benched(&machine.name, &runs),
            doing: doing(store, &machine.name, &typical),
        })
        .collect();
    let waiting = store::running_ids(store)
        .into_iter()
        .filter_map(|id| status::status(store, &id).ok())
        .flat_map(|status| {
            let run = status.id.clone();
            status.waiting.into_iter().map(move |step| WaitingStep { run: run.clone(), step })
        })
        .collect();
    Fleet { lab_pc_load: load::now(), machines, waiting }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::{Bundle, MachineRecord, ResultRecord, RunRecord, SCHEMA, StepRecord};
    use crate::testing::TempDir;

    fn booted(machine: &str, log: &str) -> ResultRecord {
        ResultRecord {
            test: "boot/a".into(),
            machine: machine.into(),
            result: crate::bundle::Outcome::Pass,
            reason: None,
            milliseconds: 1,
            boot: Some(1),
            log: Some(log.into()),
            steps: vec![StepRecord {
                step: BOOT_STEP.into(),
                result: crate::bundle::Outcome::Pass,
                milliseconds: 1,
                note: None,
                lab_pc_load: None,
            }],
            screenshots: Vec::new(),
            rerun: None,
        }
    }

    #[test]
    fn a_machine_s_health_and_benching_come_from_the_newest_run_it_took_part_in() {
        let store = TempDir::new("fleet");
        let record = RunRecord {
            schema: SCHEMA,
            id: "20261107-010000-0001".into(),
            harness: "dukovo-lab 0.1.0".into(),
            started: "2026-11-07T01:00:00Z".into(),
            ended: Some("2026-11-07T02:00:00Z".into()),
            build: None,
            image: "lab.img".into(),
            machines: ["box-1", "box-2"]
                .map(|name| MachineRecord {
                    name: name.into(),
                    about: String::new(),
                    driver: "amt".into(),
                })
                .into(),
            pools: Vec::new(),
            tests: Vec::new(),
            preset: None,
            schedule: None,
            summary: None,
            stopped: false,
            resumes: None,
            options: None,
        };
        let bundle = Bundle::create(store.path(), record).unwrap();
        bundle
            .write_results(&[
                booted("box-1", "logs/box-1/boot-a.log"),
                booted("box-2", "logs/box-2/boot-a.log"),
            ])
            .unwrap();
        let line = |text: &str| format!("Boot 1: serial log x\n{text}\nResult: Pass\n");
        for (machine, text) in [
            (
                "box-1",
                "Health check after boot 1: root 5800 MiB free; clock 0 s off the lab PC's: ok",
            ),
            ("box-2", "Health check after boot 2: clock 41 s off the lab PC's: FAILED"),
        ] {
            let folder = bundle.logs(machine);
            fs::create_dir_all(&folder).unwrap();
            fs::write(folder.join("boot-a.log"), line(text)).unwrap();
        }
        fs::write(
            bundle.dir().join("events.jsonl"),
            "{\"event\":\"machine-out\",\"machine\":\"box-2\",\"reason\":\"box-2 was benched: its health check failed twice (clock 41 s off)\"}\n",
        )
        .unwrap();

        let machines = crate::machines::load(Path::new("/nonexistent")).unwrap();
        let mut one = machines[0].clone();
        one.name = "box-1".into();
        let mut two = machines[0].clone();
        two.name = "box-2".into();
        two.pool = Some("boxes".into());
        let fleet = fleet(store.path(), &[one, two]);
        let first = &fleet.machines[0];
        assert!(first.health.as_ref().unwrap().ok);
        assert!(first.benched.is_none() && first.doing.is_none());
        let second = &fleet.machines[1];
        assert!(!second.health.as_ref().unwrap().ok);
        assert!(second.benched.as_ref().unwrap().reason.starts_with("box-2 was benched"));
        assert_eq!(second.pool.as_deref(), Some("boxes"));
        assert_eq!(first.controls["power"], "driver");
    }

    #[test]
    fn a_health_line_reads_into_its_checks() {
        let line = "Health check after boot 2: root 5800 MiB free; clock 0 s off the lab PC's: ok";
        let health = read_health(line, "20261107-010000-0001", "2026-11-07T01:00:00Z").unwrap();
        assert!(health.ok);
        assert_eq!(health.checks.len(), 2);
        assert_eq!(health.checks[0].label, "Root space");
        assert_eq!(health.checks[1].detail, "clock 0 s off the lab PC's");

        let bad =
            "Health check after boot 1: root 300 MiB free; clock 41 s off the lab PC's: FAILED";
        let health = read_health(bad, "x", "y").unwrap();
        assert!(!health.ok);
        assert_eq!(health.checks.iter().map(|check| check.ok).collect::<Vec<_>>(), [false, false]);
        let skipped = "Health check after boot 1: root space not checked (grow_root is off); clock 2 s off the lab PC's: ok";
        let health = read_health(skipped, "x", "y").unwrap();
        assert!(health.checks.iter().all(|check| check.ok), "{:?}", health.checks);
        assert_eq!(read_health("Boot 1: serial log", "x", "y"), None);
    }
}

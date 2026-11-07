// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Results bundles: what a run leaves behind.
//!
//! A bundle is a folder, `runs/<date>/<run id>/` under the store, laid out
//! as the private dukovo-os/lab-results repository lays out runs, so
//! sending one is a copy:
//!
//! ```text
//! run.json                  what ran: build, image, machines, tests, times
//! results.json              each test's result on each machine, step by step
//! logs/<machine>/           serial-<boot>.log, qemu-<boot>.log, one log per test
//! screenshots/<machine>/    <category>-<test>-<name>.png
//! ```
//!
//! The JSON is described by schema/run.schema.json and
//! schema/results.schema.json; `schema` in each file is their version,
//! raised whenever a field changes meaning or goes away.

use std::collections::hash_map::RandomState;
use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::clock::Utc;

/// The version of the bundle's JSON formats.
pub const SCHEMA: u32 = 1;

/// run.json.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RunRecord {
    pub schema: u32,
    pub id: String,
    /// The harness that ran it, as `dukovo-lab 0.1.0`.
    pub harness: String,
    /// When it started and ended, in RFC 3339 UTC. No end while it runs.
    pub started: String,
    pub ended: Option<String>,
    /// The build under test as `ob lab` names it, such as `0.13.0 fd2a9e8`.
    pub build: Option<String>,
    /// The image file, as given.
    pub image: String,
    pub machines: Vec<MachineRecord>,
    /// The tests picked, by id, in the order they ran.
    pub tests: Vec<String>,
    /// Counts of results, once the run has ended.
    pub summary: Option<Summary>,
}

/// A machine in run.json.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MachineRecord {
    pub name: String,
    pub about: String,
    pub driver: String,
}

/// How many results came out each way.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Summary {
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
    pub errors: u32,
}

impl Summary {
    /// Counts `results`.
    pub fn of(results: &[ResultRecord]) -> Summary {
        let mut summary = Summary::default();
        for result in results {
            match result.result {
                Outcome::Pass => summary.passed += 1,
                Outcome::Fail => summary.failed += 1,
                Outcome::Skip => summary.skipped += 1,
                Outcome::Error => summary.errors += 1,
            }
        }
        summary
    }
}

/// results.json.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ResultsFile {
    pub schema: u32,
    pub run: String,
    pub results: Vec<ResultRecord>,
}

/// How a test came out on a machine.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// Every step passed.
    Pass,
    /// A step's check failed, or the test passed its time limit: a finding.
    Fail,
    /// Not run on this machine; the reason says why.
    Skip,
    /// The harness or the machine broke, so the test says nothing about
    /// Dukovo: QEMU would not start, QMP stopped answering.
    Error,
}

/// One test on one machine, in results.json.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ResultRecord {
    pub test: String,
    pub machine: String,
    pub result: Outcome,
    /// Why it failed, was skipped or broke.
    pub reason: Option<String>,
    /// How long it took, boot included.
    pub milliseconds: u64,
    /// The boot it ran in: its serial log is `logs/<machine>/serial-<boot>.log`.
    pub boot: Option<u32>,
    /// Its log, relative to the bundle.
    pub log: Option<String>,
    pub steps: Vec<StepRecord>,
    /// Its screenshots, relative to the bundle.
    pub screenshots: Vec<String>,
}

/// One step of a test, in results.json.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StepRecord {
    /// The step in words.
    pub step: String,
    pub result: Outcome,
    pub milliseconds: u64,
    /// What went wrong, or what the step saw.
    pub note: Option<String>,
}

/// A bundle being written.
pub struct Bundle {
    dir: PathBuf,
    pub record: RunRecord,
}

impl Bundle {
    /// Makes a new bundle in `store` for a run starting now, and writes its
    /// run.json.
    pub fn create(store: &Path, record: RunRecord, started: Utc) -> Result<Bundle, String> {
        let dir = store.join("runs").join(started.date()).join(&record.id);
        fs::create_dir_all(&dir)
            .map_err(|err| format!("making the bundle {}: {err}", dir.display()))?;
        let bundle = Bundle { dir, record };
        bundle.write_run()?;
        Ok(bundle)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `logs/<machine>/`.
    pub fn logs(&self, machine: &str) -> PathBuf {
        self.dir.join("logs").join(machine)
    }

    /// `screenshots/<machine>/`.
    pub fn screenshots(&self, machine: &str) -> PathBuf {
        self.dir.join("screenshots").join(machine)
    }

    /// `path` relative to the bundle, as the JSON names files.
    pub fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.dir).unwrap_or(path).display().to_string()
    }

    pub fn write_run(&self) -> Result<(), String> {
        write_json(&self.dir.join("run.json"), &self.record)
    }

    pub fn write_results(&self, results: &[ResultRecord]) -> Result<(), String> {
        let file =
            ResultsFile { schema: SCHEMA, run: self.record.id.clone(), results: results.to_vec() };
        write_json(&self.dir.join("results.json"), &file)
    }
}

/// Writes `value` as pretty JSON, through a temporary file so a reader
/// never sees half a file.
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(value).map_err(|err| err.to_string())?;
    text.push('\n');
    let partial = path.with_extension("json.partial");
    fs::write(&partial, text)
        .and_then(|()| fs::rename(&partial, path))
        .map_err(|err| format!("writing {}: {err}", path.display()))
}

/// A new run id: the start time and four random hex digits, so two runs
/// started in the same second still differ.
pub fn new_run_id(started: Utc) -> String {
    let random = RandomState::new().build_hasher().finish();
    format!("{}-{:04x}", started.compact(), random & 0xffff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn writes_run_json_where_lab_results_keeps_runs() {
        let store = TempDir::new("bundle");
        let started = Utc::from_seconds(1_794_065_412);
        let id = new_run_id(started);
        assert!(id.starts_with("20261107-153012-") && id.len() == 20, "{id}");
        let record = RunRecord {
            schema: SCHEMA,
            id: id.clone(),
            harness: "dukovo-lab 0.1.0".into(),
            started: started.rfc3339(),
            ended: None,
            build: Some("0.13.0 fd2a9e8".into()),
            image: "out/lab/dukovo.img".into(),
            machines: vec![],
            tests: vec!["boot/reaches-ready".into()],
            summary: None,
        };
        let bundle = Bundle::create(store.path(), record.clone(), started).unwrap();
        let path = store.path().join("runs/2026-11-07").join(&id).join("run.json");
        assert_eq!(bundle.dir(), path.parent().unwrap());
        let read: RunRecord = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read, record);
        assert_eq!(
            bundle.relative(&bundle.logs("qemu").join("serial-1.log")),
            "logs/qemu/serial-1.log"
        );
    }
}

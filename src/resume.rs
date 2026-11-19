// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Resuming a run that did not finish: `dukovo-lab run --resume <run>`.
//!
//! A run that was stopped, or whose process died, leaves tests unrun: the
//! ones skipped as "the run was stopped before this test", the one it was
//! stopped or killed in, and the ones it never reached. A resume is a new
//! run of just those tests, on the machines they were left on, which
//! names the run it resumes in its run.json (`resumes`). What the run
//! finished stays in that run's own bundle.
//!
//! What is left is read the way `status` reads a run (see status.rs), so
//! it holds for a run that died too, which has no results.json. A resume
//! must test what the first run tested: [`plan`] refuses a build or image
//! that differs from the first run's.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::UNIX_EPOCH;

use crate::Error;
use crate::bundle::RunRecord;
use crate::clock::Utc;
use crate::run::{STOPPED_BEFORE, STOPPED_DURING};
use crate::status::{self, DIED_DURING, State, TestStateKind};
use crate::store;

/// The run being resumed and the tests it did not finish.
#[derive(Debug)]
pub struct Resume {
    /// The run's id.
    pub run: String,
    /// The run's run.json, whose machines, preset and options the resume
    /// takes.
    pub record: RunRecord,
    /// Each unfinished test, as (machine, test).
    left: BTreeSet<(String, String)>,
}

impl Resume {
    /// Whether `test` is one the run left unfinished on `machine`.
    pub fn has_left(&self, machine: &str, test: &str) -> bool {
        self.left.contains(&(machine.to_string(), test.to_string()))
    }

    /// The machines with a test left, in the order the run listed them.
    pub fn machines(&self) -> Vec<&str> {
        self.record
            .machines
            .iter()
            .map(|machine| machine.name.as_str())
            .filter(|name| self.left.iter().any(|(machine, _)| machine == name))
            .collect()
    }

    /// The tests left on some machine, in the order the run listed them.
    pub fn tests(&self) -> Vec<&str> {
        self.record
            .tests
            .iter()
            .map(String::as_str)
            .filter(|name| self.left.iter().any(|(_, test)| test == name))
            .collect()
    }
}

/// What run `id` left unfinished, for a run of `image` and `build` to
/// finish. Fails when the run is still going, when nothing is left, or
/// when the image or build is not the one the run tested.
pub fn plan(store: &Path, id: &str, image: &Path, build: Option<&str>) -> Result<Resume, Error> {
    let id = store::pick_run(store, Some(id))?;
    let status = status::status(store, &id)?;
    if status.state == State::Running {
        return Err(Error::Usage(format!(
            "run {id} is still going: wait for it (`dukovo-lab status --wait {id}`) or stop it \
             (`dukovo-lab stop {id}`) before resuming it"
        )));
    }
    let record = store::read_run(&status.bundle)
        .ok_or_else(|| Error::Usage(format!("run {id} has no readable run.json")))?;
    refuse_other_build(&record, image, build)?;
    let left: BTreeSet<(String, String)> = status
        .tests
        .iter()
        .filter(|test| match test.state {
            TestStateKind::Queued => true,
            TestStateKind::Skip => {
                matches!(test.reason.as_deref(), Some(STOPPED_BEFORE | STOPPED_DURING))
            }
            TestStateKind::Error => test.reason.as_deref() == Some(DIED_DURING),
            TestStateKind::Running | TestStateKind::Pass | TestStateKind::Fail => false,
        })
        .map(|test| (test.machine.clone(), test.test.clone()))
        .collect();
    if left.is_empty() {
        return Err(Error::Usage(format!(
            "nothing to resume: every test of run {id} ran to its end (`dukovo-lab results {id}` \
             shows how each went)"
        )));
    }
    Ok(Resume { run: id, record, left })
}

/// Fails unless `image` and `build` are those the run in `record` was
/// given. A build name (`ob lab` gives the version and commit) says what
/// the image holds, so the same build rebuilt is the same test. A run
/// given no build name has only the image's path to go by, and `ob image`
/// writes a rebuild where the old file was, so such an image is refused
/// when its file is newer than the run.
fn refuse_other_build(record: &RunRecord, image: &Path, build: Option<&str>) -> Result<(), Error> {
    let id = &record.id;
    let named = |build: Option<&str>| build.map_or("no name".to_string(), |b| format!("\"{b}\""));
    if record.build.as_deref() != build {
        return Err(Error::Usage(format!(
            "run {id} tested build {}, and this run is of build {}: a resume tests what the run \
             tested, so check out that build and rebuild the image (`dukovo-lab runs` shows each \
             run's build)",
            named(record.build.as_deref()),
            named(build)
        )));
    }
    if record.image != image.display().to_string() {
        return Err(Error::Usage(format!(
            "run {id} tested the image {}, and this run is given {}: resume with --image {} \
             (`dukovo-lab runs --json` shows each run's image)",
            record.image,
            image.display(),
            record.image
        )));
    }
    if record.build.is_some() {
        return Ok(());
    }
    let written = std::fs::metadata(image)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|time| time.as_secs());
    let started = Utc::parse_rfc3339(&record.started).map(Utc::seconds);
    if let (Some(written), Some(started)) = (written, started)
        && written > started
    {
        return Err(Error::Usage(format!(
            "the image {} was written after run {id} started, so it may not be the image the run \
             tested: build the image of run {id}'s build again and check its time with `ls -l`",
            image.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::{Bundle, MachineRecord, SCHEMA};
    use crate::testing::TempDir;

    const ID: &str = "20261107-153012-4f2a";

    /// A run of tests a to e on two machines, with the events `events`
    /// (each `(event, machine, test, result, reason)`) and the image
    /// written before it started, in a store.
    fn store_with(events: &[(&str, &str, &str, &str, &str)]) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new("resume");
        let image = dir.write("dukovo.img", "image");
        let record = RunRecord {
            schema: SCHEMA,
            id: ID.into(),
            harness: "dukovo-lab 0.1.0".into(),
            started: Utc::now().rfc3339(),
            ended: None,
            build: Some("0.13.0 fd2a9e8".into()),
            image: image.display().to_string(),
            machines: ["qemu", "bios"]
                .map(|name| MachineRecord {
                    name: name.into(),
                    about: String::new(),
                    driver: "qemu".into(),
                })
                .into(),
            tests: ["boot/a", "boot/b", "boot/c"].map(String::from).into(),
            preset: None,
            schedule: None,
            resumes: None,
            options: None,
            summary: None,
            stopped: false,
        };
        let bundle = Bundle::create(&dir.path().join("store"), record).unwrap();
        let lines: Vec<String> = events
            .iter()
            .map(|(event, machine, test, result, reason)| {
                serde_json::json!({
                    "event": event, "machine": machine, "test": test,
                    "result": result, "reason": (!reason.is_empty()).then_some(reason),
                })
                .to_string()
            })
            .collect();
        std::fs::write(bundle.dir().join("events.jsonl"), lines.join("\n")).unwrap();
        (dir, image)
    }

    /// Ends the run as `dukovo-lab stop` does.
    fn stopped(dir: &TempDir) {
        let store = dir.path().join("store");
        let bundle = store::bundle_dir(&store, ID);
        let mut record = store::read_run(&bundle).unwrap();
        record.ended = Some(Utc::now().rfc3339());
        record.stopped = true;
        Bundle::create(&store, record).unwrap();
    }

    fn plan_of(dir: &TempDir, image: &Path, build: Option<&str>) -> Result<Resume, String> {
        plan(&dir.path().join("store"), ID, image, build).map_err(|err| match err {
            Error::Usage(why) | Error::Harness(why) | Error::Busy(why) => why,
        })
    }

    const BUILD: Option<&str> = Some("0.13.0 fd2a9e8");

    #[test]
    fn a_stopped_run_has_the_tests_it_skipped_left() {
        let (dir, image) = store_with(&[
            ("test-ended", "qemu", "boot/a", "pass", ""),
            ("test-ended", "qemu", "boot/b", "skip", STOPPED_DURING),
            ("test-ended", "qemu", "boot/c", "skip", STOPPED_BEFORE),
            ("test-ended", "bios", "boot/a", "pass", ""),
            ("test-ended", "bios", "boot/b", "fail", "step 1: its output lacks \"x\""),
            ("test-ended", "bios", "boot/c", "skip", "needs a person for cord"),
        ]);
        stopped(&dir);
        let resume = plan_of(&dir, &image, BUILD).unwrap();
        assert_eq!(resume.run, ID);
        // A test skipped for any other reason, or failed, is finished.
        assert_eq!(resume.machines(), ["qemu"]);
        assert_eq!(resume.tests(), ["boot/b", "boot/c"]);
    }

    #[test]
    fn a_run_that_died_has_the_test_it_died_in_and_those_it_never_reached_left() {
        let (dir, image) = store_with(&[
            ("test-started", "qemu", "boot/a", "", ""),
            ("test-ended", "qemu", "boot/a", "pass", ""),
            ("test-started", "qemu", "boot/b", "", ""),
            ("test-ended", "bios", "boot/a", "error", "the bios machine did not start: no QEMU"),
            ("test-started", "bios", "boot/b", "", ""),
        ]);
        let resume = plan_of(&dir, &image, BUILD).unwrap();
        assert_eq!(resume.machines(), ["qemu", "bios"]);
        assert_eq!(resume.tests(), ["boot/b", "boot/c"]);
        assert!(resume.has_left("bios", "boot/b") && resume.has_left("qemu", "boot/c"));
        assert!(!resume.has_left("qemu", "boot/a"), "a test that passed is not left");
        assert!(!resume.has_left("bios", "boot/a"), "an error the lab itself had is finished");
    }

    #[test]
    fn a_run_still_going_is_not_resumed() {
        let (dir, image) = store_with(&[("test-started", "qemu", "boot/a", "", "")]);
        let running = store::Running::start(&dir.path().join("store"), ID, &["qemu"]).unwrap();
        let err = plan_of(&dir, &image, BUILD).unwrap_err();
        assert!(err.contains("is still going") && err.contains("dukovo-lab stop"), "{err}");
        running.end();
    }

    #[test]
    fn nothing_is_resumed_when_every_test_ran_to_its_end() {
        let events: Vec<_> = ["qemu", "bios"]
            .iter()
            .flat_map(|machine| {
                ["boot/a", "boot/b", "boot/c"]
                    .map(|test| ("test-ended", *machine, test, "pass", ""))
            })
            .collect();
        let (dir, image) = store_with(&events);
        stopped(&dir);
        let err = plan_of(&dir, &image, BUILD).unwrap_err();
        assert!(
            err.starts_with("nothing to resume") && err.contains("dukovo-lab results"),
            "{err}"
        );
    }

    #[test]
    fn a_build_or_image_that_differs_from_the_run_s_is_refused() {
        let (dir, image) = store_with(&[("test-ended", "qemu", "boot/a", "skip", STOPPED_BEFORE)]);
        stopped(&dir);
        let err = plan_of(&dir, &image, Some("0.13.0 0badc0d")).unwrap_err();
        assert!(
            err.contains("tested build \"0.13.0 fd2a9e8\"") && err.contains("0badc0d"),
            "{err}"
        );
        assert!(plan_of(&dir, &image, None).unwrap_err().contains("no name"));
        let other = dir.write("other.img", "image");
        let err = plan_of(&dir, &other, BUILD).unwrap_err();
        assert!(err.contains("tested the image") && err.contains("--image"), "{err}");
        // The same build rebuilt keeps its path and gets a newer time, and
        // is still the build the run tested; with no build name, the time
        // is all there is to go by.
        let record = store::read_run(&store::bundle_dir(&dir.path().join("store"), ID)).unwrap();
        let earlier = RunRecord { started: "2020-01-01T00:00:00Z".into(), ..record };
        Bundle::create(&dir.path().join("store"), earlier.clone()).unwrap();
        assert!(plan_of(&dir, &image, BUILD).is_ok());
        let unnamed = RunRecord { build: None, ..earlier };
        Bundle::create(&dir.path().join("store"), unnamed).unwrap();
        let err = plan_of(&dir, &image, None).unwrap_err();
        assert!(err.contains("was written after run"), "{err}");
    }
}

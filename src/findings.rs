// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Findings: what failed, grouped, so that a test failing the same way
//! again joins the finding it made before instead of making a new one.
//!
//! Each failed result (a `fail`, never an `error`, which says nothing
//! about Dukovo) belongs to the finding named by its test and its
//! *signature*: the first line of its reason, less what changes from one
//! run to the next (the serial log's boot number, the time limit, what SSH
//! last answered). The finding's id is a hash of the two, the same on any
//! PC and in any version, so the lab-results repository can merge what
//! several sends say about one finding.
//!
//! Findings live in the store as `findings/<id>.json`, the same file the
//! lab-results repository keeps (see send.rs), described by
//! schema/finding.schema.json. A run adds its failures when it ends; a
//! person can say a finding is not a bug, and reopen it. A thread reading
//! lab-results moves a finding on from there (fix proposed, approved,
//! fixed); a send brings that back.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::bundle::{Outcome, ResultRecord, RunRecord};
use crate::clock::Utc;

/// The version of the finding file's format.
pub const SCHEMA: u32 = 1;

/// Where a finding is.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    /// Seen, and nobody has looked at it yet.
    New,
    /// A person said it is not a bug (the test is wrong, say); it stays
    /// that way however often it comes back, and is never offered to send.
    NotABug,
    /// A thread proposed a fix (set in lab-results).
    FixProposed,
    /// The maintainer approved the fix (set in lab-results).
    Approved,
    /// The fix landed (set in lab-results). Failing again reopens it.
    Fixed,
}

impl State {
    pub fn word(self) -> &'static str {
        match self {
            State::New => "new",
            State::NotABug => "not a bug",
            State::FixProposed => "fix proposed",
            State::Approved => "fix approved",
            State::Fixed => "fixed",
        }
    }

    /// Whether lab-results sets this state, rather than the lab.
    fn set_in_results(self) -> bool {
        matches!(self, State::FixProposed | State::Approved | State::Fixed)
    }
}

/// findings/<id>.json.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Finding {
    pub schema: u32,
    pub id: String,
    pub test: String,
    /// How it fails, as its results' reasons all begin.
    pub signature: String,
    pub state: State,
    /// Why a person said it is not a bug.
    pub note: Option<String>,
    /// The first and last runs it failed in, by their start (RFC 3339 UTC).
    pub first_seen: String,
    pub last_seen: String,
    /// How many times it failed: one per run and machine.
    pub count: u32,
    /// The machines and builds it failed on, in the order first seen.
    pub machines: Vec<String>,
    pub builds: Vec<String>,
    /// Every time it failed.
    pub runs: Vec<Seen>,
    /// The machines where its test passes again since it last failed
    /// there, with the first run it passed in.
    #[serde(default)]
    pub passing: BTreeMap<String, Passing>,
    /// The pull request that fixes it, once a thread proposes one (set in
    /// lab-results).
    #[serde(default)]
    pub fix: Option<String>,
    /// When it was last sent to lab-results; never set in lab-results' own
    /// copy, which is all sent.
    pub sent: Option<String>,
}

/// One time a finding's test failed this way.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Seen {
    pub run: String,
    pub machine: String,
    pub build: Option<String>,
    /// When the run started (RFC 3339 UTC).
    pub at: String,
    /// The result's whole reason, with what the step saw.
    pub reason: String,
    /// Whether this run's bundle is in lab-results.
    #[serde(default)]
    pub sent: bool,
}

/// A run where a finding's test passed again.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Passing {
    pub run: String,
    pub at: String,
}

impl Finding {
    fn new(id: String, test: &str, signature: String) -> Finding {
        Finding {
            schema: SCHEMA,
            id,
            test: test.to_string(),
            signature,
            state: State::New,
            note: None,
            first_seen: String::new(),
            last_seen: String::new(),
            count: 0,
            machines: Vec::new(),
            builds: Vec::new(),
            runs: Vec::new(),
            passing: BTreeMap::new(),
            fix: None,
            sent: None,
        }
    }

    /// Adds a time it failed; the same run and machine count once.
    pub fn add(&mut self, seen: Seen) {
        if self.runs.iter().any(|old| old.run == seen.run && old.machine == seen.machine) {
            return;
        }
        self.passing.remove(&seen.machine);
        self.runs.push(seen);
        self.runs.sort_by(|a, b| (&a.at, &a.run, &a.machine).cmp(&(&b.at, &b.run, &b.machine)));
        self.tally();
    }

    /// Works out the counts and lists from `runs`.
    fn tally(&mut self) {
        self.count = u32::try_from(self.runs.len()).unwrap_or(u32::MAX);
        self.first_seen = self.runs.first().map(|seen| seen.at.clone()).unwrap_or_default();
        self.last_seen = self.runs.last().map(|seen| seen.at.clone()).unwrap_or_default();
        self.machines.clear();
        self.builds.clear();
        for seen in &self.runs {
            if !self.machines.contains(&seen.machine) {
                self.machines.push(seen.machine.clone());
            }
            if let Some(build) = &seen.build
                && !self.builds.contains(build)
            {
                self.builds.push(build.clone());
            }
        }
    }

    /// Whether it has failed in a run lab-results does not have yet.
    pub fn unsent(&self) -> bool {
        self.runs.iter().any(|seen| !seen.sent)
    }

    /// What lab-results should hold once this finding is sent over `theirs`
    /// (its copy there, if it has one): every time it failed, from both,
    /// and the state lab-results moved it to, unless a person here said
    /// it is not a bug or reopened it since.
    pub fn merged_with(&self, theirs: Option<&Finding>) -> Finding {
        let mut merged = self.clone();
        if let Some(theirs) = theirs {
            for seen in &theirs.runs {
                merged.add(seen.clone());
            }
            for (machine, passing) in &theirs.passing {
                merged.passing.entry(machine.clone()).or_insert_with(|| passing.clone());
            }
            if theirs.state.set_in_results()
                && !self.state.set_in_results()
                && self.state != State::NotABug
            {
                merged.state = theirs.state;
            }
            if theirs.fix.is_some() {
                merged.fix.clone_from(&theirs.fix);
            }
        }
        for seen in &mut merged.runs {
            seen.sent = true;
        }
        // When it was last sent is the lab PC's own note: in lab-results it
        // would change the file on every send, with nothing new in it.
        merged.sent = None;
        merged
    }
}

/// How a result's reason fails, without what changes from run to run: its
/// first line, less the serial log's path (it names the boot), the time
/// limit, and what SSH last answered.
pub fn signature(reason: &str) -> String {
    let first = reason.lines().next().unwrap_or_default();
    let mut text = without_parentheses(first, |inside| {
        inside.starts_with("logs/") || inside.starts_with("time limit")
    });
    if let Some(cut) = text.find("; last answer:") {
        text.truncate(cut);
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `text` without the ` (...)` groups `drop` picks.
fn without_parentheses(text: &str, drop: impl Fn(&str) -> bool) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find(" (") {
        let Some(length) = rest[open..].find(')') else { break };
        let close = open + length;
        out.push_str(&rest[..open]);
        if !drop(&rest[open + 2..close]) {
            out.push_str(&rest[open..=close]);
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// The id of the finding of `test` failing as `signature`: `f-` and 12 hex
/// digits of a 64-bit FNV-1a hash, which no Rust version or PC changes.
pub fn finding_id(test: &str, signature: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in test.bytes().chain(*b"\n").chain(signature.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("f-{:012x}", hash >> 16)
}

/// Whether `id` is a finding id as [`finding_id`] makes them. Ids from
/// outside are checked before they name any file.
pub fn valid_id(id: &str) -> bool {
    id.len() == 14
        && id.starts_with("f-")
        && id[2..].bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The findings folder of a store, or of a lab-results clone.
pub fn folder(root: &Path) -> PathBuf {
    root.join("findings")
}

/// Reads finding `id` from `root`, if it is there.
pub fn load(root: &Path, id: &str) -> Option<Finding> {
    let text = fs::read_to_string(folder(root).join(format!("{id}.json"))).ok()?;
    serde_json::from_str(&text).ok()
}

/// Every finding in `root`, most recently seen first.
pub fn load_all(root: &Path) -> Vec<Finding> {
    let mut findings: Vec<Finding> = fs::read_dir(folder(root))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let id = name.strip_suffix(".json").filter(|id| valid_id(id))?;
            load(root, id)
        })
        .collect();
    findings.sort_by(|a, b| (&b.last_seen, &a.id).cmp(&(&a.last_seen, &b.id)));
    findings
}

/// Writes `finding` into `root`, through a temporary file so a reader
/// never sees half of it.
pub fn write(root: &Path, finding: &Finding) -> Result<(), Error> {
    let folder = folder(root);
    fs::create_dir_all(&folder)
        .map_err(|err| Error::Harness(format!("making {}: {err}", folder.display())))?;
    let path = folder.join(format!("{}.json", finding.id));
    let partial = folder.join(format!("{}.json.partial", finding.id));
    let mut text = serde_json::to_string_pretty(finding).expect("findings always serialize");
    text.push('\n');
    fs::write(&partial, text)
        .and_then(|()| fs::rename(&partial, &path))
        .map_err(|err| Error::Harness(format!("writing {}: {err}", path.display())))
}

/// Holds the store's findings for one change at a time: two runs ending
/// together must not both rewrite one finding from what it was.
pub fn lock(store: &Path) -> Result<File, Error> {
    let folder = folder(store);
    fs::create_dir_all(&folder)
        .map_err(|err| Error::Harness(format!("making {}: {err}", folder.display())))?;
    let path = folder.join(".lock");
    let file = File::create(&path)
        .map_err(|err| Error::Harness(format!("opening {}: {err}", path.display())))?;
    file.lock().map_err(|err| Error::Harness(format!("locking {}: {err}", path.display())))?;
    Ok(file)
}

/// Adds a run's failures to the store's findings, and notes where tests
/// with findings pass again. Answers the ids of the findings it failed in.
pub fn record(
    store: &Path,
    run: &RunRecord,
    results: &[ResultRecord],
) -> Result<Vec<String>, Error> {
    if !results.iter().any(|result| matches!(result.result, Outcome::Pass | Outcome::Fail)) {
        return Ok(Vec::new());
    }
    let _lock = lock(store)?;
    let mut findings = load_all(store);
    let mut changed: Vec<String> = Vec::new();
    let mut failed_in: Vec<String> = Vec::new();
    for result in results.iter().filter(|result| result.result == Outcome::Fail) {
        let reason = result.reason.clone().unwrap_or_default();
        let signature = signature(&reason);
        let id = finding_id(&result.test, &signature);
        let index = match findings.iter().position(|finding| finding.id == id) {
            Some(index) => index,
            None => {
                findings.push(Finding::new(id.clone(), &result.test, signature));
                findings.len() - 1
            }
        };
        let finding = &mut findings[index];
        // A fix that landed and did not hold: the finding is open again.
        if finding.state == State::Fixed {
            finding.state = State::New;
        }
        finding.add(Seen {
            run: run.id.clone(),
            machine: result.machine.clone(),
            build: run.build.clone(),
            at: run.started.clone(),
            reason,
            sent: false,
        });
        changed.push(id.clone());
        failed_in.push(id);
    }
    for result in results.iter().filter(|result| result.result == Outcome::Pass) {
        for finding in findings.iter_mut().filter(|finding| {
            finding.test == result.test
                && finding.machines.contains(&result.machine)
                && !finding.passing.contains_key(&result.machine)
        }) {
            finding.passing.insert(
                result.machine.clone(),
                Passing { run: run.id.clone(), at: run.started.clone() },
            );
            changed.push(finding.id.clone());
        }
    }
    for finding in findings.iter().filter(|finding| changed.contains(&finding.id)) {
        write(store, finding)?;
    }
    failed_in.dedup();
    Ok(failed_in)
}

/// The findings of run `id`: those it failed in.
pub fn of_run(store: &Path, id: &str) -> Vec<Finding> {
    load_all(store)
        .into_iter()
        .filter(|finding| finding.runs.iter().any(|seen| seen.run == id))
        .collect()
}

/// Says finding `id` is not a bug, with why; or, with `None`, opens it again.
pub fn set_not_a_bug(
    store: &Path,
    id: &str,
    note: Option<Option<String>>,
) -> Result<Finding, Error> {
    let _lock = lock(store)?;
    let mut finding = find(store, id)?;
    match note {
        Some(note) => {
            finding.state = State::NotABug;
            finding.note = note;
        }
        None => {
            finding.state = State::New;
            finding.note = None;
        }
    }
    write(store, &finding)?;
    Ok(finding)
}

/// Reads finding `id`, which must be there.
pub fn find(store: &Path, id: &str) -> Result<Finding, Error> {
    if !valid_id(id) {
        return Err(Error::Usage(format!(
            "\"{id}\" is not a finding id; finding ids look like f-3a9c51d2e07b (`dukovo-lab findings` lists them)"
        )));
    }
    load(store, id).ok_or_else(|| {
        Error::Usage(format!("there is no finding {id}; `dukovo-lab findings --all` lists them"))
    })
}

/// Notes in the store that `sent` went to lab-results at `at`, and takes
/// back the state lab-results holds for each.
pub fn mark_sent(store: &Path, sent: &[Finding], runs: &[String], at: Utc) -> Result<(), Error> {
    let _lock = lock(store)?;
    for theirs in sent {
        let Some(mut finding) = load(store, &theirs.id) else { continue };
        for seen in &mut finding.runs {
            if runs.contains(&seen.run) {
                seen.sent = true;
            }
        }
        finding.sent = Some(at.rfc3339());
        if theirs.state.set_in_results() {
            finding.state = theirs.state;
        }
        finding.fix.clone_from(&theirs.fix);
        write(store, &finding)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::SCHEMA as BUNDLE_SCHEMA;
    use crate::testing::TempDir;

    #[test]
    fn the_same_failure_has_the_same_signature_every_run() {
        let first = "step 2: \"link down\" did not appear on the serial console (logs/qemu/serial-1.log) (time limit 3m, 12m under QEMU's emulator)\n$ ip link";
        let again = "step 2: \"link down\" did not appear on the serial console (logs/optiplex/serial-4.log)";
        assert_eq!(signature(first), "step 2: \"link down\" did not appear on the serial console");
        assert_eq!(signature(first), signature(again));
        assert_eq!(
            signature(
                "step 1: booted, but SSH did not answer as the lab account; last answer: Connection refused"
            ),
            "step 1: booted, but SSH did not answer as the lab account"
        );
        // What the test expects is part of how it fails.
        assert_eq!(
            signature("step 3: its output lacks \"ok (2 of 2)\"\n$ check"),
            "step 3: its output lacks \"ok (2 of 2)\""
        );
        let id = finding_id("drivers/link-pull", &signature(first));
        assert!(valid_id(&id), "{id}");
        assert_eq!(id, finding_id("drivers/link-pull", &signature(again)));
        assert_ne!(id, finding_id("drivers/link-up", &signature(first)));
        // Fixed for good: the same test and failure, the same id, anywhere.
        assert_eq!(finding_id("boot/a", "step 1: no"), "f-f6dc0d2ca36b");
        for bad in ["f-F6DC0D2CA36B", "f-f6dc0d2ca36", "../f-f6dc0d2ca36b", ""] {
            assert!(!valid_id(bad), "{bad}");
        }
    }

    fn run(id: &str, started: &str) -> RunRecord {
        RunRecord {
            schema: BUNDLE_SCHEMA,
            id: id.into(),
            harness: "dukovo-lab 0.1.0".into(),
            started: started.into(),
            ended: Some(started.into()),
            build: Some(format!("0.13.0 {}", &id[16..])),
            image: "lab.img".into(),
            machines: Vec::new(),
            tests: Vec::new(),
            preset: None,
            summary: None,
            stopped: false,
        }
    }

    fn result(test: &str, machine: &str, outcome: Outcome, reason: Option<&str>) -> ResultRecord {
        ResultRecord {
            test: test.into(),
            machine: machine.into(),
            result: outcome,
            reason: reason.map(String::from),
            milliseconds: 1000,
            boot: Some(1),
            log: None,
            steps: Vec::new(),
            screenshots: Vec::new(),
        }
    }

    #[test]
    fn a_test_failing_the_same_way_joins_its_finding() {
        let store = TempDir::new("findings-record");
        let link =
            "step 2: \"link down\" did not appear on the serial console (logs/qemu/serial-1.log)";
        let first = run("20261107-010000-0001", "2026-11-07T01:00:00Z");
        let ids = record(
            store.path(),
            &first,
            &[
                result("drivers/link-pull", "qemu", Outcome::Fail, Some(link)),
                result("boot/a", "qemu", Outcome::Pass, None),
                // A broken lab says nothing about Dukovo: no finding.
                result("boot/b", "qemu", Outcome::Error, Some("QEMU would not start")),
            ],
        )
        .unwrap();
        assert_eq!(ids.len(), 1);
        let second = run("20261107-020000-0002", "2026-11-07T02:00:00Z");
        let again = link.replace("qemu/serial-1", "bios/serial-3");
        record(
            store.path(),
            &second,
            &[
                result("drivers/link-pull", "bios", Outcome::Fail, Some(&again)),
                result("drivers/link-pull", "qemu", Outcome::Fail, Some(link)),
            ],
        )
        .unwrap();
        // Recording a run twice changes nothing.
        record(
            store.path(),
            &second,
            &[result("drivers/link-pull", "qemu", Outcome::Fail, Some(link))],
        )
        .unwrap();

        let findings = load_all(store.path());
        assert_eq!(findings.len(), 1, "one finding for one way of failing");
        let finding = &findings[0];
        assert_eq!(finding.id, ids[0]);
        assert_eq!(finding.count, 3);
        assert_eq!(finding.machines, ["qemu", "bios"]);
        assert_eq!(finding.builds, ["0.13.0 0001", "0.13.0 0002"]);
        assert_eq!(
            (finding.first_seen.as_str(), finding.last_seen.as_str()),
            ("2026-11-07T01:00:00Z", "2026-11-07T02:00:00Z")
        );
        assert!(finding.unsent());
        assert_eq!(of_run(store.path(), &first.id).len(), 1);

        // It passes again on qemu, then fails there once more.
        let third = run("20261107-030000-0003", "2026-11-07T03:00:00Z");
        record(store.path(), &third, &[result("drivers/link-pull", "qemu", Outcome::Pass, None)])
            .unwrap();
        let finding = find(store.path(), &ids[0]).unwrap();
        assert_eq!(finding.passing["qemu"].run, third.id);
        let fourth = run("20261107-040000-0004", "2026-11-07T04:00:00Z");
        record(
            store.path(),
            &fourth,
            &[result("drivers/link-pull", "qemu", Outcome::Fail, Some(link))],
        )
        .unwrap();
        assert!(find(store.path(), &ids[0]).unwrap().passing.is_empty());

        // Not a bug stays so when it comes back; reopening undoes it.
        let finding =
            set_not_a_bug(store.path(), &ids[0], Some(Some("the test's text is old".into())))
                .unwrap();
        assert_eq!(finding.state, State::NotABug);
        let fifth = run("20261107-050000-0005", "2026-11-07T05:00:00Z");
        record(
            store.path(),
            &fifth,
            &[result("drivers/link-pull", "qemu", Outcome::Fail, Some(link))],
        )
        .unwrap();
        assert_eq!(find(store.path(), &ids[0]).unwrap().state, State::NotABug);
        assert_eq!(set_not_a_bug(store.path(), &ids[0], None).unwrap().state, State::New);
        assert!(matches!(find(store.path(), "f-000000000000"), Err(Error::Usage(_))));
    }

    #[test]
    fn a_send_keeps_what_lab_results_says() {
        let mut ours = Finding::new("f-f6dc0d2ca36b".into(), "boot/a", "step 1: no".into());
        ours.add(Seen {
            run: "20261107-020000-0002".into(),
            machine: "qemu".into(),
            build: None,
            at: "2026-11-07T02:00:00Z".into(),
            reason: "step 1: no".into(),
            sent: false,
        });
        let mut theirs = ours.clone();
        theirs.runs[0].run = "20261107-010000-0001".into();
        theirs.runs[0].at = "2026-11-07T01:00:00Z".into();
        theirs.state = State::FixProposed;
        theirs.fix = Some("https://github.com/dukovo-os/dukovo/pull/250".into());
        let merged = ours.merged_with(Some(&theirs));
        assert_eq!(merged.fix, theirs.fix);
        assert_eq!(merged.sent, None, "lab-results' copy carries no send time");
        assert_eq!(merged.count, 2);
        assert_eq!(merged.first_seen, "2026-11-07T01:00:00Z");
        assert_eq!(merged.state, State::FixProposed, "a thread moved it on");
        assert!(merged.runs.iter().all(|seen| seen.sent));
        ours.state = State::NotABug;
        assert_eq!(ours.merged_with(Some(&theirs)).state, State::NotABug, "a person here said so");
    }
}

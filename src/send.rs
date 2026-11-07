// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Send: findings, and the runs they came from, to the private
//! dukovo-os/lab-results repository, where a thread reads them and
//! proposes fixes.
//!
//! The lab PC keeps a clone of lab-results (`<store>/lab-results` unless
//! `--results` names another) and sends with plain git, as the person
//! signed in on the lab PC: bring the clone up to date, copy each run's
//! bundle in (logs compressed, as the repository's README says), merge
//! each finding with the copy there, commit, push. Nothing else on the PC
//! goes: only a bundle's own files, by name, and never through a link.
//! The lab key is never in a bundle.
//!
//! git never prompts here (`GIT_TERMINAL_PROMPT=0`): a clone that needs a
//! password fails with git's message instead, as every command must.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::clock::Utc;
use crate::findings::{self, Finding};
use crate::store;

/// Where lab-results is cloned from, for the message that says how.
const RESULTS_URL: &str = "git@github.com:dukovo-os/lab-results.git";

/// What went.
#[derive(Debug, Serialize)]
pub struct Sent {
    /// The commit in lab-results; none when it already had everything.
    pub commit: Option<String>,
    pub runs: Vec<String>,
    pub findings: Vec<String>,
}

/// `sent.json` in a bundle: that it went to lab-results, when, and with
/// which findings. It is the lab PC's own note and is never sent.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SentNote {
    pub at: String,
    pub commit: Option<String>,
    pub findings: Vec<String>,
}

/// The lab-results clone: `given`, else `<store>/lab-results`.
pub fn clone_dir(store: &Path, given: Option<&Path>) -> PathBuf {
    given.map_or_else(|| store.join("lab-results"), Path::to_path_buf)
}

/// Whether run `id`'s bundle went to lab-results.
pub fn was_sent(bundle: &Path) -> bool {
    bundle.join("sent.json").is_file()
}

/// Sends `finding_ids`, the runs they failed in that lab-results lacks,
/// and the runs `run_ids`, through the clone at `clone`.
pub fn send(
    store: &Path,
    clone: &Path,
    finding_ids: &[String],
    run_ids: &[String],
) -> Result<Sent, Error> {
    // Everything is checked before the clone is touched.
    let mut picked: Vec<Finding> = Vec::new();
    for id in finding_ids {
        let finding = findings::find(store, id)?;
        if !picked.iter().any(|known| known.id == finding.id) {
            picked.push(finding);
        }
    }
    let mut runs: Vec<String> = Vec::new();
    let unsent = picked.iter().flat_map(|finding| finding.runs.iter().filter(|seen| !seen.sent));
    for id in run_ids.iter().chain(unsent.map(|seen| &seen.run)) {
        if !runs.contains(id) {
            runs.push(id.clone());
        }
    }
    for id in &runs {
        let id = store::pick_run(store, Some(id))?;
        if store::is_running(store, &id) {
            return Err(Error::Usage(format!("run {id} is still going; send it once it ends")));
        }
    }
    check_clone(clone)?;

    git(clone, &["pull", "-q", "--rebase", "origin", "main"]).map_err(|why| {
        Error::Harness(format!("could not bring the lab-results clone up to date: {why}"))
    })?;
    for id in &runs {
        copy_bundle(&store::bundle_dir(store, id), &store::bundle_dir(clone, id))?;
    }
    let mut merged: Vec<Finding> = Vec::new();
    for finding in &picked {
        let theirs = findings::load(clone, &finding.id);
        let finding = finding.merged_with(theirs.as_ref());
        findings::write(clone, &finding)?;
        merged.push(finding);
    }

    git(clone, &["add", "-A", "--", "runs", "findings"]).map_err(Error::Harness)?;
    let staged = git(clone, &["diff", "--cached", "--name-only"]).map_err(Error::Harness)?;
    let commit = if staged.trim().is_empty() {
        None
    } else {
        let message = commit_message(&runs, &merged);
        git(clone, &["commit", "-q", "-m", &message]).map_err(Error::Harness)?;
        push(clone)?;
        Some(git(clone, &["rev-parse", "HEAD"]).map_err(Error::Harness)?.trim().to_string())
    };

    // Only now, with lab-results holding it all, does the store say so.
    let now = Utc::now();
    findings::mark_sent(store, &merged, &runs, now)?;
    for id in &runs {
        note_sent(&store::bundle_dir(store, id), now, commit.as_deref(), &merged)?;
    }
    Ok(Sent { commit, runs, findings: merged.into_iter().map(|finding| finding.id).collect() })
}

/// Fails unless `clone` is a clean lab-results clone on `main`.
fn check_clone(clone: &Path) -> Result<(), Error> {
    if !clone.join(".git").exists() {
        return Err(Error::Usage(format!(
            "there is no lab-results clone at {0}; make one once with `git clone {RESULTS_URL} {0}` (the lab PC's GitHub account needs push access), or give --results",
            clone.display()
        )));
    }
    let branch = git(clone, &["symbolic-ref", "--short", "HEAD"]).map_err(Error::Harness)?;
    if branch.trim() != "main" {
        return Err(Error::Usage(format!(
            "the lab-results clone at {} is on {}, not main; `git -C {0} switch main`",
            clone.display(),
            branch.trim()
        )));
    }
    let changes = git(clone, &["status", "--porcelain"]).map_err(Error::Harness)?;
    if !changes.trim().is_empty() {
        return Err(Error::Harness(format!(
            "the lab-results clone at {0} has changes not committed (a send cut short?); look with `git -C {0} status`, then commit or drop them",
            clone.display()
        )));
    }
    Ok(())
}

/// Pushes to lab-results' main; when someone pushed first, takes their
/// commits under ours and tries once more.
fn push(clone: &Path) -> Result<(), Error> {
    if git(clone, &["push", "-q", "origin", "HEAD:main"]).is_ok() {
        return Ok(());
    }
    git(clone, &["pull", "-q", "--rebase", "origin", "main"])
        .and_then(|_| git(clone, &["push", "-q", "origin", "HEAD:main"]))
        .map(|_| ())
        .map_err(|why| {
            Error::Harness(format!(
                "the send is committed in the lab-results clone but could not be pushed, so it goes with the next send: {why}"
            ))
        })
}

fn commit_message(runs: &[String], findings: &[Finding]) -> String {
    let plural = |count: usize, noun: &str| {
        if count == 1 { format!("1 {noun}") } else { format!("{count} {noun}s") }
    };
    let mut message = match (runs.len(), findings.len()) {
        (1, 0) => format!("Send run {}", runs[0]),
        (_, 0) => format!("Send {}", plural(runs.len(), "run")),
        (0, _) => format!("Send {}", plural(findings.len(), "finding")),
        _ => {
            format!("Send {} and {}", plural(findings.len(), "finding"), plural(runs.len(), "run"))
        }
    };
    message.push_str(" from the lab\n\n");
    for finding in findings {
        message.push_str(&format!(
            "{} {}: {} ({})\n",
            finding.id,
            finding.test,
            finding.signature,
            finding.state.word()
        ));
    }
    for run in runs {
        message.push_str(&format!("run {run}\n"));
    }
    message
}

/// Runs git in `dir`; answers what it printed, or why it failed.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("git will not start ({err}); install git"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let said = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(format!("`git {}` failed: {said}", args.join(" ")))
    }
}

/// Copies a bundle's own files from `from` to `to`, logs compressed.
/// run.json goes last, so a copy cut short is redone by the next send.
fn copy_bundle(from: &Path, to: &Path) -> Result<(), Error> {
    if to.join("run.json").is_file() {
        return Ok(());
    }
    let failed = |what: &Path, err: std::io::Error| {
        Error::Harness(format!("copying {} to lab-results: {err}", what.display()))
    };
    fs::create_dir_all(to).map_err(|err| failed(to, err))?;
    for folder in ["logs", "screenshots"] {
        copy_tree(&from.join(folder), &to.join(folder))?;
    }
    for file in ["results.json", "events.jsonl", "run.json"] {
        let source = from.join(file);
        if regular_file(&source) {
            fs::copy(&source, to.join(file)).map_err(|err| failed(&source, err))?;
        }
    }
    Ok(())
}

/// Copies the regular files under `from`, never following a link.
fn copy_tree(from: &Path, to: &Path) -> Result<(), Error> {
    let Ok(entries) = fs::read_dir(from) else { return Ok(()) };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        let name = entry.file_name();
        if kind.is_dir() {
            copy_tree(&path, &to.join(&name))?;
        } else if kind.is_file() {
            fs::create_dir_all(to)
                .map_err(|err| Error::Harness(format!("making {}: {err}", to.display())))?;
            if path.extension().is_some_and(|ext| ext == "log") {
                let mut zipped = name.clone();
                zipped.push(".gz");
                gzip(&path, &to.join(zipped))?;
            } else {
                fs::copy(&path, to.join(&name)).map_err(|err| {
                    Error::Harness(format!("copying {} to lab-results: {err}", path.display()))
                })?;
            }
        }
    }
    Ok(())
}

fn regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// Compresses `from` into `to` with the system's gzip, without a name or
/// time inside, so the same log always compresses the same.
fn gzip(from: &Path, to: &Path) -> Result<(), Error> {
    let out = File::create(to)
        .map_err(|err| Error::Harness(format!("making {}: {err}", to.display())))?;
    let status = Command::new("gzip")
        .args(["-n", "-c"])
        .arg(from)
        .stdout(out)
        .status()
        .map_err(|err| Error::Harness(format!("gzip will not start ({err}); install gzip")))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Harness(format!("gzip could not compress {}", from.display())))
    }
}

/// Leaves `sent.json` in a bundle the store keeps.
fn note_sent(bundle: &Path, at: Utc, commit: Option<&str>, sent: &[Finding]) -> Result<(), Error> {
    let path = bundle.join("sent.json");
    let mut note: SentNote = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    note.at = at.rfc3339();
    if commit.is_some() {
        note.commit = commit.map(String::from);
    }
    let run = bundle.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    for finding in sent.iter().filter(|finding| finding.runs.iter().any(|seen| seen.run == run)) {
        if !note.findings.contains(&finding.id) {
            note.findings.push(finding.id.clone());
        }
    }
    let text = serde_json::to_string_pretty(&note).expect("notes always serialize");
    fs::write(&path, text + "\n")
        .map_err(|err| Error::Harness(format!("writing {}: {err}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::{Bundle, Outcome, ResultRecord, RunRecord, SCHEMA};
    use crate::findings::State;
    use crate::testing::TempDir;

    /// Runs git in `dir` for a test, which must work.
    fn run_git(dir: &Path, args: &[&str]) -> String {
        git(dir, args).unwrap_or_else(|why| panic!("{why}"))
    }

    /// A lab-results repository (bare, as GitHub's) with one commit, and
    /// a clone of it in the store.
    fn lab_results(root: &Path, store: &Path) -> (PathBuf, PathBuf) {
        let origin = root.join("origin.git");
        let clone = store.join("lab-results");
        fs::create_dir_all(&origin).unwrap();
        run_git(&origin, &["init", "-q", "--bare", "-b", "main"]);
        let seed = root.join("seed");
        fs::create_dir_all(&seed).unwrap();
        run_git(&seed, &["init", "-q", "-b", "main"]);
        fs::write(seed.join("README.md"), "# Dukovo Lab results\n").unwrap();
        run_git(&seed, &["config", "user.name", "Lab"]);
        run_git(&seed, &["config", "user.email", "lab@example.org"]);
        run_git(&seed, &["add", "README.md"]);
        run_git(&seed, &["commit", "-q", "-m", "Start"]);
        run_git(&seed, &["push", "-q", origin.to_str().unwrap(), "main"]);
        run_git(root, &["clone", "-q", origin.to_str().unwrap(), clone.to_str().unwrap()]);
        run_git(&clone, &["config", "user.name", "Lab"]);
        run_git(&clone, &["config", "user.email", "lab@example.org"]);
        (origin, clone)
    }

    /// A finished run with one failure, in the store.
    fn failed_run(store: &Path, id: &str) -> String {
        let record = RunRecord {
            schema: SCHEMA,
            id: id.into(),
            harness: "dukovo-lab 0.1.0".into(),
            started: format!("2026-11-07T{}:{}:{}Z", &id[9..11], &id[11..13], &id[13..15]),
            ended: Some("2026-11-07T09:00:00Z".into()),
            build: Some("0.13.0 fd2a9e8".into()),
            image: "lab.img".into(),
            machines: Vec::new(),
            tests: vec!["drivers/link-pull".into()],
            preset: None,
            summary: None,
            stopped: false,
        };
        let bundle = Bundle::create(store, record.clone()).unwrap();
        let results = [ResultRecord {
            test: "drivers/link-pull".into(),
            machine: "qemu".into(),
            result: Outcome::Fail,
            reason: Some("step 2: \"link down\" did not appear on the serial console".into()),
            milliseconds: 9000,
            boot: Some(1),
            log: Some("logs/qemu/drivers-link-pull.log".into()),
            steps: Vec::new(),
            screenshots: vec!["screenshots/qemu/drivers-link-pull-failed.png".into()],
        }];
        bundle.write_results(&results).unwrap();
        let dir = bundle.dir();
        fs::create_dir_all(dir.join("logs/qemu")).unwrap();
        fs::write(dir.join("logs/qemu/drivers-link-pull.log"), "step 2 FAILED\n").unwrap();
        fs::create_dir_all(dir.join("screenshots/qemu")).unwrap();
        fs::write(dir.join("screenshots/qemu/drivers-link-pull-failed.png"), b"\x89PNG").unwrap();
        // Something that is no part of a bundle, and a link out of it.
        fs::write(store.join("lab_ed25519"), "PRIVATE KEY").unwrap();
        std::os::unix::fs::symlink(store.join("lab_ed25519"), dir.join("logs/qemu/key.log"))
            .unwrap();
        findings::record(store, &record, &results).unwrap().remove(0)
    }

    #[test]
    fn sends_a_finding_and_its_run_and_nothing_else() {
        let dir = TempDir::new("send");
        let store = dir.path().join("store");
        let (origin, clone) = lab_results(dir.path(), &store);
        let id = "20261107-080000-0001";
        let finding = failed_run(&store, id);

        let sent = send(&store, &clone, std::slice::from_ref(&finding), &[]).unwrap();
        assert!(sent.commit.is_some());
        assert_eq!(sent.runs, [id]);
        // What lab-results now holds, as GitHub would.
        let check = dir.path().join("check");
        run_git(dir.path(), &["clone", "-q", origin.to_str().unwrap(), check.to_str().unwrap()]);
        let bundle = store::bundle_dir(&check, id);
        assert!(bundle.join("run.json").is_file());
        assert!(bundle.join("results.json").is_file());
        assert!(bundle.join("logs/qemu/drivers-link-pull.log.gz").is_file(), "logs compressed");
        assert!(bundle.join("screenshots/qemu/drivers-link-pull-failed.png").is_file());
        assert!(!bundle.join("logs/qemu/key.log.gz").exists(), "links are never followed");
        assert!(!bundle.join("sent.json").exists(), "the lab PC's own note stays");
        let theirs = findings::load(&check, &finding).unwrap();
        assert!(theirs.runs.iter().all(|seen| seen.sent));
        let message = run_git(&check, &["log", "-1", "--format=%B"]);
        assert!(message.starts_with("Send 1 finding and 1 run from the lab"), "{message}");
        // git grep fails when it finds nothing.
        assert!(git(&check, &["grep", "-I", "PRIVATE KEY"]).is_err(), "the key never goes");

        // The store says what went.
        let ours = findings::find(&store, &finding).unwrap();
        assert!(ours.sent.is_some() && !ours.unsent());
        assert!(was_sent(&store::bundle_dir(&store, id)));
        // Sending again: nothing new, no commit.
        assert_eq!(send(&store, &clone, std::slice::from_ref(&finding), &[]).unwrap().commit, None);

        // A thread proposes a fix in lab-results; the next send brings
        // that back with a new run of the same failure.
        let mut proposed = theirs.clone();
        proposed.state = State::FixProposed;
        findings::write(&check, &proposed).unwrap();
        run_git(&check, &["config", "user.name", "Thread"]);
        run_git(&check, &["config", "user.email", "thread@example.org"]);
        run_git(&check, &["commit", "-q", "-am", "Propose a fix"]);
        run_git(&check, &["push", "-q", "origin", "main"]);
        let again = "20261107-090000-0002";
        failed_run(&store, again);
        let sent = send(&store, &clone, std::slice::from_ref(&finding), &[]).unwrap();
        assert_eq!(sent.runs, [again], "only the run lab-results lacks");
        let ours = findings::find(&store, &finding).unwrap();
        assert_eq!(ours.state, State::FixProposed);
        assert_eq!(ours.count, 2);
    }

    #[test]
    fn refuses_what_it_cannot_send() {
        let dir = TempDir::new("send-refused");
        let store = dir.path().join("store");
        let finding = failed_run(&store, "20261107-080000-0001");
        let missing = send(&store, &store.join("lab-results"), std::slice::from_ref(&finding), &[]);
        assert!(matches!(missing, Err(Error::Usage(why)) if why.contains("git clone")));
        let (_, clone) = lab_results(dir.path(), &store);
        fs::write(clone.join("stray.txt"), "left over").unwrap();
        let dirty = send(&store, &clone, std::slice::from_ref(&finding), &[]);
        assert!(matches!(dirty, Err(Error::Harness(why)) if why.contains("not committed")));
        fs::remove_file(clone.join("stray.txt")).unwrap();
        let unknown = send(&store, &clone, &["f-000000000000".into()], &[]);
        assert!(matches!(unknown, Err(Error::Usage(_))));
        assert!(findings::find(&store, &finding).unwrap().sent.is_none(), "nothing went");
    }
}

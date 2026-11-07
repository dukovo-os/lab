// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What a run prints as it goes: plain lines for a person, or with
//! `--json` one JSON object per line (JSON Lines) for a script or agent.
//!
//! Every JSON line has an `event` field and an `at` time (RFC 3339, UTC):
//! `run-started` comes first with the run's id and bundle, then
//! `test-started`, `step-ended` and `test-ended` for each test on each
//! machine as they happen (machines run side by side, so their lines
//! interleave), `person-needed` when a step waits for a person, then
//! `run-ended` with the counts. Readers skip events they do not know, so
//! new ones can be added.
//!
//! The same lines also go to `events.jsonl` in the bundle, with or without
//! `--json`, so `status`, `results` and the runner page can follow a run
//! started anywhere, and read a run that died part way.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use serde::Serialize;

use crate::bundle::{Outcome, Summary};
use crate::clock::Utc;

/// The longest step note an event carries; the test's log has it whole.
const NOTE_LIMIT: usize = 2000;

/// One thing that happened during a run.
#[derive(Serialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event<'a> {
    RunStarted {
        id: &'a str,
        bundle: &'a Path,
        machines: Vec<&'a str>,
        tests: Vec<&'a str>,
    },
    TestStarted {
        machine: &'a str,
        test: &'a str,
    },
    /// One step of a test is done. `note` is what went wrong or what the
    /// step saw, cut at 2000 bytes.
    StepEnded {
        machine: &'a str,
        test: &'a str,
        step: &'a str,
        result: Outcome,
        milliseconds: u64,
        note: Option<&'a str>,
    },
    /// A step waits for a person to do it on the machine; `done` is the
    /// command that says it is done, which the runner page's Done button
    /// runs too.
    PersonNeeded {
        machine: &'a str,
        test: &'a str,
        step: &'a str,
        done: &'a str,
    },
    /// `log` and `screenshots` are relative to the bundle, as in
    /// results.json.
    TestEnded {
        machine: &'a str,
        test: &'a str,
        result: Outcome,
        reason: Option<&'a str>,
        milliseconds: u64,
        boot: Option<u32>,
        log: Option<&'a str>,
        screenshots: &'a [String],
    },
    RunEnded {
        id: &'a str,
        bundle: &'a Path,
        summary: Summary,
        /// Whether `dukovo-lab stop` cut the run short.
        stopped: bool,
        /// The findings its failures belong to (see findings.rs).
        findings: &'a [String],
    },
}

/// An event with its time, as a line holds it.
#[derive(Serialize)]
struct Line<'a> {
    #[serde(flatten)]
    event: &'a Event<'a>,
    at: String,
}

/// Prints events, from any thread, a whole line at a time, and keeps
/// them in the bundle's `events.jsonl` once there is one.
pub struct Reporter {
    json: bool,
    /// Whether to print at all; the bundle's events are kept either way.
    print: bool,
    file: Mutex<Option<File>>,
}

impl Reporter {
    pub fn new(json: bool) -> Reporter {
        Reporter { json, print: true, file: Mutex::new(None) }
    }

    /// A reporter that only keeps events in the bundle, for tests.
    #[cfg(test)]
    pub fn silent() -> Reporter {
        Reporter { json: true, print: false, file: Mutex::new(None) }
    }

    /// Keeps every event from now on in `file` too.
    pub fn keep_in(&self, file: File) {
        *self.file.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(file);
    }

    pub fn report(&self, event: &Event<'_>) {
        let json = serde_json::to_string(&Line { event, at: Utc::now().rfc3339() })
            .expect("events always serialize");
        // One lock for both, so the file and the screen keep one order.
        let mut file = self.file.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(file) = file.as_mut() {
            let _ = writeln!(file, "{json}");
        }
        let line = if self.json { Some(json) } else { human(event) };
        if let Some(line) = line.filter(|_| self.print) {
            let mut out = std::io::stdout().lock();
            // A closed pipe (`| head`) is no reason to stop the machines.
            let _ = writeln!(out, "{line}").and_then(|()| out.flush());
        }
    }
}

/// `note` cut to what a `step-ended` event carries, at a character's
/// start.
pub fn short_note(note: &str) -> &str {
    let mut end = note.len().min(NOTE_LIMIT);
    while !note.is_char_boundary(end) {
        end -= 1;
    }
    &note[..end]
}

/// An event as a person reads it; none for the steps, which the test's
/// log lists.
fn human(event: &Event<'_>) -> Option<String> {
    Some(match event {
        Event::RunStarted { id, bundle, machines, tests } => format!(
            "Run {id}: {} on {}\nBundle: {}",
            plural(tests.len(), "test"),
            machines.join(", "),
            bundle.display()
        ),
        Event::TestStarted { machine, test } => format!("[{machine}] {test}: running"),
        Event::StepEnded { .. } => return None,
        Event::PersonNeeded { machine, test, step, done } => {
            format!("[{machine}] {test}: NEEDS YOU: {step}; once done, run `{done}`")
        }
        Event::TestEnded { machine, test, result, reason, milliseconds, .. } => {
            let word = match result {
                Outcome::Pass => "passed",
                Outcome::Fail => "FAILED",
                Outcome::Skip => "skipped",
                Outcome::Error => "ERROR",
            };
            let time = if *result == Outcome::Skip {
                String::new()
            } else {
                format!(" in {}", seconds(*milliseconds))
            };
            match reason {
                Some(reason) => format!("[{machine}] {test}: {word}{time}: {reason}"),
                None => format!("[{machine}] {test}: {word}{time}"),
            }
        }
        Event::RunEnded { id, bundle, summary, stopped, findings } => {
            let mut text = format!(
                "Run {id} {}: {} passed, {} failed, {} skipped, {} errors\nBundle: {}",
                if *stopped { "stopped" } else { "done" },
                summary.passed,
                summary.failed,
                summary.skipped,
                summary.errors,
                bundle.display()
            );
            if !findings.is_empty() {
                text.push_str(&format!(
                    "\nFindings: {} (`dukovo-lab findings --run {id}`)",
                    findings.join(", ")
                ));
            }
            text
        }
    })
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 { format!("1 {noun}") } else { format!("{count} {noun}s") }
}

pub fn seconds(milliseconds: u64) -> String {
    let seconds = milliseconds.div_ceil(1000);
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_lines_name_their_event() {
        let event = Event::TestEnded {
            machine: "qemu",
            test: "drivers/link-pull",
            result: Outcome::Fail,
            reason: Some("step 2: no \"link down\""),
            milliseconds: 61_500,
            boot: Some(1),
            log: Some("logs/qemu/drivers-link-pull.log"),
            screenshots: &[],
        };
        let line = Line { event: &event, at: "2026-11-07T15:30:12Z".into() };
        assert_eq!(
            serde_json::to_string(&line).unwrap(),
            r#"{"event":"test-ended","machine":"qemu","test":"drivers/link-pull","result":"fail","reason":"step 2: no \"link down\"","milliseconds":61500,"boot":1,"log":"logs/qemu/drivers-link-pull.log","screenshots":[],"at":"2026-11-07T15:30:12Z"}"#
        );
        assert_eq!(
            human(&event).unwrap(),
            "[qemu] drivers/link-pull: FAILED in 1m02s: step 2: no \"link down\""
        );
    }

    #[test]
    fn events_go_to_the_bundle_with_their_time() {
        let dir = crate::testing::TempDir::new("events");
        let path = dir.path().join("events.jsonl");
        let reporter = Reporter::new(true);
        reporter.keep_in(File::create(&path).unwrap());
        let note = "\u{2713}".repeat(1000); // 3000 bytes
        reporter.report(&Event::StepEnded {
            machine: "qemu",
            test: "features/x",
            step: "run `true`",
            result: Outcome::Pass,
            milliseconds: 5,
            note: Some(short_note(&note)),
        });
        let text = std::fs::read_to_string(&path).unwrap();
        let line: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(line["event"], "step-ended");
        assert_eq!(line["note"].as_str().unwrap().len(), 1998, "cut at a character's start");
        assert!(line["at"].as_str().unwrap().ends_with('Z'));
    }
}

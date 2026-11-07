// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What a run prints as it goes: plain lines for a person, or with
//! `--json` one JSON object per line (JSON Lines) for a script or agent.
//!
//! Every JSON line has an `event` field: `run-started` comes first with
//! the run's id and bundle, then `test-started` and `test-ended` for each
//! test on each machine as they happen (machines run side by side, so
//! their lines interleave), then `run-ended` with the counts. The bundle
//! holds everything in full; these lines are for following a run live.

use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use serde::Serialize;

use crate::bundle::{Outcome, Summary};

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
    TestEnded {
        machine: &'a str,
        test: &'a str,
        result: Outcome,
        reason: Option<&'a str>,
        milliseconds: u64,
    },
    RunEnded {
        id: &'a str,
        bundle: &'a Path,
        summary: Summary,
    },
}

/// Prints events, from any thread, a whole line at a time.
pub struct Reporter {
    json: bool,
    lock: Mutex<()>,
}

impl Reporter {
    pub fn new(json: bool) -> Reporter {
        Reporter { json, lock: Mutex::new(()) }
    }

    pub fn report(&self, event: &Event<'_>) {
        let line = if self.json {
            serde_json::to_string(event).expect("events always serialize")
        } else {
            human(event)
        };
        let _guard = self.lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out = std::io::stdout().lock();
        // A closed pipe (`| head`) is no reason to stop the machines.
        let _ = writeln!(out, "{line}").and_then(|()| out.flush());
    }
}

/// An event as a person reads it.
fn human(event: &Event<'_>) -> String {
    match event {
        Event::RunStarted { id, bundle, machines, tests } => format!(
            "Run {id}: {} on {}\nBundle: {}",
            plural(tests.len(), "test"),
            machines.join(", "),
            bundle.display()
        ),
        Event::TestStarted { machine, test } => format!("[{machine}] {test}: running"),
        Event::TestEnded { machine, test, result, reason, milliseconds } => {
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
        Event::RunEnded { id, bundle, summary } => format!(
            "Run {id} done: {} passed, {} failed, {} skipped, {} errors\nBundle: {}",
            summary.passed,
            summary.failed,
            summary.skipped,
            summary.errors,
            bundle.display()
        ),
    }
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 { format!("1 {noun}") } else { format!("{count} {noun}s") }
}

fn seconds(milliseconds: u64) -> String {
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
        };
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"event":"test-ended","machine":"qemu","test":"drivers/link-pull","result":"fail","reason":"step 2: no \"link down\"","milliseconds":61500}"#
        );
        assert_eq!(
            human(&event),
            "[qemu] drivers/link-pull: FAILED in 1m02s: step 2: no \"link down\""
        );
    }
}

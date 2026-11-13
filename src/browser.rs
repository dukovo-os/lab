// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The `browser` step: a script drives a page the machine serves in real
//! browsers on the lab PC, one engine after another.
//!
//! The lab PC runs `browser-runner.mjs`, which is runner.mjs as compiled
//! into this program, with node once for each engine the step names. The
//! runner opens the engine from the browser kit (`--browsers`), gives the
//! test's script, a file beside the test, an object to drive the page
//! and check it with axe-core, and says how it went in its exit status:
//! 0 for a pass, [`SKIP_STATUS`] for an engine that is not in the kit or
//! will not start on the lab PC, anything else for a failure. Its lines
//! name what it kept: `screenshot:` lines the screenshots, which go in
//! the bundle's `screenshots/<machine>/` with the test's others, and
//! `trace:` lines the traces of a failure, in `logs/<machine>/`, which
//! `npx playwright show-trace` opens.
//!
//! This module makes each engine's command line, reads what the runner
//! printed and folds the engines' outcomes into the step's; run.rs runs
//! the commands, as it runs a `host` step's. The test's log shows each
//! engine's output below a step that passed or failed, and the reason a
//! step was skipped. docs/test-files.md, "Browser tests", describes the
//! step for the people who write them.

use std::path::Path;

use crate::peer::SKIP_STATUS;
use crate::suite::Engine;

/// The runner script, which run.rs writes into the test's folder on the
/// lab PC before the first engine.
pub const RUNNER: &str = include_str!("browser/runner.mjs");

/// What every engine's runner of one step is told.
pub struct Runner<'a> {
    /// The runner script on the lab PC.
    pub file: &'a Path,
    /// The browser kit: `node_modules/playwright` and `axe-core`, and
    /// maybe `browsers`.
    pub kit: &'a Path,
    /// The page address, `https://host:port`.
    pub url: &'a str,
    /// The test's script.
    pub script: &'a Path,
    /// Where the runner puts the screenshots it keeps.
    pub shots: &'a Path,
    /// Where it puts the traces of a failure.
    pub logs: &'a Path,
    /// The test's id with its `/` made a `-`, which starts each file the
    /// runner keeps.
    pub name: &'a str,
}

impl Runner<'_> {
    /// The command that runs the runner for `engine`, each path quoted
    /// for `sh`.
    pub fn command(&self, engine: Engine) -> String {
        format!(
            "node {} --kit {} --engine {} --url {} --script {} --shots {} --logs {} --name {}",
            quoted(self.file.display()),
            quoted(self.kit.display()),
            engine.as_str(),
            quoted(self.url),
            quoted(self.script.display()),
            quoted(self.shots.display()),
            quoted(self.logs.display()),
            quoted(self.name),
        )
    }
}

/// `word` as one word of an `sh` command line.
fn quoted(word: impl std::fmt::Display) -> String {
    format!("'{}'", word.to_string().replace('\'', "'\\''"))
}

/// How the runner ended for one engine.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The script ran to its end; the screenshots it kept, by file name.
    Passed { screenshots: Vec<String> },
    /// The engine cannot run here; the runner's reason.
    Skipped(String),
    /// The script failed; the runner's message, and the screenshots kept:
    /// those the script took and the failure's own.
    Failed { message: String, screenshots: Vec<String> },
}

/// Reads what the runner printed (`text`) and how it ended (`status`).
pub fn outcome(status: i32, text: &str) -> Outcome {
    let lines = |prefix: &str| -> Vec<String> {
        text.lines().filter_map(|line| line.strip_prefix(prefix)).map(String::from).collect()
    };
    match status {
        0 => Outcome::Passed { screenshots: lines("screenshot: ") },
        SKIP_STATUS => {
            Outcome::Skipped(lines("skip: ").pop().unwrap_or_else(|| text.trim().to_string()))
        }
        _ => {
            let mut message = lines("fail: ").join("; ");
            // A runner that node itself stopped has printed none: what node
            // printed says why.
            if message.is_empty() {
                message =
                    format!("the runner ended with status {status} and printed:\n{}", text.trim());
            }
            Outcome::Failed { message, screenshots: lines("screenshot: ") }
        }
    }
}

/// How a step ended, over every engine it ran.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Passed in at least one engine and failed in none: what ran, and
    /// what was skipped.
    Passed(String),
    /// Every engine was skipped: why, for each.
    Skipped(String),
    /// At least one engine failed: why, one line for each.
    Failed(String),
}

/// Folds the engines' outcomes, in the order they ran, into the step's.
pub fn verdict(runs: &[(Engine, Outcome)]) -> Verdict {
    let mut passed = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();
    for (engine, outcome) in runs {
        match outcome {
            Outcome::Passed { .. } => passed.push(*engine),
            Outcome::Skipped(reason) => skipped.push((engine.as_str(), reason)),
            Outcome::Failed { message, .. } => {
                failed.push(format!("{}: {message}", engine.as_str()));
            }
        }
    }
    if !failed.is_empty() {
        return Verdict::Failed(failed.join("\n"));
    }
    if passed.is_empty() {
        let reasons: Vec<String> =
            skipped.iter().map(|(engine, reason)| format!("{engine}: {reason}")).collect();
        return Verdict::Skipped(reasons.join("; "));
    }
    let mut note = format!("passed in {}", Engine::list(&passed));
    if !skipped.is_empty() {
        let skips: Vec<String> =
            skipped.iter().map(|(engine, reason)| format!("{engine} ({reason})")).collect();
        note.push_str(&format!("; skipped {}", skips.join(", ")));
    }
    Verdict::Passed(note)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passed() -> Outcome {
        Outcome::Passed { screenshots: Vec::new() }
    }

    fn skipped(reason: &str) -> Outcome {
        Outcome::Skipped(reason.into())
    }

    fn failed(message: &str) -> Outcome {
        Outcome::Failed { message: message.into(), screenshots: Vec::new() }
    }

    #[test]
    fn a_command_names_the_engine_and_quotes_every_path() {
        let runner = Runner {
            file: Path::new("/work/browser-runner.mjs"),
            kit: Path::new("/home/lab's kit"),
            url: "https://127.0.0.1:40443",
            script: Path::new("/tests/web-console/browser.mjs"),
            shots: Path::new("/bundle/screenshots/qemu"),
            logs: Path::new("/bundle/logs/qemu"),
            name: "web-console-browser",
        };
        assert_eq!(
            runner.command(Engine::Firefox),
            "node '/work/browser-runner.mjs' --kit '/home/lab'\\''s kit' --engine firefox \
             --url 'https://127.0.0.1:40443' --script '/tests/web-console/browser.mjs' \
             --shots '/bundle/screenshots/qemu' --logs '/bundle/logs/qemu' \
             --name 'web-console-browser'"
        );
    }

    #[test]
    fn a_pass_names_the_screenshots_it_kept() {
        let text = "screenshot: a-chromium-dark.png\nscreenshot: a-chromium-light.png\npass\n";
        assert_eq!(
            outcome(0, text),
            Outcome::Passed {
                screenshots: vec!["a-chromium-dark.png".into(), "a-chromium-light.png".into()]
            }
        );
    }

    #[test]
    fn a_skip_gives_the_runner_s_last_reason() {
        assert_eq!(
            outcome(SKIP_STATUS, "skip: firefox is not in the browser kit\n"),
            skipped("firefox is not in the browser kit")
        );
        assert_eq!(outcome(SKIP_STATUS, "no reason given\n"), skipped("no reason given"));
    }

    #[test]
    fn a_failure_gives_every_message_and_the_screenshots_kept() {
        let text = "screenshot: a-chromium-dark.png\nfail: the heading is not visible\n\
                    fail: could not keep a-chromium-trace-1.zip: closed\n\
                    screenshot: a-chromium-failed-1.png\n";
        assert_eq!(
            outcome(1, text),
            Outcome::Failed {
                message:
                    "the heading is not visible; could not keep a-chromium-trace-1.zip: closed"
                        .into(),
                screenshots: vec!["a-chromium-dark.png".into(), "a-chromium-failed-1.png".into()],
            }
        );
        // Node's own message, when the runner printed none.
        assert_eq!(
            outcome(1, "Error: Cannot find module 'x'\n    at somewhere\n"),
            failed(
                "the runner ended with status 1 and printed:\nError: Cannot find module 'x'\n    at somewhere"
            )
        );
    }

    #[test]
    fn a_failure_in_one_engine_fails_the_step() {
        let runs = [
            (Engine::Chromium, passed()),
            (Engine::Firefox, failed("the heading is not visible")),
            (Engine::Webkit, skipped("webkit is not in the browser kit")),
        ];
        assert_eq!(verdict(&runs), Verdict::Failed("firefox: the heading is not visible".into()));
        let both = [
            (Engine::Chromium, failed("a")),
            (Engine::Firefox, passed()),
            (Engine::Webkit, failed("b")),
        ];
        assert_eq!(verdict(&both), Verdict::Failed("chromium: a\nwebkit: b".into()));
    }

    #[test]
    fn every_engine_skipped_skips_the_step() {
        let runs = [
            (Engine::Firefox, skipped("firefox is not in the browser kit")),
            (Engine::Webkit, skipped("webkit would not start on the lab PC: no libraries")),
        ];
        assert_eq!(
            verdict(&runs),
            Verdict::Skipped(
                "firefox: firefox is not in the browser kit; \
                 webkit: webkit would not start on the lab PC: no libraries"
                    .into()
            )
        );
    }

    #[test]
    fn the_note_names_what_was_skipped() {
        let runs = [
            (Engine::Chromium, passed()),
            (Engine::Firefox, skipped("firefox is not in the browser kit")),
            (Engine::Webkit, skipped("webkit is not in the browser kit")),
        ];
        assert_eq!(
            verdict(&runs),
            Verdict::Passed(
                "passed in chromium; skipped firefox (firefox is not in the browser kit), \
                 webkit (webkit is not in the browser kit)"
                    .into()
            )
        );
        let all =
            [(Engine::Chromium, passed()), (Engine::Firefox, passed()), (Engine::Webkit, passed())];
        assert_eq!(verdict(&all), Verdict::Passed("passed in chromium, firefox and webkit".into()));
    }
}

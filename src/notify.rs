// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The push notice at the end of a round, and `dukovo-lab notify --test`.
//!
//! `notify.toml` in the configuration folder names an ntfy topic
//! (`ntfy = "https://ntfy.sh/<topic>"`) and, optionally, the address the
//! notice opens when it is tapped (`link`, normally the runner page's
//! phone view; see docs/runner-page.md). Without the file nothing is
//! ever sent. A scheduled run (schedule.rs) and a `run --notify` end by
//! POSTing one short plain-text message to the topic: the counts in its
//! title, then up to [`LISTED`] new Dukovo failures and lab faults by test
//! and machine, then the machines benched this round. No log, no result
//! text and no address other than the owner's own `link` goes in it.
//!
//! The lab's HTTP client (http.rs) speaks plain HTTP only, and ntfy.sh is
//! HTTPS, so the POST is made by `curl`, which a lab PC needs for notices
//! and nothing else. Its arguments are fixed, and the topic, the link and the text
//! travel on its standard input as a curl config: arguments are visible
//! to other accounts on the PC while the program runs, and the topic is
//! the only secret a topic has. A notice that cannot be sent is the
//! caller's to log; it never fails a run.
//!
//! When a notice does not arrive, `dukovo-lab notify --test` shows
//! whether the lab or the phone is at fault, and the schedule's log
//! (`schedules/<name>.log` in the store) or the run's `output.log` holds
//! the reason.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;
use serde_json::json;

use crate::bundle::Summary;
use crate::findings::Class;
use crate::machines::Machine;
use crate::status::State;
use crate::{Arguments, Error, fleet, grid, status};

/// How many failures and faults a notice lists; a phone shows little.
const LISTED: usize = 5;

/// How long curl may take in all, so a dead network never holds a
/// schedule up.
const SEND_SECONDS: &str = "20";

/// `notify.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// The topic's address, `https://ntfy.sh/<topic>` or a server of your own.
    ntfy: String,
    /// The address a tap on the notice opens.
    link: Option<String>,
}

/// One message to send.
#[derive(Debug, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub body: String,
    /// ntfy's 1 (min) to 5 (max).
    pub priority: u8,
    /// ntfy shows each of these emoji short codes beside the title.
    pub tags: Vec<&'static str>,
}

fn settings_file(config: &Path) -> PathBuf {
    config.join("notify.toml")
}

/// Whether `address` is an http or https address with nothing in it that
/// could end a header or a quoted word.
fn plain_address(address: &str) -> bool {
    (address.starts_with("https://") || address.starts_with("http://"))
        && address.chars().all(|c| c.is_ascii_graphic())
        && !address.contains('"')
}

/// Reads `notify.toml`; none when the lab has no such file.
///
/// # Errors
///
/// A file that does not read, or names an address that is not http or https.
pub fn load(config: &Path) -> Result<Option<Settings>, Error> {
    let file = settings_file(config);
    let text = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(Error::Harness(format!("reading {}: {err}", file.display()))),
    };
    let settings: Settings =
        toml::from_str(&text).map_err(|err| Error::Usage(format!("{}: {err}", file.display())))?;
    for (name, address) in [("ntfy", Some(&settings.ntfy)), ("link", settings.link.as_ref())] {
        if let Some(address) = address
            && !plain_address(address)
        {
            return Err(Error::Usage(format!(
                "{}: {name} must be an http or https address without spaces or quotes",
                file.display()
            )));
        }
    }
    Ok(Some(settings))
}

/// Whether a valid `notify.toml` is in place, for the runner page's line
/// about the notice.
pub fn is_set_up(config: &Path) -> bool {
    matches!(load(config), Ok(Some(_)))
}

/// Sends the notice for run `id` when `notify.toml` is in place: answers
/// whether it sent one, or why it could not.
///
/// # Errors
///
/// The reason, in a sentence, when the file is wrong, the run cannot be
/// read, or the topic does not take the message.
pub fn tell(arguments: &Arguments, id: &str) -> Result<bool, String> {
    let config = arguments.config_dir().map_err(error_text)?;
    let Some(settings) = load(&config).map_err(error_text)? else { return Ok(false) };
    let machines = arguments.load_machines().map_err(error_text)?;
    let store = arguments.store_dir().map_err(error_text)?;
    let notice = round_notice(&store, &machines, id).map_err(error_text)?;
    settings.send(&notice)?;
    Ok(true)
}

/// Sends the notice that schedule `name` could not start a round, with the
/// first line of `why`; answers whether it sent one, or why it could not.
///
/// # Errors
///
/// The reason, in a sentence, when the file is wrong or the topic does not
/// take the message.
pub fn did_not_start(arguments: &Arguments, name: &str, why: &str) -> Result<bool, String> {
    let config = arguments.config_dir().map_err(error_text)?;
    let Some(settings) = load(&config).map_err(error_text)? else { return Ok(false) };
    let notice = Notice {
        title: format!("Lab: the {name} round did not start"),
        body: plain_reason(why),
        priority: 4,
        tags: vec!["warning"],
    };
    settings.send(&notice)?;
    Ok(true)
}

/// The first line of `why` without what must not leave the PC: the log
/// path a reason ends with, and any word that is a path, an address or
/// a URL.
fn plain_reason(why: &str) -> String {
    let first = why.lines().next().unwrap_or_default();
    let first = first.split("; see ").next().unwrap_or(first);
    let address = |word: &str| {
        word.contains(['/', '~'])
            || word.contains('@')
            || word.split('.').filter(|part| part.chars().all(|c| c.is_ascii_digit())).count() >= 4
    };
    let words: Vec<&str> = first.split_whitespace().filter(|word| !address(word)).collect();
    let text: String = words.join(" ").chars().take(200).collect();
    if text.is_empty() { "No reason was given.".into() } else { text }
}

fn error_text(err: Error) -> String {
    match err {
        Error::Usage(why) | Error::Harness(why) | Error::Busy(why) => why,
    }
}

/// The notice for run `id`.
fn round_notice(store: &Path, machines: &[Machine], id: &str) -> Result<Notice, Error> {
    let results = status::results(store, id)?;
    let summary = Summary::of(&results.results);
    let failing = summary.failed + summary.errors;
    let new = grid::new_reds(store, machines, id);
    let class_of = |test: &str, machine: &str| {
        results.classes.iter().find(|c| c.test == test && c.machine == machine).map(|c| c.class)
    };
    // Dukovo's failures that are new, and the lab's own faults: what needs
    // a look. The rest are known, flaky or a test's own fault.
    let mut listed: Vec<String> = Vec::new();
    let mut new_failures = 0;
    for result in &results.results {
        match class_of(&result.test, &result.machine) {
            Some(Class::Dukovo) if new.contains(&(result.test.clone(), result.machine.clone())) => {
                new_failures += 1;
                listed.push(format!("New: {} on {}", result.test, result.machine));
            }
            Some(Class::Lab) => {
                listed.push(format!("Lab fault: {} on {}", result.test, result.machine));
            }
            _ => {}
        }
    }
    let benched: Vec<String> = fleet::fleet(store, machines)
        .machines
        .into_iter()
        .filter(|machine| machine.benched.as_ref().is_some_and(|benched| benched.run == id))
        .map(|machine| machine.name)
        .collect();
    let mut lines: Vec<String> = listed.iter().take(LISTED).cloned().collect();
    if listed.len() > LISTED {
        lines.push(format!("and {} more", listed.len() - LISTED));
    }
    if !benched.is_empty() {
        lines.push(format!("Benched: {}", benched.join(", ")));
    }
    if lines.is_empty() {
        lines.push("Nothing new to look at.".into());
    }
    let mut title = format!("Lab: {} passed, ", summary.passed);
    if failing == 0 {
        title.push_str("none failing");
    } else {
        title.push_str(&format!("{failing} failing"));
        if new_failures > 0 {
            title.push_str(&format!(" ({new_failures} new)"));
        }
    }
    match results.state {
        State::Stopped => title.push_str("; stopped early"),
        State::Died => title.push_str("; the run died"),
        State::Running | State::Ended => {}
    }
    let faults = summary.errors > 0 || !benched.is_empty();
    let mut tags = vec![if failing > 0 { "x" } else { "white_check_mark" }];
    if faults {
        tags.push("warning");
    }
    Ok(Notice {
        title,
        body: lines.join("\n"),
        priority: if new_failures > 0 || faults { 4 } else { 3 },
        tags,
    })
}

/// Sends a test notice, so the owner can see it on their phone.
pub fn command(arguments: &Arguments) -> Result<u8, Error> {
    if !arguments.test_notice {
        return Err(Error::Usage(
            "notify needs --test: it sends a test notice to the topic in notify.toml".into(),
        ));
    }
    let config = arguments.config_dir()?;
    let Some(settings) = load(&config)? else {
        return Err(Error::Usage(format!(
            "there is no {}: it names the ntfy topic notices go to (docs/schedules.md shows one)",
            settings_file(&config).display()
        )));
    };
    let notice = Notice {
        title: "Lab: test notice".into(),
        body: "This notice came from the lab PC. If it is on your phone, notices work.".into(),
        priority: 3,
        tags: vec!["white_check_mark"],
    };
    settings.send(&notice).map_err(Error::Harness)?;
    if arguments.json {
        println!("{}", json!({ "sent": true }));
    } else {
        println!("Sent a test notice. It should be on your phone in a moment.");
    }
    Ok(0)
}

/// `text` as the inside of a quoted word in a curl config: its backslash
/// and quote escaped, its line ends written out.
fn quoted(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// `text` as a header's value: one line, no control characters.
fn header_value(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

impl Settings {
    /// The curl config that POSTs `notice`: ntfy's headers, and the text
    /// as the body. `data-raw` is used because `data-binary` reads a file
    /// when its text starts with `@`.
    fn curl_config(&self, notice: &Notice) -> String {
        let mut config = format!("url = \"{}\"\n", quoted(&self.ntfy));
        let mut header = |name: &str, value: &str| {
            config.push_str(&format!("header = \"{name}: {}\"\n", quoted(&header_value(value))));
        };
        header("Title", &notice.title);
        header("Tags", &notice.tags.join(","));
        header("Priority", &notice.priority.to_string());
        header("Content-Type", "text/plain; charset=utf-8");
        if let Some(link) = &self.link {
            header("Click", link);
        }
        config.push_str(&format!("data-raw = \"{}\"\n", quoted(&notice.body)));
        config
    }

    /// POSTs `notice` to the topic.
    fn send(&self, notice: &Notice) -> Result<(), String> {
        let mut curl = Command::new("curl")
            // -q first: a .curlrc of the PC's account must not add options.
            .args(["-q", "--silent", "--show-error", "--fail"])
            .args(["--connect-timeout", "10", "--max-time", SEND_SECONDS])
            .args(["--proto", "=http,https", "--config", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| match err.kind() {
                ErrorKind::NotFound => "no notice was sent: curl is not installed on the lab PC; install it (`sudo apt install curl`)".into(),
                _ => format!("no notice was sent: starting curl: {err}"),
            })?;
        let mut stdin = curl.stdin.take().ok_or("no notice was sent: curl has no input")?;
        let written = stdin.write_all(self.curl_config(notice).as_bytes());
        // Closed so curl sees the end of its config.
        drop(stdin);
        let output = curl.wait_with_output().map_err(|err| format!("waiting for curl: {err}"))?;
        if output.status.success() {
            return written.map_err(|err| format!("no notice was sent: {err}"));
        }
        let said = String::from_utf8_lossy(&output.stderr);
        let said = said.lines().next().unwrap_or("it gave no reason").trim_start_matches("curl: ");
        Err(format!("the notice was not sent: curl said: {said}"))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::bundle::{Bundle, MachineRecord, Outcome, ResultRecord, RunRecord, SCHEMA};
    use crate::testing::TempDir;

    /// The requests a test server has seen, each as text.
    type Seen = Arc<Mutex<Vec<String>>>;

    /// A server on this PC that answers every request with `status` and
    /// keeps each request as text: its request line, headers and body.
    fn topic(status: u16) -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}/secret-topic", listener.local_addr().unwrap());
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let kept = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(&stream);
                let mut head = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    head.push_str(&line);
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                kept.lock().unwrap().push(format!("{head}\n{}", String::from_utf8_lossy(&body)));
                let _ = (&stream).write_all(
                    format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                );
            }
        });
        (address, seen)
    }

    fn arguments(config: &Path, store: &Path) -> Arguments {
        Arguments {
            config: Some(config.to_path_buf()),
            store: Some(store.to_path_buf()),
            ..Arguments::default()
        }
    }

    fn result(test: &str, machine: &str, outcome: Outcome, reason: &str) -> ResultRecord {
        ResultRecord {
            test: test.into(),
            machine: machine.into(),
            result: outcome,
            reason: (outcome != Outcome::Pass).then(|| reason.into()),
            milliseconds: 1000,
            boot: Some(1),
            log: None,
            steps: Vec::new(),
            screenshots: Vec::new(),
            rerun: None,
        }
    }

    /// Writes an ended run of `results` on one machine, `n` minutes into
    /// 2026-11-07, as the harness would, and answers its id.
    fn write_run(store: &Path, n: u32, results: &[ResultRecord]) -> String {
        let id = format!("20261107-{n:02}0000-000{n}");
        let record = RunRecord {
            schema: SCHEMA,
            id: id.clone(),
            harness: "dukovo-lab 0.1.0".into(),
            started: format!("2026-11-07T{n:02}:00:00Z"),
            ended: Some(format!("2026-11-07T{n:02}:30:00Z")),
            build: None,
            image: "lab.img".into(),
            machines: vec![MachineRecord {
                name: "box-1".into(),
                about: String::new(),
                driver: "amt".into(),
            }],
            pools: Vec::new(),
            tests: Vec::new(),
            preset: None,
            schedule: None,
            summary: Some(Summary::of(results)),
            stopped: false,
            resumes: None,
            options: None,
        };
        let bundle = Bundle::create(store, record).unwrap();
        bundle.write_results(results).unwrap();
        bundle.write_run().unwrap();
        crate::findings::record(store, &bundle.record, results).unwrap();
        id
    }

    #[test]
    fn a_notice_has_the_counts_in_its_title_and_what_is_new_in_its_body() {
        let store = TempDir::new("notify-shape");
        let ok = |test: &str| result(test, "box-1", Outcome::Pass, "");
        write_run(store.path(), 1, &[ok("boot/a"), ok("boot/b"), ok("disk/c")]);
        let id = write_run(
            store.path(),
            2,
            &[
                ok("boot/a"),
                result("boot/b", "box-1", Outcome::Fail, "shell never came up"),
                result("disk/c", "box-1", Outcome::Error, "AMT did not answer at 10.0.0.9"),
            ],
        );
        let notice = round_notice(store.path(), &[], &id).unwrap();
        assert_eq!(notice.title, "Lab: 1 passed, 2 failing (1 new)");
        assert_eq!(notice.body, "New: boot/b on box-1\nLab fault: disk/c on box-1");
        assert_eq!((notice.priority, notice.tags), (4, vec!["x", "warning"]));
        // Why a test failed stays on the lab PC, addresses included.
        assert!(!notice.body.contains("10.0.0.9") && !notice.body.contains("shell"));
    }

    #[test]
    fn a_notice_lists_five_failures_and_counts_the_rest() {
        let store = TempDir::new("notify-cap");
        let tests: Vec<String> = (1..=8).map(|n| format!("boot/t{n}")).collect();
        let passing: Vec<ResultRecord> =
            tests.iter().map(|test| result(test, "box-1", Outcome::Pass, "")).collect();
        write_run(store.path(), 1, &passing);
        let failing: Vec<ResultRecord> = tests
            .iter()
            .map(|test| result(test, "box-1", Outcome::Fail, &format!("{test} is red")))
            .collect();
        let id = write_run(store.path(), 2, &failing);
        let notice = round_notice(store.path(), &[], &id).unwrap();
        assert_eq!(notice.title, "Lab: 0 passed, 8 failing (8 new)");
        let lines: Vec<&str> = notice.body.lines().collect();
        assert_eq!(lines.len(), LISTED + 1);
        assert_eq!(lines[0], "New: boot/t1 on box-1");
        assert_eq!(lines[LISTED], "and 3 more");
    }

    #[test]
    fn a_failure_that_was_red_the_round_before_is_not_new() {
        let store = TempDir::new("notify-old");
        let red = |reason: &str| [result("boot/a", "box-1", Outcome::Fail, reason)];
        write_run(store.path(), 1, &red("same reason"));
        let id = write_run(store.path(), 2, &red("same reason"));
        let notice = round_notice(store.path(), &[], &id).unwrap();
        assert_eq!(notice.title, "Lab: 0 passed, 1 failing");
        assert_eq!(notice.body, "Nothing new to look at.");
        assert_eq!(notice.priority, 3);
    }

    #[test]
    fn a_green_round_says_nothing_failed() {
        let store = TempDir::new("notify-green");
        let id = write_run(store.path(), 1, &[result("boot/a", "box-1", Outcome::Pass, "")]);
        let notice = round_notice(store.path(), &[], &id).unwrap();
        assert_eq!(notice.title, "Lab: 1 passed, none failing");
        assert_eq!(notice.tags, vec!["white_check_mark"]);
    }

    #[test]
    fn posts_the_notice_to_the_topic_with_ntfy_s_headers() {
        let (address, seen) = topic(200);
        let settings =
            Settings { ntfy: address, link: Some("http://100.64.0.7:7420/?key=ab#phone".into()) };
        let notice = Notice {
            title: "Lab: 3 passed, 1 failing (1 new)".into(),
            body: "@New: boot/a on \"box\"\nand 2 more".into(),
            priority: 4,
            tags: vec!["x", "warning"],
        };
        settings.send(&notice).unwrap();
        let seen = seen.lock().unwrap();
        let request = seen[0].replace("\r\n", "\n");
        assert!(request.starts_with("POST /secret-topic HTTP/1.1\n"), "{request}");
        for header in [
            "Title: Lab: 3 passed, 1 failing (1 new)",
            "Tags: x,warning",
            "Priority: 4",
            "Click: http://100.64.0.7:7420/?key=ab#phone",
            "Content-Type: text/plain; charset=utf-8",
        ] {
            assert!(request.contains(header), "{header}\n{request}");
        }
        // The body is the text as it is, even when it starts with @.
        assert!(request.ends_with("\n@New: boot/a on \"box\"\nand 2 more"), "{request}");
    }

    #[test]
    fn leaves_out_the_click_header_when_there_is_no_link() {
        let (address, seen) = topic(200);
        let settings = Settings { ntfy: address, link: None };
        let notice = Notice { title: "t".into(), body: "b".into(), priority: 3, tags: vec![] };
        settings.send(&notice).unwrap();
        assert!(!seen.lock().unwrap()[0].to_ascii_lowercase().contains("click:"));
    }

    #[test]
    fn a_header_value_stays_on_one_line() {
        let notice = Notice {
            title: "one\r\nX-Evil: 1".into(),
            body: String::new(),
            priority: 3,
            tags: vec![],
        };
        let settings = Settings { ntfy: "http://127.0.0.1:1/t".into(), link: None };
        assert!(settings.curl_config(&notice).contains("header = \"Title: oneX-Evil: 1\"\n"));
    }

    #[test]
    fn a_failed_send_is_an_error_and_leaves_the_run_alone() {
        let (address, _) = topic(500);
        let settings = Settings { ntfy: address, link: None };
        let notice = Notice { title: "t".into(), body: "b".into(), priority: 3, tags: vec![] };
        let why = settings.send(&notice).unwrap_err();
        assert!(why.starts_with("the notice was not sent: curl said: "), "{why}");
        // The topic is the only secret; the reason does not repeat it.
        assert!(!why.contains("secret-topic"), "{why}");

        // `tell` hands the reason back for the caller to log; the run's
        // own result is not in its hands.
        let config = TempDir::new("notify-failed-config");
        let store = TempDir::new("notify-failed-store");
        config.write("notify.toml", &format!("ntfy = \"{}\"\n", settings.ntfy));
        let id = write_run(store.path(), 1, &[result("boot/a", "box-1", Outcome::Fail, "red")]);
        let why = tell(&arguments(config.path(), store.path()), &id).unwrap_err();
        assert!(why.starts_with("the notice was not sent"), "{why}");
    }

    #[test]
    fn a_round_that_did_not_start_is_told_in_one_line_without_paths_or_addresses() {
        let (address, seen) = topic(200);
        let config = TempDir::new("notify-nostart-config");
        let store = TempDir::new("notify-nostart-store");
        let args = arguments(config.path(), store.path());
        assert!(matches!(did_not_start(&args, "nightly", "x"), Ok(false)));
        config.write("notify.toml", &format!("ntfy = \"{address}\"\n"));
        let why = "the image did not build or the run did not start (exit 2); see /home/lab/.local/share/dukovo-lab/schedules/nightly.log\nsecond line";
        assert!(matches!(did_not_start(&args, "nightly", why), Ok(true)));
        let why = "git: could not read from https://github.com/x/y.git at 10.1.2.3 as me@host";
        assert!(matches!(did_not_start(&args, "nightly", why), Ok(true)));
        let seen = seen.lock().unwrap();
        let first = seen[0].replace("\r\n", "\n");
        assert!(first.contains("Title: Lab: the nightly round did not start"), "{first}");
        assert!(first.contains("Priority: 4"), "{first}");
        assert!(
            first.ends_with("\n\nthe image did not build or the run did not start (exit 2)"),
            "{first}"
        );
        assert!(seen[1].ends_with("\n\ngit: could not read from at as"), "{}", seen[1]);
    }

    #[test]
    fn sends_nothing_without_a_notify_file() {
        let config = TempDir::new("notify-none-config");
        let store = TempDir::new("notify-none-store");
        let id = write_run(store.path(), 1, &[result("boot/a", "box-1", Outcome::Pass, "")]);
        assert!(matches!(tell(&arguments(config.path(), store.path()), &id), Ok(false)));
        assert!(!is_set_up(config.path()));
        let test = Arguments { test_notice: true, ..arguments(config.path(), store.path()) };
        assert!(matches!(command(&test), Err(Error::Usage(why)) if why.contains("notify.toml")));
    }

    #[test]
    fn sends_the_notice_of_an_ended_run_when_a_notify_file_is_there() {
        let (address, seen) = topic(200);
        let config = TempDir::new("notify-sends-config");
        let store = TempDir::new("notify-sends-store");
        config.write("notify.toml", &format!("ntfy = \"{address}\"\n"));
        let id = write_run(store.path(), 1, &[result("boot/a", "box-1", Outcome::Pass, "")]);
        assert!(matches!(tell(&arguments(config.path(), store.path()), &id), Ok(true)));
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert!(is_set_up(config.path()));
    }

    #[test]
    fn refuses_a_notify_file_that_is_not_an_address() {
        let config = TempDir::new("notify-bad");
        for text in [
            "ntfy = \"ntfy.sh/topic\"\n",
            "ntfy = \"https://ntfy.sh/a b\"\n",
            "ntfy = \"https://ntfy.sh/t\"\nlink = \"javascript:1\"\n",
            "nfty = \"https://ntfy.sh/t\"\n",
        ] {
            config.write("notify.toml", text);
            assert!(matches!(load(config.path()), Err(Error::Usage(_))), "{text}");
        }
    }
}

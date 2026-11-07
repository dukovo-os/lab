// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Schedules: tests that run by themselves, at a time of day in the lab's
//! zone (Mountain Time) or on each new build on Dukovo's main branch, with
//! the runner page closed.
//!
//! ```text
//! <config>/schedules.json          the schedules (see docs/schedules.md)
//! <store>/schedules/<name>.json    how each is doing: when it last ran,
//!                                  what came of it, what it is doing now
//! <store>/schedules/<name>.lock    held while the schedule is going
//! <store>/schedules/<name>.log     what its last start printed
//! <store>/dukovo/                  the lab's own Dukovo checkout
//! ```
//!
//! A systemd timer (`schedule install` puts it in place) runs `schedule
//! tick` each minute. The tick only decides: a schedule whose time has
//! come, or whose branch has a new commit, is started in a process of its
//! own (`schedule go`), and the tick ends. That process waits for its
//! machines if a run holds them, brings the lab's own checkout up to main,
//! builds the lab image and runs the tests there with `ob lab run`, then
//! sends the run's findings to lab-results, or holds them, as the schedule
//! says. Scheduled runs never run Needs you tests: nobody is there.
//!
//! The lab's own checkout is never one a person works in: it is reset to
//! main on each start, so nothing typed in it would last. Scheduled runs
//! take it one at a time.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::clock::Utc;
use crate::machines::is_plain_name;
use crate::zone::{self, Zone};
use crate::{Arguments, Error, findings, send, store};

/// Where the lab's own checkout comes from unless the schedules file says.
pub const SOURCE: &str = "https://github.com/dukovo-os/dukovo.git";

/// The branch each-build schedules follow.
const BRANCH: &str = "main";

/// A time missed by more than this (the lab PC was off) is not made up.
const CATCH_UP: u64 = 12 * 3600;

/// How often an each-build schedule looks for a new commit on main.
const LOOK_EVERY: u64 = 5 * 60;

/// How long a start waits for busy machines before it gives up, and how
/// often it looks.
const WAIT_AT_MOST: u64 = 12 * 3600;
const WAIT_PAUSE: Duration = Duration::from_secs(30);

/// The systemd units `schedule install` writes.
const SERVICE: &str = "dukovo-lab-schedules.service";
const TIMER: &str = "dukovo-lab-schedules.timer";

/// Days of the week as schedules name them, Monday first.
const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// `schedules.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedules {
    /// The zone times are in; America/Denver when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
    /// Where the lab's own Dukovo checkout is cloned from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub schedules: Vec<Schedule>,
}

/// One schedule.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    /// Its name in commands: lowercase letters, digits and dashes.
    pub name: String,
    /// What people call it, such as "Full run"; the preset's title when
    /// left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// What runs: tests and categories, a preset, or every test.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub all: bool,
    /// The machines it runs on, all at once.
    pub machines: Vec<String>,
    /// When: a local time, `HH:MM`, on `days` (every day when empty)...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub days: Vec<String>,
    /// ...or on each new commit on main.
    #[serde(default, skip_serializing_if = "is_false")]
    pub each_build: bool,
    /// Whether its findings go to lab-results when it ends; if not, they
    /// are held for a person to send from the page.
    pub send: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub paused: bool,
}

// serde hands skip_serializing_if a reference.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

/// `<store>/schedules/<name>.json`: how a schedule is doing.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// The last due time the timer dealt with: started, or found missed.
    #[serde(default)]
    pub handled: Option<String>,
    /// Each-build: the newest commit on main it has seen, and when it
    /// last looked.
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub looked: Option<String>,
    /// What a start is doing now. It says nothing once the schedule's
    /// lock is free: then the start has ended, or died.
    #[serde(default)]
    pub going: Option<Going>,
    #[serde(default)]
    pub last: Option<Last>,
}

/// A start that is going.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Going {
    pub started: String,
    /// In words: "waiting for qemu, busy with run …", "building the image
    /// and running", and so on.
    pub doing: String,
    #[serde(default)]
    pub run: Option<String>,
}

/// How the last start went.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Last {
    pub started: String,
    pub ended: String,
    #[serde(default)]
    pub run: Option<String>,
    /// The commit of main it tested.
    #[serde(default)]
    pub commit: Option<String>,
    /// passed, failed, broke (the lab broke), stopped, missed (the lab PC
    /// was off at its time) or error (it could not run: the build failed,
    /// say).
    pub result: String,
    /// The run's findings.
    #[serde(default)]
    pub findings: Vec<String>,
    /// Whether they went to lab-results; none when there were none to send
    /// or the schedule holds them.
    #[serde(default)]
    pub sent: Option<bool>,
    /// What else there is to say, such as why it could not run or send.
    #[serde(default)]
    pub note: Option<String>,
}

impl Schedule {
    /// Checks a schedule from a command, the page or the file.
    pub fn check(&self) -> Result<(), String> {
        if !is_plain_name(&self.name) || self.name.len() > 40 {
            return Err(format!(
                "\"{}\" is not a schedule name: up to 40 lowercase letters, digits and dashes",
                self.name
            ));
        }
        let picks = [!self.tests.is_empty(), self.preset.is_some(), self.all];
        if picks.iter().filter(|&&pick| pick).count() != 1 {
            return Err(format!(
                "schedule {}: give tests, a preset or every test: one of them",
                self.name
            ));
        }
        let plain_test =
            |test: &String| test.split('/').count() <= 2 && test.split('/').all(is_plain_name);
        if !self.tests.iter().all(plain_test) || !self.preset.as_deref().is_none_or(is_plain_name) {
            return Err(format!("schedule {}: not a test or preset name", self.name));
        }
        if self.machines.is_empty() || !self.machines.iter().all(|name| is_plain_name(name)) {
            return Err(format!("schedule {}: give the machines it runs on", self.name));
        }
        match (&self.at, self.each_build) {
            (Some(at), false) => {
                minute_of(at).ok_or_else(|| {
                    format!("schedule {}: \"{at}\" is not a time such as 01:00", self.name)
                })?;
            }
            (None, true) if self.days.is_empty() => {}
            (None, true) => {
                return Err(format!("schedule {}: days go with a time, not each build", self.name));
            }
            _ => {
                return Err(format!(
                    "schedule {}: give a time (--at) or each build (--each-build): one of them",
                    self.name
                ));
            }
        }
        if let Some(day) = self.days.iter().find(|day| !DAYS.contains(&day.as_str())) {
            return Err(format!(
                "schedule {}: \"{day}\" is not a day; the days are {}",
                self.name,
                DAYS.join(", ")
            ));
        }
        if let Some(title) = &self.title
            && (title.chars().count() > 80 || title.contains(['\n', '\r']) || title.is_empty())
        {
            return Err(format!("schedule {}: keep the title to one line of 80", self.name));
        }
        Ok(())
    }

    /// The local time, as minutes after midnight, for a timed schedule.
    fn minute(&self) -> Option<u32> {
        self.at.as_deref().and_then(minute_of)
    }

    /// Whether it runs on weekday `weekday` (0 is Monday).
    fn runs_on(&self, weekday: u32) -> bool {
        self.days.is_empty() || self.days.iter().any(|day| day == DAYS[weekday as usize])
    }

    /// The last time it was due, at or before `now`, in the past week.
    pub fn last_due(&self, zone: &Zone, now: Utc) -> Option<Utc> {
        let minute = self.minute()?;
        let today = zone.day_of(now);
        (0..=8)
            .map(|back| today - back)
            .filter(|&day| self.runs_on(zone::weekday(day)))
            .map(|day| zone.moment(day, minute))
            .find(|&moment| moment <= now)
    }

    /// The next time it is due, after `now`.
    pub fn next_due(&self, zone: &Zone, now: Utc) -> Option<Utc> {
        let minute = self.minute()?;
        let today = zone.day_of(now);
        (0..=8)
            .map(|ahead| today + ahead)
            .filter(|&day| self.runs_on(zone::weekday(day)))
            .map(|day| zone.moment(day, minute))
            .find(|&moment| moment > now)
    }

    /// When it runs, in words: "Each new build on main", "Every day, 1:00
    /// AM", "Sundays, 2:00 AM", "Mon, Wed, Fri, 1:00 AM".
    pub fn when(&self) -> String {
        if self.each_build {
            return "Each new build on main".into();
        }
        let time = self.minute().map(clock_text).unwrap_or_default();
        let mut days: Vec<usize> =
            self.days.iter().filter_map(|day| DAYS.iter().position(|known| known == day)).collect();
        days.sort_unstable();
        days.dedup();
        let on = match days.as_slice() {
            [] | [0, 1, 2, 3, 4, 5, 6] => "Every day".to_string(),
            [0, 1, 2, 3, 4] => "Weekdays".to_string(),
            [5, 6] => "Weekends".to_string(),
            [day] => format!("{}s", DAY_NAMES[*day]),
            days => days.iter().map(|&day| &DAY_NAMES[day][..3]).collect::<Vec<_>>().join(", "),
        };
        format!("{on}, {time}")
    }

    /// What it runs, as `run` takes it.
    fn picks(&self) -> Vec<String> {
        if self.all {
            vec!["--all".into()]
        } else if let Some(preset) = &self.preset {
            vec!["--preset".into(), preset.clone()]
        } else {
            self.tests.clone()
        }
    }
}

const DAY_NAMES: [&str; 7] =
    ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

/// `HH:MM`, 24-hour, as minutes after midnight.
fn minute_of(at: &str) -> Option<u32> {
    let (hour, minute) = at.split_once(':')?;
    if hour.is_empty() || hour.len() > 2 || minute.len() != 2 {
        return None;
    }
    let (hour, minute): (u32, u32) = (hour.parse().ok()?, minute.parse().ok()?);
    (hour < 24 && minute < 60).then_some(hour * 60 + minute)
}

/// Minutes after midnight as a 12-hour clock: "1:00 AM".
fn clock_text(minute: u32) -> String {
    let (hour, minute) = (minute / 60, minute % 60);
    let half = if hour < 12 { "AM" } else { "PM" };
    let hour = match hour % 12 {
        0 => 12,
        hour => hour,
    };
    format!("{hour}:{minute:02} {half}")
}

/// A moment in local time, for people: "Sat Nov 7, 1:00 AM".
fn local_text(zone: &Zone, at: Utc) -> String {
    const MONTHS: [&str; 12] =
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let local = at.seconds() as i64 + zone.offset_at(at);
    let day = local.div_euclid(86_400);
    let (_, month, date) = crate::clock::civil_from_days(day);
    let minute = (local.rem_euclid(86_400) / 60) as u32;
    let weekday = &DAY_NAMES[zone::weekday(day) as usize][..3];
    format!("{weekday} {} {date}, {}", MONTHS[month as usize - 1], clock_text(minute))
}

/// A time to come, for people, from `now`: "Today, 1:00 AM",
/// "Tomorrow, 1:00 AM", else as [`local_text`] says it.
fn ahead_text(zone: &Zone, now: Utc, at: Utc) -> String {
    let local = at.seconds() as i64 + zone.offset_at(at);
    let clock = clock_text((local.rem_euclid(86_400) / 60) as u32);
    match zone.day_of(at) - zone.day_of(now) {
        0 => format!("Today, {clock}"),
        1 => format!("Tomorrow, {clock}"),
        _ => local_text(zone, at),
    }
}

// ---------------------------------------------------------------------
// Files.

fn schedules_file(config: &Path) -> PathBuf {
    config.join("schedules.json")
}

fn folder(store: &Path) -> PathBuf {
    store.join("schedules")
}

fn state_file(store: &Path, name: &str) -> PathBuf {
    folder(store).join(format!("{name}.json"))
}

fn going_lock(store: &Path, name: &str) -> PathBuf {
    folder(store).join(format!("{name}.lock"))
}

/// What a schedule's last start printed.
pub fn log_file(store: &Path, name: &str) -> PathBuf {
    folder(store).join(format!("{name}.log"))
}

/// The lab's own Dukovo checkout.
pub fn checkout(store: &Path) -> PathBuf {
    store.join("dukovo")
}

/// Reads the schedules; none when there is no file.
pub fn load(config: &Path) -> Result<Schedules, Error> {
    let file = schedules_file(config);
    let text = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Schedules::default()),
        Err(err) => return Err(Error::Harness(format!("reading {}: {err}", file.display()))),
    };
    let schedules: Schedules = serde_json::from_str(&text)
        .map_err(|err| Error::Usage(format!("{}: {err}", file.display())))?;
    for schedule in &schedules.schedules {
        schedule.check().map_err(|why| Error::Usage(format!("{}: {why}", file.display())))?;
    }
    Ok(schedules)
}

impl Schedules {
    fn zone(&self) -> Result<Zone, Error> {
        Zone::load(self.zone.as_deref().unwrap_or(zone::DEFAULT)).map_err(Error::Usage)
    }

    fn source(&self) -> &str {
        self.source.as_deref().unwrap_or(SOURCE)
    }

    fn find(&self, name: &str) -> Result<&Schedule, Error> {
        self.schedules.iter().find(|schedule| schedule.name == name).ok_or_else(|| {
            Error::Usage(format!("no schedule named \"{name}\"; `dukovo-lab schedule` lists them"))
        })
    }
}

/// Holds `path`'s lock until the answer is dropped, waiting for it.
fn wait_lock(path: &Path) -> Result<File, Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| Error::Harness(format!("making {}: {err}", parent.display())))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|err| Error::Harness(format!("opening {}: {err}", path.display())))?;
    file.lock().map_err(|err| Error::Harness(format!("locking {}: {err}", path.display())))?;
    Ok(file)
}

/// Writes `text` to `path` whole: to a file beside it, then renamed, so
/// a reader never sees half.
fn write_whole(path: &Path, text: &str) -> Result<(), Error> {
    let fresh = path.with_extension("new");
    fs::write(&fresh, text)
        .and_then(|()| fs::rename(&fresh, path))
        .map_err(|err| Error::Harness(format!("writing {}: {err}", path.display())))
}

/// Changes the schedules file, one change at a time.
fn edit_schedules<T>(
    config: &Path,
    change: impl FnOnce(&mut Schedules) -> Result<T, Error>,
) -> Result<T, Error> {
    let _lock = wait_lock(&config.join("schedules.lock"))?;
    let mut schedules = load(config)?;
    let answer = change(&mut schedules)?;
    let text = serde_json::to_string_pretty(&schedules).expect("schedules always serialize");
    write_whole(&schedules_file(config), &format!("{text}\n"))?;
    Ok(answer)
}

/// Reads a schedule's state; a fresh one when there is none.
pub fn load_state(store: &Path, name: &str) -> State {
    fs::read_to_string(state_file(store, name))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Changes a schedule's state, one change at a time: the tick and the
/// schedule's start both write it.
fn edit_state<T>(
    store: &Path,
    name: &str,
    change: impl FnOnce(&mut State) -> T,
) -> Result<T, Error> {
    let _lock = wait_lock(&folder(store).join(format!("{name}.state-lock")))?;
    let mut state = load_state(store, name);
    let before = serde_json::to_string_pretty(&state).expect("states always serialize");
    let answer = change(&mut state);
    let text = serde_json::to_string_pretty(&state).expect("states always serialize");
    // The tick looks each minute; only a change is written.
    if text != before || !state_file(store, name).is_file() {
        write_whole(&state_file(store, name), &format!("{text}\n"))?;
    }
    Ok(answer)
}

/// Whether a start of schedule `name` is going.
pub fn is_going(store: &Path, name: &str) -> bool {
    store::held(&going_lock(store, name))
}

// ---------------------------------------------------------------------
// The tick: what the timer runs each minute.

/// What the tick does with a timed schedule.
#[derive(Debug, PartialEq)]
enum Due {
    /// Not due, or dealt with already.
    No,
    /// Due now: start it.
    Start(Utc),
    /// Due long ago, while the lab PC was off: say it was missed.
    Missed(Utc),
    /// Due, but its last start is still going: say it was skipped.
    StillGoing(Utc),
}

/// Whether a timed schedule is due at `now`, given the last due time the
/// tick dealt with.
fn due(schedule: &Schedule, zone: &Zone, handled: Option<Utc>, now: Utc, going: bool) -> Due {
    let Some(last) = schedule.last_due(zone, now) else { return Due::No };
    if handled.is_some_and(|handled| handled >= last) {
        return Due::No;
    }
    if now.seconds_since(last) > CATCH_UP {
        Due::Missed(last)
    } else if going {
        Due::StillGoing(last)
    } else {
        Due::Start(last)
    }
}

/// `schedule tick`: starts each schedule that is due. Runs each minute
/// from the timer; quick, and safe to run by hand.
fn tick(arguments: &Arguments) -> Result<Value, Error> {
    let config = arguments.config_dir()?;
    let store = arguments.store_dir()?;
    // One tick at a time; one already going does this one's work.
    let Some(_tick) = store::take_lock(&folder(&store).join("tick.lock"))? else {
        return Ok(json!({ "started": [], "note": "another tick is going" }));
    };
    let schedules = load(&config)?;
    let now = Utc::now();
    let timed = schedules.schedules.iter().any(|schedule| schedule.at.is_some());
    let zone = if timed { Some(schedules.zone()?) } else { None };
    let mut started = Vec::new();
    let mut problems = Vec::new();
    for schedule in schedules.schedules.iter().filter(|schedule| !schedule.paused) {
        let name = &schedule.name;
        let going = is_going(&store, name);
        let start = if let Some(zone) = &zone
            && schedule.at.is_some()
        {
            edit_state(&store, name, |state| {
                let handled = state.handled.as_deref().and_then(Utc::parse_rfc3339);
                // A schedule the tick has never seen starts from now, not
                // from a time before it existed.
                let handled = handled.or_else(|| {
                    state.handled = Some(now.rfc3339());
                    Some(now)
                });
                match due(schedule, zone, handled, now, going) {
                    Due::No => false,
                    Due::Start(at) => {
                        state.handled = Some(at.rfc3339());
                        true
                    }
                    Due::Missed(at) => {
                        state.handled = Some(at.rfc3339());
                        state.last = Some(Last {
                            started: at.rfc3339(),
                            ended: at.rfc3339(),
                            result: "missed".into(),
                            note: Some(format!(
                                "the lab PC was off or asleep at {}",
                                local_text(zone, at)
                            )),
                            ..Last::default()
                        });
                        false
                    }
                    Due::StillGoing(at) => {
                        state.handled = Some(at.rfc3339());
                        problems.push(format!(
                            "{name}: its last start was still going at {}, so that time was skipped",
                            local_text(zone, at)
                        ));
                        false
                    }
                }
            })?
        } else if schedule.each_build {
            let state = load_state(&store, name);
            let looked = state.looked.as_deref().and_then(Utc::parse_rfc3339);
            if looked.is_some_and(|looked| now.seconds_since(looked) < LOOK_EVERY) || going {
                false
            } else {
                match newest_commit(schedules.source()) {
                    Ok(commit) => edit_state(&store, name, |state| {
                        state.looked = Some(now.rfc3339());
                        let new = state.commit.as_ref().is_some_and(|seen| *seen != commit);
                        // The first look only notes where main is: the
                        // schedule runs from the next build on.
                        state.commit = Some(commit);
                        new
                    })?,
                    Err(why) => {
                        problems.push(format!("{name}: {why}"));
                        false
                    }
                }
            }
        } else {
            false
        };
        if start {
            match spawn_start(arguments, name) {
                Ok(()) => started.push(name.clone()),
                Err(Error::Usage(why) | Error::Harness(why) | Error::Busy(why)) => {
                    problems.push(format!("{name}: {why}"));
                }
            }
        }
    }
    Ok(json!({ "started": started, "problems": problems }))
}

/// The newest commit on `source`'s main, without a checkout.
fn newest_commit(source: &str) -> Result<String, String> {
    let output = Command::new("git")
        .args(["ls-remote", "--", source, &format!("refs/heads/{BRANCH}")])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("git will not start ({err}); install git"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    match text.split_whitespace().next() {
        Some(commit) if output.status.success() && commit.len() >= 40 => Ok(commit.to_string()),
        _ => Err(format!(
            "could not look at {BRANCH} on {source}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// Starts schedule `name` in a process of its own, which carries on
/// after this one ends. Under the timer it goes through `systemd-run`, so
/// it is not part of the timer's own short-lived service.
fn spawn_start(arguments: &Arguments, name: &str) -> Result<(), Error> {
    let program = std::env::current_exe()
        .map_err(|err| Error::Harness(format!("finding this program: {err}")))?;
    let mut args = folder_args(arguments)?;
    args.extend(["schedule".into(), "go".into(), name.into()]);
    // Under the timer, a child would be part of the timer's service: give
    // it a unit of its own. systemd-run returns once that unit started.
    if std::env::var_os("INVOCATION_ID").is_some() && on_path("systemd-run") {
        let mut command = Command::new("systemd-run");
        command.args(["--user", "--collect", "--quiet"]);
        command.arg(format!("--description=Dukovo Lab schedule {name}"));
        if let Some(path) = std::env::var_os("PATH") {
            let mut set = std::ffi::OsString::from("--setenv=PATH=");
            set.push(path);
            command.arg(set);
        }
        command.arg("--").arg(&program).args(&args);
        let started = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if started {
            return Ok(());
        }
        // No user manager to ask: start it as a terminal would.
    }
    use std::os::unix::process::CommandExt as _;
    Command::new(&program)
        .args(&args)
        // A process group of its own: a terminal's Ctrl+C and hang-up do
        // not reach it.
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| Error::Harness(format!("starting schedule {name}: {err}")))?;
    Ok(())
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// The folder options, absolute, that every process a schedule starts
/// needs to find the same lab.
fn folder_args(arguments: &Arguments) -> Result<Vec<String>, Error> {
    let absolute = |path: PathBuf| -> Result<String, Error> {
        std::path::absolute(&path)
            .map(|path| path.display().to_string())
            .map_err(|err| Error::Usage(format!("{}: {err}", path.display())))
    };
    let mut args = vec![
        "--config".to_string(),
        absolute(arguments.config_dir()?)?,
        "--store".to_string(),
        absolute(arguments.store_dir()?)?,
    ];
    if let Some(results) = &arguments.results {
        args.extend(["--results".to_string(), absolute(results.clone())?]);
    }
    Ok(args)
}

// ---------------------------------------------------------------------
// A start: what `schedule go` does, in a process of its own.

/// Says what a start is doing, and the run once there is one, in its
/// state.
type Doing<'a> = &'a dyn Fn(&str, Option<&str>) -> Result<(), Error>;

/// Runs schedule `name` once, start to end. Its outcome goes to the
/// schedule's state, and what it printed to its log.
fn go(arguments: &Arguments, name: &str) -> Result<Value, Error> {
    let config = arguments.config_dir()?;
    let store = arguments.store_dir()?;
    let schedules = load(&config)?;
    let schedule = schedules.find(name)?.clone();
    let Some(_going) = store::take_lock(&going_lock(&store, name))? else {
        return Err(Error::Busy(format!("schedule {name} is already going")));
    };
    let started = Utc::now();
    let mut log = File::create(log_file(&store, name))
        .map_err(|err| Error::Harness(format!("making the log of schedule {name}: {err}")))?;
    let _ = writeln!(log, "{} schedule {name} starts", started.rfc3339());
    let doing = |what: &str, run: Option<&str>| {
        edit_state(&store, name, |state| {
            state.going = Some(Going {
                started: started.rfc3339(),
                doing: what.into(),
                run: run.map(String::from),
            });
        })
    };
    let outcome = run_once(arguments, &schedules, &schedule, &mut log, &doing);
    let mut last = match outcome {
        Ok(last) => last,
        Err(Error::Usage(why) | Error::Harness(why) | Error::Busy(why)) => {
            Last { result: "error".into(), note: Some(why), ..Last::default() }
        }
    };
    last.started = started.rfc3339();
    last.ended = Utc::now().rfc3339();
    let _ = writeln!(
        log,
        "{} schedule {name} ends: {}{}",
        last.ended,
        last.result,
        last.note.as_deref().map(|note| format!(" ({note})")).unwrap_or_default()
    );
    let answer = serde_json::to_value(&last).expect("outcomes always serialize");
    edit_state(&store, name, |state| {
        state.going = None;
        state.last = Some(last);
    })?;
    Ok(answer)
}

/// The steps of a start: wait for the machines, update the checkout,
/// build and run, then send or hold.
fn run_once(
    arguments: &Arguments,
    schedules: &Schedules,
    schedule: &Schedule,
    log: &mut File,
    doing: Doing<'_>,
) -> Result<Last, Error> {
    let store = arguments.store_dir()?;
    let checkout = checkout(&store);
    let waited_from = Utc::now();
    // `run` answers busy (4) if a machine was taken between the wait and
    // the run; then it waits again, a few times.
    for _ in 0..5 {
        wait_for_machines(&store, schedule, waited_from, doing)?;
        // One scheduled run at a time takes the checkout.
        doing("waiting for another schedule to finish with the Dukovo checkout", None)?;
        let checkout_lock = wait_lock(&store.join("dukovo.lock"))?;
        doing("bringing the lab's Dukovo checkout up to main", None)?;
        let commit = update_checkout(&checkout, schedules.source(), log)?;
        let short = commit[..commit.len().min(12)].to_string();
        let _ = writeln!(log, "{} testing {BRANCH} at {short}", Utc::now().rfc3339());
        doing(&format!("building the lab image of {short}, then running"), None)?;
        let id = crate::bundle::new_run_id(Utc::now());
        let status = build_and_run(arguments, schedule, &checkout, &id, log, doing)?;
        let bundle = store::bundle_dir(&store, &id);
        if status == Some(4) && !bundle.join("run.json").is_file() {
            let _ = writeln!(log, "{} a machine was taken; waiting again", Utc::now().rfc3339());
            continue;
        }
        let Some(record) = store::read_run(&bundle) else {
            return Ok(Last {
                commit: Some(commit),
                result: "error".into(),
                note: Some(format!(
                    "the image did not build or the run did not start (exit {}); see {}",
                    status.map_or("by a signal".into(), |code| code.to_string()),
                    log_file(&store, &schedule.name).display()
                )),
                ..Last::default()
            });
        };
        drop(checkout_lock);
        return Ok(finish(arguments, schedule, &store, &id, &record, commit));
    }
    Err(Error::Busy("its machines stayed busy; it gave up for this time".into()))
}

/// Waits until none of the schedule's machines is busy with a run.
fn wait_for_machines(
    store: &Path,
    schedule: &Schedule,
    since: Utc,
    doing: Doing<'_>,
) -> Result<(), Error> {
    loop {
        let busy = schedule
            .machines
            .iter()
            .find_map(|machine| store::machine_user(store, machine).map(|run| (machine, run)));
        let Some((machine, run)) = busy else { return Ok(()) };
        if Utc::now().seconds_since(since) > WAIT_AT_MOST {
            return Err(Error::Busy(format!(
                "the {machine} machine stayed busy with run {run} for 12 hours; it gave up for this time"
            )));
        }
        doing(&format!("waiting for the {machine} machine, busy with run {run}"), None)?;
        std::thread::sleep(WAIT_PAUSE);
    }
}

/// Clones the lab's checkout if there is none, then sets it to the
/// newest main; answers the commit.
fn update_checkout(checkout: &Path, source: &str, log: &mut File) -> Result<String, Error> {
    let git = |args: &[&str]| send::git(checkout, args).map_err(Error::Harness);
    if !checkout.join(".git").exists() {
        if checkout.exists() && fs::read_dir(checkout).is_ok_and(|mut dir| dir.next().is_some()) {
            return Err(Error::Harness(format!(
                "{} is there but is not a git checkout; move it away, and the next start clones Dukovo there",
                checkout.display()
            )));
        }
        let _ = writeln!(log, "{} cloning {source}", Utc::now().rfc3339());
        let parent = checkout.parent().unwrap_or(checkout);
        fs::create_dir_all(parent)
            .map_err(|err| Error::Harness(format!("making {}: {err}", parent.display())))?;
        send::git(parent, &["clone", "-q", "--", source, &checkout.display().to_string()])
            .map_err(Error::Harness)?;
    }
    git(&["remote", "set-url", "origin", source])?;
    git(&["fetch", "-q", "origin", BRANCH])?;
    // The lab's own checkout: whatever is in it gives way to main. Ignored
    // files (the build's own output) stay, so builds stay quick.
    git(&["reset", "-q", "--hard", "FETCH_HEAD"])?;
    git(&["clean", "-q", "-f", "-d"])?;
    Ok(git(&["rev-parse", "HEAD"])?.trim().to_string())
}

/// Runs `ob lab run` in the checkout: it builds the lab image of that
/// commit, with this lab's key, and runs the schedule's tests from that
/// same commit on it. Answers `ob`'s exit status.
fn build_and_run(
    arguments: &Arguments,
    schedule: &Schedule,
    checkout: &Path,
    id: &str,
    log: &File,
    doing: Doing<'_>,
) -> Result<Option<i32>, Error> {
    let store = arguments.store_dir()?;
    let program = std::env::current_exe()
        .map_err(|err| Error::Harness(format!("finding this program: {err}")))?;
    let output = |log: &File| {
        log.try_clone().map(Stdio::from).map_err(|err| Error::Harness(err.to_string()))
    };
    let mut command = Command::new(checkout.join("ob"));
    command
        .current_dir(checkout)
        .arg("lab")
        .args(folder_args(arguments)?)
        .arg("run")
        .args(schedule.picks())
        .args(["--machine", &schedule.machines.join(",")])
        .args(["--schedule", &schedule.name, "--run-id", id])
        // This same harness runs the tests, whatever is on PATH.
        .env("OB_LAB", &program)
        .stdin(Stdio::null())
        .stdout(output(log)?)
        .stderr(output(log)?);
    let mut child = command
        .spawn()
        .map_err(|err| Error::Harness(format!("starting ob in {}: {err}", checkout.display())))?;
    let bundle = store::bundle_dir(&store, id);
    let mut said_running = false;
    loop {
        if let Some(status) = child.try_wait().map_err(|err| Error::Harness(err.to_string()))? {
            return Ok(status.code());
        }
        if !said_running && bundle.join("run.json").is_file() {
            doing(&format!("running {id}"), Some(id))?;
            said_running = true;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// After a run: its result, its findings, and sends them if the schedule
/// says so.
fn finish(
    arguments: &Arguments,
    schedule: &Schedule,
    store: &Path,
    id: &str,
    record: &crate::bundle::RunRecord,
    commit: String,
) -> Last {
    let result = match (&record.summary, record.stopped) {
        (_, true) => "stopped",
        (Some(summary), _) if summary.errors > 0 => "broke",
        (Some(summary), _) if summary.failed > 0 => "failed",
        (Some(_), _) => "passed",
        // No summary: the run died part way.
        (None, _) => "broke",
    };
    let to_send: Vec<String> = findings::of_run(store, id)
        .into_iter()
        .filter(|finding| finding.state != findings::State::NotABug)
        .map(|finding| finding.id)
        .collect();
    let mut last = Last {
        run: Some(id.to_string()),
        commit: Some(commit),
        result: result.into(),
        findings: to_send.clone(),
        ..Last::default()
    };
    if to_send.is_empty() {
        return last;
    }
    if !schedule.send {
        last.note = Some("findings held: send them from the page".into());
        return last;
    }
    let clone = send::clone_dir(store, arguments.results.as_deref());
    match send::send(store, &clone, &to_send, &[id.to_string()]) {
        Ok(_) => last.sent = Some(true),
        Err(Error::Usage(why) | Error::Harness(why) | Error::Busy(why)) => {
            last.sent = Some(false);
            last.note = Some(format!("could not send, so the findings are held: {why}"));
        }
    }
    last
}

// ---------------------------------------------------------------------
// The timer's units.

/// The folder systemd reads a user's own units from.
fn unit_dir() -> Result<PathBuf, Error> {
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(config).join("systemd/user"));
    }
    crate::home(".config/systemd/user")
}

/// Whether the timer's units are in place.
pub fn timer_installed() -> bool {
    unit_dir().is_ok_and(|dir| dir.join(TIMER).is_file())
}

/// One word of a unit's command line, quoted as systemd reads it.
fn unit_word(word: &str) -> String {
    let escaped =
        word.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%").replace('$', "$$");
    format!("\"{escaped}\"")
}

/// The service and timer, as text.
fn units(program: &Path, folders: &[String], path: Option<&str>) -> (String, String) {
    let mut command = vec![unit_word(&program.display().to_string())];
    command.extend(folders.iter().map(|word| unit_word(word)));
    command.extend(["schedule".into(), "tick".into()]);
    let environment = path
        .map(|path| {
            let path = path.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%");
            format!("Environment=\"PATH={path}\"\n")
        })
        .unwrap_or_default();
    let service = format!(
        "# Made by `dukovo-lab schedule install`; it makes it again each time.\n\
         [Unit]\n\
         Description=Dukovo Lab: start the schedules that are due\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         {environment}\
         ExecStart={}\n",
        command.join(" ")
    );
    let timer = "# Made by `dukovo-lab schedule install`; it makes it again each time.\n\
                 [Unit]\n\
                 Description=Dukovo Lab: look for schedules that are due, each minute\n\
                 \n\
                 [Timer]\n\
                 OnCalendar=minutely\n\
                 AccuracySec=5s\n\
                 \n\
                 [Install]\n\
                 WantedBy=timers.target\n"
        .to_string();
    (service, timer)
}

/// `schedule install`: puts the timer in place and turns it on. Safe to
/// run again: `ob lab` does, each time, so the units always name the
/// harness installed now.
fn install(arguments: &Arguments) -> Result<Value, Error> {
    let program = std::env::current_exe()
        .map_err(|err| Error::Harness(format!("finding this program: {err}")))?;
    let folders = folder_args(arguments)?;
    let path = std::env::var("PATH").ok();
    let (service, timer) = units(&program, &folders, path.as_deref());
    let dir = unit_dir()?;
    fs::create_dir_all(&dir)
        .map_err(|err| Error::Harness(format!("making {}: {err}", dir.display())))?;
    let mut changed = false;
    for (file, text) in [(SERVICE, &service), (TIMER, &timer)] {
        let path = dir.join(file);
        if fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
            write_whole(&path, text)?;
            changed = true;
        }
    }
    let systemctl = |args: &[&str]| -> Result<(), Error> {
        let output = Command::new("systemctl")
            .arg("--user")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|err| Error::Harness(format!("systemctl will not start ({err})")))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(Error::Harness(format!(
                "`systemctl --user {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    };
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", TIMER])?;
    // Without lingering, systemd stops a person's timers when they sign
    // out; with it, schedules run whether anyone is signed in or not.
    let linger = linger();
    Ok(json!({
        "units": dir,
        "changed": changed,
        "linger": linger,
    }))
}

/// Turns on lingering for this account if it is off; answers whether it
/// is on.
fn linger() -> bool {
    let user = std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_default();
    let ask = Command::new("loginctl")
        .args(["show-user", &user, "--property=Linger", "--value"])
        .stdin(Stdio::null())
        .output();
    if ask.is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "yes") {
        return true;
    }
    Command::new("loginctl")
        .arg("enable-linger")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

// ---------------------------------------------------------------------
// The command.

/// `schedule [list | save | remove | pause | resume | start | install |
/// tick]`.
pub fn command(arguments: &Arguments) -> Result<u8, Error> {
    let (action, rest) = match arguments.names.split_first() {
        None => ("list", &[][..]),
        Some((action, rest)) => (action.as_str(), rest),
    };
    let one_name = || -> Result<&str, Error> {
        match rest {
            [name] => Ok(name.as_str()),
            _ => Err(Error::Usage(format!("schedule {action} takes one schedule's name"))),
        }
    };
    let config = arguments.config_dir()?;
    let store = arguments.store_dir()?;
    let answer = match action {
        "list" if rest.is_empty() => {
            let listed = list(&config, &store)?;
            if !arguments.json {
                print_list(&listed);
                return Ok(0);
            }
            listed
        }
        "save" => save(arguments, rest)?,
        "remove" => {
            let name = one_name()?;
            edit_schedules(&config, |schedules| {
                schedules.find(name)?;
                schedules.schedules.retain(|schedule| schedule.name != name);
                Ok(())
            })?;
            let _ = fs::remove_file(state_file(&store, name));
            json!({ "removed": name })
        }
        "pause" | "resume" => {
            let name = one_name()?;
            let paused = action == "pause";
            edit_schedules(&config, |schedules| {
                schedules.find(name)?;
                for schedule in &mut schedules.schedules {
                    if schedule.name == name {
                        schedule.paused = paused;
                    }
                }
                Ok(())
            })?;
            if !paused {
                // Resumed from now: times passed while it was paused are
                // not made up, and each-build looks at main afresh.
                edit_state(&store, name, |state| {
                    state.handled = Some(Utc::now().rfc3339());
                    state.commit = None;
                    state.looked = None;
                })?;
            }
            json!({ "schedule": name, "paused": paused })
        }
        "start" => {
            let name = one_name()?;
            load(&config)?.find(name)?;
            if is_going(&store, name) {
                return Err(Error::Busy(format!(
                    "schedule {name} is already going; `dukovo-lab schedule` shows how far"
                )));
            }
            spawn_start(arguments, name)?;
            json!({ "started": name, "log": log_file(&store, name) })
        }
        "go" => go(arguments, one_name()?)?,
        "install" if rest.is_empty() => install(arguments)?,
        "tick" if rest.is_empty() => tick(arguments)?,
        _ => {
            return Err(Error::Usage(format!(
                "no schedule action \"{}\"; see `dukovo-lab help`",
                arguments.names.join(" ")
            )));
        }
    };
    if arguments.json {
        println!("{answer}");
    } else {
        print_answer(action, &answer);
    }
    // A start that could not run says so in its exit status too.
    Ok(match answer["result"].as_str() {
        Some("failed") => 1,
        Some("broke" | "error") => 3,
        _ => 0,
    })
}

/// `schedule save <name> ...`: a new schedule, or a change to one.
fn save(arguments: &Arguments, rest: &[String]) -> Result<Value, Error> {
    let Some((name, tests)) = rest.split_first() else {
        return Err(Error::Usage("schedule save takes a name, then what to run".into()));
    };
    if arguments.send && arguments.hold {
        return Err(Error::Usage("give --send or --hold, not both".into()));
    }
    let days: Vec<String> = arguments
        .days
        .as_deref()
        .map(|days| days.split(',').map(|day| day.trim().to_lowercase()).collect())
        .unwrap_or_default();
    let schedule = Schedule {
        name: name.clone(),
        title: arguments.title.clone(),
        tests: tests.to_vec(),
        preset: arguments.preset.clone(),
        all: arguments.all,
        machines: arguments
            .machine
            .as_deref()
            .unwrap_or("qemu")
            .split(',')
            .map(|machine| machine.trim().to_string())
            .collect(),
        at: arguments.at.clone(),
        days,
        each_build: arguments.each_build,
        // Findings go to lab-results unless the schedule holds them.
        send: !arguments.hold,
        paused: false,
    };
    schedule.check().map_err(Error::Usage)?;
    // The tests and machines it names must be there, when they can be
    // looked at here.
    if arguments.tests_dir().is_dir() {
        let tests = arguments.load_tests()?;
        if let Some(preset) = &schedule.preset {
            let presets = arguments.load_presets(&tests)?;
            if !presets.iter().any(|known| known.name == *preset) {
                return Err(Error::Usage(format!("no preset named \"{preset}\"")));
            }
        } else if !schedule.tests.is_empty() {
            crate::suite::select(&tests, &schedule.tests)?;
        }
    }
    let machines = arguments.load_machines()?;
    crate::machines::select(&machines, &schedule.machines.join(","))?;
    let config = arguments.config_dir()?;
    let store = arguments.store_dir()?;
    let new = edit_schedules(&config, |schedules| {
        if schedule.at.is_some() {
            schedules.zone()?;
        }
        let old = schedules.schedules.iter_mut().find(|old| old.name == schedule.name);
        Ok(match old {
            Some(old) => {
                let paused = old.paused;
                *old = schedule.clone();
                old.paused = paused;
                false
            }
            None => {
                schedules.schedules.push(schedule.clone());
                true
            }
        })
    })?;
    // From now on: a time already passed today waits for tomorrow.
    edit_state(&store, &schedule.name, |state| {
        state.handled = Some(Utc::now().rfc3339());
        if !schedule.each_build {
            state.commit = None;
            state.looked = None;
        }
    })?;
    Ok(json!({ "saved": schedule.name, "new": new, "schedule": schedule }))
}

/// Every schedule, when it runs next and how it went last.
fn list(config: &Path, store: &Path) -> Result<Value, Error> {
    let schedules = load(config)?;
    let zone_name = schedules.zone.clone().unwrap_or_else(|| zone::DEFAULT.into());
    // A zone that will not read stops only what needs it.
    let zone = Zone::load(&zone_name);
    let now = Utc::now();
    let items: Vec<Value> = schedules
        .schedules
        .iter()
        .map(|schedule| {
            let state = load_state(store, &schedule.name);
            let going = is_going(store, &schedule.name);
            let next = match (&zone, schedule.paused) {
                (Ok(zone), false) => schedule.next_due(zone, now),
                _ => None,
            };
            let mut item = serde_json::to_value(schedule).expect("schedules always serialize");
            item["title"] = json!(schedule.title);
            item["when"] = json!(if schedule.paused { "Paused".into() } else { schedule.when() });
            item["next"] = json!(next.map(Utc::rfc3339));
            // In the lab's zone, whatever zone a browser or agent is in.
            item["next_text"] = match (&zone, next) {
                (Ok(zone), Some(next)) => json!(ahead_text(zone, now, next)),
                _ => Value::Null,
            };
            item["going"] = if going { json!(state.going) } else { Value::Null };
            item["last"] = json!(state.last);
            item
        })
        .collect();
    let (short, zone_problem) = match &zone {
        Ok(zone) => (zone.short_name(), None),
        Err(why) => (zone_name.clone(), Some(why.clone())),
    };
    Ok(json!({
        "zone": zone_name,
        "zone_short": short,
        "zone_problem": zone_problem,
        "timer": timer_installed(),
        "source": schedules.source(),
        "schedules": items,
    }))
}

fn print_list(listed: &Value) {
    let schedules = listed["schedules"].as_array().map(Vec::as_slice).unwrap_or_default();
    if schedules.is_empty() {
        println!("No schedules. Make one with `dukovo-lab schedule save`; see `dukovo-lab help`.");
    }
    let zone = Zone::load(listed["zone"].as_str().unwrap_or(zone::DEFAULT)).ok();
    let moment = |field: &Value| {
        let at = field.as_str().and_then(Utc::parse_rfc3339)?;
        Some(zone.as_ref().map_or_else(|| at.rfc3339(), |zone| local_text(zone, at)))
    };
    for schedule in schedules {
        let name = schedule["name"].as_str().unwrap_or_default();
        let title =
            schedule["title"].as_str().map(|title| format!(" ({title})")).unwrap_or_default();
        let machines: Vec<&str> = schedule["machines"]
            .as_array()
            .map(|machines| machines.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let findings = if schedule["send"].as_bool() == Some(true) {
            "sends its findings"
        } else {
            "holds its findings"
        };
        println!(
            "{name}{title}: {}, on {}; {findings}",
            schedule["when"].as_str().unwrap_or_default(),
            machines.join(", ")
        );
        if let Some(going) = schedule["going"].as_object() {
            println!("  now: {}", going.get("doing").and_then(Value::as_str).unwrap_or("going"));
        } else if let Some(next) = moment(&schedule["next"]) {
            println!("  next: {next}");
        }
        if let Some(last) = schedule["last"].as_object() {
            let run = last.get("run").and_then(Value::as_str);
            let when = moment(last.get("started").unwrap_or(&Value::Null)).unwrap_or_default();
            let mut line = format!(
                "  last: {when}, {}",
                last.get("result").and_then(Value::as_str).unwrap_or_default()
            );
            if let Some(run) = run {
                line.push_str(&format!(", run {run}"));
            }
            let count = last.get("findings").and_then(Value::as_array).map_or(0, Vec::len);
            if count > 0 {
                let sent = match last.get("sent").and_then(Value::as_bool) {
                    Some(true) => ", sent",
                    _ => ", not sent",
                };
                line.push_str(&format!(
                    ", {count} finding{}{sent}",
                    if count == 1 { "" } else { "s" }
                ));
            }
            if let Some(note) = last.get("note").and_then(Value::as_str) {
                line.push_str(&format!("; {note}"));
            }
            println!("{line}");
        }
    }
    let short = listed["zone_short"].as_str().unwrap_or_default();
    if let Some(problem) = listed["zone_problem"].as_str() {
        println!("Times cannot be worked out: {problem}");
    } else if !schedules.is_empty() {
        println!("Times are {short} ({}).", listed["zone"].as_str().unwrap_or_default());
    }
    if listed["timer"].as_bool() != Some(true) {
        println!(
            "The timer that starts schedules is not installed: `dukovo-lab schedule install`, or `ob lab`, installs it."
        );
    }
}

fn print_answer(action: &str, answer: &Value) {
    match action {
        "save" => println!(
            "{} schedule {}.",
            if answer["new"].as_bool() == Some(true) { "Made" } else { "Changed" },
            answer["saved"].as_str().unwrap_or_default()
        ),
        "remove" => {
            println!("Removed schedule {}.", answer["removed"].as_str().unwrap_or_default());
        }
        "pause" | "resume" => println!(
            "Schedule {} is {}.",
            answer["schedule"].as_str().unwrap_or_default(),
            if answer["paused"].as_bool() == Some(true) { "paused" } else { "on again" }
        ),
        "start" => {
            let name = answer["started"].as_str().unwrap_or_default();
            println!("Started schedule {name} in the background.");
            println!("Follow it: dukovo-lab schedule");
            println!("Its log:   {}", answer["log"].as_str().unwrap_or_default());
        }
        "install" => {
            println!(
                "The schedules' timer is on (units in {}).",
                answer["units"].as_str().unwrap_or_default()
            );
            if answer["linger"].as_bool() != Some(true) {
                println!(
                    "Schedules run only while you are signed in: `loginctl enable-linger` lets them run when you are not."
                );
            }
        }
        "tick" => {
            for name in answer["started"].as_array().into_iter().flatten() {
                println!("Started schedule {}.", name.as_str().unwrap_or_default());
            }
            for problem in answer["problems"].as_array().into_iter().flatten() {
                eprintln!("dukovo-lab: {}", problem.as_str().unwrap_or_default());
            }
        }
        _ => println!("{answer}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denver() -> Zone {
        Zone::parse("America/Denver", "MST7MDT,M3.2.0,M11.1.0").unwrap()
    }

    fn at(text: &str) -> Utc {
        Utc::parse_rfc3339(text).unwrap()
    }

    fn nightly() -> Schedule {
        Schedule {
            name: "nightly".into(),
            title: Some("Full run".into()),
            tests: Vec::new(),
            preset: None,
            all: true,
            machines: vec!["qemu".into()],
            at: Some("01:00".into()),
            days: Vec::new(),
            each_build: false,
            send: true,
            paused: false,
        }
    }

    #[test]
    fn checks_what_a_schedule_says() {
        assert_eq!(nightly().check(), Ok(()));
        let broken = [
            Schedule { name: "Night".into(), ..nightly() },
            Schedule { preset: Some("quick".into()), ..nightly() },
            Schedule { all: false, ..nightly() },
            Schedule { machines: Vec::new(), ..nightly() },
            Schedule { machines: vec!["--all".into()], ..nightly() },
            Schedule { at: Some("25:00".into()), ..nightly() },
            Schedule { at: Some("1am".into()), ..nightly() },
            Schedule { each_build: true, ..nightly() },
            Schedule { at: None, ..nightly() },
            Schedule { days: vec!["someday".into()], ..nightly() },
            Schedule { at: None, each_build: true, days: vec!["sun".into()], ..nightly() },
            Schedule { title: Some("two\nlines".into()), ..nightly() },
            Schedule { all: false, tests: vec!["../x".into()], ..nightly() },
        ];
        for schedule in broken {
            assert!(schedule.check().is_err(), "{schedule:?}");
        }
    }

    #[test]
    fn says_when_in_words() {
        assert_eq!(nightly().when(), "Every day, 1:00 AM");
        let on = |days: &[&str], at: &str| Schedule {
            days: days.iter().map(ToString::to_string).collect(),
            at: Some(at.into()),
            ..nightly()
        };
        assert_eq!(on(&["sun"], "02:00").when(), "Sundays, 2:00 AM");
        assert_eq!(on(&["mon", "tue", "wed", "thu", "fri"], "22:30").when(), "Weekdays, 10:30 PM");
        assert_eq!(on(&["fri", "mon", "wed"], "12:00").when(), "Mon, Wed, Fri, 12:00 PM");
        assert_eq!(on(&[], "00:05").when(), "Every day, 12:05 AM");
        let each = Schedule { at: None, each_build: true, ..nightly() };
        assert_eq!(each.when(), "Each new build on main");
    }

    #[test]
    fn finds_the_last_and_next_times_in_mountain_time() {
        let zone = denver();
        let schedule = nightly();
        // 9:00 AM MST on Saturday 7 November 2026.
        let now = at("2026-11-07T16:00:00Z");
        assert_eq!(schedule.last_due(&zone, now), Some(at("2026-11-07T08:00:00Z")));
        assert_eq!(schedule.next_due(&zone, now), Some(at("2026-11-08T08:00:00Z")));
        // In summer, 1:00 AM MDT.
        assert_eq!(
            schedule.next_due(&zone, at("2026-07-01T12:00:00Z")),
            Some(at("2026-07-02T07:00:00Z"))
        );
        // Sundays only.
        let sundays = Schedule { days: vec!["sun".into()], at: Some("02:00".into()), ..nightly() };
        assert_eq!(sundays.next_due(&zone, now), Some(at("2026-11-08T09:00:00Z")));
        assert_eq!(sundays.last_due(&zone, now), Some(at("2026-11-01T09:00:00Z")));
        assert_eq!(local_text(&zone, at("2026-11-08T09:00:00Z")), "Sun Nov 8, 2:00 AM");
        assert_eq!(ahead_text(&zone, now, at("2026-11-08T08:00:00Z")), "Tomorrow, 1:00 AM");
        assert_eq!(ahead_text(&zone, now, at("2026-11-07T23:00:00Z")), "Today, 4:00 PM");
    }

    #[test]
    fn starts_once_makes_up_a_short_miss_and_skips_a_long_one() {
        let zone = denver();
        let schedule = nightly();
        let due_at = at("2026-11-07T08:00:00Z");
        let handled = Some(at("2026-11-06T20:00:00Z"));
        // A minute after its time: start, once.
        let now = at("2026-11-07T08:00:30Z");
        assert_eq!(due(&schedule, &zone, handled, now, false), Due::Start(due_at));
        assert_eq!(due(&schedule, &zone, Some(due_at), now, false), Due::No);
        // The last start still going.
        assert_eq!(due(&schedule, &zone, handled, now, true), Due::StillGoing(due_at));
        // The PC woke at 7 AM: made up.
        let woke = at("2026-11-07T14:00:00Z");
        assert_eq!(due(&schedule, &zone, handled, woke, false), Due::Start(due_at));
        // The PC woke after 1 PM: missed, not made up.
        let late = at("2026-11-07T21:00:00Z");
        assert_eq!(due(&schedule, &zone, handled, late, false), Due::Missed(due_at));
        // Made just now, after today's time: nothing until tomorrow.
        assert_eq!(due(&schedule, &zone, Some(late), late, false), Due::No);
    }

    #[test]
    fn writes_units_that_systemd_reads_as_meant() {
        let (service, timer) = units(
            Path::new("/home/chase/.cargo/bin/dukovo-lab"),
            &["--store".into(), "/home/chase/my lab/100%".into()],
            Some("/usr/bin:/home/chase/.cargo/bin"),
        );
        assert!(service.contains(
            "ExecStart=\"/home/chase/.cargo/bin/dukovo-lab\" \"--store\" \"/home/chase/my lab/100%%\" schedule tick\n"
        ));
        assert!(service.contains("Environment=\"PATH=/usr/bin:/home/chase/.cargo/bin\"\n"));
        assert!(service.contains("Type=oneshot"));
        assert!(timer.contains("OnCalendar=minutely"));
        assert_eq!(unit_word("a$b\"c"), "\"a$$b\\\"c\"");
    }
}

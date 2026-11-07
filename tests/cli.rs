// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The commands end to end, as an agent or the runner page uses them:
//! a detached run followed with `status --wait`, its results, the list of
//! runs, and the runner page's server.
//!
//! These need no QEMU: a pretend `qemu-img` that always fails is put first
//! on PATH, so each test's boot breaks at once and the run ends quickly
//! with an `error` result, which is all these tests need.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use serde_json::Value;

/// A folder for one test, removed after.
struct Setting {
    dir: PathBuf,
}

impl Setting {
    fn new(label: &str) -> Setting {
        let dir =
            std::env::temp_dir().join(format!("dukovo-lab-cli-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let write = |relative: &str, text: &str| {
            let path = dir.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            path
        };
        write(
            "tests/boot/ready.toml",
            "about = \"boots\"\ntime = \"1m\"\n[[step]]\nwait = \"ready\"\n",
        );
        // A test that pulls the cable, and a machine with no cable to pull.
        write(
            "tests/drivers/cable.toml",
            "about = \"pulls\"\ntime = \"1m\"\n[[step]]\nlink = \"down\"\n",
        );
        write(
            "config/machines/no-cable.toml",
            "about = \"QEMU with no cable\"\ndriver = \"qemu\"\n[controls]\nlink = \"none\"\n",
        );
        write(
            "tests/presets.toml",
            "[[preset]]\nname = \"quick\"\ntitle = \"Quick check\"\ntests = [\"boot\"]\n",
        );
        write("image.img", "");
        // A QEMU that is there, and a qemu-img that always fails.
        let qemu =
            write("bin/qemu-system-x86_64", "#!/bin/sh\necho 'QEMU emulator version 9.2.0'\n");
        let image = write(
            "bin/qemu-img",
            "#!/bin/sh\necho 'pretend qemu-img: no disks here' >&2\nexit 1\n",
        );
        for script in [qemu, image] {
            fs::set_permissions(script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Setting { dir }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dukovo-lab"));
        let path = format!("{}:{}", self.dir.join("bin").display(), std::env::var("PATH").unwrap());
        command
            .env("PATH", path)
            .env("XDG_RUNTIME_DIR", self.dir.join("runtime"))
            // Never this machine's own systemd: a schedule's start runs
            // as a plain process here.
            .env_remove("INVOCATION_ID")
            .arg("--tests")
            .arg(self.dir.join("tests"))
            .arg("--config")
            .arg(self.dir.join("config"))
            .arg("--store")
            .arg(self.dir.join("store"))
            .arg("--json");
        command
    }

    fn run(&self, args: &[&str]) -> (Option<i32>, Value) {
        let Output { status, stdout, .. } = self.command().args(args).output().unwrap();
        let text = String::from_utf8_lossy(&stdout);
        let last = text.lines().last().unwrap_or("null");
        (status.code(), serde_json::from_str(last).unwrap_or(Value::Null))
    }

    fn image(&self) -> String {
        self.dir.join("image.img").display().to_string()
    }
}

impl Drop for Setting {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_detached_run_is_followed_to_its_end() {
    let setting = Setting::new("detach");
    let image = setting.image();
    let (code, started) = setting.run(&["run", "--preset", "quick", "--image", &image, "--detach"]);
    assert_eq!(code, Some(0), "{started}");
    assert_eq!(started["event"], "run-detached");
    let id = started["id"].as_str().unwrap().to_string();

    // The boot broke, so the run ends with an error: status 3.
    let (code, status) = setting.run(&["status", &id, "--wait"]);
    assert_eq!(code, Some(3), "{status}");
    assert_eq!(status["state"], "ended");
    assert_eq!(status["counts"]["errors"], 1);

    let (code, results) = setting.run(&["results", &id, "--failed"]);
    assert_eq!(code, Some(3));
    let result = &results["results"][0];
    assert_eq!(result["test"], "boot/ready");
    assert_eq!(result["result"], "error");
    assert!(result["reason"].as_str().unwrap().contains("pretend qemu-img"), "{result}");

    let (_, runs) = setting.run(&["runs"]);
    assert_eq!(runs["runs"][0]["id"], id.as_str());
    assert_eq!(runs["runs"][0]["preset"], "quick");

    // Nothing is going now: stop and done say so.
    assert_eq!(setting.run(&["stop"]).0, Some(2));
    assert_eq!(setting.run(&["done", &id, "--machine", "qemu"]).0, Some(2));
    // A bad request is refused before anything starts.
    let (code, error) = setting.run(&["run", "boot/none", "--image", &image, "--detach"]);
    assert_eq!(code, Some(2));
    assert!(error["error"].as_str().unwrap().contains("boot/none"));
}

#[test]
fn a_run_that_ends_at_once_is_still_detached() {
    let setting = Setting::new("at-once");
    let image = setting.image();
    // Its one test is skipped, so the run may well end before `run
    // --detach` first looks: that is a run that went, not one that failed.
    let args = ["run", "drivers/cable", "--machine", "no-cable", "--image", &image, "--detach"];
    let (code, started) = setting.run(&args);
    assert_eq!(code, Some(0), "{started}");
    let id = started["id"].as_str().unwrap().to_string();
    let (code, status) = setting.run(&["status", &id, "--wait"]);
    assert_eq!(code, Some(0), "{status}");
    assert_eq!(status["counts"]["skipped"], 1);
}

#[test]
fn a_detached_run_that_cannot_start_says_why() {
    let setting = Setting::new("cannot-start");
    let image = setting.image();
    // Only the run itself finds this: it cannot make its bundle.
    fs::create_dir_all(setting.dir.join("store")).unwrap();
    fs::write(setting.dir.join("store/runs"), "not a folder").unwrap();
    let (code, error) = setting.run(&["run", "boot/ready", "--image", &image, "--detach"]);
    assert_eq!(code, Some(3), "{error}");
    let message = error["error"].as_str().unwrap();
    assert!(message.contains("making the bundle"), "{message}");
    assert!(!setting.dir.join("store/running").read_dir().unwrap().any(|_| true), "nothing left");
}

/// One HTTP request to the page; answers the status line, headers and body.
fn request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[&str],
    body: &str,
) -> (String, String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut text = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\n",
        body.len()
    );
    for header in headers {
        text.push_str(header);
        text.push_str("\r\n");
    }
    text.push_str("\r\n");
    text.push_str(body);
    stream.write_all(text.as_bytes()).unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    let (head, body) = answer.split_once("\r\n\r\n").unwrap();
    let (status, headers) = head.split_once("\r\n").unwrap_or((head, ""));
    (status.to_string(), headers.to_string(), body.to_string())
}

/// The page's server, stopped when dropped.
struct Ui(Child);

impl Drop for Ui {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn the_runner_page_runs_the_same_commands() {
    let setting = Setting::new("ui");
    let image = setting.image();
    let mut child = setting
        .command()
        .args(["ui", "--port", "0", "--no-open", "--image", &image, "--build", "0.13.0 test"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut first).unwrap();
    let _ui = Ui(child);
    let started: Value = serde_json::from_str(&first).unwrap();
    let port = started["port"].as_u64().unwrap() as u16;
    let address = started["address"].as_str().unwrap();
    let key_path = &address[address.find('/').unwrap() + 2..];
    let key_path = &key_path[key_path.find('/').unwrap()..];

    // No key, no page; the printed address gives the cookie.
    assert!(request(port, "GET", "/", &[], "").0.contains("403"));
    let (status, headers, _) = request(port, "GET", key_path, &[], "");
    assert!(status.contains("303"), "{status}");
    let cookie = headers
        .lines()
        .find_map(|line| line.strip_prefix("Set-Cookie: "))
        .and_then(|value| value.split(';').next())
        .unwrap()
        .to_string();
    let cookie = format!("Cookie: {cookie}");

    let (status, _, page) = request(port, "GET", "/", &[&cookie], "");
    assert!(status.contains("200") && page.contains("Dukovo"), "{status}");
    let (_, _, list) = request(port, "GET", "/api/list", &[&cookie], "");
    let list: Value = serde_json::from_str(&list).unwrap();
    assert_eq!(list["command"], "dukovo-lab list --json");
    assert_eq!(list["answer"]["tests"][0]["id"], "boot/ready");
    assert_eq!(list["answer"]["presets"][0]["title"], "Quick check");

    // Starting a run needs the page's header, then works as the command does.
    let body = r#"{"tests":["boot/ready"],"machines":["qemu"]}"#;
    assert!(request(port, "POST", "/api/run", &[&cookie], body).0.contains("403"));
    let (_, _, started) = request(port, "POST", "/api/run", &[&cookie, "X-Dukovo-Lab: 1"], body);
    let started: Value = serde_json::from_str(&started).unwrap();
    assert!(
        started["command"]
            .as_str()
            .unwrap()
            .starts_with("dukovo-lab run --detach boot/ready --machine qemu --image")
    );
    assert!(started["command"].as_str().unwrap().contains("--build '0.13.0 test'"));
    let id = started["answer"]["id"].as_str().unwrap().to_string();
    let (code, status) = setting.run(&["status", &id, "--wait"]);
    assert_eq!((code, &status["state"]), (Some(3), &Value::from("ended")));
    let (_, _, results) = request(port, "GET", &format!("/api/results?run={id}"), &[&cookie], "");
    let results: Value = serde_json::from_str(&results).unwrap();
    assert_eq!(results["answer"]["results"][0]["result"], "error");
    // Its files are served from the bundle.
    let (status, _, _) = request(port, "GET", &format!("/files/{id}/run.json"), &[&cookie], "");
    assert!(status.contains("200"), "{status}");
    assert!(Path::new(&setting.dir).join("store/runs").is_dir());
}

/// Runs git in `dir` for a test, which must work.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn findings_are_marked_and_sent_as_an_agent_would() {
    let setting = Setting::new("findings");
    let store = setting.dir.join("store");
    // A run that ended with one failure, and the finding it made, as a
    // run leaves them (a real failure needs QEMU; findings.rs tests that).
    let run = "20261107-080000-0001";
    let bundle = store.join("runs/2026-11-07").join(run);
    fs::create_dir_all(bundle.join("logs/qemu")).unwrap();
    let reason = "step 1: the link did not go down";
    let record = serde_json::json!({
        "schema": 1, "id": run, "harness": "dukovo-lab 0.1.0",
        "started": "2026-11-07T08:00:00Z", "ended": "2026-11-07T08:01:00Z",
        "build": "0.13.0 fd2a9e8", "image": "lab.img", "machines": [],
        "tests": ["drivers/cable"],
        "summary": { "passed": 0, "failed": 1, "skipped": 0, "errors": 0 }, "stopped": false,
    });
    let results = serde_json::json!({ "schema": 1, "run": run, "results": [{
        "test": "drivers/cable", "machine": "qemu", "result": "fail", "reason": reason,
        "milliseconds": 900, "boot": 1, "log": "logs/qemu/drivers-cable.log",
        "steps": [], "screenshots": [],
    }]});
    fs::write(bundle.join("run.json"), record.to_string()).unwrap();
    fs::write(bundle.join("results.json"), results.to_string()).unwrap();
    fs::write(bundle.join("logs/qemu/drivers-cable.log"), "step 1 FAILED\n").unwrap();
    let id = "f-0123456789ab";
    let finding = serde_json::json!({
        "schema": 1, "id": id, "test": "drivers/cable", "signature": reason, "state": "new",
        "note": null, "first_seen": "2026-11-07T08:00:00Z", "last_seen": "2026-11-07T08:00:00Z",
        "count": 1, "machines": ["qemu"], "builds": ["0.13.0 fd2a9e8"],
        "runs": [{ "run": run, "machine": "qemu", "build": "0.13.0 fd2a9e8",
                   "at": "2026-11-07T08:00:00Z", "reason": reason, "sent": false }],
        "passing": {}, "sent": null,
    });
    fs::create_dir_all(store.join("findings")).unwrap();
    fs::write(store.join("findings").join(format!("{id}.json")), finding.to_string()).unwrap();

    let (code, list) = setting.run(&["findings"]);
    assert_eq!(code, Some(0));
    assert_eq!(list["findings"][0]["id"], id);
    let (_, of_run) = setting.run(&["findings", "--run", run]);
    assert_eq!(of_run["findings"].as_array().unwrap().len(), 1);

    // Not a bug, with why; it leaves the open list; reopen takes it back.
    let (code, marked) = setting.run(&["findings", id, "--not-a-bug", "--note", "test is wrong"]);
    assert_eq!(code, Some(0), "{marked}");
    assert_eq!(marked["state"], "not-a-bug");
    assert_eq!(marked["note"], "test is wrong");
    assert!(setting.run(&["findings"]).1["findings"].as_array().unwrap().is_empty());
    assert_eq!(setting.run(&["findings", id, "--reopen"]).1["state"], "new");
    assert_eq!(setting.run(&["findings", "f-nothere"]).0, Some(2));
    assert_eq!(setting.run(&["findings", id, "--note", "why"]).0, Some(2));

    // No clone yet: Send says how to make one.
    let (code, error) = setting.run(&["send", "--all"]);
    assert_eq!(code, Some(2));
    assert!(error["error"].as_str().unwrap().contains("git clone"), "{error}");

    // lab-results, bare as on GitHub, and a clone of it elsewhere.
    let origin = setting.dir.join("origin.git");
    fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);
    let clone = setting.dir.join("clone");
    git(&setting.dir, &["clone", "-q", origin.to_str().unwrap(), clone.to_str().unwrap()]);
    git(&clone, &["config", "user.name", "Lab"]);
    git(&clone, &["config", "user.email", "lab@example.org"]);
    git(&clone, &["commit", "-q", "--allow-empty", "-m", "Start"]);
    git(&clone, &["push", "-q", "origin", "main"]);

    let results_dir = clone.to_str().unwrap();
    let (code, sent) = setting.run(&["send", "--all", "--results", results_dir]);
    assert_eq!(code, Some(0), "{sent}");
    assert_eq!(sent["findings"][0], id);
    assert_eq!(sent["runs"][0], run);
    let pushed = git(&origin, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(pushed.contains(&format!("findings/{id}.json")), "{pushed}");
    assert!(pushed.contains("logs/qemu/drivers-cable.log.gz"), "{pushed}");
    // The run says it went, and there is nothing left to send.
    assert_eq!(setting.run(&["runs"]).1["runs"][0]["sent"], true);
    let (code, again) = setting.run(&["send", "--all", "--results", results_dir]);
    assert_eq!(code, Some(0), "{again}");
    assert!(again["findings"].as_array().unwrap().is_empty());
}

/// Waits for schedule `name`'s start to end, and answers how it went.
/// How schedule `name` last went, once a start other than `before` has
/// ended. A start runs in a process of its own, which may not have begun
/// yet when the command that started it returns.
fn last_of(setting: &Setting, name: &str, before: &Value) -> Value {
    for _ in 0..300 {
        let (_, listed) = setting.run(&["schedule"]);
        let schedule = listed["schedules"]
            .as_array()
            .and_then(|all| all.iter().find(|schedule| schedule["name"] == name))
            .cloned()
            .unwrap_or(Value::Null);
        if schedule["going"].is_null() && !schedule["last"].is_null() && schedule["last"] != *before
        {
            return schedule["last"].clone();
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("schedule {name} did not end");
}

#[test]
fn a_schedule_tests_main_on_the_lab_s_own_checkout() {
    let setting = Setting::new("schedule");
    // Dukovo's main, as a local repository: its own tests, an image, and
    // a pretend `ob` that runs `ob lab` as the real one does, on them.
    let source = setting.dir.join("source");
    for (relative, from) in [
        ("lab/tests/boot/ready.toml", "tests/boot/ready.toml"),
        ("lab/tests/presets.toml", "tests/presets.toml"),
    ] {
        fs::create_dir_all(source.join(relative).parent().unwrap()).unwrap();
        fs::copy(setting.dir.join(from), source.join(relative)).unwrap();
    }
    fs::write(source.join("image.img"), "").unwrap();
    let ob = source.join("ob");
    fs::write(
        &ob,
        "#!/bin/sh\n\
         root=$(cd \"$(dirname \"$0\")\" && pwd)\n\
         [ \"$1\" = lab ] || exit 2\n\
         shift\n\
         echo 'pretend ob: building the lab image'\n\
         exec \"$OB_LAB\" --tests \"$root/lab/tests\" \"$@\" --image \"$root/image.img\" \\\n\
             --build \"test $(git -C \"$root\" rev-parse --short HEAD)\"\n",
    )
    .unwrap();
    fs::set_permissions(&ob, fs::Permissions::from_mode(0o755)).unwrap();
    git(&setting.dir, &["init", "-q", "-b", "main", source.to_str().unwrap()]);
    let commit = |message: &str| {
        git(&source, &["add", "-A"]);
        git(
            &source,
            &[
                "-c",
                "user.name=Lab",
                "-c",
                "user.email=lab@example.org",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                message,
            ],
        );
        git(&source, &["rev-parse", "HEAD"]).trim().to_string()
    };
    let first = commit("First");
    fs::write(
        setting.dir.join("config/schedules.json"),
        format!("{{\"source\": \"{}\"}}\n", source.display()),
    )
    .unwrap();

    // Made with a command, as the page makes it.
    let (code, saved) = setting.run(&[
        "schedule",
        "save",
        "quick",
        "--preset",
        "quick",
        "--each-build",
        "--title",
        "Quick check",
    ]);
    assert_eq!(code, Some(0), "{saved}");
    assert_eq!(saved["new"], true);
    let (_, listed) = setting.run(&["schedule"]);
    assert_eq!(listed["schedules"][0]["when"], "Each new build on main");
    assert_eq!(listed["schedules"][0]["send"], true);
    // Bad ones are refused, and change nothing.
    for bad in [
        vec!["schedule", "save", "Quick", "--preset", "quick", "--each-build"],
        vec!["schedule", "save", "x", "--preset", "quick"],
        vec!["schedule", "save", "x", "--preset", "nope", "--each-build"],
        vec!["schedule", "save", "x", "boot", "--each-build", "--machine", "nowhere"],
        vec!["schedule", "save", "x", "boot", "--at", "1am"],
    ] {
        assert_eq!(setting.run(&bad).0, Some(2), "{bad:?}");
    }
    assert_eq!(setting.run(&["schedule"]).1["schedules"].as_array().unwrap().len(), 1);

    // Started by hand: it clones main, builds and runs there. The boot
    // breaks (no QEMU here), so the run broke.
    let (code, started) = setting.run(&["schedule", "start", "quick"]);
    assert_eq!(code, Some(0), "{started}");
    let last = last_of(&setting, "quick", &Value::Null);
    assert_eq!(last["result"], "broke", "{last}");
    assert_eq!(last["commit"], first.as_str());
    let run = last["run"].as_str().unwrap().to_string();
    let (_, runs) = setting.run(&["runs"]);
    assert_eq!(runs["runs"][0]["id"], run.as_str());
    assert_eq!(runs["runs"][0]["schedule"], "quick");
    assert_eq!(runs["runs"][0]["build"], format!("test {}", &first[..7]));
    let log = fs::read_to_string(setting.dir.join("store/schedules/quick.log")).unwrap();
    assert!(log.contains("pretend ob: building the lab image"), "{log}");

    // The timer's tick: the first look at main only notes where it is...
    let (code, ticked) = setting.run(&["schedule", "tick"]);
    assert_eq!(code, Some(0), "{ticked}");
    assert_eq!(ticked["started"], serde_json::json!([]));
    // ...and a new commit on main starts it (the look's five minutes are
    // let pass by forgetting when it looked).
    let second = commit("Second");
    let state_file = setting.dir.join("store/schedules/quick.json");
    let mut state: Value = serde_json::from_str(&fs::read_to_string(&state_file).unwrap()).unwrap();
    assert_eq!(state["commit"], first.as_str());
    state["looked"] = Value::Null;
    fs::write(&state_file, state.to_string()).unwrap();
    let (_, ticked) = setting.run(&["schedule", "tick"]);
    assert_eq!(ticked["started"], serde_json::json!(["quick"]), "{ticked}");
    let last = last_of(&setting, "quick", &last);
    assert_eq!(last["commit"], second.as_str(), "{last}");
    assert_ne!(last["run"], run.as_str());

    // Paused, it says so and the tick leaves it; then it goes.
    assert_eq!(setting.run(&["schedule", "pause", "quick"]).0, Some(0));
    assert_eq!(setting.run(&["schedule"]).1["schedules"][0]["when"], "Paused");
    assert_eq!(setting.run(&["schedule", "remove", "quick"]).0, Some(0));
    assert!(setting.run(&["schedule"]).1["schedules"].as_array().unwrap().is_empty());
    assert_eq!(setting.run(&["schedule", "start", "quick"]).0, Some(2));
}

#[test]
fn install_puts_the_timer_in_place_and_turns_it_on() {
    let setting = Setting::new("install");
    // A pretend systemctl and loginctl, which note what they were asked.
    let asked = setting.dir.join("asked");
    for (program, exit) in [("systemctl", 0), ("loginctl", 0)] {
        let script = setting.dir.join("bin").join(program);
        fs::write(
            &script,
            format!("#!/bin/sh\necho \"{program} $*\" >> '{}'\nexit {exit}\n", asked.display()),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let units = setting.dir.join("xdg/systemd/user");
    let install = |setting: &Setting| {
        let output = setting
            .command()
            .env("XDG_CONFIG_HOME", setting.dir.join("xdg"))
            .args(["schedule", "install"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        (output.status.code(), serde_json::from_str::<Value>(text.trim()).unwrap_or(Value::Null))
    };
    let (code, answer) = install(&setting);
    assert_eq!(code, Some(0), "{answer}");
    assert_eq!(answer["changed"], true);
    let service = fs::read_to_string(units.join("dukovo-lab-schedules.service")).unwrap();
    assert!(service.contains("schedule tick"), "{service}");
    assert!(service.contains(&format!("\"{}\"", setting.dir.join("store").display())));
    assert!(units.join("dukovo-lab-schedules.timer").is_file());
    let said = fs::read_to_string(&asked).unwrap();
    assert!(said.contains("systemctl --user daemon-reload"), "{said}");
    assert!(said.contains("systemctl --user enable --now dukovo-lab-schedules.timer"), "{said}");
    // Again: nothing to change.
    assert_eq!(install(&setting).1["changed"], false);

    // A PC where systemd will not: an error, and no units left to say
    // the timer is there.
    let systemctl = setting.dir.join("bin/systemctl");
    fs::write(&systemctl, "#!/bin/sh\necho 'Failed to connect to bus' >&2\nexit 1\n").unwrap();
    let (code, answer) = install(&setting);
    assert_eq!(code, Some(3), "{answer}");
    assert!(answer["error"].as_str().unwrap().contains("Failed to connect to bus"), "{answer}");
    assert!(!units.join("dukovo-lab-schedules.timer").exists());
}

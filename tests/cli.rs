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
        write(
            "tests/presets.toml",
            "[[preset]]\nname = \"quick\"\ntitle = \"Quick check\"\ntests = [\"boot\"]\n",
        );
        write("image.img", "");
        fs::create_dir_all(dir.join("config")).unwrap();
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

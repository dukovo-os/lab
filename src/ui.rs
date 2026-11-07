// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `dukovo-lab ui`: the runner page, served to a browser on the lab PC.
//!
//! The page does nothing a command cannot: each of its buttons runs a
//! `dukovo-lab` command with `--json` (`run --detach`, `status`, `stop`,
//! `done`, `results`, `runs`, `list`, `machines`, `findings`, `send`,
//! `schedule`)
//! and shows the answer,
//! and every answer names the command it ran. So whatever a person does
//! on the page, an agent can do with the same commands, and the page can
//! never drift from them.
//!
//! The page is for the lab PC alone, and guarded as such:
//!
//! - It listens on 127.0.0.1 only, so no other machine can reach it.
//! - It answers only requests addressed to `127.0.0.1:<port>` or
//!   `localhost:<port>`, so a web page elsewhere cannot reach it by
//!   pointing a name of its own at 127.0.0.1 (DNS rebinding).
//! - Each start makes a new random key. The address `ui` prints carries
//!   it once; the page trades it for a cookie (SameSite=Strict,
//!   HttpOnly), and every request needs that cookie. So a web page in the
//!   same browser gets nothing without the printed address. (Opening the
//!   browser passes it the address as an argument, which other accounts
//!   on the PC can see while it runs: on a PC with other accounts, use
//!   `--no-open`; docs/runner-page.md says so.)
//! - Actions are POSTs that need a header a cross-site form cannot send.
//! - It serves its own page, the commands' answers, and files inside
//!   results bundles; nothing else on the PC. The lab key is never in a
//!   bundle, so the page can never hand it out.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::SystemTime;

use serde_json::{Value, json};

use crate::http::{Request, Response, read_request, write_response};
use crate::machines::is_plain_name;
use crate::{Arguments, Error, findings, send, store};

/// The port `ui` listens on unless told otherwise.
const DEFAULT_PORT: u16 = 7420;

/// The page's cookie, which carries its key.
const COOKIE: &str = "dukovo_lab";

/// The page and what it loads, built into the program.
const PAGE_HTML: &str = include_str!("ui/page.html");
const PAGE_CSS: &str = include_str!("ui/page.css");
const PAGE_JS: &str = include_str!("ui/page.js");

/// Sent with every answer: nothing loads from elsewhere, nothing is kept
/// by the browser's cache, nothing frames the page.
const SECURITY_HEADERS: &[(&str, &str)] = &[
    (
        "Content-Security-Policy",
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
    ),
    ("Cache-Control", "no-store"),
    ("X-Content-Type-Options", "nosniff"),
    ("Referrer-Policy", "no-referrer"),
    ("X-Frame-Options", "DENY"),
];

/// What the server knows.
struct Page {
    port: u16,
    key: String,
    /// This program, which the page's commands run.
    program: PathBuf,
    /// The folder options every command gets, with absolute paths.
    folders: Vec<OsString>,
    store: PathBuf,
    /// The lab-results clone that Send commits to.
    results: PathBuf,
    /// The image under test and its build, which runs from the page use.
    image: Option<PathBuf>,
    build: Option<String>,
    /// When the image was made, as the page opened: a later one is a new
    /// build, which `build` no longer names.
    image_made: Option<SystemTime>,
}

/// Serves the page until the process is stopped (Ctrl+C). Runs started
/// from the page carry on without it.
pub fn serve(arguments: &Arguments) -> Result<u8, Error> {
    // Broken test or machine files stop the page here, not on its first
    // request.
    let tests = arguments.load_tests()?;
    arguments.load_presets(&tests)?;
    arguments.load_machines()?;
    let absolute = |path: PathBuf| -> Result<PathBuf, Error> {
        std::path::absolute(&path).map_err(|err| Error::Usage(format!("{}: {err}", path.display())))
    };
    let store = absolute(arguments.store_dir()?)?;
    let mut folders = vec![
        "--tests".into(),
        absolute(arguments.tests_dir())?.into(),
        "--config".into(),
        absolute(arguments.config_dir()?)?.into(),
        "--store".into(),
        store.clone().into(),
    ];
    let results = absolute(send::clone_dir(&store, arguments.results.as_deref()))?;
    if arguments.results.is_some() {
        folders.extend(["--results".into(), results.clone().into()]);
    }
    let image = match &arguments.image {
        Some(image) if !image.is_file() => {
            return Err(Error::Usage(format!("the image {} does not exist", image.display())));
        }
        Some(image) => Some(absolute(image.clone())?),
        None => None,
    };
    let port = arguments.port.unwrap_or(DEFAULT_PORT);
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|err| {
        Error::Usage(format!(
            "cannot listen on 127.0.0.1:{port} ({err}); is the page already open? Pick another port with --port"
        ))
    })?;
    let port = listener.local_addr().map_err(|err| Error::Harness(err.to_string()))?.port();
    let page = Arc::new(Page {
        port,
        key: new_key()?,
        program: std::env::current_exe()
            .map_err(|err| Error::Harness(format!("finding this program: {err}")))?,
        folders,
        image_made: image.as_deref().and_then(modified),
        store,
        results,
        image,
        build: arguments.build.clone(),
    });
    let address = format!("http://127.0.0.1:{port}/?key={}", page.key);
    if arguments.json {
        println!("{}", json!({ "event": "ui-started", "address": address, "port": port }));
    } else {
        println!("The runner page is at {address}");
        println!("Only this PC can open it. Ctrl+C closes it; runs carry on without it.");
        if page.image.is_none() {
            println!("No --image was given, so the page cannot start runs; `ob lab ui` gives one.");
        }
    }
    if !arguments.no_open
        && (std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some())
    {
        let _ = Command::new("xdg-open")
            .arg(&address)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    for stream in listener.incoming().flatten() {
        let page = Arc::clone(&page);
        std::thread::spawn(move || page.answer(&stream));
    }
    Ok(0)
}

/// A new random key: 128 bits from the system's random source.
fn new_key() -> Result<String, Error> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|err| Error::Harness(format!("reading /dev/urandom for the page's key: {err}")))?;
    Ok(bytes.iter().fold(String::new(), |mut key, byte| {
        key.push_str(&format!("{byte:02x}"));
        key
    }))
}

/// Whether two strings are the same, taking as long whatever they hold.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
}

impl Page {
    fn answer(&self, stream: &TcpStream) {
        let mut response = match read_request(stream) {
            Ok(request) => self.route(&request),
            Err(err) => Response::text(400, &err),
        };
        for &(name, value) in SECURITY_HEADERS {
            response.headers.push((name, value.into()));
        }
        write_response(stream, &response);
    }

    /// The names this page answers to.
    fn our_host(&self, host: &str) -> bool {
        host == format!("127.0.0.1:{}", self.port) || host == format!("localhost:{}", self.port)
    }

    fn route(&self, request: &Request) -> Response {
        if !request.header("host").is_some_and(|host| self.our_host(host)) {
            return Response::text(421, "This page answers only to 127.0.0.1 and localhost.");
        }
        // The printed address: trade its key for the cookie, and drop the
        // key from the address bar.
        if request.method == "GET"
            && request.path == "/"
            && let Some(key) = request.query("key")
        {
            if !same(key, &self.key) {
                return Response::text(
                    403,
                    "That key is old: `dukovo-lab ui` makes a new one each time it starts.",
                );
            }
            return Response::text(303, "").with("Location", "/").with(
                "Set-Cookie",
                format!("{COOKIE}={}; HttpOnly; SameSite=Strict; Path=/", self.key),
            );
        }
        if !request.cookie(COOKIE).is_some_and(|key| same(key, &self.key)) {
            return Response::text(
                403,
                "Open the address `dukovo-lab ui` printed: it carries this page's key.",
            );
        }
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/") => Response::new(200, "text/html; charset=utf-8", PAGE_HTML),
            ("GET", "/page.css") => Response::new(200, "text/css; charset=utf-8", PAGE_CSS),
            ("GET", "/page.js") => Response::new(200, "text/javascript; charset=utf-8", PAGE_JS),
            ("GET", "/api/page") => Response::json(
                200,
                &json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "build": self.build,
                    "image": self.image,
                    "can_run": self.image.is_some(),
                    // Send needs a lab-results clone (docs/sending.md).
                    "can_send": self.results.join(".git").exists(),
                    "results": self.results,
                }),
            ),
            ("GET", "/api/list") => self.command(&["list"]),
            ("GET", "/api/machines") => self.command(&["machines"]),
            ("GET", "/api/runs") => self.command(&["runs", "--limit", "20"]),
            ("GET", "/api/schedules") => self.command(&["schedule"]),
            ("GET", "/api/findings") => match request.query("run") {
                None => self.command(&["findings", "--all"]),
                Some(id) if store::valid_run_id(id) => self.command(&["findings", "--run", id]),
                Some(_) => bad("not a run id"),
            },
            ("GET", "/api/status" | "/api/results") => {
                let command = request.path.trim_start_matches("/api/");
                match request.query("run") {
                    None => self.command(&[command]),
                    Some(id) if store::valid_run_id(id) => self.command(&[command, id]),
                    Some(_) => bad("not a run id"),
                }
            }
            ("POST", path) if path.starts_with("/api/") => {
                if !self.trusted_post(request) {
                    return Response::text(403, "Actions come from the runner page only.");
                }
                let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
                    return bad("the body is not JSON");
                };
                match path {
                    "/api/run" => self.start_run(&body),
                    "/api/stop" => match run_id(&body) {
                        Some(id) => self.command(&["stop", id]),
                        None => bad("which run?"),
                    },
                    "/api/done" => match (run_id(&body), body["machine"].as_str()) {
                        (Some(id), Some(machine)) if is_plain_name(machine) => {
                            self.command(&["done", id, "--machine", machine])
                        }
                        _ => bad("which run and machine?"),
                    },
                    "/api/send" => self.send(&body),
                    "/api/findings" => self.change_finding(&body),
                    "/api/schedules" => self.change_schedule(&body),
                    _ => Response::text(404, "No such action."),
                }
            }
            ("GET", path) if path.starts_with("/files/") => self.bundle_file(path),
            ("GET", _) => Response::text(404, "Not here."),
            _ => Response::text(405, "The page only reads with GET and acts with POST."),
        }
    }

    /// Whether a POST comes from this page: it carries the header the
    /// page's script adds, which a form on another site cannot, and, if
    /// the browser names where it came from, it is this page.
    fn trusted_post(&self, request: &Request) -> bool {
        let from_page = request.header("x-dukovo-lab") == Some("1");
        let origin_ok = request.header("origin").is_none_or(|origin| {
            origin.strip_prefix("http://").is_some_and(|host| self.our_host(host))
        });
        from_page && origin_ok
    }

    /// Starts a run from the page: `run --detach` with the page's image.
    fn start_run(&self, body: &Value) -> Response {
        let Some(image) = &self.image else {
            return bad("the page has no image to test: open it with `ob lab ui`");
        };
        // Runs say which build they tested; a page left open across a
        // rebuild would name the old one.
        if modified(image) != self.image_made {
            return bad(
                "the image was rebuilt after this page opened, so the page no longer knows its build: close the page (Ctrl+C) and open it again with `ob lab ui`",
            );
        }
        let strings = |field: &str| -> Option<Vec<String>> {
            body[field].as_array().map(|items| {
                items.iter().filter_map(|item| item.as_str().map(String::from)).collect()
            })
        };
        let mut args: Vec<String> = vec!["run".into(), "--detach".into()];
        if let Some(preset) = body["preset"].as_str() {
            if !is_plain_name(preset) {
                return bad("not a preset name");
            }
            args.extend(["--preset".into(), preset.into()]);
        } else {
            let tests = strings("tests").unwrap_or_default();
            // Ids are a category and a name: never an option.
            let plain =
                |test: &String| test.split('/').all(is_plain_name) && test.split('/').count() <= 2;
            if tests.is_empty() || !tests.iter().all(plain) {
                return bad("pick tests to run");
            }
            args.extend(tests);
        }
        let machines = strings("machines").unwrap_or_default();
        if machines.is_empty() || !machines.iter().all(|machine| is_plain_name(machine)) {
            return bad("pick machines to run on");
        }
        args.extend(["--machine".into(), machines.join(",")]);
        args.extend(["--image".into(), image.display().to_string()]);
        if let Some(build) = &self.build {
            args.extend(["--build".into(), build.clone()]);
        }
        if body["with_person"].as_bool() == Some(true) {
            args.push("--with-person".into());
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.command(&args)
    }

    /// Sends to lab-results: `{"findings": [ids]}`, `{"run": id}` (a run
    /// and its findings) or `{"all": true}` (every finding not sent yet).
    fn send(&self, body: &Value) -> Response {
        let mut args: Vec<&str> = vec!["send"];
        if body["all"].as_bool() == Some(true) {
            args.push("--all");
        } else if let Some(id) = run_id(body) {
            args.extend(["--run", id]);
        } else {
            let ids: Vec<&str> = body["findings"]
                .as_array()
                .map(|ids| ids.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if ids.is_empty() || !ids.iter().all(|id| findings::valid_id(id)) {
                return bad("which findings?");
            }
            args.extend(ids);
        }
        self.command(&args)
    }

    /// Says a finding is not a bug, with an optional note, or reopens it:
    /// `{"finding": id, "not_a_bug": true, "note": text}` or
    /// `{"finding": id, "reopen": true}`.
    fn change_finding(&self, body: &Value) -> Response {
        let Some(id) = body["finding"].as_str().filter(|id| findings::valid_id(id)) else {
            return bad("which finding?");
        };
        if body["reopen"].as_bool() == Some(true) {
            return self.command(&["findings", id, "--reopen"]);
        }
        if body["not_a_bug"].as_bool() != Some(true) {
            return bad("say not_a_bug or reopen");
        }
        // One line, kept short: it shows in lists and in lab-results.
        let note =
            body["note"].as_str().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
        if note.chars().count() > 500 {
            return bad("keep the note under 500 characters");
        }
        if note.is_empty() {
            self.command(&["findings", id, "--not-a-bug"])
        } else {
            self.command(&["findings", id, "--not-a-bug", "--note", &note])
        }
    }

    /// Makes, changes or starts a schedule: `{"save": {...}}`, or
    /// `{"remove" | "pause" | "resume" | "start": name}`. A save holds
    /// what `schedule save` takes: name, title, preset or tests or all,
    /// machines, at and days or each_build, and send.
    fn change_schedule(&self, body: &Value) -> Response {
        for action in ["remove", "pause", "resume", "start"] {
            if let Some(name) = body[action].as_str() {
                if !is_plain_name(name) {
                    return bad("not a schedule name");
                }
                return self.command(&["schedule", action, name]);
            }
        }
        let save = &body["save"];
        let Some(name) = save["name"].as_str().filter(|name| is_plain_name(name)) else {
            return bad("give the schedule a name: lowercase letters, digits and dashes");
        };
        let strings = |field: &str| -> Vec<String> {
            save[field]
                .as_array()
                .map(|items| {
                    items.iter().filter_map(|item| item.as_str().map(String::from)).collect()
                })
                .unwrap_or_default()
        };
        let mut args: Vec<String> = vec!["schedule".into(), "save".into(), name.into()];
        if let Some(preset) = save["preset"].as_str() {
            if !is_plain_name(preset) {
                return bad("not a preset name");
            }
            args.extend(["--preset".into(), preset.into()]);
        } else if save["all"].as_bool() == Some(true) {
            args.push("--all".into());
        } else {
            let tests = strings("tests");
            let plain =
                |test: &String| test.split('/').all(is_plain_name) && test.split('/').count() <= 2;
            if tests.is_empty() || !tests.iter().all(plain) {
                return bad("pick what the schedule runs");
            }
            args.extend(tests);
        }
        let machines = strings("machines");
        if machines.is_empty() || !machines.iter().all(|machine| is_plain_name(machine)) {
            return bad("pick the machines the schedule runs on");
        }
        args.extend(["--machine".into(), machines.join(",")]);
        if save["each_build"].as_bool() == Some(true) {
            args.push("--each-build".into());
        } else {
            let Some(at) = save["at"].as_str() else { return bad("give the time it runs") };
            // A time and day names only; `schedule save` checks them again.
            let time = |text: &str| {
                text.len() == 5 && text.chars().all(|c| c.is_ascii_digit() || c == ':')
            };
            let days = strings("days");
            if !time(at) || !days.iter().all(|day| day.len() == 3 && is_plain_name(day)) {
                return bad("not a time or days");
            }
            args.extend(["--at".into(), at.into()]);
            if !days.is_empty() {
                args.extend(["--days".into(), days.join(",")]);
            }
        }
        args.push(if save["send"].as_bool() == Some(false) { "--hold" } else { "--send" }.into());
        if let Some(title) = save["title"].as_str() {
            let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
            if title.chars().count() > 80 {
                return bad("keep the title under 80 characters");
            }
            if !title.is_empty() {
                args.extend(["--title".into(), title]);
            }
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.command(&args)
    }

    /// Runs `dukovo-lab <args> --json` and answers with what it printed,
    /// the command, and its exit status.
    fn command(&self, args: &[&str]) -> Response {
        let output = Command::new(&self.program)
            .args(&self.folders)
            .arg("--json")
            .args(args)
            .stdin(Stdio::null())
            .output();
        let output = match output {
            Ok(output) => output,
            Err(err) => return Response::text(500, &format!("running dukovo-lab: {err}")),
        };
        // Each command prints one JSON object; the last line is its answer.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let answer = stdout
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str::<Value>(line).ok())
            .unwrap_or(Value::Null);
        let shown: Vec<String> = args.iter().map(|arg| shell_word(arg)).collect();
        Response::json(
            200,
            &json!({
                "command": format!("dukovo-lab {} --json", shown.join(" ")),
                "exit": output.status.code(),
                "answer": answer,
            }),
        )
    }

    /// A file inside a results bundle: `/files/<run id>/<path>`.
    fn bundle_file(&self, path: &str) -> Response {
        let rest = path.trim_start_matches("/files/");
        let Some((id, relative)) = rest.split_once('/') else { return not_found() };
        if !store::valid_run_id(id) {
            return not_found();
        }
        let relative = Path::new(relative);
        // Plain names only, under the bundle's own folders and files.
        let plain = relative.components().all(|part| matches!(part, Component::Normal(_)));
        let first = relative.components().next().and_then(|part| part.as_os_str().to_str());
        let allowed = matches!(
            first,
            Some("logs" | "screenshots" | "run.json" | "results.json" | "events.jsonl")
        );
        if !plain || !allowed {
            return not_found();
        }
        let bundle = store::bundle_dir(&self.store, id);
        // Links inside a bundle must not lead out of it.
        let (Ok(root), Ok(file)) =
            (fs::canonicalize(&bundle), fs::canonicalize(bundle.join(relative)))
        else {
            return not_found();
        };
        if !file.starts_with(&root) || !file.is_file() {
            return not_found();
        }
        let content_type = match file.extension().and_then(|ext| ext.to_str()) {
            Some("png") => "image/png",
            Some("json") => "application/json",
            Some("log" | "jsonl" | "txt") => "text/plain; charset=utf-8",
            _ => "application/octet-stream",
        };
        match fs::read(&file) {
            Ok(bytes) => Response::new(200, content_type, bytes),
            Err(_) => not_found(),
        }
    }
}

/// When the file at `path` last changed.
fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|metadata| metadata.modified()).ok()
}

fn bad(why: &str) -> Response {
    Response::json(400, &json!({ "error": why }))
}

fn not_found() -> Response {
    Response::text(404, "Not here.")
}

fn run_id(body: &Value) -> Option<&str> {
    body["run"].as_str().filter(|id| store::valid_run_id(id))
}

/// `word` as a shell would need it, for showing commands.
fn shell_word(word: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "-_./,=:+".contains(c);
    if !word.is_empty() && word.chars().all(safe) {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    fn page(store: &Path) -> Page {
        Page {
            port: 7420,
            key: "k".repeat(32),
            program: "/bin/false".into(),
            folders: Vec::new(),
            store: store.to_path_buf(),
            results: store.join("lab-results"),
            image: None,
            build: None,
            image_made: None,
        }
    }

    /// A POST from the page itself.
    fn post(page: &Page, path: &str, body: &str) -> Response {
        let mut request = get(path, "127.0.0.1:7420", Some(&page.key));
        request.method = "POST".into();
        request.headers.push(("x-dukovo-lab".into(), "1".into()));
        request.body = body.as_bytes().to_vec();
        page.route(&request)
    }

    /// The command a page action ran, from its answer.
    fn command_of(response: &Response) -> String {
        let answer: Value = serde_json::from_slice(&response.body).unwrap();
        answer["command"].as_str().unwrap_or_default().to_string()
    }

    fn get(path: &str, host: &str, cookie: Option<&str>) -> Request {
        let mut headers = vec![("host".to_string(), host.to_string())];
        if let Some(cookie) = cookie {
            headers.push(("cookie".into(), format!("{COOKIE}={cookie}")));
        }
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        let query = query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        Request { method: "GET".into(), path: path.into(), query, headers, body: Vec::new() }
    }

    #[test]
    fn answers_only_its_own_address_with_its_key() {
        let store = TempDir::new("ui-guard");
        let page = page(store.path());
        let key = page.key.clone();
        let host = "127.0.0.1:7420";
        assert_eq!(page.route(&get("/", "evil.example:7420", Some(&key))).status, 421);
        assert_eq!(page.route(&get("/", host, None)).status, 403);
        assert_eq!(page.route(&get("/", host, Some("wrong"))).status, 403);
        let entry = page.route(&get(&format!("/?key={key}"), host, None));
        assert_eq!(entry.status, 303);
        assert!(entry.headers.iter().any(|(name, value)| *name == "Set-Cookie"
            && value.contains("SameSite=Strict")
            && value.contains("HttpOnly")));
        assert_eq!(page.route(&get("/", "localhost:7420", Some(&key))).status, 200);

        // An action without the page's header, or from another site.
        let mut post = get("/api/stop", host, Some(&key));
        post.method = "POST".into();
        post.body = br#"{"run":"20261107-153012-4f2a"}"#.to_vec();
        assert_eq!(page.route(&post).status, 403);
        post.headers.push(("x-dukovo-lab".into(), "1".into()));
        post.headers.push(("origin".into(), "http://evil.example".into()));
        assert_eq!(page.route(&post).status, 403);
    }

    #[test]
    fn sends_and_marks_findings_only_by_their_ids() {
        let store = TempDir::new("ui-findings");
        let page = page(store.path());
        let id = "f-3a9c51d2e07b";
        let sent = post(&page, "/api/send", &format!(r#"{{"findings":["{id}"]}}"#));
        assert_eq!(command_of(&sent), format!("dukovo-lab send {id} --json"));
        let run = post(&page, "/api/send", r#"{"run":"20261107-153012-4f2a"}"#);
        assert_eq!(command_of(&run), "dukovo-lab send --run 20261107-153012-4f2a --json");
        // Anything else that would read as an option or a path is refused.
        for body in [r#"{"findings":["--all"]}"#, r#"{"findings":[]}"#, r#"{"run":"../x"}"#] {
            assert_eq!(post(&page, "/api/send", body).status, 400, "{body}");
        }
        let marked = post(
            &page,
            "/api/findings",
            &format!(r#"{{"finding":"{id}","not_a_bug":true,"note":"the cable\nwas out"}}"#),
        );
        assert_eq!(
            command_of(&marked),
            format!("dukovo-lab findings {id} --not-a-bug --note 'the cable was out' --json")
        );
        let reopened =
            post(&page, "/api/findings", &format!(r#"{{"finding":"{id}","reopen":true}}"#));
        assert_eq!(command_of(&reopened), format!("dukovo-lab findings {id} --reopen --json"));
        assert_eq!(post(&page, "/api/findings", r#"{"finding":"-x","reopen":true}"#).status, 400);
        let long = format!(r#"{{"finding":"{id}","not_a_bug":true,"note":"{}"}}"#, "x".repeat(501));
        assert_eq!(post(&page, "/api/findings", &long).status, 400);
    }

    #[test]
    fn makes_schedules_only_from_plain_names_and_times() {
        let store = TempDir::new("ui-schedules");
        let page = page(store.path());
        let saved = post(
            &page,
            "/api/schedules",
            r#"{"save":{"name":"nightly","title":"Full run","preset":"full","machines":["qemu","optiplex"],"at":"01:00","days":["sun"],"send":false}}"#,
        );
        assert_eq!(
            command_of(&saved),
            "dukovo-lab schedule save nightly --preset full --machine qemu,optiplex --at 01:00 --days sun --hold --title 'Full run' --json"
        );
        let each = post(
            &page,
            "/api/schedules",
            r#"{"save":{"name":"quick","tests":["boot","drivers/link-pull"],"machines":["qemu"],"each_build":true}}"#,
        );
        assert_eq!(
            command_of(&each),
            "dukovo-lab schedule save quick boot drivers/link-pull --machine qemu --each-build --send --json"
        );
        let started = post(&page, "/api/schedules", r#"{"start":"nightly"}"#);
        assert_eq!(command_of(&started), "dukovo-lab schedule start nightly --json");
        for body in [
            r#"{"start":"--all"}"#,
            r#"{"save":{"name":"-x","all":true,"machines":["qemu"],"each_build":true}}"#,
            r#"{"save":{"name":"x","tests":["--all"],"machines":["qemu"],"each_build":true}}"#,
            r#"{"save":{"name":"x","all":true,"machines":[],"each_build":true}}"#,
            r#"{"save":{"name":"x","all":true,"machines":["qemu"],"at":"--now"}}"#,
            r#"{"save":{"name":"x","all":true,"machines":["qemu"],"at":"01:00","days":["--x"]}}"#,
            r#"{"save":{"name":"x","all":true,"machines":["qemu"]}}"#,
        ] {
            assert_eq!(post(&page, "/api/schedules", body).status, 400, "{body}");
        }
    }

    #[test]
    fn serves_bundle_files_and_nothing_else() {
        let store = TempDir::new("ui-files");
        let page = page(store.path());
        let key = page.key.clone();
        let id = "20261107-153012-4f2a";
        let bundle = store::bundle_dir(store.path(), id);
        fs::create_dir_all(bundle.join("logs/qemu")).unwrap();
        fs::write(bundle.join("logs/qemu/boot-a.log"), "booted\n").unwrap();
        fs::write(store.path().join("secret"), "lab key").unwrap();
        let fetch = |path: &str| page.route(&get(path, "127.0.0.1:7420", Some(&key)));
        let log = fetch(&format!("/files/{id}/logs/qemu/boot-a.log"));
        assert_eq!((log.status, log.body.as_slice()), (200, b"booted\n".as_slice()));
        for path in [
            format!("/files/{id}/../../../secret"),
            format!("/files/{id}/logs/../../../../secret"),
            format!("/files/{id}/other/file"),
            "/files/..%2f/secret".to_string(),
            "/files/x/logs/qemu/boot-a.log".to_string(),
        ] {
            assert_eq!(fetch(&path).status, 404, "{path}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(store.path().join("secret"), bundle.join("logs/link.log"))
                .unwrap();
            assert_eq!(fetch(&format!("/files/{id}/logs/link.log")).status, 404);
        }
    }

    #[test]
    fn a_rebuilt_image_is_not_run_under_the_old_build_s_name() {
        let store = TempDir::new("ui-rebuilt");
        let image = store.write("dukovo-lab.img", "build one");
        let mut page = page(store.path());
        page.image = Some(image.clone());
        page.image_made = modified(&image);
        let body = json!({ "tests": ["boot/ready"], "machines": ["qemu"] });
        // As made when the page opened: the run starts (here /bin/false
        // stands in for dukovo-lab, so the answer is its exit status).
        assert_eq!(page.start_run(&body).status, 200);
        // Rebuilt since: refused, with what to do.
        page.image_made = Some(SystemTime::UNIX_EPOCH);
        let refused = page.start_run(&body);
        assert_eq!(refused.status, 400);
        assert!(String::from_utf8_lossy(&refused.body).contains("open it again with `ob lab ui`"));
    }

    #[test]
    fn shows_commands_as_a_shell_reads_them() {
        assert_eq!(shell_word("drivers/link-pull"), "drivers/link-pull");
        assert_eq!(shell_word("0.13.0 fd2a9e8"), "'0.13.0 fd2a9e8'");
        assert_eq!(shell_word("it's"), "'it'\\''s'");
    }
}

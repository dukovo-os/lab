// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A Redfish client: just enough to drive a server through its BMC (a
//! Dell iDRAC, an HPE iLO, a Lenovo XCC, an OpenBMC).
//!
//! Redfish is JSON over HTTPS. The lab has no TLS library, so each call
//! is one run of the system's `curl`, as notify.rs runs it, and the whole
//! request travels on curl's standard input as a config (`--config -`):
//! arguments are visible to other accounts on the PC while a program
//! runs, and the BMC's password is in the request. Every value goes into
//! the config as a double-quoted string with its backslash, quote and
//! line ends escaped ([`config_string`]), so no value can end its line
//! early and add an option of its own; a value with another control
//! character is refused.
//!
//! A BMC's certificate is self-signed, so curl runs with `insecure` and a
//! `pinnedpubkey`: curl checks the pin even then, and the pin, the hash of
//! the BMC's public key from the machine file, is what shows that the
//! lab is talking to its BMC. A client for an address that is not
//! 127.0.0.1 is refused without a pin, and plain HTTP is allowed only for
//! 127.0.0.1, which the tests' pretend BMC answers on. [`pin_of`] reads
//! the pin from a BMC, for `dukovo-lab machines check` to print on the
//! first setup.
//!
//! Answers are read as [`serde_json::Value`]. The system and the manager
//! are the first members of `/redfish/v1/Systems` and
//! `/redfish/v1/Managers` (on a Dell, `System.Embedded.1` and
//! `iDRAC.Embedded.1`). A call that fails says what the BMC answered,
//! with its own message when the body has one, and never the password.
//! When a run fails with such a message, `logs/<machine>/redfish.log`
//! says which call it was.

use std::collections::BTreeSet;
use std::net::{TcpStream, ToSocketAddrs as _};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

use crate::driver::real::run_bounded;
use crate::hash::{Hasher as _, Sha256};
use crate::redteam::base64;

/// How long curl may take in all for one call, and to connect.
const CALL_TIME: &str = "30";
const CONNECT_TIME: &str = "10";

/// The most pages of a log's entries that are followed: iDRAC's
/// lifecycle log can hold thousands of entries.
const EVENT_PAGES: usize = 50;

/// The port an iDRAC's VNC server listens on unless its attributes say
/// otherwise.
pub const DEFAULT_VNC_PORT: u16 = 5901;

/// How long reading a BMC's certificate may take.
const PIN_TIME: Duration = Duration::from_secs(20);

/// The only address plain HTTP is allowed for.
const LOOPBACK: &str = "127.0.0.1";

/// `ComputerSystem.Reset`'s types the lab uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetKind {
    /// Turns the machine on.
    On,
    /// Turns it off at once, with no warning to the system.
    ForceOff,
    /// Asks the system to shut down.
    GracefulShutdown,
    /// Restarts it at once, with no warning to the system.
    ForceRestart,
}

impl ResetKind {
    /// Redfish's name for it.
    pub fn name(self) -> &'static str {
        match self {
            ResetKind::On => "On",
            ResetKind::ForceOff => "ForceOff",
            ResetKind::GracefulShutdown => "GracefulShutdown",
            ResetKind::ForceRestart => "ForceRestart",
        }
    }
}

/// Whose BMC it is, as far as the lab has quirks for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vendor {
    Dell,
    Hpe,
    Lenovo,
    Supermicro,
    Other,
}

impl Vendor {
    /// The vendor's name, for a message.
    pub fn name(self) -> &'static str {
        match self {
            Vendor::Dell => "Dell",
            Vendor::Hpe => "HPE",
            Vendor::Lenovo => "Lenovo",
            Vendor::Supermicro => "Supermicro",
            Vendor::Other => "another vendor",
        }
    }
}

/// What a BMC says it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub vendor: Vendor,
    /// The server's model, such as `PowerEdge R740`.
    pub model: String,
    /// The BMC's firmware version.
    pub firmware: String,
}

/// One entry of a BMC's event logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// The log service's path: with `id`, what names the entry.
    pub service: String,
    pub id: String,
    pub created: String,
    pub severity: String,
    pub message_id: String,
    pub message: String,
}

impl Event {
    /// What names the entry: a log's entries are numbered within the
    /// log service alone.
    pub fn key(&self) -> (String, String) {
        (self.service.clone(), self.id.clone())
    }

    /// The entry as one line for `bmc-events.log`: when, how severe, the
    /// message's id and its text.
    pub fn line(&self) -> String {
        let field = |text: &str| {
            let text: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
            let text = text.trim();
            if text.is_empty() { "-".to_string() } else { text.to_string() }
        };
        format!(
            "{} {} {} {}",
            field(&self.created).replace(' ', "_"),
            field(&self.severity).replace(' ', "_"),
            field(&self.message_id).replace(' ', "_"),
            field(&self.message)
        )
    }
}

/// Where the system and the manager are, once found.
#[derive(Default)]
struct Found {
    system: Option<String>,
    manager: Option<String>,
}

/// A client for one BMC.
pub struct Redfish {
    /// `https://host[:port]`, or `http://127.0.0.1:port`.
    base: String,
    user: String,
    password: String,
    pin: Option<String>,
    /// The `curl` program; the tests put a stand-in here.
    curl: PathBuf,
    found: Mutex<Found>,
}

/// `text` as a double-quoted word in a curl config: its backslash and
/// quote escaped and its tab and line ends written out, so it stays on
/// one line and ends only at its own closing quote.
///
/// # Errors
///
/// Fails, naming `what` and never the text, when it holds another control
/// character, which a config has no way to write.
fn config_string(what: &str, text: &str) -> Result<String, String> {
    let mut word = String::with_capacity(text.len() + 2);
    word.push('"');
    for c in text.chars() {
        match c {
            '\\' => word.push_str("\\\\"),
            '"' => word.push_str("\\\""),
            '\n' => word.push_str("\\n"),
            '\r' => word.push_str("\\r"),
            '\t' => word.push_str("\\t"),
            c if c.is_control() => {
                return Err(format!(
                    "the {what} holds a control character, which the BMC's request cannot carry"
                ));
            }
            c => word.push(c),
        }
    }
    word.push('"');
    Ok(word)
}

/// Adds the config line `key = "value"`.
fn add_line(config: &mut String, key: &str, what: &str, value: &str) -> Result<(), String> {
    config.push_str(&format!("{key} = {}\n", config_string(what, value)?));
    Ok(())
}

/// Whether `text` is a pin as curl writes it: `sha256//` and the base64
/// of a SHA-256 hash.
pub fn is_pin(text: &str) -> bool {
    let Some(hash) = text.strip_prefix("sha256//") else { return false };
    hash.len() == 44
        && hash.ends_with('=')
        && hash[..43].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

/// `text` with every `secret` in it hidden: a BMC's message about a
/// value it refused may repeat the value.
fn hide(text: &str, secret: &str) -> String {
    if secret.is_empty() { text.to_string() } else { text.replace(secret, "***") }
}

impl Redfish {
    /// A client for the BMC at `base`, which signs in as `user`.
    ///
    /// # Errors
    ///
    /// Fails when `base` is not `https://host[:port]` with a `pin`, or
    /// `http://127.0.0.1[:port]`, or when the pin is not a pin.
    pub fn new(
        base: &str,
        user: &str,
        password: &str,
        pin: Option<&str>,
    ) -> Result<Redfish, String> {
        let Some((scheme, host)) = base.split_once("://") else {
            return Err(format!("{base} is not an address with http:// or https://"));
        };
        if host.is_empty() || host.contains(['/', '@', '?', '#']) {
            return Err(format!("{base} is not a BMC's address: it is a host and a port alone"));
        }
        match (scheme, pin) {
            ("https", None) => {
                return Err(format!(
                    "the BMC at {host} has a certificate of its own, so the lab needs its pin to \
                     know it is the BMC (redfish.pin); `dukovo-lab machines check` prints it"
                ));
            }
            ("https", Some(pin)) if !is_pin(pin) => {
                return Err("redfish.pin must be sha256// and the base64 of the BMC's public \
                            key's SHA-256 hash"
                    .into());
            }
            ("https", Some(_)) => {}
            ("http", None) if host.split(':').next() == Some(LOOPBACK) => {}
            _ => {
                return Err(format!(
                    "{base}: the lab talks to a BMC over https with its pin, and over plain http \
                     only to {LOOPBACK}"
                ));
            }
        }
        Ok(Redfish {
            base: base.to_string(),
            user: user.to_string(),
            password: password.to_string(),
            pin: pin.map(str::to_string),
            curl: "curl".into(),
            found: Mutex::new(Found::default()),
        })
    }

    /// The curl config for one call. The URL, the sign-in, the method and
    /// the body are each one quoted word; the sign-in is the only place
    /// the password is.
    fn config(&self, method: &str, path: &str, body: Option<&str>) -> Result<String, String> {
        let mut config = String::new();
        add_line(&mut config, "url", "address", &format!("{}{path}", self.base))?;
        let sign_in = format!("{}:{}", self.user, self.password);
        add_line(&mut config, "user", "user name or password", &sign_in)?;
        add_line(&mut config, "request", "method", method)?;
        add_line(&mut config, "header", "header", "Accept: application/json")?;
        if let Some(body) = body {
            add_line(&mut config, "header", "header", "Content-Type: application/json")?;
            // data-raw: data-binary reads a file when its text starts with @.
            add_line(&mut config, "data-raw", "request body", body)?;
        }
        add_line(&mut config, "write-out", "write-out", "\n%{http_code}")?;
        if let Some(pin) = &self.pin {
            config.push_str("insecure\n");
            add_line(&mut config, "pinnedpubkey", "pin", pin)?;
        }
        Ok(config)
    }

    /// Makes one call and gives the answer's body, parsed; an empty body
    /// is null.
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        if !path.starts_with("/redfish/") {
            return Err(format!("the BMC named {path}, which is not a Redfish path"));
        }
        let body = body.map(Value::to_string);
        let config = self.config(method, path, body.as_deref())?;
        let (text, status) = self.run_curl(&config)?;
        let answer: Result<Value, _> =
            if text.trim().is_empty() { Ok(Value::Null) } else { serde_json::from_str(&text) };
        if (200..300).contains(&status) {
            return answer.map_err(|_| {
                format!(
                    "the BMC at {} answered {method} {path} with text that is not JSON",
                    self.base
                )
            });
        }
        let why = answer.ok().and_then(|answer| error_message(&answer)).unwrap_or_default();
        let hint = if status == 401 {
            ": check the BMC's user and password (redfish.user, redfish.password_file)"
        } else {
            ""
        };
        let why = if why.is_empty() { String::new() } else { format!(": {why}") };
        Err(hide(
            &format!("the BMC at {} answered {status} to {method} {path}{why}{hint}", self.base),
            &self.password,
        ))
    }

    /// Runs curl with `config` on its standard input and gives the
    /// answer's body and status.
    fn run_curl(&self, config: &str) -> Result<(String, u16), String> {
        let scheme = if self.pin.is_some() { "=https" } else { "=http" };
        let mut curl = Command::new(&self.curl);
        // -q first: a .curlrc of the PC's account must not add options.
        curl.args(["-q", "--silent", "--show-error"])
            .args(["--max-time", CALL_TIME, "--connect-timeout", CONNECT_TIME])
            .args(["--proto", scheme, "--config", "-"]);
        let output =
            run_bounded(&mut curl, config.as_bytes(), Duration::from_secs(45)).map_err(|err| {
                if err.contains("not installed") {
                    "curl is not installed on the lab PC; install it (`sudo apt install curl`)"
                        .to_string()
                } else {
                    err
                }
            })?;
        if !output.status.success() {
            let said = String::from_utf8_lossy(&output.stderr);
            let said = said.lines().next().unwrap_or("it gave no reason");
            let said = said.trim_start_matches("curl: ");
            let hint = if said.contains("pinned public key") {
                ": the BMC's key is not the pin in the machine file (redfish.pin); `dukovo-lab \
                 machines check` prints the BMC's"
            } else {
                ""
            };
            return Err(hide(
                &format!("the BMC at {} did not answer: {said}{hint}", self.base),
                &self.password,
            ));
        }
        let output = String::from_utf8_lossy(&output.stdout);
        let (body, status) = output
            .rsplit_once('\n')
            .ok_or_else(|| format!("the BMC at {} gave no status", self.base))?;
        let status = status
            .trim()
            .parse()
            .map_err(|_| format!("the BMC at {} gave a status curl could not read", self.base))?;
        Ok((body.to_string(), status))
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        self.call("GET", path, None)
    }

    /// The first member of the collection at `path`.
    fn first_member(&self, path: &str) -> Result<String, String> {
        let collection = self.get(path)?;
        collection["Members"][0]["@odata.id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("the BMC at {} lists no member in {path}", self.base))
    }

    /// The system's path, found the first time.
    ///
    /// # Errors
    ///
    /// Fails when the BMC does not answer or lists no system.
    pub fn system(&self) -> Result<String, String> {
        if let Some(system) = &self.lock().system {
            return Ok(system.clone());
        }
        let system = self.first_member("/redfish/v1/Systems")?;
        self.lock().system = Some(system.clone());
        Ok(system)
    }

    /// The manager's path, found the first time.
    ///
    /// # Errors
    ///
    /// Fails when the BMC does not answer or lists no manager.
    pub fn manager(&self) -> Result<String, String> {
        if let Some(manager) = &self.lock().manager {
            return Ok(manager.clone());
        }
        let manager = self.first_member("/redfish/v1/Managers")?;
        self.lock().manager = Some(manager.clone());
        Ok(manager)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Found> {
        self.found.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the machine is on: `On`, and `PoweringOn` and `PoweringOff`
    /// too, since it is drawing power until `Off`.
    ///
    /// # Errors
    ///
    /// Fails when the BMC does not answer or gives no power state the
    /// lab knows.
    pub fn power_state(&self) -> Result<bool, String> {
        let system = self.get(&self.system()?)?;
        match system["PowerState"].as_str() {
            Some("On" | "PoweringOn" | "PoweringOff") => Ok(true),
            Some("Off") => Ok(false),
            Some(other) => Err(format!(
                "the BMC gave the power state \"{other}\", which the lab does not know"
            )),
            None => Err(format!("the BMC at {} gives the system no PowerState", self.base)),
        }
    }

    /// Does a `ComputerSystem.Reset` of `kind`.
    ///
    /// # Errors
    ///
    /// Fails when the BMC lists the types it takes and `kind` is not
    /// one, when it names no target, and when it turns the request down.
    pub fn reset(&self, kind: ResetKind) -> Result<(), String> {
        let system = self.get(&self.system()?)?;
        let action = &system["Actions"]["#ComputerSystem.Reset"];
        let target = action["target"]
            .as_str()
            .ok_or_else(|| format!("the BMC at {} offers no ComputerSystem.Reset", self.base))?;
        if let Some(allowed) = action["ResetType@Redfish.AllowableValues"].as_array() {
            let allowed: Vec<&str> = allowed.iter().filter_map(Value::as_str).collect();
            if !allowed.contains(&kind.name()) {
                return Err(format!(
                    "the BMC does not take the reset type {} (it takes {})",
                    kind.name(),
                    allowed.join(", ")
                ));
            }
        }
        self.call("POST", target, Some(&json!({ "ResetType": kind.name() }))).map(drop)
    }

    /// Has the machine boot from the network, with UEFI, the next time
    /// only.
    ///
    /// # Errors
    ///
    /// Fails when the BMC turns the request down.
    pub fn boot_from_network_once(&self) -> Result<(), String> {
        let boot = json!({ "Boot": {
            "BootSourceOverrideEnabled": "Once",
            "BootSourceOverrideTarget": "Pxe",
            "BootSourceOverrideMode": "UEFI",
        }});
        self.call("PATCH", &self.system()?, Some(&boot)).map(drop)
    }

    /// Whether the BMC lets the boot be set to the network once, when it
    /// lists the targets it takes; true when it lists none.
    ///
    /// # Errors
    ///
    /// Fails when the BMC does not answer.
    pub fn network_boot_allowed(&self) -> Result<bool, String> {
        let system = self.get(&self.system()?)?;
        let allowed = &system["Boot"]["BootSourceOverrideTarget@Redfish.AllowableValues"];
        Ok(allowed.as_array().is_none_or(|targets| targets.iter().any(|t| t == "Pxe")))
    }

    /// What the BMC says it is.
    ///
    /// # Errors
    ///
    /// Fails when the BMC does not answer.
    pub fn identity(&self) -> Result<Identity, String> {
        let manager = self.get(&self.manager()?)?;
        let system = self.get(&self.system()?)?;
        let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
        let oem: Vec<&str> = manager["Oem"]
            .as_object()
            .map(|oem| oem.keys().map(String::as_str).collect())
            .unwrap_or_default();
        Ok(Identity {
            vendor: vendor_of(manager["Manufacturer"].as_str().unwrap_or_default(), &oem),
            model: text(&system["Model"]),
            firmware: text(&manager["FirmwareVersion"]),
        })
    }

    /// The manager's vendor.
    ///
    /// # Errors
    ///
    /// Fails when the BMC does not answer.
    pub fn vendor(&self) -> Result<Vendor, String> {
        self.identity().map(|identity| identity.vendor)
    }

    /// The attributes of the system's BIOS.
    ///
    /// # Errors
    ///
    /// Fails when the BMC does not answer.
    pub fn bios_attributes(&self) -> Result<Value, String> {
        Ok(self.get(&format!("{}/Bios", self.system()?))?["Attributes"].clone())
    }

    /// The attributes of the manager (on a Dell, the iDRAC's settings).
    ///
    /// # Errors
    ///
    /// Fails when the BMC does not answer.
    pub fn manager_attributes(&self) -> Result<Value, String> {
        Ok(self.get(&format!("{}/Attributes", self.manager()?))?["Attributes"].clone())
    }

    /// Every entry of the log services of the manager and the system
    /// (an iDRAC9 keeps its system event log under the system), each
    /// service's entries followed over at most [`EVENT_PAGES`] pages.
    ///
    /// # Errors
    ///
    /// Fails when the BMC does not answer.
    pub fn event_entries(&self) -> Result<Vec<Event>, String> {
        let mut services = BTreeSet::new();
        for owner in [self.manager()?, self.system()?] {
            let owner = self.get(&owner)?;
            let Some(collection) = owner["LogServices"]["@odata.id"].as_str() else { continue };
            for member in self.get(collection)?["Members"].as_array().into_iter().flatten() {
                services.extend(member["@odata.id"].as_str().map(str::to_string));
            }
        }
        let mut events = Vec::new();
        for service in services {
            let Some(entries) =
                self.get(&service)?["Entries"]["@odata.id"].as_str().map(str::to_string)
            else {
                continue;
            };
            let mut next = Some(entries);
            for _ in 0..EVENT_PAGES {
                let Some(path) = next.take() else { break };
                let page = self.get(&path)?;
                for entry in page["Members"].as_array().into_iter().flatten() {
                    let text = |name: &str| entry[name].as_str().unwrap_or_default().to_string();
                    events.push(Event {
                        service: service.clone(),
                        id: text("Id"),
                        created: text("Created"),
                        severity: text("Severity"),
                        message_id: text("MessageId"),
                        message: text("Message"),
                    });
                }
                next = page["Members@odata.nextLink"].as_str().map(str::to_string);
            }
        }
        Ok(events)
    }

    /// Turns a Dell iDRAC's VNC server on, without encryption (the
    /// lab's VNC client has none) and with `password`, and gives the port
    /// it listens on.
    ///
    /// # Errors
    ///
    /// Fails, without the password in the message, when the BMC turns
    /// the change down: an iDRAC without a virtual console licence does.
    pub fn enable_vnc(&self, password: &str) -> Result<u16, String> {
        let attributes = json!({ "Attributes": {
            "VNCServer.1.Enable": "Enabled",
            "VNCServer.1.Password": password,
            "VNCServer.1.SSLEncryptionBitLength": "Disabled",
        }});
        let path = format!("{}/Attributes", self.manager()?);
        self.call("PATCH", &path, Some(&attributes)).map_err(|err| hide(&err, password))?;
        let port = self.manager_attributes()?["VNCServer.1.Port"].as_u64();
        Ok(port.and_then(|port| u16::try_from(port).ok()).unwrap_or(DEFAULT_VNC_PORT))
    }
}

/// The vendor a `Manufacturer` text or the names under `Oem` show.
fn vendor_of(manufacturer: &str, oem: &[&str]) -> Vendor {
    let by = |name: &str| {
        let name = name.to_ascii_lowercase();
        if name.contains("dell") {
            Vendor::Dell
        } else if name.contains("hpe") || name.contains("hewlett") {
            Vendor::Hpe
        } else if name.contains("lenovo") {
            Vendor::Lenovo
        } else if name.contains("supermicro") || name.contains("super micro") {
            Vendor::Supermicro
        } else {
            Vendor::Other
        }
    };
    match by(manufacturer) {
        Vendor::Other => {
            oem.iter().map(|name| by(name)).find(|&v| v != Vendor::Other).unwrap_or(Vendor::Other)
        }
        vendor => vendor,
    }
}

/// The message a Redfish error body carries: the first extended
/// message, which says more than the generic one before it.
fn error_message(body: &Value) -> Option<String> {
    let error = &body["error"];
    let extended = error["@Message.ExtendedInfo"][0]["Message"].as_str();
    extended.or_else(|| error["message"].as_str()).map(|message| message.trim().to_string())
}

/// A password for an iDRAC's VNC server, made afresh for each run: VNC's
/// are at most eight characters, and letters and digits alone need no
/// care in a config or a log.
///
/// # Errors
///
/// Fails when the system gives no random bytes.
pub fn screen_password() -> Result<String, String> {
    const CHARACTERS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let hex = crate::netboot::random_hex(8)?;
    Ok((0..8)
        .map(|n| {
            let byte = u8::from_str_radix(&hex[2 * n..2 * n + 2], 16).unwrap_or(0);
            char::from(CHARACTERS[usize::from(byte) % CHARACTERS.len()])
        })
        .collect())
}

/// The pin of the BMC at `host` (`host` or `host:port`, port 443 unless
/// named), as curl's `pinnedpubkey` takes it: `sha256//` and the base64
/// of the SHA-256 hash of its public key. It is read with `openssl`,
/// from the certificate the BMC offers, and is only as trustworthy as
/// the network between the lab PC and the BMC: the lab's own.
///
/// # Errors
///
/// Fails when the BMC cannot be reached or `openssl` cannot read what it
/// offers.
pub fn pin_of(host: &str) -> Result<String, String> {
    let (name, port) = host.rsplit_once(':').unwrap_or((host, "443"));
    let address = format!("{name}:{port}");
    let target = address
        .to_socket_addrs()
        .map_err(|err| format!("{address}: {err}"))?
        .next()
        .ok_or_else(|| format!("{address} has no address"))?;
    TcpStream::connect_timeout(&target, Duration::from_secs(5))
        .map_err(|err| format!("{address}: {err}"))?;
    let openssl = |args: &[&str], input: &[u8]| -> Result<Vec<u8>, String> {
        let output = run_bounded(Command::new("openssl").args(args), input, PIN_TIME)?;
        if output.status.success() {
            return Ok(output.stdout);
        }
        let said = String::from_utf8_lossy(&output.stderr);
        Err(format!("openssl {} said: {}", args[0], said.lines().next().unwrap_or("nothing")))
    };
    let offered = openssl(&["s_client", "-connect", &address, "-servername", name], b"")?;
    let key = openssl(&["x509", "-pubkey", "-noout"], &offered)?;
    let der = openssl(&["pkey", "-pubin", "-outform", "DER"], &key)?;
    let mut hash = Sha256::new();
    hash.update(&der);
    Ok(format!("sha256//{}", base64(&hash.finish())))
}

/// A pretend BMC for tests: Redfish's resources for a Dell iDRAC9, over
/// plain HTTP on 127.0.0.1, which keeps a power state and takes the
/// reset, the boot override and the VNC attributes.
#[cfg(test)]
pub mod fake {
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};

    use crate::http::{self, Response};
    use crate::redteam::base64;

    pub const USER: &str = "root";
    /// A password with the characters a curl config would trip over.
    pub const PASSWORD: &str = "Lab \"Bmc\" #1\\";
    pub const SYSTEM: &str = "/redfish/v1/Systems/System.Embedded.1";
    pub const MANAGER: &str = "/redfish/v1/Managers/iDRAC.Embedded.1";
    /// How many entries a page of a log holds.
    const PAGE: usize = 2;

    /// What happens at the pretend machine when it is turned on or reset:
    /// told whether the boot is from the network.
    pub type OnBoot = Box<dyn Fn(bool) + Send>;

    /// The pretend BMC's state.
    pub struct State {
        /// The system's `PowerState`.
        pub power: String,
        /// How many reads after a turn on still say `PoweringOn`.
        pub powering_on_reads: u32,
        /// Each change and read of a setting, as `METHOD path detail`.
        pub calls: Vec<String>,
        /// Set by the boot override, used up by the next start.
        pub network_next: bool,
        pub on_boot: Option<OnBoot>,
        pub manufacturer: String,
        /// The vendor name under the manager's `Oem`.
        pub oem: String,
        /// How many `ForceOff`s the BMC takes and does nothing with.
        pub ignored_force_offs: u32,
        /// The reset types the system lists; none lists none.
        pub reset_types: Option<Vec<String>>,
        pub boot_targets: Vec<String>,
        /// Whether the system's `PowerState` is left out.
        pub no_power_state: bool,
        /// Whether the BMC has a virtual console licence.
        pub licensed: bool,
        pub vnc_enabled: bool,
        pub vnc_password: Option<String>,
        pub vnc_port: u16,
        pub ipmi_lan: String,
        pub bios: Value,
        /// The system event log (under the system) and the lifecycle log
        /// (under the manager).
        pub sel: Vec<Value>,
        pub lclog: Vec<Value>,
    }

    impl State {
        /// The password a VNC client signs in with, while the server is on.
        pub fn open_password(&self) -> Option<String> {
            self.vnc_password.clone().filter(|_| self.vnc_enabled)
        }
    }

    /// An entry of a log.
    pub fn entry(id: u32, created: &str, severity: &str, message: &str) -> Value {
        json!({ "Id": id.to_string(), "Created": created, "Severity": severity,
                "MessageId": "SYS1000", "Message": message })
    }

    /// A pretend BMC, listening until dropped.
    pub struct FakeBmc {
        pub address: SocketAddr,
        pub state: Arc<Mutex<State>>,
        done: Arc<AtomicBool>,
    }

    impl FakeBmc {
        pub fn start() -> FakeBmc {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let state = Arc::new(Mutex::new(State {
                power: "Off".into(),
                powering_on_reads: 0,
                calls: Vec::new(),
                network_next: false,
                on_boot: None,
                manufacturer: "Dell Inc.".into(),
                oem: "Dell".into(),
                ignored_force_offs: 0,
                reset_types: Some(
                    ["On", "ForceOff", "GracefulShutdown", "ForceRestart"]
                        .map(String::from)
                        .to_vec(),
                ),
                boot_targets: ["None", "Pxe", "Hdd"].map(String::from).to_vec(),
                no_power_state: false,
                licensed: true,
                vnc_enabled: false,
                vnc_password: None,
                vnc_port: 5901,
                ipmi_lan: "Enabled".into(),
                bios: json!({ "BootMode": "Uefi", "SerialComm": "OnConRedirCom2",
                              "PxeDev1EnDis": "Enabled" }),
                sel: vec![
                    entry(1, "2026-11-20T10:00:00-06:00", "OK", "old one"),
                    entry(2, "2026-11-20T10:01:00-06:00", "OK", "old two"),
                    entry(3, "2026-11-20T10:02:00-06:00", "Warning", "old three"),
                ],
                lclog: vec![entry(1, "2026-11-20T09:00:00-06:00", "OK", "old log entry")],
            }));
            let done = Arc::new(AtomicBool::new(false));
            {
                let (state, done) = (Arc::clone(&state), Arc::clone(&done));
                std::thread::spawn(move || {
                    while !done.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((stream, _)) => answer(&stream, &state),
                            Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
                        }
                    }
                });
            }
            FakeBmc { address, state, done }
        }

        pub fn calls(&self) -> Vec<String> {
            self.state.lock().unwrap().calls.clone()
        }
    }

    impl Drop for FakeBmc {
        fn drop(&mut self) {
            self.done.store(true, Ordering::Relaxed);
        }
    }

    fn error(status: u16, message: &str) -> Response {
        let body = json!({ "error": { "code": "Base.1.12.GeneralError",
            "message": "A general error has occurred. See ExtendedInfo for more information.",
            "@Message.ExtendedInfo": [{ "Message": message }] } });
        Response::json(status, &body)
    }

    fn answer(stream: &TcpStream, state: &Arc<Mutex<State>>) {
        let _ = stream.set_nonblocking(false);
        let Ok(request) = http::read_request(stream) else { return };
        let expected = format!("Basic {}", base64(format!("{USER}:{PASSWORD}").as_bytes()));
        let response = if request.header("authorization") != Some(expected.as_str()) {
            error(401, "Unable to authenticate the user.")
        } else {
            route(&request, &mut state.lock().unwrap())
        };
        http::write_response(stream, &response);
    }

    fn page_of(entries: &[Value], path: &str, skip: usize) -> Value {
        let end = (skip + PAGE).min(entries.len());
        let mut page = json!({ "Members": entries[skip.min(end)..end] });
        if end < entries.len() {
            page["Members@odata.nextLink"] = json!(format!("{path}?$skip={end}"));
        }
        page
    }

    fn route(request: &http::Request, state: &mut State) -> Response {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let path = request.path.as_str();
        let reset = format!("{SYSTEM}/Actions/ComputerSystem.Reset");
        let sel_entries = format!("{SYSTEM}/LogServices/Sel/Entries");
        let lclog_entries = format!("{MANAGER}/LogServices/Lclog/Entries");
        let skip = request.query("$skip").and_then(|skip| skip.parse().ok()).unwrap_or(0);
        match (request.method.as_str(), path) {
            ("GET", "/redfish/v1/Systems") => {
                Response::json(200, &json!({ "Members": [{ "@odata.id": SYSTEM }] }))
            }
            ("GET", "/redfish/v1/Managers") => {
                Response::json(200, &json!({ "Members": [{ "@odata.id": MANAGER }] }))
            }
            ("GET", SYSTEM) => Response::json(200, &system(state, &reset)),
            ("PATCH", SYSTEM) => {
                let boot = &body["Boot"];
                state.calls.push(format!(
                    "PATCH Boot {} {} {}",
                    boot["BootSourceOverrideTarget"].as_str().unwrap_or("-"),
                    boot["BootSourceOverrideEnabled"].as_str().unwrap_or("-"),
                    boot["BootSourceOverrideMode"].as_str().unwrap_or("-")
                ));
                state.network_next = boot["BootSourceOverrideTarget"] == "Pxe";
                Response::json(200, &Value::Null)
            }
            ("POST", p) if p == reset => {
                reset_system(state, body["ResetType"].as_str().unwrap_or(""))
            }
            ("GET", MANAGER) => Response::json(
                200,
                &json!({
                    "Manufacturer": state.manufacturer, "Model": "14G Monolithic",
                    "FirmwareVersion": "4.40.00.00", "Oem": { state.oem.clone(): {} },
                    "LogServices": { "@odata.id": format!("{MANAGER}/LogServices") },
                }),
            ),
            ("GET", p) if p == format!("{MANAGER}/Attributes") => {
                let mut attributes = json!({ "IPMILan.1.Enable": state.ipmi_lan });
                if state.licensed {
                    attributes["VNCServer.1.Enable"] =
                        json!(if state.vnc_enabled { "Enabled" } else { "Disabled" });
                    attributes["VNCServer.1.Port"] = json!(state.vnc_port);
                }
                Response::json(200, &json!({ "Attributes": attributes }))
            }
            ("PATCH", p) if p == format!("{MANAGER}/Attributes") => {
                if !state.licensed {
                    return error(400, "The VNC server needs a virtual console licence.");
                }
                let attributes = &body["Attributes"];
                state.calls.push(format!(
                    "PATCH VNC {} {} {}",
                    attributes["VNCServer.1.Enable"].as_str().unwrap_or("-"),
                    attributes["VNCServer.1.SSLEncryptionBitLength"].as_str().unwrap_or("-"),
                    attributes["VNCServer.1.Password"].as_str().map_or(0, str::len)
                ));
                state.vnc_enabled = attributes["VNCServer.1.Enable"] == "Enabled";
                state.vnc_password = attributes["VNCServer.1.Password"].as_str().map(String::from);
                Response::json(200, &Value::Null)
            }
            ("GET", p) if p == format!("{SYSTEM}/Bios") => {
                Response::json(200, &json!({ "Attributes": state.bios }))
            }
            ("GET", p) if p == format!("{SYSTEM}/LogServices") => Response::json(
                200,
                &json!({ "Members": [{ "@odata.id": format!("{SYSTEM}/LogServices/Sel") }] }),
            ),
            ("GET", p) if p == format!("{MANAGER}/LogServices") => Response::json(
                200,
                &json!({ "Members": [{ "@odata.id": format!("{MANAGER}/LogServices/Lclog") }] }),
            ),
            ("GET", p) if p == format!("{SYSTEM}/LogServices/Sel") => {
                Response::json(200, &json!({ "Entries": { "@odata.id": sel_entries } }))
            }
            ("GET", p) if p == format!("{MANAGER}/LogServices/Lclog") => {
                Response::json(200, &json!({ "Entries": { "@odata.id": lclog_entries } }))
            }
            ("GET", p) if p == sel_entries => {
                Response::json(200, &page_of(&state.sel, &sel_entries, skip))
            }
            ("GET", p) if p == lclog_entries => {
                Response::json(200, &page_of(&state.lclog, &lclog_entries, skip))
            }
            _ => error(404, "The resource was not found."),
        }
    }

    fn system(state: &mut State, reset: &str) -> Value {
        let power = if state.powering_on_reads > 0 && state.power == "On" {
            state.powering_on_reads -= 1;
            "PoweringOn".to_string()
        } else {
            state.power.clone()
        };
        let mut action = json!({ "target": reset });
        if let Some(types) = &state.reset_types {
            action["ResetType@Redfish.AllowableValues"] = json!(types);
        }
        let mut system = json!({
            "Model": "PowerEdge R740", "Manufacturer": "Dell Inc.",
            "Boot": { "BootSourceOverrideTarget@Redfish.AllowableValues": state.boot_targets },
            "Actions": { "#ComputerSystem.Reset": action },
            "LogServices": { "@odata.id": format!("{SYSTEM}/LogServices") },
        });
        if !state.no_power_state {
            system["PowerState"] = json!(power);
        }
        system
    }

    fn reset_system(state: &mut State, kind: &str) -> Response {
        state.calls.push(format!("POST Reset {kind}"));
        match kind {
            "On" | "ForceRestart" => {
                state.power = "On".into();
                let network = std::mem::take(&mut state.network_next);
                if let Some(on_boot) = &state.on_boot {
                    on_boot(network);
                }
            }
            "ForceOff" if state.ignored_force_offs > 0 => state.ignored_force_offs -= 1,
            "ForceOff" | "GracefulShutdown" => state.power = "Off".into(),
            _ => return error(400, "The reset type is not supported."),
        }
        Response::json(200, &Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeBmc, MANAGER, PASSWORD, SYSTEM, USER, entry};
    use super::*;
    use crate::testing::TempDir;
    use std::fs;
    use std::net::TcpListener;
    use std::process::Stdio;

    fn client(bmc: &FakeBmc) -> Redfish {
        Redfish::new(&format!("http://{}", bmc.address), USER, PASSWORD, None).unwrap()
    }

    /// The lines of a config, each a key and its value.
    fn lines(config: &str) -> Vec<&str> {
        config.lines().collect()
    }

    #[test]
    fn a_value_with_a_quote_a_backslash_or_a_line_end_stays_one_word() {
        let word = |text: &str| config_string("x", text).unwrap();
        assert_eq!(word("plain"), "\"plain\"");
        assert_eq!(word("a\"b"), "\"a\\\"b\"");
        assert_eq!(word("a\\b"), "\"a\\\\b\"");
        assert_eq!(word("a\nb\r\tc"), "\"a\\nb\\r\\tc\"");
        assert_eq!(word("#not a comment \" and spaces"), "\"#not a comment \\\" and spaces\"");
        // A backslash before a quote does not turn the quote back into text.
        assert_eq!(word("\\\""), "\"\\\\\\\"\"");
        for control in ['\u{0}', '\u{7}', '\u{1b}', '\u{7f}', '\u{85}'] {
            let err = config_string("password", &format!("a{control}b")).unwrap_err();
            assert!(err.contains("password") && err.contains("control"), "{err}");
            assert!(!err.contains(&format!("a{control}b")), "{err}");
        }
    }

    #[test]
    fn a_password_cannot_add_or_break_a_config_line() {
        let redfish = Redfish::new("http://127.0.0.1:1", "root", "x", None).unwrap();
        let normal = redfish.config("GET", "/redfish/v1/Systems", None).unwrap();
        let hostile = Redfish::new(
            "http://127.0.0.1:1",
            "root",
            "pa\"ss\nurl = \"http://evil\"\n# \\ word",
            None,
        )
        .unwrap();
        let config = hostile.config("GET", "/redfish/v1/Systems", None).unwrap();
        // The same lines, one each, whatever the password holds.
        assert_eq!(lines(&config).len(), lines(&normal).len(), "{config}");
        assert_eq!(lines(&config).iter().filter(|l| l.starts_with("url = ")).count(), 1);
        let user = lines(&config).into_iter().find(|line| line.starts_with("user = ")).unwrap();
        assert_eq!(user, "user = \"root:pa\\\"ss\\nurl = \\\"http://evil\\\"\\n# \\\\ word\"");
        for line in lines(&config) {
            assert!(!line.starts_with('#'), "{line}");
        }
        // Only the sign-in line holds it.
        assert_eq!(config.matches("pa\\\"ss").count(), 1);
        let refused = Redfish::new("http://127.0.0.1:1", "root", "bell\u{7}", None).unwrap();
        let err = refused.get("/redfish/v1/Systems").unwrap_err();
        assert!(err.contains("control character") && !err.contains("bell"), "{err}");
    }

    #[test]
    fn a_request_has_its_method_its_body_and_https_its_pin() {
        let pin = "sha256//47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";
        let redfish = Redfish::new("https://10.77.0.11", "root", "pw", Some(pin)).unwrap();
        let config = redfish
            .config("PATCH", "/redfish/v1/Systems/S", Some("{\"Boot\":{\"A\":\"b\"}}"))
            .unwrap();
        assert!(config.contains("url = \"https://10.77.0.11/redfish/v1/Systems/S\"\n"));
        assert!(config.contains("request = \"PATCH\"\n"));
        assert!(config.contains("header = \"Content-Type: application/json\"\n"));
        assert!(config.contains("data-raw = \"{\\\"Boot\\\":{\\\"A\\\":\\\"b\\\"}}\"\n"));
        assert!(config.contains("write-out = \"\\n%{http_code}\"\n"));
        assert!(config.contains("\ninsecure\n"));
        assert!(config.contains(&format!("pinnedpubkey = \"{pin}\"\n")));
        // A call with no body says no Content-Type and no data.
        let get = redfish.config("GET", "/redfish/v1/", None).unwrap();
        assert!(!get.contains("Content-Type") && !get.contains("data-raw"));
    }

    #[test]
    fn https_needs_the_pin_and_plain_http_is_for_the_loopback_alone() {
        let err = Redfish::new("https://10.77.0.11", "root", "pw", None).err().unwrap();
        assert!(err.contains("pin"), "{err}");
        let err = Redfish::new("https://10.77.0.11", "root", "pw", Some("sha256//short")).err();
        assert!(err.unwrap().contains("redfish.pin"));
        for base in ["http://10.77.0.11", "http://localhost:80", "http://127.0.0.2"] {
            let err = Redfish::new(base, "root", "pw", None).err().unwrap();
            assert!(err.contains("127.0.0.1"), "{err}");
        }
        for base in ["10.77.0.11", "https://", "https://a/b", "http://127.0.0.1@evil"] {
            assert!(
                Redfish::new(base, "root", "pw", Some(&format!("sha256//{}=", "A".repeat(43))))
                    .is_err(),
                "{base}"
            );
        }
        assert!(Redfish::new("http://127.0.0.1:8000", "root", "pw", None).is_ok());
        assert!(is_pin(&format!("sha256//{}=", "A".repeat(43))));
        assert!(!is_pin("sha256//"));
        assert!(!is_pin(&format!("sha1//{}=", "A".repeat(43))));
    }

    #[test]
    fn curl_gets_the_request_on_its_input_and_no_password_in_its_arguments() {
        let dir = TempDir::new("redfish-argv");
        let base = dir.path().display();
        let script = crate::ipmi_sol::fake::script(
            &dir,
            "curl",
            &format!(
                "printf '%s\\n' \"$@\" > '{base}/args'\ncat > '{base}/input'\n\
                 printf '{{\"PowerState\":\"On\"}}\\n200'"
            ),
        );
        let mut redfish = Redfish::new("http://127.0.0.1:9", "root", PASSWORD, None).unwrap();
        redfish.curl = script;
        redfish.lock().system = Some(SYSTEM.into());
        assert_eq!(redfish.power_state(), Ok(true));
        let args = fs::read_to_string(dir.path().join("args")).unwrap();
        let input = fs::read_to_string(dir.path().join("input")).unwrap();
        assert!(!args.contains("Lab") && !args.contains(PASSWORD), "{args}");
        assert!(
            args.lines().any(|line| line == "--config") && args.lines().any(|line| line == "-")
        );
        assert_eq!(args.lines().next(), Some("-q"));
        assert!(input.contains("user = \"root:Lab \\\"Bmc\\\" #1\\\\\""), "{input}");
    }

    #[test]
    fn the_power_state_maps_to_on_or_off() {
        let bmc = FakeBmc::start();
        let redfish = client(&bmc);
        for (said, on) in
            [("On", true), ("PoweringOn", true), ("PoweringOff", true), ("Off", false)]
        {
            bmc.state.lock().unwrap().power = said.into();
            assert_eq!(redfish.power_state(), Ok(on), "{said}");
        }
        bmc.state.lock().unwrap().power = "Paused".into();
        assert!(redfish.power_state().unwrap_err().contains("\"Paused\""));
        bmc.state.lock().unwrap().no_power_state = true;
        assert!(redfish.power_state().unwrap_err().contains("no PowerState"));
    }

    #[test]
    fn a_reset_goes_to_the_target_the_system_names_with_a_type_it_lists() {
        let bmc = FakeBmc::start();
        let redfish = client(&bmc);
        redfish.reset(ResetKind::On).unwrap();
        redfish.reset(ResetKind::ForceOff).unwrap();
        redfish.reset(ResetKind::GracefulShutdown).unwrap();
        redfish.reset(ResetKind::ForceRestart).unwrap();
        assert_eq!(
            bmc.calls(),
            [
                "POST Reset On",
                "POST Reset ForceOff",
                "POST Reset GracefulShutdown",
                "POST Reset ForceRestart"
            ]
        );
        bmc.state.lock().unwrap().reset_types = Some(vec!["On".into(), "ForceOff".into()]);
        let err = redfish.reset(ResetKind::ForceRestart).unwrap_err();
        assert!(err.contains("ForceRestart") && err.contains("On, ForceOff"), "{err}");
        assert_eq!(bmc.calls().len(), 4);
        // A BMC that lists none is asked.
        bmc.state.lock().unwrap().reset_types = None;
        redfish.reset(ResetKind::ForceRestart).unwrap();
        assert_eq!(bmc.calls().len(), 5);
    }

    #[test]
    fn the_boot_override_is_one_time_pxe_with_uefi() {
        let bmc = FakeBmc::start();
        let redfish = client(&bmc);
        assert_eq!(redfish.network_boot_allowed(), Ok(true));
        redfish.boot_from_network_once().unwrap();
        assert_eq!(bmc.calls(), ["PATCH Boot Pxe Once UEFI"]);
        bmc.state.lock().unwrap().boot_targets = vec!["None".into(), "Hdd".into()];
        assert_eq!(redfish.network_boot_allowed(), Ok(false));
    }

    #[test]
    fn a_refusal_says_the_status_and_the_bmc_s_own_message() {
        let bmc = FakeBmc::start();
        let wrong = Redfish::new(&format!("http://{}", bmc.address), USER, "wrong", None).unwrap();
        let err = wrong.power_state().unwrap_err();
        assert!(err.contains("answered 401 to GET /redfish/v1/Systems"), "{err}");
        assert!(err.contains("Unable to authenticate the user."), "{err}");
        assert!(err.contains("redfish.password_file"), "{err}");
        let err = client(&bmc).get("/redfish/v1/Nothing").unwrap_err();
        assert!(err.contains("answered 404") && err.contains("was not found"), "{err}");
        // A BMC's message that repeats the password is hidden.
        assert_eq!(hide("bad value Lab1 for x", "Lab1"), "bad value *** for x");
        assert_eq!(hide("anything", ""), "anything");
        // Not a BMC at all: nothing listens.
        let dead = Redfish::new("http://127.0.0.1:1", USER, PASSWORD, None).unwrap();
        let err = dead.get("/redfish/v1/Systems").unwrap_err();
        assert!(err.contains("did not answer") && !err.contains(PASSWORD), "{err}");
        // A path that is not the BMC's own is never followed.
        let err = client(&bmc).get("http://evil/redfish/v1").unwrap_err();
        assert!(err.contains("not a Redfish path"), "{err}");
    }

    #[test]
    fn the_vendor_is_read_from_the_manufacturer_or_the_oem_names() {
        assert_eq!(vendor_of("Dell Inc.", &[]), Vendor::Dell);
        assert_eq!(vendor_of("HPE", &[]), Vendor::Hpe);
        assert_eq!(vendor_of("Hewlett Packard Enterprise", &[]), Vendor::Hpe);
        assert_eq!(vendor_of("LENOVO", &[]), Vendor::Lenovo);
        assert_eq!(vendor_of("Super Micro Computer", &[]), Vendor::Supermicro);
        assert_eq!(vendor_of("", &["Hpe"]), Vendor::Hpe);
        assert_eq!(vendor_of("Acme", &["Acme"]), Vendor::Other);
        let bmc = FakeBmc::start();
        let identity = client(&bmc).identity().unwrap();
        assert_eq!(identity.vendor, Vendor::Dell);
        assert_eq!(identity.model, "PowerEdge R740");
        assert_eq!(identity.firmware, "4.40.00.00");
        bmc.state.lock().unwrap().manufacturer = "OpenBMC".into();
        // The Oem names still say Dell.
        assert_eq!(client(&bmc).vendor(), Ok(Vendor::Dell));
        bmc.state.lock().unwrap().oem = "OpenBMC".into();
        assert_eq!(client(&bmc).vendor(), Ok(Vendor::Other));
    }

    #[test]
    fn the_event_logs_are_read_over_their_pages() {
        let bmc = FakeBmc::start();
        let events = client(&bmc).event_entries().unwrap();
        // Three entries over two pages under the system, one under the
        // manager.
        assert_eq!(events.len(), 4);
        let sel: Vec<&str> =
            events.iter().filter(|e| e.service.ends_with("/Sel")).map(|e| e.id.as_str()).collect();
        assert_eq!(sel, ["1", "2", "3"]);
        let last = events.iter().find(|e| e.message == "old three").unwrap();
        assert_eq!(last.severity, "Warning");
        assert_eq!(last.line(), "2026-11-20T10:02:00-06:00 Warning SYS1000 old three");
        bmc.state.lock().unwrap().sel.push(entry(4, "2026-11-21T00:00:00-06:00", "OK", "new"));
        assert_eq!(client(&bmc).event_entries().unwrap().len(), 5);
        // An entry with a missing field and a line end in its text is one line.
        let odd = Event {
            service: "/s".into(),
            id: "9".into(),
            created: String::new(),
            severity: "OK".into(),
            message_id: "X".into(),
            message: "a\nb".into(),
        };
        assert_eq!(odd.line(), "- OK X a b");
    }

    #[test]
    fn a_dell_vnc_server_is_turned_on_with_the_password_and_its_port_read_back() {
        let bmc = FakeBmc::start();
        bmc.state.lock().unwrap().vnc_port = 5905;
        let redfish = client(&bmc);
        assert_eq!(redfish.enable_vnc("Ab3dEf7h"), Ok(5905));
        assert_eq!(bmc.calls(), ["PATCH VNC Enabled Disabled 8"]);
        assert_eq!(bmc.state.lock().unwrap().open_password().as_deref(), Some("Ab3dEf7h"));
        // Without a licence the BMC says so, and the password is not in it.
        bmc.state.lock().unwrap().licensed = false;
        let err = redfish.enable_vnc("Ab3dEf7h").unwrap_err();
        assert!(err.contains("licence") && !err.contains("Ab3dEf7h"), "{err}");
        let attributes = redfish.manager_attributes().unwrap();
        assert!(attributes.get("VNCServer.1.Enable").is_none());
        assert_eq!(attributes["IPMILan.1.Enable"], "Enabled");
        assert_eq!(redfish.bios_attributes().unwrap()["BootMode"], "Uefi");
        assert_eq!(MANAGER, "/redfish/v1/Managers/iDRAC.Embedded.1");
    }

    #[test]
    fn a_screen_password_is_eight_letters_and_digits() {
        let password = screen_password().unwrap();
        assert_eq!(password.len(), 8);
        assert!(password.bytes().all(|b| b.is_ascii_alphanumeric()), "{password}");
        assert_ne!(password, screen_password().unwrap());
    }

    /// An `openssl s_server` with a certificate of its own, which answers
    /// `GET /redfish/v1/Systems` from a file, for the tests of the pin.
    struct TlsServer {
        dir: TempDir,
        port: u16,
        child: std::process::Child,
    }

    impl TlsServer {
        fn start() -> TlsServer {
            let dir = TempDir::new("redfish-tls");
            dir.write(
                "www/redfish/v1/Systems",
                "{\"Members\":[{\"@odata.id\":\"/redfish/v1/Systems/S1\"}]}",
            );
            let root = dir.path();
            let made = Command::new("openssl")
                .args(["req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1"])
                .args(["-nodes", "-days", "2", "-subj", "/CN=bmc.lab"])
                .arg("-keyout")
                .arg(root.join("key.pem"))
                .arg("-out")
                .arg(root.join("cert.pem"))
                .output()
                .expect("these tests need openssl");
            assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
            let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
            let child = Command::new("openssl")
                .args(["s_server", "-quiet", "-accept", &format!("127.0.0.1:{port}")])
                .arg("-cert")
                .arg(root.join("cert.pem"))
                .arg("-key")
                .arg(root.join("key.pem"))
                .arg("-WWW")
                .current_dir(root.join("www"))
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let server = TlsServer { dir, port, child };
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while TcpStream::connect(("127.0.0.1", port)).is_err() {
                assert!(std::time::Instant::now() < deadline, "s_server did not listen");
                std::thread::sleep(Duration::from_millis(20));
            }
            server
        }
    }

    impl Drop for TlsServer {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = &self.dir;
        }
    }

    #[test]
    fn the_pin_is_enforced_and_read_from_the_bmc() {
        let server = TlsServer::start();
        let host = format!("127.0.0.1:{}", server.port);
        let pin = pin_of(&host).unwrap();
        assert!(is_pin(&pin), "{pin}");
        let base = format!("https://{host}");
        let good = Redfish::new(&base, USER, PASSWORD, Some(&pin)).unwrap();
        assert_eq!(good.system(), Ok("/redfish/v1/Systems/S1".to_string()));
        // Any other key is refused, though the certificate is not checked.
        let other = format!("sha256//{}=", "A".repeat(43));
        let wrong = Redfish::new(&base, USER, PASSWORD, Some(&other)).unwrap();
        let err = wrong.system().unwrap_err();
        assert!(err.contains("not the pin in the machine file"), "{err}");
        assert!(!err.contains(PASSWORD), "{err}");
        // Nothing listens: the pin cannot be read, and the error says so.
        let gone = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        assert!(pin_of(&gone.to_string()).is_err());
    }
}

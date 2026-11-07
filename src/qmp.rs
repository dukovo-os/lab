// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A client for the QEMU Machine Protocol (QMP), the JSON interface the
//! QEMU driver controls its machines through.
//!
//! QMP sends one JSON object per line: a greeting, then a reply to each
//! command (`{"return": ...}` or `{"error": ...}`), with events
//! (`{"event": ...}`) arriving between them at any time. Commands are
//! sent one at a time and their replies read in order; events seen on
//! the way are kept for [`Qmp::wait_event`]. The protocol is in QEMU's
//! docs/interop/qmp-spec.rst.
//!
//! When QMP misbehaves, the machine's `qemu-<n>.log` in the bundle holds
//! QEMU's own messages.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// How long a command's reply may take. QEMU answers at once unless it is
/// stuck, so a slow reply is a broken machine, not a slow guest.
const REPLY_TIME: Duration = Duration::from_secs(30);

/// A connection to one QEMU's QMP socket.
pub struct Qmp {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    /// Events read while waiting for replies, oldest first.
    events: Vec<Value>,
}

impl Qmp {
    /// Connects to the socket at `path`, waiting up to `wait` for QEMU to
    /// make it, and leaves the greeting's capabilities negotiation done.
    pub fn connect(path: &Path, wait: Duration) -> Result<Qmp, String> {
        let deadline = Instant::now() + wait;
        let stream = loop {
            match UnixStream::connect(path) {
                Ok(stream) => break stream,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(err) => return Err(format!("connecting to QMP at {}: {err}", path.display())),
            }
        };
        stream.set_read_timeout(Some(REPLY_TIME)).map_err(|err| err.to_string())?;
        let reader = BufReader::new(stream.try_clone().map_err(|err| err.to_string())?);
        let mut qmp = Qmp { stream, reader, events: Vec::new() };
        let greeting = qmp.read_message()?;
        if greeting.get("QMP").is_none() {
            return Err(format!("QMP did not greet as QMP does: {greeting}"));
        }
        qmp.execute("qmp_capabilities", &json!({}))?;
        Ok(qmp)
    }

    /// Runs `command` with `arguments` and returns its reply's `return`.
    pub fn execute(&mut self, command: &str, arguments: &Value) -> Result<Value, String> {
        let message = json!({ "execute": command, "arguments": arguments });
        writeln!(self.stream, "{message}")
            .map_err(|err| format!("sending {command} to QMP: {err}"))?;
        loop {
            let mut reply = self.read_message()?;
            if reply.get("event").is_some() {
                self.events.push(reply);
                continue;
            }
            if let Some(result) = reply.get_mut("return") {
                return Ok(result.take());
            }
            let error = reply.get("error").and_then(|error| error.get("desc"));
            return Err(format!("QMP {command}: {}", error.unwrap_or(&reply)));
        }
    }

    /// Waits up to `wait` for an event named `name` (such as
    /// `DEVICE_DELETED`) whose data holds every field of `data`, and
    /// removes it from the events kept.
    pub fn wait_event(
        &mut self,
        name: &str,
        data: &Value,
        wait: Duration,
    ) -> Result<Value, String> {
        let deadline = Instant::now() + wait;
        loop {
            if let Some(at) = self.events.iter().position(|event| event_matches(event, name, data))
            {
                return Ok(self.events.remove(at));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("QEMU sent no {name} event within {}s", wait.as_secs()));
            }
            self.stream.set_read_timeout(Some(left)).map_err(|err| err.to_string())?;
            let read = self.read_message();
            self.stream.set_read_timeout(Some(REPLY_TIME)).map_err(|err| err.to_string())?;
            match read {
                Ok(message) => self.events.push(message),
                // A timeout ends the wait on the next turn round the loop.
                Err(_) if Instant::now() >= deadline => {}
                Err(err) => return Err(err),
            }
        }
    }

    fn read_message(&mut self) -> Result<Value, String> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => Err("QEMU closed its QMP connection".into()),
            Ok(_) => serde_json::from_str(&line).map_err(|err| {
                format!("QMP sent a line that is not JSON ({err}): {}", line.trim())
            }),
            Err(err) => Err(format!("reading from QMP: {err}")),
        }
    }
}

/// Whether `event` is named `name` and its data holds every field of `data`.
fn event_matches(event: &Value, name: &str, data: &Value) -> bool {
    if event.get("event").and_then(Value::as_str) != Some(name) {
        return false;
    }
    let Some(wanted) = data.as_object() else { return true };
    wanted.iter().all(|(key, value)| event.get("data").and_then(|d| d.get(key)) == Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;
    use std::os::unix::net::UnixListener;

    /// A pretend QEMU: greets, then answers each command with `replies`
    /// in turn, an event before the second.
    fn fake_qemu(
        listener: UnixListener,
        replies: Vec<&'static str>,
    ) -> std::thread::JoinHandle<Vec<String>> {
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            writeln!(stream, r#"{{"QMP": {{"version": {{}}, "capabilities": []}}}}"#).unwrap();
            let mut received = Vec::new();
            for (index, reply) in replies.into_iter().enumerate() {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                received.push(line.trim().to_string());
                if index == 1 {
                    writeln!(
                        stream,
                        r#"{{"event": "DEVICE_DELETED", "data": {{"device": "stick1"}}}}"#
                    )
                    .unwrap();
                }
                writeln!(stream, "{reply}").unwrap();
            }
            received
        })
    }

    #[test]
    fn sends_commands_and_keeps_events() {
        let dir = TempDir::new("qmp");
        let socket = dir.path().join("qmp.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let qemu = fake_qemu(
            listener,
            vec![
                r#"{"return": {}}"#,
                r#"{"return": {"running": true}}"#,
                r#"{"error": {"class": "GenericError", "desc": "no such device"}}"#,
            ],
        );
        let mut qmp = Qmp::connect(&socket, Duration::from_secs(5)).unwrap();
        let status = qmp.execute("query-status", &json!({})).unwrap();
        assert_eq!(status["running"], true);
        let error = qmp.execute("device_del", &json!({"id": "x"})).unwrap_err();
        assert_eq!(error, "QMP device_del: \"no such device\"");
        let event = qmp
            .wait_event("DEVICE_DELETED", &json!({"device": "stick1"}), Duration::from_secs(1))
            .unwrap();
        assert_eq!(event["data"]["device"], "stick1");
        let sent = qemu.join().unwrap();
        assert_eq!(sent[0], r#"{"arguments":{},"execute":"qmp_capabilities"}"#);
        assert_eq!(sent[2], r#"{"arguments":{"id":"x"},"execute":"device_del"}"#);
    }
}

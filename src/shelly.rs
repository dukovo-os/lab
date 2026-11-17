// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A Shelly smart plug (Gen 2 and later), through its RPC over plain HTTP:
//! the power switch of a real machine that has no AMT, and the watts the
//! plug measures, which say whether the machine runs.
//!
//! The plug is on the home network, not the lab one, and the lab PC
//! reaches it through its ordinary card. `Switch.GetStatus` and
//! `Switch.Set` are the plug's own methods, called as `GET /rpc/<method>`.
//! Wi-Fi drops a packet now and then, so a call that gets no answer is
//! tried three times. A plug that asks for a password is refused for now:
//! its HTTP sign-in is a digest the lab does not speak yet.
//!
//! When a machine's power does not change, `plug.log` in the run's bundle
//! names each call and the plug's answer to it (see driver/plug.rs).

use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::http;

/// The port a plug's web server listens on, unless the machine file's
/// address gives another.
const PORT: u16 = 80;

/// How long connecting to the plug, and each read of its answer, may take.
const TIME: Duration = Duration::from_secs(5);

/// How many times a call that gets no answer is tried.
const TRIES: u32 = 3;

/// How long a call waits before it is tried again.
const RETRY_GAP: Duration =
    if cfg!(test) { Duration::from_millis(20) } else { Duration::from_secs(1) };

/// A plug on the home network.
pub struct Shelly {
    /// `host:port`.
    address: String,
}

/// What the plug's switch is doing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlugStatus {
    /// Whether the switch is closed: the machine has power.
    pub output: bool,
    /// What the machine draws now, in watts.
    pub watts: f64,
}

/// What `Switch.GetStatus` answers, less what the lab does not read.
#[derive(Deserialize)]
struct Status {
    output: bool,
    apower: f64,
}

/// What `Switch.Set` answers: whether the switch was closed before.
#[derive(Deserialize)]
struct Set {
    was_on: bool,
}

impl Shelly {
    /// The plug at `address`: a host, or `host:port` for a plug that
    /// listens on another port than 80.
    pub fn new(address: &str) -> Shelly {
        let address =
            if address.contains(':') { address.to_string() } else { format!("{address}:{PORT}") };
        Shelly { address }
    }

    /// Reads the switch and the power the plug measures.
    ///
    /// # Errors
    ///
    /// Fails when the plug does not answer, or answers something that is
    /// not a Gen 2 plug's status. The message names the plug's address.
    pub fn status(&self) -> Result<PlugStatus, String> {
        let method = "Switch.GetStatus?id=0";
        let body = self.call(method)?;
        let status: Status = self.parse(method, &body)?;
        Ok(PlugStatus { output: status.output, watts: status.apower })
    }

    /// Closes the switch (`on`) or opens it, and says whether it was
    /// closed before.
    ///
    /// # Errors
    ///
    /// As [`Shelly::status`].
    pub fn set(&self, on: bool) -> Result<bool, String> {
        let method = format!("Switch.Set?id=0&on={on}");
        let body = self.call(&method)?;
        self.parse::<Set>(&method, &body).map(|set| set.was_on)
    }

    /// Calls `method` (with its query), trying again while the plug does
    /// not answer at all. An answer that refuses the call is final.
    fn call(&self, method: &str) -> Result<Vec<u8>, String> {
        let mut failure = String::new();
        for attempt in 1..=TRIES {
            if attempt > 1 {
                std::thread::sleep(RETRY_GAP);
            }
            let path = format!("/rpc/{method}");
            match http::send(&self.address, "GET", &path, &[], &[], TIME, Duration::ZERO) {
                Ok(answer) if answer.status == 200 => return Ok(answer.body),
                Ok(answer) => return Err(self.refused(method, answer.status, &answer.body)),
                Err(err) => failure = err,
            }
        }
        Err(format!(
            "the Shelly plug at {} did not answer {method} in {TRIES} tries: {failure}",
            self.address
        ))
    }

    /// The message for a plug that answered with an error status.
    fn refused(&self, method: &str, status: u16, body: &[u8]) -> String {
        let why = String::from_utf8_lossy(body);
        let hint = if status == 401 {
            " (the plug asks for a password, which the lab cannot give yet: turn the plug's \
             authentication off in its web page)"
        } else {
            ""
        };
        format!(
            "the Shelly plug at {} refused {method} with status {status}{hint}: {}",
            self.address,
            why.trim()
        )
    }

    /// Reads the JSON answer to `method`.
    fn parse<T: DeserializeOwned>(&self, method: &str, body: &[u8]) -> Result<T, String> {
        serde_json::from_slice(body).map_err(|err| {
            format!(
                "the Shelly plug at {} answered {method} with something a Gen 2 plug does not \
                 say ({err}): is {} a Shelly plug with power metering?",
                self.address, self.address
            )
        })
    }
}

#[cfg(test)]
pub mod fake {
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use serde_json::json;

    use crate::http::{self, Response};

    /// The pretend plug's state.
    pub struct State {
        pub output: bool,
        /// What the machine behind the plug draws while the switch is on.
        pub draws: f64,
        /// Each call, as `Set on=true` or `GetStatus`.
        pub calls: Vec<String>,
        /// How many connections are hung up on before one is answered.
        pub hang_ups: u32,
        /// A status to refuse every call with, and the text it comes with.
        pub refusal: Option<(u16, &'static str)>,
        /// Answers every call with a web page, as a router's would.
        pub garbled: bool,
        /// Called when the switch is closed: the pretend machine starts.
        pub on_switch_on: Option<Box<dyn Fn() + Send>>,
    }

    /// A pretend plug, listening until dropped.
    pub struct FakeShelly {
        pub address: SocketAddr,
        pub state: Arc<Mutex<State>>,
        done: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl FakeShelly {
        /// A plug whose switch is open, with a machine that draws 33 W.
        pub fn start() -> FakeShelly {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let state = Arc::new(Mutex::new(State {
                output: false,
                draws: 33.0,
                calls: Vec::new(),
                hang_ups: 0,
                refusal: None,
                garbled: false,
                on_switch_on: None,
            }));
            let done = Arc::new(AtomicBool::new(false));
            let thread = {
                let (state, done) = (Arc::clone(&state), Arc::clone(&done));
                std::thread::spawn(move || {
                    while !done.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((stream, _)) => answer(&stream, &state),
                            Err(_) => std::thread::sleep(Duration::from_millis(5)),
                        }
                    }
                })
            };
            FakeShelly { address, state, done, thread: Some(thread) }
        }

        pub fn calls(&self) -> Vec<String> {
            self.state.lock().unwrap().calls.clone()
        }
    }

    impl Drop for FakeShelly {
        fn drop(&mut self) {
            self.done.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn answer(stream: &TcpStream, state: &Mutex<State>) {
        let _ = stream.set_nonblocking(false);
        let Ok(request) = http::read_request(stream) else { return };
        let mut state = state.lock().unwrap();
        if state.hang_ups > 0 {
            state.hang_ups -= 1;
            return;
        }
        if let Some((status, text)) = state.refusal {
            http::write_response(stream, &Response::text(status, text));
            return;
        }
        if state.garbled {
            http::write_response(stream, &Response::text(200, "<html>a router</html>"));
            return;
        }
        let reply = match request.path.as_str() {
            "/rpc/Switch.GetStatus" => {
                state.calls.push("GetStatus".into());
                let watts = if state.output { state.draws } else { 0.0 };
                json!({ "id": 0, "source": "HTTP", "output": state.output, "apower": watts })
            }
            "/rpc/Switch.Set" => {
                let on = request.query("on") == Some("true");
                state.calls.push(format!("Set on={on}"));
                let was_on = std::mem::replace(&mut state.output, on);
                if on
                    && !was_on
                    && let Some(start) = &state.on_switch_on
                {
                    start();
                }
                json!({ "was_on": was_on })
            }
            _ => {
                http::write_response(stream, &Response::text(404, "No handler for that"));
                return;
            }
        };
        http::write_response(stream, &Response::json(200, &reply));
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeShelly;
    use super::*;

    fn plug(fake: &FakeShelly) -> Shelly {
        Shelly::new(&fake.address.to_string())
    }

    #[test]
    fn reads_the_switch_and_the_watts() {
        let fake = FakeShelly::start();
        let plug = plug(&fake);
        assert_eq!(plug.status().unwrap(), PlugStatus { output: false, watts: 0.0 });
        fake.state.lock().unwrap().output = true;
        assert_eq!(plug.status().unwrap(), PlugStatus { output: true, watts: 33.0 });
    }

    #[test]
    fn sets_the_switch() {
        let fake = FakeShelly::start();
        let plug = plug(&fake);
        assert!(!plug.set(true).unwrap());
        assert!(fake.state.lock().unwrap().output);
        assert!(plug.set(false).unwrap());
        assert!(!fake.state.lock().unwrap().output);
        assert_eq!(fake.calls(), ["Set on=true", "Set on=false"]);
    }

    #[test]
    fn a_call_that_gets_no_answer_is_tried_again() {
        let fake = FakeShelly::start();
        fake.state.lock().unwrap().hang_ups = 2;
        assert!(!plug(&fake).status().unwrap().output);
        // A plug that stays silent is given up on, naming its address.
        fake.state.lock().unwrap().hang_ups = 3;
        let err = plug(&fake).status().unwrap_err();
        assert!(err.contains(&fake.address.to_string()), "{err}");
        assert!(err.contains("did not answer Switch.GetStatus?id=0 in 3 tries"), "{err}");
    }

    #[test]
    fn a_refusal_is_not_tried_again_and_a_password_is_named() {
        let fake = FakeShelly::start();
        fake.state.lock().unwrap().refusal = Some((401, "Unauthorized"));
        let err = plug(&fake).set(true).unwrap_err();
        assert!(err.contains("refused Switch.Set?id=0&on=true with status 401"), "{err}");
        assert!(err.contains("authentication off"), "{err}");
        fake.state.lock().unwrap().refusal = Some((500, "busy"));
        let err = plug(&fake).status().unwrap_err();
        assert!(err.contains("status 500: busy"), "{err}");
    }

    #[test]
    fn an_answer_that_is_not_a_plugs_says_so() {
        let fake = FakeShelly::start();
        fake.state.lock().unwrap().garbled = true;
        let err = plug(&fake).status().unwrap_err();
        assert!(err.contains("with something a Gen 2 plug does not say"), "{err}");
        let err = plug(&fake).set(false).unwrap_err();
        assert!(err.contains("Switch.Set"), "{err}");
    }

    #[test]
    fn an_address_without_a_port_gets_the_web_port() {
        assert_eq!(Shelly::new("192.168.1.3").address, "192.168.1.3:80");
        assert_eq!(Shelly::new("192.168.1.3:8080").address, "192.168.1.3:8080");
    }
}

// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A real machine's screen, keyboard and mouse through its management
//! controller's VNC server, which amt.rs (Intel AMT's remote screen) and
//! redfish.rs (a Dell iDRAC's virtual console) share.
//!
//! The driver gives a [`RemoteScreen`] the address of the VNC server and
//! a way to turn the server on, which sets a password made for the run and
//! gives it back. The first look at the screen turns the server on and
//! opens a session; the session stays open, so a controller that plugs
//! its keyboard and mouse into the machine when a session opens keeps them
//! there across boots. A session that fails is closed, and the next
//! action opens another. Each step of a session goes to the driver's log
//! as a line beginning `remote screen:`, never with the password.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use super::pixel;
use super::real::DriverLog;
use crate::suite::Button;
use crate::vnc::{self, Vnc};
use crate::{png, screen};

/// How long the screen may take to open a session: the controller may
/// still be closing the one before.
const OPEN_TIME: Duration = Duration::from_secs(30);

/// How long the keyboard and mouse a controller plugs in when a session
/// opens are given to appear to the system before they are used.
const SETTLE: Duration =
    if cfg!(test) { Duration::from_millis(100) } else { Duration::from_secs(2) };

/// How long after a session's first key, a Shift tap, the keys typed
/// wait. On the lab's OptiPlex 7040 the first key of each session was
/// held about a second (repeating some 24 times), whatever came after
/// it: a Shift held that long types nothing.
const FIRST_KEY_SETTLE: Duration =
    if cfg!(test) { Duration::from_millis(10) } else { Duration::from_millis(1500) };

/// How long the whole screen may take to arrive: a few megabytes of raw
/// pixels.
const CAPTURE_TIME: Duration = Duration::from_secs(90);

/// How long a key is held, and the pause after each combination.
const KEY_HOLD: Duration = Duration::from_millis(50);
const KEY_GAP: Duration = Duration::from_millis(100);
/// The left Shift's X keysym.
const SHIFT: u32 = 0xffe1;

/// What turning the controller's VNC server on gave.
pub struct Enabled {
    /// The password the server takes, made for this run.
    pub password: String,
    /// The server's address as `host:port`, when turning it on showed it
    /// is not where the screen was expected.
    pub address: Option<String>,
}

/// Turns the controller's VNC server on with a password made for the
/// run.
pub type TurnOn = Box<dyn Fn() -> Result<Enabled, String> + Send>;

/// A machine's screen, keyboard and mouse over VNC.
pub struct RemoteScreen {
    /// The VNC server's `host:port`.
    pub address: String,
    /// What is turned on, for the log: "port 5900".
    what: String,
    /// Whether a session's first key is a Shift tap, followed by a wait
    /// (see [`FIRST_KEY_SETTLE`]).
    prime_first_key: bool,
    notes: DriverLog,
    turn_on: TurnOn,
    /// The password set for this run, once the server is turned on.
    password: Option<String>,
    /// The session and when it opened.
    session: Option<(Vnc, Instant)>,
    /// Whether a key has gone through this session yet.
    primed: bool,
    /// Where the pointer was put last.
    pointer: (u16, u16),
}

/// A session's first input: a Shift tap, then [`FIRST_KEY_SETTLE`].
fn prime(vnc: &mut Vnc) -> Result<(), String> {
    vnc.key(SHIFT, true)?;
    std::thread::sleep(KEY_HOLD);
    vnc.key(SHIFT, false)?;
    std::thread::sleep(FIRST_KEY_SETTLE);
    Ok(())
}

impl RemoteScreen {
    /// A screen at `address` that `turn_on` turns on the first time it is
    /// wanted. `what` names the thing turned on in the log, and
    /// `prime_first_key` says whether a session's first key is a Shift
    /// tap. Nothing is opened yet.
    pub fn new(
        address: String,
        what: &str,
        prime_first_key: bool,
        notes: DriverLog,
        turn_on: TurnOn,
    ) -> RemoteScreen {
        RemoteScreen {
            address,
            what: what.into(),
            prime_first_key,
            notes,
            turn_on,
            password: None,
            session: None,
            primed: false,
            pointer: (0, 0),
        }
    }

    /// Closes the session, if one is open, and says whether one was.
    pub fn close(&mut self) -> bool {
        self.session.take().is_some()
    }

    /// Opens a session unless one is open, turning the server on first
    /// the first time.
    fn open(&mut self) -> Result<(), String> {
        if let Some((vnc, _)) = &mut self.session
            && vnc.is_open()
        {
            return Ok(());
        }
        self.session = None;
        let password = match &self.password {
            Some(password) => password.clone(),
            None => {
                self.notes.note(&format!(
                    "remote screen: turning on {}, with a password made for this run",
                    self.what
                ));
                let enabled = (self.turn_on)().inspect_err(|err| {
                    self.notes.note(&format!("remote screen: {err}"));
                })?;
                if let Some(address) = enabled.address {
                    self.address = address;
                }
                self.password = Some(enabled.password.clone());
                enabled.password
            }
        };
        let deadline = Instant::now() + OPEN_TIME;
        match Vnc::connect(&self.address, &password, deadline) {
            Ok(vnc) => {
                let (width, height) = vnc.size();
                self.notes.note(&format!(
                    "remote screen: session open, {width}x{height} \"{}\"",
                    vnc.name
                ));
                self.session = Some((vnc, Instant::now()));
                self.primed = false;
                Ok(())
            }
            Err(err) => {
                self.notes.note(&format!("remote screen: {err}"));
                Err(err)
            }
        }
    }

    /// Does `act` in the session, opening one if need be. Keys and the
    /// pointer (`input`) wait until the session's keyboard and mouse have
    /// had time to appear. A session that fails is closed, and the next
    /// action opens another.
    fn with_session<T>(
        &mut self,
        input: bool,
        act: impl FnOnce(&mut Vnc) -> Result<T, String>,
    ) -> Result<T, String> {
        self.open()?;
        let Some((vnc, opened)) = self.session.as_mut() else {
            return Err("the remote screen has no session".into());
        };
        if input && let Some(wait) = SETTLE.checked_sub(opened.elapsed()) {
            std::thread::sleep(wait);
        }
        let mut done = Ok(());
        if input && self.prime_first_key && !self.primed {
            done = prime(vnc);
            self.primed = done.is_ok();
        }
        let done = done.and_then(|()| act(vnc));
        if let Err(err) = &done {
            self.session = None;
            self.notes.note(&format!("remote screen: {err}"));
        }
        done
    }

    /// The whole screen now, with one more try in a new session if the
    /// one open failed (a controller may end a session when the machine
    /// restarts).
    fn capture(&mut self) -> Result<screen::Image, String> {
        let take = |vnc: &mut Vnc| vnc.capture(Instant::now() + CAPTURE_TIME);
        self.with_session(false, take).or_else(|_| self.with_session(false, take))
    }

    /// Presses each key combination in turn.
    ///
    /// # Errors
    ///
    /// Fails, before any key goes down, when a key has no name the lab
    /// knows, and when the session fails.
    pub fn press_keys(&mut self, combos: &[String]) -> Result<(), String> {
        let combos =
            combos.iter().map(|combo| vnc::keysyms(combo)).collect::<Result<Vec<_>, _>>()?;
        self.with_session(true, |vnc| {
            for keys in &combos {
                for &key in keys {
                    vnc.key(key, true)?;
                }
                std::thread::sleep(KEY_HOLD);
                for &key in keys.iter().rev() {
                    vnc.key(key, false)?;
                }
                std::thread::sleep(KEY_GAP);
            }
            Ok(())
        })
    }

    /// Moves the pointer to pixel (`x`, `y`), each found by [`pixel`].
    ///
    /// # Errors
    ///
    /// Fails when the pixel is off the screen, and when the session fails.
    pub fn point(&mut self, x: i32, y: i32) -> Result<(), String> {
        let (mut width, mut height) = self.with_session(false, |vnc| Ok(vnc.size()))?;
        let known = pixel(x, width).zip(pixel(y, height));
        if x < 0 || y < 0 || known.is_none() {
            // The size known may be from before the screen last changed,
            // and a pixel counted from the far edge depends on it.
            let now = self.capture()?;
            (width, height) = (now.width as u32, now.height as u32);
        }
        let off = || format!("({x}, {y}) is off the screen, which is {width}x{height} pixels");
        let (Some(at_x), Some(at_y)) = (pixel(x, width), pixel(y, height)) else {
            return Err(off());
        };
        let (Ok(x), Ok(y)) = (u16::try_from(at_x), u16::try_from(at_y)) else {
            return Err(off());
        };
        self.with_session(true, |vnc| vnc.pointer(0, x, y))?;
        self.pointer = (x, y);
        Ok(())
    }

    /// Presses `button` where the pointer is and lets it go.
    ///
    /// # Errors
    ///
    /// Fails when the session fails.
    pub fn click(&mut self, button: Button) -> Result<(), String> {
        let held = match button {
            Button::Left => 1,
            Button::Middle => 2,
            Button::Right => 4,
        };
        let (x, y) = self.pointer;
        self.with_session(true, |vnc| {
            for buttons in [held, 0] {
                vnc.pointer(buttons, x, y)?;
                // As long as a person holds a button.
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(())
        })
    }

    /// Saves the screen as a PNG file at `to`.
    ///
    /// # Errors
    ///
    /// Fails when the screen cannot be read or `to` cannot be written.
    pub fn screenshot(&mut self, to: &Path) -> Result<(), String> {
        let image = self.capture()?;
        fs::write(to, png::encode(&image)).map_err(|err| format!("{}: {err}", to.display()))
    }

    /// Saves the screen as a binary PPM file at `to`.
    ///
    /// # Errors
    ///
    /// Fails when the screen cannot be read or `to` cannot be written.
    pub fn screen_dump(&mut self, to: &Path) -> Result<(), String> {
        let image = self.capture()?;
        fs::write(to, image.to_ppm()).map_err(|err| format!("{}: {err}", to.display()))
    }
}

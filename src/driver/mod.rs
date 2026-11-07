// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Machine drivers: the one interface every kind of machine offers.
//!
//! The runner drives every machine through [`Driver`], so a test written
//! against QEMU runs unchanged on a real machine once that machine has a
//! driver (AMT, in PR 9). Each method is one [`Control`] of a machine
//! file; the runner calls only those the machine file says the driver
//! does.
//!
//! [`Control`]: crate::machines::Control

pub mod qemu;

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::bench::Kit;
use crate::machines::{DriverKind, Machine};
use crate::ssh;
use crate::stop::Stop;
use crate::suite::{Button, PowerAction, Stick};

/// The machine under test's address on the private lab network an attack
/// runs over. An `attack` step's `{target}` becomes this.
pub const TARGET_ON_SEGMENT: &str = "10.9.9.1";

/// The attacker's address on that same network.
pub const ATTACKER_ON_SEGMENT: &str = "10.9.9.2";

/// A machine the runner is driving.
pub trait Driver: Send {
    /// Turns the machine off if it is on, then boots it from a clean disk
    /// holding the image under test, with a new serial log. Returns once
    /// it is powering on; the runner waits for it to be ready.
    fn boot_fresh(&mut self) -> Result<(), String>;

    /// Turns the machine off if it is on, then boots it from the install
    /// stick `stick` (an image the driver never writes to) with a blank
    /// disk of `disk` bytes, for the installer, and a new serial log. The
    /// stick stays in until [`Driver::unplug_usb`] pulls it.
    fn boot_stick(&mut self, stick: &Path, disk: u64) -> Result<(), String>;

    /// Does a power action. [`PowerAction::Off`] returns once the machine
    /// is off, or fails at `deadline` or when the run is stopped.
    fn power(&mut self, action: PowerAction, deadline: Instant) -> Result<(), String>;

    /// Whether the machine is on.
    fn is_on(&mut self) -> bool;

    /// The current boot's serial log, which grows while the machine runs.
    fn serial_log(&self) -> &Path;

    /// Plugs the network cable in or pulls it.
    fn set_link(&mut self, up: bool) -> Result<(), String>;

    /// Plugs in a USB stick: a copy of an image, or a blank one.
    fn plug_usb(&mut self, stick: &Stick) -> Result<(), String>;

    /// Pulls the USB stick out.
    fn unplug_usb(&mut self) -> Result<(), String>;

    /// Presses each key combination in turn.
    fn press_keys(&mut self, combos: &[String]) -> Result<(), String>;

    /// Moves the pointer to pixel (`x`, `y`) of the screen.
    fn point(&mut self, x: u32, y: u32) -> Result<(), String>;

    /// Presses `button` and lets it go.
    fn click(&mut self, button: Button) -> Result<(), String>;

    /// Saves the screen as a PNG file at `to`.
    fn screenshot(&mut self, to: &Path) -> Result<(), String>;

    /// Saves the screen as a binary PPM file at `to`, for reading its text.
    fn screen_dump(&mut self, to: &Path) -> Result<(), String>;

    /// Runs `command` on the machine as the lab account, with `input` as
    /// its standard input (none: empty), stopping it at `deadline`, or
    /// sooner when the run is stopped (the output's status is then none).
    /// Fails when there is no way to run it: the machine is off, or the
    /// lab has no key.
    fn run_command(
        &mut self,
        command: &str,
        input: Option<&str>,
        deadline: Instant,
    ) -> Result<ssh::Output, String>;

    /// Says whether the next [`Driver::boot_fresh`] gives the machine a
    /// second network port on a private network of its own, for an
    /// attacker to reach it on. The runner calls it before booting a test
    /// that attacks, and with `false` before one that does not, so the
    /// port is there only when it is wanted (an idle one would show up in
    /// a network test). A driver with no attacker ignores it.
    fn want_attacker(&mut self, _yes: bool) {}

    /// Brings the attacker up on the private network, if it is not up
    /// already, so [`Driver::attack`] can run commands on it. Fails when
    /// this machine has no attacker, or it does not come up by `deadline`.
    fn start_attacker(&mut self, _deadline: Instant) -> Result<(), String> {
        Err("this machine has no attacker".into())
    }

    /// Runs `command` on the attacker, with `input` as its standard input,
    /// and gives back what it printed. As [`Driver::run_command`], but on
    /// the attacker rather than the machine under test.
    fn attack(
        &mut self,
        _command: &str,
        _input: Option<&str>,
        _deadline: Instant,
    ) -> Result<ssh::Output, String> {
        Err("this machine has no attacker".into())
    }

    /// How many times longer than on real hardware tests may take here.
    fn slowdown(&self) -> u32;

    /// Boots Linux from `kit` on a machine set up as this one is, and
    /// returns what it printed on its serial console, which is also kept
    /// in `log`, once it printed the kit's done line. The machine under
    /// test does nothing meanwhile, so the two never share the CPUs. Fails
    /// when Linux ends without the done line, at `deadline`, or when the
    /// run is stopped.
    fn linux_twin(&mut self, kit: &Kit, log: &Path, deadline: Instant) -> Result<String, String>;

    /// Whether numbers measured here measure an emulator rather than the
    /// machine: QEMU without KVM. Performance tests are not held to
    /// Linux's there.
    fn emulated(&self) -> bool;

    /// Turns the machine off at once and lets go of what it holds. The
    /// runner calls it when the run ends, also on failure.
    fn shut_off(&mut self);
}

/// What a driver is given to work with.
pub struct Setup {
    /// The image under test. Drivers never write to it.
    pub image: PathBuf,
    /// The bundle's `logs/<machine>/`, for serial and driver logs.
    pub logs: PathBuf,
    /// A folder for disks and other large files the bundle does not keep.
    pub work: PathBuf,
    /// A short folder for Unix sockets, whose paths may not pass 107 bytes.
    pub sockets: PathBuf,
    /// The lab key, for machines with an SSH channel.
    pub key: Option<PathBuf>,
    /// Set when the run is stopped: long waits end early.
    pub stop: Stop,
}

/// Starts the driver for `machine`. The machine stays off until
/// [`Driver::boot_fresh`].
pub fn start(machine: &Machine, setup: Setup) -> Result<Box<dyn Driver>, String> {
    match machine.driver {
        DriverKind::Qemu => {
            let settings = machine.qemu.clone().unwrap_or_default();
            Ok(Box::new(qemu::Qemu::new(settings, setup)?))
        }
    }
}

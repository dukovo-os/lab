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
use crate::suite::{Button, Floppy, PowerAction, Reader, Stick};

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

    /// As [`Driver::boot_stick`], from the install DVD `dvd` (an ISO the
    /// driver never writes to) in the machine's optical drive, which the
    /// runner asked for with [`Driver::want_disc_drive`]. The disc stays
    /// in until [`Driver::eject_disc`] takes it out.
    fn boot_dvd(&mut self, _dvd: &Path, _disk: u64) -> Result<(), String> {
        Err("this machine's driver cannot start it from a disc".into())
    }

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

    /// Says whether the next [`Driver::boot_fresh`] gives the machine an
    /// empty card reader, and which kind, which [`Driver::insert_card`]
    /// puts cards in. The runner calls it before booting a test with card
    /// or sd steps, and with `None` before one without, so that the
    /// reader, an empty drive, is there only when it is wanted (it would
    /// show up in a disk test). A driver whose machines have their own
    /// reader ignores it.
    fn want_card_reader(&mut self, _reader: Option<Reader>) {}

    /// Puts a memory card in the card reader: a copy of an image, or a
    /// blank one.
    fn insert_card(&mut self, _card: &Stick) -> Result<(), String> {
        Err("this machine's driver cannot put a card in its reader".into())
    }

    /// Takes the memory card out of the card reader, by force: whether or
    /// not the machine let it go.
    fn remove_card(&mut self) -> Result<(), String> {
        Err("this machine's driver cannot take a card out of its reader".into())
    }

    /// Puts the memory card taken out last back in the card reader,
    /// holding what the machine wrote on it.
    fn reinsert_card(&mut self) -> Result<(), String> {
        Err("this machine's driver cannot put a card in its reader".into())
    }

    /// Says whether the next [`Driver::boot_fresh`] gives the machine an
    /// empty optical drive, which [`Driver::insert_disc`] puts discs in,
    /// as [`Driver::want_card_reader`] says of a card reader.
    fn want_disc_drive(&mut self, _yes: bool) {}

    /// Puts a disc holding `image` in the optical drive.
    fn insert_disc(&mut self, _image: &Path) -> Result<(), String> {
        Err("this machine's driver cannot put a disc in its drive".into())
    }

    /// Takes the disc out of the optical drive, by force: whether or not
    /// the machine let it go.
    fn eject_disc(&mut self) -> Result<(), String> {
        Err("this machine's driver cannot take a disc out of its drive".into())
    }

    /// Puts the disc taken out last back in the optical drive.
    fn reinsert_disc(&mut self) -> Result<(), String> {
        Err("this machine's driver cannot put a disc in its drive".into())
    }

    /// Presses the optical drive's eject button: a drive whose disc the
    /// machine locked in keeps it and asks the machine to let it go; one
    /// the machine did not lock opens its tray, and the disc is taken out.
    fn press_disc_button(&mut self) -> Result<(), String> {
        Err("this machine's driver cannot press its drive's eject button".into())
    }

    /// Says whether the next [`Driver::boot_fresh`] gives the machine an
    /// empty floppy drive, which [`Driver::insert_floppy`] puts disks in,
    /// as [`Driver::want_card_reader`] says of a card reader.
    fn want_floppy_drive(&mut self, _yes: bool) {}

    /// Says whether the next [`Driver::boot_fresh`] gives the machine a
    /// blank CompactFlash card of this many bytes in an IDE adapter, as
    /// [`Driver::want_card_reader`] says of a card reader. A real
    /// machine's card is in its adapter already.
    fn want_cf_card(&mut self, _bytes: Option<u64>) {}

    /// Puts a floppy disk in the floppy drive: a copy of an image, or a
    /// blank one, write-protected or not.
    fn insert_floppy(&mut self, _floppy: &Floppy) -> Result<(), String> {
        Err("this machine's driver cannot put a floppy disk in its drive".into())
    }

    /// Takes the floppy disk out of the floppy drive, by force: whether or
    /// not the machine let it go.
    fn eject_floppy(&mut self) -> Result<(), String> {
        Err("this machine's driver cannot take a floppy disk out of its drive".into())
    }

    /// Puts the floppy disk taken out last back in the floppy drive,
    /// holding what the machine wrote on it.
    fn reinsert_floppy(&mut self) -> Result<(), String> {
        Err("this machine's driver cannot put a floppy disk in its drive".into())
    }

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

    /// The lab PC's address as the machine sees it, which a test's `run`
    /// steps reach the lab PC's servers at (`{peer}`); none when the lab
    /// PC is not on the machine's network.
    fn peer_address(&self) -> Option<String> {
        None
    }

    /// The machine's address as the lab PC reaches it (`{machine}`).
    fn machine_address(&self) -> Option<String> {
        None
    }

    /// The lab PC's port that reaches the machine's `port` (UDP when
    /// `udp`, else TCP) at [`Driver::machine_address`], for the lab PC's
    /// clients to reach a server of the machine's (`{machine-port:N}`).
    /// A driver that forwards a port of the lab PC's does so now, for the
    /// rest of this boot; one whose machine is on the lab PC's network
    /// gives `port` itself.
    fn reach(&mut self, _port: u16, _udp: bool) -> Result<u16, String> {
        Err("the lab PC cannot reach this machine's ports".into())
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
    /// Dukovo's ob, whose `ob disc-drive` is a QEMU machine's drive of
    /// music CDs, when the run was given it.
    pub ob: Option<PathBuf>,
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

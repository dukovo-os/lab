// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Machine files: one TOML file per machine the lab can run tests on.
//!
//! They live in `machines/` under the configuration folder
//! (`~/.config/dukovo-lab/machines/` unless `--config` says otherwise), and
//! the file's name is the machine's name. A built-in machine named `qemu`
//! needs no file. docs/machine-files.md describes the format.
//!
//! Each machine says, for every [`Control`], whether its driver does it,
//! a person must do it, or it cannot be done there at all. That one table
//! decides which tests run on a machine, which wait for a person (the
//! runner's Needs you group) and which are skipped.
//!
//! A machine file may name a pool. The machines of a pool are
//! interchangeable, so a run on the pool (`pool:<name>`) shares its tests
//! out among them instead of running every test on each. [`load`] holds
//! the members of a pool to the same controls, as a test run on whichever
//! member is free must fit all of them alike.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Error;

/// A way the harness acts on a machine. A test step uses at most one,
/// except a bench step, which runs its program over SSH and boots Linux.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Control {
    /// Power on, shut down, the power button, reset.
    Power,
    /// Pulling the power cord: a power cut the system gets no warning of.
    Cord,
    /// Reading the serial console.
    Serial,
    /// The machine's console from the moment it powers on: the firmware's,
    /// GRUB's and the kernel's first and last lines, as a serial cable
    /// gives. A console that starts with the network (netlog) has none of
    /// them.
    #[serde(rename = "early-console")]
    EarlyConsole,
    /// Plugging and pulling the network cable.
    Link,
    /// Plugging and pulling a USB stick.
    Usb,
    /// Putting a memory card in the machine's card reader and taking it
    /// out.
    Card,
    /// Putting a memory card in the machine's own SD card reader, on PCI
    /// as a laptop's is, and taking it out.
    Sd,
    /// Putting a disc in the machine's optical drive and ejecting it.
    Disc,
    /// Putting a blank CD, DVD or Blu-ray disc in the machine's optical
    /// drive, which writes them, for a test to burn.
    Burner,
    /// Putting a floppy disk in the machine's floppy drive and taking it
    /// out.
    Floppy,
    /// A CompactFlash card in an IDE adapter, which tests may format: a
    /// blank one QEMU gives a boot for a test that asks for one, or the
    /// card a real machine has in its adapter.
    Cf,
    /// Pressing keys on the machine's keyboard.
    Keys,
    /// Moving the pointer and pressing its buttons.
    Mouse,
    /// Taking screenshots of the machine's screen.
    Screen,
    /// Running commands over SSH as the lab account.
    Ssh,
    /// Booting Linux on the machine, or on one set up the same way, to
    /// measure what performance tests hold Dukovo to.
    Linux,
    /// Running an attacker on a private lab network of the machine's own,
    /// to probe it from the outside.
    Attack,
    /// Using the lab PC as a peer on the machine's network: services the
    /// lab PC runs that the machine reaches (a Kerberos KDC, a file
    /// server, a WireGuard end), and ports of the machine the lab PC
    /// reaches to use the machine's own servers.
    Peer,
    /// Reaching the machine's out-of-band management, Intel AMT, from the
    /// lab PC (`{amt}`), as an administrator's console does across the
    /// network while the system runs.
    Amt,
    /// A QEMU guest agent channel: a virtio console port named
    /// `org.qemu.guest_agent.0`, as Proxmox's QEMU Guest Agent option and
    /// virt-manager give a virtual machine, whose host end the lab PC
    /// holds (`{guest-agent}`).
    GuestAgent,
    /// A virtio SCSI adapter with a blank disk on it, as Proxmox and
    /// Google Cloud attach disks.
    VirtioScsi,
}

impl Control {
    /// Every control, in the order lists show them.
    pub const ALL: [Control; 22] = [
        Control::Power,
        Control::Cord,
        Control::Serial,
        Control::EarlyConsole,
        Control::Link,
        Control::Usb,
        Control::Card,
        Control::Sd,
        Control::Disc,
        Control::Burner,
        Control::Floppy,
        Control::Cf,
        Control::Keys,
        Control::Mouse,
        Control::Screen,
        Control::Ssh,
        Control::Linux,
        Control::Attack,
        Control::Peer,
        Control::Amt,
        Control::GuestAgent,
        Control::VirtioScsi,
    ];

    /// Its name in machine files and output.
    pub fn name(self) -> &'static str {
        match self {
            Control::Power => "power",
            Control::Cord => "cord",
            Control::Serial => "serial",
            Control::EarlyConsole => "early-console",
            Control::Link => "link",
            Control::Usb => "usb",
            Control::Card => "card",
            Control::Sd => "sd",
            Control::Disc => "disc",
            Control::Burner => "burner",
            Control::Floppy => "floppy",
            Control::Cf => "cf",
            Control::Keys => "keys",
            Control::Mouse => "mouse",
            Control::Screen => "screen",
            Control::Ssh => "ssh",
            Control::Linux => "linux",
            Control::Attack => "attack",
            Control::Peer => "peer",
            Control::Amt => "amt",
            Control::GuestAgent => "guest-agent",
            Control::VirtioScsi => "virtio-scsi",
        }
    }
}

impl fmt::Display for Control {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Who does a control on a machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Who {
    /// The machine's driver, with no one at the machine.
    Driver,
    /// A person at the machine, when the runner asks.
    Person,
    /// No one: the machine cannot do it, and tests that need it skip it.
    None,
}

/// Which machine driver runs a machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriverKind {
    /// A QEMU virtual machine on the lab PC.
    Qemu,
    /// A real machine with Intel AMT, on the lab network: power and the
    /// next boot through AMT, a fresh disk for each test through net
    /// boot, its console through a serial cable to the lab PC.
    Amt,
    /// A real machine with neither AMT nor a serial port, on the lab
    /// network: power through a Shelly smart plug, a fresh disk for each
    /// test through net boot (its firmware boots from the network first),
    /// its console through Dukovo's netlog over UDP.
    Plug,
}

/// Firmware a QEMU machine boots through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Firmware {
    /// OVMF.
    Uefi,
    /// SeaBIOS, QEMU's own.
    Bios,
}

/// When a QEMU machine uses KVM.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kvm {
    /// When `/dev/kvm` opens, else QEMU's own emulator (TCG).
    Auto,
    /// Always: fail if it does not open.
    On,
    /// Never: always TCG.
    Off,
}

/// How a QEMU machine's disk is attached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskBus {
    /// SATA, as the OptiPlex's disk in AHCI mode.
    Sata,
    /// NVMe.
    Nvme,
    /// virtio-blk.
    Virtio,
}

/// A QEMU machine's hardware.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QemuSettings {
    #[serde(default = "default_firmware")]
    pub firmware: Firmware,
    #[serde(default = "default_cpus")]
    pub cpus: u32,
    /// Memory as QEMU's `-m` reads it: `2G`, `512M`.
    #[serde(default = "default_memory")]
    pub memory: String,
    #[serde(default = "default_kvm")]
    pub kvm: Kvm,
    #[serde(default = "default_disk")]
    pub disk: DiskBus,
    /// The network card, as a QEMU device name.
    #[serde(default = "default_nic")]
    pub nic: String,
    /// How many times longer tests may take under TCG, where Dukovo runs
    /// several times slower than under KVM.
    #[serde(default = "default_tcg_slowdown")]
    pub tcg_slowdown: u32,
    /// Whether a fresh boot's root is grown to the end of the disk before
    /// the test starts (see [`GROW_ROOT`](crate::run::GROW_ROOT)).
    #[serde(default = "default_grow_root")]
    pub grow_root: bool,
}

fn default_firmware() -> Firmware {
    Firmware::Uefi
}
fn default_cpus() -> u32 {
    2
}
fn default_memory() -> String {
    "2G".into()
}
fn default_kvm() -> Kvm {
    Kvm::Auto
}
fn default_disk() -> DiskBus {
    DiskBus::Sata
}
fn default_nic() -> String {
    "virtio-net-pci".into()
}
fn default_tcg_slowdown() -> u32 {
    4
}

impl Default for QemuSettings {
    fn default() -> Self {
        Self {
            firmware: default_firmware(),
            cpus: default_cpus(),
            memory: default_memory(),
            kvm: default_kvm(),
            disk: default_disk(),
            nic: default_nic(),
            tcg_slowdown: default_tcg_slowdown(),
            grow_root: default_grow_root(),
        }
    }
}

/// How the lab reaches a real machine with Intel AMT, and boots it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AmtSettings {
    /// AMT's address on the lab network, as set in the machine's MEBx.
    pub address: String,
    /// AMT's user.
    #[serde(default = "default_amt_user")]
    pub user: String,
    /// A file holding AMT's password, and nothing else, readable by this
    /// user alone. A relative path is from the machine file's folder.
    pub password_file: PathBuf,
    /// The MAC address of the machine's network card on the lab network,
    /// which it boots from the network with.
    pub mac: String,
    /// The serial number of the disk the image is written to, the only
    /// disk the writer may write (`lsblk -o NAME,SERIAL` shows it).
    pub disk: String,
    /// The lab PC's serial port that the machine's console is cabled to.
    pub serial: PathBuf,
    /// The serial console's speed.
    #[serde(default = "default_baud")]
    pub baud: u32,
    /// The machine's address on the lab network, for SSH and the lab
    /// PC's clients (`{machine}`).
    #[serde(default = "default_machine_address")]
    pub machine_address: String,
    /// The lab PC's address on the lab network: the net-boot server
    /// listens on it, and tests reach the lab PC there (`{peer}`).
    #[serde(default = "default_lab_address")]
    pub lab_address: String,
    /// The net-boot server's port.
    #[serde(default = "default_boot_port")]
    pub boot_port: u16,
    /// dnsmasq's TFTP folder, which boot scripts are written into.
    #[serde(default = "default_tftp")]
    pub tftp: PathBuf,
    /// The writer's kit: its kernel, initrd and command line, as `ob lab`
    /// installs it.
    #[serde(default = "default_writer")]
    pub writer: PathBuf,
    /// The serial console as Linux names it, for the writer and the Linux
    /// twin: `console=` gets this.
    #[serde(default = "default_console")]
    pub console: String,
    /// Whether a fresh boot's root is grown to the end of the disk before
    /// the test starts (see [`GROW_ROOT`](crate::run::GROW_ROOT)).
    #[serde(default = "default_grow_root")]
    pub grow_root: bool,
}

fn default_amt_user() -> String {
    "admin".into()
}
fn default_baud() -> u32 {
    115_200
}
fn default_machine_address() -> String {
    "10.77.0.2".into()
}
fn default_lab_address() -> String {
    "10.77.0.1".into()
}
fn default_boot_port() -> u16 {
    7431
}
fn default_tftp() -> PathBuf {
    "/srv/dukovo-lab/tftp".into()
}
fn default_writer() -> PathBuf {
    "/srv/dukovo-lab/writer".into()
}
fn default_console() -> String {
    "ttyS0,115200".into()
}

/// How the lab reaches a real machine with no AMT and no serial port, and
/// boots it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlugSettings {
    /// The Shelly plug's address on the home network, with `:port` for a
    /// plug that does not listen on port 80.
    pub shelly: String,
    /// Reserved for a plug that asks for a password: the lab cannot sign
    /// in to one yet, so a machine file that names a file is refused.
    pub password_file: Option<PathBuf>,
    /// The MAC address of the machine's network card on the lab network,
    /// which it boots from the network with.
    pub mac: String,
    /// The serial number of the disk the image is written to, the only
    /// disk the writer may write (`lsblk -o NAME,SERIAL` shows it).
    pub disk: String,
    /// The machine's address on the lab network, for SSH, the netlog's
    /// sender and the lab PC's clients (`{machine}`).
    #[serde(default = "default_machine_address")]
    pub machine_address: String,
    /// The lab PC's address on the lab network: the net-boot server and
    /// the netlog reader listen on it, and tests reach the lab PC there
    /// (`{peer}`).
    #[serde(default = "default_lab_address")]
    pub lab_address: String,
    /// The net-boot server's port.
    #[serde(default = "default_boot_port")]
    pub boot_port: u16,
    /// dnsmasq's TFTP folder, which boot scripts are written into.
    #[serde(default = "default_tftp")]
    pub tftp: PathBuf,
    /// The writer's kit, as `ob lab` installs it.
    #[serde(default = "default_writer")]
    pub writer: PathBuf,
    /// The UDP port the lab PC listens on for the machine's netlog.
    #[serde(default = "default_netlog_port")]
    pub netlog_port: u16,
    /// The least the machine draws while it runs, in watts: a plug that
    /// is on with the machine drawing less means the machine shut down.
    #[serde(default = "default_on_watts")]
    pub on_watts: f64,
    /// Whether a fresh boot's root is grown to the end of the disk before
    /// the test starts (see [`GROW_ROOT`](crate::run::GROW_ROOT)).
    #[serde(default = "default_grow_root")]
    pub grow_root: bool,
}

fn default_grow_root() -> bool {
    true
}
fn default_netlog_port() -> u16 {
    6666
}
fn default_on_watts() -> f64 {
    5.0
}

/// The serial console line that says a machine has finished booting,
/// unless its file names another.
pub const DEFAULT_READY: &str = "Boot complete.";

/// One machine.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Machine {
    /// Its name: the machine file's name without `.toml`.
    pub name: String,
    pub about: String,
    /// The file it was read from; none for the built-in `qemu`.
    pub file: Option<PathBuf>,
    pub driver: DriverKind,
    /// Text on the serial console that says the machine has booted.
    pub ready: String,
    pub controls: BTreeMap<Control, Who>,
    /// The pool the machine belongs to: machines of one pool have the
    /// same hardware and controls, and a run on the pool shares its tests
    /// among them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    /// Whether a run on the pool leaves the machine out, keeping it for
    /// reruns and bisects.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub spare: bool,
    /// For a QEMU machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qemu: Option<QemuSettings>,
    /// For a real machine with AMT.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amt: Option<AmtSettings>,
    /// For a real machine on a smart plug.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plug: Option<PlugSettings>,
}

impl Machine {
    /// Who does `control` on this machine.
    pub fn who(&self, control: Control) -> Who {
        self.controls.get(&control).copied().unwrap_or(Who::None)
    }

    /// The machine that needs no file: QEMU with the default hardware.
    fn built_in_qemu() -> Machine {
        Machine {
            name: "qemu".into(),
            about: "QEMU on this PC, UEFI, 2 CPUs, 2 GiB".into(),
            file: None,
            driver: DriverKind::Qemu,
            ready: DEFAULT_READY.into(),
            controls: driver_controls(&DriverKind::Qemu),
            pool: None,
            spare: false,
            qemu: Some(QemuSettings::default()),
            amt: None,
            plug: None,
        }
    }
}

/// The controls a driver can do by itself.
fn driver_controls(driver: &DriverKind) -> BTreeMap<Control, Who> {
    match driver {
        // QMP does all of them, and SSH reaches the guest through QEMU's
        // port forwarding; a QEMU machine has no AMT.
        DriverKind::Qemu => Control::ALL
            .iter()
            .filter(|&&control| control != Control::Amt)
            .map(|&control| (control, Who::Driver))
            .collect(),
        // AMT does power and the power cut (its hard off, which the system
        // gets no warning of), and its remote screen the keys, mouse and
        // screen; the serial cable does the console, from the firmware's
        // first line to the system's last; the lab network does SSH and
        // the lab PC's side, and net boot the Linux twin. Cables, sticks,
        // cards and discs are a person's to handle.
        DriverKind::Amt => [
            Control::Power,
            Control::Cord,
            Control::Serial,
            Control::EarlyConsole,
            Control::Keys,
            Control::Mouse,
            Control::Screen,
            Control::Ssh,
            Control::Linux,
            Control::Peer,
            Control::Amt,
        ]
        .into_iter()
        .map(|control| (control, Who::Driver))
        .collect(),
        // The plug does power and the power cut (a hard power loss, which
        // the system gets no warning of), netlog the console, and the lab
        // network SSH and the lab PC's side. The machine has no screen,
        // keyboard or mouse the lab can reach, and no Linux twin yet.
        DriverKind::Plug => {
            [Control::Power, Control::Cord, Control::Serial, Control::Ssh, Control::Peer]
                .into_iter()
                .map(|control| (control, Who::Driver))
                .collect()
        }
    }
}

/// A machine file as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MachineFile {
    about: String,
    driver: DriverKind,
    ready: Option<String>,
    #[serde(default)]
    controls: BTreeMap<Control, Who>,
    pool: Option<String>,
    #[serde(default)]
    spare: bool,
    qemu: Option<QemuSettings>,
    amt: Option<AmtSettings>,
    plug: Option<PlugSettings>,
}

/// Reads every machine file in `dir`, and adds the built-in `qemu` unless
/// a file takes its name. A missing folder holds no files.
pub fn load(dir: &Path) -> Result<Vec<Machine>, Error> {
    let mut machines = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries.collect::<Result<Vec<_>, _>>(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err),
    }
    .map_err(|err| Error::Usage(format!("reading the machines folder {}: {err}", dir.display())))?;
    for entry in entries {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "toml") {
            machines.push(load_file(&path)?);
        }
    }
    if !machines.iter().any(|machine| machine.name == "qemu") {
        machines.push(Machine::built_in_qemu());
    }
    machines.sort_by(|a, b| a.name.cmp(&b.name));
    check_pools(&machines)?;
    Ok(machines)
}

fn word(who: Who) -> String {
    format!("{who:?}").to_lowercase()
}

/// Holds every machine of a pool to the controls of the pool's first
/// machine, by name: a test that fits one must fit them all, as it runs
/// on whichever is free.
fn check_pools(machines: &[Machine]) -> Result<(), Error> {
    let mut first: BTreeMap<&str, &Machine> = BTreeMap::new();
    for machine in machines {
        let Some(pool) = machine.pool.as_deref() else { continue };
        let Some(&leader) = first.get(pool) else {
            first.insert(pool, machine);
            continue;
        };
        for control in Control::ALL {
            let (who, leader_who) = (machine.who(control), leader.who(control));
            if who != leader_who {
                let (who, leader_who) = (word(who), word(leader_who));
                let file = machine
                    .file
                    .as_ref()
                    .map_or(String::new(), |file| format!("{}: ", file.display()));
                return Err(Error::Usage(format!(
                    "{file}machine {} is in the pool \"{pool}\" but its {control} control is \
                     {who}, and {}'s is {leader_who}: the machines of a pool must have the \
                     same controls, so change [controls] in one of the files or give one of the \
                     machines another pool",
                    machine.name, leader.name,
                )));
            }
        }
    }
    Ok(())
}

/// Reads one machine file.
fn load_file(path: &Path) -> Result<Machine, Error> {
    let bad = |what: String| Error::Usage(format!("{}: {what}", path.display()));
    let name = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default().to_string();
    if !is_plain_name(&name) {
        return Err(bad("a machine file's name may hold only a-z, 0-9 and -".into()));
    }
    let text = fs::read_to_string(path).map_err(|err| bad(err.to_string()))?;
    let file: MachineFile = toml::from_str(&text).map_err(|err| bad(err.to_string()))?;
    if file.about.trim().is_empty() || file.about.contains('\n') {
        return Err(bad("about must be one line saying what the machine is".into()));
    }
    if let Some(pool) = &file.pool
        && !is_plain_name(pool)
    {
        return Err(bad("pool may hold only a-z, 0-9 and -".into()));
    }
    if file.spare && file.pool.is_none() {
        return Err(bad(
            "spare = true needs a pool: a spare is kept out of a run on its pool".into()
        ));
    }
    let mut controls = driver_controls(&file.driver);
    for (control, who) in file.controls {
        // A file may hand a control to a person or take it away, but cannot
        // make a driver do what it cannot.
        if who == Who::Driver && controls.get(&control) != Some(&Who::Driver) {
            return Err(bad(format!("the {:?} driver cannot do {control}", file.driver)));
        }
        // A person can pull a cord but cannot hand the runner Linux's
        // numbers, so booting the Linux twin is the driver's or no one's.
        if control == Control::Linux && who == Who::Person {
            return Err(bad(
                "linux is \"driver\" or \"none\": a person cannot measure Linux".into()
            ));
        }
        // An attack is the attacker machine's to run, not a person's, so
        // it is the driver's or no one's, as the Linux twin is.
        if control == Control::Attack && who == Who::Person {
            return Err(bad(
                "attack is \"driver\" or \"none\": a person does not run the attacker".into(),
            ));
        }
        // The lab PC's peer services and forwarded ports are the driver's
        // to set up, so a person cannot stand in for them either.
        if control == Control::Peer && who == Who::Person {
            return Err(bad(
                "peer is \"driver\" or \"none\": a person does not set up the lab PC's side".into(),
            ));
        }
        // AMT is reached across the lab network, or not at all, and a
        // virtual machine's devices are its driver's to give it.
        if control == Control::Amt && who == Who::Person {
            return Err(bad(
                "amt is \"driver\" or \"none\": a person cannot stand in for AMT".into()
            ));
        }
        // Reading the console from the power-on is a cable's or nothing:
        // no one at the machine can read what the firmware printed.
        if control == Control::EarlyConsole && who == Who::Person {
            return Err(bad(
                "early-console is \"driver\" or \"none\": a person cannot read the console from the power-on".into(),
            ));
        }
        if matches!(control, Control::GuestAgent | Control::VirtioScsi) && who == Who::Person {
            return Err(bad(format!(
                "{control} is \"driver\" or \"none\": a person does not add a virtual machine's devices"
            )));
        }
        controls.insert(control, who);
    }
    for (present, section, driver) in [
        (file.qemu.is_some(), "qemu", DriverKind::Qemu),
        (file.amt.is_some(), "amt", DriverKind::Amt),
        (file.plug.is_some(), "plug", DriverKind::Plug),
    ] {
        if present && file.driver != driver {
            return Err(bad(format!("[{section}] is for machines whose driver is {section}")));
        }
    }
    let needs = |article: &str, section: &str| {
        bad(format!("a machine whose driver is {section} needs {article} [{section}] section"))
    };
    let qemu = match file.driver {
        DriverKind::Qemu => Some(file.qemu.unwrap_or_default()),
        _ => None,
    };
    let amt = match file.driver {
        DriverKind::Amt => {
            let mut settings = file.amt.ok_or_else(|| needs("an", "amt"))?;
            if settings.password_file.is_relative()
                && let Some(folder) = path.parent()
            {
                settings.password_file = folder.join(&settings.password_file);
            }
            Some(settings)
        }
        _ => None,
    };
    let plug = match file.driver {
        DriverKind::Plug => Some(file.plug.ok_or_else(|| needs("a", "plug"))?),
        _ => None,
    };
    if let Some(settings) = &qemu {
        check_qemu(settings).map_err(bad)?;
    }
    if let Some(settings) = &amt {
        check_amt(settings).map_err(bad)?;
    }
    if let Some(settings) = &plug {
        check_plug(settings).map_err(bad)?;
    }
    Ok(Machine {
        name,
        about: file.about,
        file: Some(path.to_path_buf()),
        driver: file.driver,
        ready: file.ready.unwrap_or_else(|| DEFAULT_READY.into()),
        controls,
        pool: file.pool,
        spare: file.spare,
        qemu,
        amt,
        plug,
    })
}

/// Catches settings QEMU would refuse only once a run has started.
fn check_qemu(settings: &QemuSettings) -> Result<(), String> {
    if !(1..=64).contains(&settings.cpus) {
        return Err("qemu.cpus must be 1 to 64".into());
    }
    let memory = settings.memory.strip_suffix(['M', 'G']).unwrap_or("");
    if memory.is_empty() || !memory.bytes().all(|b| b.is_ascii_digit()) {
        return Err("qemu.memory must be a whole number of M or G, such as 2G".into());
    }
    if settings.nic.is_empty() || settings.nic.contains([',', ' ']) {
        return Err("qemu.nic must be one QEMU device name, such as virtio-net-pci".into());
    }
    if settings.tcg_slowdown == 0 {
        return Err("qemu.tcg_slowdown must be 1 or more".into());
    }
    Ok(())
}

/// Catches AMT settings that could only fail on the machine.
fn check_amt(settings: &AmtSettings) -> Result<(), String> {
    check_address("amt.address", &settings.address)?;
    check_lab_keys(
        "amt",
        &LabKeys {
            machine_address: &settings.machine_address,
            lab_address: &settings.lab_address,
            mac: &settings.mac,
            disk: &settings.disk,
            boot_port: settings.boot_port,
        },
    )?;
    if settings.user.is_empty() {
        return Err("amt.user must name AMT's user, admin as a rule".into());
    }
    if ![9600, 19_200, 38_400, 57_600, 115_200].contains(&settings.baud) {
        return Err("amt.baud must be 9600, 19200, 38400, 57600 or 115200".into());
    }
    if settings.console.is_empty() || settings.console.contains(char::is_whitespace) {
        return Err("amt.console must be one Linux console name, such as ttyS0,115200".into());
    }
    Ok(())
}

/// Catches plug settings that could only fail on the machine.
fn check_plug(settings: &PlugSettings) -> Result<(), String> {
    // A port after the address is for a plug behind a forward, or a test.
    let (host, port) = settings.shelly.split_once(':').unwrap_or((&settings.shelly, "80"));
    if host.parse::<std::net::Ipv4Addr>().is_err() || port.parse::<u16>().is_err() {
        return Err("plug.shelly must be the plug's IPv4 address on the home network, such as \
                    192.168.1.3"
            .into());
    }
    if settings.password_file.is_some() {
        return Err("plug.password_file: the lab cannot sign in to a plug that asks for a \
                    password yet; turn the plug's authentication off in its web page"
            .into());
    }
    check_lab_keys(
        "plug",
        &LabKeys {
            machine_address: &settings.machine_address,
            lab_address: &settings.lab_address,
            mac: &settings.mac,
            disk: &settings.disk,
            boot_port: settings.boot_port,
        },
    )?;
    if settings.netlog_port == 0 {
        return Err("plug.netlog_port must be a UDP port, 6666 as a rule".into());
    }
    if !settings.on_watts.is_finite() || settings.on_watts <= 0.0 {
        return Err("plug.on_watts must be more than 0: the least the machine draws while it \
                    runs, in watts"
            .into());
    }
    Ok(())
}

/// The settings of the lab network that every real machine's file has.
struct LabKeys<'a> {
    machine_address: &'a str,
    lab_address: &'a str,
    mac: &'a str,
    disk: &'a str,
    boot_port: u16,
}

/// Checks that `address` is an IPv4 address, as the lab network's are.
fn check_address(key: &str, address: &str) -> Result<(), String> {
    if address.parse::<std::net::Ipv4Addr>().is_err() {
        return Err(format!("{key} must be an IPv4 address on the lab network, such as 10.77.0.2"));
    }
    Ok(())
}

/// Catches lab network settings, of a driver's section named `section`,
/// that could only fail on the machine.
fn check_lab_keys(section: &str, keys: &LabKeys<'_>) -> Result<(), String> {
    check_address(&format!("{section}.machine_address"), keys.machine_address)?;
    check_address(&format!("{section}.lab_address"), keys.lab_address)?;
    let pairs: Vec<&str> = keys.mac.split(':').collect();
    if pairs.len() != 6
        || !pairs.iter().all(|pair| pair.len() == 2 && pair.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(format!("{section}.mac must be a MAC address written aa:bb:cc:dd:ee:ff"));
    }
    let serial = keys.disk.trim();
    if serial.is_empty() || serial != keys.disk || !serial.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(format!(
            "{section}.disk must be the disk's serial number, as `lsblk -o NAME,SERIAL` shows it"
        ));
    }
    crate::netboot::ports(keys.boot_port)
        .map(drop)
        .map_err(|err| format!("{section}.boot_port: {err}"))
}

/// Whether `name` is a test's or machine's name: a-z, 0-9 and `-`, not
/// starting with `-`.
pub fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Picks machines by name from a comma-separated list.
pub fn select<'a>(machines: &'a [Machine], names: &str) -> Result<Vec<&'a Machine>, Error> {
    let mut picked: Vec<&Machine> = Vec::new();
    for name in names.split(',').map(str::trim) {
        let machine = find(machines, name, false)?;
        if picked.iter().any(|already| already.name == machine.name) {
            return Err(Error::Usage(format!("machine \"{name}\" is picked twice")));
        }
        picked.push(machine);
    }
    Ok(picked)
}

fn find<'a>(machines: &'a [Machine], name: &str, pools_too: bool) -> Result<&'a Machine, Error> {
    machines.iter().find(|machine| machine.name == name).ok_or_else(|| {
        let known: Vec<&str> = machines.iter().map(|machine| machine.name.as_str()).collect();
        let pools = pool_names(machines);
        let pools = if pools_too && !pools.is_empty() {
            format!(" and the pools are {}", pools.join(", "))
        } else {
            String::new()
        };
        Error::Usage(format!(
            "no machine named \"{name}\"; the machines are {}{pools}",
            known.join(", ")
        ))
    })
}

/// The pools the machines name, as `--machine` writes them.
fn pool_names(machines: &[Machine]) -> Vec<String> {
    let pools: BTreeSet<&str> =
        machines.iter().filter_map(|machine| machine.pool.as_deref()).collect();
    pools.into_iter().map(|pool| format!("{POOL_PREFIX}{pool}")).collect()
}

/// What `--machine pool:<name>` is written with before the pool's name:
/// a colon cannot be in a machine's name, so the two never clash.
pub const POOL_PREFIX: &str = "pool:";

/// Whether `name` is a machine's name or a pool as `--machine` writes it.
pub fn is_machine_ref(name: &str) -> bool {
    is_plain_name(name.strip_prefix(POOL_PREFIX).unwrap_or(name))
}

/// A pool a run shares its tests out among.
#[derive(Clone, Debug)]
pub struct Pool<'a> {
    pub name: String,
    /// The machines that take part: the pool's machines but its spares.
    pub members: Vec<&'a Machine>,
    /// The pool's spares, which a run keeps out of its queue and runs a
    /// failed test on again, before a member does.
    pub spares: Vec<&'a Machine>,
}

impl Pool<'_> {
    /// How results and events name the pool, as `--machine` does.
    pub fn label(&self) -> String {
        format!("{POOL_PREFIX}{}", self.name)
    }
}

/// What a `--machine` list names: machines that each run every test, and
/// pools whose machines share their tests out.
#[derive(Debug, Default)]
pub struct Pick<'a> {
    pub direct: Vec<&'a Machine>,
    pub pools: Vec<Pool<'a>>,
}

impl<'a> Pick<'a> {
    /// Every machine the run uses: those named, then each pool's members.
    pub fn every(&self) -> Vec<&'a Machine> {
        let members = self.pools.iter().flat_map(|pool| pool.members.iter().copied());
        self.direct.iter().copied().chain(members).collect()
    }
}

/// Picks the machines and pools a comma-separated list names. A pool is
/// written `pool:<name>`. A spare machine named on its own runs every
/// test; a pool leaves it out.
pub fn pick<'a>(machines: &'a [Machine], names: &str) -> Result<Pick<'a>, Error> {
    let mut picked = Pick::default();
    for name in names.split(',').map(str::trim) {
        let Some(pool) = name.strip_prefix(POOL_PREFIX) else {
            let machine = find(machines, name, true)?;
            if picked.direct.iter().any(|already| already.name == machine.name) {
                return Err(Error::Usage(format!("machine \"{name}\" is picked twice")));
            }
            picked.direct.push(machine);
            continue;
        };
        let in_pool = || machines.iter().filter(|machine| machine.pool.as_deref() == Some(pool));
        if in_pool().next().is_none() {
            let known = pool_names(machines);
            return Err(Error::Usage(format!(
                "no pool named \"{pool}\"; {}",
                if known.is_empty() {
                    "no machine file has pool = \"...\" yet".to_string()
                } else {
                    format!("the pools are {}", known.join(", "))
                }
            )));
        }
        let members: Vec<&Machine> = in_pool().filter(|machine| !machine.spare).collect();
        let spares: Vec<&Machine> = in_pool().filter(|machine| machine.spare).collect();
        if members.is_empty() {
            return Err(Error::Usage(format!(
                "every machine of the pool \"{pool}\" is a spare, so a run on it has no machine \
                 to use: name a spare directly to run on it, or take spare = true out of a file"
            )));
        }
        if picked.pools.iter().any(|already| already.name == pool) {
            return Err(Error::Usage(format!("pool \"{pool}\" is picked twice")));
        }
        picked.pools.push(Pool { name: pool.to_string(), members, spares });
    }
    for pool in &picked.pools {
        if let Some(machine) = picked.direct.iter().find(|machine| pool.members.contains(machine)) {
            return Err(Error::Usage(format!(
                "machine \"{}\" is picked twice: by name and as a machine of {}",
                machine.name,
                pool.label()
            )));
        }
    }
    Ok(picked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn the_built_in_qemu_machine_needs_no_file() {
        let dir = TempDir::new("machines-none");
        let machines = load(&dir.path().join("missing")).unwrap();
        assert_eq!(machines.len(), 1);
        assert_eq!(machines[0].name, "qemu");
        for control in Control::ALL {
            let who = if control == Control::Amt { Who::None } else { Who::Driver };
            assert_eq!(machines[0].who(control), who, "{control}");
        }
    }

    #[test]
    fn a_file_hands_controls_to_a_person_or_takes_them_away() {
        let dir = TempDir::new("machines-controls");
        dir.write(
            "qemu-bios.toml",
            "about = \"QEMU, BIOS\"\ndriver = \"qemu\"\n\n[qemu]\nfirmware = \"bios\"\n\n\
             [controls]\ncord = \"person\"\nusb = \"none\"\n",
        );
        let machines = load(dir.path()).unwrap();
        let names: Vec<&str> = machines.iter().map(|machine| machine.name.as_str()).collect();
        assert_eq!(names, ["qemu", "qemu-bios"]);
        let bios = &machines[1];
        assert_eq!(bios.qemu.as_ref().unwrap().firmware, Firmware::Bios);
        assert_eq!(bios.who(Control::Cord), Who::Person);
        assert_eq!(bios.who(Control::Usb), Who::None);
        assert_eq!(bios.who(Control::Power), Who::Driver);
        assert_eq!(bios.ready, DEFAULT_READY);
    }

    #[test]
    fn bad_files_say_what_is_wrong() {
        for (file, text, complaint) in [
            ("Big.toml", "about = \"x\"\ndriver = \"qemu\"\n", "only a-z"),
            ("a.toml", "about = \"x\"\n", "missing field `driver`"),
            ("b.toml", "about = \"x\"\ndriver = \"qemu\"\nspeed = 3\n", "unknown field `speed`"),
            (
                "c.toml",
                "about = \"x\"\ndriver = \"qemu\"\n[qemu]\nmemory = \"lots\"\n",
                "qemu.memory",
            ),
            (
                "d.toml",
                "about = \"x\"\ndriver = \"qemu\"\n[controls]\nsmell = \"driver\"\n",
                "smell",
            ),
            (
                "e.toml",
                "about = \"x\"\ndriver = \"qemu\"\n[controls]\nlinux = \"person\"\n",
                "a person cannot measure Linux",
            ),
            (
                "f.toml",
                "about = \"x\"\ndriver = \"qemu\"\n[controls]\nattack = \"person\"\n",
                "a person does not run the attacker",
            ),
            (
                "g.toml",
                "about = \"x\"\ndriver = \"qemu\"\n[controls]\npeer = \"person\"\n",
                "a person does not set up the lab PC's side",
            ),
        ] {
            let dir = TempDir::new("machines-bad");
            dir.write(file, text);
            let Err(Error::Usage(message)) = load(dir.path()) else {
                panic!("{file} should not load");
            };
            assert!(message.contains(file), "{message}");
            assert!(message.contains(complaint), "{message}");
        }
    }

    #[test]
    fn the_example_machine_files_load() {
        let folder = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/machines"));
        let machines = load(folder).unwrap();
        let example = machines.iter().find(|machine| machine.name == "qemu-bios").unwrap();
        assert_eq!(example.qemu.as_ref().unwrap().firmware, Firmware::Bios);
        let real = machines.iter().find(|machine| machine.name == "optiplex").unwrap();
        assert_eq!(real.pool.as_deref(), Some("optiplex"));
        let amt = real.amt.as_ref().unwrap();
        assert_eq!(amt.password_file, folder.join("optiplex-amt-password"));
        assert_eq!(real.who(Control::Usb), Who::Person);
    }

    #[test]
    fn the_example_plug_machine_has_the_plug_s_controls() {
        let folder = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/machines"));
        let machines = load(folder).unwrap();
        let m75q = machines.iter().find(|machine| machine.name == "m75q").unwrap();
        assert_eq!(m75q.driver, DriverKind::Plug);
        let plug = m75q.plug.as_ref().unwrap();
        assert_eq!(plug.shelly, "192.168.1.3");
        assert_eq!(plug.machine_address, "10.77.0.3");
        assert_eq!(plug.netlog_port, 6666);
    }

    #[test]
    fn a_plug_machine_has_the_plug_s_controls_and_defaults() {
        let dir = TempDir::new("machines-plug");
        dir.write(
            "box.toml",
            "about = \"a box\"\ndriver = \"plug\"\n[plug]\nshelly = \"192.168.1.3\"\n\
             mac = \"AA:bb:cc:00:11:22\"\ndisk = \"S3Z5NB0K\"\n[controls]\nlink = \"person\"\n",
        );
        let machines = load(dir.path()).unwrap();
        let real = machines.iter().find(|machine| machine.name == "box").unwrap();
        assert_eq!(real.driver, DriverKind::Plug);
        assert!(real.qemu.is_none() && real.amt.is_none());
        for control in [Control::Power, Control::Cord, Control::Serial, Control::Ssh, Control::Peer]
        {
            assert_eq!(real.who(control), Who::Driver, "{control}");
        }
        assert_eq!(real.who(Control::Link), Who::Person);
        for control in
            [Control::Keys, Control::Mouse, Control::Screen, Control::Linux, Control::Amt]
        {
            assert_eq!(real.who(control), Who::None, "{control}");
        }
        let plug = real.plug.as_ref().unwrap();
        assert_eq!(plug.password_file, None);
        assert_eq!((plug.netlog_port, plug.boot_port), (6666, 7431));
        assert!((plug.on_watts - 5.0).abs() < f64::EPSILON);
        assert_eq!(plug.machine_address, "10.77.0.2");
        assert_eq!(plug.lab_address, "10.77.0.1");
        assert_eq!(plug.tftp, Path::new("/srv/dukovo-lab/tftp"));
    }

    #[test]
    fn real_machines_grow_their_root_unless_the_file_says_not() {
        let dir = TempDir::new("machines-grow-root");
        let plug = "driver = \"plug\"\n[plug]\nshelly = \"192.168.1.3\"\n\
                    mac = \"aa:bb:cc:00:11:22\"\ndisk = \"D\"\n";
        let amt = "driver = \"amt\"\n[amt]\naddress = \"10.77.0.10\"\npassword_file = \"p\"\n\
                   mac = \"aa:bb:cc:00:11:22\"\ndisk = \"D\"\nserial = \"/dev/ttyUSB0\"\n";
        for (name, settings) in [("plug", plug), ("amt", amt)] {
            dir.write(&format!("{name}-default.toml"), &format!("about = \"x\"\n{settings}"));
            dir.write(
                &format!("{name}-off.toml"),
                &format!("about = \"x\"\n{settings}grow_root = false\n"),
            );
        }
        let machines = load(dir.path()).unwrap();
        let grows = |name: &str| {
            let machine = machines.iter().find(|machine| machine.name == name).unwrap();
            match (&machine.plug, &machine.amt) {
                (Some(plug), _) => plug.grow_root,
                (_, Some(amt)) => amt.grow_root,
                _ => unreachable!("{name} is a real machine"),
            }
        };
        assert!(grows("plug-default") && grows("amt-default"));
        assert!(!grows("plug-off") && !grows("amt-off"));
    }

    #[test]
    fn a_qemu_machine_grows_its_root_unless_the_file_says_not() {
        let dir = TempDir::new("machines-qemu-grow-root");
        dir.write("on.toml", "about = \"x\"\ndriver = \"qemu\"\n");
        dir.write("off.toml", "about = \"x\"\ndriver = \"qemu\"\n[qemu]\ngrow_root = false\n");
        let machines = load(dir.path()).unwrap();
        let grows = |name: &str| {
            let machine = machines.iter().find(|machine| machine.name == name).unwrap();
            machine.qemu.clone().unwrap_or_default().grow_root
        };
        assert!(grows("on") && !grows("off"));
    }

    #[test]
    fn the_console_from_the_power_on_is_amt_s_and_qemu_s_alone() {
        let dir = TempDir::new("machines-early-console");
        dir.write(
            "plugbox.toml",
            "about = \"x\"\ndriver = \"plug\"\n[plug]\nshelly = \"192.168.1.3\"\n\
             mac = \"aa:bb:cc:00:11:22\"\ndisk = \"D\"\n",
        );
        dir.write(
            "amtbox.toml",
            "about = \"x\"\ndriver = \"amt\"\n[amt]\naddress = \"10.77.0.10\"\n\
             password_file = \"p\"\nmac = \"aa:bb:cc:00:11:22\"\ndisk = \"D\"\n\
             serial = \"/dev/ttyUSB0\"\n",
        );
        let machines = load(dir.path()).unwrap();
        let who = |name: &str| {
            machines.iter().find(|machine| machine.name == name).unwrap().who(Control::EarlyConsole)
        };
        assert_eq!(who("qemu"), Who::Driver);
        assert_eq!(who("amtbox"), Who::Driver);
        assert_eq!(who("plugbox"), Who::None);
        assert_eq!(Control::EarlyConsole.name(), "early-console");
        // A machine file names it with the same spelling.
        dir.write(
            "amtbox.toml",
            "about = \"x\"\ndriver = \"amt\"\n[amt]\naddress = \"10.77.0.10\"\n\
             password_file = \"p\"\nmac = \"aa:bb:cc:00:11:22\"\ndisk = \"D\"\n\
             serial = \"/dev/ttyUSB0\"\n[controls]\nearly-console = \"none\"\n",
        );
        let machines = load(dir.path()).unwrap();
        let amt = machines.iter().find(|machine| machine.name == "amtbox").unwrap();
        assert_eq!(amt.who(Control::EarlyConsole), Who::None);
        // A person cannot read what the firmware printed.
        dir.write(
            "amtbox.toml",
            "about = \"x\"\ndriver = \"amt\"\n[amt]\naddress = \"10.77.0.10\"\n\
             password_file = \"p\"\nmac = \"aa:bb:cc:00:11:22\"\ndisk = \"D\"\n\
             serial = \"/dev/ttyUSB0\"\n[controls]\nearly-console = \"person\"\n",
        );
        let Err(Error::Usage(message)) = load(dir.path()) else {
            panic!("a person cannot do early-console");
        };
        assert!(message.contains("early-console is \"driver\" or \"none\""), "{message}");
    }

    #[test]
    fn bad_plug_settings_say_what_is_wrong() {
        let plug = |extra: &str| {
            format!(
                "about = \"x\"\ndriver = \"plug\"\n[plug]\nmac = \"aa:bb:cc:00:11:22\"\n\
                 disk = \"D\"\n{extra}"
            )
        };
        for (text, complaint) in [
            ("about = \"x\"\ndriver = \"plug\"\n".to_string(), "needs a [plug] section"),
            (plug(""), "missing field `shelly`"),
            (plug("shelly = \"plug.lan\"\n"), "plug.shelly"),
            (plug("shelly = \"192.168.1.3:x\"\n"), "plug.shelly"),
            (plug("shelly = \"192.168.1.3\"\npassword_file = \"p\"\n"), "authentication off"),
            (plug("shelly = \"192.168.1.3\"\nlab_address = \"lab\"\n"), "plug.lab_address"),
            (plug("shelly = \"192.168.1.3\"\nnetlog_port = 0\n"), "plug.netlog_port"),
            (plug("shelly = \"192.168.1.3\"\non_watts = 0.0\n"), "plug.on_watts"),
            (plug("shelly = \"192.168.1.3\"\nbaud = 9600\n"), "unknown field `baud`"),
            (
                plug("shelly = \"192.168.1.3\"\n[controls]\nscreen = \"driver\"\n"),
                "cannot do screen",
            ),
            (
                plug("shelly = \"192.168.1.3\"\n").replace("\"plug\"", "\"qemu\""),
                "[plug] is for machines whose driver is plug",
            ),
            (
                plug("shelly = \"192.168.1.3\"\n").replace("\"plug\"", "\"amt\""),
                "[plug] is for machines whose driver is plug",
            ),
        ] {
            let dir = TempDir::new("machines-plug-bad");
            dir.write("box.toml", &text);
            let Err(Error::Usage(message)) = load(dir.path()) else {
                panic!("{text} should not load");
            };
            assert!(message.contains(complaint), "{message}");
        }
    }

    #[test]
    fn a_real_machine_has_amt_s_controls_and_settings() {
        let dir = TempDir::new("machines-amt");
        dir.write(
            "box.toml",
            "about = \"a box\"\ndriver = \"amt\"\n[amt]\naddress = \"10.77.0.10\"\n\
             password_file = \"box-password\"\nmac = \"AA:bb:cc:00:11:22\"\ndisk = \"S3Z5NB0K\"\n\
             serial = \"/dev/ttyUSB0\"\n[controls]\nlink = \"person\"\n",
        );
        let machines = load(dir.path()).unwrap();
        let real = machines.iter().find(|machine| machine.name == "box").unwrap();
        assert_eq!(real.driver, DriverKind::Amt);
        assert!(real.qemu.is_none());
        for control in [
            Control::Power,
            Control::Cord,
            Control::Serial,
            Control::Keys,
            Control::Mouse,
            Control::Screen,
            Control::Ssh,
            Control::Peer,
            Control::Amt,
        ] {
            assert_eq!(real.who(control), Who::Driver, "{control}");
        }
        assert_eq!(real.who(Control::Linux), Who::Driver);
        assert_eq!(real.who(Control::Link), Who::Person);
        for control in [
            Control::Usb,
            Control::Card,
            Control::Disc,
            Control::Attack,
            Control::GuestAgent,
            Control::VirtioScsi,
        ] {
            assert_eq!(real.who(control), Who::None, "{control}");
        }
        let amt = real.amt.as_ref().unwrap();
        assert_eq!(amt.password_file, dir.path().join("box-password"));
        assert_eq!((amt.user.as_str(), amt.baud, amt.boot_port), ("admin", 115_200, 7431));
        assert_eq!(amt.machine_address, "10.77.0.2");
        assert_eq!(amt.lab_address, "10.77.0.1");
        assert_eq!(amt.tftp, Path::new("/srv/dukovo-lab/tftp"));
    }

    #[test]
    fn bad_amt_settings_say_what_is_wrong() {
        let amt = |extra: &str| {
            format!(
                "about = \"x\"\ndriver = \"amt\"\n[amt]\naddress = \"10.77.0.10\"\n\
                 password_file = \"p\"\nserial = \"/dev/ttyUSB0\"\n{extra}"
            )
        };
        for (text, complaint) in [
            ("about = \"x\"\ndriver = \"amt\"\n".to_string(), "needs an [amt] section"),
            (amt("mac = \"aa:bb\"\ndisk = \"D\"\n"), "amt.mac"),
            (amt("mac = \"aa:bb:cc:00:11:22\"\ndisk = \" D\"\n"), "amt.disk"),
            (
                amt("mac = \"aa:bb:cc:00:11:22\"\ndisk = \"D\"\nlab_address = \"lab\"\n"),
                "amt.lab_address",
            ),
            (amt("mac = \"aa:bb:cc:00:11:22\"\ndisk = \"D\"\nbaud = 1\n"), "amt.baud"),
            (amt("mac = \"aa:bb:cc:00:11:22\"\ndisk = \"D\"\nconsole = \"a b\"\n"), "amt.console"),
            (
                amt("mac = \"aa:bb:cc:00:11:22\"\ndisk = \"D\"\nsmell = 1\n"),
                "unknown field `smell`",
            ),
            (
                amt("mac = \"aa:bb:cc:00:11:22\"\ndisk = \"D\"\n[controls]\nusb = \"driver\"\n"),
                "cannot do usb",
            ),
            (
                amt("mac = \"aa:bb:cc:00:11:22\"\ndisk = \"D\"\n").replace("\"amt\"", "\"qemu\""),
                "[amt] is for machines whose driver is amt",
            ),
        ] {
            let dir = TempDir::new("machines-amt-bad");
            dir.write("box.toml", &text);
            let Err(Error::Usage(message)) = load(dir.path()) else {
                panic!("{text} should not load");
            };
            assert!(message.contains(complaint), "{message}");
        }
    }

    /// A QEMU machine file in the pool `pool`, with the extra lines.
    fn pooled(pool: &str, extra: &str) -> String {
        format!("about = \"x\"\ndriver = \"qemu\"\npool = \"{pool}\"\n{extra}")
    }

    #[test]
    fn a_machine_file_names_a_pool_and_says_whether_it_is_a_spare() {
        let dir = TempDir::new("machines-pool");
        dir.write("box-1.toml", &pooled("boxes", ""));
        dir.write("box-2.toml", &pooled("boxes", "spare = true\n"));
        dir.write("alone.toml", "about = \"x\"\ndriver = \"qemu\"\n");
        let machines = load(dir.path()).unwrap();
        let get = |name: &str| machines.iter().find(|machine| machine.name == name).unwrap();
        assert_eq!((get("box-1").pool.as_deref(), get("box-1").spare), (Some("boxes"), false));
        assert_eq!((get("box-2").pool.as_deref(), get("box-2").spare), (Some("boxes"), true));
        assert_eq!((get("alone").pool.as_deref(), get("alone").spare), (None, false));
        assert_eq!(get("qemu").pool, None);
    }

    #[test]
    fn a_pool_whose_machines_differ_in_a_control_is_an_error_naming_the_machine_and_control() {
        let dir = TempDir::new("machines-pool-differs");
        dir.write("box-1.toml", &pooled("boxes", "[controls]\nusb = \"person\"\n"));
        dir.write("box-2.toml", &pooled("boxes", "[controls]\nusb = \"none\"\n"));
        let Err(Error::Usage(message)) = load(dir.path()) else {
            panic!("a pool of machines with different controls should not load");
        };
        assert!(
            message.contains("box-2.toml")
                && message.contains("machine box-2 is in the pool \"boxes\"")
                && message.contains("its usb control is none, and box-1's is person"),
            "{message}"
        );
        // The same controls, written differently, are the same.
        dir.write(
            "box-2.toml",
            &pooled("boxes", "[controls]\nusb = \"person\"\nlink = \"driver\"\n"),
        );
        assert!(load(dir.path()).is_ok());
    }

    #[test]
    fn bad_pool_settings_say_what_is_wrong() {
        for (text, complaint) in [
            (pooled("Big Pool", ""), "pool may hold only a-z"),
            (
                "about = \"x\"\ndriver = \"qemu\"\nspare = true\n".to_string(),
                "spare = true needs a pool",
            ),
        ] {
            let dir = TempDir::new("machines-pool-bad");
            dir.write("box.toml", &text);
            let Err(Error::Usage(message)) = load(dir.path()) else {
                panic!("{text} should not load");
            };
            assert!(message.contains(complaint), "{message}");
        }
    }

    #[test]
    fn a_pool_is_picked_as_its_machines_but_its_spares() {
        let dir = TempDir::new("machines-pick-pool");
        dir.write("box-1.toml", &pooled("boxes", ""));
        dir.write("box-2.toml", &pooled("boxes", ""));
        dir.write("box-3.toml", &pooled("boxes", "spare = true\n"));
        let machines = load(dir.path()).unwrap();
        let names = |picked: &[&Machine]| -> Vec<String> {
            picked.iter().map(|machine| machine.name.clone()).collect()
        };
        let picked = pick(&machines, "qemu,pool:boxes").unwrap();
        assert_eq!(names(&picked.direct), ["qemu"]);
        assert_eq!(picked.pools.len(), 1);
        assert_eq!(picked.pools[0].label(), "pool:boxes");
        assert_eq!(names(&picked.pools[0].members), ["box-1", "box-2"]);
        assert_eq!(names(&picked.every()), ["qemu", "box-1", "box-2"]);
        // A spare named on its own runs every test.
        let picked = pick(&machines, "box-3, pool:boxes").unwrap();
        assert_eq!(names(&picked.direct), ["box-3"]);
        assert_eq!(names(&picked.pools[0].members), ["box-1", "box-2"]);
    }

    #[test]
    fn a_pool_that_is_not_there_or_is_picked_twice_is_refused() {
        let dir = TempDir::new("machines-pick-pool-bad");
        dir.write("box-1.toml", &pooled("boxes", ""));
        dir.write("lone.toml", &pooled("spares", "spare = true\n"));
        let machines = load(dir.path()).unwrap();
        let refused = |names: &str| match pick(&machines, names) {
            Err(Error::Usage(message)) => message,
            other => panic!("{names} should be refused, got {other:?}"),
        };
        assert!(refused("pool:crates").contains("the pools are pool:boxes, pool:spares"));
        assert!(refused("pool:boxes,pool:boxes").contains("picked twice"));
        assert!(refused("box-1,pool:boxes").contains("picked twice"));
        assert!(refused("pool:spares").contains("is a spare"));
        let unknown = refused("crates");
        assert!(
            unknown.contains("the machines are box-1, lone, qemu and the pools are"),
            "{unknown}"
        );
        assert!(is_machine_ref("pool:boxes") && is_machine_ref("box-1"));
        assert!(
            !is_machine_ref("pool:") && !is_machine_ref("pool:Boxes") && !is_machine_ref("a:b")
        );
    }

    #[test]
    fn picks_machines_by_name() {
        let machines = [Machine::built_in_qemu()];
        assert_eq!(select(&machines, "qemu").unwrap().len(), 1);
        assert!(select(&machines, "qemu,qemu").is_err());
        assert!(select(&machines, "optiplex").is_err());
    }
}

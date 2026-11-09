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

use std::collections::BTreeMap;
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
    /// Plugging and pulling the network cable.
    Link,
    /// Plugging and pulling a USB stick.
    Usb,
    /// Putting a memory card in the machine's card reader and taking it
    /// out.
    Card,
    /// Putting a disc in the machine's optical drive and ejecting it.
    Disc,
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
}

impl Control {
    /// Every control, in the order lists show them.
    pub const ALL: [Control; 14] = [
        Control::Power,
        Control::Cord,
        Control::Serial,
        Control::Link,
        Control::Usb,
        Control::Card,
        Control::Disc,
        Control::Keys,
        Control::Mouse,
        Control::Screen,
        Control::Ssh,
        Control::Linux,
        Control::Attack,
        Control::Peer,
    ];

    /// Its name in machine files and output.
    pub fn name(self) -> &'static str {
        match self {
            Control::Power => "power",
            Control::Cord => "cord",
            Control::Serial => "serial",
            Control::Link => "link",
            Control::Usb => "usb",
            Control::Card => "card",
            Control::Disc => "disc",
            Control::Keys => "keys",
            Control::Mouse => "mouse",
            Control::Screen => "screen",
            Control::Ssh => "ssh",
            Control::Linux => "linux",
            Control::Attack => "attack",
            Control::Peer => "peer",
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
        }
    }
}

/// The serial console line that says a machine has finished booting,
/// unless its file names another.
pub const DEFAULT_READY: &str = "Boot complete.";

/// One machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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
    /// For a QEMU machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qemu: Option<QemuSettings>,
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
            qemu: Some(QemuSettings::default()),
        }
    }
}

/// The controls a driver can do by itself.
fn driver_controls(driver: &DriverKind) -> BTreeMap<Control, Who> {
    match driver {
        // QMP does all of them, and SSH reaches the guest through QEMU's
        // port forwarding.
        DriverKind::Qemu => Control::ALL.iter().map(|&control| (control, Who::Driver)).collect(),
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
    qemu: Option<QemuSettings>,
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
    Ok(machines)
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
        controls.insert(control, who);
    }
    let qemu = match file.driver {
        DriverKind::Qemu => Some(file.qemu.unwrap_or_default()),
    };
    if let Some(settings) = &qemu {
        check_qemu(settings).map_err(bad)?;
    }
    Ok(Machine {
        name,
        about: file.about,
        file: Some(path.to_path_buf()),
        driver: file.driver,
        ready: file.ready.unwrap_or_else(|| DEFAULT_READY.into()),
        controls,
        qemu,
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
        let machine = machines.iter().find(|machine| machine.name == name).ok_or_else(|| {
            let known: Vec<&str> = machines.iter().map(|machine| machine.name.as_str()).collect();
            Error::Usage(format!(
                "no machine named \"{name}\"; the machines are {}",
                known.join(", ")
            ))
        })?;
        if picked.iter().any(|already| already.name == machine.name) {
            return Err(Error::Usage(format!("machine \"{name}\" is picked twice")));
        }
        picked.push(machine);
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
        assert!(Control::ALL.iter().all(|&control| machines[0].who(control) == Who::Driver));
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
    fn the_example_machine_file_loads() {
        let machines = load(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/machines"))).unwrap();
        let example = machines.iter().find(|machine| machine.name == "qemu-bios").unwrap();
        assert_eq!(example.qemu.as_ref().unwrap().firmware, Firmware::Bios);
    }

    #[test]
    fn picks_machines_by_name() {
        let machines = [Machine::built_in_qemu()];
        assert_eq!(select(&machines, "qemu").unwrap().len(), 1);
        assert!(select(&machines, "qemu,qemu").is_err());
        assert!(select(&machines, "optiplex").is_err());
    }
}

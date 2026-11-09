// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The QEMU machine driver: a virtual machine on the lab PC, controlled
//! through QMP.
//!
//! Each fresh boot gets a qcow2 overlay over the image under test, so the
//! image is never written and machines running side by side never share
//! writes. The guest's first serial port goes to `serial-<boot>.log` in
//! the bundle, and QEMU's own messages and command lines to
//! `qemu-<boot>.log` beside it: look there first when a machine will not
//! start. SSH reaches the guest through user-mode networking, forwarded
//! from a free port on 127.0.0.1, so nothing needs root and the guest is
//! reachable only from the lab PC.
//!
//! Every control maps to one QMP command: power to `system_powerdown`,
//! `system_reset` and `quit` (the cord), the network cable to `set_link`,
//! a USB stick to `blockdev-add` and `device_add usb-storage` on an xhci
//! controller, a memory card to `blockdev-change-medium` and `eject` on
//! a card reader (a `usb-storage` whose medium is removable, empty at
//! boot, there only for a test with card steps), a disc to the same on
//! an optical drive (an `ide-cd` on the q35 chipset's AHCI controller,
//! empty at boot, there only for a test with disc steps), keys to
//! `send-key`, the pointer (a USB tablet) to `input-send-event`, the
//! screen to `screendump`.
//!
//! The lab PC is the guest's peer on QEMU's user network, where the guest
//! reaches the lab PC's loopback at 10.0.2.2 (`{peer}`); a guest's port
//! the lab PC's clients need is forwarded from a free port of the lab
//! PC's loopback with the monitor's `hostfwd_add`, for the rest of the
//! boot.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

use super::{Driver, Setup};
use crate::bench::Kit;
use crate::machines::{DiskBus, Firmware, Kvm, QemuSettings};
use crate::qmp::Qmp;
use crate::screen::Image;
use crate::ssh;
use crate::suite::{Button, PowerAction, Stick};

/// The QEMU binary.
const QEMU: &str = "qemu-system-x86_64";

/// The network device's id, which `set_link` and `hostfwd_add` name.
const NETDEV: &str = "lab0";

/// The card reader's drive, which a card goes in and comes out of.
const CARD_DRIVE: &str = "card-reader";

/// The optical drive's drive, which a disc goes in and comes out of.
const DISC_DRIVE: &str = "optical-drive";

/// The lab PC's address on QEMU's user network, as the guest sees it.
const HOST_ON_USER_NET: &str = "10.0.2.2";

/// The private-network device's id on the machine under test and on the
/// attacker, joining the two over a QEMU socket link.
const SEGMENT: &str = "seg0";

/// Fixed MAC addresses on the private network, so each side is told apart.
const TARGET_MAC: &str = "52:54:00:4c:a1:01";
const ATTACKER_MAC: &str = "52:54:00:4c:a1:02";

/// The most of where QEMU's pointing devices point, on each axis
/// (INPUT_EVENT_ABS_MAX): the screen's right and bottom edges.
const ABS_MAX: usize = 0x7fff;

/// Where distributions install OVMF, as (code, variables) pairs; the same
/// list as `ob`'s.
const OVMF_PATHS: &[(&str, &str)] = &[
    // Debian and Ubuntu, including Linux Mint (4 MiB build, then 2 MiB)
    ("/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_VARS_4M.fd"),
    ("/usr/share/OVMF/OVMF_CODE.fd", "/usr/share/OVMF/OVMF_VARS.fd"),
    // Fedora
    ("/usr/share/edk2/ovmf/OVMF_CODE.fd", "/usr/share/edk2/ovmf/OVMF_VARS.fd"),
    // Arch Linux
    ("/usr/share/edk2/x64/OVMF_CODE.4m.fd", "/usr/share/edk2/x64/OVMF_VARS.4m.fd"),
    ("/usr/share/edk2-ovmf/x64/OVMF_CODE.fd", "/usr/share/edk2-ovmf/x64/OVMF_VARS.fd"),
];

/// How long a hard power-off may take before QEMU is killed.
const QUIT_TIME: Duration = Duration::from_secs(10);

/// One QEMU virtual machine.
pub struct Qemu {
    settings: QemuSettings,
    setup: Setup,
    kvm: bool,
    /// OVMF's code and the variable store copied for each boot.
    ovmf: Option<(PathBuf, PathBuf)>,
    /// Fresh boots so far: names this boot's disk, variables and logs.
    boot: u32,
    /// The QEMU process while the machine is on.
    running: Option<Running>,
    /// This boot's serial log.
    serial: PathBuf,
    /// The forwarded SSH port of this boot.
    ssh_port: u16,
    /// USB sticks plugged in so far, to name the next; and the one in now.
    sticks: u32,
    stick: Option<u32>,
    /// Memory cards put in so far, to name the next; the one in now; and
    /// the one taken out last, whose file is kept so that it can go back
    /// in holding what was written on it.
    cards: u32,
    card: Option<u32>,
    card_out: Option<u32>,
    /// Whether the next fresh boot gives the machine an empty card reader,
    /// set by [`Driver::want_card_reader`].
    want_card_reader: bool,
    /// The image of the disc in the optical drive now, and of the one
    /// taken out last, to go back in. The guest never writes a disc, so
    /// it is the test's own file, opened read-only.
    disc: Option<PathBuf>,
    disc_out: Option<PathBuf>,
    /// Whether the next fresh boot gives the machine an empty optical
    /// drive, set by [`Driver::want_disc_drive`].
    want_disc_drive: bool,
    /// The install stick this boot started from, while it is still in:
    /// its number and the overlay over the image it holds. It stays in
    /// across power cycles, as a real stick would, until it is pulled.
    boot_stick: Option<(u32, PathBuf)>,
    /// Whether the next fresh boot gives the machine a second network port
    /// on a private network for an attacker, set by [`Driver::want_attacker`].
    want_attacker: bool,
    /// The port the machine's private-network port listens on this boot,
    /// which the attacker connects to; set only on an armed boot.
    segment_port: u16,
    /// The attacker machine while it is running, with its SSH port.
    attacker: Option<Attacker>,
    /// The guest's ports forwarded to the lab PC this boot, as (UDP,
    /// the guest's port) to the lab PC's port.
    forwards: BTreeMap<(bool, u16), u16>,
}

/// The attacker machine: a second QEMU on the private network.
struct Attacker {
    running: Running,
    ssh_port: u16,
}

/// A running QEMU and its QMP connection.
struct Running {
    child: Child,
    qmp: Qmp,
}

impl Qemu {
    /// Checks that QEMU and, for UEFI, OVMF are installed, and makes the
    /// driver's folders. Starts nothing.
    pub fn new(settings: QemuSettings, setup: Setup) -> Result<Qemu, String> {
        Command::new(QEMU).arg("--version").output().map_err(|err| {
            format!(
                "{QEMU} will not start ({err}); install QEMU (Mint: apt install qemu-system-x86)"
            )
        })?;
        let kvm = match settings.kvm {
            Kvm::Off => false,
            Kvm::Auto => kvm_opens(),
            Kvm::On if kvm_opens() => true,
            Kvm::On => {
                return Err("the machine file asks for KVM, but /dev/kvm does not open".into());
            }
        };
        let ovmf = match settings.firmware {
            Firmware::Bios => None,
            Firmware::Uefi => Some(find_ovmf().ok_or(
                "UEFI machines need OVMF (Mint: apt install ovmf), or set DUKOVO_LAB_OVMF_CODE and DUKOVO_LAB_OVMF_VARS",
            )?),
        };
        for folder in [&setup.logs, &setup.work, &setup.sockets] {
            fs::create_dir_all(folder)
                .map_err(|err| format!("making {}: {err}", folder.display()))?;
        }
        Ok(Qemu {
            settings,
            setup,
            kvm,
            ovmf,
            boot: 0,
            running: None,
            serial: PathBuf::new(),
            ssh_port: 0,
            sticks: 0,
            stick: None,
            cards: 0,
            card: None,
            card_out: None,
            want_card_reader: false,
            disc: None,
            disc_out: None,
            want_disc_drive: false,
            boot_stick: None,
            want_attacker: false,
            segment_port: 0,
            attacker: None,
            forwards: BTreeMap::new(),
        })
    }

    fn disk(&self) -> PathBuf {
        self.setup.work.join(format!("disk-{}.qcow2", self.boot))
    }

    fn variables(&self) -> PathBuf {
        self.setup.work.join(format!("vars-{}.fd", self.boot))
    }

    fn qemu_log(&self) -> PathBuf {
        self.setup.logs.join(format!("qemu-{}.log", self.boot))
    }

    fn qmp_socket(&self) -> PathBuf {
        self.setup.sockets.join("qmp.sock")
    }

    /// Runs `qemu-img create -q` with `arguments`, which make `what`.
    fn qemu_img(arguments: &[&str], what: &str) -> Result<(), String> {
        let output = Command::new("qemu-img")
            .arg("create")
            .arg("-q")
            .args(arguments)
            .output()
            .map_err(|err| format!("qemu-img will not start ({err}); install QEMU's tools (Mint: apt install qemu-utils)"))?;
        if !output.status.success() {
            return Err(format!(
                "qemu-img could not make {what}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }

    /// Makes this boot's disk: an overlay whose reads fall through to the
    /// image and whose writes stay in the overlay.
    fn make_disk(&self) -> Result<(), String> {
        let image = fs::canonicalize(&self.setup.image)
            .map_err(|err| format!("the image {}: {err}", self.setup.image.display()))?;
        let format =
            if image.extension().is_some_and(|ext| ext == "qcow2") { "qcow2" } else { "raw" };
        let disk = self.disk();
        Self::qemu_img(
            &["-f", "qcow2", "-F", format, "-b", &image.to_string_lossy(), &disk.to_string_lossy()],
            &format!("a disk over {}", image.display()),
        )
    }

    /// QEMU's arguments for this boot.
    fn arguments(&self) -> Vec<String> {
        let settings = &self.settings;
        let (accel, cpu) = if self.kvm { ("kvm", "host") } else { ("tcg", "max") };
        let mut args: Vec<String> = vec![
            "-M".into(),
            "q35".into(),
            "-accel".into(),
            accel.into(),
            "-cpu".into(),
            cpu.into(),
            "-smp".into(),
            settings.cpus.to_string(),
            "-m".into(),
            settings.memory.clone(),
            // No window: the screen is read through QMP's screendump.
            "-display".into(),
            "none".into(),
            "-monitor".into(),
            "none".into(),
            // append=on keeps one log across power cycles of a boot, so the
            // runner's place in it stays valid.
            "-chardev".into(),
            format!("file,id=serial0,path={},append=on", option_value(&self.serial)),
            "-serial".into(),
            "chardev:serial0".into(),
            "-qmp".into(),
            format!("unix:{},server=on,wait=off", option_value(&self.qmp_socket())),
        ];
        if let Some((code, _)) = &self.ovmf {
            args.push("-drive".into());
            args.push(format!("if=pflash,format=raw,readonly=on,file={}", option_value(code)));
            args.push("-drive".into());
            args.push(format!("if=pflash,format=raw,file={}", option_value(&self.variables())));
        }
        args.push("-drive".into());
        args.push(format!("if=none,id=disk,format=qcow2,file={}", option_value(&self.disk())));
        args.push("-device".into());
        // While the install stick is in, the firmware starts it first, as
        // a person picks it in the boot menu; the disk after.
        let disk_index = if self.boot_stick.is_some() { 1 } else { 0 };
        args.push(match settings.disk {
            // q35's own SATA controller is AHCI, as the OptiPlex's in AHCI mode.
            DiskBus::Sata => format!("ide-hd,drive=disk,bootindex={disk_index}"),
            DiskBus::Nvme => format!("nvme,drive=disk,serial=lab-disk,bootindex={disk_index}"),
            DiskBus::Virtio => format!("virtio-blk-pci,drive=disk,bootindex={disk_index}"),
        });
        args.push("-netdev".into());
        args.push(format!("user,id={NETDEV},hostfwd=tcp:127.0.0.1:{}-:22", self.ssh_port));
        args.push("-device".into());
        args.push(format!("{},netdev={NETDEV}", settings.nic));
        // A second port on a private network of its own (net1), for an
        // attacker to reach the machine on, when a test attacks. It
        // listens; the attacker connects to it when it comes up. It is
        // added only on an armed boot, so an idle one never shows up in a
        // network test.
        if self.want_attacker {
            args.push("-netdev".into());
            args.push(format!("socket,id={SEGMENT},listen=127.0.0.1:{}", self.segment_port));
            args.push("-device".into());
            args.push(format!("{},netdev={SEGMENT},mac={TARGET_MAC}", settings.nic));
        }
        // USB sticks are plugged into this controller.
        args.push("-device".into());
        args.push("qemu-xhci,id=xhci".into());
        // The pointer: a tablet, which says where it points, so that a
        // test points at a pixel rather than moving by guesswork.
        args.push("-device".into());
        args.push("usb-tablet,bus=xhci.0,id=pointer".into());
        // A card reader, empty: a USB one, as QEMU has no other a card
        // can be put in and taken out of while it runs. Only on a boot for
        // a test with card steps, as its drive would show up in a disk
        // test.
        if self.want_card_reader {
            args.push("-drive".into());
            args.push(format!("if=none,id={CARD_DRIVE}"));
            args.push("-device".into());
            args.push(format!(
                "usb-storage,bus=xhci.0,drive={CARD_DRIVE},id=usb-card-reader,removable=on"
            ));
        }
        // An optical drive, empty: a DVD drive on the chipset's AHCI
        // controller's second port, after a SATA disk's first. Only on a
        // boot for a test with disc steps, for the same reason.
        if self.want_disc_drive {
            args.push("-drive".into());
            args.push(format!("if=none,id={DISC_DRIVE},media=cdrom"));
            args.push("-device".into());
            args.push(format!("ide-cd,drive={DISC_DRIVE},bus=ide.1,id=dvd-drive"));
        }
        // The install stick, named as a stick plugged in later is, so that
        // pulling it works the same way.
        if let Some((number, overlay)) = &self.boot_stick {
            args.push("-blockdev".into());
            args.push(format!(
                "driver=qcow2,node-name=stick{number},file.driver=file,file.filename={}",
                option_value(overlay)
            ));
            args.push("-device".into());
            args.push(format!(
                "usb-storage,bus=xhci.0,drive=stick{number},id=usb-stick{number},removable=on,bootindex=0"
            ));
        }
        args
    }

    /// Starts QEMU for the current boot and connects to its QMP socket.
    fn power_on(&mut self) -> Result<(), String> {
        if self.is_on() {
            return Err("the machine is already on".into());
        }
        let socket = self.qmp_socket();
        let _ = fs::remove_file(&socket);
        let arguments = self.arguments();
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.qemu_log())
            .map_err(|err| format!("opening {}: {err}", self.qemu_log().display()))?;
        let _ = writeln!(log, "Starting: {QEMU} {}", arguments.join(" "));
        let mut child = Command::new(QEMU)
            .args(&arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .map_err(|err| format!("starting {QEMU}: {err}"))?;
        let qmp = match Qmp::connect(&socket, Duration::from_secs(10)) {
            Ok(qmp) => qmp,
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{err}; QEMU's messages are in {}", self.qemu_log().display()));
            }
        };
        self.running = Some(Running { child, qmp });
        // A stick plugged in while it ran is gone with the old QEMU; the
        // install stick, given on its command line, is still in. So is a
        // card, and the new reader is empty: a card in the old one counts
        // as taken out.
        self.stick = self.boot_stick.as_ref().map(|(number, _)| *number);
        if let Some(card) = self.card.take() {
            self.forget_card_out();
            self.card_out = Some(card);
        }
        if let Some(disc) = self.disc.take() {
            self.disc_out = Some(disc);
        }
        Ok(())
    }

    /// The QMP connection, if the machine is on.
    fn qmp(&mut self) -> Result<&mut Qmp, String> {
        if !self.is_on() {
            return Err("the machine is off".into());
        }
        Ok(&mut self.running.as_mut().expect("is_on said so").qmp)
    }

    /// Boots the Linux twin: Linux from `kit` on the machine's processor,
    /// CPUs and memory, with no disk, network card or firmware of its own,
    /// the kernel booted directly. Its console goes to `log`.
    fn boot_twin(&self, kit: &Kit, log: &Path, deadline: Instant) -> Result<String, String> {
        File::create(log).map_err(|err| format!("making {}: {err}", log.display()))?;
        let (accel, cpu) = if self.kvm { ("kvm", "host") } else { ("tcg", "max") };
        let arguments: Vec<String> = vec![
            "-M".into(),
            "q35".into(),
            "-accel".into(),
            accel.into(),
            "-cpu".into(),
            cpu.into(),
            "-smp".into(),
            self.settings.cpus.to_string(),
            "-m".into(),
            self.settings.memory.clone(),
            // Only what Linux needs: no network card, screen or monitor.
            "-nodefaults".into(),
            "-display".into(),
            "none".into(),
            "-no-reboot".into(),
            "-chardev".into(),
            format!("file,id=twin,path={}", option_value(log)),
            "-serial".into(),
            "chardev:twin".into(),
            "-kernel".into(),
            kit.kernel.display().to_string(),
            "-initrd".into(),
            kit.initrd.display().to_string(),
            "-append".into(),
            kit.cmdline.clone(),
        ];
        let mut messages = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.qemu_log())
            .map_err(|err| format!("opening {}: {err}", self.qemu_log().display()))?;
        let _ = writeln!(messages, "Starting the Linux twin: {QEMU} {}", arguments.join(" "));
        let mut child = Command::new(QEMU)
            .args(&arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(messages)
            .spawn()
            .map_err(|err| format!("starting {QEMU} for the Linux twin: {err}"))?;
        // Linux powers itself off once its benchmarks are done; the done
        // line is the sign they all ran.
        let outcome = loop {
            let text = fs::read_to_string(log).unwrap_or_default();
            if text.contains(&kit.done) {
                break Ok(text);
            }
            if let Ok(Some(status)) = child.try_wait() {
                break Err(format!(
                    "Linux ended ({status}) before it printed \"{}\"; its console is in {}",
                    kit.done,
                    log.display()
                ));
            }
            if Instant::now() >= deadline {
                break Err(format!(
                    "Linux had not printed \"{}\" at the test's time limit; its console is in {}",
                    kit.done,
                    log.display()
                ));
            }
            if self.setup.stop.requested() {
                break Err("the run was stopped while Linux was measuring".into());
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        // It may still be powering off; nothing it does now counts.
        let _ = child.kill();
        let _ = child.wait();
        outcome
    }

    /// Waits for QEMU to end until `deadline`, or until the run is
    /// stopped; kills it if `kill` and it has not ended.
    fn wait_for_exit(&mut self, deadline: Instant, kill: bool) -> bool {
        while Instant::now() < deadline {
            if !self.is_on() {
                return true;
            }
            if !kill && self.setup.stop.requested() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if kill && let Some(mut running) = self.running.take() {
            let _ = running.child.kill();
            let _ = running.child.wait();
            return true;
        }
        !self.is_on()
    }
}

impl Qemu {
    /// Starts a new boot: the old boot's files go, and this boot's
    /// firmware variables, serial log and SSH port are made; the disk is
    /// the caller's to make.
    fn next_boot(&mut self) -> Result<(), String> {
        self.shut_off();
        // The last boot's disk and stick are not needed again; images are
        // large.
        let _ = fs::remove_file(self.disk());
        if let Some((_, overlay)) = self.boot_stick.take() {
            let _ = fs::remove_file(overlay);
        }
        self.stick = None;
        self.boot += 1;
        if let Some((_, template)) = &self.ovmf {
            fs::copy(template, self.variables()).map_err(|err| {
                format!("copying OVMF's variables from {}: {err}", template.display())
            })?;
        }
        self.serial = self.setup.logs.join(format!("serial-{}.log", self.boot));
        File::create(&self.serial)
            .map_err(|err| format!("making {}: {err}", self.serial.display()))?;
        self.ssh_port = free_port()?;
        // A new QEMU has none of the last boot's forwarded ports.
        self.forwards.clear();
        // The attacker is of the last boot; a new boot gets its own.
        self.stop_attacker();
        // An armed boot opens the private network's port for the attacker
        // to connect to; the port is picked now so it is on the command line.
        self.segment_port = if self.want_attacker { free_port()? } else { 0 };
        Ok(())
    }

    /// The attacker's disk overlay, serial log and QMP socket.
    fn attacker_disk(&self) -> PathBuf {
        self.setup.work.join(format!("attacker-disk-{}.qcow2", self.boot))
    }

    fn attacker_serial(&self) -> PathBuf {
        self.setup.logs.join(format!("attacker-serial-{}.log", self.boot))
    }

    fn attacker_qmp_socket(&self) -> PathBuf {
        self.setup.sockets.join("attacker-qmp.sock")
    }

    /// The attacker's QEMU arguments: like the machine under test, but
    /// with no firmware, pointer or stick of its own, its own forwarded
    /// SSH port, and its private-network port connecting to the machine's.
    fn attacker_arguments(&self, ssh_port: u16) -> Vec<String> {
        let (accel, cpu) = if self.kvm { ("kvm", "host") } else { ("tcg", "max") };
        vec![
            "-M".into(),
            "q35".into(),
            "-accel".into(),
            accel.into(),
            "-cpu".into(),
            cpu.into(),
            "-smp".into(),
            self.settings.cpus.to_string(),
            "-m".into(),
            self.settings.memory.clone(),
            "-display".into(),
            "none".into(),
            "-monitor".into(),
            "none".into(),
            "-chardev".into(),
            format!("file,id=serial0,path={},append=on", option_value(&self.attacker_serial())),
            "-serial".into(),
            "chardev:serial0".into(),
            "-qmp".into(),
            format!("unix:{},server=on,wait=off", option_value(&self.attacker_qmp_socket())),
            "-drive".into(),
            format!("if=none,id=disk,format=qcow2,file={}", option_value(&self.attacker_disk())),
            "-device".into(),
            format!(
                "{},drive=disk,bootindex=0",
                match self.settings.disk {
                    DiskBus::Sata => "ide-hd",
                    DiskBus::Nvme => "nvme",
                    DiskBus::Virtio => "virtio-blk-pci",
                }
            ),
            // Its own way in, over user networking as the machine's is.
            "-netdev".into(),
            format!("user,id={NETDEV},hostfwd=tcp:127.0.0.1:{ssh_port}-:22"),
            "-device".into(),
            format!("{},netdev={NETDEV}", self.settings.nic),
            // The private network: it connects to the machine's open port.
            "-netdev".into(),
            format!("socket,id={SEGMENT},connect=127.0.0.1:{}", self.segment_port),
            "-device".into(),
            format!("{},netdev={SEGMENT},mac={ATTACKER_MAC}", self.settings.nic),
        ]
    }

    /// How SSH reaches the attacker on `ssh_port`.
    fn attacker_target(&self, ssh_port: u16, key: PathBuf) -> ssh::Target {
        ssh::Target {
            host: "127.0.0.1".into(),
            port: ssh_port,
            key,
            known_hosts: self.setup.work.join(format!("attacker-known_hosts-{}", self.boot)),
            scratch: self.setup.work.join("attacker-ssh-output"),
        }
    }

    /// Turns the attacker off and lets go of it, if it is running.
    fn stop_attacker(&mut self) {
        if let Some(mut attacker) = self.attacker.take() {
            let _ = attacker.running.qmp.execute("quit", &json!({}));
            let deadline = Instant::now() + QUIT_TIME;
            while Instant::now() < deadline {
                if matches!(attacker.running.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = attacker.running.child.kill();
            let _ = attacker.running.child.wait();
        }
    }
}

impl Qemu {
    /// The file that memory card `number` holds.
    fn card_file(&self, number: u32) -> PathBuf {
        self.setup.work.join(format!("card-{number}.img"))
    }

    /// Puts memory card `number`, whose file is made, in the card reader.
    fn put_card_in(&mut self, number: u32) -> Result<(), String> {
        let file = self.card_file(number);
        self.qmp()?.execute(
            "blockdev-change-medium",
            &json!({ "device": CARD_DRIVE, "filename": file, "format": "raw" }),
        )?;
        self.card = Some(number);
        Ok(())
    }

    /// Puts a disc holding `image` in the optical drive, read-only.
    fn put_disc_in(&mut self, image: &Path) -> Result<(), String> {
        if !self.want_disc_drive {
            return Err("this boot has no optical drive".into());
        }
        if let Some(disc) = &self.disc {
            return Err(format!(
                "a disc holding {} is still in the optical drive; take it out first",
                disc.display()
            ));
        }
        self.qmp()?.execute(
            "blockdev-change-medium",
            &json!({
                "device": DISC_DRIVE,
                "filename": image,
                "format": "raw",
                "read-only-mode": "read-only",
            }),
        )?;
        self.disc = Some(image.to_path_buf());
        Ok(())
    }

    /// Deletes the file of the card taken out last, which will not go
    /// back in.
    fn forget_card_out(&mut self) {
        if let Some(number) = self.card_out.take() {
            let _ = fs::remove_file(self.card_file(number));
        }
    }
}

impl Driver for Qemu {
    fn boot_fresh(&mut self) -> Result<(), String> {
        self.next_boot()?;
        self.make_disk()?;
        self.power_on()
    }

    fn boot_stick(&mut self, stick: &Path, disk: u64) -> Result<(), String> {
        self.next_boot()?;
        // A blank disk for the installer, which takes room only as it is
        // written, and the stick as an overlay over its image, so the
        // image stays as it is whatever the live system writes to it.
        let blank = self.disk();
        Self::qemu_img(
            &["-f", "qcow2", &blank.to_string_lossy(), &disk.to_string()],
            "the installer's blank disk",
        )?;
        let image = fs::canonicalize(stick)
            .map_err(|err| format!("the install stick {}: {err}", stick.display()))?;
        self.sticks += 1;
        let number = self.sticks;
        let overlay = self.setup.work.join(format!("stick-{number}.qcow2"));
        Self::qemu_img(
            &[
                "-f",
                "qcow2",
                "-F",
                "raw",
                "-b",
                &image.to_string_lossy(),
                &overlay.to_string_lossy(),
            ],
            "the install stick's overlay",
        )?;
        self.boot_stick = Some((number, overlay));
        self.power_on()
    }

    fn power(&mut self, action: PowerAction, deadline: Instant) -> Result<(), String> {
        match action {
            PowerAction::On => self.power_on(),
            PowerAction::Button => self.qmp()?.execute("system_powerdown", &json!({})).map(drop),
            PowerAction::Reset => self.qmp()?.execute("system_reset", &json!({})).map(drop),
            PowerAction::Off => {
                self.qmp()?.execute("system_powerdown", &json!({}))?;
                if self.wait_for_exit(deadline, false) {
                    Ok(())
                } else {
                    Err("the machine was still on at the test's time limit after the power button"
                        .into())
                }
            }
            PowerAction::Cut => {
                // `quit` ends QEMU at once: the guest gets no warning, as
                // when a cord is pulled.
                let _ = self.qmp()?.execute("quit", &json!({}));
                self.wait_for_exit(Instant::now() + QUIT_TIME, true);
                Ok(())
            }
        }
    }

    fn is_on(&mut self) -> bool {
        let Some(running) = &mut self.running else { return false };
        match running.child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                let _ = OpenOptions::new()
                    .append(true)
                    .open(self.qemu_log())
                    .and_then(|mut log| writeln!(log, "QEMU ended: {status}"));
                self.running = None;
                false
            }
            Err(_) => false,
        }
    }

    fn serial_log(&self) -> &Path {
        &self.serial
    }

    fn set_link(&mut self, up: bool) -> Result<(), String> {
        self.qmp()?.execute("set_link", &json!({ "name": NETDEV, "up": up })).map(drop)
    }

    fn plug_usb(&mut self, stick: &Stick) -> Result<(), String> {
        if let Some(stick) = self.stick {
            return Err(format!("USB stick {stick} is still plugged in; pull it first"));
        }
        self.qmp()?;
        // Each stick gets a new number, even when plugging one in fails
        // halfway, so its QMP names are never taken.
        self.sticks += 1;
        let number = self.sticks;
        // A copy, so the test's own file stays as it is whatever the guest
        // writes; or zeros, as a stick fresh from the shop holds.
        let copy = self.setup.work.join(format!("stick-{number}.img"));
        make_medium(stick, &copy)?;
        let node = format!("stick{number}");
        let qmp = self.qmp()?;
        qmp.execute(
            "blockdev-add",
            &json!({ "driver": "raw", "node-name": node, "file": { "driver": "file", "filename": copy } }),
        )?;
        qmp.execute(
            "device_add",
            &json!({ "driver": "usb-storage", "bus": "xhci.0", "drive": node, "id": format!("usb-{node}"), "removable": true }),
        )?;
        self.stick = Some(number);
        Ok(())
    }

    fn unplug_usb(&mut self) -> Result<(), String> {
        let number = self.stick.ok_or("no USB stick is plugged in")?;
        let node = format!("stick{number}");
        let device = format!("usb-{node}");
        let qmp = self.qmp()?;
        qmp.execute("device_del", &json!({ "id": device }))?;
        // The stick's drive can go only once the guest has let go of the device.
        qmp.wait_event("DEVICE_DELETED", &json!({ "device": device }), Duration::from_secs(10))?;
        qmp.execute("blockdev-del", &json!({ "node-name": node }))?;
        self.stick = None;
        let _ = fs::remove_file(self.setup.work.join(format!("stick-{number}.img")));
        // The install stick is out for good: QEMU starts again without it.
        if self.boot_stick.as_ref().is_some_and(|(boot_stick, _)| *boot_stick == number)
            && let Some((_, overlay)) = self.boot_stick.take()
        {
            let _ = fs::remove_file(overlay);
        }
        Ok(())
    }

    fn want_card_reader(&mut self, yes: bool) {
        self.want_card_reader = yes;
    }

    fn insert_card(&mut self, card: &Stick) -> Result<(), String> {
        if !self.want_card_reader {
            return Err("this boot has no card reader".into());
        }
        if let Some(card) = self.card {
            return Err(format!(
                "memory card {card} is still in the card reader; take it out first"
            ));
        }
        // Running, before a file is made for the card.
        self.qmp()?;
        self.cards += 1;
        let number = self.cards;
        make_medium(card, &self.card_file(number))?;
        self.put_card_in(number)
    }

    fn remove_card(&mut self) -> Result<(), String> {
        let number = self.card.ok_or("no memory card is in the card reader")?;
        // By force: a card can be pulled out whether or not the machine
        // let it go, and a reader that locks it in only asks the guest.
        self.qmp()?.execute("eject", &json!({ "device": CARD_DRIVE, "force": true }))?;
        self.card = None;
        // Only the last card out is kept, to go back in.
        self.forget_card_out();
        self.card_out = Some(number);
        Ok(())
    }

    fn reinsert_card(&mut self) -> Result<(), String> {
        if !self.want_card_reader {
            return Err("this boot has no card reader".into());
        }
        if let Some(card) = self.card {
            return Err(format!(
                "memory card {card} is still in the card reader; take it out first"
            ));
        }
        let number = self.card_out.ok_or("no memory card was taken out to put back")?;
        self.put_card_in(number)?;
        self.card_out = None;
        Ok(())
    }

    fn want_disc_drive(&mut self, yes: bool) {
        self.want_disc_drive = yes;
    }

    fn insert_disc(&mut self, image: &Path) -> Result<(), String> {
        self.put_disc_in(image)
    }

    fn eject_disc(&mut self) -> Result<(), String> {
        let disc = self.disc.take().ok_or("no disc is in the optical drive")?;
        // By force: the tray opens whether or not the machine locked the
        // disc in, as a paper clip in the drive's hole opens it.
        if let Err(err) = self
            .qmp()
            .and_then(|qmp| qmp.execute("eject", &json!({ "device": DISC_DRIVE, "force": true })))
        {
            self.disc = Some(disc);
            return Err(err);
        }
        self.disc_out = Some(disc);
        Ok(())
    }

    fn reinsert_disc(&mut self) -> Result<(), String> {
        let disc = self.disc_out.clone().ok_or("no disc was taken out to put back")?;
        self.put_disc_in(&disc)?;
        self.disc_out = None;
        Ok(())
    }

    fn press_disc_button(&mut self) -> Result<(), String> {
        if self.disc.is_none() {
            return Err("no disc is in the optical drive".into());
        }
        // Not by force: QEMU refuses to open a drive the machine locked,
        // and has the drive report an eject request (GET EVENT STATUS
        // NOTIFICATION), as a real drive's button does; the machine then
        // opens the tray itself, and a `disc = "eject"` takes the disc
        // out. A drive the machine did not lock opens at once.
        match self.qmp().and_then(|qmp| qmp.execute("eject", &json!({ "device": DISC_DRIVE }))) {
            Ok(_) => {
                self.disc_out = self.disc.take();
                Ok(())
            }
            Err(err) if err.contains("is locked") => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn press_keys(&mut self, combos: &[String]) -> Result<(), String> {
        for combo in combos {
            let keys: Vec<_> =
                combo.split('-').map(|key| json!({ "type": "qcode", "data": key })).collect();
            self.qmp()?.execute("send-key", &json!({ "keys": keys }))?;
            // A person does not press keys faster than this, and the
            // guest's keyboard buffer is small.
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }

    fn point(&mut self, x: u32, y: u32) -> Result<(), String> {
        // QEMU takes where a tablet points as 0 to ABS_MAX on each axis,
        // the whole screen whatever its size, so the pixel is scaled by
        // the size the screen has now.
        let dump = self.setup.work.join("pointer-screen.ppm");
        self.screen_dump(&dump)?;
        let bytes = fs::read(&dump).map_err(|err| format!("{}: {err}", dump.display()))?;
        let _ = fs::remove_file(&dump);
        let screen = Image::parse_ppm(&bytes)?;
        let (x, y) = (x as usize, y as usize);
        if x >= screen.width || y >= screen.height {
            return Err(format!(
                "({x}, {y}) is off the screen, which is {}x{} pixels",
                screen.width, screen.height
            ));
        }
        let scale = |at: usize, size: usize| (at * ABS_MAX / (size - 1).max(1)) as u64;
        let events = json!({ "events": [
            { "type": "abs", "data": { "axis": "x", "value": scale(x, screen.width) } },
            { "type": "abs", "data": { "axis": "y", "value": scale(y, screen.height) } },
        ] });
        self.qmp()?.execute("input-send-event", &events).map(drop)
    }

    fn click(&mut self, button: Button) -> Result<(), String> {
        for down in [true, false] {
            let event = json!({ "type": "btn", "data": { "down": down, "button": button.name() } });
            self.qmp()?.execute("input-send-event", &json!({ "events": [event] }))?;
            // As long as a person holds a button: long enough for the
            // pointer to report it down before it goes up.
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }

    fn screenshot(&mut self, to: &Path) -> Result<(), String> {
        self.qmp()?.execute("screendump", &json!({ "filename": to, "format": "png" })).map(drop)
    }

    fn screen_dump(&mut self, to: &Path) -> Result<(), String> {
        // PPM, screendump's own format, which the lab reads without a PNG
        // decoder.
        self.qmp()?.execute("screendump", &json!({ "filename": to, "format": "ppm" })).map(drop)
    }

    fn run_command(
        &mut self,
        command: &str,
        input: Option<&str>,
        deadline: Instant,
    ) -> Result<ssh::Output, String> {
        let key = self
            .setup
            .key
            .clone()
            .ok_or("the lab has no key: run `dukovo-lab key`, then build the image again")?;
        if !self.is_on() {
            return Err("the machine is off".into());
        }
        let target = ssh::Target {
            host: "127.0.0.1".into(),
            port: self.ssh_port,
            key,
            known_hosts: self.setup.work.join(format!("known_hosts-{}", self.boot)),
            scratch: self.setup.work.join("ssh-output"),
        };
        target.run(command, input, deadline, &self.setup.stop)
    }

    fn want_attacker(&mut self, yes: bool) {
        self.want_attacker = yes;
    }

    fn start_attacker(&mut self, deadline: Instant) -> Result<(), String> {
        if self.attacker.as_mut().is_some_and(|a| matches!(a.running.child.try_wait(), Ok(None))) {
            return Ok(());
        }
        if self.segment_port == 0 {
            return Err(
                "the machine was not booted for an attack (its private network is off)".into()
            );
        }
        let key = self
            .setup
            .key
            .clone()
            .ok_or("the lab has no key: run `dukovo-lab key`, then build the image again")?;
        // A disk of the attacker's own over the same image.
        let image = fs::canonicalize(&self.setup.image)
            .map_err(|err| format!("the image {}: {err}", self.setup.image.display()))?;
        let format =
            if image.extension().is_some_and(|ext| ext == "qcow2") { "qcow2" } else { "raw" };
        Self::qemu_img(
            &[
                "-f",
                "qcow2",
                "-F",
                format,
                "-b",
                &image.to_string_lossy(),
                &self.attacker_disk().to_string_lossy(),
            ],
            "the attacker's disk",
        )?;
        File::create(self.attacker_serial())
            .map_err(|err| format!("making {}: {err}", self.attacker_serial().display()))?;
        let socket = self.attacker_qmp_socket();
        let _ = fs::remove_file(&socket);
        let ssh_port = free_port()?;
        let arguments = self.attacker_arguments(ssh_port);
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.qemu_log())
            .map_err(|err| format!("opening {}: {err}", self.qemu_log().display()))?;
        let _ = writeln!(log, "Starting the attacker: {QEMU} {}", arguments.join(" "));
        let mut child = Command::new(QEMU)
            .args(&arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .map_err(|err| format!("starting {QEMU} for the attacker: {err}"))?;
        let qmp = match Qmp::connect(&socket, Duration::from_secs(10)) {
            Ok(qmp) => qmp,
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "the attacker's {err}; QEMU's messages are in {}",
                    self.qemu_log().display()
                ));
            }
        };
        self.attacker = Some(Attacker { running: Running { child, qmp }, ssh_port });
        // Wait until the attacker answers SSH, so commands can run on it.
        let target = self.attacker_target(ssh_port, key);
        loop {
            if self.setup.stop.requested() {
                return Err("the run was stopped before the attacker was ready".into());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "the attacker did not answer SSH in time; its console is in {}",
                    self.attacker_serial().display()
                ));
            }
            let soon = (Instant::now() + Duration::from_secs(5)).min(deadline);
            if let Ok(output) = target.run("true", None, soon, &self.setup.stop)
                && output.status == Some(0)
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    fn attack(
        &mut self,
        command: &str,
        input: Option<&str>,
        deadline: Instant,
    ) -> Result<ssh::Output, String> {
        let key = self
            .setup
            .key
            .clone()
            .ok_or("the lab has no key: run `dukovo-lab key`, then build the image again")?;
        let ssh_port = {
            let Some(attacker) = &mut self.attacker else {
                return Err("the attacker is not up; it is started on the first attack step".into());
            };
            if !matches!(attacker.running.child.try_wait(), Ok(None)) {
                return Err("the attacker turned off".into());
            }
            attacker.ssh_port
        };
        let target = self.attacker_target(ssh_port, key);
        target.run(command, input, deadline, &self.setup.stop)
    }

    fn peer_address(&self) -> Option<String> {
        Some(HOST_ON_USER_NET.into())
    }

    fn machine_address(&self) -> Option<String> {
        Some("127.0.0.1".into())
    }

    fn reach(&mut self, port: u16, udp: bool) -> Result<u16, String> {
        if let Some(&forwarded) = self.forwards.get(&(udp, port)) {
            return Ok(forwarded);
        }
        let (protocol, host_port) =
            if udp { ("udp", crate::peer::free_udp_port()?) } else { ("tcp", free_port()?) };
        // QMP has no command of its own for this; the human monitor's,
        // through QMP, says nothing when it works and why when it does not.
        let rule = format!("{protocol}:127.0.0.1:{host_port}-:{port}");
        let said = self.qmp()?.execute(
            "human-monitor-command",
            &json!({ "command-line": format!("hostfwd_add {NETDEV} {rule}") }),
        )?;
        let said = said.as_str().unwrap_or_default().trim();
        if !said.is_empty() {
            return Err(format!("forwarding the guest's port: hostfwd_add {rule}: {said}"));
        }
        self.forwards.insert((udp, port), host_port);
        Ok(host_port)
    }

    fn slowdown(&self) -> u32 {
        if self.kvm { 1 } else { self.settings.tcg_slowdown }
    }

    fn linux_twin(&mut self, kit: &Kit, log: &Path, deadline: Instant) -> Result<String, String> {
        // The machine under test is paused rather than turned off, so the
        // test goes on where it was once Linux is done.
        let paused = self.is_on();
        if paused {
            self.qmp()?.execute("stop", &json!({}))?;
        }
        let measured = self.boot_twin(kit, log, deadline);
        if paused && self.is_on() {
            let resumed = self.qmp().and_then(|qmp| qmp.execute("cont", &json!({})));
            if let Err(err) = resumed {
                return Err(format!("the machine would not go on after Linux measured: {err}"));
            }
        }
        measured
    }

    fn emulated(&self) -> bool {
        !self.kvm
    }

    fn shut_off(&mut self) {
        self.stop_attacker();
        if self.is_on() {
            if let Some(running) = &mut self.running {
                let _ = running.qmp.execute("quit", &json!({}));
            }
            self.wait_for_exit(Instant::now() + QUIT_TIME, true);
        }
    }
}

impl Drop for Qemu {
    fn drop(&mut self) {
        self.shut_off();
    }
}

/// Whether this user can open /dev/kvm.
fn kvm_opens() -> bool {
    OpenOptions::new().read(true).write(true).open("/dev/kvm").is_ok()
}

/// OVMF's code and variable store, from the environment or where
/// distributions put them.
fn find_ovmf() -> Option<(PathBuf, PathBuf)> {
    if let (Some(code), Some(vars)) =
        (std::env::var_os("DUKOVO_LAB_OVMF_CODE"), std::env::var_os("DUKOVO_LAB_OVMF_VARS"))
    {
        return Some((code.into(), vars.into()));
    }
    OVMF_PATHS
        .iter()
        .map(|&(code, vars)| (PathBuf::from(code), PathBuf::from(vars)))
        .find(|(code, vars)| code.is_file() && vars.is_file())
}

/// A TCP port on 127.0.0.1 that nothing listens on now.
fn free_port() -> Result<u16, String> {
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|err| format!("finding a free port: {err}"))?;
    listener.local_addr().map(|addr| addr.port()).map_err(|err| err.to_string())
}

/// `path` as a QEMU option's value, where a comma ends the value unless
/// it is doubled.
fn option_value(path: &Path) -> String {
    path.display().to_string().replace(',', ",,")
}

/// Makes what a stick or card holds at `copy`: a copy of its image, so
/// the test's own file stays as it is whatever the guest writes, or
/// zeros, as one fresh from the shop holds.
fn make_medium(medium: &Stick, copy: &Path) -> Result<(), String> {
    match medium {
        Stick::Image(image) => fs::copy(image, copy)
            .map(drop)
            .map_err(|err| format!("copying {}: {err}", image.display())),
        Stick::Blank(bytes) => File::create(copy)
            .and_then(|file| file.set_len(*bytes))
            .map_err(|err| format!("making {}: {err}", copy.display())),
    }
}

/// These start a real QEMU, with a blank disk and no operating system, and
/// drive it through every control. They run when DUKOVO_LAB_QEMU_TESTS=1,
/// as CI sets, since they need QEMU installed.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::machines::Firmware;
    use crate::testing::TempDir;

    fn wanted() -> bool {
        std::env::var_os("DUKOVO_LAB_QEMU_TESTS").is_some_and(|value| value == "1")
    }

    fn start(dir: &TempDir, firmware: Firmware) -> Qemu {
        let image = dir.path().join("blank.img");
        File::create(&image).unwrap().set_len(64 << 20).unwrap();
        let settings =
            QemuSettings { firmware, memory: "256M".into(), cpus: 1, ..QemuSettings::default() };
        let setup = Setup {
            image,
            logs: dir.path().join("logs"),
            work: dir.path().join("work"),
            sockets: dir.path().join("s"),
            key: None,
            stop: crate::stop::Stop::default(),
        };
        Qemu::new(settings, setup).unwrap()
    }

    fn drive_every_control(firmware: Firmware) {
        let dir = TempDir::new("qemu");
        let mut qemu = start(&dir, firmware);
        let soon = || Instant::now() + Duration::from_secs(30);
        qemu.boot_fresh().unwrap();
        assert!(qemu.is_on());
        assert_eq!(qemu.serial_log(), dir.path().join("logs/serial-1.log"));
        assert!(dir.path().join("work/disk-1.qcow2").is_file());

        qemu.set_link(false).unwrap();
        qemu.set_link(true).unwrap();

        // A guest's port, forwarded to the lab PC once a boot.
        let forwarded = qemu.reach(445, false).unwrap();
        assert_eq!(qemu.reach(445, false).unwrap(), forwarded);
        assert_ne!(qemu.reach(51820, true).unwrap(), 0);

        let image = dir.write("stick.img", "");
        File::options().write(true).open(&image).unwrap().set_len(1 << 20).unwrap();
        let stick = Stick::Image(image);
        qemu.plug_usb(&stick).unwrap();
        assert!(qemu.plug_usb(&stick).unwrap_err().contains("still plugged in"));
        qemu.unplug_usb().unwrap();
        // A blank one: zeros of its size, made where the copies go.
        qemu.plug_usb(&Stick::Blank(2 << 20)).unwrap();
        let blank = dir.path().join("work/stick-2.img");
        assert_eq!(fs::metadata(&blank).unwrap().len(), 2 << 20);
        qemu.unplug_usb().unwrap();
        assert!(!blank.exists(), "a pulled stick's file goes");
        qemu.plug_usb(&stick).unwrap();

        qemu.press_keys(&["shift-a".into(), "ret".into()]).unwrap();
        let shot = dir.path().join("screen.png");
        qemu.screenshot(&shot).unwrap();
        assert_eq!(&fs::read(&shot).unwrap()[..4], b"\x89PNG");
        let dump = dir.path().join("screen.ppm");
        qemu.screen_dump(&dump).unwrap();
        let screen = Image::parse_ppm(&fs::read(&dump).unwrap()).unwrap();
        assert!(screen.width > 0 && screen.height > 0);

        // The pointer: anywhere on the screen, never past its edge. OVMF
        // may change the screen's size as it starts, so each check takes
        // the size the screen has just before it.
        let size = |qemu: &mut Qemu| {
            qemu.screen_dump(&dump).unwrap();
            let screen = Image::parse_ppm(&fs::read(&dump).unwrap()).unwrap();
            (screen.width as u32, screen.height as u32)
        };
        qemu.point(0, 0).unwrap();
        let (width, height) = size(&mut qemu);
        qemu.point(width - 1, height - 1).unwrap();
        // Tried again should the screen grow between its size and the point.
        let off = (0..5)
            .find_map(|_| {
                let (width, _) = size(&mut qemu);
                qemu.point(width, 0).err()
            })
            .expect("a point past the screen's edge is refused");
        assert!(off.contains("off the screen"), "{off}");
        qemu.click(Button::Left).unwrap();
        qemu.click(Button::Right).unwrap();

        let error = qemu.run_command("true", None, soon()).unwrap_err();
        assert!(error.contains("no key"), "{error}");

        qemu.power(PowerAction::Reset, soon()).unwrap();
        assert!(qemu.is_on());
        qemu.power(PowerAction::Cut, soon()).unwrap();
        assert!(!qemu.is_on());
        assert!(qemu.set_link(true).unwrap_err().contains("off"));

        // On again: the same boot, its disk and serial log kept.
        qemu.power(PowerAction::On, soon()).unwrap();
        assert!(qemu.is_on());
        assert!(qemu.power(PowerAction::On, soon()).unwrap_err().contains("already on"));
        // A card reader, on a boot that asks for one: cards go in and come
        // out, by force, while the reader stays.
        qemu.want_card_reader(true);
        qemu.boot_fresh().unwrap();
        assert!(!dir.path().join("work/disk-1.qcow2").exists(), "the old boot's disk is gone");
        assert!(qemu.remove_card().unwrap_err().contains("no memory card"));
        qemu.insert_card(&stick).unwrap();
        assert!(qemu.insert_card(&stick).unwrap_err().contains("still in the card reader"));
        qemu.remove_card().unwrap();
        qemu.insert_card(&Stick::Blank(2 << 20)).unwrap();
        let card = dir.path().join("work/card-2.img");
        assert_eq!(fs::metadata(&card).unwrap().len(), 2 << 20);
        qemu.remove_card().unwrap();
        // Put back, it holds what it held.
        File::options().write(true).open(&card).unwrap().write_all(b"written").unwrap();
        qemu.reinsert_card().unwrap();
        assert!(qemu.reinsert_card().unwrap_err().contains("still in the card reader"));
        qemu.remove_card().unwrap();
        assert_eq!(&fs::read(&card).unwrap()[..7], b"written");
        qemu.insert_card(&Stick::Blank(1 << 20)).unwrap();
        qemu.remove_card().unwrap();
        assert!(!card.exists(), "only the card taken out last is kept");
        qemu.want_card_reader(false);
        // An optical drive, on a boot that asks for one: a disc goes in,
        // is ejected by force, and goes back in; its file is never
        // written.
        let disc = dir.write("disc.iso", "");
        File::options().write(true).open(&disc).unwrap().set_len(1 << 20).unwrap();
        assert!(qemu.insert_disc(&disc).unwrap_err().contains("no optical drive"));
        qemu.want_disc_drive(true);
        qemu.boot_fresh().unwrap();
        assert!(qemu.eject_disc().unwrap_err().contains("no disc"));
        assert!(qemu.reinsert_disc().unwrap_err().contains("no disc was taken out"));
        qemu.insert_disc(&disc).unwrap();
        assert!(qemu.insert_disc(&disc).unwrap_err().contains("still in the optical drive"));
        qemu.eject_disc().unwrap();
        qemu.reinsert_disc().unwrap();
        // Nothing locked it in: its button opens the tray.
        qemu.press_disc_button().unwrap();
        assert!(qemu.press_disc_button().unwrap_err().contains("no disc"));
        qemu.reinsert_disc().unwrap();
        qemu.eject_disc().unwrap();
        assert_eq!(fs::metadata(&disc).unwrap().len(), 1 << 20);
        qemu.want_disc_drive(false);

        // The install stick, with a blank disk of the size asked for; the
        // stick's own file is never written.
        let install = dir.write("install.img", "");
        File::options().write(true).open(&install).unwrap().set_len(1 << 20).unwrap();
        qemu.boot_stick(&install, 1 << 30).unwrap();
        assert!(qemu.is_on());
        assert!(dir.path().join("work/disk-4.qcow2").is_file());
        assert!(qemu.unplug_usb().is_ok(), "the install stick pulls as any stick does");
        assert_eq!(fs::metadata(&install).unwrap().len(), 1 << 20);
        qemu.shut_off();
        assert!(!qemu.is_on());
        let log = fs::read_to_string(dir.path().join("logs/qemu-1.log")).unwrap();
        assert!(log.starts_with("Starting: qemu-system-x86_64 -M q35"), "{log}");
    }

    #[test]
    fn drives_a_bios_machine() {
        if wanted() {
            drive_every_control(Firmware::Bios);
        }
    }

    #[test]
    fn drives_a_uefi_machine() {
        if wanted() {
            drive_every_control(Firmware::Uefi);
        }
    }
}

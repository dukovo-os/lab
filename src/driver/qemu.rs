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
//! controller, keys to `send-key`, the screen to `screendump`.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

use super::{Driver, Setup};
use crate::machines::{DiskBus, Firmware, Kvm, QemuSettings};
use crate::qmp::Qmp;
use crate::ssh;
use crate::suite::{PowerAction, Stick};

/// The QEMU binary.
const QEMU: &str = "qemu-system-x86_64";

/// The network device's id, which `set_link` names.
const NETDEV: &str = "lab0";

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

    /// Makes this boot's disk: an overlay whose reads fall through to the
    /// image and whose writes stay in the overlay.
    fn make_disk(&self) -> Result<(), String> {
        let image = fs::canonicalize(&self.setup.image)
            .map_err(|err| format!("the image {}: {err}", self.setup.image.display()))?;
        let format =
            if image.extension().is_some_and(|ext| ext == "qcow2") { "qcow2" } else { "raw" };
        let output = Command::new("qemu-img")
            .args(["create", "-q", "-f", "qcow2", "-F", format, "-b"])
            .arg(&image)
            .arg(self.disk())
            .output()
            .map_err(|err| format!("qemu-img will not start ({err}); install QEMU's tools (Mint: apt install qemu-utils)"))?;
        if !output.status.success() {
            return Err(format!(
                "qemu-img could not make a disk over {}: {}",
                image.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
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
        args.push(match settings.disk {
            // q35's own SATA controller is AHCI, as the OptiPlex's in AHCI mode.
            DiskBus::Sata => "ide-hd,drive=disk,bootindex=0".into(),
            DiskBus::Nvme => "nvme,drive=disk,serial=lab-disk,bootindex=0".into(),
            DiskBus::Virtio => "virtio-blk-pci,drive=disk,bootindex=0".into(),
        });
        args.push("-netdev".into());
        args.push(format!("user,id={NETDEV},hostfwd=tcp:127.0.0.1:{}-:22", self.ssh_port));
        args.push("-device".into());
        args.push(format!("{},netdev={NETDEV}", settings.nic));
        // USB sticks are plugged into this controller.
        args.push("-device".into());
        args.push("qemu-xhci,id=xhci".into());
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
        self.stick = None;
        Ok(())
    }

    /// The QMP connection, if the machine is on.
    fn qmp(&mut self) -> Result<&mut Qmp, String> {
        if !self.is_on() {
            return Err("the machine is off".into());
        }
        Ok(&mut self.running.as_mut().expect("is_on said so").qmp)
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

impl Driver for Qemu {
    fn boot_fresh(&mut self) -> Result<(), String> {
        self.shut_off();
        // The last boot's disk is not needed again; images are large.
        let _ = fs::remove_file(self.disk());
        self.boot += 1;
        self.make_disk()?;
        if let Some((_, template)) = &self.ovmf {
            fs::copy(template, self.variables()).map_err(|err| {
                format!("copying OVMF's variables from {}: {err}", template.display())
            })?;
        }
        self.serial = self.setup.logs.join(format!("serial-{}.log", self.boot));
        File::create(&self.serial)
            .map_err(|err| format!("making {}: {err}", self.serial.display()))?;
        self.ssh_port = free_port()?;
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
        match stick {
            Stick::Image(image) => fs::copy(image, &copy)
                .map(drop)
                .map_err(|err| format!("copying {}: {err}", image.display()))?,
            Stick::Blank(bytes) => File::create(&copy)
                .and_then(|file| file.set_len(*bytes))
                .map_err(|err| format!("making {}: {err}", copy.display()))?,
        }
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
        Ok(())
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

    fn screenshot(&mut self, to: &Path) -> Result<(), String> {
        self.qmp()?.execute("screendump", &json!({ "filename": to, "format": "png" })).map(drop)
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

    fn slowdown(&self) -> u32 {
        if self.kvm { 1 } else { self.settings.tcg_slowdown }
    }

    fn shut_off(&mut self) {
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
        qemu.boot_fresh().unwrap();
        assert!(!dir.path().join("work/disk-1.qcow2").exists(), "the old boot's disk is gone");
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

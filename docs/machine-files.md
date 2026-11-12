<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Machine files

Each machine the lab runs tests on is one TOML file in
`~/.config/dukovo-lab/machines/` (or `machines/` under `--config`). The
file's name is the machine's name: `optiplex.toml` is the machine
`optiplex`. A machine named `qemu` is built in and needs no file: QEMU on
the lab PC with UEFI, 2 CPUs and 2 GiB. A file named `qemu.toml` replaces
it.

```toml
about = "QEMU with BIOS firmware and an NVMe disk"
driver = "qemu"

[qemu]
firmware = "bios"
disk = "nvme"

[controls]
cord = "person"    # treat pulling the cord as a person's job here
```

[machines/qemu-bios.toml](../machines/qemu-bios.toml) is an example to copy,
and [machines/optiplex.toml](../machines/optiplex.toml) one for a real
machine with Intel AMT (see [Real machines](real-machines.md)).

## Keys

| Key | Required | Meaning |
| --- | --- | --- |
| `about` | yes | What the machine is, in one line. |
| `driver` | yes | How the lab controls it: `qemu`, or `amt` for a real machine with Intel AMT. |
| `ready` | no | The serial console line that says the machine has booted. Default `Boot complete.` |
| `[controls]` | no | Who does each control (below). |
| `[qemu]` | no | A QEMU machine's hardware (below). |
| `[amt]` | for `amt` | How the lab reaches a real machine and boots it (below). |

## Controls

A control is a way the lab acts on a machine: `power`, `cord` (pulling
the power cord), `serial`, `link` (the network cable), `usb` (a USB
stick), `card` (a memory card in a card reader), `sd` (a memory card in
the machine's own SD card slot), `disc` (a disc in an
optical drive), `burner` (a blank disc in an optical drive that writes
them, for a test to burn), `floppy` (a floppy disk in a floppy drive), `cf` (a
CompactFlash card in an IDE adapter, which tests format), `keys`,
`mouse` (moving the pointer and pressing its buttons), `screen`, `ssh`,
`linux` (booting the machine's Linux twin, which performance tests
measure against), `attack` (standing up an attacker on a private network
of the machine's own, which security tests probe it from) and `peer`
(the lab PC as a peer on the machine's network: servers the lab PC runs
that the machine reaches, such as a Kerberos KDC or a file server, and
the machine's ports forwarded to the lab PC, so the lab PC's clients
reach the machine's own servers). Each is done by one of:

| Value | Meaning |
| --- | --- |
| `driver` | The machine's driver does it, with no one at the machine. |
| `person` | A person at the machine does it when the runner asks: tests that need it are Needs you tests on this machine, run only with `--with-person` (or ticked on the runner page), and each such step waits for `dukovo-lab done` or the page's Done button. |
| `none` | It cannot be done here: tests that need it are skipped. |

A machine starts with every control its driver can do (QEMU can do all
of them) set to `driver`. `[controls]` can hand one to a person or take
it away, but cannot make a driver do what it cannot. `linux`, `attack`
and `peer` are `driver` or `none`: a person cannot measure Linux, run the
attacker or stand in for the lab PC's side.

So one test file serves every machine: plugging in a USB stick is
automatic on QEMU, a Needs you step on a real machine.

What each driver does by itself:

| Driver | Controls |
| --- | --- |
| `qemu` | All of them. |
| `amt` | `power`, `cord` (AMT's hard off, which the system gets no warning of), `serial`, `keys`, `mouse` and `screen` (AMT's remote screen), `ssh`, `linux` and `peer`. Cables, sticks, cards and discs (`link`, `usb`, `card`, `sd`, `disc`, `burner`, `floppy`, `cf`) can be handed to a person; `attack` is not done on real machines yet. |

## QEMU hardware

| Key | Default | Meaning |
| --- | --- | --- |
| `firmware` | `uefi` | `uefi` (OVMF) or `bios` (SeaBIOS). |
| `cpus` | `2` | 1 to 64. |
| `memory` | `2G` | A whole number of `M` or `G`. |
| `kvm` | `auto` | `auto` uses KVM when `/dev/kvm` opens; `on` fails without it; `off` never uses it. |
| `disk` | `sata` | `sata` (AHCI, as the OptiPlex in AHCI mode), `nvme` or `virtio`. |
| `nic` | `virtio-net-pci` | The network card, as a QEMU device name such as `e1000e`. |
| `tcg_slowdown` | `4` | How many times longer tests may take without KVM. |

QEMU needs `qemu-system-x86`, `qemu-utils` and, for UEFI, `ovmf` (the
Mint and Ubuntu package names). OVMF is looked for where distributions
install it; `DUKOVO_LAB_OVMF_CODE` and `DUKOVO_LAB_OVMF_VARS` name other
files.

## AMT settings

A machine whose driver is `amt` is a real machine with Intel AMT on the
lab network. [Real machines](real-machines.md) sets one up.

| Key | Default | Meaning |
| --- | --- | --- |
| `address` | (required) | AMT's address on the lab network, as set in the machine's MEBx. |
| `user` | `admin` | AMT's user. |
| `password_file` | (required) | A file holding AMT's password alone, readable by this user alone (`chmod 600`). A relative path is from the machine file's folder. |
| `mac` | (required) | The MAC address of the machine's network card on the lab network, written `aa:bb:cc:dd:ee:ff`. |
| `disk` | (required) | The serial number of the disk each fresh boot writes the image to, as `lsblk -o NAME,SERIAL` shows it. No other disk is ever written. |
| `serial` | (required) | The lab PC's serial port the machine's console is cabled to, such as `/dev/ttyUSB0`. |
| `baud` | `115200` | The serial console's speed. |
| `machine_address` | `10.77.0.2` | The machine's address on the lab network, for SSH and `{machine}`. |
| `lab_address` | `10.77.0.1` | The lab PC's address on the lab network: the net-boot server listens there, and tests reach the lab PC there as `{peer}`. |
| `boot_port` | `7431` | The first of the net-boot server's four ports, which boots take in turn ([Net boot](net-boot.md) says why). Machines in the same run need four ports of their own. |
| `tftp` | `/srv/dukovo-lab/tftp` | dnsmasq's TFTP folder, where boot scripts are written. |
| `writer` | `/srv/dukovo-lab/writer` | The writer's kit, which `ob lab` installs. |
| `console` | `ttyS0,115200` | The machine's serial port as Linux names it, for the writer and the Linux twin. A board's own ports are `ttyS0` to `ttyS3`; PCI ports come after, from `ttyS4`, and with Serial-over-LAN on, AMT's own port is the first of them, so a serial card's first port is then `ttyS5`. |

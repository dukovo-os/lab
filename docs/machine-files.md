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

[machines/qemu-bios.toml](../machines/qemu-bios.toml) is an example to copy.

## Keys

| Key | Required | Meaning |
| --- | --- | --- |
| `about` | yes | What the machine is, in one line. |
| `driver` | yes | How the lab controls it: `qemu`. Real machines through Intel AMT come in lab PR 9. |
| `ready` | no | The serial console line that says the machine has booted. Default `Boot complete.` |
| `[controls]` | no | Who does each control (below). |
| `[qemu]` | no | A QEMU machine's hardware (below). |

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

So one test file serves every machine: pulling the cord is automatic on
QEMU, a Needs you step on a real machine until it has a smart plug.

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

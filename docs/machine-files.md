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
[machines/optiplex.toml](../machines/optiplex.toml) one for a real
machine with Intel AMT, [machines/m75q.toml](../machines/m75q.toml)
one for a real machine on a Shelly smart plug, and
[machines/r740.toml](../machines/r740.toml) one for a server with a BMC
(see [Real machines](real-machines.md)). [machines/optiplex-plug.toml](../machines/optiplex-plug.toml)
is the same with an HDMI capture stick, a CH9329 cable and a serial cable.

## Keys

| Key | Required | Meaning |
| --- | --- | --- |
| `about` | yes | What the machine is, in one line. |
| `driver` | yes | How the lab controls it: `qemu`; `amt` for a real machine with Intel AMT; `plug` for a real machine without AMT, powered through a Shelly smart plug, with optional cables to the lab PC for its console, screen, keyboard and mouse; or `redfish` for a server with a BMC (a Dell iDRAC9, as a rule) driven over Redfish and IPMI. |
| `pool` | no | The pool the machine belongs to (below). |
| `spare` | no | `true` keeps the machine out of a run on its pool, which uses it to run failed tests again (below). Needs `pool`. |
| `ready` | no | The serial console line that says the machine has booted. Default `Boot complete.` |
| `[controls]` | no | Who does each control (below). |
| `[qemu]` | no | A QEMU machine's hardware (below). |
| `[amt]` | for `amt` | How the lab reaches a real machine and boots it (below). |
| `[plug]` | for `plug` | How the lab powers a real machine through a plug and boots it (below). |
| `[redfish]` | for `redfish` | How the lab reaches a server through its BMC and boots it (below). |

## Pools

Machines with the same hardware and the same controls are interchangeable,
such as three OptiPlex 7040s. A machine file puts its machine in a pool
with `pool = "optiplex"` (a-z, 0-9 and `-`). `dukovo-lab machines` shows
each machine's pool.

A run on a machine by name runs every test on it. A run on a pool,
`--machine pool:optiplex`, shares its tests out among the pool's machines:
each test runs once, on whichever machine is free ([run](commands.md#run)
says how).

So that a test fits whichever machine takes it, every machine of a pool
must have the same controls, and the same person or driver to do each. A
pool whose machines differ is an error when the machine files are read,
naming the machine and the control, such as `optiplex-2.toml: machine
optiplex-2 is in the pool "optiplex" but its usb control is none, and
optiplex-1's is person`. The machines' addresses, MACs, disks, serial
ports and boot ports are their own, and are not compared.

`spare = true` keeps a machine in the pool but out of a run on the pool:
the others share the tests, and the spare is kept for reruns and bisects,
which name it directly (`--machine optiplex-4`), and then it runs every
test.

```toml
about = "Dell OptiPlex 7040 SFF, Core i5-6500, Intel AMT"
driver = "amt"
pool = "optiplex"
spare = true
```

## Controls

A control is a way the lab acts on a machine: `power`, `cord` (pulling
the power cord), `serial`, `early-console` (the console from the moment
the machine powers on: the firmware's, GRUB's and the kernel's first and
last lines, as a serial cable gives; a console that starts with the
network, as netlog does, has none of them), `link` (the network cable),
`usb` (a USB
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
reach the machine's own servers), `amt` (the lab PC reaching the
machine's Intel AMT, `{amt}`, while the system runs), and a virtual
machine's devices a test asks for with `hardware`: `guest-agent` (a QEMU
guest agent channel) and `virtio-scsi` (a disk on a virtio SCSI
adapter). Each is done by one of:

| Value | Meaning |
| --- | --- |
| `driver` | The machine's driver does it, with no one at the machine. |
| `person` | A person at the machine does it when the runner asks: tests that need it are Needs you tests on this machine, run only with `--with-person` (or ticked on the runner page), and each such step waits for `dukovo-lab done` or the page's Done button. |
| `none` | It cannot be done here: tests that need it are skipped. |

A machine starts with every control its driver can do (QEMU can do all
of them) set to `driver`. `[controls]` can hand one to a person or take
it away, but cannot make a driver do what it cannot. `early-console`,
`linux`, `attack`, `peer`, `amt`, `guest-agent` and `virtio-scsi` are
`driver` or `none`: a person cannot read the console from the power-on,
measure Linux, run the attacker, stand in for the lab PC's side or for
AMT, or add a virtual machine's devices.

So one test file serves every machine: plugging in a USB stick is
automatic on QEMU, a Needs you step on a real machine.

What each driver does by itself:

| Driver | Controls |
| --- | --- |
| `qemu` | All of them but `amt`. |
| `amt` | `power`, `cord` (AMT's hard off, which the system gets no warning of), `serial` and `early-console` (AMT's Serial-over-LAN, or the serial cable the machine file names, either of which carries the firmware's and GRUB's lines too), `keys`, `mouse` and `screen` (AMT's remote screen), `ssh`, `linux`, `peer` and `amt`. Cables, sticks, cards and discs (`link`, `usb`, `card`, `sd`, `disc`, `burner`, `floppy`, `cf`) can be handed to a person; `attack` is not done on real machines yet. |
| `redfish` | `power`, `cord` (the BMC's force off, which the system gets no warning of), `serial` and `early-console` (IPMI Serial-over-LAN, or the serial cable the machine file names, either of which carries the firmware's and GRUB's lines too), `keys`, `mouse` and `screen` (a Dell iDRAC's VNC server; a file for another BMC sets them to `none`), `ssh`, `linux` and `peer`. Cables, sticks, cards and discs can be handed to a person; `amt` and `attack` are not done on a server. |
| `plug` | `power` (the plug switches the machine on, and a reset is the plug off and on again; the power button and a graceful `off` are not available), `cord` (the plug off: a hard power loss the system gets no warning of), `serial` (the console over netlog, which starts once the network is up and stops when it goes down, or over the serial cable the file names), `ssh` and `peer`. Each cable the file names adds controls: `plug.serial` adds `early-console` and `linux`, `plug.capture` adds `screen`, and `plug.hid` adds `keys` and `mouse`. Without its cable, each of these stays `none`, as does `amt`. Cables, sticks, cards and discs can be handed to a person. |

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
| `grow_root` | `true` | Whether each fresh boot grows the root to the end of its disk, an 8 GiB overlay over the image, before the test starts (see [Real machines](real-machines.md#the-root-grows-to-the-disks-end)); a test that installs a bundle needs the room. |

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
| `pin` | (none) | AMT's TLS key as curl names it: `sha256//` and the base64 of the SHA-256 hash of the key. It turns TLS on for power, the console and the screen, which AMT from release 16.1 on serves no other way, and the lab talks to AMT only when its key is this one. `dukovo-lab machines check` prints the line to put here when AMT answers only over TLS. With none, the lab talks to AMT in the clear, as AMT before 16.1 serves it (see [Real machines](real-machines.md#amt-161-and-later-tls-only)). |
| `serial` | (none) | The lab PC's serial port the machine's console is cabled to, such as `/dev/ttyUSB0`. With none, the console is read from AMT's Serial-over-LAN, which the driver turns on; the disk writer's download stalls while it is open, so a machine with a serial port names its cable (see [Real machines](real-machines.md#the-serial-console)). |
| `baud` | `115200` | The serial cable's speed. |
| `machine_address` | `10.77.0.2` | The machine's address on the lab network, for SSH and `{machine}`. |
| `lab_address` | `10.77.0.1` | The lab PC's address on the lab network: the net-boot server listens there, and tests reach the lab PC there as `{peer}`. |
| `boot_port` | `7431` | The first of the net-boot server's four ports, which boots take in turn ([Net boot](net-boot.md) says why). Machines in the same run need four ports of their own. |
| `tftp` | `/srv/dukovo-lab/tftp` | dnsmasq's TFTP folder, where boot scripts are written. |
| `writer` | `/srv/dukovo-lab/writer` | The writer's kit, which `ob lab` installs. |
| `console` | `ttyS4,115200`, or `ttyS0,115200` with a `serial` cable | The machine's serial port as Linux names it, for the writer and the Linux twin. A board's own ports are `ttyS0` to `ttyS3`; PCI ports come after, from `ttyS4`, and with Serial-over-LAN on, AMT's own port is the first of them, so a serial card's first port is then `ttyS5`. |
| `grow_root` | `true` | Whether each fresh boot grows the root to the end of the disk before the test starts (see [Real machines](real-machines.md#the-root-grows-to-the-disks-end)). `false` leaves the image's 512 MiB root, which a PHP, PostgreSQL or website bundle fills. A QEMU machine's disk is an 8 GiB overlay over the image, and its `[qemu]` section takes the same setting. |

## Redfish settings

A machine whose driver is `redfish` is a server with a BMC on the lab
network, driven through Redfish (power, the next boot, a Dell iDRAC's
VNC screen) and IPMI (the console). [Real machines](real-machines.md#a-server-with-a-bmc)
sets one up.

| Key | Default | Meaning |
| --- | --- | --- |
| `address` | (required) | The BMC's address on the lab network. |
| `user` | `root` | The BMC's user, one with the Administrator role. |
| `password_file` | (required) | A file holding the BMC user's password alone, readable by this user alone (`chmod 600`). A relative path is from the machine file's folder. |
| `pin` | (none) | The BMC's public key as curl names it: `sha256//` and the base64 of the SHA-256 hash of the key. A BMC's certificate is its own, so the lab talks to it only when its key is this one. `dukovo-lab machines check` prints the line to put here when the pin is missing or wrong. A run needs it. |
| `mac` | (required) | The MAC address of the network card cabled to the lab network, written `aa:bb:cc:dd:ee:ff`. |
| `disk` | (required) | The serial number of the disk each fresh boot writes the image to, as `lsblk -o NAME,SERIAL` shows it. No other disk is ever written. |
| `serial` | (none) | The lab PC's serial port the machine's console is cabled to, such as `/dev/ttyUSB0`. With none, the console is read through the BMC's IPMI Serial-over-LAN. |
| `baud` | `115200` | The serial cable's speed. |
| `machine_address` | `10.77.0.2` | The machine's address on the lab network, for SSH and `{machine}`. |
| `lab_address` | `10.77.0.1` | The lab PC's address on the lab network: the net-boot server listens there, and tests reach the lab PC there as `{peer}`. |
| `boot_port` | `7431` | The first of the net-boot server's four ports, as for `amt`. |
| `tftp` | `/srv/dukovo-lab/tftp` | dnsmasq's TFTP folder, where boot scripts are written. |
| `writer` | `/srv/dukovo-lab/writer` | The writer's kit, which `ob lab` installs. |
| `console` | `ttyS1,115200`, or `ttyS0,115200` with a `serial` cable | The machine's serial port as Linux names it, for the writer and the Linux twin. An iDRAC's Serial-over-LAN is the board's COM2, `ttyS1`. |
| `grow_root` | `true` | Whether each fresh boot grows the root to the end of the disk before the test starts, as for `amt`. |
| `ipmitool` | `ipmitool` | The `ipmitool` program that reads Serial-over-LAN. |
| `write_minutes` | `12` | How many minutes a fresh boot may take to load the writer and write the image, 1 to 240. A two-socket server spends 2 to 4 minutes in its firmware at each start, and a fresh boot starts it twice. The runner adds this to each test's time limit. |

## Plug settings

A machine whose driver is `plug` is a real machine without Intel AMT, on
the lab network, powered through a Shelly smart plug (Gen 2 or later) on
the home network. It may have up to three cables to the lab PC: a serial
cable, an HDMI capture stick and a CH9329 keyboard and mouse cable. Each
one named below turns on its controls. [Real machines](real-machines.md)
sets one up.

| Key | Default | Meaning |
| --- | --- | --- |
| `shelly` | (required) | The plug's IPv4 address on the home network, such as `192.168.1.3`, with `:port` for a plug that does not listen on port 80. The plug should keep its address (a DHCP reservation). |
| `password_file` | none | Not supported yet: a plug that asks for a password cannot be signed in to, and a machine file that names a file is refused. Turn the plug's authentication off in its web page. |
| `mac` | (required) | The MAC address of the machine's network card on the lab network, written `aa:bb:cc:dd:ee:ff`. |
| `disk` | (required) | The serial number of the disk each fresh boot writes the image to, as `lsblk -o NAME,SERIAL` shows it. No other disk is ever written. When it is not known, any text that is not a serial number: the first fresh boot fails, and its message lists the disks the writer found. |
| `machine_address` | `10.77.0.2` | The machine's address on the lab network, for SSH, `{machine}` and as the sender netlog's datagrams are taken from. A second real machine on the lab network needs an address of its own, and dnsmasq's `dhcp-host` line for it. |
| `lab_address` | `10.77.0.1` | The lab PC's address on the lab network: the net-boot server and the netlog reader listen on it, and tests reach the lab PC there as `{peer}`. |
| `boot_port` | `7431` | The first of the net-boot server's four ports, as for `amt`. |
| `tftp` | `/srv/dukovo-lab/tftp` | dnsmasq's TFTP folder, where boot scripts are written. |
| `writer` | `/srv/dukovo-lab/writer` | The writer's kit, which `ob lab` installs. |
| `netlog_port` | `6666` | The UDP port the lab PC listens on for the machine's netlog. The lab image sends to 6666, so change this only with the image. Plug machines in one run share the port, and each one's datagrams are told apart by its `machine_address`, so two machines with the same address in one run are refused. |
| `on_watts` | `5.0` | The least the machine draws while it runs, in watts. A plug that is on with the machine drawing less means the machine shut itself down. |
| `grow_root` | `true` | Whether each fresh boot grows the root to the end of the disk before the test starts (see [Real machines](real-machines.md#the-root-grows-to-the-disks-end)). `false` leaves the image's 512 MiB root, which a PHP, PostgreSQL or website bundle fills. A QEMU machine's disk is an 8 GiB overlay over the image, and its `[qemu]` section takes the same setting. |
| `serial` | none | The lab PC's serial port that the cable to the board's COM1 is plugged into. With it, the console comes over the cable, the firmware's and GRUB's lines too (`early-console`), the writer's console is `writer-<n>.log`, and the Linux twin can run (`linux`). Without it, the console comes over netlog. Name it as `/dev/serial/by-path/...`: cheap cables of one model share their `/dev/serial/by-id/` names. |
| `baud` | `115200` | The serial cable's speed: 9600, 19200, 38400, 57600 or 115200. |
| `console` | `ttyS0,115200` | The serial port as Linux names it, for the writer and the Linux twin: one name with no spaces. `ttyS0` is the board's COM1. |
| `capture` | none | The video device of the HDMI capture stick, which brings `screen`, such as `/dev/v4l/by-path/...-video-index0`. Cheap sticks of one model share their `by-id` names, so use the `by-path` name. Needs `v4l-utils` and the `video` group on the lab PC. |
| `capture_size` | `1920x1080` | The size of the pictures taken from the stick, `WIDTHxHEIGHT`, with an even width of 2 to 7680 and a height of 1 to 4320. It must be the size of the machine's screen, which Dukovo prints in its console as `Framebuffer: WxH, ...`; a picture of another size is refused with a message naming the setting. |
| `hid` | none | The lab PC's serial port of the CH9329 cable, which brings `keys` and `mouse`. Name it as `/dev/serial/by-path/...`, for the reason given for `serial`. It must be a different port from `serial`. |
| `hid_baud` | `9600` | The CH9329's serial speed: 1200, 2400, 4800, 9600, 14400, 19200, 38400, 57600 or 115200. A new cable speaks 9600. |
